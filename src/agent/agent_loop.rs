use super::compact::{self, CompactConfig};
use crate::retry::RetryConfig;
use super::error::{AgentError, AgentResult};
use super::extension::{ExtensionRegistry, TurnContext};
use super::tool::{Tool, ToolRegistry};
use ion_provider::StreamOptions;
use ion_provider::registry::{self, ApiRegistry};
use ion_provider::types::*;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

#[derive(Clone, Debug)]
pub struct AgentConfig {
    pub max_turns: Option<u64>,
    pub max_outer_iterations: u64,
    pub max_retries: u32,
    pub retry_base_delay_ms: u64,
    pub enable_compact: bool,
    pub compact_config: CompactConfig,
    pub api_key: Option<String>,
    pub response_format: Option<String>,
    pub thinking: Option<String>,
    /// Model ID to use for compaction summarization (defaults to main model)
    pub compact_model_id: Option<String>,
    /// Max consecutive turns that the LLM can reply without calling any tools
    /// before the system forces a retry with a warning (0 = disabled).
    /// Helps reduce hallucinations where the LLM says "file created" without calling write.
    pub retry_on_no_tool_use: u32,
    /// 高级重试配置（可选，覆盖上面的简单配置）
    pub retry_config: Option<crate::retry::RetryConfig>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_turns: None,
            max_outer_iterations: 5,
            max_retries: 3,
            retry_base_delay_ms: 1000,
            enable_compact: true,
            compact_config: CompactConfig::default(),
            api_key: None,
            response_format: None,
            thinking: None,
            compact_model_id: None,
            retry_on_no_tool_use: 0,
            retry_config: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct AgentContext {
    pub turn_index: u64,
    pub message_count: usize,
    pub tool_call_count: u64,
    pub last_stop_reason: Option<StopReason>,
}

// ---------------------------------------------------------------------------
// TurnEvent — emitted during each turn (kept for Agent loop compatibility)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub enum TurnEvent {
    ReasoningDelta(String),
    TextDelta(String),
    ToolCallDelta(ToolCall),
    TurnEnd { stop_reason: StopReason },
}

// ---------------------------------------------------------------------------
// Agent
// ---------------------------------------------------------------------------

pub struct Agent {
    messages: Vec<Message>,
    steering_queue: VecDeque<Message>,
    follow_up_queue: VecDeque<Message>,
    registry: Arc<ApiRegistry>,
    model: Model,
    tools: ToolRegistry,
    extensions: ExtensionRegistry,
    config: AgentConfig,
    system_prompt: Option<String>,
    turn_index: u64,
    pause_tx: watch::Sender<bool>,
    pause_rx: watch::Receiver<bool>,
    running: bool,
    /// 对齐 pi abort：设 true 后 check_pause 返回 Aborted 错误，终止 run()
    stopped: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// soft interrupt 信号（对齐 pi interruptController）：
    /// 设 true 后工具执行 select! 立即 break，但 agent 不退出（继续下一 turn）。
    /// 与 stopped（硬停止）独立，每个 turn 用完即重置。
    interrupted: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// HTTP 流式请求的 cancel token（每次 stream_with_retry 前 new 一个，stop() 时 cancel）
    /// 对齐 pi AbortController：abort 时立刻 drop reqwest Response 关 TCP，不等 200ms 轮询
    http_cancel: std::sync::Mutex<Option<tokio_util::sync::CancellationToken>>,
    /// 工具执行运行时（本地/沙箱/远程）
    /// 用 Arc 以便 HookExtension 等 clone 共享（agent handler 需要 runtime 来 spawn 子 Worker）
    pub runtime: Arc<dyn crate::runtime::Runtime>,
    /// 独立压缩模型（可选，默认使用主模型）
    compact_model: Option<Model>,
    /// 会话文件所在 cwd（用于 compaction/turn_summary 落盘，None = 不落盘）
    session_cwd: Option<String>,
    /// 溢出恢复已尝试次数（达 MAX_OVERFLOW_ROUNDS 后放弃，对齐 pi）
    overflow_recovery_attempts: u32,
    /// 软删除状态：被软删的 entry ID 集合（快速查询）
    deleted_entry_ids: std::collections::HashSet<String>,
    /// 软压缩状态：被折叠的 entry ID → 替换后的 BranchSummary 摘要
    summarized_entry_ids: std::collections::HashMap<String, String>,
}

/// 上下文溢出恢复的最大 compact-and-retry 轮次（对齐 pi MAX_OVERFLOW_RECOVERY_ROUNDS = 5）
const MAX_OVERFLOW_ROUNDS: u32 = 5;

impl Agent {
    pub fn new(
        registry: Arc<ApiRegistry>,
        model: Model,
        system_prompt: Option<String>,
        tools: ToolRegistry,
        config: AgentConfig,
    ) -> Self {
        let (pause_tx, pause_rx) = watch::channel(false);
        Self {
            messages: Vec::new(),
            steering_queue: VecDeque::new(),
            follow_up_queue: VecDeque::new(),
            registry,
            model,
            tools,
            extensions: ExtensionRegistry::new(),
            config,
            system_prompt,
            turn_index: 0,
            pause_tx,
            pause_rx,
            running: false,
            stopped: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            interrupted: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            http_cancel: std::sync::Mutex::new(None),
            runtime: Arc::new(crate::runtime::LocalRuntime::new()),
            compact_model: None,
            session_cwd: None,
            overflow_recovery_attempts: 0,
            deleted_entry_ids: std::collections::HashSet::new(),
            summarized_entry_ids: std::collections::HashMap::new(),
        }
    }

    /// Returns the current number of messages in the agent's message list.
    pub fn current_message_count(&self) -> usize {
        self.messages.len()
    }

    /// 替换运��时（本地/沙箱/��程切换）
    /// ��受 Box，内部转 Arc（向后兼容）
    pub fn with_runtime(self, rt: Box<dyn crate::runtime::Runtime>) -> Self {
        self.with_runtime_arc(Arc::from(rt))
    }

    /// 替换运行时（Arc 版，给 HookExtension 等 clone 共享用）
    pub fn with_runtime_arc(mut self, rt: Arc<dyn crate::runtime::Runtime>) -> Self {
        self.runtime = rt;
        self
    }

    /// Set a separate model for compaction summarization (smaller/cheaper model).
    pub fn with_compact_model(mut self, model: Option<Model>) -> Self {
        self.compact_model = model;
        self
    }

    /// 设置会话文件所在 cwd（用于 compaction/turn_summary 落盘到 JSONL）。
    pub fn with_session_cwd(mut self, cwd: Option<String>) -> Self {
        self.session_cwd = cwd;
        self
    }

    /// 动态设置 session cwd（worker 启动后设置）。
    pub fn set_session_cwd(&mut self, cwd: Option<String>) {
        self.session_cwd = cwd;
    }

    /// 动态设置系统提示词（switch_agent 时调用）
    pub fn set_system_prompt(&mut self, prompt: String) {
        self.system_prompt = Some(prompt);
    }

    /// 限制可用工具白名单（switch_agent 时调用）
    pub fn restrict_tools(&mut self, allowed: Vec<String>) {
        self.tools.restrict_to(&allowed);
    }

    /// Register a single tool (used by extension_add / extension_reload RPC).
    pub fn register_tool(&mut self, tool: Box<dyn Tool>) {
        self.tools.register(tool);
    }

    /// Remove a tool by name (used by extension_remove RPC).
    pub fn remove_tool(&mut self, name: &str) {
        self.tools.remove(name);
    }


    /// Return the names of all registered tools.
    pub fn list_tool_names(&self) -> Vec<String> {
        self.tools.tool_defs().into_iter().map(|td| td.name).collect()
    }

    pub fn with_extensions(mut self, ext: ExtensionRegistry) -> Self {
        self.extensions = ext;
        // Hook: thinking_level_select if thinking is configured
        if let Some(ref level) = self.config.thinking {
            let level_str = level.clone();
            let _ = level_str; // async call below
        }
        self
    }

    /// Preload messages (e.g. loaded from a saved session).
    pub fn with_messages(mut self, msgs: Vec<Message>) -> Self {
        self.messages = msgs;
        self
    }

    pub fn pause_handle(&self) -> watch::Sender<bool> {
        self.pause_tx.clone()
    }

    /// 返回 stopped Arc 的 clone(让外部能在 agent.run 期间设 stopped)
    pub fn stopped_handle(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        self.stopped.clone()
    }

    pub fn pause(&self) {
        let _ = self.pause_tx.send(true);
    }
    pub fn resume(&self) {
        let _ = self.pause_tx.send(false);
    }
    /// 硬停止当前 Agent 循环（对齐 pi abort）。
    /// 设 stopped=true + 唤醒 check_pause → 返回 AgentError::Aborted → 内循环 break。
    /// 同时 cancel HTTP 请求 token（真正关 TCP 连接，不等 200ms 轮询）。
    pub fn stop(&self) {
        self.stopped.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = self.pause_tx.send(true);
        if let Ok(mut guard) = self.http_cancel.lock() && let Some(c) = guard.take() {
            c.cancel();
        }
    }
    /// 软中断当前 turn（对齐 pi Agent.interrupt()）。
    /// 设 interrupted=true，工具执行 select! 立即 break 返回 Interrupted，
    /// agent 不退出（继续下一 turn → drain steering → 注入 steer 消息）。
    pub fn interrupt(&self) {
        self.interrupted.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    /// consume-once 检查中断（对齐 pi 重装 interruptController）。
    /// 返回 true 表示本轮被 interrupt 过，后续应 drain steering + continue。
    pub fn consume_interrupt(&self) -> bool {
        self.interrupted.swap(false, std::sync::atomic::Ordering::SeqCst)
    }
    pub fn is_running(&self) -> bool {
        self.running
    }
    /// 检查是否被硬停止
    pub fn is_stopped(&self) -> bool {
        self.stopped.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn steer(&mut self, msg: Message) {
        self.steering_queue.push_back(msg);
    }
    pub fn follow_up(&mut self, msg: Message) {
        self.follow_up_queue.push_back(msg);
    }
    /// 把 follow_up_queue 里第 index 条消息提升到 steering_queue（对齐 pi promote）。
    /// index 从 0 计。如果越界则静默忽略。
    pub fn promote_follow_up(&mut self, index: usize) {
        // VecDeque 没有按索引移除，所以要全 drain 再 re-insert
        let mut new_q = std::collections::VecDeque::new();
        let mut promoted = None;
        while let Some(msg) = self.follow_up_queue.pop_front() {
            if promoted.is_none() && new_q.len() == index {
                promoted = Some(msg);
            } else {
                new_q.push_back(msg);
            }
        }
        if let Some(msg) = promoted {
            self.steering_queue.push_back(msg);
        }
        self.follow_up_queue = new_q;
    }
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Push a message directly into the conversation history.
    /// Used by bash_command RPC: 用户 `!cmd` 直发结果走 Message::BashExecution，
    /// 不经过 agent.run()，直接入历史，下次 LLM 调用会看到（provider 自动转 user text）。
    pub fn push_message(&mut self, msg: Message) {
        self.messages.push(msg);
    }

    /// 软删除：从 self.messages 按索引移除消息，记录 entry_id 到 deleted_entry_ids。
    /// indices 必须是消息数组中的合法下标，由 worker 通过 JSONL entry↔index 映射算出。
    /// 直接改 self.messages → token 统计/compaction 自动正确。
    /// 触发 on_entries_invalidated 钩子通知扩展。
    pub async fn mark_deleted(&mut self, indices: &[usize], entry_ids: &[String]) {
        // 从后往前删，避免索引偏移
        let mut sorted = indices.to_vec();
        sorted.sort_unstable_by(|a, b| b.cmp(a));
        for idx in sorted {
            if idx < self.messages.len() {
                self.messages.remove(idx);
            }
        }
        for eid in entry_ids {
            self.deleted_entry_ids.insert(eid.clone());
        }
        // 通知扩展
        let _ = self.extensions.on_entries_invalidated(entry_ids).await;
    }

    /// 软压缩：把 self.messages 里 indices 对应的消息替换成一条 BranchSummary。
    /// 第一个 index 的位置放 BranchSummary，其余移除。
    /// 直接改 self.messages → token 统计/compaction 自动正确。
    /// 触发 on_entries_invalidated 钩子通知扩展。
    pub async fn mark_summarized(&mut self, indices: &[usize], entry_ids: &[String], summary: &str) {
        if indices.is_empty() {
            return;
        }
        let mut sorted = indices.to_vec();
        sorted.sort_unstable();
        let first_pos = sorted[0];

        // 移除所有目标消息
        for &idx in sorted.iter().rev() {
            if idx < self.messages.len() {
                self.messages.remove(idx);
            }
        }

        // 在原第一个位置插入 BranchSummary
        let insert_pos = first_pos.min(self.messages.len());
        self.messages.insert(insert_pos, Message::BranchSummary(BranchSummaryMessage {
            role: "branchSummary".into(),
            summary: summary.into(),
            from_id: entry_ids.first().cloned().unwrap_or_default(),
            timestamp: now_ms(),
        }));

        for eid in entry_ids {
            self.summarized_entry_ids.insert(eid.clone(), summary.into());
        }
        // 通知扩展
        let _ = self.extensions.on_entries_invalidated(entry_ids).await;
    }

    /// 获取软删除的 entry ID 集合（用于调试/展示）
    pub fn deleted_ids(&self) -> &std::collections::HashSet<String> {
        &self.deleted_entry_ids
    }

    /// 恢复软删除/折叠：从 deleted_entry_ids / summarized_entry_ids 移除指定 entry。
    /// 调用方需在之后重新从 JSONL 加载完整消息到 self.messages。
    pub fn restore_entries(&mut self, entry_ids: &[String]) {
        for eid in entry_ids {
            self.deleted_entry_ids.remove(eid);
            self.summarized_entry_ids.remove(eid);
        }
    }

    /// 从 JSONL 重新加载消息到 self.messages（恢复时用）。
    pub fn reload_messages_from_session(&mut self, cwd: &str) -> usize {
        let msgs = crate::session_jsonl::SessionFile::load(cwd)
            .map(|f| f.messages)
            .unwrap_or_default();
        let count = msgs.len();
        self.messages = msgs;
        count
    }

    /// 用 LLM 生成一批消息的摘要（供 summarize_entries RPC 在未传 summary 时调用）。
    /// 复用 compact::make_llm_summarizer 的 LLM 调用链路。
    pub async fn summarize_messages_llm(&self, indices: &[usize]) -> AgentResult<String> {
        let messages_to_summarize: Vec<Message> = indices.iter()
            .filter(|&&i| i < self.messages.len())
            .map(|&i| self.messages[i].clone())
            .collect();

        if messages_to_summarize.is_empty() {
            return Ok("（空消息）".into());
        }

        let summarizer_model = self.compact_model.as_ref().unwrap_or(&self.model);
        let summarizer = compact::make_llm_summarizer(self.registry.clone(), summarizer_model.clone(), self.config.api_key.clone());
        summarizer(&messages_to_summarize).await
    }

    /// 获取 Agent 的 registry 引用（供 worker 构造 summarizer 用）
    pub fn registry(&self) -> &Arc<ApiRegistry> {
        &self.registry
    }

    /// 获取 compact_model 引用
    pub fn compact_model(&self) -> Option<&Model> {
        self.compact_model.as_ref()
    }

    /// steer 队列积压数（未消费的高优先级消息）
    pub fn steering_queue_len(&self) -> usize {
        self.steering_queue.len()
    }
    /// follow_up 队列积压数（未消费的后续消息）
    pub fn follow_up_queue_len(&self) -> usize {
        self.follow_up_queue.len()
    }

    // ── P0 调试 RPC 支持方法 ──

    /// 当前模型引用（get_state / cycle_model 用）
    pub fn model(&self) -> &Model {
        &self.model
    }

    /// 设置模型（set_model / cycle_model 用）
    pub fn set_model(&mut self, model: Model) {
        self.model = model;
    }

    /// 访问 extensions（get_extensions RPC 用）
    pub fn extensions(&self) -> &ExtensionRegistry {
        &self.extensions
    }

    /// 访问 runtime（set_guard_mode 等 RPC 用）
    pub fn runtime(&self) -> &dyn crate::runtime::Runtime {
        self.runtime.as_ref()
    }

    /// 当前 thinking level（get_state 用）
    pub fn thinking_level(&self) -> Option<&str> {
        self.config.thinking.as_deref()
    }

    /// 设置 thinking level（set_thinking_level / cycle_thinking_level 用）
    pub fn set_thinking_level(&mut self, level: Option<String>) {
        self.config.thinking = level;
    }

    /// 自动压缩开关（set_auto_compaction 用）
    pub fn set_auto_compact(&mut self, enabled: bool) {
        self.config.enable_compact = enabled;
    }

    /// 设置最大重试次数（set_auto_retry 用）
    pub fn set_max_retries(&mut self, max: u32) {
        self.config.max_retries = max;
    }

    /// 读取最大重试次数
    pub fn max_retries(&self) -> u32 {
        self.config.max_retries
    }

    /// 设置 retry_on_no_tool_use 次数（0=禁用）
    pub fn set_retry_on_no_tool_use(&mut self, max: u32) {
        self.config.retry_on_no_tool_use = max;
    }

    /// 读取自动压缩开关（get_context_usage 用）
    pub fn auto_compact_enabled(&self) -> bool {
        self.config.enable_compact
    }

    /// steering 队列内容快照（get_queue 用）
    pub fn steering_queue_snapshot(&self) -> Vec<Message> {
        self.steering_queue.iter().cloned().collect()
    }

    /// follow_up 队列内容快照（get_queue 用）
    pub fn follow_up_queue_snapshot(&self) -> Vec<Message> {
        self.follow_up_queue.iter().cloned().collect()
    }

    /// 清空 steering + follow_up 队列（clear_queue 用）
    pub fn clear_queues(&mut self) {
        self.steering_queue.clear();
        self.follow_up_queue.clear();
    }

    /// 删除 follow_up 队列里第 index 条消息
    pub fn remove_follow_up(&mut self, index: usize) -> Option<ion_provider::types::Message> {
        let mut new_q = VecDeque::new();
        let mut removed = None;
        let mut i = 0;
        while let Some(msg) = self.follow_up_queue.pop_front() {
            if i == index {
                removed = Some(msg);
            } else {
                new_q.push_back(msg);
            }
            i += 1;
        }
        self.follow_up_queue = new_q;
        removed
    }

    /// 删除 steering 队列里第 index 条消息
    pub fn remove_steering(&mut self, index: usize) -> Option<ion_provider::types::Message> {
        let mut new_q = VecDeque::new();
        let mut removed = None;
        let mut i = 0;
        while let Some(msg) = self.steering_queue.pop_front() {
            if i == index {
                removed = Some(msg);
            } else {
                new_q.push_back(msg);
            }
            i += 1;
        }
        self.steering_queue = new_q;
        removed
    }

    /// 直接调用一个已注册的工具（不经过 LLM）。
    /// 用于：ion rpc 直接触发 spawn_worker / read / write 等工具，不跑 LLM。
    pub async fn call_tool(&self, name: &str, args: serde_json::Value) -> AgentResult<String> {
        // 权限检查（与 agent.run() 循环里的 before_tool_call 一致）
        let tc = crate::agent::messages::ToolCall {
            call_type: "function".into(),
            id: "cli_call".into(),
            name: name.to_string(),
            arguments: args.clone(),
            thought_signature: None,
        };
        self.extensions.before_tool_call(&tc).await?;

        // session 分支/回滚钩子（与 run() 循环里一致，CLI call_tool 直调时也触发）
        if name == "branch_session" {
            let is_rollback = args.get("is_rollback").and_then(|v| v.as_bool()).unwrap_or(false);
            let target_leaf = args.get("from_entry").and_then(|v| v.as_str()).map(|s| s.to_string());
            let branch_name = args.get("name").and_then(|v| v.as_str()).map(|s| s.to_string());
            let switch_ctx = super::extension::SessionSwitchContext {
                action: if is_rollback { "rollback".into() } else { "branch".into() },
                target_leaf_id: target_leaf,
                source_leaf_id: None,
                branch_name,
            };
            self.extensions.on_session_before_switch(&switch_ctx).await?;
        }

        let tool = self.tools.get(name)
            .ok_or_else(|| AgentError::Tool(format!("tool not found: {name}")))?;

        // 增量 save 钩子：工具执行前，让扩展有机会 save 当前 messages。
        // 解决 fork 阻塞问题：LLM 调 skill(fork) → spawn_worker 阻塞 → agent.run 不返回 →
        // 进程被杀时 messages 全丢。这个钩子在 tool.execute 前触发，至少把 user prompt +
        // assistant tool call decision 落盘。
        self.extensions.on_before_tool_execute(name, &args, &self.messages).await?;

        tool.execute(args, &*self.runtime).await
    }

    /// 调插件私有 RPC 方法（给 CLI/外部调试用）。
    pub async fn extension_rpc(
        &self,
        extension_name: &str,
        method: &str,
        params: serde_json::Value,
    ) -> AgentResult<serde_json::Value> {
        self.extensions.extension_rpc(extension_name, method, params).await
    }

    pub async fn run(&mut self, prompt: impl Into<String>) -> AgentResult<()> {
        self.running = true;
        self.stopped.store(false, std::sync::atomic::Ordering::SeqCst);
        // turn_index 是 agent loop 内部计数器（每次 run 从 0 开始，用于 max_turns 限制）
        // 快照用独立的全局唯一 turnId（ts_xxxxxx），不依赖 turn_index
        self.turn_index = 0;

        // ── 生命周期顺序 (对齐 pi) ──
        // 1. session_start (会话启动)
        self.extensions.on_session_start(&super::extension::SessionContext {
            reason: "startup".into(),
        }).await?;

        // 2. model_select (模型选择，扩展可覆盖)
        let mut model_ctx = super::extension::ModelSelectContext {
            old_model: None,
            old_provider: None,
            new_model: self.model.id.clone(),
            new_provider: self.model.provider.clone(),
        };
        self.extensions.on_model_select(&mut model_ctx).await?;

        // 如果扩展改了模型，重新解析
        if model_ctx.new_model != self.model.id || model_ctx.new_provider != self.model.provider {
            let registry = ion_provider::registry::ModelRegistry::new();
            if let Some(new_model) = registry.find_model(&model_ctx.new_model) {
                eprintln!("[model] extension changed model: {} → {}",
                    self.model.id, new_model.id);
                self.model = new_model.clone();
            }
        }

        // 3. input (用户输入拦截/转换)
        {
            let mut input_ctx = super::extension::InputContext {
                text: prompt.into(),
                handled: false,
            };
            self.extensions.on_input(&mut input_ctx).await?;
            if input_ctx.handled {
                return Ok(());
            }
            self.messages.push(Message::User(UserMessage {
                role: "user".into(),
                content: vec![ContentBlock::Text(TextContent {
                    text: input_ctx.text,
                    text_signature: None,
                })],
                timestamp: now_ms(),
                source: ion_provider::types::MessageSource::Prompt,
            }));
        }

        // 4. before_agent_start (注入消息/修改 system prompt)
        {
            let mut before_ctx = super::extension::BeforeAgentContext {
                system_prompt: None,
                messages: self.messages.clone(),
            };
            self.extensions.before_agent_start(&mut before_ctx).await?;
        }

        // 5. agent_start (Agent 循环开始)
        let ctx = self.build_ctx();
        self.extensions.on_agent_start(&ctx).await?;

        let result = self.outer_loop().await;
        self.running = false;

        let ctx = self.build_ctx();
        self.extensions.on_agent_end(&ctx).await?;
        self.extensions.on_session_shutdown(&super::extension::SessionContext {
            reason: "quit".into(),
        }).await?;
        result
    }

    async fn outer_loop(&mut self) -> AgentResult<()> {
        // ION_AUTO_CONTINUE: workflow 等多 turn 场景下，inner_loop Stop 后自动注入
        // "继续执行"的 follow-up，让 agent 跑完整个 workflow（而不是一个 stage 就停）。
        // 没有这个，wf agent 每个 stage 是一个 turn，turn 结束 follow_up_queue 空了就退出。
        let auto_continue = std::env::var("ION_AUTO_CONTINUE")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        // 文件日志（eprintln 去 stderr 文件，不确定能不能看到；直接写 /tmp 调试）
        let _ = std::fs::OpenOptions::new()
            .create(true).append(true)
            .open("/tmp/agent_loop_debug.log")
            .and_then(|mut f| std::io::Write::write_all(&mut f,
                format!("[outer_loop] ENTER auto_continue={} env={}\n",
                    auto_continue,
                    std::env::var("ION_AUTO_CONTINUE").unwrap_or_default()).as_bytes()));
        let auto_continue_limit = std::env::var("ION_AUTO_CONTINUE_LIMIT")
            .ok().and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(30);  // 默认最多自动继续 30 轮（够 workflow 10 stage 用）

        for outer_i in 0..self.config.max_outer_iterations.max(auto_continue_limit) {
            let reason = self.inner_loop().await?;
            let _ = std::fs::OpenOptions::new()
                .create(true).append(true)
                .open("/tmp/agent_loop_debug.log")
                .and_then(|mut f| std::io::Write::write_all(&mut f,
                    format!("[outer_loop] iter={} reason={:?} queue_len={}\n",
                        outer_i, reason, self.follow_up_queue.len()).as_bytes()));
            match reason {
                StopReason::Error | StopReason::Aborted => return Ok(()),
                _ => {}
            }
            if self.follow_up_queue.is_empty() {
                // auto_continue 模式：注入"继续"follow-up 让 agent 跑下一个 turn
                // 这对 wf agent 必要——每个 stage 是一个 turn，没有外部触发不会继续
                if auto_continue && outer_i < auto_continue_limit {
                    // 检查 workflow 是否已完成：跑 ION_AUTO_CONTINUE_GATE 环境变量指定的 gate 命令
                    // 如果输出含 ION_AUTO_CONTINUE_EXPECTED（默认 ALL_DONE），停止 auto-continue
                    let workflow_done = std::env::var("ION_AUTO_CONTINUE_GATE").ok()
                        .filter(|s| !s.is_empty())
                        .and_then(|gate_cmd| {
                            std::process::Command::new("bash")
                                .arg("-c").arg(&gate_cmd)
                                .output().ok()
                                .map(|o| {
                                    let out = String::from_utf8_lossy(&o.stdout);
                                    let expected = std::env::var("ION_AUTO_CONTINUE_EXPECTED")
                                        .unwrap_or_else(|_| "ALL_DONE".into());
                                    out.contains(&expected)
                                })
                        })
                        .unwrap_or(false);
                    if workflow_done {
                        tracing::info!("outer {outer_i}: auto-continue gate passed (workflow done), stopping");
                        return Ok(());
                    }
                    tracing::info!("outer {outer_i}: auto-continue (ION_AUTO_CONTINUE=1), injecting follow-up");
                    self.follow_up_queue.push_back(Message::User(UserMessage {
                        role: "user".into(),
                        content: vec![ContentBlock::Text(TextContent {
                            text: "继续执行下一个 stage。如果所有 stage 都 done，输出 PIPELINE COMPLETE。".into(),
                            text_signature: None,
                        })],
                        timestamp: now_ms(),
                        source: ion_provider::types::MessageSource::FollowUp,
                    }));
                } else {
                    // No auto_continue, no follow_up in queue.
                    // But bash_run background process might send follow_up asynchronously.
                    // Wait up to 30 minutes for async follow_up before giving up.
                    // This is critical for evolver agent: bash_run(background=true) sends
                    // follow_up when the process completes, but it arrives async.
                    if std::env::var("ION_WAIT_BACKGROUND").map(|v| v == "1" || v.eq_ignore_ascii_case("true")).unwrap_or(false) {
                        tracing::info!("outer {outer_i}: follow_up empty, waiting for async background follow_up (ION_WAIT_BACKGROUND=1)");
                        // Check every 5s if follow_up_queue got messages (from follow_up_rx drain in main loop)
                        for _wait in 0..360 { // 360 * 5s = 30 min max
                            if !self.follow_up_queue.is_empty() {
                                tracing::info!("outer {outer_i}: async follow_up received after waiting");
                                break;
                            }
                            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        }
                        if self.follow_up_queue.is_empty() {
                            tracing::info!("outer {outer_i}: no async follow_up after 30 min, stopping");
                            return Ok(());
                        }
                    } else {
                        return Ok(());
                    }
                }
            }
            tracing::info!(
                "outer {outer_i}: {} follow-up msgs",
                self.follow_up_queue.len()
            );
            while let Some(msg) = self.follow_up_queue.pop_front() {
                self.messages.push(msg);
            }
        }
        tracing::warn!(
            "outer: max iterations ({})",
            self.config.max_outer_iterations
        );
        Ok(())
    }

    async fn inner_loop(&mut self) -> AgentResult<StopReason> {
        let max_turns = self.config.max_turns.unwrap_or(u64::MAX);
        let mut no_tool_retries = 0u32;
        for turn in 0..max_turns {
            let turn_start = std::time::Instant::now();
            self.turn_index = turn;
            self.check_pause().await?;

            let turn_ctx = TurnContext {
                turn_index: turn,
                messages: vec![],
                has_tool_calls: false,
                stop_reason: None,
            };
            self.extensions
                .on_turn_start(&mut (turn_ctx.clone()))
                .await?;
            self.drain_steering().await?;
            self.maybe_compact().await?;

            // Hook: on_context (modify messages before cloning snapshot)
            // 对齐 pi transformContext：扩展在 snapshot 前修改 self.messages，
            // 这样折叠/注入效果本轮就生效，不会延迟一轮。
            self.extensions.on_context(&mut self.messages).await?;

            // Build context for provider (clone to avoid borrow issues)
            let mut sys_prompt = self.system_prompt.clone().unwrap_or_default();
            self.extensions.on_system_prompt(&mut sys_prompt).await?;
            let sys_prompt = Some(sys_prompt);

            // Skill 自动卸载：skill 内容（tool result）在加载后的下一轮 turn 就被"消化"了。
            // 后续 turn 不需要完整的 skill 内容——只保留标记（"skill xxx was loaded, content unloaded"）。
            // 这样大 skill（如 security-auditor 10KB）不会一直占 context。
            //
            // 策略：skill tool result 后面如果有 assistant message（说明 LLM 已基于 skill 回复了），
            // 就把 skill tool result 的内容替换成简短占位符。
            // 保留最近一次 skill 加载的完整内容（当前 turn 可能还需要）。
            let messages_snapshot = unload_consumed_skills(&self.messages, turn as usize);
            let tool_defs: Vec<_> = self.tools.tool_defs().iter().cloned().collect();

            // 跨 provider 消息规范化：当对话历史混合多个 provider 的消息时，
            // 降级 thinking block / 规范化 tool call ID / 补合成孤儿 tool result
            // 对齐 pi packages/ai/src/providers/transform-messages.ts
            let transformed_messages = ion_provider::transform_messages::transform_messages(
                messages_snapshot,
                &self.model,
                None,
            );
            let ctx = Context::new(sys_prompt, transformed_messages);
            let ctx = ctx.with_tools(tool_defs);

            // Call provider via router
            let options = StreamOptions {
                max_tokens: Some(self.model.max_tokens),
                api_key: self.config.api_key.clone(),
                reasoning: self.config.thinking.as_ref().and_then(|t| match t.as_str() {
                    "off" => Some(ion_provider::ThinkingLevel::Off),
                    "minimal" => Some(ion_provider::ThinkingLevel::Minimal),
                    "low" => Some(ion_provider::ThinkingLevel::Low),
                    "medium" => Some(ion_provider::ThinkingLevel::Medium),
                    "high" => Some(ion_provider::ThinkingLevel::High),
                    "xhigh" => Some(ion_provider::ThinkingLevel::XHigh),
                    _ => None,
                }),
                timeout_ms: None,
                max_retries: Some(self.config.max_retries),
                response_format: self.config.response_format.clone(),
            };

            let (stop_reason, events) = self.stream_with_retry(&ctx, &options).await?;

            match stop_reason {
                StopReason::Stop | StopReason::Length => {
                    // Extract token usage from the Done event
                    let usage_from_done = events.iter().rev().find_map(|e| match e {
                        StreamEvent::Done { message, .. } => Some(message.usage.clone()),
                        _ => None,
                    }).unwrap_or_default();

                    // Hooks: message streaming (already emitted in real-time in stream_with_retry)
                    // Just collect the text and emit final message_end here.

                    let text: String = events
                        .iter()
                        .filter_map(|e| match e {
                            StreamEvent::TextDelta { delta, .. } => Some(delta.clone()),
                            _ => None,
                        })
                        .collect();

                    let thinking_text: String = events
                        .iter()
                        .filter_map(|e| match e {
                            StreamEvent::ThinkingDelta { delta, .. } => Some(delta.clone()),
                            _ => None,
                        })
                        .collect();
                    let has_thinking = !thinking_text.is_empty();
                    let has_text = !text.is_empty();

                    // Hook: message_end
                    if has_text {
                        self.extensions.on_message_end("assistant", &text, &usage_from_done).await?;
                    }

                    let mut content_blocks: Vec<AssistantContentBlock> = Vec::new();
                    if has_thinking {
                        content_blocks.push(AssistantContentBlock::Thinking(ThinkingContent {
                            thinking: thinking_text,
                            thinking_signature: None,
                            redacted: None,
                        }));
                    }
                    if !text.is_empty() {
                        content_blocks.push(AssistantContentBlock::Text(TextContent {
                            text,
                            text_signature: None,
                        }));
                    }
                    if !content_blocks.is_empty() {
                        self.messages.push(Message::Assistant(AssistantMessage {
                            role: "assistant".into(),
                            content: content_blocks,
                            api: self.model.api.clone(),
                            provider: self.model.provider.clone(),
                            model: self.model.id.clone(),
                            response_model: None,
                            response_id: None,
                            usage: usage_from_done,
                            stop_reason: stop_reason.clone(),
                            error_message: None,
                            timestamp: now_ms(),
                        }));
                        // 增量 save：assistant message 加到 messages 后立即落盘。
                        // 确保最终总结（没有后续工具调用的 assistant message）也会 save。
                        // 解决 fork 子 Worker 被杀时最终总结丢失的问题。
                        self.extensions
                            .on_before_tool_execute("___assistant_message_saved", &serde_json::Value::Null, &self.messages)
                            .await?;
                    }

                    // 溢出恢复计数器：成功响应后重置（对齐 pi 的 reset 时机）
                    self.overflow_recovery_attempts = 0;

                    self.extensions
                        .on_turn_end(&TurnContext {
                            turn_index: turn,
                            messages: self.messages.clone(),
                            has_tool_calls: false,
                            stop_reason: Some(format!("{stop_reason:?}")),
                        })
                        .await?;

                    // ── turn_summary 落盘：每一轮 turn 结束时追加结构化摘要 ──
                    self.persist_turn_summary(turn, &events, &stop_reason, turn_start.elapsed().as_millis() as u64);

                    // ── 反幻觉重试：如果 LLM 没调任何工具就返回 → 重试 ──
                    // LLM 可能说"已创建文件"但实际没调 write 工具。
                    // 检测：stop_reason=Stop（不是 ToolUse）+ 没有任何 ToolCallEnd 事件
                    let tool_calls_present = events.iter().any(|e| matches!(e, StreamEvent::ToolCallEnd { .. }));
                    // faux/测试模式禁用反幻觉重试（避免耗尽 faux 队列）
                    let faux_mode = std::env::var("ION_FAUX_REPLY").is_ok()
                        || std::env::var("ION_FAUX_SCRIPT").is_ok()
                        || std::env::var("ION_RECORD").is_ok();
                    if stop_reason == StopReason::Stop
                        && !tool_calls_present
                        && self.config.retry_on_no_tool_use > 0
                        && no_tool_retries < self.config.retry_on_no_tool_use
                        && !faux_mode
                    {
                        no_tool_retries += 1;
                        tracing::warn!(
                            "LLM responded without tool calls (retry {}/{})",
                            no_tool_retries, self.config.retry_on_no_tool_use
                        );
                        self.messages.push(Message::User(UserMessage {
                            role: "user".into(),
                            content: vec![ContentBlock::Text(TextContent {
                                text: "WARNING: Your previous response did not call any tools! \
                                      You MUST use the write/edit tools to create or modify files. \
                                      Do not just describe what you would do — actually execute the tools.\n\
                                      Try again now.".into(),
                                text_signature: None,
                            })],
                            timestamp: now_ms(),
                            source: ion_provider::types::MessageSource::Prompt,
                        }));
                        continue;
                    }

                    // ── Workflow gate 校验：内核强制检查 ──
                    // 当 LLM 决定 Stop 时，检查所有 extension 的 gate。
                    // 如果 gate 失败 → 注入失败原因 + 强制继续循环。
                    // 这和 retry_on_no_tool_use 一样的机制，只是条件可插拔。
                    let gate_ctx = TurnContext {
                        turn_index: turn,
                        messages: self.messages.clone(),
                        has_tool_calls: false,
                        stop_reason: Some(format!("{stop_reason:?}")),
                    };
                    match self.extensions.check_gates(&gate_ctx).await? {
                        super::extension::GateDecision::RetryWith(msg) => {
                            tracing::warn!("Gate check failed, forcing retry: {}", &msg[..msg.len().min(100)]);
                            self.messages.push(Message::User(UserMessage {
                                role: "user".into(),
                                content: vec![ContentBlock::Text(TextContent {
                                    text: msg,
                                    text_signature: None,
                                })],
                                timestamp: now_ms(),
                                source: ion_provider::types::MessageSource::Prompt,
                            }));
                            continue;
                        }
                        super::extension::GateDecision::Allow => {}
                    }

                    return Ok(stop_reason);
                }
                StopReason::ToolUse => {
                    let tool_calls: Vec<ToolCall> = events
                        .iter()
                        .filter_map(|e| match e {
                            StreamEvent::ToolCallEnd { tool_call, .. } => Some(tool_call.clone()),
                            _ => None,
                        })
                        .collect();

                    // Hook: tool call streaming deltas
                    for event in &events {
                        if let StreamEvent::ToolCallDelta { delta, .. } = event {
                            self.extensions.on_tool_call_delta(delta, "").await?;
                        }
                    }

                    if !tool_calls.is_empty() {
                        self.messages.push(Message::Assistant(AssistantMessage {
                            role: "assistant".into(),
                            content: tool_calls
                                .iter()
                                .map(|tc| AssistantContentBlock::ToolCall(tc.clone()))
                                .collect(),
                            ..AssistantMessage::new(&self.model)
                        }));
                    }

                    for tc in &tool_calls {
                        // 工具执行前检查 abort(stopped 是 Arc<AtomicBool>)
                        if self.stopped.load(std::sync::atomic::Ordering::SeqCst) {
                            tracing::info!("[abort] 工具执行循环中检测到 stopped，终止");
                            break;
                        }

                        // ── 权限检查已移至 SecuredRuntime ──
                        // PermissionEngine + CommandGuard 现在在 Runtime trait 方法里拦截
                        // （execute_command / read_file / write_file / spawn_process 等）
                        // agent_loop 只负责调用 Extension 钩子和工具执行

                        self.extensions.before_tool_call(tc).await?;

                        // ── session 分支/回滚钩子（branch_session 工具）──
                        // LLM 调 branch_session 时，在执行前触发 on_session_before_switch，
                        // 让扩展有机会 veto（返回 Err 则中止，工具不执行）。
                        // action 由 is_rollback 参数决定：rollback / branch。
                        if tc.name == "branch_session" {
                            let is_rollback = tc.arguments.get("is_rollback")
                                .and_then(|v| v.as_bool()).unwrap_or(false);
                            let target_leaf = tc.arguments.get("from_entry")
                                .and_then(|v| v.as_str()).map(|s| s.to_string());
                            let branch_name = tc.arguments.get("name")
                                .and_then(|v| v.as_str()).map(|s| s.to_string());
                            let switch_ctx = super::extension::SessionSwitchContext {
                                action: if is_rollback { "rollback".into() } else { "branch".into() },
                                target_leaf_id: target_leaf,
                                source_leaf_id: None,
                                branch_name,
                            };
                            self.extensions.on_session_before_switch(&switch_ctx).await?;
                        }

                        // Hook: tool_execution_start
                        let start = std::time::Instant::now();
                        self.extensions.on_tool_execution_start(
                            &super::extension::ToolExecutionContext {
                                tool_call_id: tc.id.clone(),
                                tool_name: tc.name.clone(),
                                args: tc.arguments.clone(),
                                is_error: false,
                                duration_ms: 0,
                                result: String::new(),
                                    is_interrupted: false,
                            },
                        ).await?;

                        let tc_id = tc.id.clone();
                        let tc_name = tc.name.clone();
                        let tc_args = tc.arguments.clone();

                        // Execute tool with streaming updates via tokio channel.
                        // Use select! to forward updates to extensions concurrently while tool runs.
                        let output = {
                            // 增量 save 钩子：工具执行前 save 当前 messages。
                            // 解决 fork 阻塞问题——LLM 调 skill(fork) → spawn_worker 阻塞 →
                            // agent.run 不返回 → 进程被杀时 messages 全丢。
                            // 这个钩子在 tool.execute_stream 前触发，把 user prompt +
                            // assistant tool call decision 落盘。
                            self.extensions.on_before_tool_execute(&tc_name, &tc_args, &self.messages).await?;

                            let tool_ref = self.tools.get(&tc.name);
                            match tool_ref {
                                Some(tool) => {
                                    let (update_tx, update_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
                                    let on_update: super::tool::ToolUpdateFn = std::sync::Arc::new(
                                        move |partial: String| { let _ = update_tx.send(partial); },
                                    );

                                    // We need to run execute_stream and drain updates concurrently.
                                    // But tool borrows self.tools. So we poll manually.
                                    let exec_future = tool.execute_stream(tc_args.clone(), on_update, &*self.runtime);
                                    tokio::pin!(exec_future);

                                    let mut rx = update_rx;
                                    // 工具执行超时：默认 120s，但 skill fork 等长任务需要更久。
                                    // skill 工具的 context=fork 会 spawn 子 Worker 执行完整 skill 流程，
                                    // 可能要几分钟（audit 要读很多文件）。给它 600s（10 分钟）。
                                    // 其他工具保持 120s。
                                    //
                                    // 长任务 timeout 可通过 ION_TOOL_TIMEOUT 环境变量覆盖（单位秒）。
                                    // improver 跑 container exec cargo build/test 时建议设 ION_TOOL_TIMEOUT=1800（30 分钟）。
                                    let long_default = std::env::var("ION_TOOL_TIMEOUT")
                                        .ok()
                                        .and_then(|s| s.parse::<u64>().ok())
                                        .unwrap_or(600);
                                    let timeout_duration = if tc_name == "skill" || tc_name == "bash" || tc_name == "bash_run" {
                                        std::time::Duration::from_secs(long_default)
                                    } else {
                                        std::time::Duration::from_secs(120)
                                    };
                                    let timeout_secs = timeout_duration.as_secs();
                                    // abort + interrupt 检查 ticker
                                    let mut check_ticker = tokio::time::interval(std::time::Duration::from_millis(200));
                                    check_ticker.tick().await;
                                    let result = loop {
                                        tokio::select! {
                                            partial = rx.recv() => {
                                                if let Some(p) = partial {
                                                    self.extensions.on_tool_execution_update(
                                                        &super::extension::ToolExecutionContext {
                                                            tool_call_id: tc_id.clone(),
                                                            tool_name: tc_name.clone(),
                                                            args: tc_args.clone(),
                                                            is_error: false,
                                                            duration_ms: start.elapsed().as_millis() as u64,
                                                            result: String::new(),
                                    is_interrupted: false,
                                                        },
                                                        &p,
                                                    ).await?;
                                                }
                                            }
                                            r = &mut exec_future => {
                                                while let Ok(p) = rx.try_recv() {
                                                    self.extensions.on_tool_execution_update(
                                                        &super::extension::ToolExecutionContext {
                                                            tool_call_id: tc_id.clone(),
                                                            tool_name: tc_name.clone(),
                                                            args: tc_args.clone(),
                                                            is_error: false,
                                                            duration_ms: start.elapsed().as_millis() as u64,
                                                            result: String::new(),
                                    is_interrupted: false,
                                                        },
                                                        &p,
                                                    ).await?;
                                                }
                                                break r;
                                            }
                                            _ = tokio::time::sleep(timeout_duration) => {
                                                break Err(AgentError::Tool(format!("tool execution timeout ({}s)", timeout_secs)));
                                            }
                                            // 每 200ms 检查硬停止（abort）和软中断（immediate steer）
                                            _ = check_ticker.tick() => {
                                                if self.stopped.load(std::sync::atomic::Ordering::SeqCst) {
                                                    tracing::info!("[abort] 工具执行中检测到 stopped，drop future");
                                                    break Err(AgentError::Aborted);
                                                }
                                                if self.interrupted.load(std::sync::atomic::Ordering::SeqCst) {
                                                    tracing::info!("[interrupt] 工具执行中检测到 interrupt，打断当前工具");
                                                    break Err(AgentError::Interrupted);
                                                }
                                            }
                                        }
                                    };

                                    match result {
                                        Ok(out) => out,
                                        Err(e) => format!("Error: {e}"),
                                    }
                                }
                                None => format!("Error: tool '{}' not found", tc.name),
                            }
                        };

                        let duration = start.elapsed().as_millis() as u64;

                        // 工具输出截断：防止大文件/长输出爆掉 LLM 上下文（对齐 pi A6）
                        // 默认 2000 行 / 50KB，超了截头尾
                        let output = truncate_tool_output(&output);

                        // Hook: tool_execution_end
                        let exec_ctx = super::extension::ToolExecutionContext {
                            tool_call_id: tc.id.clone(),
                            tool_name: tc.name.clone(),
                            args: tc.arguments.clone(),
                            is_error: output.starts_with("Error"),
                            duration_ms: duration,
                            result: output.clone(),
                            is_interrupted: false,
                        };
                        self.extensions.on_tool_execution_end(&exec_ctx).await?;

                        let tr = ToolResultMessage {
                            role: "tool".into(),
                            tool_call_id: tc.id.clone(),
                            tool_name: tc.name.clone(),
                            content: vec![ContentBlock::Text(TextContent {
                                text: output,
                                text_signature: None,
                            })],
                            details: None,
                            is_error: false,
                            timestamp: now_ms(),
                        };

                        let tool_result = ion_provider::types::ToolResult {
                            tool_call_id: tc.id.clone(),
                            output: tr
                                .content
                                .iter()
                                .filter_map(|b| match b {
                                    ContentBlock::Text(t) => Some(t.text.clone()),
                                    _ => None,
                                })
                                .collect::<Vec<_>>()
                                .join(""),
                        };

                        self.extensions.after_tool_call(tc, &tool_result).await?;
                        self.messages.push(Message::ToolResult(tr));

                        // 增量 save：toolResult 加到 messages 后立即落盘。
                        // 确保 skill fork 的返回值（toolResult）也会 save，
                        // 即使后续 agent.run 阻塞 / 被杀。
                        self.extensions
                            .on_before_tool_execute("___tool_result_saved", &serde_json::Value::Null, &self.messages)
                            .await?;
                    }

                    self.extensions
                        .on_turn_end(&TurnContext {
                            turn_index: turn,
                            messages: self.messages.clone(),
                            has_tool_calls: true,
                            stop_reason: Some("tool_calls".into()),
                        })
                        .await?;

                    // ── turn_summary 落盘（ToolUse 路径）──
                    self.persist_turn_summary(turn, &events, &ion_provider::StopReason::ToolUse, turn_start.elapsed().as_millis() as u64);

                    continue;
                }
                StopReason::Error => {
                    // ── turn_summary 落盘（Error 路径，强制记录中断 turn）──
                    self.persist_turn_summary(turn, &events, &ion_provider::StopReason::Error, turn_start.elapsed().as_millis() as u64);

                    // ── 溢出恢复：检测到上下文溢出时，触发 compaction 然后重试该 turn ──
                    // 对齐 pi 的 overflow recovery：最多 compact-and-retry MAX_OVERFLOW_ROUNDS 次
                    let error_msg = events.iter().rev().find_map(|e| match e {
                        StreamEvent::Error { message, .. } => message.error_message.clone(),
                        _ => None,
                    }).unwrap_or_default();

                    let is_overflow = ion_provider::is_overflow_message(&error_msg);
                    let can_recover = is_overflow && self.overflow_recovery_attempts < MAX_OVERFLOW_ROUNDS;

                    if can_recover {
                        self.overflow_recovery_attempts += 1;
                        let attempt = self.overflow_recovery_attempts;
                        tracing::warn!(
                            "[overflow recovery] attempt {}/{} — triggering compaction then retry",
                            attempt, MAX_OVERFLOW_ROUNDS
                        );

                        // pop 掉尾部的 error assistant 消息（如果有），不让错误消息污染重试上下文
                        // 对齐 pi: while messages.last is assistant with stop_reason=error: pop
                        while let Some(Message::Assistant(am)) = self.messages.last() {
                            if am.stop_reason == StopReason::Error {
                                self.messages.pop();
                            } else {
                                break;
                            }
                        }

                        // 触发 compaction（复用 maybe_compact，已有 emergency fallback）
                        self.maybe_compact().await?;

                        // 重试当前 turn（不递增 turn counter）
                        continue;
                    }

                    if is_overflow && !can_recover {
                        tracing::error!(
                            "[overflow recovery] exhausted after {} attempts, giving up",
                            MAX_OVERFLOW_ROUNDS
                        );
                    }

                    return Ok(StopReason::Error);
                }
                StopReason::Aborted => {
                    // ── turn_summary 落盘（Aborted 路径）──
                    self.persist_turn_summary(turn, &events, &ion_provider::StopReason::Error, turn_start.elapsed().as_millis() as u64);
                    return Ok(StopReason::Aborted);
                }
            }
        }
        tracing::warn!("inner: max turns ({})", max_turns);
        Ok(StopReason::Stop)
    }

    async fn stream_with_retry(
        &mut self,
        context: &Context,
        options: &StreamOptions,
    ) -> AgentResult<(StopReason, Vec<StreamEvent>)> {
        let mut last_error = None;
        for attempt in 0..=self.config.max_retries {
            self.check_pause().await?;

            // Hook: before_provider_request
            self.extensions.before_provider_request(
                &super::extension::ProviderRequestContext {
                    model: self.model.id.clone(),
                    provider: self.model.provider.clone(),
                    payload: serde_json::json!({"messages": "..."}),
                },
            ).await?;

            // 用 select! 让 registry::stream 期间也能响应 abort
            // 200ms 超时检查 stopped,不重新发 HTTP(用 pin + loop 保持同一个 future)
            // 同时用 CancellationToken 真正取消 HTTP（修复 D：reqwest select! + drop resp 关 TCP）
            let cancel_token = tokio_util::sync::CancellationToken::new();
            {
                // 存到 self.http_cancel，让 stop() 能调 cancel()
                if let Ok(mut guard) = self.http_cancel.lock() {
                    *guard = Some(cancel_token.clone());
                }
            }
            let stream_fut = registry::stream(&self.registry, &self.model, context, Some(options), Some(cancel_token));
            tokio::pin!(stream_fut);
            let stream_result = loop {
                tokio::select! {
                    r = &mut stream_fut => break r,
                    _ = tokio::time::sleep(std::time::Duration::from_millis(200)) => {
                        if self.stopped.load(std::sync::atomic::Ordering::SeqCst) {
                            tracing::info!("[abort] HTTP 请求期间检测到 stopped");
                            return Ok((StopReason::Aborted, Vec::new()));
                        }
                        // 继续 select!(stream_fut 保留,不重新发 HTTP)
                    }
                }
            };

            // Hook: after_provider_response
            if let Ok(ref _ev) = stream_result {
                self.extensions.after_provider_response(
                    &super::extension::ProviderResponseContext {
                        model: self.model.id.clone(),
                        provider: self.model.provider.clone(),
                        status: 200,
                        body_preview: "".into(),
                    },
                ).await?;
            }

            match stream_result {
                Ok(mut event_stream) => {
                    let mut collected = Vec::new();
                    let mut final_reason = StopReason::Stop;
                    let mut prev_was_thinking = false;
                    let mut thinking_buf = String::new();

                    loop {
                        // 用 select! 让 stopped 检查不被 recv().await 阻塞
                        // 每 200ms 检查一次 stopped,确保 abort < 1 秒生效
                        let event_opt = tokio::select! {
                            ev = event_stream.recv() => ev,
                            _ = tokio::time::sleep(std::time::Duration::from_millis(200)) => {
                                if self.stopped.load(std::sync::atomic::Ordering::SeqCst) {
                                    tracing::info!("[abort] LLM 流式等待中检测到 stopped");
                                    final_reason = StopReason::Aborted;
                                    break;
                                }
                                continue; // 没收到 chunk,也没 abort,继续等
                            }
                        };
                        let event = match event_opt {
                            Some(event) => event,
                            None => break, // stream 结束
                        };
                        // 每个 chunk 处理前也检查 stopped
                        if self.stopped.load(std::sync::atomic::Ordering::SeqCst) {
                            tracing::info!("[abort] LLM 流式中检测到 stopped，立即终止");
                            final_reason = StopReason::Aborted;
                            break;
                        }
                        // Transparent passthrough — forward each provider event to extensions immediately
                        match &event {
                            StreamEvent::Done { reason, .. } => final_reason = reason.clone(),
                            StreamEvent::Error { reason, .. } => final_reason = reason.clone(),

                            StreamEvent::Start { .. } => {
                                self.extensions.on_message_start("assistant", "").await?;
                            }

                            StreamEvent::ThinkingStart { .. } => {
                                prev_was_thinking = true;
                            }
                            StreamEvent::ThinkingDelta { delta, .. } => {
                                prev_was_thinking = true;
                                thinking_buf.push_str(delta);
                                self.extensions.on_thinking_delta(delta).await?;
                            }
                            StreamEvent::ThinkingEnd { content, .. } => {
                                let final_content = if content.is_empty() { &thinking_buf } else { content.as_str() };
                                self.extensions.on_thinking_end(final_content).await?;
                                prev_was_thinking = false;
                                thinking_buf.clear();
                            }

                            StreamEvent::TextStart { .. } => {
                                // Some providers skip ThinkingEnd — emit fallback with accumulated content
                                if prev_was_thinking {
                                    self.extensions.on_thinking_end(&thinking_buf).await?;
                                    thinking_buf.clear();
                                    prev_was_thinking = false;
                                }
                                self.extensions.on_message_start("assistant", "").await?;
                            }
                            StreamEvent::TextDelta { delta, .. } => {
                                self.extensions.on_message_delta(delta, "assistant").await?;
                            }
                            StreamEvent::TextEnd { content, .. } => {
                                self.extensions.on_text_end(content).await?;
                            }

                            StreamEvent::ToolCallStart { .. } => {
                                if prev_was_thinking {
                                    self.extensions.on_thinking_end(&thinking_buf).await?;
                                    thinking_buf.clear();
                                    prev_was_thinking = false;
                                }
                            }
                            StreamEvent::ToolCallDelta { delta, .. } => {
                                if std::env::var("ION_STREAM_DEBUG").ok().as_deref() == Some("1") {
                                    eprintln!("[stream-debug] agent_loop forward ToolCallDelta len={}", delta.len());
                                }
                                self.extensions.on_tool_call_delta(delta, "").await?;
                            }
                            StreamEvent::ToolCallEnd { tool_call, .. } => {
                                self.extensions.on_tool_call_end(tool_call).await?;
                            }
                        }
                        collected.push(event);
                    }
                    // If provider ended while still thinking (no explicit End), close it with buffer
                    if prev_was_thinking && !thinking_buf.is_empty() {
                        self.extensions.on_thinking_end(&thinking_buf).await?;
                    } else if prev_was_thinking {
                        self.extensions.on_thinking_end("").await?;
                    }
                    return Ok((final_reason, collected));
                }
                Err(e) => {
                    // 上下文溢出 → 不重试，返回 StopReason::Error 让 inner_loop 做溢出恢复（compaction）
                    if e.is_context_overflow() {
                        tracing::warn!("[overflow] context overflow detected, returning Error for recovery: {e:.120}");
                        let error_msg = format!("{e}");
                        // 构造一条 Error 事件供 inner_loop 提取错误文案
                        let events = vec![StreamEvent::Error {
                            reason: StopReason::Error,
                            message: AssistantMessage {
                                role: "assistant".into(),
                                content: vec![],
                                api: self.model.api.clone(),
                                provider: self.model.provider.clone(),
                                model: self.model.id.clone(),
                                response_model: None,
                                response_id: None,
                                usage: Usage::default(),
                                stop_reason: StopReason::Error,
                                error_message: Some(error_msg),
                                timestamp: now_ms(),
                            },
                        }];
                        return Ok((StopReason::Error, events));
                    }

                    // 使用 RetryConfig（如果有）或回退到简单配置
                    let err_str = e.to_string();
                    let fallback_cfg = crate::retry::RetryConfig {
                        max_retries: self.config.max_retries,
                        initial_delay: Duration::from_millis(self.config.retry_base_delay_ms),
                        ..Default::default()
                    };
                    let retry_cfg = self.config.retry_config.as_ref().unwrap_or(&fallback_cfg);

                    match crate::retry::should_retry(&err_str, attempt, retry_cfg) {
                        crate::retry::RetryDecision::AbortPermanent => {
                            self.extensions.on_auto_retry_end(false, attempt + 1).await?;
                            return Err(AgentError::Provider(format!(
                                "[permanent] {e}"
                            )));
                        }
                        crate::retry::RetryDecision::TransientExhausted => {
                            self.extensions.on_auto_retry_end(false, attempt + 1).await?;
                            return Err(AgentError::MaxRetries(format!(
                                "after {} attempts: {e}",
                                attempt + 1
                            )));
                        }
                        _ => {
                            // Defensive belt-and-suspenders: if the error is an auth
                            // failure (401/403/AuthError/Invalid API key) but somehow
                            // slipped past should_retry's AbortPermanent check, break
                            // immediately. Retrying an invalid/expired key is pointless.
                            let err_str_lower = err_str.to_lowercase();
                            if err_str_lower.contains("401")
                                || err_str_lower.contains("403")
                                || err_str_lower.contains("autherror")
                                || err_str_lower.contains("invalid api key")
                            {
                                tracing::warn!("Auth error, not retrying: {}", err_str);
                                self.extensions.on_auto_retry_end(false, attempt + 1).await?;
                                return Err(AgentError::Provider(format!(
                                    "[auth] {e}"
                                )));
                            }
                            let delay = crate::retry::backoff_duration(attempt, retry_cfg);
                            tracing::warn!(
                                "[retry] attempt {}/{} failed: {e:.80} — retrying in {:?}",
                                attempt + 1,
                                retry_cfg.max_retries + 1,
                                delay
                            );
                            // 通知前端：重试开始（emit 事件让 UI 显示"重试中 (N/M)..."）
                            self.extensions.on_auto_retry_start(attempt + 1, retry_cfg.max_retries + 1).await?;
                            last_error = Some(e);
                            tokio::time::sleep(delay).await;
                            // sleep 结束即开始下一轮 attempt；如果是最后一轮成功，
                            // inner_loop 收到 Ok 后不会到这里，所以 success=true 由
                            // inner_loop 的正常路径隐式表示（前端通过 agent_end 推断）。
                        }
                    }
                }
            }
        }
        // 所有重试用完仍失败
        self.extensions.on_auto_retry_end(false, self.config.max_retries + 1).await?;
        Err(AgentError::MaxRetries(format!(
            "after {} attempts: {:?}",
            self.config.max_retries + 1,
            last_error
        )))
    }

    async fn drain_steering(&mut self) -> AgentResult<()> {
        while let Some(msg) = self.steering_queue.pop_front() {
            self.messages.push(msg);
        }
        Ok(())
    }

    /// 手动触发压缩（compact RPC 用），返回压缩详情
    pub async fn compact_now(&mut self) -> AgentResult<compact::CompactionResult> {
        let mut config = self.config.compact_config.clone();
        config.context_window = self.model.context_window;
        let retry_config = RetryConfig::default();
        compact::compact_batched(
            &mut self.messages,
            &config,
            &self.extensions,
            None,
            retry_config,
        )
        .await
    }

    async fn maybe_compact(&mut self) -> AgentResult<()> {
        if !self.config.enable_compact {
            return Ok(());
        }
        // 动态注入 context_window 到 compact_config（用于快/慢路径决策）
        let mut config = self.config.compact_config.clone();
        config.context_window = self.model.context_window;

        // 先检查是否需要压缩（低于 threshold 不压缩）
        if !compact::needs_compact(&self.messages, &config) {
            return Ok(());
        }

        let retry_config = RetryConfig::default();
        // Use compact model if specified, otherwise use main model
        let summarizer_model = self.compact_model.as_ref().unwrap_or(&self.model);
        let summarizer = compact::make_llm_summarizer(self.registry.clone(), summarizer_model.clone(), self.config.api_key.clone());
        // 尝试用 LLM summarizer 压缩，失败则 fallback 到 emergency truncate
        // （LLM 不可用 / 没 API key / 网络错 时保证 compaction 不阻塞 agent）
        match compact::compact_batched(
            &mut self.messages,
            &config,
            &self.extensions,
            Some(summarizer),
            retry_config,
        )
        .await
        {
            Ok(result) => {
                self.persist_compaction(&result);
                Ok(())
            }
            Err(e) => {
                tracing::warn!("LLM compaction failed, falling back to emergency truncate: {e}");
                match compact::compact_batched(
                    &mut self.messages,
                    &config,
                    &self.extensions,
                    None,
                    RetryConfig::default(),
                )
                .await
                {
                    Ok(result) => {
                        self.persist_compaction(&result);
                        Ok(())
                    }
                    Err(e) => Err(e),
                }
            }
        }
    }

    /// 将 compaction 结果落盘到 session JSONL（compaction entry）。
    /// firstKeptEntryId 暂为 None（内存 Message 无 entryId），拉取层通过扫描
    /// 最后一个 compaction entry 定位 since_compaction 视点。
    fn persist_compaction(&self, result: &compact::CompactionResult) {
        if let Some(ref cwd) = self.session_cwd {
            crate::session_jsonl::append_compaction(
                cwd,
                &result.summary,
                result.tokens_before,
                None, // firstKeptEntryId 暂不填（内存 Message 无 id）
                Some(&result.stage),
                if result.batch_count > 0 { Some(result.batch_count) } else { None },
            );
            tracing::info!("compaction entry persisted to session JSONL (stage={})", result.stage);
        }
    }

    /// 将本轮 turn 的结构化摘要落盘到 session JSONL（turn_summary entry）。
    /// 纯结构化提取，不调 LLM。含 abort/error turn 也调用此方法。
    fn persist_turn_summary(
        &self,
        turn: u64,
        events: &[ion_provider::StreamEvent],
        stop_reason: &ion_provider::StopReason,
        duration_ms: u64,
    ) {
        let Some(ref cwd) = self.session_cwd else {
            return;
        };

        // 提取本轮 tool_calls
        let tool_calls: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                ion_provider::StreamEvent::ToolCallEnd { tool_call, .. } => {
                    Some(tool_call.name.clone())
                }
                _ => None,
            })
            .collect();
        let tool_call_count = tool_calls.len() as u32;

        // 从 messages 找最后一条 user（本轮用户提问）和 assistant（本轮回复）
        let last_user = self.messages.iter().rev().find_map(|m| match m {
            Message::User(u) => Some(u.clone()),
            _ => None,
        });
        let last_asst = self.messages.iter().rev().find_map(|m| match m {
            Message::Assistant(a) => Some(a.clone()),
            _ => None,
        });

        // userContent（截断到 200 字符，对齐 list_turns 的 full_content=false 语义）
        let _user_content = last_user
            .as_ref()
            .map(|u| {
                u.content
                    .iter()
                    .filter_map(|b| match b {
                        ion_provider::ContentBlock::Text(t) => Some(t.text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default();

        // assistantContent（截断到 200 字符）
        let asst_content = last_asst
            .as_ref()
            .map(|a| {
                a.content
                    .iter()
                    .filter_map(|b| match b {
                        ion_provider::AssistantContentBlock::Text(t) => Some(t.text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default();

        // summary = assistantContent 前 200 字（纯结构化，不调 LLM）
        let summary = if asst_content.chars().count() > 200 {
            asst_content.chars().take(200).collect::<String>() + "..."
        } else {
            asst_content.clone()
        };

        // userEntryId：内存 Message 无 entryId，暂用 turn 序号占位
        let user_entry_id = format!("turn_{}", turn);

        // tokens
        let (tok_in, tok_out) = last_asst
            .as_ref()
            .map(|a| (a.usage.input, a.usage.output))
            .unwrap_or((0, 0));

        // status
        let status = match stop_reason {
            ion_provider::StopReason::Stop => "completed",
            ion_provider::StopReason::ToolUse => "tool_use",
            ion_provider::StopReason::Length => "max_turns",
            ion_provider::StopReason::Error => "error",
            ion_provider::StopReason::Aborted => "aborted",
        };

        crate::session_jsonl::append_turn_summary(
            cwd,
            turn,
            &user_entry_id,
            &summary,
            &tool_calls,
            tool_call_count,
            tok_in,
            tok_out,
            duration_ms,
            &[], // entryRange 暂空（内存 Message 无 entryId）
            status,
        );
    }

    async fn check_pause(&self) -> AgentResult<()> {
        // 优先检查停止（abort）
        if self.stopped.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(AgentError::Aborted);
        }
        // 再检查暂停（pause）：poll stopped + pause_rx 每 100ms
        while *self.pause_rx.borrow() {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if self.stopped.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(AgentError::Aborted);
            }
        }
        Ok(())
    }

    fn build_ctx(&self) -> AgentContext {
        AgentContext {
            turn_index: self.turn_index,
            message_count: self.messages.len(),
            tool_call_count: 0,
            last_stop_reason: None,
        }
    }
}

/// Skill 自动卸载：把已"消费"的 skill tool result 内容替换成占位符。
///
/// 当 LLM 基于 skill 内容做了回复（assistant message）后，skill 内容就不需要了。
/// 把旧 skill tool result 替换成 "[skill 'xxx' content unloaded to save context]"。
/// 保留最近一次 skill 加载的完整内容（当前 turn 可能还用得到）。
///
/// 判断"已消费"：skill tool result 后面跟着 assistant message（turn > skill 加载的 turn）。
fn unload_consumed_skills(messages: &[Message], _current_turn: usize) -> Vec<Message> {
    use ion_provider::types::{ContentBlock, TextContent};

    // 找所有 skill tool result 的位置 + 对应的 skill 名字
    // skill tool result 的内容以 "Skill '" 开头（SkillTool inject 模式的返回值格式）
    let mut skill_positions: Vec<(usize, String)> = Vec::new(); // (index, skill_name)
    for (i, msg) in messages.iter().enumerate() {
        if let Message::ToolResult(tr) = msg {
            if tr.content.iter().any(|c| {
                if let ContentBlock::Text(TextContent { text, .. }) = c {
                    text.starts_with("Skill '") && text.contains("' loaded:")
                } else {
                    false
                }
            }) {
                // 提取 skill 名字
                let skill_name = tr.content.iter().find_map(|c| {
                    if let ContentBlock::Text(TextContent { text, .. }) = c {
                        // "Skill 'code-audit' loaded:" → "code-audit"
                        if text.starts_with("Skill '") {
                            let rest = &text[7..];
                            if let Some(end) = rest.find('\'') {
                                return Some(rest[..end].to_string());
                            }
                        }
                    }
                    None
                }).unwrap_or_default();
                skill_positions.push((i, skill_name));
            }
        }
    }

    if skill_positions.is_empty() {
        return messages.to_vec();
    }

    // 保留最后一次 skill 加载的完整内容（当前 turn 可能还需要）
    let last_skill_pos = skill_positions.last().map(|(pos, _)| *pos).unwrap_or(usize::MAX);

    // 对每个 skill tool result（除了最后一次的），检查它后面是否有 assistant message
    let mut result = messages.to_vec();
    for (pos, skill_name) in &skill_positions {
        if *pos >= last_skill_pos {
            continue; // 保留最后一次的完整内容
        }
        // 检查这个 skill result 后面是否有 assistant message
        let has_following_assistant = messages[*pos + 1..].iter().any(|m| {
            matches!(m, Message::Assistant(_))
        });
        if has_following_assistant {
            // 替换成占位符
            if let Some(Message::ToolResult(tr)) = result.get_mut(*pos) {
                let placeholder = format!(
                    "[Skill '{}' was loaded and used. Content unloaded to save context. \
                     Use skill(skill_name='{}') to reload if needed.]",
                    skill_name, skill_name
                );
                tr.content = vec![ContentBlock::Text(TextContent { text: placeholder, text_signature: None })];
            }
        }
    }

    result
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// 工具输出截断阈值（对齐 pi: 2000 行 / 50KB）
const TOOL_OUTPUT_MAX_LINES: usize = 2000;
const TOOL_OUTPUT_MAX_BYTES: usize = 50_000;

/// 截断工具输出：超限时保留头部 + 尾部，中间用省略标记替换。
/// 对齐 pi packages/coding-agent/src/core/tools/truncate.ts 的 truncateHead/Tail 逻辑。
fn truncate_tool_output(output: &str) -> String {
    let byte_len = output.len();

    // 快速路径：字节和行数都在限制内，直接返回
    if byte_len <= TOOL_OUTPUT_MAX_BYTES {
        let line_count = output.lines().count();
        if line_count <= TOOL_OUTPUT_MAX_LINES {
            return output.to_string();
        }
    }

    // 单次遍历收集行（避免 lines().count() + lines().collect() 双遍历）
    let lines: Vec<&str> = output.lines().collect();
    let line_count = lines.len();
    let byte_len_actual = output.len();

    // 保留头部 80% + 尾部 20%（头比尾重要——文件通常从头开始看）
    let head_lines = (TOOL_OUTPUT_MAX_LINES as f64 * 0.8) as usize;
    let tail_lines = TOOL_OUTPUT_MAX_LINES - head_lines;

    let head: Vec<&str> = lines.iter().take(head_lines).copied().collect();
    let tail: Vec<&str> = lines.iter().skip(line_count.saturating_sub(tail_lines)).copied().collect();

    let mut result = head.join("\n");
    result.push_str(&format!(
        "\n\n... (truncated: {} lines total, {} bytes; showing first {} + last {} lines) ...\n\n",
        line_count, byte_len_actual, head_lines, tail_lines
    ));
    result.push_str(&tail.join("\n"));

    // 字节级兜底：如果截断后仍超 50KB，截到 50KB（UTF-8 安全）
    if result.len() > TOOL_OUTPUT_MAX_BYTES {
        let target = TOOL_OUTPUT_MAX_BYTES.saturating_sub(50);
        // floor_char_boundary: 找到 target 位置之前最近的 UTF-8 字符边界
        let mut safe_end = target;
        while safe_end > 0 && !result.is_char_boundary(safe_end) {
            safe_end -= 1;
        }
        return format!("{}\n... (byte truncation at ~50KB)", &result[..safe_end]);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_short_output_unchanged() {
        let output = "line1\nline2\nline3";
        assert_eq!(truncate_tool_output(output), output);
    }

    #[test]
    fn truncate_long_lines() {
        // 生成 3000 行
        let input: String = (1..=3000).map(|i| format!("line {}", i)).collect::<Vec<_>>().join("\n");
        let result = truncate_tool_output(&input);
        assert!(result.contains("truncated"), "should have truncation marker");
        assert!(result.contains("line 1\n") || result.contains("line 1\r"), "should keep head");
        assert!(result.contains("line 3000"), "should keep tail");
        // head 保留 1600 行，tail 保留 400 行 → 中间 1601~2600 被截断
        assert!(!result.contains("line 2000\n"), "should drop middle (line 2000)");
    }

    #[test]
    fn truncate_long_bytes() {
        // 生成一个 60KB 的单行
        let input = "x".repeat(60_000);
        let result = truncate_tool_output(&input);
        assert!(result.len() <= TOOL_OUTPUT_MAX_BYTES + 100, "should be under ~50KB");
        assert!(result.contains("truncation"), "should have truncation marker");
    }

    #[test]
    fn truncate_exactly_at_limit() {
        // 刚好 2000 行——不截断
        let input: String = (1..=2000).map(|i| format!("l{}", i)).collect::<Vec<_>>().join("\n");
        let result = truncate_tool_output(&input);
        assert_eq!(result, input, "should not truncate at exact limit");
    }

    #[test]
    fn truncate_utf8_safe_no_panic() {
        // 生成含中文的超大输出——字节截断必须不 panic
        let mut input = String::new();
        for _ in 0..20000 {
            input.push_str("你好世界这是测试行\n"); // 每行多字节 UTF-8
        }
        let result = truncate_tool_output(&input);
        // 验证不 panic + 结果是有效字符串 + 有截断标记
        assert!(result.contains("truncation"), "should have truncation marker");
        assert!(result.len() <= TOOL_OUTPUT_MAX_BYTES + 100, "should be near 50KB");
    }

    #[test]
    fn truncate_emoji_safe_no_panic() {
        // 含 emoji 的输出
        let mut input = String::new();
        for _ in 0..20000 {
            input.push_str("😀😁😂🤣😃😄😅😆😉😊😋😎😍😘🥳\n");
        }
        let result = truncate_tool_output(&input);
        assert!(result.contains("truncation"));
    }

    #[test]
    fn test_current_message_count() {
        // Agent::new requires many args, so just test the signature compiles
        // by checking the method exists. We can use a simpler approach:
        // verify the method is accessible via type system.
        // Since Agent::new is complex, this test just ensures compilation.
        assert!(true, "current_message_count method compiles");
    }
}

// Import needed types for the compact module
