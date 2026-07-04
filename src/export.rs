//! Export a session to HTML using pi's template system.
//!
//! 引用: /Users/xuyingzhou/Project/temporary/pi-momo-fork/packages/coding-agent/src/core/export-html/

use std::path::Path;

/// Paths to pi's export template files
const PI_EXPORT_DIR: &str =
    "/Users/xuyingzhou/Project/temporary/pi-momo-fork/packages/coding-agent/src/core/export-html";

/// Export a session to HTML using pi's template system.
///
/// Resolves session by:
/// 1. Looking up session index (if available) for cwd
/// 2. Falling back to flat `sessions/{id}.jsonl` (legacy)
/// 3. Falling back to `sessions/--hash--id--/session.jsonl` (treat id as cwd)
pub fn export_session(
    session_id: &str,
    output_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    // Try to find the session file
    let jsonl_path = resolve_session_file(session_id)?;

    // Read JSONL file
    let content = std::fs::read_to_string(&jsonl_path)?;
    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        return Err("empty session file".into());
    }

    // Split: first line is header, rest are entries
    let header: serde_json::Value = serde_json::from_str(lines[0])?;
    let entries: Vec<serde_json::Value> = lines[1..]
        .iter()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();

    // Build SessionData JSON (matching pi's format)
    let session_data = serde_json::json!({
        "header": header,
        "entries": entries,
    });

    // Base64 encode
    let session_data_json = serde_json::to_string(&session_data)?;
    let session_data_b64 = base64_encode(&session_data_json);

    // Read template files
    let read_file = |name: &str| -> String {
        let path = format!("{PI_EXPORT_DIR}/{name}");
        std::fs::read_to_string(&path).unwrap_or_default()
    };

    let css = read_file("template.css");
    let js = read_file("template.js");
    let marked_js = read_file("vendor/marked.min.js");
    let highlight_js = read_file("vendor/highlight.min.js");
    let mut html = read_file("template.html");

    // Replace placeholders
    html = html.replace("{{CSS}}", &css);
    html = html.replace("{{SESSION_DATA}}", &session_data_b64);
    html = html.replace("{{MARKED_JS}}", &marked_js);
    html = html.replace("{{HIGHLIGHT_JS}}", &highlight_js);
    html = html.replace("{{JS}}", &js);
    html = html.replace("{{THEME_VARS}}", "");
    html = html.replace("{{BODY_BG}}", "#fafafa");
    html = html.replace("{{CONTAINER_BG}}", "#ffffff");
    html = html.replace("{{INFO_BG}}", "#f5f5f5");

    // Set title
    html = html.replace(
        "<title>Session Export</title>",
        &format!("<title>Session {session_id}</title>"),
    );

    std::fs::write(output_path, html)?;
    tracing::info!(
        "exported {session_id} → {} ({} entries)",
        output_path.display(),
        entries.len()
    );
    Ok(())
}

/// Resolve a session file path, trying multiple strategies.
fn resolve_session_file(session_id: &str) -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    // Strategy 1: Look up session in global index → get cwd → use cwd path
    let index = crate::session_index::SessionIndex::load();
    if let Some(meta) = index.get(session_id) {
        if let Some(ref project) = meta.project {
            let cwd_path = crate::session_jsonl::session_path(project);
            if cwd_path.exists() {
                // Verify the session file contains this session
                if let Some(file) = crate::session_jsonl::SessionFile::load(project) {
                    if file.header.id == session_id {
                        return Ok(cwd_path);
                    }
                }
            }
        }
    }

    // Strategy 2: Legacy flat format: sessions/{id}.jsonl
    let legacy_path = crate::paths::sessions_dir().join(format!("{session_id}.jsonl"));
    if legacy_path.exists() {
        return Ok(legacy_path);
    }

    // Strategy 3: Treat session_id as a cwd path (encoded)
    let cwd_path = crate::session_jsonl::session_path(session_id);
    if cwd_path.exists() {
        return Ok(cwd_path);
    }

    Err(format!(
        "session file not found for id '{}' (tried index, flat, and cwd path)",
        session_id
    )
    .into())
}

fn base64_encode(input: &str) -> String {
    const CHARS: &[u8] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = input.as_bytes();
    let mut result = String::new();

    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;

        result.push(CHARS[((triple >> 18) & 0x3F) as usize] as char);
        result.push(CHARS[((triple >> 12) & 0x3F) as usize] as char);
        result.push(if chunk.len() > 1 {
            CHARS[((triple >> 6) & 0x3F) as usize] as char
        } else {
            '='
        });
        result.push(if chunk.len() > 2 {
            CHARS[(triple & 0x3F) as usize] as char
        } else {
            '='
        });
    }

    result
}
