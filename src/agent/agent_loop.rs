use super::compact::{self, CompactConfig};
use super::error::{AgentError, AgentResult};
use super::extension::{ExtensionRegistry, TurnContext};
use super::tool::ToolRegistry;
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
    turn_index: u64,
    pause_tx: watch::Sender<bool>,
    pause_rx: watch::Receiver<bool>,
    running: bool,
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
        let mut messages = Vec::new();
        if let Some(sp) = system_prompt {
            // Add system prompt as a user message with special content
            // (In the new type system, system prompt goes in Context, not messages)
            messages.push(Message::User(UserMessage {
                role: "user".into(),
                content: vec![ContentBlock::Text(TextContent {
                    text: sp,
                    text_signature: None,
                })],
                timestamp: 0,
            }));
        }
        Self {
            messages,
            steering_queue: VecDeque::new(),
            follow_up_queue: VecDeque::new(),
            registry,
            model,
            tools,
            extensions: ExtensionRegistry::new(),
            config,
            turn_index: 0,
            pause_tx,
            pause_rx,
            running: false,
        }
    }

    pub fn with_extensions(mut self, ext: ExtensionRegistry) -> Self {
        self.extensions = ext;
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
    pub fn is_running(&self) -> bool {
        self.running
    }

    pub fn steer(&mut self, msg: Message) {
        self.steering_queue.push_back(msg);
    }
    pub fn follow_up(&mut self, msg: Message) {
        self.follow_up_queue.push_back(msg);
    }
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub async fn run(&mut self, prompt: impl Into<String>) -> AgentResult<()> {
        self.running = true;
        self.turn_index = 0;

        // Hooks: on_input
        {
            let mut input_ctx = super::extension::InputContext {
                text: prompt.into(),
                handled: false,
            };
            self.extensions.on_input(&mut input_ctx).await?;
            if input_ctx.handled {
                return Ok(());
            }
            // Add input message (use modified text if changed)
            self.messages.push(Message::User(UserMessage {
                role: "user".into(),
                content: vec![ContentBlock::Text(TextContent {
                    text: input_ctx.text,
                    text_signature: None,
                })],
                timestamp: now_ms(),
            }));
        }

        // Hook: before_agent_start
        {
            let mut before_ctx = super::extension::BeforeAgentContext {
                system_prompt: None,
                messages: self.messages.clone(),
            };
            self.extensions.before_agent_start(&mut before_ctx).await?;
        }

        // Hook: session_start
        self.extensions.on_session_start(&super::extension::SessionContext {
            reason: "startup".into(),
        }).await?;

        // Hook: model_select
        self.extensions.on_model_select(
            &super::extension::ModelSelectContext {
                old_model: None,
                old_provider: None,
                new_model: self.model.id.clone(),
                new_provider: self.model.provider.clone(),
            },
        ).await?;

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

            // Build context for provider
            let _ctx_messages: Vec<Message> = Vec::new();
            let ctx = Context::new(None, self.messages.clone());
            let ctx = ctx.with_tools(self.tools.tool_defs().iter().map(|td| td.clone()).collect());

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

                    // Hooks: message streaming
                    for event in &events {
                        match event {
                            StreamEvent::TextDelta { delta, .. } => {
                                self.extensions.on_message_delta(delta, "assistant").await?;
                            }
                            StreamEvent::ThinkingDelta { delta, .. } => {
                                self.extensions.on_thinking_delta(delta).await?;
                            }
                            _ => {}
                        }
                    }
                    // Hook: message_start (if there are text deltas)
                    let has_text = events.iter().any(|e| matches!(e, StreamEvent::TextDelta { .. }));
                    if has_text {
                        self.extensions.on_message_start("assistant", "").await?;
                    }

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
                    if has_thinking {
                        self.extensions.on_thinking_end(&thinking_text).await?;
                    }

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

                        let output = match self.tools.get(&tc.name) {
                            Some(tool) => match tool.execute(tc.arguments.clone()).await {
                                Ok(out) => {
                                    // Hook: tool_execution_update with partial result
                                    self.extensions.on_tool_execution_update(
                                        &super::extension::ToolExecutionContext {
                                            tool_call_id: tc.id.clone(),
                                            tool_name: tc.name.clone(),
                                            args: tc.arguments.clone(),
                                            is_error: false,
                                            duration_ms: start.elapsed().as_millis() as u64,
                                        },
                                        &out,
                                    ).await?;
                                    out
                                }
                                Err(e) => format!("Error: {e}"),
                            },
                            None => format!("Error: tool '{}' not found", tc.name),
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

                    while let Some(event) = event_stream.recv().await {
                        match &event {
                            StreamEvent::Done { reason, .. } => final_reason = reason.clone(),
                            StreamEvent::Error { reason, .. } => final_reason = reason.clone(),
                            _ => {}
                        }
                        collected.push(event);
                    }
                    return Ok((final_reason, collected));
                }
                Err(e) => {
                    tracing::warn!(
                        "{e} — retry {}/{} in {delay}ms",
                        attempt + 1,
                        self.config.max_retries,
                        delay = self.config.retry_base_delay_ms * (1 << attempt)
                    );
                    last_error = Some(e);
                    if attempt < self.config.max_retries {
                        tokio::time::sleep(Duration::from_millis(
                            self.config.retry_base_delay_ms * (1 << attempt),
                        ))
                        .await;
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
        if *self.pause_rx.borrow() {
            let mut rx = self.pause_rx.clone();
            loop {
                rx.changed().await.ok();
                if !*rx.borrow() {
                    break;
                }
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
