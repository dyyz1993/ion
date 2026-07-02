use std::collections::HashMap;
use std::path::Path;

/// Paths to pi's export template files
const PI_EXPORT_DIR: &str = "/Users/xuyingzhou/Project/temporary/pi-momo-fork/packages/coding-agent/src/core/export-html";

/// Export a session to HTML using pi's template system.
pub fn export_session(session_id: &str, output_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let jsonl_path = crate::session_jsonl::session_path(session_id);
    if !jsonl_path.exists() {
        return Err(format!("session file not found: {}", jsonl_path.display()).into());
    }

    // Read JSONL file
    let content = std::fs::read_to_string(&jsonl_path)?;
    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        return Err("empty session file".into());
    }

    // Split: first line is header, rest are entries
    let header: serde_json::Value = serde_json::from_str(lines[0])?;
    let entries: Vec<serde_json::Value> = lines[1..].iter()
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
    let mut html = html.replace("{{CSS}}", &css);
    html = html.replace("{{SESSION_DATA}}", &session_data_b64);
    html = html.replace("{{MARKED_JS}}", &marked_js);
    html = html.replace("{{HIGHLIGHT_JS}}", &highlight_js);
    html = html.replace("{{JS}}", &js);
    // Also replace theme variables (use defaults)
    html = html.replace("{{THEME_VARS}}", "");
    html = html.replace("{{BODY_BG}}", "#fafafa");
    html = html.replace("{{CONTAINER_BG}}", "#ffffff");
    html = html.replace("{{INFO_BG}}", "#f5f5f5");

    // Set title
    html = html.replace("<title>Session Export</title>", 
        &format!("<title>Session {session_id}</title>"));

    let line_count = html.lines().count();
    std::fs::write(output_path, html)?;
    tracing::info!("exported {session_id} → {} ({} lines, {} entries)", 
        output_path.display(), line_count, entries.len());
    Ok(())
}

fn base64_encode(input: &str) -> String {
    // Simple base64 encoding without external deps
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = input.as_bytes();
    let mut result = String::new();

    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;

        result.push(CHARS[((triple >> 18) & 0x3F) as usize] as char);
        result.push(CHARS[((triple >> 12) & 0x3F) as usize] as char);
        result.push(if chunk.len() > 1 { CHARS[((triple >> 6) & 0x3F) as usize] as char } else { '=' });
        result.push(if chunk.len() > 2 { CHARS[(triple & 0x3F) as usize] as char } else { '=' });
    }

    result
}
