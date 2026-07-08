//! Global Memory — 跨项目记忆库（SQLite + FTS5）。
//!
//! 机器级唯一数据库 ~/.ion/agent/global-memory.db。
//! 被 Memory Agent（V0.2 单例扩展）使用，所有项目共享。
//!
//! 设计文档：docs/design/MEMORY_AGENT.md

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// 全局记忆条目
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GlobalMemoryEntry {
    pub id: String,
    pub project: String,
    pub content: String,
    pub category: String,
    pub tags: String,
    pub importance: i32,
    pub archived: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

/// 全局记忆库（线程安全，Arc<Mutex<Connection>>）
pub struct GlobalMemoryStore {
    conn: Arc<Mutex<Connection>>,
}

impl GlobalMemoryStore {
    /// 打开或创建全局记忆库。
    /// path 通常是 ~/.ion/agent/global-memory.db
    pub fn open(path: &PathBuf) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("create dir: {}", e))?;
        }
        let conn = Connection::open(path).map_err(|e| format!("open db: {}", e))?;
        // 性能优化
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")
            .map_err(|e| format!("pragma: {}", e))?;
        // Schema
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS entries (
                id TEXT PRIMARY KEY,
                project TEXT NOT NULL,
                content TEXT NOT NULL,
                category TEXT DEFAULT '',
                tags TEXT DEFAULT '',
                importance INTEGER DEFAULT 5,
                archived INTEGER DEFAULT 0,
                created_at INTEGER DEFAULT (unixepoch()),
                updated_at INTEGER DEFAULT (unixepoch())
            );
            CREATE VIRTUAL TABLE IF NOT EXISTS entries_fts USING fts5(
                content, category, tags,
                content=entries, content_rowid=rowid
            );
            CREATE TABLE IF NOT EXISTS outlines (
                id TEXT PRIMARY KEY,
                summary TEXT,
                project TEXT,
                entry_count INTEGER DEFAULT 0,
                updated_at INTEGER DEFAULT (unixepoch())
            );",
        )
        .map_err(|e| format!("init schema: {}", e))?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// 保存一条记忆。返回生成的 ID。
    pub fn save(
        &self,
        content: &str,
        category: &str,
        tags: &str,
        project: &str,
        importance: i32,
    ) -> Result<String, String> {
        let id = format!("gmem_{}", uuid_str());
        let conn = self.conn.lock().map_err(|e| format!("lock: {}", e))?;
        conn.execute(
            "INSERT INTO entries (id, project, content, category, tags, importance) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, project, content, category, tags, importance],
        )
        .map_err(|e| format!("insert: {}", e))?;
        // FTS5 索引（手动同步，因为用了 external content table）
        conn.execute(
            "INSERT INTO entries_fts (rowid, content, category, tags) VALUES (
                (SELECT rowid FROM entries WHERE id = ?1), ?2, ?3, ?4)",
            params![id, content, category, tags],
        )
        .map_err(|e| format!("fts insert: {}", e))?;
        Ok(id)
    }

    /// FTS5 全文搜索。
    pub fn search(&self, query: &str, project: Option<&str>) -> Result<Vec<GlobalMemoryEntry>, String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {}", e))?;
        let sql = if project.is_some() {
            "SELECT e.id, e.project, e.content, e.category, e.tags, e.importance, e.archived, e.created_at, e.updated_at
             FROM entries e
             JOIN entries_fts f ON e.rowid = f.rowid
             WHERE entries_fts MATCH ?1 AND e.archived = 0 AND e.project = ?2
             ORDER BY e.importance DESC, e.updated_at DESC"
        } else {
            "SELECT e.id, e.project, e.content, e.category, e.tags, e.importance, e.archived, e.created_at, e.updated_at
             FROM entries e
             JOIN entries_fts f ON e.rowid = f.rowid
             WHERE entries_fts MATCH ?1 AND e.archived = 0
             ORDER BY e.importance DESC, e.updated_at DESC"
        };
        let mut stmt = conn.prepare(sql).map_err(|e| format!("prepare: {}", e))?;
        let rows = if let Some(p) = project {
            stmt.query_map(params![query, p], map_entry).map_err(|e| format!("query: {}", e))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("row: {}", e))?
        } else {
            stmt.query_map(params![query], map_entry).map_err(|e| format!("query: {}", e))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("row: {}", e))?
        };
        Ok(rows)
    }

    /// 软删除（archived = 1）
    pub fn forget(&self, id: &str) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {}", e))?;
        conn.execute("UPDATE entries SET archived = 1 WHERE id = ?1", params![id])
            .map_err(|e| format!("update: {}", e))?;
        Ok(())
    }

    /// 列出所有记忆（不含 archived）
    pub fn list(&self, project: Option<&str>) -> Result<Vec<GlobalMemoryEntry>, String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {}", e))?;
        let sql = if project.is_some() {
            "SELECT id, project, content, category, tags, importance, archived, created_at, updated_at
             FROM entries WHERE archived = 0 AND project = ?1 ORDER BY updated_at DESC"
        } else {
            "SELECT id, project, content, category, tags, importance, archived, created_at, updated_at
             FROM entries WHERE archived = 0 ORDER BY updated_at DESC"
        };
        let mut stmt = conn.prepare(sql).map_err(|e| format!("prepare: {}", e))?;
        let rows = if let Some(p) = project {
            stmt.query_map(params![p], map_entry).map_err(|e| format!("query: {}", e))?
                .collect::<Result<Vec<_>, _>>().map_err(|e| format!("row: {}", e))?
        } else {
            stmt.query_map([], map_entry).map_err(|e| format!("query: {}", e))?
                .collect::<Result<Vec<_>, _>>().map_err(|e| format!("row: {}", e))?
        };
        Ok(rows)
    }

    /// 获取全局记忆库路径
    pub fn db_path() -> PathBuf {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_else(|_| ".".into());
        PathBuf::from(home).join(".ion").join("agent").join("global-memory.db")
    }

    /// 从 V0.1 JSON 文件自动迁移到 SQLite。
    /// 扫描所有 project-data/*/memory/outlines/*.json，导入到全局库。
    /// 只在 DB 空时执行（避免重复导入）。
    pub fn migrate_from_v01(&self) -> Result<usize, String> {
        // 如果 DB 已有数据，跳过
        if !self.list(None)?.is_empty() {
            tracing::info!("[global-memory] db has data, skip migration");
            return Ok(0);
        }
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_else(|_| ".".into());
        let project_data_root = PathBuf::from(&home).join(".ion").join("agent").join("project-data");
        if !project_data_root.exists() {
            tracing::info!("[global-memory] no project-data dir, skip migration");
            return Ok(0);
        }
        let mut count = 0;
        // 遍历每个项目目录
        for project_dir in std::fs::read_dir(&project_data_root).map_err(|e| format!("read project-data: {}", e))? {
            let project_dir = match project_dir { Ok(d) => d, Err(_) => continue };
            let memory_dir = project_dir.path().join("memory").join("outlines");
            if !memory_dir.exists() { continue; }
            // 从目录名提取项目名（格式：--hash--name--）
            let dir_name = project_dir.file_name().to_string_lossy().to_string();
            let project_name = dir_name.split("--").last().unwrap_or("unknown").trim_end_matches("--").to_string();
            let project_name = if project_name.is_empty() { "unknown".into() } else { project_name };

            // 遍历每个 outline 文件
            for outline_file in std::fs::read_dir(&memory_dir).into_iter().flatten().flatten() {
                let content = match std::fs::read_to_string(outline_file.path()) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                let entries: Vec<serde_json::Value> = match serde_json::from_str(&content) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                for entry in entries {
                    let id = entry.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let mem_content = entry.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let category = entry.get("category").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let tags_arr = entry.get("tags").and_then(|v| v.as_array());
                    let tags = tags_arr.map(|a| a.iter().filter_map(|t| t.as_str()).collect::<Vec<_>>().join(",")).unwrap_or_default();
                    let archived = entry.get("archived").and_then(|v| v.as_bool()).unwrap_or(false);
                    if mem_content.is_empty() || archived { continue; }
                    // 导入
                    if let Err(e) = self.save(&mem_content, &category, &tags, &project_name, 5) {
                        tracing::warn!("[global-memory] migrate entry {} failed: {}", id, e);
                        continue;
                    }
                    count += 1;
                }
            }
        }
        tracing::info!("[global-memory] migrated {} entries from V0.1", count);
        Ok(count)
    }
}

fn map_entry(row: &rusqlite::Row) -> rusqlite::Result<GlobalMemoryEntry> {
    Ok(GlobalMemoryEntry {
        id: row.get(0)?,
        project: row.get(1)?,
        content: row.get(2)?,
        category: row.get(3)?,
        tags: row.get(4)?,
        importance: row.get(5)?,
        archived: row.get::<_, i32>(6)? != 0,
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
    })
}

/// 生成简易 UUID（不依赖 uuid crate 的 v4，用时间戳+随机）
fn uuid_str() -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pid = std::process::id();
    format!("{:x}{:x}", ts, pid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env::temp_dir;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn test_store() -> GlobalMemoryStore {
        let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = temp_dir().join(format!("global_mem_test_{}_{}.db", std::process::id(), n));
        let _ = std::fs::remove_file(&path);
        GlobalMemoryStore::open(&path).unwrap()
    }

    #[test]
    fn test_db_init() {
        let store = test_store();
        // 表存在
        let entries = store.list(None).unwrap();
        assert!(entries.is_empty(), "fresh db should have no entries");
    }

    #[test]
    fn test_save_and_fts_search() {
        let store = test_store();
        let id = store.save("用户偏好 Rust 的 async/await", "preference", "rust,async", "project-a", 8).unwrap();
        assert!(id.starts_with("gmem_"));

        // FTS5 搜索 "rust"
        let results = store.search("rust", None).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].content, "用户偏好 Rust 的 async/await");
        assert_eq!(results[0].importance, 8);
    }

    #[test]
    fn test_cross_project_search() {
        let store = test_store();
        store.save("project uses typescript", "preference", "ts", "project-a", 5).unwrap();
        store.save("project uses python", "preference", "py", "project-b", 5).unwrap();

        // 全局搜索（不指定 project）
        let results = store.search("project", None).unwrap();
        assert_eq!(results.len(), 2, "should find both projects' entries");

        // 只搜 project-b（用 python 关键词）
        let results = store.search("python", Some("project-b")).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].project, "project-b");
    }

    #[test]
    fn test_importance_ranking() {
        let store = test_store();
        store.save("low importance note", "note", "test", "p", 2).unwrap();
        store.save("high importance note", "note", "test", "p", 10).unwrap();
        store.save("medium importance note", "note", "test", "p", 5).unwrap();

        let results = store.search("note", None).unwrap();
        assert_eq!(results.len(), 3);
        // 按重要性降序
        assert_eq!(results[0].importance, 10);
        assert_eq!(results[1].importance, 5);
        assert_eq!(results[2].importance, 2);
    }

    #[test]
    fn test_soft_delete() {
        let store = test_store();
        let id = store.save("entry to delete", "note", "test", "p", 5).unwrap();
        // 搜索能找到
        assert_eq!(store.search("delete", None).unwrap().len(), 1);
        // 软删除
        store.forget(&id).unwrap();
        // 搜索不返回 archived
        assert_eq!(store.search("delete", None).unwrap().len(), 0);
    }

    #[test]
    fn test_id_unique() {
        let store = test_store();
        let id1 = store.save("第一条", "note", "t", "p", 5).unwrap();
        let id2 = store.save("第二条", "note", "t", "p", 5).unwrap();
        assert_ne!(id1, id2, "IDs must be unique");
    }
}
