//! Phase 3: Skill Distillation
//!
//! When a session ends with write operations (write/edit/bash that modified files),
//! this module:
//!   1. Loads the session messages
//!   2. Secret-redacts them via `secret_detector::redact_secrets`
//!   3. Skips if not worth distilling (< 4 messages, < 300 chars, no write ops)
//!   4. Calls LLM to extract a reusable `.md` skill file
//!   5. Writes to `~/.ion/agent/skills/distilled-<slug>.md`
//!
//! Design principles:
//!   - Pure function `analyze_session` is shared with `learning_extension` (DRY)
//!   - All errors are logged, never propagated (best-effort — must not block session exit)
//!   - Idempotent: if a skill with the same slug already exists, skip (don't overwrite user edits)

use std::path::PathBuf;
use std::sync::Arc;

use crate::learning_extension::{analyze_session, LearningDecision};
use crate::paths;

/// Distill a skill from a session, writing to ~/.ion/agent/skills/.
///
/// This is the entry point invoked from `LearningExtension::on_session_shutdown`
/// in a `tokio::spawn` (non-blocking).
///
/// Returns Ok(Some(path)) if a skill was written, Ok(None) if skipped, Err on failure.
pub async fn run_skill_distillation(
    session_id: &str,
    project_name: &str,
    registry: &Arc<ion_provider::registry::ApiRegistry>,
    model: &ion_provider::types::Model,
) -> Result<Option<PathBuf>, String> {
    tracing::info!(
        "[skill-distill] processing session {} for project {}",
        session_id,
        project_name
    );

    // Step 1: Load session JSONL
    let session_file = paths::sessions_dir().join(format!("{}.jsonl", session_id));
    let content = std::fs::read_to_string(&session_file)
        .map_err(|e| format!("read session file: {e}"))?;

    // Step 2: Parse + extract text per entry
    let raw_messages: Vec<String> = content
        .lines()
        .filter(|l| !l.is_empty())
        .take(200)
        .filter_map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).ok()?;
            let role = v.get("role").and_then(|r| r.as_str()).unwrap_or("?");
            let text = v
                .get("text")
                .and_then(|t| t.as_str())
                .or_else(|| v.get("content").and_then(|c| c.as_str()))
                .unwrap_or("");
            if text.is_empty() {
                None
            } else {
                Some(format!("{role}: {text}"))
            }
        })
        .collect();

    if raw_messages.is_empty() {
        tracing::info!("[skill-distill] no messages in session, skipping");
        return Ok(None);
    }

    // Step 3: Analyze session — must have write operations to be worth distilling
    let decision: LearningDecision = analyze_session(&raw_messages);
    if !decision.should_distill_skill {
        tracing::info!(
            "[skill-distill] skip: {} (messages={}, content_len={})",
            decision.reason,
            decision.message_count,
            decision.content_length
        );
        return Ok(None);
    }

    // Step 4: Secret-redact messages
    let redacted: Vec<String> = raw_messages
        .iter()
        .map(|m| crate::secret_detector::redact_secrets(m))
        .collect();
    let conversation_text = redacted.join("\n");

    // Cap input size to avoid runaway tokens
    let conversation_text: String = conversation_text.chars().take(8000).collect();

    tracing::info!(
        "[skill-distill] session qualifies ({} secrets redacted), calling LLM",
        decision.secret_count
    );

    // Step 5: LLM call — distill a procedure
    let system_prompt = r#"You are a Skill Distillation Agent. Given a coding session transcript, extract a reusable step-by-step procedure (a "skill") that captures how the agent solved the problem.

Rules:
1. ONLY extract a skill if the session demonstrates a NON-TRIVIAL procedure (>= 3 distinct steps involving file edits or commands). Pure Q&A sessions, bug triage without resolution, or trivial edits should produce NO skill.
2. The skill must be GENERALIZABLE — describe the procedure, not the specific files/variables of this session.
3. Use plain Markdown format (H1 title, ## When to use, ## Steps (numbered), ## Notes).
4. Title should be action-oriented: "How to <verb> <noun>".
5. Each step should be 1-2 sentences with an example command if applicable.
6. Total length: 30-80 lines.
7. If the session is NOT worth distilling, output exactly: NO_SKILL

Output (Markdown only, no JSON, no code fence around the whole thing):"#;

    let context = ion_provider::types::Context {
        system_prompt: Some(system_prompt.into()),
        messages: vec![ion_provider::types::Message::User(
            ion_provider::types::UserMessage {
                role: "user".into(),
                content: vec![ion_provider::types::ContentBlock::Text(
                    ion_provider::types::TextContent {
                        text: format!(
                            "Project: {project_name}\nSession ID: {session_id}\n\nTranscript:\n{conversation_text}"
                        ),
                        text_signature: None,
                    },
                )],
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0),
                source: ion_provider::types::MessageSource::Prompt,
            },
        )],
        tools: None,
    };

    let options = ion_provider::StreamOptions {
        max_tokens: Some(2048),
        api_key: None,
        reasoning: None,
        timeout_ms: Some(90000),
        max_retries: None,
        response_format: None,
    };

    let response = ion_provider::registry::complete(registry, model, &context, Some(&options))
        .await
        .map_err(|e| format!("LLM call failed: {e}"))?;

    // Extract text from response
    let skill_text: String = response
        .content
        .iter()
        .filter_map(|c| {
            if let ion_provider::types::AssistantContentBlock::Text(t) = c {
                Some(t.text.clone())
            } else {
                None
            }
        })
        .collect::<String>()
        .trim()
        .to_string();

    // Step 6: Skip if LLM declined
    if skill_text.is_empty()
        || skill_text == "NO_SKILL"
        || skill_text.starts_with("NO_SKILL")
    {
        tracing::info!("[skill-distill] LLM declined (NO_SKILL or empty)");
        return Ok(None);
    }

    // Step 7: Derive slug from H1 title (first # line)
    let title = extract_h1_title(&skill_text).unwrap_or_else(|| format!("session-{session_id}"));
    let slug = slugify(&title);
    let skills_dir = paths::skills_dir();
    let _ = std::fs::create_dir_all(&skills_dir);
    let skill_path = skills_dir.join(format!("distilled-{slug}.md"));

    // Step 8: Idempotency — don't overwrite existing skill (user may have edited)
    if skill_path.exists() {
        tracing::info!(
            "[skill-distill] skill already exists at {}, skipping (idempotent)",
            skill_path.display()
        );
        return Ok(None);
    }

    // Step 9: Write skill file with provenance header
    let header = format!(
        "<!--\n  Distilled by ION Learning Extension\n  Session: {session_id}\n  Project: {project_name}\n  Generated: {ts}\n  Edit this file freely — it will NOT be overwritten.\n-->\n\n",
        ts = now_iso8601()
    );
    let full = format!("{header}{skill_text}\n");
    std::fs::write(&skill_path, &full)
        .map_err(|e| format!("write skill file: {e}"))?;

    tracing::info!(
        "[skill-distill] skill written to {} ({} bytes)",
        skill_path.display(),
        full.len()
    );

    Ok(Some(skill_path))
}

/// Extract the first H1 title from a Markdown string.
fn extract_h1_title(md: &str) -> Option<String> {
    for line in md.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("# ") {
            return Some(rest.trim().to_string());
        }
    }
    None
}

/// Convert a title into a filesystem-safe slug.
fn slugify(s: &str) -> String {
    let slug: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() }
            else if c == ' ' || c == '_' { '-' }
            else { '-' }
        })
        .collect();
    // Collapse consecutive dashes, trim leading/trailing
    let mut out = String::with_capacity(slug.len());
    let mut prev_dash = false;
    for c in slug.chars() {
        if c == '-' {
            if !prev_dash { out.push('-'); }
            prev_dash = true;
        } else {
            out.push(c);
            prev_dash = false;
        }
    }
    out.trim_matches('-').to_string()
}

fn now_iso8601() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("unix:{secs}")
}

// ═══════════════════════════════════════════════════════════════════════════
// Unit tests — pure functions only (no LLM, no filesystem)
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_slugify_basic() {
        assert_eq!(slugify("How to Fix Rust Panics"), "how-to-fix-rust-panics");
    }

    #[test]
    fn test_slugify_collapses_dashes() {
        assert_eq!(slugify("Foo   Bar---Baz"), "foo-bar-baz");
    }

    #[test]
    fn test_slugify_trims_dashes() {
        assert_eq!(slugify("---leading and trailing---"), "leading-and-trailing");
    }

    #[test]
    fn test_slugify_non_ascii() {
        // Non-ASCII chars become dashes (good enough for filenames)
        let s = slugify("Rust 修复 panic");
        assert!(s.starts_with("rust"));
        assert!(!s.contains("修"));
    }

    #[test]
    fn test_slugify_empty() {
        assert_eq!(slugify(""), "");
        assert_eq!(slugify("!!!"), "");
    }

    #[test]
    fn test_extract_h1_first_match() {
        let md = "# How to Debug Rust\n\nbody";
        assert_eq!(extract_h1_title(md), Some("How to Debug Rust".into()));
    }

    #[test]
    fn test_extract_h1_ignores_h2() {
        let md = "## Subsection\n\n# Real Title\nbody";
        assert_eq!(extract_h1_title(md), Some("Real Title".into()));
    }

    #[test]
    fn test_extract_h1_with_leading_whitespace() {
        let md = "    # Indented Title\nbody";
        assert_eq!(extract_h1_title(md), Some("Indented Title".into()));
    }

    #[test]
    fn test_extract_h1_no_h1() {
        let md = "Just text\n\nNo heading here";
        assert_eq!(extract_h1_title(md), None);
    }

    #[test]
    fn test_extract_h1_empty_string() {
        assert_eq!(extract_h1_title(""), None);
    }

    #[test]
    fn test_extract_h1_only_hash() {
        // A lone "#" without trailing space is not H1
        assert_eq!(extract_h1_title("#notitle\nbody"), None);
    }
}
