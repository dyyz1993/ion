//! File Index — 会话 JSONL 的内存稀疏偏移索引（B 线：长会话渲染）
//!
//! 问题：host 直读路径此前对每次 RPC 整文件 read_to_string + 逐行全量
//! serde 解析（400MB → 1-2GB 峰值堆内存，秒级~十几秒延迟）。
//!
//! 方案：一次扫描建立**每行偏移索引**，只提取小字段（type/id/parentId/
//! timestamp/role/220 字符预览/审批类 targetIds）；分页查询在索引上完成
//! 过滤与分页，仅对返回页的行做 read_at + 全量解析。
//! 借助 append-only 不变量：偏移永不变 → mtime 变化只需从上次末尾增量扫描。
//!
//! 存储落位：纯内存缓存（不建 sidecar 文件，符合 AGENTS.md 存储落位原则）。
//!
//! metas：与 heads 平行的小 JSON 值（不含 content），形状兼容
//! session_tree / apply_visibility_filter / group_into_turns 的字段读取
//! （type/id/parentId/timestamp/message.role/targetIds/summary），
//! 使现有纯函数无需改动即可在索引层运行。

use serde::Deserialize;
use serde_json::value::RawValue;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// 预览截断长度（list_turns 的 overview 截 200，留余量）
const HEAD_LEN: usize = 220;

// ── 扫描期临时结构（借用行缓冲，提取后丢弃） ──────────────────────────

#[derive(Deserialize)]
struct RawHead<'a> {
    #[serde(rename = "type", borrow)]
    etype: Option<&'a str>,
    #[serde(borrow)]
    id: Option<&'a str>,
    #[serde(rename = "parentId", borrow)]
    parent_id: Option<&'a str>,
    #[serde(borrow)]
    timestamp: Option<&'a str>,
    #[serde(rename = "customType", borrow)]
    custom_type: Option<&'a str>,
    #[serde(borrow)]
    message: Option<&'a RawValue>,
    /// deletion / segment_summary / restoration 的 targetIds（小数组）
    #[serde(rename = "targetIds")]
    target_ids: Option<&'a RawValue>,
    #[serde(borrow)]
    summary: Option<&'a str>,
    /// leaf_pointer 的 leafId（Live 视点解析依赖；丢弃会导致 leaf 过滤失效走全量）
    #[serde(rename = "leafId", borrow)]
    leaf_id: Option<&'a str>,
    /// label 的 targetId + label（named_branches / make_checkout 依赖）
    #[serde(rename = "targetId", borrow)]
    target_id: Option<&'a str>,
    #[serde(borrow)]
    label: Option<&'a str>,
}

fn head220(s: &str) -> String {
    if s.chars().count() <= HEAD_LEN {
        s.to_string()
    } else {
        s.chars().take(HEAD_LEN).collect()
    }
}

/// message.content 的首个 Text 块文本预览（content 可能是 string 或块数组）
fn content_preview(raw: &RawValue) -> Option<String> {
    let s = raw.get();
    // string 形态
    if let Ok(text) = serde_json::from_str::<String>(s) {
        return Some(head220(&text));
    }
    // 块数组形态：只解析首个带 Text 的块（块本身小；用 RawValue 借用零拷贝遍历）
    let trimmed = s.trim_start();
    if trimmed.starts_with('[') {
        if let Ok(blocks) = serde_json::from_str::<Vec<Box<RawValue>>>(s) {
            for b in &blocks {
                if let Ok(w) = serde_json::from_str::<TextWrapper>(b.get()) {
                    if let Some(inner) = w.text {
                        if let Some(text) = inner.text {
                            return Some(head220(&text));
                        }
                    }
                }
            }
        }
    }
    None
}

/// 块形态：{"Text":{"text":"..."}} / {"ToolCall":{...}} 等——只关心 Text
#[derive(Deserialize)]
struct TextWrapper {
    #[serde(rename = "Text")]
    text: Option<TextInner>,
}
#[derive(Deserialize)]
struct TextInner {
    text: Option<String>,
}

/// message 对象的形态：{"User":{...}} / {"Assistant":{...}} / {"ToolResult":{...}}
/// 变体名即 role；只对 User/Assistant 提取 content 预览。
fn message_head(raw: &RawValue) -> (Option<&'static str>, Option<String>, Option<String>) {
    let s = raw.get();
    // 快速判定变体（避免整对象反序列化）："User"/"Assistant"/"ToolResult" 之一为首个 key
    let variant = if s.contains("\"User\"") {
        "user"
    } else if s.contains("\"Assistant\"") {
        "assistant"
    } else if s.contains("\"ToolResult\"") {
        "toolResult"
    } else {
        return (None, None, None);
    };
    // 解出 content（RawValue 借用）
    #[derive(Deserialize)]
    struct MsgInner<'a> {
        #[serde(borrow)]
        content: Option<&'a RawValue>,
    }
    #[derive(Deserialize)]
    struct MsgEnum<'a> {
        #[serde(borrow)]
        #[serde(rename = "User")]
        user: Option<MsgInner<'a>>,
        #[serde(borrow)]
        #[serde(rename = "Assistant")]
        assistant: Option<MsgInner<'a>>,
        #[serde(borrow)]
        #[serde(rename = "ToolResult")]
        tool_result: Option<MsgInner<'a>>,
    }
    let Ok(parsed) = serde_json::from_str::<MsgEnum>(s) else {
        return (Some(variant), None, None);
    };
    let preview = match (&parsed.user, &parsed.assistant) {
        (Some(inner), _) => inner.content.and_then(content_preview),
        (_, Some(inner)) => inner.content.and_then(content_preview),
        _ => None,
    };
    (Some(variant), preview.clone(), preview)
}

// ── 索引结构 ──────────────────────────────────────────────────────────

/// 单行条目的头信息（不含 content）
#[derive(Clone, Debug)]
pub struct EntryHead {
    /// 行起始偏移（不含换行符的行内容区 [offset, offset+len)）
    pub offset: u64,
    pub len: u64,
    pub etype: String,
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp: Option<String>,
    /// user / assistant / toolResult（message 条目）
    pub role: Option<&'static str>,
    /// User content 首块预览（≤220 字符）
    pub user_head: Option<String>,
    /// Assistant content 首块预览（≤220 字符）
    pub asst_head: Option<String>,
    pub custom_type: Option<String>,
    /// deletion/segment_summary/restoration 的 targetIds
    pub target_ids: Vec<String>,
    pub summary: Option<String>,
}

/// 会话文件的偏移索引 + 小字段 metas
pub struct FileIndex {
    pub path: PathBuf,
    pub file_len: u64,
    pub mtime: Option<SystemTime>,
    pub heads: Vec<EntryHead>,
    /// 与 heads 平行的小 JSON 值（无 content），兼容现有纯函数的字段读取
    pub metas: Vec<serde_json::Value>,
    pub id_to_idx: HashMap<String, usize>,
    /// 扫描期遇到的坏行数（截断/半行）
    pub bad_lines: usize,
    /// 预计算：live 视点过滤 + visibility 过滤后的 message 序列（索引到 heads 的下标）。
    /// 构建时一次算完 → 翻页 O(slice)；分支/compaction 视图仍走全量管线（罕见路径）
    pub live_message_idxs: Vec<usize>,
    /// live 视点的 totalCount（= live_message_idxs.len()）
    pub live_total: usize,
}

impl FileIndex {
    /// 全量扫描建索引
    pub fn build(path: &Path) -> std::io::Result<Self> {
        let file = std::fs::File::open(path)?;
        let meta = file.metadata()?;
        let mut idx = Self {
            path: path.to_path_buf(),
            file_len: meta.len(),
            mtime: meta.modified().ok(),
            heads: Vec::new(),
            metas: Vec::new(),
            id_to_idx: HashMap::new(),
            bad_lines: 0,
            live_message_idxs: Vec::new(),
            live_total: 0,
        };
        let mut reader = BufReader::new(file);
        idx.scan_from(&mut reader, 0)?;
        idx.precompute_live_view();
        Ok(idx)
    }

    /// 预计算 live 视点的过滤后消息序列（一次，构建/refresh 后调用）
    fn precompute_live_view(&mut self) {
        let metas = std::mem::take(&mut self.metas);
        // view=Live（解析 leaf → root→leaf 路径）
        let view_filtered = crate::message_retrieval::apply_view_filter(
            &metas,
            &crate::message_retrieval::View::Live,
        );
        // visibility（deletion/segment/restoration）
        let visible = crate::message_retrieval::apply_visibility_filter(&view_filtered);
        // 只留 message 类型
        self.live_message_idxs = visible
            .iter()
            .filter(|e| {
                let t = e.get("type").and_then(|v| v.as_str()).unwrap_or("");
                t == "message"
            })
            .filter_map(|e| {
                let id = e.get("id").and_then(|v| v.as_str()).unwrap_or("");
                self.id_to_idx.get(id).copied()
            })
            .collect();
        self.live_total = self.live_message_idxs.len();
        self.metas = metas; // 归还
    }

    /// 增量刷新：文件 append 后只扫新增区。返回 false = 需全量重建
    /// （文件被截短/重写，append-only 假设破坏）。
    pub fn refresh(&mut self) -> bool {
        let Ok(meta) = std::fs::metadata(&self.path) else {
            return false;
        };
        let new_len = meta.len();
        if new_len < self.file_len {
            return false; // 截短 → 重写，放弃增量
        }
        if new_len == self.file_len && meta.modified().ok() == self.mtime {
            return true; // 无变化
        }
        let Ok(mut file) = std::fs::File::open(&self.path) else {
            return false;
        };
        if file.seek(SeekFrom::Start(self.file_len)).is_err() {
            return false;
        }
        let mut reader = BufReader::new(file);
        if self.scan_from(&mut reader, self.file_len).is_err() {
            return false;
        }
        self.file_len = new_len;
        self.mtime = meta.modified().ok();
        // ⚠️ 必须重算 live 视点：append 可能带来 leaf_pointer（fork/checkout 落盘），
        // 不重算则 live_message_idxs 停留在旧的全量序列 → fast path 绕过 leaf 过滤
        // （实测 2026-08-25：checkout 后 get_messages 仍返回主干全量）。纯内存 O(n)，可接受。
        self.precompute_live_view();
        true
    }

    /// 从 reader 当前位置扫描到 EOF（start = 起始偏移，用于 offset 记账）
    fn scan_from(
        &mut self,
        reader: &mut BufReader<std::fs::File>,
        start: u64,
    ) -> std::io::Result<()> {
        let mut line = String::new();
        let mut offset = start;
        loop {
            line.clear();
            let n = reader.read_line(&mut line)?;
            if n == 0 {
                break;
            }
            let trimmed = line.trim_end_matches(['\n', '\r']);
            let line_len = trimmed.len() as u64;
            if line_len > 0 {
                self.parse_line(trimmed, offset, line_len);
            }
            offset += n as u64;
        }
        self.file_len = offset.max(self.file_len);
        Ok(())
    }

    fn parse_line(&mut self, line: &str, offset: u64, len: u64) {
        let Ok(raw) = serde_json::from_str::<RawHead>(line) else {
            self.bad_lines += 1;
            return;
        };
        let (role, user_head, asst_head) = match raw.message {
            Some(m) => {
                let (r, uh, ah) = message_head(m);
                (r, uh, ah)
            }
            None => (None, None, None),
        };
        let target_ids: Vec<String> = raw
            .target_ids
            .and_then(|t| serde_json::from_str::<Vec<String>>(t.get()).ok())
            .unwrap_or_default();
        let head = EntryHead {
            offset,
            len,
            etype: raw.etype.unwrap_or("unknown").to_string(),
            id: raw.id.unwrap_or("").to_string(),
            parent_id: raw.parent_id.map(str::to_string),
            timestamp: raw.timestamp.map(str::to_string),
            role,
            user_head,
            asst_head,
            custom_type: raw.custom_type.map(str::to_string),
            target_ids: target_ids.clone(),
            summary: raw.summary.map(str::to_string),
        };
        // metas：兼容 session_tree / visibility / group_into_turns 的字段形状
        let mut meta = serde_json::json!({
            "type": head.etype,
            "id": head.id,
        });
        if let Some(p) = &head.parent_id {
            meta["parentId"] = serde_json::json!(p);
        }
        if let Some(ts) = &head.timestamp {
            meta["timestamp"] = serde_json::json!(ts);
        }
        if let Some(r) = head.role {
            meta["message"] = serde_json::json!({ "role": r });
            // message_role 兼容：变体 key 形态
            let variant = match r {
                "user" => "User",
                "assistant" => "Assistant",
                _ => "ToolResult",
            };
            meta["message"][variant] = serde_json::json!({});
        }
        if !head.target_ids.is_empty() {
            meta["targetIds"] = serde_json::json!(head.target_ids);
        }
        if let Some(s) = &head.summary {
            meta["summary"] = serde_json::json!(s);
        }
        // 会话树字段透传：leafId / targetId / label（apply_view_filter 的 Live
        // 视点与 named_branches 都在 metas 上工作，缺字段 = leaf 过滤失效）
        if let Some(l) = &raw.leaf_id {
            meta["leafId"] = serde_json::json!(l);
        }
        if let Some(t) = &raw.target_id {
            meta["targetId"] = serde_json::json!(t);
        }
        if let Some(l) = &raw.label {
            meta["label"] = serde_json::json!(l);
        }
        if !head.id.is_empty() {
            self.id_to_idx.insert(head.id.clone(), self.heads.len());
        }
        self.heads.push(head);
        self.metas.push(meta);
    }

    /// 按索引解析单行（read_at 精确读取，不整文件加载）
    pub fn parse_entry(&self, i: usize) -> Option<serde_json::Value> {
        use std::os::unix::fs::FileExt;
        let head = self.heads.get(i)?;
        let mut buf = vec![0u8; head.len as usize];
        let file = std::fs::File::open(&self.path).ok()?;
        file.read_exact_at(&mut buf, head.offset).ok()?;
        let line = std::str::from_utf8(&buf).ok()?;
        serde_json::from_str(line).ok()
    }

    pub fn total_lines(&self) -> usize {
        self.heads.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_tmp(lines: &[String]) -> (std::path::PathBuf, std::path::PathBuf) {
        write_tmp_named(lines, "s.jsonl")
    }
    fn write_tmp_named(lines: &[String], name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("ion-fidx-{:?}-{}", std::process::id(), name));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, lines.join("\n") + "\n").unwrap();
        (dir, p)
    }

    #[test]
    fn test_build_heads_and_offsets() {
        let l1 = r#"{"type":"session","id":"s1","cwd":"/t"}"#.to_string();
        let l2 = r#"{"type":"message","id":"u1","parentId":"s1","timestamp":"t1","message":{"User":{"content":"你好世界","role":"user"}}}"#.to_string();
        let l3 = r#"{"type":"message","id":"a1","parentId":"u1","message":{"Assistant":{"content":[{"Text":{"text":"回答内容"}}],"role":"assistant"}}}"#.to_string();
        let l4 = r#"{"type":"deletion","id":"d1","targetIds":["a1"]}"#.to_string();
        let (_dir, p) = write_tmp_named(&[l1, l2, l3, l4], "t1.jsonl");
        let idx = FileIndex::build(&p).unwrap();
        assert_eq!(idx.total_lines(), 4);
        assert_eq!(idx.heads[0].etype, "session");
        // 偏移正确性：按 offset+len 读回并解析 == 原行
        for i in 0..4 {
            let v = idx.parse_entry(i).unwrap();
            assert_eq!(v["id"], idx.metas[i]["id"]);
        }
        // role 与预览
        assert_eq!(idx.heads[1].role, Some("user"));
        assert_eq!(idx.heads[1].user_head.as_deref(), Some("你好世界"));
        assert_eq!(idx.heads[2].role, Some("assistant"));
        assert_eq!(idx.heads[2].asst_head.as_deref(), Some("回答内容"));
        // deletion targetIds 进 meta（visibility 兼容）
        assert_eq!(
            idx.metas[3]["targetIds"],
            serde_json::json!(["a1".to_string()])
        );
        // id 索引
        assert_eq!(idx.id_to_idx.get("a1"), Some(&2));
    }

    #[test]
    fn test_incremental_append() {
        let l1 = r#"{"type":"session","id":"s1"}"#.to_string();
        let (_dir, p) = write_tmp_named(&[l1.clone()], "t2.jsonl");
        let mut idx = FileIndex::build(&p).unwrap();
        assert_eq!(idx.total_lines(), 1);
        // append 两行
        let l2 = r#"{"type":"message","id":"u1","message":{"User":{"content":"第二个","role":"user"}}}"#;
        let l3 = r#"{"type":"message","id":"u2","message":{"User":{"content":"第三个","role":"user"}}}"#;
        std::fs::write(&p, format!("{l1}\n{l2}\n{l3}\n")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert!(idx.refresh());
        assert_eq!(idx.total_lines(), 3);
        assert_eq!(idx.heads[2].id, "u2");
        assert_eq!(idx.heads[2].user_head.as_deref(), Some("第三个"));
    }

    #[test]
    fn test_truncate_forces_rebuild_signal() {
        let l1 = r#"{"type":"session","id":"s1"}"#.to_string();
        let l2 = r#"{"type":"message","id":"u1","message":{"User":{"content":"x","role":"user"}}}"#;
        let (_dir, p) = write_tmp_named(&[l1.to_string(), l2.to_string()], "t3.jsonl");
        let mut idx = FileIndex::build(&p).unwrap();
        std::fs::write(&p, "{\"type\":\"session\",\"id\":\"s9\"}\n").unwrap();
        assert!(!idx.refresh(), "截短必须要求全量重建");
    }
}
