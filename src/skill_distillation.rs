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

use crate::learning_extension::{LearningDecision, analyze_session};
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
    _model: &ion_provider::types::Model,
) -> Result<Option<PathBuf>, String> {
    tracing::info!(
        "[skill-distill] processing session {} for project {}",
        session_id,
        project_name
    );

    // Step 1: Resolve session JSONL path.
    //
    // Sessions are stored as `~/.ion/agent/sessions/--<cwd_hash>--<dir>--/<file>.jsonl`.
    // The main worker uses `session.jsonl`; forked/branched workers use `<session_id>.jsonl`.
    // We don't have CWD context here, so we search for the file by name.
    let session_file = resolve_session_file(session_id);
    let session_file = match session_file {
        Some(p) => p,
        None => {
            tracing::warn!(
                "[skill-distill] session file not found for {} (searched sessions/*/{session_id}.jsonl + session.jsonl)",
                session_id
            );
            return Ok(None);
        }
    };
    let content = std::fs::read_to_string(&session_file)
        .map_err(|e| format!("read session file {}: {e}", session_file.display()))?;

    // Step 2: Parse + extract text per entry.
    //
    // ION session JSONL uses pi-style tagged unions. Two on-disk variants exist:
    //   - cmd_run (save_session): {"type":"message","message":{"User":{"content":[{"Text":...}]}}}
    //   - worker (custom_message): {"customType":"message","message":{...}}
    // We accept both by checking either field.
    let raw_messages: Vec<String> = content
        .lines()
        .filter(|l| !l.is_empty())
        .take(300)
        .filter_map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).ok()?;
            // Accept either type="message" or customType="message"
            let is_message = v.get("type").and_then(|s| s.as_str()) == Some("message")
                || v.get("customType").and_then(|s| s.as_str()) == Some("message");
            if !is_message {
                return None;
            }
            let msg = v.get("message")?;
            // message is an object with exactly one key: User | Assistant | Tool
            let msg_obj = msg.as_object()?;
            if msg_obj.len() != 1 {
                return None;
            }
            let (role, inner) = msg_obj.iter().next()?;
            let content_arr = inner.get("content")?.as_array()?;
            let parts: Vec<String> = content_arr.iter().filter_map(extract_block_text).collect();
            if parts.is_empty() {
                None
            } else {
                Some(format!("{}: {}", role, parts.join(" | ")))
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

    // ★ Migrated to IonConfig::query_tier (was 50 lines of manual Context +
    // StreamOptions + complete + text extraction). query_tier handles tier
    // resolution + api_key + options construction internally.
    let user_msg = format!(
        "Project: {project_name}\nSession ID: {session_id}\n\nTranscript:\n{conversation_text}"
    );
    let skill_text = crate::config::IonConfig::load()
        .query_tier(
            registry,
            "fast", // skill 蒸馏用 fast tier（经济）
            &system_prompt,
            &user_msg,
            false, // 不需要 JSON
        )
        .await?;

    // Step 6: Skip if LLM declined
    if skill_text.is_empty() || skill_text == "NO_SKILL" || skill_text.starts_with("NO_SKILL") {
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
    std::fs::write(&skill_path, &full).map_err(|e| format!("write skill file: {e}"))?;

    tracing::info!(
        "[skill-distill] skill written to {} ({} bytes)",
        skill_path.display(),
        full.len()
    );

    Ok(Some(skill_path))
}

/// Public wrapper around [`resolve_session_file`] for cross-module use
/// (e.g. `agent::memory` reuses this to fix its own path bug).
pub fn resolve_session_file_pub(session_id: &str) -> Option<PathBuf> {
    resolve_session_file(session_id)
}

/// Extract readable text from a single content block.
///
/// Content blocks are tagged unions: `{"Text":{"text":"..."}}`, `{"ToolCall":{...}}`,
/// `{"ToolResult":{"content":"..."}}`, etc. Returns a short human-readable summary
/// of each block (suitable for LLM skill distillation).
pub fn extract_block_text(block: &serde_json::Value) -> Option<String> {
    let obj = block.as_object()?;
    if obj.len() != 1 {
        return None;
    }
    let (tag, inner) = obj.iter().next()?;

    match tag.as_str() {
        "Text" => inner
            .get("text")
            .and_then(|t| t.as_str())
            .map(|s| s.to_string()),
        "ToolCall" => {
            // Summarize tool calls so the LLM sees "what was done" without dumping huge args
            let name = inner.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
            let args = inner
                .get("arguments")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let args_summary = summarize_args(&args);
            Some(format!("[tool_call:{name} {args_summary}]"))
        }
        "ToolResult" => {
            // Just take a short preview of the result
            let content = inner.get("content").unwrap_or(&serde_json::Value::Null);
            let s = match content {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            let preview: String = s.chars().take(120).collect();
            Some(format!("[tool_result:{preview}]"))
        }
        "Thinking" => None, // Skip thinking blocks (not relevant to skill)
        other => {
            // Unknown block type — include a stub for visibility
            Some(format!("[{other}]"))
        }
    }
}

/// Produce a compact summary of a tool call's arguments for the LLM.
/// E.g. `{"file_path":"src/main.rs","content":"fn main(){}"}` →
///      `file_path=src/main.rs content=...`
fn summarize_args(args: &serde_json::Value) -> String {
    let obj = match args.as_object() {
        Some(o) => o,
        None => return args.to_string(),
    };
    let parts: Vec<String> = obj
        .iter()
        .map(|(k, v)| {
            let v_str: String = match v {
                serde_json::Value::String(s) => s.chars().take(60).collect(),
                _ => v.to_string().chars().take(60).collect(),
            };
            format!("{k}={v_str}")
        })
        .collect();
    parts.join(" ")
}

/// Resolve a session JSONL file path from session_id alone.
///
/// Sessions are stored under `~/.ion/agent/sessions/<subdir>/<file>.jsonl` where:
///   - `<subdir>` = `--<cwd_hash>--<dir_name>--` (one per working directory)
///   - `<file>` = `session.jsonl` (main worker) or `<session_id>.jsonl` (forked worker)
///
/// Strategy:
///   1. Search ALL subdirectories for a file named exactly `<session_id>.jsonl` (forked).
///   2. If not found, fall back to the file pointed to by `~/.ion/agent/last_session`
///      (which contains the session_id; we find its parent dir's session.jsonl).
///   3. If still not found, return None.
fn resolve_session_file(session_id: &str) -> Option<PathBuf> {
    let sessions_root = paths::sessions_dir();

    // Safe-ify session_id (mirrors session_jsonl_path_by_id)
    let safe_id: String = session_id
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    let target_name = format!("{safe_id}.jsonl");

    // Strategy 1: search all subdirs for <session_id>.jsonl
    if let Ok(entries) = std::fs::read_dir(&sessions_root) {
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            let candidate = dir.join(&target_name);
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }

    // Strategy 2: read the first line of each subdir's session.jsonl; match by `id` field.
    // The first line is a session header JSON like {"type":"session","id":"sess_xxx",...}.
    if let Ok(entries) = std::fs::read_dir(&sessions_root) {
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            let candidate = dir.join("session.jsonl");
            if let Ok(content) = std::fs::read_to_string(&candidate)
                && let Some(first_line) = content.lines().next()
                && let Ok(v) = serde_json::from_str::<serde_json::Value>(first_line)
                && v.get("id").and_then(|i| i.as_str()) == Some(session_id)
            {
                return Some(candidate);
            }
        }
    }

    None
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
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else if c == ' ' || c == '_' {
                '-'
            } else {
                '-'
            }
        })
        .collect();
    // Collapse consecutive dashes, trim leading/trailing
    let mut out = String::with_capacity(slug.len());
    let mut prev_dash = false;
    for c in slug.chars() {
        if c == '-' {
            if !prev_dash {
                out.push('-');
            }
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

/// Resolve the API key for a provider, mirroring `build_registry_and_model`:
///   1. Environment variable (e.g. ZAI_API_KEY / ION_API_KEY)
///   2. config.json `providers[provider].api_key`
///   3. auth.json `provider_api_keys[provider]` and top-level `api_key`
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
        assert_eq!(
            slugify("---leading and trailing---"),
            "leading-and-trailing"
        );
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

    #[test]
    fn test_resolve_session_file_forked_match() {
        let _guard = crate::paths::env_test_lock();
        // Create a temp sessions dir, search for a forked file by session_id
        let tmp = std::env::temp_dir().join(format!("ion-skill-test-{}", std::process::id()));
        let subdir = tmp.join("sessions").join("--abc--foo--");
        std::fs::create_dir_all(&subdir).unwrap();
        let target = subdir.join("sess_xyz123.jsonl");
        std::fs::write(&target, "{}\n").unwrap();

        // SAFETY: tests are single-threaded within this binary; set/remove var is benign.
        unsafe {
            std::env::set_var("ION_SESSION_DIR", tmp.join("sessions"));
        }
        let found = resolve_session_file("sess_xyz123");
        unsafe {
            std::env::remove_var("ION_SESSION_DIR");
        }

        assert_eq!(found, Some(target.clone()));

        // Cleanup
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_resolve_session_file_main_fallback() {
        let _guard = crate::paths::env_test_lock();
        // No forked file by id — should match via session header's `id` field
        let tmp = std::env::temp_dir().join(format!("ion-skill-test2-{}", std::process::id()));
        let subdir = tmp.join("sessions").join("--def--bar--");
        std::fs::create_dir_all(&subdir).unwrap();
        let main = subdir.join("session.jsonl");
        // Write a session header line with matching id
        std::fs::write(&main, "{\"type\":\"session\",\"id\":\"sess_target\"}\n").unwrap();

        unsafe {
            std::env::set_var("ION_SESSION_DIR", tmp.join("sessions"));
        }
        let found = resolve_session_file("sess_target");
        unsafe {
            std::env::remove_var("ION_SESSION_DIR");
        }

        assert_eq!(found, Some(main.clone()));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_resolve_session_file_not_found() {
        let _guard = crate::paths::env_test_lock();
        let tmp = std::env::temp_dir().join(format!("ion-skill-test3-{}", std::process::id()));
        std::fs::create_dir_all(tmp.join("sessions")).unwrap();

        unsafe {
            std::env::set_var("ION_SESSION_DIR", tmp.join("sessions"));
        }
        let found = resolve_session_file("sess_ghost");
        unsafe {
            std::env::remove_var("ION_SESSION_DIR");
        }

        assert_eq!(found, None);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_resolve_session_file_path_traversal_safe() {
        let _guard = crate::paths::env_test_lock();
        // session_id with path-traversal chars gets filtered
        let tmp = std::env::temp_dir().join(format!("ion-skill-test4-{}", std::process::id()));
        std::fs::create_dir_all(tmp.join("sessions")).unwrap();

        unsafe {
            std::env::set_var("ION_SESSION_DIR", tmp.join("sessions"));
        }
        // Even with ../, the safe_id filter strips to alphanumeric/dash/underscore
        let found = resolve_session_file("../../etc/passwd");
        unsafe {
            std::env::remove_var("ION_SESSION_DIR");
        }

        assert_eq!(found, None); // No file named "etcpasswd.jsonl" exists

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_extract_block_text_user_text() {
        let block = serde_json::json!({"Text":{"text":"hello world"}});
        assert_eq!(extract_block_text(&block), Some("hello world".into()));
    }

    #[test]
    fn test_extract_block_text_tool_call() {
        let block = serde_json::json!({
            "ToolCall": {
                "name": "write",
                "arguments": {"file_path": "src/main.rs", "content": "fn main(){}"}
            }
        });
        let out = extract_block_text(&block).unwrap();
        assert!(out.starts_with("[tool_call:write"));
        assert!(out.contains("file_path=src/main.rs"));
    }

    #[test]
    fn test_extract_block_text_tool_result_truncated() {
        let long_text = "x".repeat(500);
        let block = serde_json::json!({"ToolResult":{"content":long_text}});
        let out = extract_block_text(&block).unwrap();
        assert!(out.starts_with("[tool_result:"));
        // Should be truncated to ~120 chars + prefix
        assert!(out.len() < 200);
    }

    #[test]
    fn test_extract_block_text_thinking_skipped() {
        let block = serde_json::json!({"Thinking":{"text":"internal reasoning"}});
        assert_eq!(extract_block_text(&block), None);
    }

    #[test]
    fn test_summarize_args_long_value_truncated() {
        let args = serde_json::json!({"content": "x".repeat(200)});
        let out = summarize_args(&args);
        assert!(out.len() < 80);
        assert!(out.starts_with("content="));
    }

    #[test]
    fn test_summarize_args_non_object() {
        let args = serde_json::json!("just a string");
        let out = summarize_args(&args);
        assert_eq!(out, "\"just a string\"");
    }
}
