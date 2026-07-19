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

/// 大纲索引条目（outlines 表）
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutlineEntry {
    pub id: String,
    pub summary: String,
    pub project: String,
    pub entry_count: i64,
    pub updated_at: i64,
}

/// consolidate 结果统计
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ConsolidationStats {
    pub deduplicated: usize,
    pub archived: usize,
    pub total: usize,
}

/// 全局记忆库（线程安全，Arc<Mutex<Connection>>，可 Clone 因为 Arc）
#[derive(Clone)]
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
            );
            -- FTS5 同步触发器（external content table 模式必须）
            -- 保证 INSERT/UPDATE/DELETE 时 FTS 索引自动同步
            CREATE TRIGGER IF NOT EXISTS entries_ai AFTER INSERT ON entries BEGIN
                INSERT INTO entries_fts(rowid, content, category, tags)
                VALUES (new.rowid, new.content, new.category, new.tags);
            END;
            CREATE TRIGGER IF NOT EXISTS entries_ad AFTER DELETE ON entries BEGIN
                INSERT INTO entries_fts(entries_fts, rowid, content, category, tags)
                VALUES ('delete', old.rowid, old.content, old.category, old.tags);
            END;
            CREATE TRIGGER IF NOT EXISTS entries_au AFTER UPDATE ON entries BEGIN
                INSERT INTO entries_fts(entries_fts, rowid, content, category, tags)
                VALUES ('delete', old.rowid, old.content, old.category, old.tags);
                INSERT INTO entries_fts(rowid, content, category, tags)
                VALUES (new.rowid, new.content, new.category, new.tags);
            END;",
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
        // FTS5 索引由 AFTER INSERT 触发器自动维护，无需手动同步
        Ok(id)
    }

    /// FTS5 全文搜索（含中文 LIKE fallback）。
    ///
    /// 先用 FTS5 MATCH（英文/分词语言效果好），如果结果为空则用 LIKE 模糊匹配
    /// （中文场景 fallback，因为 FTS5 默认 tokenizer 对中文不友好）。
    pub fn search(&self, query: &str, project: Option<&str>) -> Result<Vec<GlobalMemoryEntry>, String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {}", e))?;

        // 1. 先用 FTS5 MATCH
        let fts_sql = if project.is_some() {
            "SELECT e.id, e.project, e.content, e.category, e.tags, e.importance, e.archived, e.created_at, e.updated_at
             FROM entries e JOIN entries_fts f ON e.rowid = f.rowid
             WHERE entries_fts MATCH ?1 AND e.archived = 0 AND e.project = ?2
             ORDER BY e.importance DESC, e.updated_at DESC"
        } else {
            "SELECT e.id, e.project, e.content, e.category, e.tags, e.importance, e.archived, e.created_at, e.updated_at
             FROM entries e JOIN entries_fts f ON e.rowid = f.rowid
             WHERE entries_fts MATCH ?1 AND e.archived = 0
             ORDER BY e.importance DESC, e.updated_at DESC"
        };
        let mut stmt = conn.prepare(fts_sql).map_err(|e| format!("prepare fts: {}", e))?;
        let fts_rows = if let Some(p) = project {
            stmt.query_map(params![query, p], map_entry).map_err(|e| format!("query fts: {}", e))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("row fts: {}", e))?
        } else {
            stmt.query_map(params![query], map_entry).map_err(|e| format!("query fts: {}", e))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("row fts: {}", e))?
        };
        drop(stmt);

        // FTS5 有结果就直接返回
        if !fts_rows.is_empty() {
            return Ok(fts_rows);
        }

        // 2. FTS5 无结果 → LIKE 模糊匹配（中文 fallback）
        //    先按空格/标点拆词，再把连续中文段按 2 字滑动窗口拆（中文无空格分词）
        let mut words: Vec<String> = Vec::new();
        for part in query.split(|c: char| c.is_whitespace() || "，。、！？".contains(c)) {
            if part.is_empty() { continue; }
            // 检查是否含中文字符
            let has_cjk = part.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c));
            if has_cjk && part.chars().count() > 2 {
                // 连续中文：2 字滑动窗口（bigram）
                let chars: Vec<char> = part.chars().collect();
                for i in 0..chars.len().saturating_sub(1) {
                    words.push(chars[i..i+2].iter().collect());
                }
            } else {
                words.push(part.to_string());
            }
        }
        let words: Vec<&str> = if words.is_empty() { vec![query] } else { words.iter().map(|s| s.as_str()).collect() };

        let mut like_rows = Vec::new();
        for word in &words {
            if word.len() < 2 { continue; }  // 跳过单字（噪音太大）
            let like_pattern = format!("%{}%", word);
            let like_sql = if project.is_some() {
                "SELECT id, project, content, category, tags, importance, archived, created_at, updated_at
                 FROM entries WHERE archived = 0 AND project = ?2 AND (content LIKE ?1 OR category LIKE ?1 OR tags LIKE ?1)"
            } else {
                "SELECT id, project, content, category, tags, importance, archived, created_at, updated_at
                 FROM entries WHERE archived = 0 AND (content LIKE ?1 OR category LIKE ?1 OR tags LIKE ?1)"
            };
            let mut stmt2 = conn.prepare(like_sql).map_err(|e| format!("prepare like: {}", e))?;
            let rows = if let Some(p) = project {
                stmt2.query_map(params![like_pattern, p], map_entry).map_err(|e| format!("query like: {}", e))?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| format!("row like: {}", e))?
            } else {
                stmt2.query_map(params![like_pattern], map_entry).map_err(|e| format!("query like: {}", e))?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| format!("row like: {}", e))?
            };
            like_rows.extend(rows);
        }
        // 去重（同一条可能被多个词命中）+ 按 importance 排序
        let mut seen = std::collections::HashSet::new();
        like_rows.retain(|e| seen.insert(e.id.clone()));
        like_rows.sort_by(|a, b| b.importance.cmp(&a.importance).then(b.updated_at.cmp(&a.updated_at)));
        Ok(like_rows)
    }

    /// 软删除（archived = 1）
    pub fn forget(&self, id: &str) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {}", e))?;
        conn.execute("UPDATE entries SET archived=1 WHERE id=?1", params![id])
            .map_err(|e| format!("forget: {}", e))?;
        Ok(())
    }

    /// 批量清空（测试用，DELETE entries + 重建 FTS5 索引 + 清 outlines）
    pub fn clear_all(&self) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {}", e))?;
        // 1. 先清空 entries 表。
        //    由于 FTS5 是 external-content table，DELETE 会触发 AFTER DELETE 触发器
        //    逐行同步删除 entries_fts 中的对应索引。
        conn.execute("DELETE FROM entries", [])
            .map_err(|e| format!("clear entries: {}", e))?;
        // 2. 用 'rebuild' 命令重建 FTS5 索引，彻底清理所有残留。
        //    （DELETE 触发器理论上已清空，但 'rebuild' 可保证一致性，
        //     即使此前触发器因 bug 未执行也能恢复正确状态。）
        conn.execute("INSERT INTO entries_fts(entries_fts) VALUES('rebuild')", [])
            .map_err(|e| format!("rebuild fts: {}", e))?;
        // 3. 清空 outlines 表
        conn.execute("DELETE FROM outlines", [])
            .map_err(|e| format!("clear outlines: {}", e))?;
        Ok(())
    }

    /// 活跃条目数
    pub fn count(&self) -> Result<i64, String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {}", e))?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM entries WHERE archived=0", [], |row| row.get(0)
        ).unwrap_or(0);
        Ok(count)
    }

    /// 检查是否已有相同 content 的活跃记忆（去重用）
    pub fn has_content(&self, content: &str) -> Result<bool, String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {}", e))?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM entries WHERE content = ?1 AND archived = 0",
            params![content],
            |row| row.get(0),
        ).unwrap_or(0);
        Ok(count > 0)
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

    /// 列出所有项目的大纲索引（outlines 表）
    pub fn list_outlines(&self) -> Result<Vec<OutlineEntry>, String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, summary, project, entry_count, updated_at FROM outlines ORDER BY entry_count DESC"
        ).map_err(|e| format!("prepare: {}", e))?;
        let rows = stmt.query_map([], |row| {
            Ok(OutlineEntry {
                id: row.get(0)?,
                summary: row.get(1)?,
                project: row.get(2)?,
                entry_count: row.get(3)?,
                updated_at: row.get(4)?,
            })
        }).map_err(|e| format!("query: {}", e))?;
        let mut result = Vec::new();
        for r in rows {
            result.push(r.map_err(|e| format!("row: {}", e))?);
        }
        Ok(result)
    }

    /// 整理全局记忆库：去重 + 归档 + 更新大纲索引
    pub fn consolidate(&self) -> Result<ConsolidationStats, String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {}", e))?;
        let mut stats = ConsolidationStats::default();

        // 1. 去重：内容完全相同的记忆，保留 importance 最高的，其余 archived
        let dupes: Vec<(String, usize)> = {
            let mut stmt = conn.prepare(
                "SELECT content FROM entries WHERE archived=0 GROUP BY content HAVING COUNT(*) > 1"
            ).map_err(|e| format!("prepare dupes: {}", e))?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))
                .map_err(|e| format!("query dupes: {}", e))?;
            let mut dups = Vec::new();
            for r in rows {
                let content = r.map_err(|e| format!("row: {}", e))?;
                // 找这个 content 里 importance 最高的 id
                let max_id: Option<String> = conn.query_row(
                    "SELECT id FROM entries WHERE content=?1 AND archived=0 ORDER BY importance DESC LIMIT 1",
                    rusqlite::params![content],
                    |row| row.get(0)
                ).ok();
                if let Some(keep_id) = max_id {
                    let changed = conn.execute(
                        "UPDATE entries SET archived=1 WHERE content=?1 AND archived=0 AND id != ?2",
                        rusqlite::params![content, keep_id],
                    ).map_err(|e| format!("dedup update: {}", e))?;
                    stats.deduplicated += changed;
                    dups.push((keep_id, changed));
                }
            }
            dups
        };

        // 2. 归档：importance=0 且超过 30 天的 → archived=1
        let thirty_days_ago = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64 - 30 * 86400)
            .unwrap_or(0);
        stats.archived = conn.execute(
            "UPDATE entries SET archived=1 WHERE importance=0 AND created_at < ?1 AND archived=0",
            rusqlite::params![thirty_days_ago],
        ).map_err(|e| format!("archive update: {}", e))?;

        // 3. 更新大纲索引（outlines 表）
        conn.execute("DELETE FROM outlines", []).map_err(|e| format!("clear outlines: {}", e))?;
        conn.execute(
            "INSERT INTO outlines (id, summary, project, entry_count, updated_at)
             SELECT
                project,
                COALESCE(GROUP_CONCAT(SUBSTR(content, 1, 80), ' | '), ''),
                project,
                COUNT(*),
                unixepoch()
             FROM entries WHERE archived = 0
             GROUP BY project",
            [],
        ).map_err(|e| format!("update outlines: {}", e))?;

        // 统计剩余活跃条数
        stats.total = conn.query_row(
            "SELECT COUNT(*) FROM entries WHERE archived=0", [], |row| row.get(0)
        ).unwrap_or(0);

        Ok(stats)
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
