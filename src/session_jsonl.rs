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
//! {"type":"custom_message","id":"...","parentId":"...","customType":"...","content":"...","display":true}
//! {"type":"system_event","id":"...","parentId":"...","customType":"...","label":"...","display":true}
//! {"type":"label","id":"...","parentId":"...","targetId":"...","label":"..."}
//! {"type":"active_tools_change","id":"...","parentId":"...","activeToolNames":["bash","read"]}
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
    #[serde(rename = "parentId")]
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
    #[serde(rename = "parentSession")]
    pub parent_session: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

/// Message entry (the core conversation data).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MessageEntry {
    #[serde(rename = "type")]
    pub entry_type: String, // "message"
    pub id: String,
    #[serde(rename = "parentId")]
    pub parent_id: String,
    pub timestamp: String,
    pub message: serde_json::Value,
}

/// Model change entry.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelChangeEntry {
    #[serde(rename = "type")]
    pub entry_type: String, // "model_change"
    pub id: String,
    #[serde(rename = "parentId")]
    pub parent_id: String,
    pub timestamp: String,
    pub provider: String,
    #[serde(rename = "modelId")]
    pub model_id: String,
}

/// Thinking level change entry.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ThinkingLevelChangeEntry {
    #[serde(rename = "type")]
    pub entry_type: String, // "thinking_level_change"
    pub id: String,
    #[serde(rename = "parentId")]
    pub parent_id: String,
    pub timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "thinkingLevel")]
    pub thinking_level: Option<String>,
}

/// Agent change entry.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentChangeEntry {
    #[serde(rename = "type")]
    pub entry_type: String, // "agent_change"
    pub id: String,
    #[serde(rename = "parentId")]
    pub parent_id: String,
    pub timestamp: String,
    #[serde(rename = "agentName")]
    pub agent_name: String,
}

/// Session info entry (name changes).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionInfoEntry {
    #[serde(rename = "type")]
    pub entry_type: String, // "session_info"
    pub id: String,
    #[serde(rename = "parentId")]
    pub parent_id: String,
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
    #[serde(rename = "parentId")]
    pub parent_id: String,
    pub timestamp: String,
    pub summary: String,
    #[serde(rename = "tokensBefore")]
    pub tokens_before: u64,
    /// 分批压缩：批次数（0 = emergency / single）
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "batchCount")]
    pub batch_count: Option<usize>,
    /// 分批压缩阶段：single / batched_merged / batched_three_step / emergency
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage: Option<String>,
    /// 各批次 partial summary（审计用）
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "batchSummaries")]
    pub batch_summaries: Option<Vec<String>>,
    /// Step 2 输出的合并 summary（仅 batched_three_step 阶段有）
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "mergedSummary")]
    pub merged_summary: Option<String>,
    /// 压缩后保留的第一条 entry 的 id（对齐 pi firstKeptEntryId）
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "firstKeptEntryId")]
    pub first_kept_entry_id: Option<String>,
}

/// Branch summary entry.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BranchSummaryEntry {
    #[serde(rename = "type")]
    pub entry_type: String, // "branch_summary"
    pub id: String,
    #[serde(rename = "parentId")]
    pub parent_id: String,
    pub timestamp: String,
    pub summary: String,
}

/// Deletion entry (软删除标记).
/// 标记一批 message entry 为已删除，拉取层和 LLM context 层过滤掉它们。
/// JSONL 留痕（only-append 不变量），可通过删除此 entry 恢复。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeletionEntry {
    #[serde(rename = "type")]
    pub entry_type: String, // "deletion"
    pub id: String,
    #[serde(rename = "parentId")]
    pub parent_id: String,
    pub timestamp: String,
    /// 被删除的 message entry id 列表
    #[serde(rename = "targetIds")]
    pub target_ids: Vec<String>,
    /// 删除原因（审计用）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Segment summary entry (软压缩/折叠标记).
/// 将一批 message entry 替换成一条摘要（BranchSummary），原文在 JSONL 留痕。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SegmentSummaryEntry {
    #[serde(rename = "type")]
    pub entry_type: String, // "segment_summary"
    pub id: String,
    #[serde(rename = "parentId")]
    pub parent_id: String,
    pub timestamp: String,
    /// 被折叠的 message entry id 列表
    #[serde(rename = "targetIds")]
    pub target_ids: Vec<String>,
    /// LLM 生成或用户提供的摘要文本
    pub summary: String,
    /// 替换后的 BranchSummary entry id
    #[serde(rename = "summaryEntryId", skip_serializing_if = "Option::is_none")]
    pub summary_entry_id: Option<String>,
}

/// Custom entry.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CustomEntry {
    #[serde(rename = "type")]
    pub entry_type: String, // "custom"
    pub id: String,
    #[serde(rename = "parentId")]
    pub parent_id: String,
    pub timestamp: String,
    #[serde(rename = "customType")]
    pub custom_type: String,
    pub data: serde_json::Value,
}

/// CustomMessage entry (LLM 可见的扩展自定义消息).
/// 对齐 pi CustomMessageEntry.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CustomMessageEntry {
    #[serde(rename = "type")]
    pub entry_type: String, // "custom_message"
    pub id: String,
    #[serde(rename = "parentId")]
    pub parent_id: String,
    pub timestamp: String,
    #[serde(rename = "customType")]
    pub custom_type: String,
    pub content: serde_json::Value, // string | (TextContent | ImageContent)[]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
    pub display: bool,
}

/// System event entry (ION 原创设计，无 pi 对应).
/// 模型/agent 切换等系统事件，可选 UI 可见.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SystemEventEntry {
    #[serde(rename = "type")]
    pub entry_type: String, // "system_event"
    pub id: String,
    #[serde(rename = "parentId")]
    pub parent_id: String,
    pub timestamp: String,
    #[serde(rename = "customType")]
    pub custom_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub display: bool,
}

/// Label entry (书签标记).
/// 对齐 pi LabelEntry.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LabelEntry {
    #[serde(rename = "type")]
    pub entry_type: String, // "label"
    pub id: String,
    #[serde(rename = "parentId")]
    pub parent_id: String,
    pub timestamp: String,
    #[serde(rename = "targetId")]
    pub target_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Active tools change entry (工具集变更记录).
/// 对齐 pi ActiveToolsChangeEntry.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActiveToolsChangeEntry {
    #[serde(rename = "type")]
    pub entry_type: String, // "active_tools_change"
    pub id: String,
    #[serde(rename = "parentId")]
    pub parent_id: String,
    pub timestamp: String,
    #[serde(rename = "activeToolNames")]
    pub active_tool_names: Vec<String>,
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
        let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
        if lines.is_empty() {
            return None;
        }

        // First line is the header（损坏时用空 header 继续，不丢弃会话）
        let header: SessionHeader = serde_json::from_str(lines[0]).unwrap_or_else(|_| SessionHeader {
            entry_type: "session".into(),
            version: 3,
            id: "recovered".into(),
            timestamp: String::new(),
            cwd: cwd.into(),
            parent_session: None,
            agent: None,
            model: None,
            provider: None,
        });
        let mut entries: Vec<serde_json::Value> = Vec::new();

        for line in &lines[1..] {
            // 容错：跳过损坏的 JSON 行（半行写入/竞态交错），不丢弃整个会话
            let val: serde_json::Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("[session_jsonl] skipping corrupted line: {} ({})",
                        &line[..line.len().min(80)], e);
                    continue;
                }
            };
            entries.push(val);
        }

        // ── 计算 live path（只保留 root → current_leaf 路径上的 message）──
        // 如果有 leaf_pointer，被回滚的消息不在 live path 上，不加入 messages。
        let messages = filter_messages_on_live_path(&entries, &header.id);

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

// ═══════════════════════════════════════════════════════════════════════════
// Append helpers（only-append 不变量：只追加，不改/删旧行）
// ═══════════════════════════════════════════════════════════════════════════

/// 通用：往 session 文件追加一行 JSON entry。
/// 确保 session 文件有 header（第一行 type=="session"）。
///
/// 过滤出 live path 上的 message（F1 修复）。
///
/// 如果 entries 里有 leaf_pointer，用最后一个 leaf_pointer 的 leafId 作为起点，
/// 沿 parentId 链回溯到 root，收集这条路径上的所有 entry id。
/// 只反序列化 id 在这个集合里的 message entry。
///
/// 如果没有 leaf_pointer，所有 message 都在 live path 上（线性会话）。
fn filter_messages_on_live_path(
    entries: &[serde_json::Value],
    session_id: &str,
) -> Vec<crate::agent::messages::Message> {
    use std::collections::HashSet;

    // 找最后一个 leaf_pointer
    let leaf_id: Option<&str> = entries.iter().rev()
        .find(|e| e.get("type").and_then(|v| v.as_str()) == Some("leaf_pointer"))
        .and_then(|lp| lp.get("leafId").and_then(|v| v.as_str()));

    // 如果没有 leaf_pointer，所有 message 都加载
    let live_ids: Option<HashSet<String>> = if let Some(leaf) = leaf_id {
        // 沿 parentId 链从 leaf 回溯到 root
        let by_id: std::collections::HashMap<&str, &serde_json::Value> = entries.iter()
            .filter_map(|e| e.get("id").and_then(|v| v.as_str()).map(|id| (id, e)))
            .collect();

        let mut live = HashSet::new();
        let mut cur: Option<&str> = Some(leaf);
        let mut visited = HashSet::new();
        while let Some(id) = cur {
            if !visited.insert(id) { break; } // 环保护
            live.insert(id.to_string());
            cur = by_id.get(id)
                .and_then(|e| e.get("parentId").and_then(|v| v.as_str()));
            // parentId == session_id 或 null 时停（root 已加入）
            if cur == Some(session_id) || cur.is_none() {
                if let Some(sid) = cur { live.insert(sid.to_string()); }
                break;
            }
        }
        Some(live)
    } else {
        None
    };

    // 反序列化 live path 上的 message
    let mut messages = Vec::new();
    for val in entries {
        if val.get("type").and_then(|v| v.as_str()) != Some("message") {
            continue;
        }
        // 如果有 live_ids 过滤集，检查 entry id 是否在集合里
        if let Some(ref ids) = live_ids {
            let eid = val.get("id").and_then(|v| v.as_str()).unwrap_or("");
            if !ids.contains(eid) {
                continue; // 不在 live path 上，跳过
            }
        }
        if let Some(msg_val) = val.get("message") {
            if let Ok(msg) = serde_json::from_value::<crate::agent::messages::Message>(msg_val.clone()) {
                messages.push(msg);
            }
        }
    }
    messages
}

/// 如果文件不存在或第一行不是 session header，在文件开头插入 header。
/// 这防止 turn_summary / message 在 header 之前被追加（worker 启动时调用）。
///
/// 返回 true 如果新建了 header（之前不存在），false 如果已存在。
pub fn ensure_session_header(cwd: &str, sid: &str) -> bool {
    let path = session_path(cwd);

    // 文件不存在 → 创建 header
    if !path.exists() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // agent 名从环境变量读（ion_worker 启动时设的）
        let agent = std::env::var("ION_SESSION_AGENT").unwrap_or_default();
        let model = std::env::var("ION_SESSION_MODEL").unwrap_or_default();
        let provider = std::env::var("ION_SESSION_PROVIDER").unwrap_or_default();
        let mut header = serde_json::json!({
            "type": "session",
            "version": 3,
            "id": sid,
            "timestamp": timestamp_iso(),
            "cwd": cwd,
            "parentSession": null,
        });
        if !agent.is_empty() { header["agent"] = serde_json::json!(agent); }
        if !model.is_empty() { header["model"] = serde_json::json!(model); }
        if !provider.is_empty() { header["provider"] = serde_json::json!(provider); }
        let json = serde_json::to_string(&header).unwrap_or_default();
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).write(true).truncate(true).open(&path) {
            let _ = f.write_all(format!("{}\n", json).as_bytes());
        }
        return true;
    }

    // 文件存在 → 检查第一行是否是 session header
    if let Ok(content) = std::fs::read_to_string(&path) {
        let first_line = content.lines().next().unwrap_or("");
        if let Ok(first_val) = serde_json::from_str::<serde_json::Value>(first_line) {
            if first_val.get("type").and_then(|v| v.as_str()) == Some("session") {
                return false; // header 已存在
            }
        }
        // 第一行不是 session header → 需要在开头插入
        // 读取全部内容，prepend header，重写文件
        let header = serde_json::json!({
            "type": "session",
            "version": 3,
            "id": sid,
            "timestamp": timestamp_iso(),
            "cwd": cwd,
            "parentSession": null,
        });
        let header_json = serde_json::to_string(&header).unwrap_or_default();
        let new_content = format!("{}\n{}", header_json, content);
        let _ = std::fs::write(&path, new_content);
        return true;
    }

    false
}

/// 自动处理文件末尾换行防粘连。
/// 使用单次 write_all 避免 \n 和 JSON 之间的交错（并发安全）。
/// 全局 session 文件路径覆盖。
/// ion_worker 启动时设置（fork 子 Worker 用 <sid>.jsonl 而不是 session.jsonl）。
/// 如果设了，append_raw_entry / append_turn_summary 用这个路径。
static SESSION_FILE_OVERRIDE: std::sync::OnceLock<std::sync::Mutex<Option<std::path::PathBuf>>> = std::sync::OnceLock::new();

/// 设置全局 session 文件路径覆盖（ion_worker 启动时调）。
pub fn set_session_file_override(path: Option<std::path::PathBuf>) {
    let lock = SESSION_FILE_OVERRIDE.get_or_init(|| std::sync::Mutex::new(None));
    *lock.lock().unwrap() = path;
}

/// 获取 session 文件路径：优先用全局覆盖，否则用 session_path(cwd)。
fn resolve_session_file(cwd: &str) -> std::path::PathBuf {
    if let Some(lock) = SESSION_FILE_OVERRIDE.get() {
        if let Some(path) = lock.lock().unwrap().as_ref() {
            return path.clone();
        }
    }
    session_path(cwd)
}

pub fn append_raw_entry(cwd: &str, entry: &serde_json::Value) {
    let path = resolve_session_file(cwd);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let json = serde_json::to_string(entry).unwrap_or_default();
        // 合并 \n + JSON 为单次 write_all，避免两阶段写入的交错窗口
        let needs_newline = f.metadata().map(|m| m.len() > 0).unwrap_or(false);
        let payload = if needs_newline {
            format!("\n{}", json)
        } else {
            json
        };
        let _ = f.write_all(payload.as_bytes());
    }
}

/// 追加一条 leaf_pointer entry（移动光标到 leaf_id）。
pub fn append_leaf_pointer(cwd: &str, leaf_id: Option<&str>) {
    let entry = serde_json::json!({
        "type": "leaf_pointer",
        "id": generate_id(),
        "parentId": null,
        "timestamp": timestamp_iso(),
        "leafId": leaf_id,
    });
    append_raw_entry(cwd, &entry);
}

/// 追加一条 label entry（给 entry 命名）。
pub fn append_label(cwd: &str, target_id: &str, label: &str) {
    let entry = serde_json::json!({
        "type": "label",
        "id": generate_id(),
        "parentId": null,
        "timestamp": timestamp_iso(),
        "targetId": target_id,
        "label": label,
    });
    append_raw_entry(cwd, &entry);
}

/// 追加一条 branch_summary entry（tombstone，纯文本，不调 LLM）。
pub fn append_branch_summary(cwd: &str, from_id: &str, summary: &str) {
    let entry = serde_json::json!({
        "type": "branch_summary",
        "id": generate_id(),
        "parentId": from_id,
        "timestamp": timestamp_iso(),
        "fromId": from_id,
        "summary": summary,
        "fromHook": false,
    });
    append_raw_entry(cwd, &entry);
}

/// 追加一条 deletion entry（软删除标记）。
/// target_ids: 被删除的 message entry id 列表。
/// reason: 删除原因（审计用，可选）。
pub fn append_deletion(cwd: &str, target_ids: &[String], reason: Option<&str>) {
    let mut entry = serde_json::json!({
        "type": "deletion",
        "id": generate_id(),
        "parentId": null,
        "timestamp": timestamp_iso(),
        "targetIds": target_ids,
    });
    if let Some(r) = reason {
        entry["reason"] = serde_json::Value::String(r.into());
    }
    append_raw_entry(cwd, &entry);
}

/// 追加一条 segment_summary entry（软压缩/折叠标记）。
/// target_ids: 被折叠的 message entry id 列表。
/// summary: 摘要文本（LLM 生成或用户提供）。
pub fn append_segment_summary(cwd: &str, target_ids: &[String], summary: &str) {
    let entry = serde_json::json!({
        "type": "segment_summary",
        "id": generate_id(),
        "parentId": null,
        "timestamp": timestamp_iso(),
        "targetIds": target_ids,
        "summary": summary,
    });
    append_raw_entry(cwd, &entry);
}

/// 追加一条 restoration entry（恢复标记）。
/// 撤销 deletion/segment_summary：拉取层和 context 层不再过滤这些 entry。
/// 对齐 only-append 不变量：不物理删除 entry，而是追加 restoration。
pub fn append_restoration(cwd: &str, target_ids: &[String]) {
    let entry = serde_json::json!({
        "type": "restoration",
        "id": generate_id(),
        "parentId": null,
        "timestamp": timestamp_iso(),
        "targetIds": target_ids,
    });
    append_raw_entry(cwd, &entry);
}

/// 追加一条 compaction entry（压缩锚点，记录 firstKeptEntryId 供 since_compaction 视点用）。
pub fn append_compaction(
    cwd: &str,
    summary: &str,
    tokens_before: u64,
    first_kept_entry_id: Option<&str>,
    stage: Option<&str>,
    batch_count: Option<usize>,
) {
    // parentId 指向压缩前最后一个 entry（修复 check_compaction_safety 拦截失效 bug）
    // 之前 parentId=null 导致 is_descendant_of 第一步就断了 → 穿越压缩点的回滚拦不住
    let parent_id = last_entry_id(cwd);
    let mut entry = serde_json::json!({
        "type": "compaction",
        "id": generate_id(),
        "parentId": parent_id,
        "timestamp": timestamp_iso(),
        "summary": summary,
        "tokensBefore": tokens_before,
    });
    if let Some(id) = first_kept_entry_id {
        entry["firstKeptEntryId"] = serde_json::json!(id);
    }
    if let Some(s) = stage {
        entry["stage"] = serde_json::json!(s);
    }
    if let Some(bc) = batch_count {
        entry["batchCount"] = serde_json::json!(bc);
    }
    append_raw_entry(cwd, &entry);
}

/// 读取 session.jsonl 最后一个 entry 的 id（用于 compaction 的 parentId）
fn last_entry_id(cwd: &str) -> Option<String> {
    let path = session_path(cwd);
    let content = std::fs::read_to_string(&path).ok()?;
    content.lines()
        .filter(|l| !l.trim().is_empty())
        .last()
        .and_then(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .and_then(|e| e.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()))
}

/// 追加一条 turn_summary entry（每轮 turn 结束时的结构化摘要）。
pub fn append_turn_summary(
    cwd: &str,
    turn_id: u64,
    user_entry_id: &str,
    summary: &str,
    key_steps: &[String],
    tool_call_count: u32,
    tokens_input: u64,
    tokens_output: u64,
    duration_ms: u64,
    entry_range: &[String],
    status: &str,
) {
    let entry = serde_json::json!({
        "type": "turn_summary",
        "id": generate_id(),
        "parentId": null,
        "timestamp": timestamp_iso(),
        "turnId": turn_id,
        "userEntryId": user_entry_id,
        "summary": summary,
        "keySteps": key_steps,
        "toolCallCount": tool_call_count,
        "tokens": { "input": tokens_input, "output": tokens_output },
        "durationMs": duration_ms,
        "entryRange": entry_range,
        "status": status,
    });
    append_raw_entry(cwd, &entry);
}

/// 读取上一条 turn_summary 之后所有 message entry 的 id（即本轮新增的消息 entry）。
///
/// 用于 persist_turn_summary 填 entryRange。
pub fn read_last_turn_entry_range(cwd: &str) -> Option<Vec<String>> {
    let path = session_path(cwd);
    let content = std::fs::read_to_string(&path).ok()?;
    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();

    // 从后往前找最后一个 turn_summary
    let last_ts_index = lines.iter().enumerate().rev()
        .find_map(|(i, line)| {
            serde_json::from_str::<serde_json::Value>(line).ok().and_then(|val| {
                if val.get("type").and_then(|v| v.as_str()) == Some("turn_summary") {
                    Some(i)
                } else { None }
            })
        });

    let start = last_ts_index.map(|i| i + 1).unwrap_or(0);
    let mut ids = Vec::new();
    for line in &lines[start..] {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
            if val.get("type").and_then(|v| v.as_str()) == Some("message") {
                if let Some(id) = val.get("id").and_then(|v| v.as_str()) {
                    ids.push(id.to_string());
                }
            }
        }
    }
    if ids.is_empty() { None } else { Some(ids) }
}

/// 给定 entry_id，找到它所属 turn_summary 的 turnId。
///
/// 策略 1：entryRange 包含 entry_id → 直接返回
/// 策略 2：entry_id 在文件中的位置之后，第一个 turn_summary 就是它所属的 turn
pub fn find_turn_id_for_entry(cwd: &str, entry_id: &str) -> Option<String> {
    let path = session_path(cwd);
    let content = std::fs::read_to_string(&path).ok()?;
    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();

    // 策略 1：entryRange 包含
    for line in &lines {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
            if val.get("type").and_then(|v| v.as_str()) == Some("turn_summary") {
                let in_range = val.get("entryRange")
                    .and_then(|v| v.as_array())
                    .map_or(false, |arr| arr.iter().any(|a| a.as_str() == Some(entry_id)));
                if in_range {
                    return val.get("turnId").and_then(|v| v.as_str()).map(|s| s.to_string());
                }
            }
        }
    }

    // 策略 2：位置回溯
    let entry_pos = lines.iter().position(|line| {
        serde_json::from_str::<serde_json::Value>(line)
            .ok()
            .and_then(|v| {
                if v.get("type").and_then(|t| t.as_str()) == Some("message") {
                    v.get("id").and_then(|v| v.as_str()).map(|id| id == entry_id)
                } else { None }
            })
            .unwrap_or(false)
    });

    if let Some(pos) = entry_pos {
        for line in &lines[pos..] {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
                if val.get("type").and_then(|v| v.as_str()) == Some("turn_summary") {
                    return val.get("turnId").and_then(|v| v.as_str()).map(|s| s.to_string());
                }
            }
        }
    }

    None
}

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

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个临时 cwd（用 tmpdir + 子目录隔离）
    fn test_cwd(name: &str) -> String {
        let dir = std::env::temp_dir().join(format!("ion_test_jsonl_{}_{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        format!("{}", dir.display())
    }

    fn cleanup(cwd: &str) {
        let _ = std::fs::remove_dir_all(cwd);
    }

    fn write_header(cwd: &str) {
        let header = SessionHeader {
            entry_type: "session".into(),
            version: 3,
            id: "test_session".into(),
            timestamp: timestamp_iso(),
            cwd: cwd.into(),
            parent_session: None,
            agent: None,
            model: None,
            provider: None,
        };
        SessionFile::save(cwd, &header, &[]);
    }

    #[test]
    fn test_append_compaction_writes_entry() {
        let cwd = test_cwd("compaction");
        write_header(&cwd);

        append_compaction(&cwd, "压缩摘要", 32000, Some("msg_011"), Some("batched_merged"), Some(3));

        let file = SessionFile::load(&cwd).expect("file should exist");
        let compaction_entry = file.entries.iter().find(|e| {
            e.get("type").and_then(|v| v.as_str()) == Some("compaction")
        });
        assert!(compaction_entry.is_some(), "compaction entry should exist");
        let entry = compaction_entry.unwrap();
        assert_eq!(entry["summary"].as_str(), Some("压缩摘要"));
        assert_eq!(entry["tokensBefore"].as_u64(), Some(32000));
        assert_eq!(entry["firstKeptEntryId"].as_str(), Some("msg_011"));
        assert_eq!(entry["stage"].as_str(), Some("batched_merged"));
        assert_eq!(entry["batchCount"].as_u64(), Some(3));

        cleanup(&cwd);
    }

    #[test]
    fn test_append_compaction_without_optional_fields() {
        let cwd = test_cwd("compaction_min");
        write_header(&cwd);

        append_compaction(&cwd, "emergency", 50000, None, None, None);

        let file = SessionFile::load(&cwd).expect("file should exist");
        let entry = file.entries.iter().find(|e| {
            e.get("type").and_then(|v| v.as_str()) == Some("compaction")
        }).unwrap();
        assert_eq!(entry["summary"].as_str(), Some("emergency"));
        // firstKeptEntryId / stage / batchCount 应该不存在（skip_serializing_if）
        assert!(entry.get("firstKeptEntryId").is_none() || entry["firstKeptEntryId"].is_null());
        assert!(entry.get("stage").is_none() || entry["stage"].is_null());

        cleanup(&cwd);
    }

    #[test]
    fn test_append_compaction_parentid_links_to_last_entry() {
        // XL1 修复：compaction 的 parentId 应指向压缩前最后一个 entry（不是 null）
        // 这样 check_compaction_safety 才能拦住穿越压缩点的回滚
        let cwd = test_cwd("compaction_parentid");
        write_header(&cwd);

        // 先写几条普通 entry（模拟压缩前的对话）
        let msg1 = serde_json::json!({"type":"user", "id":"msg_001", "parentId":null, "message":{"role":"user","content":"hi"}});
        let msg2 = serde_json::json!({"type":"assistant", "id":"msg_002", "parentId":"msg_001", "message":{"role":"assistant","content":"hello"}});
        append_raw_entry(&cwd, &msg1);
        append_raw_entry(&cwd, &msg2);

        // 触发压缩
        append_compaction(&cwd, "压缩了 msg_001/msg_002", 5000, Some("msg_003"), None, None);

        let file = SessionFile::load(&cwd).expect("file should exist");
        let compaction = file.entries.iter().find(|e| {
            e.get("type").and_then(|v| v.as_str()) == Some("compaction")
        }).expect("compaction entry should exist");

        // parentId 应指向 msg_002（压缩前最后一个 entry），不是 null
        assert_eq!(
            compaction["parentId"].as_str(), Some("msg_002"),
            "compaction parentId 应指向压缩前最后一个 entry（修复 check_compaction_safety 拦截 bug）"
        );

        cleanup(&cwd);
    }

    #[test]
    fn test_append_turn_summary_writes_entry() {
        let cwd = test_cwd("turn_summary");
        write_header(&cwd);

        append_turn_summary(
            &cwd,
            3,
            "msg_007",
            "重构了消息拉取接口",
            &["read".into(), "edit".into(), "bash".into()],
            3,
            1200,
            800,
            5200,
            &["msg_007".into(), "msg_008".into()],
            "completed",
        );

        let file = SessionFile::load(&cwd).expect("file should exist");
        let entry = file.entries.iter().find(|e| {
            e.get("type").and_then(|v| v.as_str()) == Some("turn_summary")
        });
        assert!(entry.is_some(), "turn_summary entry should exist");
        let entry = entry.unwrap();
        assert_eq!(entry["turnId"].as_u64(), Some(3));
        assert_eq!(entry["userEntryId"].as_str(), Some("msg_007"));
        assert_eq!(entry["summary"].as_str(), Some("重构了消息拉取接口"));
        assert_eq!(entry["toolCallCount"].as_u64(), Some(3));
        assert_eq!(entry["tokens"]["input"].as_u64(), Some(1200));
        assert_eq!(entry["tokens"]["output"].as_u64(), Some(800));
        assert_eq!(entry["status"].as_str(), Some("completed"));
        let steps = entry["keySteps"].as_array().unwrap();
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[0].as_str(), Some("read"));

        cleanup(&cwd);
    }

    #[test]
    fn test_append_turn_summary_aborted_status() {
        let cwd = test_cwd("turn_abort");
        write_header(&cwd);

        append_turn_summary(&cwd, 8, "msg_015", "好的我来重构这", &[], 0, 340, 0, 100, &[], "aborted");

        let file = SessionFile::load(&cwd).expect("file should exist");
        let entry = file.entries.iter().find(|e| {
            e.get("type").and_then(|v| v.as_str()) == Some("turn_summary")
        }).unwrap();
        assert_eq!(entry["status"].as_str(), Some("aborted"));

        cleanup(&cwd);
    }

    #[test]
    fn test_compaction_entry_first_kept_entry_id_field() {
        // 验证 CompactionEntry 结构体可正确反序列化带 firstKeptEntryId 的 JSON
        let json_str = r#"{
            "type": "compaction",
            "id": "cmp_005",
            "parentId": "msg_009",
            "timestamp": "2026-07-08T10:00:00Z",
            "summary": "测试摘要",
            "tokensBefore": 32000,
            "firstKeptEntryId": "msg_042"
        }"#;
        let entry: CompactionEntry = serde_json::from_str(json_str).expect("should deserialize");
        assert_eq!(entry.summary, "测试摘要");
        assert_eq!(entry.tokens_before, 32000);
        assert_eq!(entry.first_kept_entry_id, Some("msg_042".to_string()));
    }
}
