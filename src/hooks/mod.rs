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

    /// Returns total number of hook entries across all events (sum of Vec<HookEntry> lengths)
    pub fn count_hooks(&self) -> usize {
        self.hooks.values().map(|v| v.len()).sum()
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_handler(handler_type: HandlerType, command: Option<&str>, url: Option<&str>, prompt: Option<&str>) -> HookEntry {
        HookEntry::Handler(HookHandler {
            handler_type,
            command: command.map(|s| s.to_string()),
            url: url.map(|s| s.to_string()),
            prompt: prompt.map(|s| s.to_string()),
            agent: None,
            server: None,
            tool: None,
            input: None,
            model: None,
            timeout: None,
            if_clause: None,
            r#async: false,
            async_rewake: false,
            once: false,
            status_message: None,
            allowed_tools: None,
            max_turns: None,
        })
    }

    #[test]
    fn test_count_hooks() {
        let mut hooks = HooksConfig::default();
        assert_eq!(hooks.count_hooks(), 0, "empty config should have 0 hooks");

        // Add entries for event "event_a"
        hooks.hooks.insert(
            "event_a".to_string(),
            vec![
                make_handler(HandlerType::Command, Some("echo hello"), None, None),
                make_handler(HandlerType::Command, Some("echo world"), None, None),
            ],
        );
        assert_eq!(hooks.count_hooks(), 2, "2 hooks in one event");

        // Add entries for event "event_b"
        hooks.hooks.insert(
            "event_b".to_string(),
            vec![
                make_handler(HandlerType::Http, None, Some("https://example.com"), None),
            ],
        );
        assert_eq!(hooks.count_hooks(), 3, "3 hooks across two events");

        // Add a group entry to verify groups count as 1 entry
        hooks.hooks.entry("event_a".to_string()).or_default().push(
            HookEntry::Group(HookGroup {
                matcher: None,
                loop_limit: None,
                hooks: vec![
                    HookHandler {
                        handler_type: HandlerType::Prompt,
                        command: None,
                        url: None,
                        prompt: Some("test".to_string()),
                        agent: None,
                        server: None,
                        tool: None,
                        input: None,
                        model: None,
                        timeout: None,
                        if_clause: None,
                        r#async: false,
                        async_rewake: false,
                        once: false,
                        status_message: None,
                        allowed_tools: None,
                        max_turns: None,
                    },
                ],
            }),
        );
        assert_eq!(hooks.count_hooks(), 4, "4 hooks after adding a group (group counts as 1)");
    }

    // -------------------------------------------------------------------------
    // HooksConfig construction and defaults
    // -------------------------------------------------------------------------

    #[test]
    fn test_hooks_config_default() {
        let cfg = HooksConfig::default();
        assert_eq!(cfg.version, 0, "default version should be 0 (serde default overrides on deserialize, but Default trait yields 0)");
        assert!(!cfg.disable_all_hooks, "disable_all_hooks should default to false");
        assert!(cfg.hooks.is_empty(), "hooks map should default to empty");
    }

    #[test]
    fn test_default_version_fn() {
        // The serde default function for the version field.
        assert_eq!(default_version(), 1, "default_version() helper returns 1");
    }

    #[test]
    fn test_hooks_config_construction() {
        let mut hooks = HashMap::new();
        hooks.insert(
            "PreToolUse".to_string(),
            vec![make_handler(HandlerType::Command, Some("echo hi"), None, None)],
        );
        let cfg = HooksConfig {
            version: 2,
            disable_all_hooks: true,
            hooks,
        };
        assert_eq!(cfg.version, 2);
        assert!(cfg.disable_all_hooks);
        assert_eq!(cfg.hooks.len(), 1);
    }

    #[test]
    fn test_hooks_config_serde_roundtrip() {
        let json = r#"{
            "version": 1,
            "disableAllHooks": false,
            "hooks": {
                "PreToolUse": [
                    { "type": "command", "command": "echo hi" }
                ]
            }
        }"#;
        let cfg: HooksConfig = serde_json::from_str(json).expect("parse should succeed");
        assert_eq!(cfg.version, 1, "version should deserialize to 1 via default_version");
        assert!(!cfg.disable_all_hooks);
        assert_eq!(cfg.hooks.len(), 1);
        assert_eq!(cfg.count_hooks(), 1);
    }

    #[test]
    fn test_hooks_config_empty_json_uses_defaults() {
        // An empty JSON object should fall back to all serde defaults.
        let cfg: HooksConfig = serde_json::from_str("{}").expect("empty object parses");
        assert_eq!(cfg.version, 1, "missing version uses default_version() = 1");
        assert!(!cfg.disable_all_hooks);
        assert!(cfg.hooks.is_empty());
    }

    #[test]
    fn test_disable_all_hooks_field_rename() {
        // The field is serialized as disableAllHooks (camelCase).
        let json = r#"{ "disableAllHooks": true }"#;
        let cfg: HooksConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.disable_all_hooks);
    }

    // -------------------------------------------------------------------------
    // Empty config handling
    // -------------------------------------------------------------------------

    #[test]
    fn test_is_empty_true_by_default() {
        let cfg = HooksConfig::default();
        assert!(cfg.is_empty(), "default config is empty");
    }

    #[test]
    fn test_is_empty_false_with_hooks() {
        let mut cfg = HooksConfig::default();
        cfg.hooks.insert(
            "PostToolUse".to_string(),
            vec![make_handler(HandlerType::Command, Some("true"), None, None)],
        );
        assert!(!cfg.is_empty(), "config with one event is not empty");
    }

    #[test]
    fn test_event_count() {
        let mut cfg = HooksConfig::default();
        assert_eq!(cfg.event_count(), 0);

        cfg.hooks.insert("a".to_string(), vec![]);
        assert_eq!(cfg.event_count(), 1, "empty vec still counts as an event");

        cfg.hooks.insert("b".to_string(), vec![]);
        assert_eq!(cfg.event_count(), 2);
    }

    #[test]
    fn test_handler_count_empty() {
        let cfg = HooksConfig::default();
        assert_eq!(cfg.handler_count(), 0);
    }

    #[test]
    fn test_handler_count_with_handlers_and_groups() {
        let mut cfg = HooksConfig::default();
        // Two plain handlers under "a"
        cfg.hooks.insert(
            "a".to_string(),
            vec![
                make_handler(HandlerType::Command, Some("echo 1"), None, None),
                make_handler(HandlerType::Command, Some("echo 2"), None, None),
            ],
        );
        // One group with 2 inner handlers under "b"
        cfg.hooks.insert(
            "b".to_string(),
            vec![HookEntry::Group(HookGroup {
                matcher: Some("Bash".to_string()),
                loop_limit: None,
                hooks: vec![
                    make_handler(HandlerType::Command, Some("echo g1"), None, None).into_handler(),
                    make_handler(HandlerType::Command, Some("echo g2"), None, None).into_handler(),
                ],
            })],
        );
        // 2 plain + 2 in group = 4
        assert_eq!(cfg.handler_count(), 4);
    }

    // -------------------------------------------------------------------------
    // HandlerType enum variants
    // -------------------------------------------------------------------------

    #[test]
    fn test_handler_type_serde_variants() {
        // Verify snake_case serialization for all 5 variants.
        let cases = [
            (HandlerType::Command, "\"command\""),
            (HandlerType::Http, "\"http\""),
            (HandlerType::Prompt, "\"prompt\""),
            (HandlerType::Agent, "\"agent\""),
            (HandlerType::McpTool, "\"mcp_tool\""),
        ];
        for (variant, expected) in cases {
            let s = serde_json::to_string(&variant).unwrap();
            assert_eq!(s, expected, "serialize {:?}", variant);
            let back: HandlerType = serde_json::from_str(expected).unwrap();
            assert_eq!(back, variant, "deserialize {:?}", variant);
        }
    }

    #[test]
    fn test_handler_type_equality() {
        assert_eq!(HandlerType::Command, HandlerType::Command);
        assert_ne!(HandlerType::Command, HandlerType::Http);
    }

    // -------------------------------------------------------------------------
    // HookHandler struct — all 5 handler variants
    // -------------------------------------------------------------------------

    #[test]
    fn test_command_handler_construction() {
        let h = HookHandler {
            handler_type: HandlerType::Command,
            command: Some("echo hello".to_string()),
            ..full_default_handler()
        };
        assert_eq!(h.handler_type, HandlerType::Command);
        assert_eq!(h.command.as_deref(), Some("echo hello"));
        assert!(h.url.is_none());
    }

    #[test]
    fn test_http_handler_construction() {
        let h = HookHandler {
            handler_type: HandlerType::Http,
            url: Some("https://example.com/hook".to_string()),
            ..full_default_handler()
        };
        assert_eq!(h.handler_type, HandlerType::Http);
        assert_eq!(h.url.as_deref(), Some("https://example.com/hook"));
        assert!(h.command.is_none());
    }

    #[test]
    fn test_prompt_handler_construction() {
        let h = HookHandler {
            handler_type: HandlerType::Prompt,
            prompt: Some("Is this safe?".to_string()),
            model: Some("gpt-4".to_string()),
            ..full_default_handler()
        };
        assert_eq!(h.handler_type, HandlerType::Prompt);
        assert_eq!(h.prompt.as_deref(), Some("Is this safe?"));
        assert_eq!(h.model.as_deref(), Some("gpt-4"));
    }

    #[test]
    fn test_agent_handler_construction() {
        let h = HookHandler {
            handler_type: HandlerType::Agent,
            agent: Some("outline-syncer".to_string()),
            prompt: Some("sync outline".to_string()),
            allowed_tools: Some(vec!["Read".to_string(), "Write".to_string()]),
            max_turns: Some(5),
            ..full_default_handler()
        };
        assert_eq!(h.handler_type, HandlerType::Agent);
        assert_eq!(h.agent.as_deref(), Some("outline-syncer"));
        assert_eq!(h.allowed_tools.as_deref(), Some(&["Read".to_string(), "Write".to_string()][..]));
        assert_eq!(h.max_turns, Some(5));
    }

    #[test]
    fn test_mcp_tool_handler_construction() {
        let h = HookHandler {
            handler_type: HandlerType::McpTool,
            server: Some("context7".to_string()),
            tool: Some("search".to_string()),
            input: Some(serde_json::json!({"query": "rust"})),
            ..full_default_handler()
        };
        assert_eq!(h.handler_type, HandlerType::McpTool);
        assert_eq!(h.server.as_deref(), Some("context7"));
        assert_eq!(h.tool.as_deref(), Some("search"));
        assert_eq!(h.input, Some(serde_json::json!({"query": "rust"})));
    }

    #[test]
    fn test_handler_optional_fields() {
        let h = HookHandler {
            handler_type: HandlerType::Command,
            command: Some("run".to_string()),
            timeout: Some(60),
            if_clause: Some("Bash(rm *)".to_string()),
            r#async: true,
            async_rewake: true,
            once: true,
            status_message: Some("working...".to_string()),
            ..full_default_handler()
        };
        assert_eq!(h.timeout, Some(60));
        assert_eq!(h.if_clause.as_deref(), Some("Bash(rm *)"));
        assert!(h.r#async);
        assert!(h.async_rewake);
        assert!(h.once);
        assert_eq!(h.status_message.as_deref(), Some("working..."));
    }

    #[test]
    fn test_handler_if_clause_rename() {
        // The field is serialized as "if".
        let json = r#"{ "type": "command", "command": "echo x", "if": "Bash(rm *)" }"#;
        let h: HookHandler = serde_json::from_str(json).unwrap();
        assert_eq!(h.if_clause.as_deref(), Some("Bash(rm *)"));
    }

    #[test]
    fn test_handler_async_keyword_field() {
        let json = r#"{ "type": "command", "command": "echo x", "async": true }"#;
        let h: HookHandler = serde_json::from_str(json).unwrap();
        assert!(h.r#async);
    }

    // -------------------------------------------------------------------------
    // HookGroup and HookEntry
    // -------------------------------------------------------------------------

    #[test]
    fn test_hook_group_construction() {
        let g = HookGroup {
            matcher: Some("Bash.*".to_string()),
            loop_limit: Some(3),
            hooks: vec![make_handler(HandlerType::Command, Some("echo"), None, None).into_handler()],
        };
        assert_eq!(g.matcher.as_deref(), Some("Bash.*"));
        assert_eq!(g.loop_limit, Some(3));
        assert_eq!(g.hooks.len(), 1);
    }

    #[test]
    fn test_hook_group_defaults() {
        let g = HookGroup {
            matcher: None,
            loop_limit: None,
            hooks: vec![],
        };
        assert!(g.matcher.is_none());
        assert!(g.loop_limit.is_none());
        assert!(g.hooks.is_empty());
    }

    #[test]
    fn test_hook_entry_untagged_deserialize_handler() {
        // Untagged enum: an object without "hooks" is treated as a plain Handler.
        let json = r#"{ "type": "command", "command": "echo hi" }"#;
        let entry: HookEntry = serde_json::from_str(json).unwrap();
        match entry {
            HookEntry::Handler(h) => {
                assert_eq!(h.handler_type, HandlerType::Command);
                assert_eq!(h.command.as_deref(), Some("echo hi"));
            }
            HookEntry::Group(_) => panic!("should deserialize as Handler"),
        }
    }

    #[test]
    fn test_hook_entry_untagged_deserialize_group() {
        // An object with a "hooks" array is treated as a Group.
        let json = r#"{
            "matcher": "Bash",
            "hooks": [ { "type": "command", "command": "echo g" } ]
        }"#;
        let entry: HookEntry = serde_json::from_str(json).unwrap();
        match entry {
            HookEntry::Group(g) => {
                assert_eq!(g.matcher.as_deref(), Some("Bash"));
                assert_eq!(g.hooks.len(), 1);
            }
            HookEntry::Handler(_) => panic!("should deserialize as Group"),
        }
    }

    // -------------------------------------------------------------------------
    // handlers_for_event helper
    // -------------------------------------------------------------------------

    #[test]
    fn test_handlers_for_event_missing() {
        let cfg = HooksConfig::default();
        let result = cfg.handlers_for_event("Nonexistent");
        assert!(result.is_empty());
    }

    #[test]
    fn test_handlers_for_event_plain_handlers() {
        let mut cfg = HooksConfig::default();
        cfg.hooks.insert(
            "PreToolUse".to_string(),
            vec![
                make_handler(HandlerType::Command, Some("a"), None, None),
                make_handler(HandlerType::Command, Some("b"), None, None),
            ],
        );
        let result = cfg.handlers_for_event("PreToolUse");
        assert_eq!(result.len(), 2);
        // Plain handlers have no matcher.
        assert!(result.iter().all(|(m, _)| m.is_none()));
    }

    #[test]
    fn test_handlers_for_event_with_group() {
        let mut cfg = HooksConfig::default();
        cfg.hooks.insert(
            "PreToolUse".to_string(),
            vec![HookEntry::Group(HookGroup {
                matcher: Some("Bash".to_string()),
                loop_limit: None,
                hooks: vec![
                    make_handler(HandlerType::Command, Some("g1"), None, None).into_handler(),
                    make_handler(HandlerType::Command, Some("g2"), None, None).into_handler(),
                ],
            })],
        );
        let result = cfg.handlers_for_event("PreToolUse");
        assert_eq!(result.len(), 2);
        // All handlers from a group carry the group's matcher.
        assert!(result.iter().all(|(m, _)| *m == Some("Bash")));
    }

    #[test]
    fn test_handlers_for_event_mixed() {
        let mut cfg = HooksConfig::default();
        cfg.hooks.insert(
            "PostToolUse".to_string(),
            vec![
                // Plain handler -> matcher None
                make_handler(HandlerType::Command, Some("plain"), None, None),
                // Group handler -> matcher Some("Read")
                HookEntry::Group(HookGroup {
                    matcher: Some("Read".to_string()),
                    loop_limit: None,
                    hooks: vec![
                        make_handler(HandlerType::Command, Some("grp"), None, None).into_handler(),
                    ],
                }),
            ],
        );
        let result = cfg.handlers_for_event("PostToolUse");
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].0, None);
        assert_eq!(result[1].0, Some("Read"));
    }

    // -------------------------------------------------------------------------
    // count_hooks vs handler_count distinction
    // -------------------------------------------------------------------------

    #[test]
    fn test_count_hooks_vs_handler_count() {
        let mut cfg = HooksConfig::default();
        // A group with 3 inner handlers counts as 1 entry but 3 handlers.
        cfg.hooks.insert(
            "e".to_string(),
            vec![HookEntry::Group(HookGroup {
                matcher: None,
                loop_limit: None,
                hooks: vec![
                    make_handler(HandlerType::Command, Some("1"), None, None).into_handler(),
                    make_handler(HandlerType::Command, Some("2"), None, None).into_handler(),
                    make_handler(HandlerType::Command, Some("3"), None, None).into_handler(),
                ],
            })],
        );
        assert_eq!(cfg.count_hooks(), 1, "count_hooks counts entries (group = 1)");
        assert_eq!(cfg.handler_count(), 3, "handler_count flattens groups");
    }

    // -------------------------------------------------------------------------
    // HookOutcome
    // -------------------------------------------------------------------------

    #[test]
    fn test_hook_outcome_default() {
        let o = HookOutcome::default();
        assert!(!o.block);
        assert!(o.block_reason.is_none());
        assert!(!o.ask);
        assert!(o.additional_context.is_none());
        assert!(o.updated_input.is_none());
        assert!(!o.is_terminal(), "default outcome is not terminal");
    }

    #[test]
    fn test_hook_outcome_is_terminal() {
        let o = HookOutcome { block: true, ..Default::default() };
        assert!(o.is_terminal());
    }

    #[test]
    fn test_hook_outcome_merge_block_propagates() {
        let a = HookOutcome { block: false, ..Default::default() };
        let b = HookOutcome { block: true, block_reason: Some("nope".to_string()), ..Default::default() };
        let m = a.merge(b);
        assert!(m.block);
        assert_eq!(m.block_reason.as_deref(), Some("nope"));
        assert!(m.is_terminal());
    }

    #[test]
    fn test_hook_outcome_merge_ask_propagates() {
        let a = HookOutcome { ask: true, ..Default::default() };
        let b = HookOutcome { ask: false, ..Default::default() };
        let m = a.merge(b);
        assert!(m.ask, "ask should be OR-ed");
    }

    #[test]
    fn test_hook_outcome_merge_additional_context_both() {
        let a = HookOutcome { additional_context: Some("ctx-a".to_string()), ..Default::default() };
        let b = HookOutcome { additional_context: Some("ctx-b".to_string()), ..Default::default() };
        let m = a.merge(b);
        assert_eq!(m.additional_context.as_deref(), Some("ctx-a\n\nctx-b"));
    }

    #[test]
    fn test_hook_outcome_merge_additional_context_one_side() {
        let a = HookOutcome { additional_context: None, ..Default::default() };
        let b = HookOutcome { additional_context: Some("only-b".to_string()), ..Default::default() };
        let m = a.merge(b);
        assert_eq!(m.additional_context.as_deref(), Some("only-b"));

        let a2 = HookOutcome { additional_context: Some("only-a".to_string()), ..Default::default() };
        let b2 = HookOutcome { additional_context: None, ..Default::default() };
        let m2 = a2.merge(b2);
        assert_eq!(m2.additional_context.as_deref(), Some("only-a"));
    }

    #[test]
    fn test_hook_outcome_merge_block_reason_other_none() {
        // other (second) has no block_reason -> falls back to self's.
        let a = HookOutcome { block_reason: Some("from-a".to_string()), ..Default::default() };
        let b = HookOutcome { block_reason: None, ..Default::default() };
        let m = a.merge(b);
        assert_eq!(m.block_reason.as_deref(), Some("from-a"));
    }

    #[test]
    fn test_hook_outcome_merge_updated_input_other_wins() {
        let a = HookOutcome { updated_input: Some(serde_json::json!({"a": 1})), ..Default::default() };
        let b = HookOutcome { updated_input: Some(serde_json::json!({"b": 2})), ..Default::default() };
        let m = a.merge(b);
        assert_eq!(m.updated_input, Some(serde_json::json!({"b": 2})), "other takes priority");

        let a2 = HookOutcome { updated_input: Some(serde_json::json!({"a": 1})), ..Default::default() };
        let b2 = HookOutcome { updated_input: None, ..Default::default() };
        let m2 = a2.merge(b2);
        assert_eq!(m2.updated_input, Some(serde_json::json!({"a": 1})), "falls back to self when other is None");
    }

    // -------------------------------------------------------------------------
    // Helper trait/impl to reduce boilerplate in tests
    // -------------------------------------------------------------------------

    /// Convenience trait to unwrap a HookEntry::Handler into its inner HookHandler.
    impl From<HookEntry> for HookHandler {
        fn from(entry: HookEntry) -> Self {
            match entry {
                HookEntry::Handler(h) => h,
                HookEntry::Group(g) => g.hooks.into_iter().next().expect("group must have at least one handler"),
            }
        }
    }

    impl HookEntry {
        /// Convert a Handler-style HookEntry back into its inner HookHandler (test helper).
        fn into_handler(self) -> HookHandler {
            self.into()
        }
    }

    /// Build a HookHandler with all fields defaulted except handler_type (caller overrides).
    fn full_default_handler() -> HookHandler {
        HookHandler {
            handler_type: HandlerType::Command,
            command: None,
            url: None,
            prompt: None,
            agent: None,
            server: None,
            tool: None,
            input: None,
            model: None,
            timeout: None,
            if_clause: None,
            r#async: false,
            async_rewake: false,
            once: false,
            status_message: None,
            allowed_tools: None,
            max_turns: None,
        }
    }
}
