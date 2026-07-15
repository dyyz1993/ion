//! Hooks 系统 — 配置式生命周期触发器
//!
//! 用户通过 `hooks.json` 声明"当某事件发生时执行什么动作"，存盘后立即生效。
//! 对齐 pi 的 `extensions/pi-hooks/`（12 事件 + 5 handler），但 agent handler 真传 tools。
//!
//! 详见 [docs/design/HOOKS_AND_OUTLINE_SYNC.md](../../docs/design/HOOKS_AND_OUTLINE_SYNC.md)
//!
//! 模块结构：
//! - `mod.rs`（本文件）— 数据结构 + 配置加载（每次动态读，热重载核心）
//! - `handler_runner.rs` — 5 种 handler 执行引擎
//! - `stdin_builder.rs` — 各事件 stdin JSON 组装
//! - `matcher.rs` — matcher 正则 + if 条件过滤
//! - `extension.rs` — HookExtension 实现 Extension trait

pub mod handler_runner;
pub mod matcher;
pub mod stdin_builder;
pub mod extension;

use std::collections::HashMap;
use std::path::Path;

// ---------------------------------------------------------------------------
// 数据结构
// ---------------------------------------------------------------------------

/// hooks.json 的 Rust 映射
///
/// 支持全局 (`~/.ion/hooks.json`) 和项目级 (`<project>/.ion/hooks.json`) 合并。
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct HooksConfig {
    /// Schema 版本，默认 1
    #[serde(default = "default_version")]
    pub version: u32,
    /// 紧急逃生阀——设 true 全局禁用所有 hooks
    #[serde(default, rename = "disableAllHooks")]
    pub disable_all_hooks: bool,
    /// 事件名 → Hook 条目列表的映射
    #[serde(default)]
    pub hooks: HashMap<String, Vec<HookEntry>>,
}

fn default_version() -> u32 { 1 }

/// 一个事件下的一条配置：可以是单个 handler，也可以是带 matcher 的 group
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum HookEntry {
    /// 单个 handler（简写形式，等价于 hooks: [单个]）
    Handler(HookHandler),
    /// 带 matcher 的 group（PreToolUse/PostToolUse 按 matcher 过滤工具名）
    Group(HookGroup),
}

/// 带 matcher 的 handler 组
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct HookGroup {
    /// 正则匹配工具名（仅 PreToolUse/PostToolUse 有效）。
    /// None / "*" / 空 = 全匹配
    #[serde(default)]
    pub matcher: Option<String>,
    /// Stop 事件的循环阻断上限（默认 5）
    #[serde(default)]
    pub loop_limit: Option<u32>,
    pub hooks: Vec<HookHandler>,
}

/// 单个 handler 的定义
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct HookHandler {
    #[serde(rename = "type")]
    pub handler_type: HandlerType,

    // ── 各类型必填字段（按 type 取用）──
    /// command 类型必填
    #[serde(default)]
    pub command: Option<String>,
    /// http 类型必填
    #[serde(default)]
    pub url: Option<String>,
    /// prompt / agent 类型必填（LLM 指令）
    #[serde(default)]
    pub prompt: Option<String>,
    /// agent 类型可选：指定 agent 角色（对应 .ion/agents/<name>.md）。
    /// 不填用 "default"。用户可自定义专用 agent（如 "outline-syncer"）。
    #[serde(default)]
    pub agent: Option<String>,
    /// mcp_tool 类型必填
    #[serde(default)]
    pub server: Option<String>,
    #[serde(default)]
    pub tool: Option<String>,

    // ── 通用可选字段 ──
    #[serde(default)]
    pub input: Option<serde_json::Value>,
    #[serde(default)]
    pub model: Option<String>,
    /// 超时秒数，默认 30
    #[serde(default)]
    pub timeout: Option<u32>,
    /// 条件表达式（对齐 pi if-parser，如 `Bash(rm *)`）
    #[serde(default, rename = "if")]
    pub if_clause: Option<String>,
    /// 仅 PreToolUse 生效，后台跑不阻塞
    #[serde(default)]
    pub r#async: bool,
    /// async + exit 2 时把 reason 作为 nextTurn 消息注入
    #[serde(default)]
    pub async_rewake: bool,
    /// 只触发一次（per session）
    #[serde(default)]
    pub once: bool,
    /// 执行时给 UI 显示的状态文案
    #[serde(default)]
    pub status_message: Option<String>,
    /// agent 类型：子 Worker 工具白名单
    #[serde(default)]
    pub allowed_tools: Option<Vec<String>>,
    /// agent 类型：子 Worker 最大 turn 数
    #[serde(default)]
    pub max_turns: Option<u64>,
}

/// Handler 类型（5 种）
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HandlerType {
    /// spawn bash + stdin/stdout/退出码协议
    Command,
    /// POST 到 url
    Http,
    /// 调 callLLM 单轮判断（无工具）
    Prompt,
    /// spawn 带 tools + maxTurns 的子 Worker（依赖补丁 1）
    Agent,
    /// 调 MCP server 的工具
    McpTool,
}

// ---------------------------------------------------------------------------
// 配置加载（热重载核心：每次动态读，不缓存）
// ---------------------------------------------------------------------------

impl HooksConfig {
    /// 每次事件触发时调用——重新读文件，零状态，改完即生效。
    ///
    /// 读取顺序：全局 `~/.ion/hooks.json` + 项目级 `<project>/.ion/hooks.json`，合并执行。
    /// hooks.json 通常几十行，读+解析微秒级，相比 handler spawn 的几十 ms 可忽略。
    pub fn load_fresh(project_dir: Option<&Path>) -> HooksConfig {
        let mut merged = HooksConfig::default();

        // 全局配置
        let global_path = crate::paths::root().join("hooks.json");
        if global_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&global_path) {
                if let Ok(global) = serde_json::from_str::<HooksConfig>(&content) {
                    // disableAllHooks 任一文件设 true 则全局禁用
                    if global.disable_all_hooks {
                        merged.disable_all_hooks = true;
                    }
                    // 合并 hooks（拼接，不覆盖）
                    for (event, entries) in global.hooks {
                        merged.hooks.entry(event).or_default().extend(entries);
                    }
                }
            }
        }

        // 项目级配置（合并）
        if let Some(proj) = project_dir {
            let proj_path = proj.join(".ion").join("hooks.json");
            if proj_path.exists() {
                if let Ok(content) = std::fs::read_to_string(&proj_path) {
                    if let Ok(proj_cfg) = serde_json::from_str::<HooksConfig>(&content) {
                        if proj_cfg.disable_all_hooks {
                            merged.disable_all_hooks = true;
                        }
                        for (event, entries) in proj_cfg.hooks {
                            merged.hooks.entry(event).or_default().extend(entries);
                        }
                    }
                }
            }
        }

        merged
    }

    /// 是否为空（没有任何 hook）
    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    /// 获取某事件的所有 handler（展平 group）
    pub fn handlers_for_event(&self, event: &str) -> Vec<(Option<&str>, &HookHandler)> {
        let mut result = Vec::new();
        if let Some(entries) = self.hooks.get(event) {
            for entry in entries {
                match entry {
                    HookEntry::Handler(h) => result.push((None, h)),
                    HookEntry::Group(g) => {
                        for h in &g.hooks {
                            result.push((g.matcher.as_deref(), h));
                        }
                    }
                }
            }
        }
        result
    }

    /// 事件总数
    pub fn event_count(&self) -> usize {
        self.hooks.len()
    }

    /// handler 总数（展平 group）
    pub fn handler_count(&self) -> usize {
        self.hooks.values().map(|entries| {
            entries.iter().map(|e| match e {
                HookEntry::Handler(_) => 1,
                HookEntry::Group(g) => g.hooks.len(),
            }).sum::<usize>()
        }).sum()
    }
}

// ---------------------------------------------------------------------------
// Hook 执行结果
// ---------------------------------------------------------------------------

/// 一次 hook 执行的结果（汇总到 Extension trait 方法的返回）
#[derive(Clone, Debug, Default)]
pub struct HookOutcome {
    /// 是否阻断（block）
    pub block: bool,
    /// 阻断原因（block=true 时有意义）
    pub block_reason: Option<String>,
    /// 是否请求用户确认（exit 3，仅 PreToolUse）
    pub ask: bool,
    /// 要注入到 system prompt 的附加上下文
    pub additional_context: Option<String>,
    /// 工具参数覆盖（PreToolUse 的 updatedInput）
    pub updated_input: Option<serde_json::Value>,
}

impl HookOutcome {
    /// 合并两个 outcome（后者优先）
    pub fn merge(self, other: HookOutcome) -> HookOutcome {
        HookOutcome {
            block: self.block || other.block,
            block_reason: other.block_reason.or(self.block_reason),
            ask: self.ask || other.ask,
            additional_context: match (self.additional_context, other.additional_context) {
                (Some(a), Some(b)) => Some(format!("{a}\n\n{b}")),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            },
            updated_input: other.updated_input.or(self.updated_input),
        }
    }

    pub fn is_terminal(&self) -> bool {
        self.block
    }
}
