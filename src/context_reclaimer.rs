//! Context Reclaimer — priority-based context recycling.
//!
//! Runs on `on_context` hook (every turn, before LLM call).
//! Removes low-value content to keep token usage low, without LLM summarization.
//!
//! ## Reclaim priority (highest = reclaimed first)
//!
//! 1. **Thinking blocks** — reasoning output, useless after the turn. Biggest token sink.
//! 2. **Old bash output** — logs, compile output, test results. "Fire and forget".
//! 3. **Old grep/find/ls output** — search results, already consumed.
//! 4. **Old read output** — file contents. Reclaimed last (agent might re-reference).
//! 5. **Never reclaimed** — User messages, recent N messages, ToolCall blocks.
//!
//! ## Token budget
//!
//! Target: keep context under `max_tokens` (default: 60% of context_window).
//! When exceeded, reclaim by priority until under budget or nothing left to reclaim.

use crate::agent::error::AgentResult;
use crate::agent::extension::Extension;
use ion_provider::types::{AssistantContentBlock, ContentBlock, Message, TextContent};

/// Default: start reclaiming when context exceeds 60% of window.
const DEFAULT_USAGE_PERCENT: u64 = 60;

/// Number of recent messages to always keep (never reclaim).
const KEEP_RECENT: usize = 8;

/// Minimum tool result chars to bother reclaiming (don't trim tiny results).
const MIN_RECLAIM_CHARS: usize = 200;

/// Tools whose output is lowest value (reclaimed first).
const LOW_VALUE_TOOLS: &[&str] = &["bash", "bash_run"];

/// Tools whose output is medium value (reclaimed second).
const MEDIUM_VALUE_TOOLS: &[&str] = &["grep", "find", "ls", "glob"];

/// Tools whose output is higher value (reclaimed last).
const HIGH_VALUE_TOOLS: &[&str] = &["read"];

/// Context Reclaimer extension.
pub struct ContextReclaimer;

impl ContextReclaimer {
    pub fn new() -> Self {
        Self
    }

    /// Check if a tool name is in a priority tier.
    fn tool_tier(tool_name: &str) -> u8 {
        if LOW_VALUE_TOOLS.contains(&tool_name) {
            1 // lowest value, reclaim first
        } else if MEDIUM_VALUE_TOOLS.contains(&tool_name) {
            2
        } else if HIGH_VALUE_TOOLS.contains(&tool_name) {
            3
        } else {
            4 // unknown tools — don't reclaim
        }
    }

    /// Estimate token count for messages (rough: chars / 4).
    fn estimate_tokens(messages: &[Message]) -> usize {
        messages.iter().map(|m| Self::msg_chars(m) / 4).sum()
    }

    /// Total character count of a message's content.
    fn msg_chars(msg: &Message) -> usize {
        match msg {
            Message::User(m) => m
                .content
                .iter()
                .map(|b| match b {
                    ContentBlock::Text(t) => t.text.len(),
                    ContentBlock::Image(_) => 0,
                })
                .sum(),
            Message::Assistant(m) => m
                .content
                .iter()
                .map(|b| match b {
                    AssistantContentBlock::Text(t) => t.text.len(),
                    AssistantContentBlock::Thinking(th) => th.thinking.len(),
                    AssistantContentBlock::ToolCall(tc) => tc.arguments.to_string().len(),
                })
                .sum(),
            Message::ToolResult(m) => m
                .content
                .iter()
                .map(|b| match b {
                    ContentBlock::Text(t) => t.text.len(),
                    ContentBlock::Image(_) => 0,
                })
                .sum(),
            Message::BashExecution(m) => m.command.len() + m.output.len(),
            Message::Custom(m) => match &m.content {
                ion_provider::types::CustomContent::Text(s) => s.len(),
                ion_provider::types::CustomContent::Blocks(blocks) => {
                    blocks.iter().map(|b| match b {
                        ContentBlock::Text(t) => t.text.len(),
                        _ => 0,
                    }).sum()
                }
            },
            _ => 0,
        }
    }

    /// Strip thinking blocks from all assistant messages.
    /// Returns number of blocks removed.
    fn strip_thinking(messages: &mut Vec<Message>) -> usize {
        let mut removed = 0;
        for msg in messages.iter_mut() {
            if let Message::Assistant(a) = msg {
                let before = a.content.len();
                a.content.retain(|b| !matches!(b, AssistantContentBlock::Thinking(_)));
                removed += before - a.content.len();
            }
        }
        removed
    }

    /// Reclaim old tool results by priority tier.
    /// `tier` = which tier to reclaim (1=bash, 2=grep/find, 3=read).
    /// Skips the last `keep_recent` messages.
    /// Returns total chars reclaimed.
    fn reclaim_tier(
        messages: &mut Vec<Message>,
        tier: u8,
        keep_recent: usize,
    ) -> usize {
        let mut reclaimed = 0;
        let start = messages.len().saturating_sub(keep_recent);

        for i in 0..start {
            if let Message::ToolResult(tr) = &mut messages[i] {
                if Self::tool_tier(&tr.tool_name) == tier {
                    // Calculate current size
                    let chars: usize = tr
                        .content
                        .iter()
                        .map(|b| match b {
                            ContentBlock::Text(t) => t.text.len(),
                            _ => 0,
                        })
                        .sum();

                    if chars > MIN_RECLAIM_CHARS {
                        // Replace with a compact placeholder
                        let placeholder = format!(
                            "[reclaimed: {} output was {} chars]",
                            tr.tool_name, chars
                        );
                        reclaimed += chars;
                        reclaimed -= placeholder.len();
                        tr.content = vec![ContentBlock::Text(TextContent {
                            text: placeholder,
                            text_signature: None,
                        })];
                    }
                }
            }
        }
        reclaimed
    }

    /// Run the full reclaim pipeline.
    /// Returns summary of what was reclaimed.
    fn run_reclaim(
        messages: &mut Vec<Message>,
        context_window: u64,
    ) -> ReclaimSummary {
        let target_tokens = (context_window as u64 * DEFAULT_USAGE_PERCENT / 100) as usize;
        let mut summary = ReclaimSummary::default();

        // Phase 1: Always strip thinking blocks (free win, every turn)
        summary.thinking_blocks_removed = Self::strip_thinking(messages);

        // Check if we're under budget after thinking strip
        let current = Self::estimate_tokens(messages);
        if current <= target_tokens {
            summary.tokens_after = current;
            return summary;
        }

        // Phase 2: Reclaim old bash output (tier 1)
        summary.bash_chars_reclaimed = Self::reclaim_tier(messages, 1, KEEP_RECENT);

        let current = Self::estimate_tokens(messages);
        if current <= target_tokens {
            summary.tokens_after = current;
            return summary;
        }

        // Phase 3: Reclaim old grep/find/ls output (tier 2)
        summary.search_chars_reclaimed = Self::reclaim_tier(messages, 2, KEEP_RECENT);

        let current = Self::estimate_tokens(messages);
        if current <= target_tokens {
            summary.tokens_after = current;
            return summary;
        }

        // Phase 4: Reclaim old read output (tier 3)
        summary.read_chars_reclaimed = Self::reclaim_tier(messages, 3, KEEP_RECENT);

        summary.tokens_after = Self::estimate_tokens(messages);
        summary
    }
}

#[derive(Default, Debug)]
struct ReclaimSummary {
    thinking_blocks_removed: usize,
    bash_chars_reclaimed: usize,
    search_chars_reclaimed: usize,
    read_chars_reclaimed: usize,
    tokens_after: usize,
}

#[async_trait::async_trait]
impl Extension for ContextReclaimer {
    async fn on_context(&self, messages: &mut Vec<Message>) -> AgentResult<()> {
        // Only reclaim if we have enough messages to matter
        if messages.len() < KEEP_RECENT * 2 {
            return Ok(());
        }

        // Estimate context window from total tokens (rough)
        // Default to 128000 if we can't determine
        let context_window: u64 = 128_000;

        let before = Self::estimate_tokens(messages);
        let summary = Self::run_reclaim(messages, context_window);
        let after = summary.tokens_after;

        if before != after {
            tracing::info!(
                "[reclaimer] {} → {} tokens (saved {}). \
                 thinking_blocks={} bash_chars={} search_chars={} read_chars={}",
                before,
                after,
                before.saturating_sub(after),
                summary.thinking_blocks_removed,
                summary.bash_chars_reclaimed,
                summary.search_chars_reclaimed,
                summary.read_chars_reclaimed,
            );
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ion_provider::types::{
        AssistantMessage, ThinkingContent, ToolResultMessage,
    };
    use crate::agent::messages::{UserMessage, MessageSource};

    fn make_assistant_with_thinking(thinking: &str, text: &str) -> Message {
        Message::Assistant(AssistantMessage {
            role: "assistant".into(),
            content: vec![
                AssistantContentBlock::Thinking(ThinkingContent {
                    thinking: thinking.into(),
                    thinking_signature: None,
                    redacted: None,
                }),
                AssistantContentBlock::Text(TextContent {
                    text: text.into(),
                    text_signature: None,
                }),
            ],
            api: String::new(),
            provider: String::new(),
            model: String::new(),
            response_model: None,
            response_id: None,
            usage: Default::default(),
            stop_reason: ion_provider::types::StopReason::Stop,
            error_message: None,
            timestamp: 0,
        })
    }

    fn make_tool_result(tool_name: &str, output: &str) -> Message {
        Message::ToolResult(ToolResultMessage {
            role: "toolResult".into(),
            tool_call_id: format!("call_{}", tool_name),
            tool_name: tool_name.into(),
            content: vec![ContentBlock::Text(TextContent {
                text: output.into(),
                text_signature: None,
            })],
            details: None,
            is_error: false,
            timestamp: 0,
        })
    }

    fn make_user(text: &str) -> Message {
        Message::User(UserMessage {
            role: "user".into(),
            content: vec![ContentBlock::Text(TextContent { text: text.into(), text_signature: None })],
            timestamp: 0,
            source: MessageSource::Prompt,
        })
    }

    fn tool_result_text(msg: &Message) -> String {
        match msg {
            Message::ToolResult(tr) => tr.content.first().map(|b| match b {
                ContentBlock::Text(t) => t.text.clone(),
                _ => String::new(),
            }).unwrap_or_default(),
            _ => String::new(),
        }
    }

    #[test]
    fn test_strip_thinking_removes_blocks() {
        let mut msgs = vec![
            make_assistant_with_thinking("long reasoning...", "Hello!"),
            make_assistant_with_thinking("more thinking...", "World!"),
        ];
        let removed = ContextReclaimer::strip_thinking(&mut msgs);
        assert_eq!(removed, 2);
        for msg in &msgs {
            if let Message::Assistant(a) = msg {
                assert!(a.content.iter().all(|b| !matches!(b, AssistantContentBlock::Thinking(_))));
            }
        }
    }

    #[test]
    fn test_reclaim_bash_output() {
        let mut msgs = vec![
            make_user("do something"),
            make_assistant_with_thinking("thinking", "Let me run bash"),
            make_tool_result("bash", &"line\n".repeat(500)),
            make_assistant_with_thinking("thinking", "Done"),
            make_tool_result("bash", &"log\n".repeat(500)),
            make_user("thanks"),
            make_user("ok"),
        ];
        let reclaimed = ContextReclaimer::reclaim_tier(&mut msgs, 1, 4);
        assert!(reclaimed > 0, "should reclaim bash output");
        assert!(tool_result_text(&msgs[2]).contains("reclaimed"));
    }

    #[test]
    fn test_reclaim_preserves_recent() {
        let mut msgs: Vec<Message> = (0..20)
            .map(|i| make_tool_result("bash", &format!("output line {}\n", i).repeat(50)))
            .collect();
        let reclaimed = ContextReclaimer::reclaim_tier(&mut msgs, 1, 8);
        assert!(reclaimed > 0);
        for i in 12..20 {
            assert!(!tool_result_text(&msgs[i]).contains("reclaimed"),
                "message {} should be preserved", i);
        }
    }

    #[test]
    fn test_tool_tier_classification() {
        assert_eq!(ContextReclaimer::tool_tier("bash"), 1);
        assert_eq!(ContextReclaimer::tool_tier("bash_run"), 1);
        assert_eq!(ContextReclaimer::tool_tier("grep"), 2);
        assert_eq!(ContextReclaimer::tool_tier("find"), 2);
        assert_eq!(ContextReclaimer::tool_tier("read"), 3);
        assert_eq!(ContextReclaimer::tool_tier("edit"), 4);
    }

    #[test]
    fn test_skip_small_results() {
        let mut msgs = vec![make_tool_result("bash", "ok")];
        let reclaimed = ContextReclaimer::reclaim_tier(&mut msgs, 1, 0);
        assert_eq!(reclaimed, 0, "should not reclaim tiny results");
    }

    #[test]
    fn test_priority_order() {
        let mut msgs = vec![
            make_tool_result("bash", &"b".repeat(1000)),
            make_tool_result("grep", &"g".repeat(1000)),
            make_tool_result("read", &"r".repeat(1000)),
            make_user("recent1"),
            make_user("recent2"),
        ];
        let bash_reclaimed = ContextReclaimer::reclaim_tier(&mut msgs, 1, 2);
        assert!(bash_reclaimed > 0);
        assert!(!tool_result_text(&msgs[1]).contains("reclaimed"));
        assert!(!tool_result_text(&msgs[2]).contains("reclaimed"));
    }

    #[test]
    fn test_estimate_tokens() {
        let msgs = vec![make_user(&"x".repeat(400))];
        let tokens = ContextReclaimer::estimate_tokens(&msgs);
        assert_eq!(tokens, 100);
    }
}
