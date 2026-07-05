use super::error::AgentResult;
use crate::agent::extension::ExtensionRegistry;
use ion_provider::types::*;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Configuration for context compaction.
#[derive(Clone, Debug)]
pub struct CompactConfig {
    pub threshold: usize,
    pub target: usize,
    pub keep_newest: usize,
}

impl Default for CompactConfig {
    fn default() -> Self {
        Self {
            threshold: 32000,
            target: 16000,
            keep_newest: 4,
        }
    }
}

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub type SummarizerFn = Arc<dyn Fn(&[Message]) -> BoxFuture<'_, AgentResult<String>> + Send + Sync>;

/// Approximate token count for any message.
fn msg_tokens(msg: &Message) -> usize {
    match msg {
        Message::User(m) => m
            .content
            .iter()
            .map(|b| match b {
                ContentBlock::Text(t) => t.text.len() / 4,
                ContentBlock::Image(_) => 1000,
            })
            .sum(),
        Message::Assistant(m) => m
            .content
            .iter()
            .map(|b| match b {
                AssistantContentBlock::Text(t) => t.text.len() / 4,
                AssistantContentBlock::Thinking(th) => th.thinking.len() / 4,
                AssistantContentBlock::ToolCall(tc) => tc.arguments.to_string().len() / 4,
            })
            .sum(),
        Message::ToolResult(m) => m
            .content
            .iter()
            .map(|b| match b {
                ContentBlock::Text(t) => t.text.len() / 4,
                ContentBlock::Image(_) => 1000,
            })
            .sum(),
        Message::BashExecution(m) => (m.command.len() + m.output.len()) / 4,
        Message::Custom(m) => match &m.content {
            CustomContent::Text(s) => s.len() / 4,
            CustomContent::Blocks(blocks) => blocks
                .iter()
                .map(|b| match b {
                    ContentBlock::Text(t) => t.text.len() / 4,
                    ContentBlock::Image(_) => 1000,
                })
                .sum(),
        },
        Message::BranchSummary(m) => m.summary.len() / 4,
        Message::CompactionSummary(m) => m.summary.len() / 4,
    }
}

pub fn needs_compact(messages: &[Message], config: &CompactConfig) -> bool {
    let total: usize = messages.iter().map(msg_tokens).sum();
    total > config.threshold
}

pub async fn compact(
    messages: &mut Vec<Message>,
    config: &CompactConfig,
    extensions: &ExtensionRegistry,
    summarizer: Option<SummarizerFn>,
) -> AgentResult<()> {
    if !needs_compact(messages, config) {
        return Ok(());
    }
    let total: usize = messages.iter().map(msg_tokens).sum();
    tracing::info!("compacting: {total} tokens, {} msgs", messages.len());

    let mut new_msgs: Vec<Message> = Vec::new();

    // Keep system message — the first user message that looks like a system prompt
    if let Some(first) = messages.first() {
        // In the new type system, we treat first user message as system if it looks like one
        new_msgs.push(first.clone());
    }

    let system_count = new_msgs.len();
    let keep_count = config.keep_newest;
    let summarize_end = messages.len().saturating_sub(keep_count);
    let summarize_start = system_count;

    let to_summarize: Vec<Message> = if summarize_end > summarize_start {
        messages[summarize_start..summarize_end].to_vec()
    } else {
        Vec::new()
    };

    if !to_summarize.is_empty() {
        if let Some(ref summarizer) = summarizer {
            let summary = summarizer(&to_summarize).await?;
            new_msgs.push(Message::User(UserMessage {
                role: "user".into(),
                content: vec![ContentBlock::Text(TextContent {
                    text: format!("[Context summary]: {summary}"),
                    text_signature: None,
                })],
                timestamp: 0,
            }));
        } else {
            new_msgs.push(Message::User(UserMessage {
                role: "user".into(),
                content: vec![ContentBlock::Text(TextContent {
                    text: format!("[{} previous messages omitted]", to_summarize.len()),
                    text_signature: None,
                })],
                timestamp: 0,
            }));
        }
    }

    // Keep newest messages
    let start = messages.len().saturating_sub(keep_count);
    for msg in messages.iter().skip(start) {
        new_msgs.push(msg.clone());
    }

    *messages = new_msgs;
    extensions.on_session_compact(messages).await?;
    tracing::info!("compacted to {} msgs", messages.len());
    Ok(())
}

pub fn make_llm_summarizer(
    provider: Arc<ion_provider::registry::ApiRegistry>,
    model: Model,
) -> SummarizerFn {
    Arc::new(move |old_messages: &[Message]| {
        let p = Arc::clone(&provider);
        let m = model.clone();
        let msgs = old_messages.to_vec();
        Box::pin(async move {
            let ctx = ion_provider::Context::new(
                Some("Summarize key information from these conversation messages.".into()),
                msgs,
            );
            let msg = ion_provider::registry::complete(&p, &m, &ctx, None).await?;
            Ok(msg
                .content
                .iter()
                .filter_map(|b| match b {
                    AssistantContentBlock::Text(t) => Some(t.text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(""))
        })
    })
}

// ---- Tests ----
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_config_defaults() {
        let cfg = CompactConfig::default();
        assert_eq!(cfg.threshold, 32000);
    }

    #[test]
    fn needs_compact_below_threshold() {
        let msgs = vec![Message::User(UserMessage {
            role: "user".into(),
            content: vec![ContentBlock::Text(TextContent {
                text: "hello".into(),
                text_signature: None,
            })],
            timestamp: 0,
        })];
        let cfg = CompactConfig {
            threshold: 1000,
            ..Default::default()
        };
        assert!(!needs_compact(&msgs, &cfg));
    }

    #[tokio::test]
    async fn compact_without_summarizer() {
        let mut msgs = vec![
            Message::User(UserMessage {
                role: "user".into(),
                content: vec![ContentBlock::Text(TextContent {
                    text: "system: you are helpful".into(),
                    text_signature: None,
                })],
                timestamp: 0,
            }),
            Message::User(UserMessage {
                role: "user".into(),
                content: vec![ContentBlock::Text(TextContent {
                    text: "Hello!".into(),
                    text_signature: None,
                })],
                timestamp: 0,
            }),
            Message::Assistant(AssistantMessage {
                role: "assistant".into(),
                content: vec![AssistantContentBlock::Text(TextContent {
                    text: "Hi there!".into(),
                    text_signature: None,
                })],
                api: "".into(),
                provider: "".into(),
                model: "".into(),
                response_model: None,
                response_id: None,
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                error_message: None,
                timestamp: 0,
            }),
        ];
        let cfg = CompactConfig {
            threshold: 1,
            target: 100,
            keep_newest: 1,
        };
        let ext = ExtensionRegistry::new();
        compact(&mut msgs, &cfg, &ext, None).await.unwrap();
        assert!(msgs.len() <= 3);
    }

    #[test]
    fn test_msg_tokens_new_variants() {
        // BashExecution
        let bash = Message::BashExecution(BashExecutionMessage {
            role: "bashExecution".into(),
            command: "ls".into(),
            output: "a.txt".into(),
            exit_code: Some(0),
            cancelled: false,
            truncated: false,
            full_output_path: None,
            timestamp: 0,
            exclude_from_context: None,
        });
        let tokens = msg_tokens(&bash);
        assert!(tokens > 0);
        // "ls" (2) + "a.txt" (5) = 7 / 4 = 1
        assert_eq!(tokens, (2 + 5) / 4);

        // Custom (Text)
        let custom = Message::Custom(CustomMessage {
            role: "custom".into(),
            custom_type: "note".into(),
            content: CustomContent::Text("hello world".into()),
            display: true,
            details: None,
            timestamp: 0,
        });
        let tokens = msg_tokens(&custom);
        assert_eq!(tokens, "hello world".len() / 4);

        // BranchSummary
        let branch = Message::BranchSummary(BranchSummaryMessage {
            role: "branchSummary".into(),
            summary: "summary text".into(),
            from_id: "sess_x".into(),
            timestamp: 0,
        });
        let tokens = msg_tokens(&branch);
        assert_eq!(tokens, "summary text".len() / 4);

        // CompactionSummary
        let comp = Message::CompactionSummary(CompactionSummaryMessage {
            role: "compactionSummary".into(),
            summary: "compacted".into(),
            tokens_before: 100,
            timestamp: 0,
        });
        let tokens = msg_tokens(&comp);
        assert_eq!(tokens, "compacted".len() / 4);
    }
}
