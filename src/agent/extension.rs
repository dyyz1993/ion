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

// ---------------------------------------------------------------------------
// Extension trait — 29 hook points matching pi spec
// ---------------------------------------------------------------------------

#[async_trait]
pub trait Extension: Send + Sync {
    /// Optional name for extension routing (used by extension_rpc dispatch).
    fn name(&self) -> &str { "anonymous" }

    // ── Session lifecycle (4) ──
    async fn on_session_start(&self, _ctx: &SessionContext) -> AgentResult<()> { Ok(()) }
    async fn on_session_shutdown(&self, _ctx: &SessionContext) -> AgentResult<()> { Ok(()) }
    async fn on_session_before_compact(&self, _msgs: &mut Vec<Message>) -> AgentResult<()> { Ok(()) }
    async fn on_session_compact(&self, _messages: &mut Vec<Message>) -> AgentResult<()> { Ok(()) }

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
    /// Called during tool execution with partial results (e.g., streaming bash output).
    async fn on_tool_execution_update(&self, _ctx: &ToolExecutionContext, _partial: &str) -> AgentResult<()> { Ok(()) }
    async fn on_tool_execution_end(&self, _ctx: &ToolExecutionContext) -> AgentResult<()> { Ok(()) }
    async fn before_tool_call(&self, _call: &ToolCall) -> AgentResult<()> { Ok(()) }
    async fn after_tool_call(&self, _call: &ToolCall, _result: &ToolResult) -> AgentResult<()> { Ok(()) }

    // ── Model (3) ──
    async fn on_model_select(&self, _ctx: &ModelSelectContext) -> AgentResult<()> { Ok(()) }
    async fn on_thinking_level_select(&self, _level: &str, _old: Option<&str>) -> AgentResult<()> { Ok(()) }

    // ── Entries ──
    /// Called when entries are deleted or summarized.
    async fn on_entries_invalidated(&self, _entry_ids: &[String]) -> AgentResult<()> { Ok(()) }

    // ── Session navigation (stubs - 待对应功能实现后接入) ──
    /// Called before switching to another session. Can cancel.
    async fn on_session_before_switch(&self, _target: &str) -> AgentResult<()> { Ok(()) }
    /// Called before forking a session. Can cancel.
    async fn on_session_before_fork(&self, _entry_id: &str) -> AgentResult<()> { Ok(()) }
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
    /// 在此创建 Memory Agent / 打开 DB / 注册服务等。
    async fn on_singleton_init(&self) -> AgentResult<()> { Ok(()) }

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
// ExtensionRegistry
// ---------------------------------------------------------------------------

pub struct ExtensionRegistry {
    extensions: Vec<Box<dyn Extension>>,
    /// 内核权限引擎（可选，用于工具执行前权限检查）
    pub permission_engine: Option<crate::kernel::PermissionEngine>,
    /// UI 事件系统（可选，用于确认弹窗）
    pub ui_system: Option<crate::kernel::UiSystem>,
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

    pub fn register(&mut self, ext: Box<dyn Extension>) { self.extensions.push(ext); }
    pub fn is_empty(&self) -> bool { self.extensions.is_empty() }
    pub fn len(&self) -> usize { self.extensions.len() }

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
    pub async fn on_model_select(&self, ctx: &ModelSelectContext) -> AgentResult<()> {
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
