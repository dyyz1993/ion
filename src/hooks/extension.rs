//! HookExtension — 实现 Extension trait
//!
//! 在 12 个钩子点查 hooks.json 配置，匹配后执行 handler。
//! 核心方法 `process_event` 每次动态读 hooks.json（热重载）。

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

use super::handler_runner::{self, HookExecContext};
use super::matcher;
use super::stdin_builder;
use super::{HookOutcome, HooksConfig};

use crate::agent::error::{AgentError, AgentResult};
use crate::agent::extension::{
    BeforeAgentContext, Extension, InputContext, SessionContext, TurnContext,
};
use crate::agent::messages::Message;
use crate::agent::agent_loop::AgentContext;
use ion_provider::types::{ContentBlock, AssistantContentBlock, TextContent, ToolCall, ToolResult, UserMessage};

/// HookExtension — 读 hooks.json + 在 12 个事件点执行 handler
pub struct HookExtension {
    /// 项目目录（读 .ion/hooks.json + 变量替换用）
    project_dir: PathBuf,
    /// Runtime（command + agent handler 用）
    runtime: Option<Arc<dyn crate::runtime::Runtime>>,
    /// Stop 循环计数（per session，防死循环）
    /// key = session_id + event，value = loop_count
    loop_counts: std::sync::Mutex<std::collections::HashMap<String, u32>>,
    /// once 去重（per session）
    once_fired: std::sync::Mutex<std::collections::HashSet<String>>,
    /// follow_up_tx（block Stop/SubagentStop 时注入 reason 作为新 query）
    follow_up_tx: Option<tokio::sync::mpsc::UnboundedSender<Message>>,
}

impl HookExtension {
    pub fn new(
        project_dir: PathBuf,
        runtime: Option<Arc<dyn crate::runtime::Runtime>>,
        follow_up_tx: Option<tokio::sync::mpsc::UnboundedSender<Message>>,
    ) -> Self {
        Self {
            project_dir,
            runtime,
            loop_counts: std::sync::Mutex::new(std::collections::HashMap::new()),
            once_fired: std::sync::Mutex::new(std::collections::HashSet::new()),
            follow_up_tx,
        }
    }

    /// 检查 hooks 配置是否非空（决定是否注册）
    pub fn has_hooks(project_dir: &PathBuf) -> bool {
        let cfg = HooksConfig::load_fresh(Some(project_dir));
        !cfg.is_empty() && !cfg.disable_all_hooks
    }

    /// 核心方法：处理一个事件
    ///
    /// 每次动态读 hooks.json（热重载），匹配 matcher/if，执行 handler，合并结果。
    async fn process_event(&self, event: &str, stdin: serde_json::Value) -> HookOutcome {
        // ⭐ 每次都重新读文件（零状态，改完即生效）
        let config = HooksConfig::load_fresh(Some(&self.project_dir));
        if config.disable_all_hooks || config.is_empty() {
            return HookOutcome::default();
        }

        // ── 跨进程递归深度保护（防 agent handler 死循环）──
        // agent handler spawn 的子 Worker 继承此环境变量（Manager spawn 时自动传递）
        // depth > 0 说明当前是 hooks spawn 的子 Worker，跳过 agent handler 避免递归
        let hook_depth = std::env::var("ION_HOOK_DEPTH")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(0);

        // 从 stdin 提取 tool_name（matcher 过滤用）
        let tool_name = stdin.get("tool_name").and_then(|v| v.as_str());
        let tool_input = stdin.get("tool_input");

        // Stop/SubagentStop 的 loop_count 检查（防死循环）
        if event == "Stop" || event == "SubagentStop" {
            let session_id = stdin.get("session_id").and_then(|v| v.as_str()).unwrap_or("");
            let key = format!("{session_id}:{event}");
            let mut counts = self.loop_counts.lock().unwrap();
            let count = counts.entry(key.clone()).or_insert(0);
            *count += 1;
            // loop_limit 默认 5
            let limit = 5u32;
            if *count > limit {
                tracing::info!("[hooks] {event} loop_count={count} > {limit}, skipping (防死循环)");
                drop(counts);
                return HookOutcome::default();
            }
            drop(counts);
        }

        let handlers = config.handlers_for_event(event);
        if handlers.is_empty() {
            return HookOutcome::default();
        }

        let exec_ctx = HookExecContext {
            project_dir: self.project_dir.to_string_lossy().to_string(),
            event_name: event.to_string(),
            runtime: self.runtime.clone(),
        };

        let mut combined = HookOutcome::default();

        for (matcher_str, handler) in handlers {
            // ── agent handler 递归保护 ──
            // depth >= 2 说明当前 Worker 是 hooks spawn 的子 Worker
            // 入口 Worker depth=1，能 spawn（这是用户要的）
            // 子 Worker depth=2+，跳过 agent handler 阻断递归
            if handler.handler_type == super::HandlerType::Agent && hook_depth >= 2 {
                tracing::info!(
                    "[hooks] 跳过 agent handler（递归深度 {} >= 2，防死循环）",
                    hook_depth
                );
                continue;
            }

            // once 去重
            if handler.once {
                let session_id = stdin.get("session_id").and_then(|v| v.as_str()).unwrap_or("");
                let once_key = format!("{session_id}:{event}:{}", handler.command.as_deref()
                    .or(handler.prompt.as_deref())
                    .or(handler.url.as_deref())
                    .unwrap_or("?"));
                let mut fired = self.once_fired.lock().unwrap();
                if !fired.insert(once_key) {
                    continue; // 已经触发过
                }
            }

            // matcher 过滤（PreToolUse/PostToolUse 按 tool_name 过滤）
            if let Some(tn) = tool_name {
                if event == "PreToolUse" || event == "PostToolUse" || event == "PostToolUseFailure" {
                    if !matcher::matches_matcher(matcher_str, tn) {
                        continue;
                    }
                }
            }

            // if 条件过滤
            if !matcher::matches_if_clause(handler, tool_name, tool_input) {
                continue;
            }

            // 执行 handler
            let outcome = handler_runner::run_handler(handler, stdin.clone(), &exec_ctx).await;
            combined = combined.merge(outcome);

            if combined.is_terminal() {
                break; // block 后停止后续 handler
            }
        }

        combined
    }

    /// block Stop/SubagentStop 时，把 reason 作为新 query 注入
    fn inject_follow_up(&self, reason: &str) {
        if let Some(ref tx) = self.follow_up_tx {
            let msg = Message::User(UserMessage {
                role: "user".into(),
                content: vec![ContentBlock::Text(TextContent { text: reason.to_string(), text_signature: None })],
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0),
            });
            let _ = tx.send(msg);
        }
    }
}

#[async_trait]
impl Extension for HookExtension {
    fn name(&self) -> &str { "hooks" }

    // ── Session lifecycle ──

    async fn on_session_start(&self, ctx: &SessionContext) -> AgentResult<()> {
        let outcome = self.process_event("SessionStart", stdin_builder::session_start("SessionStart", &ctx.reason)).await;
        // SessionStart 的 additionalContext 通过 on_system_prompt 注入（下面）
        if outcome.block {
            tracing::info!("[hooks] SessionStart blocked: {:?}", outcome.block_reason);
        }
        // startup 时额外触发 Setup
        if ctx.reason == "startup" {
            let _ = self.process_event("Setup", stdin_builder::session_start("Setup", &ctx.reason)).await;
        }
        Ok(())
    }

    async fn on_session_shutdown(&self, _ctx: &SessionContext) -> AgentResult<()> {
        let _ = self.process_event("SessionEnd", stdin_builder::session_end()).await;
        Ok(())
    }

    async fn on_session_before_compact(&self, msgs: &mut Vec<Message>) -> AgentResult<()> {
        let _ = self.process_event("PreCompact", stdin_builder::pre_compact(msgs.len())).await;
        Ok(())
    }

    // ── Input ──

    async fn on_input(&self, ctx: &mut InputContext) -> AgentResult<()> {
        let stdin = stdin_builder::user_prompt_submit(&ctx.text);
        let outcome = self.process_event("UserPromptSubmit", stdin).await;
        if outcome.block {
            ctx.handled = true;
            ctx.text = format!("[blocked by hook] {}", outcome.block_reason.unwrap_or_default());
        } else if let Some(extra) = outcome.additional_context {
            ctx.text = format!("{}\n\n---\n{}", ctx.text, extra);
        }
        Ok(())
    }

    // ── Tool ──

    async fn before_tool_call(&self, call: &ToolCall) -> AgentResult<()> {
        let stdin = stdin_builder::pre_tool_use(&call.name, &call.arguments, &call.id);
        let outcome = self.process_event("PreToolUse", stdin).await;
        if outcome.block {
            return Err(AgentError::Tool(format!(
                "tool '{}' blocked by hook: {}",
                call.name,
                outcome.block_reason.unwrap_or_default()
            )));
        }
        // ask（exit 3）暂不处理（需要 UI 确认，后续接入 PermissionEngine）
        Ok(())
    }

    async fn after_tool_call(&self, call: &ToolCall, result: &ToolResult) -> AgentResult<()> {
        let response = serde_json::json!({
            "output": result.output,
        });
        // ToolResult 只有 output 字段，用是否包含 "error" 简单判断 is_error
        let is_error = result.output.to_lowercase().contains("error") || result.output.is_empty();
        let stdin = stdin_builder::post_tool_use(&call.name, &call.arguments, &response, is_error);
        let outcome = self.process_event(
            if is_error { "PostToolUseFailure" } else { "PostToolUse" },
            stdin,
        ).await;
        // PostToolUse 的 block 通知 LLM（工具已执行无法撤销）
        if outcome.block {
            tracing::info!("[hooks] PostToolUse block notification: {:?}", outcome.block_reason);
        }
        Ok(())
    }

    // ── Agent lifecycle ──

    async fn before_agent_start(&self, _ctx: &mut BeforeAgentContext) -> AgentResult<()> {
        // pi 把 before_agent_start 映射到 UserPromptSubmit，但 ION 已经在 on_input 做了
        // 这里不重复处理
        Ok(())
    }

    async fn on_agent_start(&self, _ctx: &AgentContext) -> AgentResult<()> {
        let _ = self.process_event("SubagentStart", stdin_builder::subagent_start()).await;
        Ok(())
    }

    async fn on_agent_end(&self, ctx: &AgentContext) -> AgentResult<()> {
        let last_msg = format!("agent ended (turns={}, tools={})", ctx.turn_index, ctx.tool_call_count);
        let stdin = stdin_builder::subagent_stop(&last_msg, 0);
        let outcome = self.process_event("SubagentStop", stdin).await;
        if outcome.block {
            self.inject_follow_up(&outcome.block_reason.unwrap_or_else(|| "continue working".into()));
        }
        Ok(())
    }

    // ── Turn lifecycle ──

    async fn on_turn_end(&self, ctx: &TurnContext) -> AgentResult<()> {
        // 从最后一条消息提取文本（简化版）
        let mut last_msg = String::new();
        if let Some(Message::Assistant(a)) = ctx.messages.last() {
            for c in &a.content {
                if let AssistantContentBlock::Text(t) = c {
                    last_msg.push_str(&t.text);
                }
            }
        }
        let stdin = stdin_builder::stop(&last_msg, 0);
        let outcome = self.process_event("Stop", stdin).await;
        if outcome.block {
            self.inject_follow_up(&outcome.block_reason.unwrap_or_else(|| "please continue".into()));
        }
        Ok(())
    }

    // ── System prompt（注入 additionalContext）──

    async fn on_system_prompt(&self, prompt: &mut String) -> AgentResult<()> {
        // SessionStart 的 additionalContext 注入到 system prompt
        let outcome = self.process_event("SessionStart", stdin_builder::session_start("SessionStart", "prompt_inject")).await;
        if let Some(extra) = outcome.additional_context {
            prompt.push_str(&format!("\n\n---\n{extra}"));
        }
        Ok(())
    }
}
