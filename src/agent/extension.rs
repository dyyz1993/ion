use super::agent_loop::AgentContext;
use super::error::AgentError;
use super::error::AgentResult;
use super::messages::{Message, ToolCall};
use async_trait::async_trait;
use ion_provider::types::{ToolResult, Usage};

// ---------------------------------------------------------------------------
// Context objects
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct TurnContext {
    pub turn_index: u64,
    pub messages: Vec<Message>,
    pub has_tool_calls: bool,
    pub stop_reason: Option<String>,
}

#[derive(Clone, Debug)]
pub struct InputContext {
    pub text: String,
    pub handled: bool,
}

#[derive(Clone, Debug)]
pub struct BeforeAgentContext {
    pub system_prompt: Option<String>,
    pub messages: Vec<Message>,
}

#[derive(Clone, Debug)]
pub struct ProviderRequestContext {
    pub model: String,
    pub provider: String,
    pub payload: serde_json::Value,
}

#[derive(Clone, Debug)]
pub struct ProviderResponseContext {
    pub model: String,
    pub provider: String,
    pub status: u16,
    pub body_preview: String,
}

#[derive(Clone, Debug)]
pub struct ToolExecutionContext {
    pub tool_call_id: String,
    pub tool_name: String,
    pub args: serde_json::Value,
    pub is_error: bool,
    pub duration_ms: u64,
    /// 工具执行结果文本（对齐 pi tool_execution_end 的 result 字段）
    pub result: String,
    /// 工具被 soft interrupt 打断（文件回滚用）
    pub is_interrupted: bool,
}

#[derive(Clone, Debug)]
pub struct ModelSelectContext {
    pub old_model: Option<String>,
    pub old_provider: Option<String>,
    pub new_model: String,
    pub new_provider: String,
}

#[derive(Clone, Debug)]
pub struct SessionContext {
    pub reason: String, // "startup" | "reload" | "new" | "resume" | "fork" | "quit"
}

/// Context for session_before_switch / session_before_fork hooks.
#[derive(Clone, Debug)]
pub struct SessionSwitchContext {
    /// "switch" (checkout) | "fork" (fork-from-leaf) | "branch" (in-file branch)
    pub action: String,
    /// Target leaf entry id being switched/forked to.
    pub target_leaf_id: Option<String>,
    /// Source leaf entry id being switched from (if known).
    pub source_leaf_id: Option<String>,
    /// Branch name (for named branch operations).
    pub branch_name: Option<String>,
}

// ---------------------------------------------------------------------------
// Extension trait — ~30 lifecycle hooks + singleton management + RPC + gate (matching pi spec)
// ---------------------------------------------------------------------------

#[async_trait]
pub trait Extension: Send + Sync {
    /// Optional name for extension routing (used by extension_rpc dispatch).
    fn name(&self) -> &str { "anonymous" }

    // ── Session lifecycle (6) ──
    async fn on_session_start(&self, _ctx: &SessionContext) -> AgentResult<()> { Ok(()) }
    async fn on_session_shutdown(&self, _ctx: &SessionContext) -> AgentResult<()> { Ok(()) }
    async fn on_session_before_compact(&self, _msgs: &mut Vec<Message>) -> AgentResult<()> { Ok(()) }
    async fn on_session_compact(&self, _messages: &mut Vec<Message>) -> AgentResult<()> { Ok(()) }
    /// Fired before a session branch/checkout (in-file switch). Return Err to veto.
    async fn on_session_before_switch(&self, _ctx: &SessionSwitchContext) -> AgentResult<()> { Ok(()) }
    /// Fired before a fork-from-leaf (cross-file). Return Err to veto.
    async fn on_session_before_fork(&self, _ctx: &SessionSwitchContext) -> AgentResult<()> { Ok(()) }

    // ── Input (1) ──
    /// Intercept or transform user input before agent processes it.
    /// Return `handled: true` to skip agent processing.
    async fn on_input(&self, _ctx: &mut InputContext) -> AgentResult<()> { Ok(()) }

    // ── Agent lifecycle (4) ──
    async fn before_agent_start(&self, _ctx: &mut BeforeAgentContext) -> AgentResult<()> { Ok(()) }
    async fn on_agent_start(&self, _ctx: &AgentContext) -> AgentResult<()> { Ok(()) }
    async fn on_agent_end(&self, _ctx: &AgentContext) -> AgentResult<()> { Ok(()) }

    // ── Turn lifecycle (2) ──
    async fn on_turn_start(&self, _ctx: &mut TurnContext) -> AgentResult<()> { Ok(()) }
    async fn on_turn_end(&self, _ctx: &TurnContext) -> AgentResult<()> { Ok(()) }

    // ── Context / Provider (3) ──
    async fn on_context(&self, _messages: &mut Vec<Message>) -> AgentResult<()> { Ok(()) }
    async fn before_provider_request(&self, _ctx: &ProviderRequestContext) -> AgentResult<()> { Ok(()) }
    async fn after_provider_response(&self, _ctx: &ProviderResponseContext) -> AgentResult<()> { Ok(()) }
    /// 自动重试开始（LLM 调用失败后退避重试）。前端可显示"重试中 (N/M)..."
    async fn on_auto_retry_start(&self, _attempt: u32, _max_retries: u32) -> AgentResult<()> { Ok(()) }
    /// 自动重试结束（success=true 表示重试成功；false 表示所有重试用完）
    async fn on_auto_retry_end(&self, _success: bool, _attempt: u32) -> AgentResult<()> { Ok(()) }

    // ── Streaming (8) ──
    async fn on_message_start(&self, _role: &str, _content: &str) -> AgentResult<()> { Ok(()) }
    async fn on_message_delta(&self, _delta: &str, _role: &str) -> AgentResult<()> { Ok(()) }
    async fn on_message_end(&self, _role: &str, _full_content: &str, _usage: &Usage) -> AgentResult<()> { Ok(()) }
    /// Called for each thinking delta during streaming.
    async fn on_thinking_delta(&self, _delta: &str) -> AgentResult<()> { Ok(()) }
    /// Called when thinking content is complete.
    async fn on_thinking_end(&self, _content: &str) -> AgentResult<()> { Ok(()) }
    /// Called for tool call deltas during streaming (partial tool name/args).
    async fn on_tool_call_delta(&self, _delta: &str, _name: &str) -> AgentResult<()> { Ok(()) }
    /// Called when a text block ends (provider's TextEnd event).
    async fn on_text_end(&self, _content: &str) -> AgentResult<()> { Ok(()) }
    /// Called when a tool call completes (provider's ToolCallEnd event).
    async fn on_tool_call_end(&self, _tool_call: &ToolCall) -> AgentResult<()> { Ok(()) }

    // ── Tool execution (5) ──
    async fn on_tool_execution_start(&self, _ctx: &ToolExecutionContext) -> AgentResult<()> { Ok(()) }

    /// 工具执行**前**触发（增量 save 用）。
    /// 此时 messages 已包含 user prompt + assistant tool call decision。
    /// ion_worker 的 StreamingExtension 用这个钩子在 fork 阻塞前 save。
    async fn on_before_tool_execute(
        &self,
        _tool_name: &str,
        _args: &serde_json::Value,
        _messages: &[crate::agent::messages::Message],
    ) -> AgentResult<()> {
        Ok(())
    }

    /// Called during tool execution with partial results (e.g., streaming bash output).
    async fn on_tool_execution_update(&self, _ctx: &ToolExecutionContext, _partial: &str) -> AgentResult<()> { Ok(()) }
    async fn on_tool_execution_end(&self, _ctx: &ToolExecutionContext) -> AgentResult<()> { Ok(()) }
    async fn before_tool_call(&self, _call: &ToolCall) -> AgentResult<()> { Ok(()) }
    async fn after_tool_call(&self, _call: &ToolCall, _result: &ToolResult) -> AgentResult<()> { Ok(()) }

    // ── Model (3) ──
    /// 模型选择钩子。ctx 可变 → 扩展能覆盖模型选择（自定义策略）。
    async fn on_model_select(&self, _ctx: &mut ModelSelectContext) -> AgentResult<()> { Ok(()) }
    async fn on_thinking_level_select(&self, _level: &str, _old: Option<&str>) -> AgentResult<()> { Ok(()) }

    // ── Entries ──
    /// Called when entries are deleted or summarized.
    async fn on_entries_invalidated(&self, _entry_ids: &[String]) -> AgentResult<()> { Ok(()) }

    // ── Session navigation (stubs - 已在上面定义 on_session_before_switch/fork) ──
    /// Called before tree navigation. Can customize summary.
    async fn on_session_before_tree(&self, _target: &str) -> AgentResult<()> { Ok(()) }
    /// Called after tree navigation.
    async fn on_session_tree(&self, _leaf_id: &str) -> AgentResult<()> { Ok(()) }

    // ── Extension RPC ──
    /// 插件私有 RPC 方法（给 CLI/外部调试用）。
    /// 外部通过 `extension_rpc memory save {...}` 调用此方法。
    /// 默认返回 method_not_found，插件覆盖需要的分支。
    async fn on_extension_rpc(
        &self,
        _method: &str,
        _params: serde_json::Value,
    ) -> AgentResult<serde_json::Value> {
        Err(AgentError::Tool("extension rpc method not found".into()))
    }

    // ── Permission (stub) ──
    /// Called when a permission check is needed.
    async fn on_permission_request(&self, _tool: &str, _args: &serde_json::Value) -> AgentResult<()> { Ok(()) }
    /// Called before each LLM request to allow extensions to modify the system prompt.
    /// The `prompt` string starts as the agent's current system prompt.
    async fn on_system_prompt(&self, _prompt: &mut String) -> AgentResult<()> { Ok(()) }

    // ── Workflow gate (1) ──
    /// Called when the LLM decides to Stop (no more tool calls).
    /// Return `RetryWith(msg)` to force the loop to continue with an injected message.
    /// Return `Allow` to let the agent stop normally.
    /// This is the kernel-enforced gate check — the LLM cannot skip it.
    async fn on_gate_check(&self, _ctx: &TurnContext) -> AgentResult<GateDecision> {
        Ok(GateDecision::Allow)
    }

    // ── Singleton lifecycle（host 级单例扩展，场景 3）──
    // 这些钩子仅对 is_singleton()=true 的扩展生效。
    // 内核通过 singleton_key() 聚合相同单例，保证整个 host 只创建一份。
    // 引用计数由内核维护（SingletonRegistry），扩展不用自己数。
    // 某个 Worker 崩溃 → on_user_leave 触发，但单例不关（还有别的 Worker 在用）。
    // 最后一个 Worker 离开 → on_last_user_gone 触发，单例可决定是否关闭。
    // host 确定性关闭 → on_singleton_shutdown 触发。

    /// 是否单例。true = 整个 host 只创建一份（host 级）。
    /// false = 每个 Worker 创建一份（会话级，默认）。
    fn is_singleton(&self) -> bool { false }

    /// 单例的唯一标识。is_singleton()=true 时必须返回非空。
    /// 相同 key = 同一个单例（只创建一份，多 Worker 共享）。
    /// 不同 key = 不同的单例（各创建一份）。
    fn singleton_key(&self) -> &str { "" }

    /// 单例创建时调用（host 启动，只一次）。
    /// 在此打开 DB / 注册服务等轻量初始化。
    async fn on_singleton_init(&self) -> AgentResult<()> { Ok(()) }

    /// 单例 init 之后调用，拿到 WorkerRegistry 句柄。
    /// 在此 spawn 系统级 Worker（如 Active Memory sub-agent）——
    /// host 端直接操作 registry（不走 bridge），比 Worker 内 create_worker 更简单。
    /// 默认空实现（向后兼容，不影响现有单例）。
    async fn on_singleton_post_init(
        &self,
        _registry: &std::sync::Arc<tokio::sync::Mutex<crate::worker_registry::WorkerRegistry>>,
    ) -> AgentResult<()> {
        let _ = _registry; // 避免 unused warning
        Ok(())
    }

    /// 有 Worker 开始使用此单例时调用（引用计数 +1）。
    async fn on_user_join(&self, _worker_id: &str) -> AgentResult<()> { Ok(()) }

    /// 有 Worker 停止使用此单例时调用（引用计数 -1）。
    /// 某个 Worker 崩溃/退出 → 触发此钩子，但单例不关。
    async fn on_user_leave(&self, _worker_id: &str) -> AgentResult<()> { Ok(()) }

    /// 最后一个用户离开时调用（引用计数 == 0）。
    /// 单例可在此决定是否关闭自己。
    async fn on_last_user_gone(&self) -> AgentResult<()> { Ok(()) }

    /// host 确定性关闭时调用（ion serve shutdown）。
    async fn on_singleton_shutdown(&self) -> AgentResult<()> { Ok(()) }
}

// ---------------------------------------------------------------------------
// GateDecision — workflow gate result
// ---------------------------------------------------------------------------

/// Result of a workflow gate check.
#[derive(Clone, Debug)]
pub enum GateDecision {
    /// Gate passed — allow the agent to stop.
    Allow,
    /// Gate failed — inject `msg` as a user message and force another loop iteration.
    /// The LLM will see this message and must fix the issue before it can stop.
    RetryWith(String),
}

// ---------------------------------------------------------------------------
// FileSystemCapability — ctx.fs 统一文件访问（给扩展用，不走裸 std::fs）
// ---------------------------------------------------------------------------

/// 扩展的 4 级数据目录（对齐 pi ExtensionContext，EXTENSION_HOST_API.md §2.5）。
///
/// 内置 Rust 扩展通过 `registry.data_dirs(self.name())` 拿到自己的 4 级目录。
/// 每一级按 ext_name 隔离，互不干扰。WASM 扩展走散装 host 函数（host_read_global_data 等）。
///
/// | 级别 | 路径 | 隔离维度 |
/// |------|------|---------|
/// | global | `~/.ion/agent/extensions-data/<ext>/` | 全局（所有项目共享） |
/// | project | `~/.ion/agent/project-data/<git_key>/<ext>/` | 项目（worktree 共享） |
/// | cwd | `~/.ion/agent/cwd-data/<encoded-cwd>/<ext>/` | cwd（worktree 独立） |
/// | session | `sessions/<hash>/data/<sid>/<ext>/` | 会话级 |
#[derive(Clone, Debug)]
pub struct ExtensionDataDirs {
    /// 会话级：每会话独立，会话结束可清理
    pub session: std::path::PathBuf,
    /// CWD 级：按工作目录隔离（worktree 各自独立）
    pub cwd: std::path::PathBuf,
    /// 项目级：按 git common dir 隔离（主仓库 + worktree 共享）
    pub project: std::path::PathBuf,
    /// 全局级：所有项目共享
    pub global: std::path::PathBuf,
}

/// 目录条目（list_dir 返回）
#[derive(Clone, Debug)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

/// 文件系统能力——扩展通过它访问文件（而不是裸 std::fs）。
///
/// 走 Runtime 路由（本地/沙箱/远程透明），受 allowed_roots 白名单管控。
/// 对齐 pi 的 `ExtensionContext.fs`（FileSystemCapability）。
#[async_trait::async_trait]
pub trait FileSystemCapability: Send + Sync {
    /// 读文件全文。路径必须在 allowed_roots 之内（否则报错）。
    async fn read_file(&self, path: &str) -> Result<String, String>;

    /// 写文件。路径必须在 allowed_roots 之内。
    async fn write_file(&self, path: &str, content: &str) -> Result<(), String>;

    /// 列目录，返回每个条目的 name/is_dir/size。
    async fn list_dir(&self, path: &str) -> Result<Vec<DirEntry>, String>;

    /// 文件是否存在。
    async fn path_exists(&self, path: &str) -> bool;

    /// 简化版 glob：在 allowed_roots[0] 下按 `*` 通配符匹配相对路径。
    /// 不引入 regex/glob crate，只支持 `*`（单段）和 `**`（多段）。
    async fn glob(&self, pattern: &str) -> Result<Vec<String>, String>;
}

/// 基于 Runtime 的 FileSystemCapability 实现。
///
/// 把文件操作委托给 Runtime（走本地/沙箱/远程路由），
/// 同时用 allowed_roots 白名单 + safe_join 防 `../../../` 逃逸。
pub struct RuntimeFileSystem {
    runtime: std::sync::Arc<dyn crate::runtime::Runtime>,
    /// 允许访问的根目录白名单（canonicalized）。路径必须落在其中之一下面。
    allowed_roots: Vec<std::path::PathBuf>,
}

impl RuntimeFileSystem {
    /// 构造。`allowed_roots` 会被 fs-canonicalize（解析符号链接成 realpath）。
    ///
    /// 注意：`safe_join` 对目标路径也用同一套 fs-canonicalize（resolve_symlinks），
    /// 保证 root 和目标在同一坐标系（如 macOS /var ↔ /private/var 不会误判逃逸）。
    /// 路径逃逸（`../`）的拦截在 fs-canonicalize 之前的字符串级规范化阶段完成，
    /// 避免 TOCTOU：先字符串规范化去掉 `..`，再做 fs-canonicalize。
    pub fn new(
        runtime: std::sync::Arc<dyn crate::runtime::Runtime>,
        allowed_roots: Vec<std::path::PathBuf>,
    ) -> Self {
        let allowed_roots = allowed_roots
            .into_iter()
            .map(|p| std::fs::canonicalize(&p).unwrap_or(p))
            .collect();
        Self { runtime, allowed_roots }
    }

    /// 默认 allowed_roots：项目根目录 + ~/.ion/
    pub fn default_allowed_roots(project_root: &std::path::Path) -> Vec<std::path::PathBuf> {
        vec![
            project_root.to_path_buf(),
            crate::paths::root(),
        ]
    }

    /// 路径安全检查：确保 `path` 解析后仍在某个 allowed_root 之内（防 `../../../` 逃逸）。
    ///
    /// 步骤：
    /// 1. null byte 拒绝
    /// 2. 字符串级规范化（解析 `.` / `..`，不访问文件系统）—— 拦截 `../` 逃逸
    /// 3. fs-canonicalize（解析符号链接成 realpath）—— 与 canonicalize 过的 root 对齐坐标系
    /// 4. 检查是否落在某个 root 之下
    ///
    /// 返回规范化后的绝对路径字符串，或逃逸错误。
    pub fn safe_join(&self, path: &str) -> Result<String, String> {
        if path.contains('\0') {
            return Err(format!("path contains null byte: {path}"));
        }
        let p = std::path::Path::new(path);
        let resolved = if p.is_absolute() {
            p.to_path_buf()
        } else {
            // 相对路径相对第一个 root 解析（无 root 报错）
            let base = self
                .allowed_roots
                .first()
                .ok_or_else(|| "no allowed_roots configured".to_string())?;
            base.join(p)
        };
        // 1) 字符串级规范化（不访问文件系统）：解析 . 和 .. —— 这一步拦截 ../../../ 逃逸
        let canon = canonicalize_path_buf(&resolved);
        // 2) 解析符号链接成 realpath（与 fs-canonicalize 过的 root 对齐）。
        //    安全性：上一步已去掉 `..`，这里 canonicalize 不会引入逃逸（符号链接若指向
        //    root 外，仍会被下面的 starts_with 检查拦住）。
        let canon = resolve_symlinks(&canon);
        for root in &self.allowed_roots {
            let root_canon = canonicalize_path_buf(root);
            if canon.starts_with(&root_canon) {
                return Ok(canon.to_string_lossy().to_string());
            }
        }
        Err(format!("path '{}' outside allowed roots", path))
    }
}

/// 解析符号链接成 realpath，但不要求整条路径都存在。
///
/// 找到最长存在的祖先前缀，canonicalize 它（解析符号链接），
/// 再把不存在的尾部拼回去。这样 macOS `/var` ↔ `/private/var` 能对齐，
/// 而读一个还不存在的文件（write_file）也不会因为 canonicalize 失败而报错。
fn resolve_symlinks(p: &std::path::Path) -> std::path::PathBuf {
    // 整条都存在 → 直接 canonicalize
    if let Ok(c) = std::fs::canonicalize(p) {
        return c;
    }
    // 否则：从尾向前找最长存在的祖先，canonicalize 它，拼回不存在的尾部
    let mut existing = p.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    while !existing.exists() {
        let fname = existing.file_name().map(|s| s.to_owned());
        match fname {
            Some(f) => {
                tail.push(f);
                if !existing.pop() {
                    break;
                }
            }
            None => break,
        }
    }
    match std::fs::canonicalize(&existing) {
        Ok(mut c) => {
            for t in tail.into_iter().rev() {
                c.push(t);
            }
            c
        }
        Err(_) => p.to_path_buf(),
    }
}

#[async_trait::async_trait]
impl FileSystemCapability for RuntimeFileSystem {
    async fn read_file(&self, path: &str) -> Result<String, String> {
        let safe_path = self.safe_join(path)?;
        self.runtime.read_file(&safe_path).await
    }

    async fn write_file(&self, path: &str, content: &str) -> Result<(), String> {
        let safe_path = self.safe_join(path)?;
        self.runtime.write_file(&safe_path, content).await
    }

    async fn list_dir(&self, path: &str) -> Result<Vec<DirEntry>, String> {
        let safe_path = self.safe_join(path)?;
        let entries = self.runtime.list_dir(&safe_path).await?;
        // 把字符串条目转成 DirEntry（补 is_dir/size 元数据）
        let mut out = Vec::with_capacity(entries.len());
        for name in entries {
            let full = std::path::Path::new(&safe_path).join(&name);
            let meta = std::fs::metadata(&full).ok();
            out.push(DirEntry {
                name,
                is_dir: meta.as_ref().map(|m| m.is_dir()).unwrap_or(false),
                size: meta.as_ref().map(|m| m.len()).unwrap_or(0),
            });
        }
        Ok(out)
    }

    async fn path_exists(&self, path: &str) -> bool {
        let Ok(safe_path) = self.safe_join(path) else {
            return false;
        };
        self.runtime.path_exists(&safe_path).await
    }

    async fn glob(&self, pattern: &str) -> Result<Vec<String>, String> {
        // 简化 glob：相对 allowed_roots[0] 遍历，按 * / ** 匹配。
        // 不引入外部 crate；只支持 *（单段非分隔符）和 **（跨段）。
        let root = self
            .allowed_roots
            .first()
            .ok_or_else(|| "no allowed_roots configured".to_string())?;
        let root_canon = canonicalize_path_buf(root);
        let matches = glob_walk(&root_canon, pattern);
        Ok(matches)
    }
}

/// 字符串级路径规范化（不访问文件系统）：解析 . 和 ..，保留其余分量。
fn canonicalize_path_buf(p: &std::path::Path) -> std::path::PathBuf {
    let mut parts: Vec<std::ffi::OsString> = Vec::new();
    for comp in p.components() {
        use std::path::Component;
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if !parts.is_empty() {
                    parts.pop();
                }
            }
            Component::RootDir | Component::Prefix(_) => parts.push(comp.as_os_str().to_owned()),
            Component::Normal(s) => parts.push(s.to_owned()),
        }
    }
    parts.iter().collect()
}

/// 简化 glob：递归遍历 `base`，返回匹配 `pattern`（相对 base）的路径。
/// 支持 `*`（单段，不含路径分隔符）和 `**`（匹配任意层级）。
fn glob_walk(base: &std::path::Path, pattern: &str) -> Vec<String> {
    let segs: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    let mut out = Vec::new();
    glob_rec(base, base, &segs, 0, &mut out);
    out.sort();
    out
}

fn glob_rec(
    base: &std::path::Path,
    cur: &std::path::Path,
    segs: &[&str],
    idx: usize,
    out: &mut Vec<String>,
) {
    if idx >= segs.len() {
        // 匹配到末尾，记录（相对 base 的路径）
        if let Ok(rel) = cur.strip_prefix(base) {
            if !rel.as_os_str().is_empty() {
                out.push(rel.to_string_lossy().to_string());
            }
        }
        return;
    }
    let seg = segs[idx];
    let rd = match std::fs::read_dir(cur) {
        Ok(r) => r,
        Err(_) => return,
    };
    for entry in rd.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if seg == "**" {
            // ** 匹配任意层级：递归进子目录，并尝试匹配当前段之后的剩余段
            let child = entry.path();
            // 1. ** 继续吃（跨层）
            glob_rec(base, &child, segs, idx, out);
            // 2. ** 结束，匹配下一段
            glob_rec(base, &child, segs, idx + 1, out);
        } else if wildcard_match(seg, &name_str) {
            let child = entry.path();
            glob_rec(base, &child, segs, idx + 1, out);
        }
    }
}

/// `*` 通配符匹配（单段，不含 /）。`?` 匹配单个字符。
fn wildcard_match(pattern: &str, text: &str) -> bool {
    fn rec(p: &[u8], t: &[u8]) -> bool {
        match (p.first(), t.first()) {
            (None, None) => true,
            (None, Some(_)) => false,
            (Some(b'*'), _) => rec(&p[1..], t) || (!t.is_empty() && rec(p, &t[1..])),
            (Some(b'?'), Some(_)) => rec(&p[1..], &t[1..]),
            (Some(&pc), Some(&tc)) if pc == tc => rec(&p[1..], &t[1..]),
            _ => false,
        }
    }
    rec(pattern.as_bytes(), text.as_bytes())
}

// ---------------------------------------------------------------------------
// ExtensionRegistry
// ---------------------------------------------------------------------------

pub struct ExtensionRegistry {
    extensions: Vec<Box<dyn Extension>>,
    /// 内核权限引擎（可选，用于工具执行前权限检查）
    pub permission_engine: Option<crate::kernel::PermissionEngine>,
    /// UI 事件系统（可选，用于确认弹窗）
    pub ui_system: Option<crate::kernel::UiSystem>,
    /// ctx.fs 统一文件访问能力（可选）。扩展通过 registry.filesystem() 拿到。
    pub fs: Option<std::sync::Arc<dyn FileSystemCapability>>,
    /// 存储上下文（可选）。扩展通过 registry.data_dirs(name) 拿 4 级数据目录。
    pub storage: Option<crate::storage_context::StorageContext>,
    /// 运行时 flag 值（extension_name → flag_name → value）
    /// 静态定义在 ExtensionDef.flags，运行时值覆盖 default
    runtime_flags: std::sync::Mutex<std::collections::HashMap<String, std::collections::HashMap<String, serde_json::Value>>>,
}

impl Default for ExtensionRegistry {
    fn default() -> Self { Self::new() }
}

impl ExtensionRegistry {
    pub fn new() -> Self {
        Self {
            extensions: Vec::new(),
            permission_engine: None,
            ui_system: None,
            fs: None,
            storage: None,
            runtime_flags: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// 启用权限引擎（带默认规则）
    pub fn with_permissions(mut self, engine: crate::kernel::PermissionEngine) -> Self {
        self.permission_engine = Some(engine);
        self
    }

    /// 启用 UI 系统
    pub fn with_ui(mut self, ui: crate::kernel::UiSystem) -> Self {
        self.ui_system = Some(ui);
        self
    }

    /// 注入 ctx.fs 统一文件访问能力（RuntimeFileSystem）。
    /// 扩展通过 `registry.filesystem()` 拿到 `Arc<dyn FileSystemCapability>`。
    pub fn with_filesystem(mut self, fs: std::sync::Arc<dyn FileSystemCapability>) -> Self {
        self.fs = Some(fs);
        self
    }

    /// 获取 ctx.fs（扩展在钩子里调用）。
    pub fn filesystem(&self) -> Option<&std::sync::Arc<dyn FileSystemCapability>> {
        self.fs.as_ref()
    }

    /// 注入存储上下文（StorageContext）。扩展通过 `registry.data_dirs(name)` 拿 4 级数据目录。
    pub fn with_storage(mut self, ctx: crate::storage_context::StorageContext) -> Self {
        self.storage = Some(ctx);
        self
    }

    /// 按 ext_name 计算扩展的 4 级数据目录（global/project/cwd/session）。
    ///
    /// 复用 StorageContext 已有的 4 个目录方法（委托 paths.rs）。
    /// 没注入 storage 时返回 None（调用方安全降级）。
    pub fn data_dirs(&self, ext_name: &str) -> Option<ExtensionDataDirs> {
        let s = self.storage.as_ref()?;
        Some(ExtensionDataDirs {
            global: s.global_dir(ext_name),
            project: s.project_dir(ext_name),
            cwd: s.cwd_dir(ext_name),
            session: s.session_dir(ext_name),
        })
    }

    pub fn register(&mut self, ext: Box<dyn Extension>) { self.extensions.push(ext); }
    pub fn is_empty(&self) -> bool { self.extensions.is_empty() }

    /// 获取扩展的 flag 值（运行时值优先，否则 default）
    pub fn get_flags(&self, extension_name: &str) -> serde_json::Value {
        // 先从运行时存储取
        let runtime = self.runtime_flags.lock().unwrap();
        if let Some(ext_flags) = runtime.get(extension_name) {
            return serde_json::Value::Object(
                ext_flags.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
            );
        }
        // 没有 → 返回空对象（静态定义在 ExtensionDef 里，运行时不一定能拿到）
        serde_json::json!({})
    }

    /// 设置扩展的运行时 flag 值
    pub fn set_flag(&self, extension_name: &str, flag_name: &str, value: serde_json::Value) {
        let mut runtime = self.runtime_flags.lock().unwrap();
        runtime
            .entry(extension_name.to_string())
            .or_default()
            .insert(flag_name.to_string(), value);
    }

    /// 读取单个 flag 的运行时值（扩展内部用）
    pub fn get_flag(&self, extension_name: &str, flag_name: &str) -> Option<serde_json::Value> {
        let runtime = self.runtime_flags.lock().unwrap();
        runtime.get(extension_name)?.get(flag_name).cloned()
    }
    pub fn len(&self) -> usize { self.extensions.len() }

    /// 列出所有扩展名（get_extensions RPC 用）
    pub fn names(&self) -> Vec<String> {
        self.extensions.iter().map(|e| e.name().to_string()).collect()
    }

    /// Returns the list of loaded extension names by iterating self.extensions.
    pub fn loaded_extension_names(&self) -> Vec<String> {
        self.extensions.iter().map(|e| e.name().to_string()).collect()
    }

    pub async fn on_session_start(&self, ctx: &SessionContext) -> AgentResult<()> {
        for ext in &self.extensions { ext.on_session_start(ctx).await?; } Ok(())
    }
    pub async fn on_session_shutdown(&self, ctx: &SessionContext) -> AgentResult<()> {
        for ext in &self.extensions { ext.on_session_shutdown(ctx).await?; } Ok(())
    }
    pub async fn on_session_before_compact(&self, msgs: &mut Vec<Message>) -> AgentResult<()> {
        for ext in &self.extensions { ext.on_session_before_compact(msgs).await?; } Ok(())
    }
    pub async fn on_session_compact(&self, msgs: &mut Vec<Message>) -> AgentResult<()> {
        for ext in &self.extensions { ext.on_session_compact(msgs).await?; } Ok(())
    }
    pub async fn on_session_before_switch(&self, ctx: &SessionSwitchContext) -> AgentResult<()> {
        for ext in &self.extensions { ext.on_session_before_switch(ctx).await?; } Ok(())
    }
    pub async fn on_session_before_fork(&self, ctx: &SessionSwitchContext) -> AgentResult<()> {
        for ext in &self.extensions { ext.on_session_before_fork(ctx).await?; } Ok(())
    }
    pub async fn on_input(&self, ctx: &mut InputContext) -> AgentResult<()> {
        for ext in &self.extensions { ext.on_input(ctx).await?; } Ok(())
    }
    pub async fn before_agent_start(&self, ctx: &mut BeforeAgentContext) -> AgentResult<()> {
        for ext in &self.extensions { ext.before_agent_start(ctx).await?; } Ok(())
    }
    pub async fn on_agent_start(&self, ctx: &AgentContext) -> AgentResult<()> {
        for ext in &self.extensions { ext.on_agent_start(ctx).await?; } Ok(())
    }
    pub async fn on_agent_end(&self, ctx: &AgentContext) -> AgentResult<()> {
        for ext in &self.extensions { ext.on_agent_end(ctx).await?; } Ok(())
    }
    pub async fn on_turn_start(&self, ctx: &mut TurnContext) -> AgentResult<()> {
        for ext in &self.extensions { ext.on_turn_start(ctx).await?; } Ok(())
    }
    pub async fn on_turn_end(&self, ctx: &TurnContext) -> AgentResult<()> {
        for ext in &self.extensions { ext.on_turn_end(ctx).await?; } Ok(())
    }
    pub async fn on_context(&self, msgs: &mut Vec<Message>) -> AgentResult<()> {
        for ext in &self.extensions { ext.on_context(msgs).await?; } Ok(())
    }
    pub async fn before_provider_request(&self, ctx: &ProviderRequestContext) -> AgentResult<()> {
        for ext in &self.extensions { ext.before_provider_request(ctx).await?; } Ok(())
    }
    pub async fn after_provider_response(&self, ctx: &ProviderResponseContext) -> AgentResult<()> {
        for ext in &self.extensions { ext.after_provider_response(ctx).await?; } Ok(())
    }
    pub async fn on_auto_retry_start(&self, attempt: u32, max_retries: u32) -> AgentResult<()> {
        for ext in &self.extensions { ext.on_auto_retry_start(attempt, max_retries).await?; } Ok(())
    }
    pub async fn on_auto_retry_end(&self, success: bool, attempt: u32) -> AgentResult<()> {
        for ext in &self.extensions { ext.on_auto_retry_end(success, attempt).await?; } Ok(())
    }
    pub async fn on_message_start(&self, role: &str, content: &str) -> AgentResult<()> {
        for ext in &self.extensions { ext.on_message_start(role, content).await?; } Ok(())
    }
    pub async fn on_message_delta(&self, delta: &str, role: &str) -> AgentResult<()> {
        for ext in &self.extensions { ext.on_message_delta(delta, role).await?; } Ok(())
    }
    pub async fn on_message_end(&self, role: &str, content: &str, usage: &Usage) -> AgentResult<()> {
        for ext in &self.extensions { ext.on_message_end(role, content, usage).await?; } Ok(())
    }
    pub async fn on_thinking_delta(&self, delta: &str) -> AgentResult<()> {
        for ext in &self.extensions { ext.on_thinking_delta(delta).await?; } Ok(())
    }
    pub async fn on_thinking_end(&self, content: &str) -> AgentResult<()> {
        for ext in &self.extensions { ext.on_thinking_end(content).await?; } Ok(())
    }
    pub async fn on_tool_call_delta(&self, delta: &str, name: &str) -> AgentResult<()> {
        for ext in &self.extensions { ext.on_tool_call_delta(delta, name).await?; } Ok(())
    }
    pub async fn on_text_end(&self, content: &str) -> AgentResult<()> {
        for ext in &self.extensions { ext.on_text_end(content).await?; } Ok(())
    }
    pub async fn on_tool_call_end(&self, tool_call: &ToolCall) -> AgentResult<()> {
        for ext in &self.extensions { ext.on_tool_call_end(tool_call).await?; } Ok(())
    }
    /// 转发 on_before_tool_execute（增量 save 钩子）
    pub async fn on_before_tool_execute(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        messages: &[crate::agent::messages::Message],
    ) -> AgentResult<()> {
        for ext in &self.extensions {
            ext.on_before_tool_execute(tool_name, args, messages).await?;
        }
        Ok(())
    }
    pub async fn on_tool_execution_start(&self, ctx: &ToolExecutionContext) -> AgentResult<()> {
        for ext in &self.extensions { ext.on_tool_execution_start(ctx).await?; } Ok(())
    }
    pub async fn on_tool_execution_update(&self, ctx: &ToolExecutionContext, partial: &str) -> AgentResult<()> {
        for ext in &self.extensions { ext.on_tool_execution_update(ctx, partial).await?; } Ok(())
    }
    pub async fn on_tool_execution_end(&self, ctx: &ToolExecutionContext) -> AgentResult<()> {
        for ext in &self.extensions { ext.on_tool_execution_end(ctx).await?; } Ok(())
    }
    pub async fn before_tool_call(&self, call: &ToolCall) -> AgentResult<()> {
        for ext in &self.extensions { ext.before_tool_call(call).await?; } Ok(())
    }
    pub async fn after_tool_call(&self, call: &ToolCall, result: &ToolResult) -> AgentResult<()> {
        for ext in &self.extensions { ext.after_tool_call(call, result).await?; } Ok(())
    }
    pub async fn on_model_select(&self, ctx: &mut ModelSelectContext) -> AgentResult<()> {
        for ext in &self.extensions { ext.on_model_select(ctx).await?; } Ok(())
    }
    pub async fn on_thinking_level_select(&self, level: &str, old: Option<&str>) -> AgentResult<()> {
        for ext in &self.extensions { ext.on_thinking_level_select(level, old).await?; } Ok(())
    }
    pub async fn on_system_prompt(&self, prompt: &mut String) -> AgentResult<()> {
        for ext in &self.extensions { ext.on_system_prompt(prompt).await?; } Ok(())
    }

    /// 通知扩展：消息数组被软删除/折叠操作修改了。
    pub async fn on_entries_invalidated(&self, entry_ids: &[String]) -> AgentResult<()> {
        for ext in &self.extensions { ext.on_entries_invalidated(entry_ids).await?; } Ok(())
    }

    /// 路由 extension_rpc 到对应名称的扩展。
    /// 按 `extension` 名匹配 extension，找到后调 `on_extension_rpc`。
    pub async fn extension_rpc(
        &self,
        extension_name: &str,
        method: &str,
        params: serde_json::Value,
    ) -> AgentResult<serde_json::Value> {
        for ext in &self.extensions {
            // 如果指定了扩展名，只调匹配的 extension
            if !extension_name.is_empty() && ext.name() != extension_name {
                continue;
            }
            let result = ext.on_extension_rpc(method, params.clone()).await;
            match result {
                Ok(v) => return Ok(v),
                Err(AgentError::Tool(ref msg)) if msg == "extension rpc method not found" => continue,
                Err(e) => return Err(e),
            }
        }
        Err(AgentError::Tool(format!("extension '{extension_name}' not found or method '{method}' not implemented")))
    }

    /// Check all registered extensions' gates. Returns the first RetryWith (failure),
    /// or Allow if all gates pass. Called by agent_loop when the LLM decides to Stop.
    pub async fn check_gates(&self, ctx: &TurnContext) -> AgentResult<GateDecision> {
        for ext in &self.extensions {
            let decision = ext.on_gate_check(ctx).await?;
            if matches!(decision, GateDecision::RetryWith(_)) {
                return Ok(decision);
            }
        }
        Ok(GateDecision::Allow)
    }
}

// ---------------------------------------------------------------------------
// Extension loader — JSON definition files
// ---------------------------------------------------------------------------

/// Load extensions from `--extension <path>` arguments.
/// Expects JSON files with the following structure:
/// ```json
/// {
///   "name": "my-extension",
///   "description": "...",
///   "tools": [ ... ],          // Optional: tools to register
///   "systemPrompt": "...",     // Optional: appended to system prompt
///   "flags": { ... }           // Optional: CLI flags
/// }
/// ```
pub fn load_extensions(paths: &[String]) -> Vec<Box<dyn Extension>> {
    let mut exts: Vec<Box<dyn Extension>> = Vec::new();
    for path in paths {
        match std::fs::read_to_string(path) {
            Ok(content) => {
                match serde_json::from_str::<ExtensionDef>(&content) {
                    Ok(def) => {
                        tracing::info!("loaded extension: {} ({})", def.name, path);
                        exts.push(Box::new(GenericExtension { def }));
                    }
                    Err(e) => {
                        tracing::warn!("failed to parse extension {path}: {e}");
                    }
                }
            }
            Err(e) => {
                tracing::warn!("failed to read extension {path}: {e}");
            }
        }
    }
    exts
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ExtensionDef {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub tools: Vec<ToolDefEntry>,
    #[serde(default)]
    pub flags: std::collections::HashMap<String, FlagDef>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ToolDefEntry {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct FlagDef {
    pub description: String,
    #[serde(default)]
    pub r#type: String,
    #[serde(default)]
    pub default: Option<serde_json::Value>,
}

/// A generic extension loaded from a JSON file.
/// Injects system prompt and can define tools.
struct GenericExtension {
    def: ExtensionDef,
}

#[async_trait]
impl Extension for GenericExtension {
    async fn before_agent_start(&self, ctx: &mut BeforeAgentContext) -> AgentResult<()> {
        if let Some(ref sp) = self.def.system_prompt {
            if let Some(ref mut existing) = ctx.system_prompt {
                existing.push_str("\n");
                existing.push_str(sp);
            } else {
                ctx.system_prompt = Some(sp.clone());
            }
        }
        Ok(())
    }

    async fn on_input(&self, ctx: &mut InputContext) -> AgentResult<()> {
        // Handle custom commands from the extension
        if ctx.text.starts_with('/') && ctx.text[1..].starts_with(&self.def.name) {
            ctx.handled = true;
        }
        Ok(())
    }
}

#[cfg(test)]
mod fs_tests {
    use super::*;
    use std::sync::Arc;

    /// 测试用 Runtime：直接 std::fs，不走路由。
    struct TestRuntime;

    #[async_trait::async_trait]
    impl crate::runtime::Runtime for TestRuntime {
        async fn execute_command(&self, _c: &str, _t: u64) -> Result<(String, String, i32), String> {
            Err("not supported".into())
        }
        async fn read_file(&self, path: &str) -> Result<String, String> {
            std::fs::read_to_string(path).map_err(|e| e.to_string())
        }
        async fn write_file(&self, path: &str, content: &str) -> Result<(), String> {
            std::fs::write(path, content).map_err(|e| e.to_string())
        }
        async fn edit_file(&self, _path: &str, _old: &str, _new: &str) -> Result<(), String> {
            Err("not supported".into())
        }
        async fn path_exists(&self, path: &str) -> bool {
            std::path::Path::new(path).exists()
        }
        async fn list_dir(&self, path: &str) -> Result<Vec<String>, String> {
            std::fs::read_dir(path)
                .map_err(|e| e.to_string())?
                .filter_map(|e| e.ok())
                .map(|e| Ok(e.file_name().to_string_lossy().to_string()))
                .collect()
        }
        async fn remove_file(&self, _p: &str) -> Result<(), String> { Err("not supported".into()) }
        async fn grep_search(&self, _p: &str, _path: &str) -> Result<Vec<String>, String> { Err("no".into()) }
        async fn find_files(&self, _p: &str, _n: &str) -> Result<Vec<String>, String> { Err("no".into()) }
        async fn file_info(&self, _p: &str) -> Result<Vec<crate::runtime::FileEntry>, String> { Err("no".into()) }
        fn runtime_type(&self) -> String { "test".into() }
    }

    /// 创建唯一临时目录，返回路径。测试结束手动清理。
    fn make_tmp_root() -> std::path::PathBuf {
        let id = uuid::Uuid::new_v4().to_string()[..8].to_string();
        let dir = std::env::temp_dir().join(format!("ion_fs_test_{id}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn make_fs() -> (std::path::PathBuf, RuntimeFileSystem) {
        let dir = make_tmp_root();
        let rt: Arc<dyn crate::runtime::Runtime> = Arc::new(TestRuntime);
        let fs = RuntimeFileSystem::new(rt, vec![dir.clone()]);
        (dir, fs)
    }

    #[tokio::test]
    async fn read_file_in_root_works() {
        let (dir, fs) = make_fs();
        let f = dir.join("hello.txt");
        std::fs::write(&f, "hi there").unwrap();
        let content = fs.read_file("hello.txt").await.unwrap();
        assert_eq!(content, "hi there");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn read_file_absolute_in_root_works() {
        let (dir, fs) = make_fs();
        let f = dir.join("notes.md");
        std::fs::write(&f, "# Notes").unwrap();
        let abs = f.to_string_lossy().to_string();
        let content = fs.read_file(&abs).await.unwrap();
        assert_eq!(content, "# Notes");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn read_file_outside_root_blocked() {
        let (dir, fs) = make_fs();
        // /etc/passwd 不在临时 root 下
        let err = fs.read_file("/etc/passwd").await.unwrap_err();
        assert!(err.contains("outside allowed roots"), "got: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn parent_dir_traversal_blocked() {
        let (dir, fs) = make_fs();
        let err = fs.read_file("../../../etc/passwd").await.unwrap_err();
        assert!(err.contains("outside allowed roots"), "got: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn traversal_with_subdir_blocked() {
        let (dir, fs) = make_fs();
        let err = fs.read_file("subdir/../../etc/passwd").await.unwrap_err();
        assert!(err.contains("outside allowed roots"), "got: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn null_byte_in_path_blocked() {
        let (dir, fs) = make_fs();
        let err = fs.read_file("key\0/etc/passwd").await.unwrap_err();
        assert!(err.contains("null byte"), "got: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn write_then_read_roundtrip() {
        let (dir, fs) = make_fs();
        fs.write_file("out.txt", "payload").await.unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("out.txt")).unwrap(), "payload");
        let back = fs.read_file("out.txt").await.unwrap();
        assert_eq!(back, "payload");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn list_dir_returns_entries() {
        let (dir, fs) = make_fs();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        std::fs::create_dir(dir.join("sub")).unwrap();
        let entries = fs.list_dir(".").await.unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"a.txt"));
        assert!(names.contains(&"sub"));
        let sub = entries.iter().find(|e| e.name == "sub").unwrap();
        assert!(sub.is_dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn path_exists_in_root_true() {
        let (dir, fs) = make_fs();
        std::fs::write(dir.join("x"), "1").unwrap();
        assert!(fs.path_exists("x").await);
        assert!(!fs.path_exists("nope").await);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn path_exists_outside_root_false() {
        let (dir, fs) = make_fs();
        // 路径逃逸 → exists 返回 false（而不是报错）
        assert!(!fs.path_exists("/etc/passwd").await);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn safe_join_relative_resolves_against_first_root() {
        let (dir, fs) = make_fs();
        let resolved = fs.safe_join("foo/bar.txt").unwrap();
        // root 在 new() 里被 fs-canonicalize，对比也用 canonicalize
        let expected = std::fs::canonicalize(&dir).unwrap().join("foo/bar.txt");
        assert_eq!(resolved, expected.to_string_lossy().to_string());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn safe_join_dotdot_inside_stays_in_root() {
        let (dir, fs) = make_fs();
        // a/../b → b，仍在 root 内
        let resolved = fs.safe_join("a/../b").unwrap();
        let expected = std::fs::canonicalize(&dir).unwrap().join("b");
        assert_eq!(resolved, expected.to_string_lossy().to_string());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn safe_join_traversal_outside_blocked() {
        let (dir, fs) = make_fs();
        assert!(fs.safe_join("../escape").is_err());
        assert!(fs.safe_join("/etc/passwd").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn glob_matches_simple_pattern() {
        let (dir, fs) = make_fs();
        std::fs::write(dir.join("a.txt"), "1").unwrap();
        std::fs::write(dir.join("b.txt"), "2").unwrap();
        std::fs::write(dir.join("c.md"), "3").unwrap();
        let matches = fs.glob("*.txt").await.unwrap();
        assert!(matches.contains(&"a.txt".to_string()));
        assert!(matches.contains(&"b.txt".to_string()));
        assert!(!matches.iter().any(|m| m == "c.md"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn glob_double_star_recursive() {
        let (dir, fs) = make_fs();
        std::fs::create_dir_all(dir.join("src/nested")).unwrap();
        std::fs::write(dir.join("src/nested/deep.rs"), "").unwrap();
        std::fs::write(dir.join("src/top.rs"), "").unwrap();
        let matches = fs.glob("**/*.rs").await.unwrap();
        assert!(matches.iter().any(|m| m.contains("top.rs")), "matches: {matches:?}");
        assert!(matches.iter().any(|m| m.contains("deep.rs")), "matches: {matches:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod data_dirs_tests {
    use super::*;

    #[test]
    fn data_dirs_returns_none_without_storage() {
        let reg = ExtensionRegistry::new();
        assert!(reg.data_dirs("my-ext").is_none());
    }

    #[test]
    fn data_dirs_returns_four_levels_with_ext_name() {
        let storage = crate::storage_context::StorageContext::new(
            "/proj/myapp", "sess_abc", "/proj/myapp",
        );
        let reg = ExtensionRegistry::new().with_storage(storage);
        let dirs = reg.data_dirs("my-ext").expect("data_dirs should return Some");

        // 4 级都非空
        assert!(!dirs.global.as_os_str().is_empty(), "global empty");
        assert!(!dirs.project.as_os_str().is_empty(), "project empty");
        assert!(!dirs.cwd.as_os_str().is_empty(), "cwd empty");
        assert!(!dirs.session.as_os_str().is_empty(), "session empty");

        // 每级路径都含 ext_name
        let g = dirs.global.to_string_lossy();
        let p = dirs.project.to_string_lossy();
        let c = dirs.cwd.to_string_lossy();
        let s = dirs.session.to_string_lossy();
        assert!(g.contains("my-ext"), "global should contain ext name: {g}");
        assert!(p.contains("my-ext"), "project should contain ext name: {p}");
        assert!(c.contains("my-ext"), "cwd should contain ext name: {c}");
        assert!(s.contains("my-ext"), "session should contain ext name: {s}");

        // global 在 extensions-data 下，session 在 data 下
        assert!(g.contains("extensions-data"), "global path: {g}");
        assert!(s.contains("sess_abc"), "session should contain session_id: {s}");
    }

    #[test]
    fn data_dirs_different_ext_names_isolate() {
        let storage = crate::storage_context::StorageContext::new("/p", "s1", "/p");
        let reg = ExtensionRegistry::new().with_storage(storage);
        let a = reg.data_dirs("ext-a").unwrap();
        let b = reg.data_dirs("ext-b").unwrap();
        assert_ne!(a.global, b.global, "different exts must have different dirs");
        assert_ne!(a.session, b.session);
    }
}

#[cfg(test)]
mod loaded_extension_names_tests {
    use super::*;

    /// A simple extension for testing purposes.
    struct TestExt {
        name: String,
    }

    impl TestExt {
        fn new(name: &str) -> Self {
            Self { name: name.to_string() }
        }
    }

    #[async_trait]
    impl Extension for TestExt {
        fn name(&self) -> &str {
            &self.name
        }
    }

    #[test]
    fn loaded_extension_names_returns_registered_names() {
        let mut registry = ExtensionRegistry::new();
        assert!(registry.loaded_extension_names().is_empty());

        registry.register(Box::new(TestExt::new("ext-alpha")));
        registry.register(Box::new(TestExt::new("ext-beta")));
        registry.register(Box::new(TestExt::new("ext-gamma")));

        let names = registry.loaded_extension_names();
        assert_eq!(names.len(), 3);
        assert!(names.contains(&"ext-alpha".to_string()));
        assert!(names.contains(&"ext-beta".to_string()));
        assert!(names.contains(&"ext-gamma".to_string()));
    }
}
