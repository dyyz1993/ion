//! Session JSONL 存储 —— 对齐 pi 的 JSONL v3 格式。
//!
//! 文件位置: `sessions/--hash--cwd--/session.jsonl`
//! (不再使用 flat `sessions/{id}.jsonl`)
//!
//! ## JSONL 格式 (v3)
//!
//! ```json
//! {"type":"session","version":3,"id":"uuid","cwd":"...","timestamp":"..."}
//! {"type":"message","id":"...","parentId":"...","message":{...}}
//! {"type":"model_change","id":"...","parentId":"...","provider":"...","modelId":"..."}
//! {"type":"thinking_level_change","id":"...","parentId":"...","thinkingLevel":"..."}
//! {"type":"agent_change","id":"...","parentId":"...","agentName":"..."}
//! {"type":"compaction","id":"...","parentId":"...","summary":"...","tokensBefore":...}
//! {"type":"branch_summary","id":"...","parentId":"...","summary":"..."}
//! {"type":"custom","id":"...","parentId":"...","customType":"...","data":{...}}
//! ```

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Session entry types (pi JSONL spec v3)
// ---------------------------------------------------------------------------

/// Base fields for every session entry.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionEntryBase {
    #[serde(rename = "type")]
    pub entry_type: String,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parentId: Option<String>,
    pub timestamp: String,
}

/// Session header (first line).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionHeader {
    #[serde(rename = "type")]
    pub entry_type: String, // "session"
    pub version: u32,
    pub id: String,
    pub timestamp: String,
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parentSession: Option<String>,
}

/// Message entry (the core conversation data).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MessageEntry {
    #[serde(rename = "type")]
    pub entry_type: String, // "message"
    pub id: String,
    pub parentId: String,
    pub timestamp: String,
    pub message: serde_json::Value,
}

/// Model change entry.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelChangeEntry {
    #[serde(rename = "type")]
    pub entry_type: String, // "model_change"
    pub id: String,
    pub parentId: String,
    pub timestamp: String,
    pub provider: String,
    pub modelId: String,
}

/// Thinking level change entry.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ThinkingLevelChangeEntry {
    #[serde(rename = "type")]
    pub entry_type: String, // "thinking_level_change"
    pub id: String,
    pub parentId: String,
    pub timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinkingLevel: Option<String>,
}

/// Agent change entry.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentChangeEntry {
    #[serde(rename = "type")]
    pub entry_type: String, // "agent_change"
    pub id: String,
    pub parentId: String,
    pub timestamp: String,
    pub agentName: String,
}

/// Session info entry (name changes).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionInfoEntry {
    #[serde(rename = "type")]
    pub entry_type: String, // "session_info"
    pub id: String,
    pub parentId: String,
    pub timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Compaction entry.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompactionEntry {
    #[serde(rename = "type")]
    pub entry_type: String, // "compaction"
    pub id: String,
    pub parentId: String,
    pub timestamp: String,
    pub summary: String,
    pub tokensBefore: u64,
}

/// Branch summary entry.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BranchSummaryEntry {
    #[serde(rename = "type")]
    pub entry_type: String, // "branch_summary"
    pub id: String,
    pub parentId: String,
    pub timestamp: String,
    pub summary: String,
}

/// Custom entry.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CustomEntry {
    #[serde(rename = "type")]
    pub entry_type: String, // "custom"
    pub id: String,
    pub parentId: String,
    pub timestamp: String,
    pub customType: String,
    pub data: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Session path helpers (delegate to paths.rs)
// ---------------------------------------------------------------------------

/// Get the path to a session JSONL file for a given cwd.
/// Uses paths::session_jsonl_path which gives `sessions/--hash--cwd--/session.jsonl`.
pub fn session_path(cwd: &str) -> PathBuf {
    crate::paths::session_jsonl_path(cwd)
}

/// ~/.ion/agent/sessions/
pub fn sessions_dir() -> PathBuf {
    crate::paths::sessions_dir()
}

/// ~/.ion/agent/last_session
pub fn last_session_path() -> PathBuf {
    crate::paths::last_session_path()
}

/// ~/.ion/agent/sessions.index.json
pub fn index_path() -> PathBuf {
    crate::paths::sessions_index_path()
}

// ---------------------------------------------------------------------------
// ID generation (8-char hex, matching pi spec)
// ---------------------------------------------------------------------------

pub fn generate_id() -> String {
    use std::time::SystemTime;
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:08x}", (nanos & 0xFFFFFFFF) as u32)
}

pub fn timestamp_iso() -> String {
    use std::time::SystemTime;
    let d = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs();
    let millis = d.subsec_millis();

    let days_since_epoch = secs / 86400;
    let time_secs = secs % 86400;
    let h = time_secs / 3600;
    let m = (time_secs % 3600) / 60;
    let s = time_secs % 60;

    // Approximate date from days since epoch
    let mut y = 2025u64;
    let mut days_remaining = days_since_epoch.saturating_sub(20089);
    loop {
        let days_in_year = if is_leap(y) { 366 } else { 365 };
        if days_remaining < days_in_year {
            break;
        }
        days_remaining -= days_in_year;
        y += 1;
    }
    let month_days = if is_leap(y) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut mo = 1u64;
    for &md in &month_days {
        if days_remaining < md {
            break;
        }
        days_remaining -= md;
        mo += 1;
    }
    let day = days_remaining + 1;

    format!("{y:04}-{mo:02}-{day:02}T{h:02}:{m:02}:{s:02}.{millis:03}Z")
}

fn is_leap(y: u64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

// ---------------------------------------------------------------------------
// Full session file: read/write
// ---------------------------------------------------------------------------

/// All parsed entries from a session file.
pub struct SessionFile {
    pub header: SessionHeader,
    pub entries: Vec<serde_json::Value>,
    /// The last entry's ID (leaf).
    pub last_id: Option<String>,
    /// Cached messages for context reconstruction.
    pub messages: Vec<crate::agent::messages::Message>,
}

impl SessionFile {
    /// Load and parse a session file for a given cwd.
    pub fn load(cwd: &str) -> Option<Self> {
        let path = session_path(cwd);
        if !path.exists() {
            return None;
        }
        let content = std::fs::read_to_string(path).ok()?;
        let mut lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
        if lines.is_empty() {
            return None;
        }

        // First line is the header
        let header: SessionHeader = serde_json::from_str(lines[0]).ok()?;
        let mut entries: Vec<serde_json::Value> = Vec::new();
        let mut messages: Vec<crate::agent::messages::Message> = Vec::new();

        for line in &lines[1..] {
            let val: serde_json::Value = serde_json::from_str(line).ok()?;
            let entry_type = val["type"].as_str().unwrap_or("").to_string();

            // Extract messages
            if entry_type == "message" {
                if let Some(msg_val) = val.get("message") {
                    if let Ok(msg) =
                        serde_json::from_value::<crate::agent::messages::Message>(msg_val.clone())
                    {
                        messages.push(msg);
                    }
                }
            }

            entries.push(val);
        }

        let last_id = entries
            .last()
            .and_then(|e| e["id"].as_str().map(|s| s.to_string()))
            .or_else(|| Some(header.id.clone()));

        Some(Self {
            header,
            entries,
            last_id,
            messages,
        })
    }

    /// Save a session: writes header + all entries as JSONL.
    /// Uses cwd to determine the path.
    pub fn save(cwd: &str, header: &SessionHeader, entries: &[serde_json::Value]) {
        let dir = session_path(cwd);
        if let Some(parent) = dir.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        let mut lines = Vec::new();
        if let Ok(h) = serde_json::to_string(header) {
            lines.push(h);
        }
        for entry in entries {
            if let Ok(e) = serde_json::to_string(entry) {
                lines.push(e);
            }
        }
        let content = lines.join("\n");
        if !content.is_empty() {
            let _ = std::fs::write(&dir, &content);
            let _ = std::fs::write(last_session_path(), &header.id);
        }
    }
}

// ---------------------------------------------------------------------------
// Helper: convert Message → JSONL entry
// ---------------------------------------------------------------------------

/// Convert a Message to a JSONL entry value with id/parentId chain.
pub fn message_to_entry(
    msg: &crate::agent::messages::Message,
    parent_id: &str,
) -> serde_json::Value {
    let msg_val = serde_json::to_value(msg).unwrap_or_default();
    serde_json::json!({
        "type": "message",
        "id": generate_id(),
        "parentId": parent_id,
        "timestamp": timestamp_iso(),
        "message": msg_val
    })
}
