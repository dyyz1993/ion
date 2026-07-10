//! Context Index Extension — 上下文索引与快照折叠
//!
//! 追踪 `read` 工具读进上下文的文件快照，在 `write`/`edit` 覆盖后
//! 自动把旧快照折叠成占位符，减少 LLM context 的 token 浪费。
//!
//! V1: 只追 `read`（100% 精确），grep/bash/find 标注为未索引。
//! 设计文档: docs/design/CONTEXT_INDEX.md

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use super::error::AgentResult;
use super::extension::{Extension, TurnContext};
use super::messages::{Message, ToolCall};
use ion_provider::types::{ToolResult, ToolResultMessage, ContentBlock, TextContent};

// ---------------------------------------------------------------------------
// 数据结构
// ---------------------------------------------------------------------------

/// 一条 read 记录
#[derive(Clone, Debug)]
pub struct ReadRecord {
    /// 哪一轮 turn 读的
    pub turn: u32,
    /// 读取时的内容 hash（djb2）
    pub content_hash: u64,
    /// 关联的 tool_call_id（用于在 messages 里定位）
    pub tool_call_id: String,
    /// 新鲜度
    pub status: Freshness,
}

/// 一条 write/edit 记录
#[derive(Clone, Debug)]
pub struct WriteRecord {
    pub turn: u32,
    pub kind: WriteKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Freshness {
    Current,
    Stale { overwritten_by_turn: u32, kind: WriteKind },
}

#[derive(Clone, Debug, PartialEq)]
pub enum WriteKind {
    Write,
    Edit,
}

/// 单个文件的记录
#[derive(Clone, Debug, Default)]
pub struct FileRecord {
    pub reads: Vec<ReadRecord>,
    pub writes: Vec<WriteRecord>,
}

/// Context Index 核心状态
#[derive(Debug, Default)]
pub struct ContextIndex {
    /// 文件路径 → 记录
    pub files: HashMap<String, FileRecord>,
    /// 诚实的未索引来源
    pub untracked_sources: Vec<String>,
    /// 当前 turn（由 on_turn_start 更新）
    pub current_turn: u32,
}

impl ContextIndex {
    pub fn new() -> Self {
        Self {
            files: HashMap::new(),
            untracked_sources: vec!["grep".into(), "bash".into(), "find".into()],
            current_turn: 0,
        }
    }

    /// 记录一次 read 操作
    pub fn record_read(&mut self, path: &str, tool_call_id: &str, content: &str) {
        let hash = djb2(content);
        let record = self.files.entry(path.to_string()).or_default();
        record.reads.push(ReadRecord {
            turn: self.current_turn,
            content_hash: hash,
            tool_call_id: tool_call_id.to_string(),
            status: Freshness::Current,
        });
    }

    /// 记录一次 write/edit 操作，标记旧 read 为 Stale
    pub fn record_write(&mut self, path: &str, kind: WriteKind) {
        let record = self.files.entry(path.to_string()).or_default();
        // 标记该文件的所有 Current read 为 Stale
        for read in &mut record.reads {
            if read.status == Freshness::Current {
                read.status = Freshness::Stale {
                    overwritten_by_turn: self.current_turn,
                    kind: kind.clone(),
                };
            }
        }
        record.writes.push(WriteRecord {
            turn: self.current_turn,
            kind,
        });
    }

    /// 构建 tree 视图（注入 system prompt 用）
    pub fn build_tree(&self) -> String {
        if self.files.is_empty() {
            return "(no files indexed)".into();
        }
        let mut lines = Vec::new();
        // 按路径排序
        let mut paths: Vec<&String> = self.files.keys().collect();
        paths.sort();

        for path in paths {
            let record = &self.files[path];
            // 找最新的 read 状态
            let latest_read = record.reads.last();
            let status_str = match latest_read {
                Some(r) => match &r.status {
                    Freshness::Current => {
                        format!("current · turn {}", r.turn)
                    }
                    Freshness::Stale { overwritten_by_turn, kind } => {
                        format!("STALE · turn {}, overwritten by turn {} ({:?})", r.turn, overwritten_by_turn, kind)
                    }
                },
                None => "no reads".to_string(),
            };
            lines.push(format!("  {} [{}]", path, status_str));
        }
        if !self.untracked_sources.is_empty() {
            lines.push(format!(
                "\n注: {} 读取的内容不在索引内",
                self.untracked_sources.join("/")
            ));
        }
        lines.join("\n")
    }

    /// 找出所有 Stale 的 read 对应的 tool_call_id（on_context 折叠用）
    pub fn stale_tool_call_ids(&self) -> Vec<(String, String)> {
        // Vec<(tool_call_id, placeholder_text)>
        let mut result = Vec::new();
        for (path, record) in &self.files {
            for read in &record.reads {
                if let Freshness::Stale { overwritten_by_turn, kind } = &read.status {
                    let placeholder = format!(
                        "[ContextIndex: {} — read at turn {}, overwritten by turn {} ({:?})]\n\
                         [Re-read {} for latest content]",
                        path, read.turn, overwritten_by_turn, kind, path
                    );
                    result.push((read.tool_call_id.clone(), placeholder));
                }
            }
        }
        result
    }
}

/// djb2 hash（复用 Memory 扩展的算法）
fn djb2(s: &str) -> u64 {
    let mut hash: u64 = 5381;
    for b in s.bytes() {
        hash = hash.wrapping_mul(33).wrapping_add(b as u64);
    }
    hash
}

// ---------------------------------------------------------------------------
// Extension
// ---------------------------------------------------------------------------

pub struct ContextIndexExtension {
    pub index: Arc<Mutex<ContextIndex>>,
    name: String,
}

impl ContextIndexExtension {
    pub fn new() -> Self {
        Self {
            index: Arc::new(Mutex::new(ContextIndex::new())),
            name: "context-index".into(),
        }
    }

    pub fn new_with_index(index: Arc<Mutex<ContextIndex>>) -> Self {
        Self {
            index,
            name: "context-index".into(),
        }
    }
}

#[async_trait::async_trait]
impl Extension for ContextIndexExtension {
    fn name(&self) -> &str {
        &self.name
    }

    async fn on_turn_start(&self, ctx: &mut TurnContext) -> AgentResult<()> {
        let mut idx = self.index.lock().await;
        idx.current_turn = ctx.turn_index as u32;
        Ok(())
    }

    async fn after_tool_call(&self, call: &ToolCall, result: &ToolResult) -> AgentResult<()> {
        let mut idx = self.index.lock().await;

        match call.name.as_str() {
            "read" => {
                if let Some(path) = call.arguments.get("file_path").and_then(|v| v.as_str()) {
                    idx.record_read(path, &call.id, &result.output);
                }
            }
            "write" => {
                if let Some(path) = call.arguments.get("file_path").and_then(|v| v.as_str()) {
                    idx.record_write(path, WriteKind::Write);
                }
            }
            "edit" => {
                if let Some(path) = call.arguments.get("file_path").and_then(|v| v.as_str()) {
                    idx.record_write(path, WriteKind::Edit);
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn on_context(&self, messages: &mut Vec<Message>) -> AgentResult<()> {
        let idx = self.index.lock().await;
        let stale_ids = idx.stale_tool_call_ids();

        if stale_ids.is_empty() {
            return Ok(());
        }

        // 构建 tool_call_id → placeholder 映射
        let mut fold_map: HashMap<&str, &str> = HashMap::new();
        for (tcid, placeholder) in &stale_ids {
            fold_map.insert(tcid.as_str(), placeholder.as_str());
        }

        // 遍历 messages，折叠 Stale 的 ToolResult
        for msg in messages.iter_mut() {
            if let Message::ToolResult(tr) = msg {
                if let Some(placeholder) = fold_map.get(tr.tool_call_id.as_str()) {
                    tr.content = vec![ContentBlock::Text(TextContent {
                        text: placeholder.to_string(),
                        text_signature: None,
                    })];
                }
            }
        }
        Ok(())
    }

    async fn on_system_prompt(&self, prompt: &mut String) -> AgentResult<()> {
        let idx = self.index.lock().await;
        if idx.files.is_empty() {
            return Ok(());
        }
        let tree = idx.build_tree();
        prompt.push_str(&format!("\n<context_index>\n{}\n</context_index>\n", tree));
        Ok(())
    }

    async fn on_extension_rpc(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> AgentResult<serde_json::Value> {
        let idx = self.index.lock().await;
        match method {
            "tree" | "list" => {
                let mut files = Vec::new();
                let mut paths: Vec<&String> = idx.files.keys().collect();
                paths.sort();
                for path in paths {
                    let record = &idx.files[path];
                    let latest = record.reads.last();
                    let (status, turn) = match latest {
                        Some(r) => match &r.status {
                            Freshness::Current => ("current".to_string(), r.turn),
                            Freshness::Stale { overwritten_by_turn, .. } => {
                                ("stale".to_string(), *overwritten_by_turn)
                            }
                        },
                        None => ("none".to_string(), 0),
                    };
                    files.push(serde_json::json!({
                        "path": path,
                        "status": status,
                        "turn": turn,
                        "readCount": record.reads.len(),
                        "writeCount": record.writes.len(),
                    }));
                }
                Ok(serde_json::json!({
                    "files": files,
                    "untracked": idx.untracked_sources,
                }))
            }
            "ranges" => {
                let path = params
                    .get("path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let record = idx.files.get(path);
                let reads = record
                    .map(|r| {
                        r.reads
                            .iter()
                            .map(|read| {
                                let (status, detail) = match &read.status {
                                    Freshness::Current => ("current".to_string(), serde_json::Value::Null),
                                    Freshness::Stale { overwritten_by_turn, kind } => (
                                        "stale".to_string(),
                                        serde_json::json!({
                                            "overwrittenByTurn": overwritten_by_turn,
                                            "kind": format!("{:?}", kind),
                                        }),
                                    ),
                                };
                                serde_json::json!({
                                    "turn": read.turn,
                                    "status": status,
                                    "detail": detail,
                                })
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                Ok(serde_json::json!({ "path": path, "reads": reads }))
            }
            _ => Ok(serde_json::json!({"error": format!("unknown method: {}", method)})),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_read_creates_entry() {
        let mut idx = ContextIndex::new();
        idx.record_read("src/foo.rs", "tc_001", "fn main() {}");
        assert!(idx.files.contains_key("src/foo.rs"));
        assert_eq!(idx.files["src/foo.rs"].reads.len(), 1);
        assert_eq!(idx.files["src/foo.rs"].reads[0].status, Freshness::Current);
    }

    #[test]
    fn write_marks_reads_stale() {
        let mut idx = ContextIndex::new();
        idx.current_turn = 3;
        idx.record_read("src/foo.rs", "tc_001", "old content");
        idx.current_turn = 5;
        idx.record_write("src/foo.rs", WriteKind::Write);

        let reads = &idx.files["src/foo.rs"].reads;
        assert_eq!(reads[0].status, Freshness::Stale {
            overwritten_by_turn: 5,
            kind: WriteKind::Write,
        });
    }

    #[test]
    fn edit_marks_reads_stale() {
        let mut idx = ContextIndex::new();
        idx.current_turn = 2;
        idx.record_read("src/bar.rs", "tc_002", "content");
        idx.current_turn = 4;
        idx.record_write("src/bar.rs", WriteKind::Edit);

        assert_eq!(idx.files["src/bar.rs"].reads[0].status, Freshness::Stale {
            overwritten_by_turn: 4,
            kind: WriteKind::Edit,
        });
    }

    #[test]
    fn stale_tool_call_ids_returns_correct_pairs() {
        let mut idx = ContextIndex::new();
        idx.current_turn = 1;
        idx.record_read("a.rs", "tc_a", "content a");
        idx.record_read("b.rs", "tc_b", "content b");
        idx.current_turn = 3;
        idx.record_write("a.rs", WriteKind::Write); // a.rs stale, b.rs still current

        let stale = idx.stale_tool_call_ids();
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].0, "tc_a");
        assert!(stale[0].1.contains("a.rs"));
        assert!(stale[0].1.contains("Re-read"));
    }

    #[test]
    fn build_tree_shows_status() {
        let mut idx = ContextIndex::new();
        idx.current_turn = 1;
        idx.record_read("src/main.rs", "tc_001", "fn main(){}");
        idx.current_turn = 3;
        idx.record_write("src/main.rs", WriteKind::Write);

        let tree = idx.build_tree();
        assert!(tree.contains("src/main.rs"));
        assert!(tree.contains("STALE"));
        assert!(tree.contains("grep/bash/find")); // untracked sources
    }

    #[test]
    fn build_tree_empty() {
        let idx = ContextIndex::new();
        let tree = idx.build_tree();
        assert_eq!(tree, "(no files indexed)");
    }

    #[test]
    fn multiple_reads_independent_status() {
        let mut idx = ContextIndex::new();
        idx.current_turn = 1;
        idx.record_read("x.rs", "tc_1", "v1");
        idx.current_turn = 2;
        idx.record_write("x.rs", WriteKind::Write); // tc_1 stale
        idx.current_turn = 3;
        idx.record_read("x.rs", "tc_2", "v2"); // new read, current

        let reads = &idx.files["x.rs"].reads;
        assert_eq!(reads.len(), 2);
        assert_eq!(reads[0].status, Freshness::Stale {
            overwritten_by_turn: 2,
            kind: WriteKind::Write,
        });
        assert_eq!(reads[1].status, Freshness::Current);
    }

    #[test]
    fn djb2_consistent() {
        assert_eq!(djb2("hello"), djb2("hello"));
        assert_ne!(djb2("hello"), djb2("world"));
    }

    #[tokio::test]
    async fn extension_after_tool_call_records_read() {
        let ext = ContextIndexExtension::new();
        let call = ToolCall {
            call_type: "function".into(),
            id: "tc_test".into(),
            name: "read".into(),
            arguments: serde_json::json!({"file_path": "src/test.rs"}),
            thought_signature: None,
        };
        let result = ToolResult {
            tool_call_id: "tc_test".into(),
            output: "fn test() {}".into(),
        };
        ext.after_tool_call(&call, &result).await.unwrap();

        let idx = ext.index.lock().await;
        assert!(idx.files.contains_key("src/test.rs"));
    }

    #[tokio::test]
    async fn extension_after_tool_call_write_marks_stale() {
        let ext = ContextIndexExtension::new();

        let read_call = ToolCall {
            call_type: "function".into(),
            id: "tc_read".into(),
            name: "read".into(),
            arguments: serde_json::json!({"file_path": "foo.rs"}),
            thought_signature: None,
        };
        let read_result = ToolResult {
            tool_call_id: "tc_read".into(),
            output: "old".into(),
        };
        ext.after_tool_call(&read_call, &read_result).await.unwrap();

        let write_call = ToolCall {
            call_type: "function".into(),
            id: "tc_write".into(),
            name: "write".into(),
            arguments: serde_json::json!({"file_path": "foo.rs"}),
            thought_signature: None,
        };
        let write_result = ToolResult {
            tool_call_id: "tc_write".into(),
            output: "wrote".into(),
        };
        ext.after_tool_call(&write_call, &write_result).await.unwrap();

        let idx = ext.index.lock().await;
        assert_eq!(idx.files["foo.rs"].reads[0].status, Freshness::Stale {
            overwritten_by_turn: 0,
            kind: WriteKind::Write,
        });
    }

    #[tokio::test]
    async fn extension_on_context_folds_stale() {
        let ext = ContextIndexExtension::new();

        let read_call = ToolCall {
            call_type: "function".into(),
            id: "tc_fold".into(),
            name: "read".into(),
            arguments: serde_json::json!({"file_path": "fold.rs"}),
            thought_signature: None,
        };
        ext.after_tool_call(&read_call, &ToolResult {
            tool_call_id: "tc_fold".into(),
            output: "original content".into(),
        }).await.unwrap();

        let write_call = ToolCall {
            call_type: "function".into(),
            id: "tc_w".into(),
            name: "write".into(),
            arguments: serde_json::json!({"file_path": "fold.rs"}),
            thought_signature: None,
        };
        ext.after_tool_call(&write_call, &ToolResult {
            tool_call_id: "tc_w".into(),
            output: "wrote".into(),
        }).await.unwrap();

        let mut messages = vec![Message::ToolResult(ToolResultMessage {
            role: "toolResult".into(),
            tool_call_id: "tc_fold".into(),
            tool_name: "read".into(),
            content: vec![ContentBlock::Text(TextContent {
                text: "original content".into(),
                text_signature: None,
            })],
            details: None,
            is_error: false,
            timestamp: 0,
        })];

        ext.on_context(&mut messages).await.unwrap();

        if let Message::ToolResult(tr) = &messages[0] {
            let text = match &tr.content[0] {
                ContentBlock::Text(t) => &t.text,
                _ => panic!("expected text"),
            };
            assert!(text.contains("[ContextIndex"), "should be folded, got: {}", text);
            assert!(text.contains("fold.rs"));
            assert!(!text.contains("original content"));
        } else {
            panic!("expected ToolResult");
        }
    }
}
