use super::compact::{self, CompactConfig};
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
    pub max_turns: u64,
    pub max_outer_iterations: u64,
    pub max_retries: u32,
    pub retry_base_delay_ms: u64,
    pub enable_compact: bool,
    pub compact_config: CompactConfig,
    pub api_key: Option<String>,
    pub response_format: Option<String>,
    pub thinking: Option<String>,
    /// 高级重试配置（可选，覆盖上面的简单配置）
    pub retry_config: Option<crate::retry::RetryConfig>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_turns: 20,
            max_outer_iterations: 5,
            max_retries: 3,
            retry_base_delay_ms: 1000,
            enable_compact: true,
            compact_config: CompactConfig::default(),
            api_key: None,
            response_format: None,
            thinking: None,
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
    stopped: std::sync::atomic::AtomicBool,
    /// 工具执行运行时（本地/沙箱/远程）
    pub runtime: Box<dyn crate::runtime::Runtime>,
}

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
            stopped: std::sync::atomic::AtomicBool::new(false),
            runtime: Box::new(crate::runtime::LocalRuntime::new()),
        }
    }

    /// 替换运行时（本地/沙箱/远程切换）
    pub fn with_runtime(mut self, rt: Box<dyn crate::runtime::Runtime>) -> Self {
        self.runtime = rt;
        self
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

    pub fn pause(&self) {
        let _ = self.pause_tx.send(true);
    }
    pub fn resume(&self) {
        let _ = self.pause_tx.send(false);
    }
    /// 硬停止当前 Agent 循环（对齐 pi abort）。
    /// 设 stopped=true + 唤醒 check_pause → 返回 AgentError::Aborted → 内循环 break。
    pub fn stop(&self) {
        self.stopped.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = self.pause_tx.send(true);
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

    /// steer 队列积压数（未消费的高优先级消息）
    pub fn steering_queue_len(&self) -> usize {
        self.steering_queue.len()
    }
    /// follow_up 队列积压数（未消费的后续消息）
    pub fn follow_up_queue_len(&self) -> usize {
        self.follow_up_queue.len()
    }

    /// 直接调用一个已注册的工具（不经过 LLM）。
    /// 用于：ion rpc 直接触发 spawn_worker / read / write 等工具，不跑 LLM。
    pub async fn call_tool(&self, name: &str, args: serde_json::Value) -> AgentResult<String> {
        let tool = self.tools.get(name)
            .ok_or_else(|| AgentError::Tool(format!("tool not found: {name}")))?;
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
        self.turn_index = 0;

        // ── 生命周期顺序 (对齐 pi) ──
        // 1. session_start (会话启动)
        self.extensions.on_session_start(&super::extension::SessionContext {
            reason: "startup".into(),
        }).await?;

        // 2. model_select (模型选择)
        self.extensions.on_model_select(
            &super::extension::ModelSelectContext {
                old_model: None,
                old_provider: None,
                new_model: self.model.id.clone(),
                new_provider: self.model.provider.clone(),
            },
        ).await?;

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
        for outer_i in 0..self.config.max_outer_iterations {
            let reason = self.inner_loop().await?;
            match reason {
                StopReason::Error | StopReason::Aborted => return Ok(()),
                _ => {}
            }
            if self.follow_up_queue.is_empty() {
                return Ok(());
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
        for turn in 0..self.config.max_turns {
            self.turn_index = turn;
            self.check_pause().await?;

            let turn_ctx = TurnContext {
                turn_index: turn as u64,
                messages: vec![],
                has_tool_calls: false,
                stop_reason: None,
            };
            self.extensions
                .on_turn_start(&mut (turn_ctx.clone()))
                .await?;
            self.drain_steering().await?;
            self.maybe_compact().await?;

            // Build context for provider (clone to avoid borrow issues)
            let mut sys_prompt = self.system_prompt.clone().unwrap_or_default();
            self.extensions.on_system_prompt(&mut sys_prompt).await?;
            let sys_prompt = Some(sys_prompt);
            let messages_snapshot = self.messages.clone();
            let tool_defs: Vec<_> = self.tools.tool_defs().iter().cloned().collect();
            let ctx = Context::new(sys_prompt, messages_snapshot);
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

            // Hook: on_system_prompt → on_context (对齐 pi: 先提示词后消息)
            if let Some(ref sp) = self.system_prompt {
                let mut sp_mut = sp.clone();
                self.extensions.on_system_prompt(&mut sp_mut).await?;
            }

            // Hook: on_context (modify messages before sending to LLM)
            self.extensions.on_context(&mut self.messages).await?;

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
                    }

                    self.extensions
                        .on_turn_end(&TurnContext {
                            turn_index: turn as u64,
                            messages: self.messages.clone(),
                            has_tool_calls: false,
                            stop_reason: Some(format!("{stop_reason:?}")),
                        })
                        .await?;

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
                        // ── 权限检查 ──
                        if let Some(ref engine) = self.extensions.permission_engine {
                            let path = tc.arguments.get("path")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            let command = tc.arguments.get("command")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            let target = if !path.is_empty() { path } else { command };
                            let action = crate::kernel::Action::from_tool(&tc.name);

                            match engine.check(target, action) {
                                crate::kernel::PermissionResult::Allow => {}
                                crate::kernel::PermissionResult::Deny(reason) => {
                                    tracing::warn!("[permission] denied: {reason}");
                                    let deny_result = ToolResultMessage {
                                        role: "tool".into(),
                                        tool_call_id: tc.id.clone(),
                                        tool_name: tc.name.clone(),
                                        content: vec![ContentBlock::Text(TextContent {
                                            text: format!("[Permission Denied] {reason}"),
                                            text_signature: None,
                                        })],
                                        details: None,
                                        is_error: true,
                                        timestamp: now_ms(),
                                    };
                                    self.messages.push(Message::ToolResult(deny_result));
                                    continue;
                                }
                                crate::kernel::PermissionResult::Ask { title, message } => {
                                    let allowed = self.extensions.ui_system.as_ref()
                                        .map(|ui| ui.confirm(&title, &message))
                                        .unwrap_or(true);
                                    if !allowed {
                                        let deny_result = ToolResultMessage {
                                            role: "tool".into(),
                                            tool_call_id: tc.id.clone(),
                                            tool_name: tc.name.clone(),
                                            content: vec![ContentBlock::Text(TextContent {
                                                text: "[Permission Denied] 用户拒绝了操作".into(),
                                                text_signature: None,
                                            })],
                                            details: None,
                                            is_error: true,
                                            timestamp: now_ms(),
                                        };
                                        self.messages.push(Message::ToolResult(deny_result));
                                        continue;
                                    }
                                }
                            }
                        }

                        // ── CommandGuard（bash 命令守卫）──
                        if tc.name == "bash" {
                            let command = tc.arguments.get("command")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            if !command.is_empty() {
                                let guard = crate::command_guard::CommandGuard::default();
                                match guard.check(command) {
                                    crate::command_guard::GuardDecision::Allow => {}
                                    crate::command_guard::GuardDecision::Deny(pattern) => {
                                        let msg = if let Some(ref sug) = pattern.suggestion {
                                            format!("[Command Guard] {} 建议: {}", pattern.message, sug)
                                        } else {
                                            format!("[Command Guard] {}", pattern.message)
                                        };
                                        let deny_result = ToolResultMessage {
                                            role: "tool".into(),
                                            tool_call_id: tc.id.clone(),
                                            tool_name: tc.name.clone(),
                                            content: vec![ContentBlock::Text(TextContent {
                                                text: msg,
                                                text_signature: None,
                                            })],
                                            details: None,
                                            is_error: true,
                                            timestamp: now_ms(),
                                        };
                                        self.messages.push(Message::ToolResult(deny_result));
                                        continue;
                                    }
                                    crate::command_guard::GuardDecision::Ask(pattern) => {
                                        let allowed = self.extensions.ui_system.as_ref()
                                            .map(|ui| ui.confirm(
                                                &format!("高危命令"),
                                                &format!("{}\n\n命令: `{}`", pattern.message, command),
                                            ))
                                            .unwrap_or(true);
                                        if !allowed {
                                            if let Some(ref sug) = pattern.suggestion {
                                                let deny_result = ToolResultMessage {
                                                    role: "tool".into(),
                                                    tool_call_id: tc.id.clone(),
                                                    tool_name: tc.name.clone(),
                                                    content: vec![ContentBlock::Text(TextContent {
                                                        text: format!("[Command Guard] 已跳过，建议: {}", sug),
                                                        text_signature: None,
                                                    })],
                                                    details: None,
                                                    is_error: true,
                                                    timestamp: now_ms(),
                                                };
                                                self.messages.push(Message::ToolResult(deny_result));
                                                continue;
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        self.extensions.before_tool_call(tc).await?;

                        // Hook: tool_execution_start
                        let start = std::time::Instant::now();
                        self.extensions.on_tool_execution_start(
                            &super::extension::ToolExecutionContext {
                                tool_call_id: tc.id.clone(),
                                tool_name: tc.name.clone(),
                                args: tc.arguments.clone(),
                                is_error: false,
                                duration_ms: 0,
                            },
                        ).await?;

                        let tc_id = tc.id.clone();
                        let tc_name = tc.name.clone();
                        let tc_args = tc.arguments.clone();

                        // Execute tool with streaming updates via tokio channel.
                        // Use select! to forward updates to extensions concurrently while tool runs.
                        let output = {
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
                                    let timeout_duration = std::time::Duration::from_secs(120);
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
                                                        },
                                                        &p,
                                                    ).await?;
                                                }
                                                break r;
                                            }
                                            _ = tokio::time::sleep(timeout_duration) => {
                                                break Err(AgentError::Tool("tool execution timeout (120s)".to_string()));
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

                        // Hook: tool_execution_end
                        let exec_ctx = super::extension::ToolExecutionContext {
                            tool_call_id: tc.id.clone(),
                            tool_name: tc.name.clone(),
                            args: tc.arguments.clone(),
                            is_error: output.starts_with("Error"),
                            duration_ms: duration,
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
                    }

                    self.extensions
                        .on_turn_end(&TurnContext {
                            turn_index: turn as u64,
                            messages: self.messages.clone(),
                            has_tool_calls: true,
                            stop_reason: Some("tool_calls".into()),
                        })
                        .await?;

                    continue;
                }
                StopReason::Error => {
                    return Ok(StopReason::Error);
                }
                StopReason::Aborted => return Ok(StopReason::Aborted),
            }
        }
        tracing::warn!("inner: max turns ({})", self.config.max_turns);
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

            let stream_result = registry::stream(&self.registry, &self.model, context, Some(options)).await;

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

                    while let Some(event) = event_stream.recv().await {
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
                            return Err(AgentError::Provider(format!(
                                "[permanent] {e}"
                            )));
                        }
                        crate::retry::RetryDecision::TransientExhausted => {
                            return Err(AgentError::MaxRetries(format!(
                                "after {} attempts: {e}",
                                attempt + 1
                            )));
                        }
                        _ => {
                            let delay = crate::retry::backoff_duration(attempt, retry_cfg);
                            tracing::warn!(
                                "[retry] attempt {}/{} failed: {e:.80} — retrying in {:?}",
                                attempt + 1,
                                retry_cfg.max_retries + 1,
                                delay
                            );
                            last_error = Some(e);
                            tokio::time::sleep(delay).await;
                        }
                    }
                }
            }
        }
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

    async fn maybe_compact(&mut self) -> AgentResult<()> {
        if !self.config.enable_compact {
            return Ok(());
        }
        compact::compact(
            &mut self.messages,
            &self.config.compact_config,
            &self.extensions,
            None,
        )
        .await
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

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

// Import needed types for the compact module
