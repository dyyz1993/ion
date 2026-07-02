use crate::ids::SessionId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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
    pub parent_id: Option<String>,
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
    pub parent_session: Option<String>,
}

/// Message entry (the core conversation data).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MessageEntry {
    #[serde(rename = "type")]
    pub entry_type: String, // "message"
    pub id: String,
    pub parent_id: String,
    pub timestamp: String,
    pub message: serde_json::Value,
}

/// Session info entry (name changes).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionInfoEntry {
    #[serde(rename = "type")]
    pub entry_type: String, // "session_info"
    pub id: String,
    pub parent_id: String,
    pub timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Model change entry.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelChangeEntry {
    #[serde(rename = "type")]
    pub entry_type: String, // "model_change"
    pub id: String,
    pub parent_id: String,
    pub timestamp: String,
    pub provider: String,
    pub model_id: String,
}

/// Compaction entry.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompactionEntry {
    #[serde(rename = "type")]
    pub entry_type: String, // "compaction"
    pub id: String,
    pub parent_id: String,
    pub timestamp: String,
    pub summary: String,
    pub tokens_before: u64,
}

// ---------------------------------------------------------------------------
// Session path helpers
// ---------------------------------------------------------------------------

pub fn agent_dir() -> PathBuf {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".ion").join("agent")
}

pub fn sessions_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("ION_SESSION_DIR") {
        return PathBuf::from(dir);
    }
    agent_dir().join("sessions")
}

pub fn session_path(id: &str) -> PathBuf {
    sessions_dir().join(format!("{id}.jsonl"))
}

pub fn last_session_path() -> PathBuf {
    agent_dir().join("last_session")
}

pub fn index_path() -> PathBuf {
    agent_dir().join("sessions.index.json")
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
    // Use lower 32 bits for a pseudo-random 8-char hex
    format!("{:08x}", (nanos & 0xFFFFFFFF) as u32)
}

pub fn timestamp_iso() -> String {
    use std::time::SystemTime;
    let d = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs();
    let millis = d.subsec_millis();

    // Simple ISO 8601 without chrono
    let days_since_epoch = secs / 86400;
    let time_secs = secs % 86400;
    let h = time_secs / 3600;
    let m = (time_secs % 3600) / 60;
    let s = time_secs % 60;

    // Approximate date from days since epoch (2025-01-01 = days 20089)
    let mut y = 2025u64;
    let mut days_remaining = days_since_epoch.saturating_sub(20089);
    loop {
        let days_in_year = if is_leap(y) { 366 } else { 365 };
        if days_remaining < days_in_year { break; }
        days_remaining -= days_in_year;
        y += 1;
    }
    let month_days = if is_leap(y) {
        [31,29,31,30,31,30,31,31,30,31,30,31]
    } else {
        [31,28,31,30,31,30,31,31,30,31,30,31]
    };
    let mut mo = 1u64;
    for &md in &month_days {
        if days_remaining < md { break; }
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
    /// Load and parse a session file.
    pub fn load(id: &str) -> Option<Self> {
        let path = session_path(id);
        if !path.exists() { return None; }
        let content = std::fs::read_to_string(path).ok()?;
        let mut lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
        if lines.is_empty() { return None; }

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
                    if let Ok(msg) = serde_json::from_value::<crate::agent::messages::Message>(msg_val.clone()) {
                        messages.push(msg);
                    }
                }
            }

            entries.push(val);
        }

        let last_id = entries.last()
            .and_then(|e| e["id"].as_str().map(|s| s.to_string()))
            .or_else(|| Some(header.id.clone()));

        Some(Self { header, entries, last_id, messages })
    }

    /// Save a session: writes header + all entries as JSONL.
    pub fn save(id: &str, header: &SessionHeader, entries: &[serde_json::Value]) {
        let dir = sessions_dir();
        let _ = std::fs::create_dir_all(&dir);
        let path = session_path(id);

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
            let _ = std::fs::write(&path, &content);
            let _ = std::fs::write(last_session_path(), id);
        }
    }
}

// ---------------------------------------------------------------------------
// Helper: convert Message → JSONL entry
// ---------------------------------------------------------------------------

/// Convert a Message to a JSONL entry value with id/parentId chain.
pub fn message_to_entry(msg: &crate::agent::messages::Message, parent_id: &str) -> serde_json::Value {
    let msg_val = serde_json::to_value(msg).unwrap_or_default();
    serde_json::json!({
        "type": "message",
        "id": generate_id(),
        "parentId": parent_id,
        "timestamp": timestamp_iso(),
        "message": msg_val
    })
}
