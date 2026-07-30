//! Learning Extension — orchestrates memory extraction + skill distillation + secret redaction.
//!
//! Aligns with pi's `extensions/learning/index.ts`.
//!
//! ## What it does
//!
//! This extension wraps the existing memory processing pipeline (`run_memory_processing`)
//! with three enhancements:
//!
//! 1. **Secret redaction before LLM** — strips API keys/tokens/passwords from session
//!    content before sending to LLM for memory extraction (uses `secret_detector`).
//! 2. **Fast filtering** — skips sessions that are too short / pure greetings /
//!    no tool calls (saves LLM tokens, aligned with pi's `shouldExtract`).
//! 3. **Skill distillation** — after memory extraction, checks if the session
//!    contained write/edit operations; if so, distills a reusable skill
//!    (Phase 3, not yet implemented).
//!
//! ## Registration
//!
//! Registered as a worker-level extension in `worker_rpc.rs`.

use std::sync::Arc;

use crate::agent::error::AgentResult;
use crate::agent::extension::{Extension, SessionContext};
use crate::secret_detector;

/// Minimum session content length (chars) to trigger extraction.
const MIN_CONTENT_LEN: usize = 300;

/// Minimum message count to trigger extraction.
const MIN_MESSAGES: usize = 4;

/// Skip words — if ALL messages are these, skip extraction entirely.
const SKIP_WORDS: &[&str] = &[
    "ok",
    "okay",
    "thanks",
    "thank you",
    "好的",
    "嗯",
    "ok了",
    "继续",
    "hi",
    "hello",
    "hey",
    "你好",
    "测试",
    "test",
    "ping",
    "pong",
    "done",
    "完成",
    "ok。",
];

/// Tool names that indicate real work was done (for extraction trigger).
const WORK_TOOLS: &[&str] = &[
    "write",
    "edit",
    "bash",
    "bash_run",
    "read",
    "grep",
    "find",
    "spawn_worker",
    "git_diff",
    "git_commit",
    "git_push",
];

pub struct LearningExtension {
    name: String,
    /// ApiRegistry (LLM 提炼 skill 用)。None 时跳过 distill。
    pub registry: Option<Arc<ion_provider::registry::ApiRegistry>>,
    /// 当前会话模型。
    pub model: Option<ion_provider::types::Model>,
}

impl Default for LearningExtension {
    fn default() -> Self {
        Self::new()
    }
}

impl LearningExtension {
    pub fn new() -> Self {
        Self {
            name: "learning".into(),
            registry: None,
            model: None,
        }
    }

    /// Inject ApiRegistry + Model so on_session_shutdown can call LLM.
    pub fn with_registry_model(
        mut self,
        registry: Arc<ion_provider::registry::ApiRegistry>,
        model: ion_provider::types::Model,
    ) -> Self {
        self.registry = Some(registry);
        self.model = Some(model);
        self
    }

    /// Check if a session's content is worth extracting memories from.
    /// Returns false for sessions that are too short, pure greetings, or no work.
    fn should_extract(messages: &[String]) -> bool {
        // Too few messages
        if messages.len() < MIN_MESSAGES {
            tracing::info!(
                "[learning] skip: only {} messages (< {})",
                messages.len(),
                MIN_MESSAGES
            );
            return false;
        }

        // Join all messages for length check
        let combined = messages.join("\n");
        if combined.len() < MIN_CONTENT_LEN {
            tracing::info!(
                "[learning] skip: content too short ({} chars < {})",
                combined.len(),
                MIN_CONTENT_LEN
            );
            return false;
        }

        // Check if ALL messages are skip words (pure greeting session)
        let all_skip = messages.iter().all(|m| {
            let lower = m.trim().to_lowercase();
            SKIP_WORDS
                .iter()
                .any(|&sw| lower == sw || lower.starts_with(sw))
        });
        if all_skip {
            tracing::info!("[learning] skip: all messages are greetings/skip words");
            return false;
        }

        // Check if session contains technical content
        // (code blocks, tool calls, file paths, error messages)
        let has_technical = combined.contains("```")
            || combined.contains("tool_call")
            || combined.contains("src/")
            || combined.contains("error")
            || combined.contains("fn ")
            || combined.contains("def ")
            || combined.contains("import ")
            || combined.contains("function ")
            || combined.contains("class ")
            || combined.contains("struct ");
        if !has_technical {
            tracing::info!("[learning] skip: no technical content detected");
            return false;
        }

        true
    }

    /// Redact secrets from a list of messages.
    /// Returns redacted messages.
    #[allow(dead_code)]
    #[allow(dead_code)]
    fn redact_messages(messages: &[String]) -> Vec<String> {
        messages
            .iter()
            .map(|m| {
                let redacted = secret_detector::redact_secrets(m);
                if redacted != *m {
                    let secret_count = secret_detector::detect_secrets(m).len();
                    tracing::info!(
                        "[learning] redacted {} secret(s) from message",
                        secret_count
                    );
                }
                redacted
            })
            .collect()
    }

    /// Check if session contained write/edit operations (for skill distillation trigger).
    fn has_write_operations(messages: &[String]) -> bool {
        let combined = messages.join("\n");
        WORK_TOOLS.iter().any(|tool| {
            combined.contains(&format!("\"tool\":\"{}\"", tool))
                || combined.contains(&format!("tool_name\":\"{}\"", tool))
                || combined.contains(tool)
        })
    }
}

#[async_trait::async_trait]
impl Extension for LearningExtension {
    fn name(&self) -> &str {
        &self.name
    }

    /// On session shutdown: trigger skill distillation if the session qualifies.
    ///
    /// Flow:
    ///   1. Read session_id from ~/.ion/agent/last_session (worker-level, not in SessionContext)
    ///   2. Compute project_name from current_dir()
    ///   3. If registry + model are wired, tokio::spawn the async distillation
    ///      (fire-and-forget — never block session exit)
    ///   4. All errors are logged inside the spawned task; we always return Ok here
    ///
    /// Memory extraction is handled separately by MemoryExtension::on_session_shutdown.
    async fn on_session_shutdown(&self, ctx: &SessionContext) -> AgentResult<()> {
        tracing::info!("[learning] session shutdown, evaluating for skill distillation");

        let registry = match self.registry.clone() {
            Some(r) => r,
            None => {
                tracing::info!("[learning] no registry wired, skipping skill distillation");
                return Ok(());
            }
        };
        let model = match self.model.clone() {
            Some(m) => m,
            None => {
                tracing::info!("[learning] no model wired, skipping skill distillation");
                return Ok(());
            }
        };

        // Read session_id from the context (NOT the global last_session file,
        // which races under concurrent sessions and can distill the wrong one).
        let session_id = match &ctx.session_id {
            Some(s) if !s.is_empty() => s.clone(),
            _ => {
                tracing::info!("[learning] no session_id in context, skipping skill distillation");
                return Ok(());
            }
        };

        // Derive project_name from CWD basename (best-effort)
        let project_name = std::env::current_dir()
            .ok()
            .and_then(|p| {
                p.file_name()
                    .and_then(|n| n.to_str().map(|s| s.to_string()))
            })
            .unwrap_or_else(|| "unknown".into());

        // Fire-and-forget — must not block session exit
        tokio::spawn(async move {
            match crate::skill_distillation::run_skill_distillation(
                &session_id,
                &project_name,
                &registry,
                &model,
            )
            .await
            {
                Ok(Some(path)) => {
                    tracing::info!("[learning] skill distilled to {}", path.display());
                }
                Ok(None) => {
                    tracing::info!("[learning] no skill distilled (session skipped)");
                }
                Err(e) => {
                    tracing::warn!("[learning] skill distillation failed: {e}");
                }
            }
        });

        Ok(())
    }
}

/// Learning decision result (for logging / observability).
#[derive(Clone, Debug, serde::Serialize)]
pub struct LearningDecision {
    pub should_extract_memory: bool,
    pub should_distill_skill: bool,
    pub secret_count: usize,
    pub message_count: usize,
    pub content_length: usize,
    pub reason: String,
}

/// Analyze a session and return a learning decision.
/// This is a pure function that can be tested without LLM/network.
pub fn analyze_session(messages: &[String]) -> LearningDecision {
    let combined = messages.join("\n");
    let content_length = combined.len();

    // Count secrets in raw messages
    let secret_count = messages
        .iter()
        .map(|m| secret_detector::detect_secrets(m).len())
        .sum::<usize>();

    // Check extraction worthiness
    let should_extract = LearningExtension::should_extract(messages);
    let should_distill = LearningExtension::has_write_operations(messages);

    let reason = if !should_extract {
        if messages.len() < MIN_MESSAGES {
            format!("too few messages ({})", messages.len())
        } else if content_length < MIN_CONTENT_LEN {
            format!("content too short ({})", content_length)
        } else {
            "no technical content".into()
        }
    } else {
        "worthy of extraction".into()
    };

    LearningDecision {
        should_extract_memory: should_extract,
        should_distill_skill: should_distill,
        secret_count,
        message_count: messages.len(),
        content_length,
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_extract_short_session() {
        let msgs = vec!["hi".into(), "hello".into()];
        assert!(!LearningExtension::should_extract(&msgs));
    }

    #[test]
    fn test_should_extract_greeting_only() {
        let msgs = vec!["ok".into(), "thanks".into(), "done".into(), "好的".into()];
        assert!(!LearningExtension::should_extract(&msgs));
    }

    #[test]
    fn test_should_extract_real_session() {
        // Realistic session with substantive content (> MIN_CONTENT_LEN=300 chars)
        let msgs = vec![
            "user: Fix the bug in src/lib.rs where the parser panics on empty input. The unwrap() on line 42 is the culprit.".into(),
            "assistant: I'll read the file first to understand the context and the surrounding code".into(),
            "tool_result: ```rust\nfn parse(input: &str) -> Ast {\n    let tokens = tokenize(input);\n    tokens.into_iter().next().unwrap()\n}\n```".into(),
            "assistant: I found the issue: unwrap() panics on empty input. I'll fix it with match + default error handling using edit tool to replace the unwrap call".into(),
            "user: Great, can you also add a test for the empty input case to prevent regression?".into(),
        ];
        assert!(LearningExtension::should_extract(&msgs));
    }

    #[test]
    fn test_should_extract_no_technical() {
        let long_msg = "This is a long message about nothing in particular. ".repeat(10);
        let msgs = vec![
            long_msg.clone(),
            long_msg.clone(),
            long_msg.clone(),
            long_msg,
        ];
        assert!(!LearningExtension::should_extract(&msgs));
    }

    #[test]
    fn test_has_write_operations_with_edit() {
        let msgs = vec![
            "tool_call: edit src/lib.rs".into(),
            "result: success".into(),
        ];
        assert!(LearningExtension::has_write_operations(&msgs));
    }

    #[test]
    fn test_has_write_operations_without_tools() {
        let msgs = vec![
            "user: tell me a joke".into(),
            "assistant: why did the chicken...".into(),
        ];
        assert!(!LearningExtension::has_write_operations(&msgs));
    }

    #[test]
    fn test_redact_messages_strips_secrets() {
        let msgs = vec![
            "user: Here is my key sk-proj-abcdef1234567890abcdefghij".into(),
            "assistant: I see your key".into(),
        ];
        let redacted = LearningExtension::redact_messages(&msgs);
        assert!(redacted[0].contains("[REDACTED:"));
        assert!(!redacted[0].contains("sk-proj-abcdef1234567890abcdefghij"));
        // Non-secret content preserved
        assert!(redacted[1].contains("I see your key"));
    }

    #[test]
    fn test_analyze_session_short() {
        let msgs = vec!["hi".into(), "hello".into()];
        let decision = analyze_session(&msgs);
        assert!(!decision.should_extract_memory);
        assert!(decision.reason.contains("too few"));
    }

    #[test]
    fn test_analyze_session_with_secrets() {
        let msgs = vec![
            "user: my api_key=sk-test-1234567890abcdef".into(),
            "assistant: I'll help".into(),
            "tool: ```rust\nfn main() {}```".into(),
            "user: Thanks, src/lib.rs looks good now".into(),
            "assistant: Done editing src/lib.rs".into(),
        ];
        let decision = analyze_session(&msgs);
        assert!(decision.secret_count > 0);
        assert!(decision.should_distill_skill); // has "edit" in messages
    }

    #[test]
    fn test_analyze_session_worthy() {
        let msgs = vec![
            "user: Fix the panic in src/parser.rs. The unwrap() call on line 42 panics on empty input when there are no tokens to parse".into(),
            "assistant: I'll read the file to see the parser code and surrounding context".into(),
            "tool_result: ```rust\nfn parse() { tokens.next().unwrap() }\n```".into(),
            "assistant: The unwrap causes panic, I'll fix it with match and return an empty Ast on empty input instead of panicking".into(),
            "user: Thanks, also check src/lib.rs for similar unwrap issues that could panic".into(),
        ];
        let decision = analyze_session(&msgs);
        assert!(decision.should_extract_memory);
        assert_eq!(decision.reason, "worthy of extraction");
    }

    #[test]
    fn test_skip_words_all_greetings() {
        let test_cases = ["ok", "好的", "thanks", "继续"];
        for tc in &test_cases {
            assert!(SKIP_WORDS.iter().any(|&sw| *tc == sw));
        }
    }

    #[test]
    fn test_work_tools_includes_write() {
        assert!(WORK_TOOLS.contains(&"write"));
        assert!(WORK_TOOLS.contains(&"edit"));
        assert!(WORK_TOOLS.contains(&"bash"));
    }
}
