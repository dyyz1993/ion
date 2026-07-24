//! session-supervisor — WASM extension that quality-checks the agent's work.
//!
//! Hooks into `on_agent_end`, fetches the full conversation via
//! `host_get_messages`, scans the last assistant message for leftover
//! TODO/FIXME markers (and related smells), and — if any are found —
//! steers the agent to fix them before it is allowed to finish.
//!
//! ABI: ION WASM extension v1
//! Target: wasm32-wasip1 (std-enabled)
//! Build:  cargo build --target wasm32-wasip1 --release

use std::sync::{Mutex, OnceLock};

// ── Host functions provided by the ION WASM runtime ─────────────────────────
// Imported from "env"; resolved by wasmtime at instantiation time. All operate
// on (ptr, len) pairs inside this module's own WASM linear memory.

extern "C" {
    /// Write the full conversation (JSON) into `out_buf`.
    /// Returns bytes written; 0 means no messages or buffer too small.
    fn host_get_messages(out_buf: *mut u8, out_cap: u32) -> u32;
    /// Inject a steer message into the agent's steering queue.
    /// Returns 0 on success, 1 on error.
    fn host_steer(text_ptr: *const u8, text_len: u32) -> u32;
    /// Emit a textual message to the host event stream (no return value).
    fn host_send_message(msg_ptr: *const u8, msg_len: u32);
}

// ── Global state ────────────────────────────────────────────────────────────
// Store the most recent quality check so later `on_rpc` "status" queries can
// report it without re-running the scan. A plain `Mutex<Option<String>>` is
// the simplest portable container (no lazy_static crate available here).

fn last_result() -> &'static Mutex<Option<String>> {
    static STATE: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(None))
}

/// Replace the stored last-check result (serialized JSON object).
fn set_last_result(value: &serde_json::Value) {
    let body = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    if let Ok(mut slot) = last_result().lock() {
        *slot = Some(body);
    }
}

// ── Memory helpers ──────────────────────────────────────────────────────────
// The host hands us WASM-linear-memory offsets as u32; we treat them as raw
// pointers valid inside this module's own memory and copy bytes around.

/// Read a UTF-8 string from a (ptr, len) pair inside WASM memory.
fn read_wasm_string(ptr: u32, len: u32) -> String {
    unsafe {
        let slice = std::slice::from_raw_parts(ptr as *const u8, len as usize);
        String::from_utf8_lossy(slice).to_string()
    }
}

/// Copy `s` into the host-provided output buffer, truncating to `out_cap`.
/// Returns the number of bytes actually written.
fn write_wasm_string(s: &str, out_buf: u32, out_cap: u32) -> u32 {
    let bytes = s.as_bytes();
    let len = bytes.len().min(out_cap as usize);
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), out_buf as *mut u8, len); }
    len as u32
}

/// Thin wrapper around the raw `host_send_message` import.
fn send_message(msg: &str) {
    unsafe { host_send_message(msg.as_ptr(), msg.len() as u32); }
}

// ── Conversation retrieval ──────────────────────────────────────────────────

/// Fetch the full conversation from the host as a JSON value.
///
/// The host returns the agent's `get_full_messages` RPC result, typically
/// `{"messages":[...], "count":N}` — but we tolerate a bare array too.
/// A 256 KiB scratch buffer covers long sessions; if the host truncates we
/// still get a best-effort view.
fn fetch_messages() -> serde_json::Value {
    let mut buf = vec![0u8; 262_144]; // 256 KiB scratch buffer
    let n = unsafe { host_get_messages(buf.as_mut_ptr(), buf.len() as u32) };
    if n == 0 {
        return serde_json::Value::Null;
    }
    let raw = String::from_utf8_lossy(&buf[..n as usize]);
    serde_json::from_str(&raw).unwrap_or(serde_json::Value::Null)
}

/// Normalize whatever the host returned into a JSON array of messages.
fn messages_array(host_value: &serde_json::Value) -> Vec<serde_json::Value> {
    match host_value {
        serde_json::Value::Array(a) => a.clone(),
        serde_json::Value::Object(o) => o
            .get("messages")
            .and_then(|m| m.as_array())
            .cloned()
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// Heuristic: does this JSON message look like an assistant turn?
fn is_assistant_message(msg: &serde_json::Value) -> bool {
    if msg.get("type").and_then(|t| t.as_str()) == Some("assistant") {
        return true;
    }
    if msg.get("role").and_then(|r| r.as_str()) == Some("assistant") {
        return true;
    }
    // Internally tagged enum variant: {"Assistant":{...}}.
    msg.get("Assistant").is_some()
}

/// Extract the content array of the last assistant message, if any.
/// Handles both {"type":"assistant","content":[...]} and the internally
/// tagged form {"Assistant":{"content":[...]}}.
fn last_assistant_content(messages: &[serde_json::Value]) -> Option<&Vec<serde_json::Value>> {
    for msg in messages.iter().rev() {
        if !is_assistant_message(msg) {
            continue;
        }
        if let Some(c) = msg.get("content").and_then(|c| c.as_array()) {
            return Some(c);
        }
        if let Some(c) = msg.get("Assistant").and_then(|a| a.get("content")).and_then(|c| c.as_array()) {
            return Some(c);
        }
    }
    None
}

/// Concatenate the text blocks of the last assistant message.
fn last_assistant_text(messages: &[serde_json::Value]) -> String {
    let Some(content) = last_assistant_content(messages) else { return String::new(); };
    let mut text = String::new();
    for block in content {
        if block.get("type").and_then(|t| t.as_str()) == Some("text") {
            if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                text.push_str(t);
                text.push('\n');
            }
        }
    }
    text
}

/// Did the last assistant turn emit any tool calls?
fn last_assistant_used_tools(messages: &[serde_json::Value]) -> bool {
    if let Some(content) = last_assistant_content(messages) {
        for block in content {
            let bt = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
            if bt == "tool_use" || bt == "ToolCall" {
                return true;
            }
        }
    }
    // Some shapes carry tool calls in a sibling "tool_calls" array.
    for msg in messages.iter().rev() {
        if is_assistant_message(msg) {
            if let Some(calls) = msg.get("tool_calls").and_then(|c| c.as_array()) {
                return !calls.is_empty();
            }
            break;
        }
    }
    false
}

// ── Issue scanning ──────────────────────────────────────────────────────────

/// Leftover-marker smells detected in the assistant's final output.
/// Matched case-insensitively as substrings.
const MARKERS: &[&str] = &["TODO", "FIXME", "WIP", "not implemented", "placeholder"];

/// Scan `text` for any leftover markers. Returns the distinct markers found,
/// in the order they appear in `MARKERS`. Matching is case-insensitive.
fn find_markers(text: &str) -> Vec<&'static str> {
    let lower = text.to_lowercase();
    MARKERS.iter().filter(|m| lower.contains(&m.to_lowercase())).copied().collect()
}

/// Structured result of a single quality check.
struct CheckOutcome {
    markers: Vec<&'static str>,
    used_tools: bool,
    empty_reply: bool,
}

impl CheckOutcome {
    fn is_clean(&self) -> bool {
        self.markers.is_empty() && self.used_tools && !self.empty_reply
    }

    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "checked": true,
            "clean": self.is_clean(),
            "issues": self.markers,
            "issue_count": self.markers.len(),
            "used_tools": self.used_tools,
            "empty_reply": self.empty_reply,
        })
    }
}

/// Run the full quality check against a JSON messages value.
fn run_check(messages: &serde_json::Value) -> CheckOutcome {
    let arr = messages_array(messages);
    let text = last_assistant_text(&arr);
    let markers = find_markers(&text);
    CheckOutcome {
        markers,
        used_tools: last_assistant_used_tools(&arr),
        empty_reply: text.trim().is_empty(),
    }
}

// ── ABI entry points ─────────────────────────────────────────────────────────

/// Return the extension ABI version. Must be `1`.
#[no_mangle]
pub extern "C" fn extension_version() -> u32 {
    1
}

/// Called once when the host loads the extension.
#[no_mangle]
pub extern "C" fn extension_init() {
    send_message("session-supervisor initialized");
}

/// on_agent_end hook — runs when the agent says "done".
///
/// Signature: `extension_on_agent_end(json_ptr, json_len) -> u32`
/// Input JSON shape: {"turn_index":N,"message_count":N}
/// Returns: 0 (always — we steer via host_steer rather than returning non-zero).
///
/// Steps: fetch the conversation, scan the last assistant message for leftover
/// markers and related smells, steer the agent to fix issues if any, and store
/// the outcome for the "status" RPC.
#[no_mangle]
pub extern "C" fn extension_on_agent_end(json_ptr: u32, json_len: u32) -> u32 {
    let _ = read_wasm_string(json_ptr, json_len); // ack the (mostly ignored) payload

    let outcome = run_check(&fetch_messages());
    set_last_result(&outcome.to_json());

    if outcome.is_clean() {
        send_message("session-supervisor: clean, no action");
        return 0;
    }

    // Build a human-readable steer message so the next turn targets the issue.
    let mut notes: Vec<String> = Vec::new();
    if !outcome.markers.is_empty() {
        notes.push(format!(
            "you have {} leftover marker(s): {}",
            outcome.markers.len(),
            outcome.markers.join(", ")
        ));
    }
    if !outcome.used_tools {
        notes.push("your last turn used no tools".to_string());
    }
    if outcome.empty_reply {
        notes.push("your final reply was empty".to_string());
    }
    let steer_text = format!(
        "Wait — please fix before finishing: {}. Review your changes and resolve every TODO/FIXME, then confirm completion.",
        notes.join("; ")
    );

    unsafe { host_steer(steer_text.as_ptr(), steer_text.len() as u32); }
    send_message(&format!(
        "session-supervisor: flagged {} issue(s), steering agent",
        outcome.markers.len() + if outcome.used_tools { 0 } else { 1 }
    ));

    0
}

/// on_rpc hook — query supervisor status or run an ad-hoc check.
///
/// Signature:
///   extension_on_rpc(method_ptr, method_len, params_ptr, params_len,
///                    out_buf, out_cap) -> u32
/// Methods:
///   "status" — return the last `on_agent_end` outcome (or {"checked":false}).
///   "check"  — given {"messages":[...]} (or a bare array), run and report.
/// Returns bytes written to `out_buf`; 0 on error.
#[no_mangle]
pub extern "C" fn extension_on_rpc(
    method_ptr: u32,
    method_len: u32,
    params_ptr: u32,
    params_len: u32,
    out_buf: u32,
    out_cap: u32,
) -> u32 {
    let method = read_wasm_string(method_ptr, method_len);
    let params_raw = read_wasm_string(params_ptr, params_len);
    let params: serde_json::Value =
        serde_json::from_str(&params_raw).unwrap_or(serde_json::Value::Null);

    let response = match method.as_str() {
        "status" => {
            // Return the stored last-check result, or a placeholder.
            let stored = last_result()
                .lock()
                .ok()
                .and_then(|s| s.clone())
                .unwrap_or_else(|| r#"{"checked":false}"#.to_string());
            serde_json::from_str(&stored)
                .unwrap_or_else(|_| serde_json::json!({"checked": false, "raw": stored}))
        }
        "check" => {
            // Accept either a bare messages array or {"messages":[...]}.
            let messages = if params.is_array() {
                params
            } else {
                params.get("messages").cloned().unwrap_or(serde_json::Value::Null)
            };
            run_check(&messages).to_json()
        }
        _ => serde_json::json!({ "error": format!("unknown method: {}", method) }),
    };

    let body = match serde_json::to_string(&response) {
        Ok(s) => s,
        Err(_) => return 0,
    };
    write_wasm_string(&body, out_buf, out_cap)
}

// ── Unit tests ───────────────────────────────────────────────────────────────
// Run under the native target (cargo test) and exercise pure helper logic
// without touching host imports.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_todo_and_fixme() {
        let text = "I left a TODO here and a FIXME there.";
        let mut m = find_markers(text);
        m.sort();
        assert_eq!(m, vec!["FIXME", "TODO"]);
    }

    #[test]
    fn finds_case_insensitive_markers() {
        let text = "todo: finish this; wip section below";
        let m = find_markers(text);
        assert!(m.contains(&"TODO"));
        assert!(m.contains(&"WIP"));
    }

    #[test]
    fn clean_text_has_no_markers() {
        let text = "All done. The feature is fully implemented.";
        assert!(find_markers(text).is_empty());
    }

    #[test]
    fn last_assistant_text_extracts_text() {
        let msgs = serde_json::json!([{
            "type": "assistant",
            "content": [{"type": "text", "text": "Here is my TODO answer."}]
        }]);
        let text = last_assistant_text(&messages_array(&msgs));
        assert!(text.contains("TODO"));
    }

    #[test]
    fn detects_tool_use() {
        let msgs = serde_json::json!([{
            "type": "assistant",
            "content": [
                {"type": "text", "text": "running it"},
                {"type": "tool_use", "id": "1", "name": "bash"}
            ]
        }]);
        assert!(last_assistant_used_tools(&messages_array(&msgs)));
    }

    #[test]
    fn detects_no_tool_use() {
        let msgs = serde_json::json!([{
            "type": "assistant",
            "content": [{"type": "text", "text": "just talking"}]
        }]);
        assert!(!last_assistant_used_tools(&messages_array(&msgs)));
    }

    #[test]
    fn outcome_clean_when_no_markers_and_tools_used() {
        let outcome = CheckOutcome { markers: vec![], used_tools: true, empty_reply: false };
        assert!(outcome.is_clean());
    }

    #[test]
    fn outcome_dirty_with_markers() {
        let outcome = CheckOutcome { markers: vec!["TODO"], used_tools: true, empty_reply: false };
        assert!(!outcome.is_clean());
    }
}
