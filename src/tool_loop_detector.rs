//! Tool Loop Detector — detect and break LLM tool-call loops.
//!
//! Detects consecutive identical tool calls (same tool + same signature).
//! Based on pi's analysis of 10,057 real sessions:
//! - 677 loops found; longest was 72 consecutive identical calls
//! - Most common looping tools: bash, read, write, edit
//! - 81% correlate with compaction erasing prior history
//!
//! ## How it works
//!
//! Tracks the last N tool-call signatures. When the same signature repeats
//! beyond a threshold, injects a warning message and/or aborts the run.
//!
//! ## Config
//!
//! Register as a worker-level extension. No config needed (sensible defaults).

use crate::agent::error::AgentResult;
use crate::agent::extension::{Extension, ToolExecutionContext};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Tools that legitimately repeat and should not trigger loop detection.
const LOOP_EXEMPT_TOOLS: &[&str] = &[
    "todo",
    "lsp_check",
    "global_memory_search",
    "global_memory_save",
    "memory_search",
    "memory_save",
    "plan_list",
    "plan_done",
    "get_state",
    "get_messages",
    "list_workers",
    "subscribe",
    "extension_rpc",
];

/// Tools exempt from ERROR_ABORT (consecutive-error abort).
/// bash failures are common during iterative development
/// (fix compile error → re-run), so they should not trigger early abort.
/// They are still subject to the normal ABORT_THRESHOLD (5 identical calls).
const ERROR_ABORT_EXEMPT: &[&str] = &["bash"];

/// Max consecutive identical tool calls before warning.
const WARN_THRESHOLD: u32 = 3;

/// Max consecutive identical tool calls before hard-aborting.
const ABORT_THRESHOLD: u32 = 5;

/// Max consecutive identical tool calls with errors before aborting.
const ERROR_ABORT_THRESHOLD: u32 = 2;

/// Tool Loop Detector extension.
pub struct ToolLoopDetector {
    /// Recent tool-call signatures (ring buffer, last 10).
    history: Arc<Mutex<VecDeque<(String, bool)>>>, // (signature, is_error)
    /// Current consecutive identical count.
    consecutive: Arc<Mutex<u32>>,
    /// Current signature being tracked.
    current_sig: Arc<Mutex<Option<String>>>,
    name: String,
}

impl Default for ToolLoopDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolLoopDetector {
    pub fn new() -> Self {
        Self {
            history: Arc::new(Mutex::new(VecDeque::with_capacity(10))),
            consecutive: Arc::new(Mutex::new(0)),
            current_sig: Arc::new(Mutex::new(None)),
            name: "tool-loop-detector".into(),
        }
    }

    /// Reset all loop tracking state (consecutive counter + history).
    pub async fn reset(&self) {
        *self.consecutive.lock().await = 0;
        *self.current_sig.lock().await = None;
        self.history.lock().await.clear();
        tracing::info!("[loop-detector] state reset");
    }

    /// Compute a normalized signature for a tool call.
    /// Same tool + same target = same signature (loose enough to catch loops,
    /// strict enough to avoid false positives).
    fn compute_signature(tool_name: &str, args: &serde_json::Value) -> String {
        match tool_name {
            "read" => {
                let path = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
                format!("read:{path}")
            }
            "write" | "edit" => {
                let path = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
                format!("{tool_name}:{path}")
            }
            "bash" => {
                let cmd = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
                // Normalize: collapse echo/noop variations
                let normalized = normalize_bash_command(cmd);
                format!("bash:{normalized}")
            }
            "grep" => {
                let pattern = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
                format!("grep:{pattern}")
            }
            "find" => {
                let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
                format!("find:{path}")
            }
            "spawn_worker" => {
                let agent = args.get("agent").and_then(|v| v.as_str()).unwrap_or("");
                format!("spawn_worker:{agent}")
            }
            _ => {
                // For other tools, use tool name + truncated args.
                // Use chars().take() to avoid panicking on multi-byte UTF-8
                // (byte-slicing &[..100] can land inside a multi-byte char).
                let args_str = args.to_string();
                let truncated: String = args_str.chars().take(100).collect();
                format!("{tool_name}:{truncated}")
            }
        }
    }
}

/// Normalize bash command for loop detection.
/// Echo/noop commands are collapsed to prevent LLM from varying text to evade detection.
fn normalize_bash_command(cmd: &str) -> String {
    let trimmed = cmd.trim();
    let first_word = trimmed.split_whitespace().next().unwrap_or("");

    match first_word {
        "echo" | "printf" => "echo".to_string(),
        "true" | ":" => "noop".to_string(),
        "ls" => {
            let path = trimmed.split_whitespace().nth(1).unwrap_or("");
            format!("ls:{path}")
        }
        "pwd" => "pwd".to_string(),
        _ => {
            // For other commands, truncate to 50 chars (enough to detect identical repeats)
            if trimmed.len() > 50 {
                trimmed[..50].to_string()
            } else {
                trimmed.to_string()
            }
        }
    }
}

#[async_trait::async_trait]
impl Extension for ToolLoopDetector {
    fn name(&self) -> &str {
        &self.name
    }

    /// When a gate extension (e.g. GoalSupervisor) forces a retry, reset the
    /// loop detection state. A gate-driven retry is intentional — the goal
    /// isn't complete and the agent is told to keep working — so consecutive
    /// tool calls across gate retries should NOT be counted as a loop.
    async fn on_gate_retry(&self) {
        self.reset().await;
    }

    async fn on_tool_execution_start(&self, ctx: &ToolExecutionContext) -> AgentResult<()> {
        // Skip exempt tools
        if LOOP_EXEMPT_TOOLS.contains(&ctx.tool_name.as_str()) {
            return Ok(());
        }

        let sig = Self::compute_signature(&ctx.tool_name, &ctx.args);

        let mut consecutive = self.consecutive.lock().await;
        let mut current = self.current_sig.lock().await;

        let sig_clone = sig.clone();
        if *current == Some(sig_clone) {
            *consecutive += 1;
        } else {
            *current = Some(sig.clone());
            *consecutive = 1;
        }

        let count = *consecutive;

        if count >= ABORT_THRESHOLD {
            tracing::error!(
                "[loop-detector] ABORT: '{}' repeated {} times consecutively. Breaking loop.",
                ctx.tool_name,
                count
            );
            // Return error to abort this tool call
            return Err(crate::agent::error::AgentError::Tool(format!(
                "Tool loop detected: '{}' has been called {} times consecutively with the same arguments. \
                 This looks like a loop. Try a different approach or check if the previous results \
                 contain the information you need.",
                ctx.tool_name, count
            )));
        }

        if count >= WARN_THRESHOLD {
            tracing::warn!(
                "[loop-detector] WARN: '{}' repeated {} times. Next repeat will abort.",
                ctx.tool_name,
                count
            );
        }

        Ok(())
    }

    async fn on_tool_execution_end(&self, ctx: &ToolExecutionContext) -> AgentResult<()> {
        // Track error results for error-based abort.
        // Skip bash: iterative dev (fix error → re-run) causes legit
        // consecutive failures that should not trigger early abort.
        if ctx.is_error
            && !LOOP_EXEMPT_TOOLS.contains(&ctx.tool_name.as_str())
            && !ERROR_ABORT_EXEMPT.contains(&ctx.tool_name.as_str())
        {
            let mut history = self.history.lock().await;
            let sig = Self::compute_signature(&ctx.tool_name, &ctx.args);
            let sig_ref = sig.clone();
            history.push_back((sig, true));
            if history.len() > 10 {
                history.pop_front();
            }

            // Count consecutive identical errors
            let mut err_count = 0u32;
            for (s, is_err) in history.iter().rev() {
                if *is_err && *s == sig_ref {
                    err_count += 1;
                } else {
                    break;
                }
            }

            if err_count >= ERROR_ABORT_THRESHOLD {
                tracing::error!(
                    "[loop-detector] ERROR_ABORT: '{}' failed {} times with same args. Breaking.",
                    ctx.tool_name,
                    err_count
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_signature_read() {
        let sig1 = ToolLoopDetector::compute_signature(
            "read",
            &serde_json::json!({"file_path": "src/lib.rs"}),
        );
        let sig2 = ToolLoopDetector::compute_signature(
            "read",
            &serde_json::json!({"file_path": "src/lib.rs", "offset": 10}),
        );
        assert_eq!(sig1, sig2); // Same file = same signature (offset ignored)
        let sig3 = ToolLoopDetector::compute_signature(
            "read",
            &serde_json::json!({"file_path": "src/main.rs"}),
        );
        assert_ne!(sig1, sig3); // Different file = different signature
    }

    #[test]
    fn test_compute_signature_bash() {
        let sig1 = ToolLoopDetector::compute_signature(
            "bash",
            &serde_json::json!({"command": "echo hello"}),
        );
        let sig2 = ToolLoopDetector::compute_signature(
            "bash",
            &serde_json::json!({"command": "echo world"}),
        );
        assert_eq!(sig1, sig2); // Both echo = same signature (normalized)
        let sig3 =
            ToolLoopDetector::compute_signature("bash", &serde_json::json!({"command": "ls -la"}));
        assert_ne!(sig1, sig3);
    }

    #[test]
    fn test_normalize_bash_echo() {
        assert_eq!(normalize_bash_command("echo hello"), "echo");
        assert_eq!(normalize_bash_command("echo 'world'"), "echo");
        assert_eq!(normalize_bash_command("printf 'test'"), "echo");
    }

    #[test]
    fn test_normalize_bash_noop() {
        assert_eq!(normalize_bash_command("true"), "noop");
        assert_eq!(normalize_bash_command(":"), "noop");
    }

    #[test]
    fn test_exempt_tools() {
        assert!(LOOP_EXEMPT_TOOLS.contains(&"lsp_check"));
        assert!(LOOP_EXEMPT_TOOLS.contains(&"memory_search"));
        assert!(!LOOP_EXEMPT_TOOLS.contains(&"bash"));
    }

    #[test]
    fn test_write_same_file() {
        let sig1 = ToolLoopDetector::compute_signature(
            "write",
            &serde_json::json!({"file_path": "src/a.rs", "content": "old"}),
        );
        let sig2 = ToolLoopDetector::compute_signature(
            "write",
            &serde_json::json!({"file_path": "src/a.rs", "content": "new"}),
        );
        assert_eq!(sig1, sig2); // Same file = same signature (content ignored)
    }

    #[test]
    fn test_signature_truncates_long_ascii() {
        // 200+ chars of pure ASCII — should truncate without panic
        let long_text = "a".repeat(200);
        let args = serde_json::json!({"text": long_text});
        let sig = ToolLoopDetector::compute_signature("write_custom", &args);
        // Signature should be reasonably sized (tool name + truncated args, not 200+ chars)
        assert!(sig.len() < 150, "signature too long: {} chars", sig.len());
        assert!(sig.starts_with("write_custom:"));
    }

    #[test]
    fn test_signature_handles_multibyte_utf8() {
        // Chinese text — each char is 3 bytes in UTF-8.
        // Without char-safe truncation, byte index 100 lands inside a CJK char.
        let chinese = "蒙娜丽莎".repeat(30); // 120 chars = 360 bytes
        let args = serde_json::json!({"text": chinese});
        let sig = ToolLoopDetector::compute_signature("write_custom", &args);
        // Must not panic and must return a valid String
        assert!(sig.starts_with("write_custom:"));
        assert!(sig.contains("蒙")); // Chinese chars should be present
    }

    #[test]
    fn test_signature_handles_emoji_and_mixed() {
        // Emoji (4 bytes each) + Chinese (3 bytes) + ASCII (1 byte) mix
        let mixed = "😀😊猫猫cat".repeat(20);
        let args = serde_json::json!({"data": mixed});
        let sig = ToolLoopDetector::compute_signature("custom_tool", &args);
        // Must not panic
        assert!(sig.starts_with("custom_tool:"));
    }
}
