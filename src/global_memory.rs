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
use uuid::Uuid;

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

    /// Save multiple entries in a single transaction.
    /// Each tuple is (content, category, tags, project, importance).
    /// Returns list of generated IDs on success.
    /// If any save fails, all inserts are rolled back.
    pub fn batch_save(&self, entries: Vec<(&str, &str, &str, &str, i32)>) -> Result<Vec<String>, String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {}", e))?;
        // Use savepoint for rollback capability on error
        conn.execute("SAVEPOINT batch_save", [])
            .map_err(|e| format!("savepoint: {}", e))?;
        let mut ids = Vec::with_capacity(entries.len());
        let result = (|| -> Result<(), String> {
            for (content, category, tags, project, importance) in &entries {
                let id = format!("gmem_{}", uuid_str());
                conn.execute(
                    "INSERT INTO entries (id, project, content, category, tags, importance) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![id, project, content, category, tags, importance],
                )
                .map_err(|e| format!("insert in batch_save: {}", e))?;
                ids.push(id);
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                conn.execute("RELEASE batch_save", [])
                    .map_err(|e| format!("release savepoint: {}", e))?;
                Ok(ids)
            }
            Err(e) => {
                conn.execute("ROLLBACK TO batch_save", [])
                    .map_err(|_| format!("rollback failed after: {}", e))?;
                Err(e)
            }
        }
    }

    /// FTS5 full-text search (with Chinese LIKE fallback).
    ///
    /// 先用 FTS5 MATCH（英文/分词语言效果好），如果结果为空则用 LIKE 模糊匹配
    /// （中文场景 fallback，因为 FTS5 默认 tokenizer 对中文不友好）。
    pub fn search(&self, query: &str, project: Option<&str>) -> Result<Vec<GlobalMemoryEntry>, String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {}", e))?;

        // 1. 先用 FTS5 MATCH
        //    将用户 query 用双引号包裹使其成为字面字符串短语，
        //    query 内部的双引号用双写 ("") 转义，防止注入 FTS5 语法。
        let escaped_query = format!("\"{}\"", query.replace('"', "\"\""));
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
            stmt.query_map(params![escaped_query, p], map_entry).map_err(|e| format!("query fts: {}", e))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("row fts: {}", e))?
        } else {
            stmt.query_map(params![escaped_query], map_entry).map_err(|e| format!("query fts: {}", e))?
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
            if word.chars().count() < 2 { continue; }  // 跳过单字（噪音太大，用字符数而非字节数）
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

    /// 清空有活跃记忆（DELETE FROM entries WHERE archived=0），同时重建 FTS5 索引。
    /// 已档 (archived=1) 的条目不受影响。
    pub fn clear_active(&self) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {}", e))?;
        // 1. 删所有活跃条目
        //    FTS5 的 AFTER DELETE 触发器会自动同步删除 entries_fts 中的对应行
        conn.execute("DELETE FROM entries WHERE archived=0", [])
            .map_err(|e| format!("clear active entries: {}", e))?;
        // 2. 重建 FTS5 索引引，确保一致性
        conn.execute("INSERT INTO entries_fts(entries_fts) VALUES('rebuild')", [])
            .map_err(|e| format!("rebuild fts after clear_active: {}", e))?;
        Ok(())
    }

    /// 批量清空（测试用，DELETE entries + 重建 FTS5 索引引 + 清 outlines）
    pub fn clear_all(&self) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {}", e))?;
        // 1. 先清空 entries 表。
        //    由于 FTS5 是 external-content table，DELETE 会触发 AFTER DELETE 触发器
        //    逐行同步删除 entries_fts 中的对应索引。
        conn.execute("DELETE FROM entries", [])
            .map_err(|e| format!("clear entries: {}", e))?;
        // 2. 用 'rebuild' 命令重建 FTS5 索引引，彻底清理所有残留。
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

    /// 指定项目的活跃条目数
    pub fn count_by_project(&self, project: &str) -> Result<i64, String> {
        self.count_active_by_project(project)
    }

    /// 统计指定项目中活跃（archived=0）的条目数。
    pub fn count_active_by_project(&self, project: &str) -> Result<i64, String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {}", e))?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM entries WHERE archived=0 AND project = ?1",
            params![project],
            |row| row.get(0)
        ).unwrap_or(0);
        Ok(count)
    }

    /// 所有记忆总数（含活跃和归档）
    pub fn memory_count(&self) -> Result<i64, String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {}", e))?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM entries", [], |row| row.get(0)
        ).unwrap_or(0);
        Ok(count)
    }

    /// 归档条目数
    pub fn archived_total(&self) -> Result<i64, String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {}", e))?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM entries WHERE archived=1", [], |row| row.get(0)
        ).unwrap_or(0);
        Ok(count)
    }

    /// 返回所有条目的摘要符串，格式："Total: {total}, Active: {active}, Archived: {archived}"
    pub fn entries_summary(&self) -> Result<String, String> {
        let total = self.memory_count()?;
        let archived = self.archived_total()?;
        let active = total - archived;
        Ok(format!("Total: {total}, Active: {active}, Archived: {archived}"))
    }

    pub fn count_all_archived(&self) -> Result<i64, String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {}", e))?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM entries WHERE archived=1", [], |row| row.get(0)
        ).unwrap_or(0);
        Ok(count)
    }

    /// 指定项目的归档条目数
    pub fn count_archived_by_project(&self, project: &str) -> Result<i64, String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {}", e))?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM entries WHERE archived=1 AND project = ?1",
            params![project],
            |row| row.get(0)
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

    /// 检查指定 ID 的记忆是否存在（含活跃和已归档条目）
    pub fn entry_exists(&self, id: &str) -> Result<bool, String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {}", e))?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM entries WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )
        .unwrap_or(0);
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
        //    先 DELETE 再按 project GROUP 重建。每行 ID 用 randomblob(16) 生成唯一值。
        //    randomblob() 是非确定性函数，SQLite 会对 SELECT 输出的每一行独立求值，
        //    因此 GROUP BY project 后每个项目组都会拿到独立的 16 字节随机 ID，不会碰撞
        conn.execute("DELETE FROM outlines", []).map_err(|e| format!("clear outlines: {}", e))?;
        conn.execute(
            "INSERT INTO outlines (id, summary, project, entry_count, updated_at)
             SELECT
                'outl_' || lower(hex(randomblob(16))),
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

        // 验证返回唯一项目名称列表（按字母序）
    pub fn project_list(&self) -> Result<Vec<String>, String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {}", e))?;
        let mut stmt = conn
            .prepare(
                "SELECT DISTINCT project FROM entries WHERE archived=0 ORDER BY project COLLATE NOCASE"
            )
            .map_err(|e| format!("prepare project_list: {}", e))?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| format!("query project_list: {}", e))?;
        let mut projects = Vec::new();
        for r in rows {
            projects.push(r.map_err(|e| format!("row project_list: {}", e))?);
        }
        Ok(projects)
    }

    /// 返回活跃条目中唯一项目的数量。
    pub fn project_count(&self) -> Result<i64, String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {}", e))?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(DISTINCT project) FROM entries WHERE archived=0", [], |row| row.get(0)
        ).unwrap_or(0);
        Ok(count)
    }

    /// 统计包含指定标签（tags 字段）的活跃条目数。
    /// tags 是逗号分隔的字符串，使用 LIKE 模糊匹配。
    pub fn tag_count(&self, tag: &str) -> Result<i64, String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {}", e))?;
        let pattern = format!("%{}%", tag);
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM entries WHERE archived=0 AND tags LIKE ?1",
            params![pattern],
            |row| row.get(0)
        ).unwrap_or(0);
        Ok(count)
    }

    /// 按 created_at 降序返回最近 limit 条 entry。
    pub fn recent_entries(&self, limit: usize) -> Result<Vec<GlobalMemoryEntry>, String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {}", e))?;
        let mut stmt = conn
            .prepare(
                "SELECT id, project, content, category, tags, importance, archived, created_at, updated_at
                 FROM entries ORDER BY created_at DESC LIMIT ?1",
            )
            .map_err(|e| format!("prepare recent_entries: {}", e))?;
        let rows = stmt
            .query_map(params![limit as i64], map_entry)
            .map_err(|e| format!("query recent_entries: {}", e))?;
        let mut results = Vec::new();
        for r in rows {
            results.push(r.map_err(|e| format!("row recent_entries: {}", e))?);
        }
        Ok(results)
    }

    /// Return up to N most recent entries (created_at DESC) for the given project.
    pub fn list_recent_by_project(&self, project: &str, limit: usize) -> Result<Vec<GlobalMemoryEntry>, String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {}", e))?;
        let mut stmt = conn
            .prepare(
                "SELECT id, project, content, category, tags, importance, archived, created_at, updated_at
                 FROM entries WHERE project = ?1 ORDER BY created_at DESC LIMIT ?2",
            )
            .map_err(|e| format!("prepare list_recent_by_project: {}", e))?;
        let rows = stmt
            .query_map(params![project, limit as i64], map_entry)
            .map_err(|e| format!("query list_recent_by_project: {}", e))?;
        let mut results = Vec::new();
        for r in rows {
            results.push(r.map_err(|e| format!("row list_recent_by_project: {}", e))?);
        }
        Ok(results)
    }

    /// 返回 archived=1 的 entry 总数
    pub fn archive_count(&self) -> Result<i64, String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {}", e))?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM entries WHERE archived = 1", [], |row| row.get(0)
        ).unwrap_or(0);
        Ok(count)
    }

    /// 返回最早 entry 的 created_at如果表为空返回 None）。
    pub fn oldest_entry_age(&self) -> Result<Option<i64>, String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {}", e))?;
        let result: Option<i64> = conn.query_row(
            "SELECT MIN(created_at) FROM entries", [], |row| row.get(0)
        ).ok().flatten();
        Ok(result)
    }

    /// Return the entry with the lowest importance value (min importance).
    /// Returns Ok(None) if the table is empty.
    pub fn oldest_by_importance(&self) -> Result<Option<GlobalMemoryEntry>, String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {}", e))?;
        let result: Option<GlobalMemoryEntry> = conn.query_row(
            "SELECT id, project, content, category, tags, importance, archived, created_at, updated_at
             FROM entries ORDER BY importance ASC LIMIT 1",
            [],
            map_entry,
        ).ok();
        Ok(result)
    }

    /// 统计 tags 列中包含指定 tag 字符串的 entry 数。
    /// tags 是逗号分隔的字符串（例如 rust,sqlite,memory），用 LIKE 模糊匹配。
    pub fn count_by_tags(&self, tag: &str) -> Result<i64, String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {}", e))?;
        let pattern = format!("%{}%", tag);
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM entries WHERE tags LIKE ?1",
            params![pattern],
            |row| row.get(0),
        ).map_err(|e| format!("query count_by_tags: {}", e))?;
        Ok(count)
    }

    /// 统计指定 category 的 entry 数（精确匹配，不用 LIKE）。
    pub fn count_by_category(&self, category: &str) -> Result<i64, String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {}", e))?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM entries WHERE category = ?1",
            params![category],
            |row| row.get(0),
        ).map_err(|e| format!("query count_by_category: {}", e))?;
        Ok(count)
    }

    /// 返回指定 project 下 created_at 最早的 entry（一条）。
    /// 如果该 project 没有任何 entry，返回 None。
    pub fn find_oldest_by_project(&self, project: &str) -> Result<Option<GlobalMemoryEntry>, String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {}", e))?;
        let mut stmt = conn
            .prepare(
                "SELECT id, project, content, category, tags, importance, archived, created_at, updated_at
                 FROM entries WHERE project = ?1 ORDER BY created_at ASC LIMIT 1"
            )
            .map_err(|e| format!("prepare find_oldest_by_project: {}", e))?;
        let mut rows = stmt
            .query_map(params![project], map_entry)
            .map_err(|e| format!("query find_oldest_by_project: {}", e))?;
        match rows.next() {
            Some(Ok(entry)) => Ok(Some(entry)),
            Some(Err(e)) => Err(format!("row find_oldest_by_project: {}", e)),
            None => Ok(None),
        }
    }

    /// Delete all entries (including archived) for a given project.
    /// Returns the number of rows deleted.
    pub fn delete_by_project(&self, project: &str) -> Result<usize, String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {}", e))?;
        let deleted = conn
            .execute("DELETE FROM entries WHERE project = ?1", params![project])
            .map_err(|e| format!("delete_by_project: {}", e))?;
        Ok(deleted)
    }

        /// Advanced multi-field search using FTS5 MATCH for content,
    /// with optional filters for project, category, and minimum importance.
    pub fn search_advanced(
        &self,
        query: &str,
        project: Option<&str>,
        category: Option<&str>,
        min_importance: i32,
    ) -> Result<Vec<GlobalMemoryEntry>, String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {}", e))?;
        let escaped_query = format!( "\"{}\"", query.replace('"', "\"\"") );
        let mut where_clauses = vec![
            "entries_fts MATCH ?1".to_string(),
            "e.archived = 0".to_string(),
            "e.importance >= ?2".to_string(),
        ];
        let mut param_idx = 3;
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![
            Box::new(escaped_query),
            Box::new(min_importance),
        ];
        if let Some(proj) = project {
            where_clauses.push(format!("e.project = ?{}", param_idx));
            params.push(Box::new(proj.to_string()));
            param_idx += 1;
        }
        if let Some(cat) = category {
            where_clauses.push(format!("e.category = ?{}", param_idx));
            params.push(Box::new(cat.to_string()));
        }
        let where_sql = where_clauses.join(" AND ");
        let sql = format!(
            "SELECT e.id, e.project, e.content, e.category, e.tags, e.importance, e.archived, e.created_at, e.updated_at
             FROM entries e JOIN entries_fts f ON e.rowid = f.rowid
             WHERE {}
             ORDER BY e.importance DESC",
            where_sql
        );
        let mut stmt = conn.prepare(&sql).map_err(|e| format!("prepare: {}", e))?;
        let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let rows = stmt.query_map(param_refs.as_slice(), map_entry).map_err(|e| format!("query: {}", e))?;
        let mut results = Vec::new();
        for r in rows {
            results.push(r.map_err(|e| format!("row: {}", e))?);
        }
        Ok(results)
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

    /// Check if any entry's tags column contains the given tag (LIKE '%tag%').
    /// Returns true if at least one match exists, false otherwise.
    pub fn has_tag(&self, tag: &str) -> Result<bool, String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {}", e))?;
        let pattern = format!("%{}%", tag);
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM entries WHERE tags LIKE ?1",
            params![pattern],
            |row| row.get(0),
        ).unwrap_or(0);
        Ok(count > 0)
    }

    /// Find all entries whose content starts with the given prefix.
    /// Uses SQL `content LIKE 'prefix%'`. Results are ordered by created_at DESC.
    pub fn find_by_content_prefix(&self, prefix: &str) -> Result<Vec<GlobalMemoryEntry>, String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {}", e))?;
        let pattern = format!("{}%", prefix);
        let mut stmt = conn
            .prepare(
                "SELECT id, project, content, category, tags, importance, archived, created_at, updated_at
                 FROM entries WHERE content LIKE ?1 ORDER BY created_at DESC",
            )
            .map_err(|e| format!("prepare find_by_content_prefix: {}", e))?;
        let rows = stmt
            .query_map(params![pattern], map_entry)
            .map_err(|e| format!("query find_by_content_prefix: {}", e))?;
        let mut results = Vec::new();
        for r in rows {
            results.push(r.map_err(|e| format!("row find_by_content_prefix: {}", e))?);
        }
        Ok(results)
    }

    /// Update the importance of an existing entry.
    /// Returns Err if no row with the given id exists.
    pub fn update_importance(&self, id: &str, importance: i32) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {}", e))?;
        let affected = conn
            .execute(
                "UPDATE entries SET importance=?2 WHERE id=?1",
                params![id, importance],
            )
            .map_err(|e| format!("update_importance: {}", e))?;
        if affected == 0 {
            return Err(format!("entry with id '{}' not found", id));
        }
        Ok(())
    }

    /// Archive all entries for a given project by setting archived=1.
    /// Returns the number of rows updated.
    pub fn archive_by_project(&self, project: &str) -> Result<usize, String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {}", e))?;
        let updated = conn
            .execute("UPDATE entries SET archived=1 WHERE project=?1", params![project])
            .map_err(|e| format!("archive_by_project: {}", e))?;
        Ok(updated)
    }

    /// Return total count of archived=1 entries across all projects.
    pub fn count_archived(&self) -> Result<i64, String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {}", e))?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM entries WHERE archived=1", [], |row| row.get(0)
        ).unwrap_or(0);
        Ok(count)
    }

    /// Update the category of an existing entry.
    /// Returns Err if no row with the given id exists (0 rows affected).
    pub fn update_category(&self, id: &str, new_category: &str) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {}", e))?;
        let affected = conn
            .execute(
                "UPDATE entries SET category = ?2 WHERE id = ?1",
                params![id, new_category],
            )
            .map_err(|e| format!("update_category: {}", e))?;
        if affected == 0 {
            return Err(format!("entry with id '{}' not found", id));
        }
        Ok(())
    }

    /// Find entries whose content appears more than once in the table.
    /// Uses GROUP BY content HAVING COUNT(*) > 1.
    /// Returns one representative entry per duplicate group.
    pub fn find_duplicates(&self) -> Result<Vec<GlobalMemoryEntry>, String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {}", e))?;
        let mut stmt = conn
            .prepare(
                "SELECT id, project, content, category, tags, importance, archived, created_at, updated_at
                 FROM entries
                 WHERE content IN (
                     SELECT content FROM entries GROUP BY content HAVING COUNT(*) > 1
                 )
                 GROUP BY content"
            )
            .map_err(|e| format!("prepare find_duplicates: {}", e))?;
        let rows = stmt
            .query_map([], map_entry)
            .map_err(|e| format!("query find_duplicates: {}", e))?;
        let mut results = Vec::new();
        for r in rows {
            results.push(r.map_err(|e| format!("row find_duplicates: {}", e))?);
        }
        Ok(results)
    }

    /// Parse a JSON array of objects and import each as a new memory entry.
    ///
    /// Each object may have fields: `content`, `category`, `tags`, `project`, `importance`.
    /// Returns the number of entries successfully imported.
    /// Returns `Err` if the JSON string is syntactically invalid.
    pub fn import_json(&self, json_str: &str) -> Result<usize, String> {
        #[derive(serde::Deserialize)]
        struct JsonImportItem {
            content: String,
            #[serde(default)]
            category: String,
            #[serde(default)]
            tags: String,
            #[serde(default)]
            project: String,
            #[serde(default = "default_importance")]
            importance: i32,
        }

        fn default_importance() -> i32 {
            5
        }

        let items: Vec<JsonImportItem> =
            serde_json::from_str(json_str).map_err(|e| format!("invalid JSON: {}", e))?;

        let mut count = 0;
        for item in &items {
            self.save(&item.content, &item.category, &item.tags, &item.project, item.importance)?;
            count += 1;
        }
        Ok(count)
    }

    /// Export all entries as a JSON array. If filter_project is Some,
    /// only entries for that project are included.
    /// Each entry has fields: id, content, category, project, archived, created_at.
    pub fn export_json(&self, filter_project: Option<&str>) -> Result<String, String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {}", e))?;
        let (sql, has_filter) = if let Some(project) = filter_project {
            (
                "SELECT id, content, category, project, archived, created_at FROM entries WHERE project = ?1 ORDER BY created_at ASC",
                true,
            )
        } else {
            (
                "SELECT id, content, category, project, archived, created_at FROM entries ORDER BY created_at ASC",
                false,
            )
        };
        let mut stmt = conn.prepare(sql).map_err(|e| format!("prepare export_json: {}", e))?;
        let rows = if has_filter {
            // We need the project param, extract it from the option again
            // because borrow checker can't see through the let binding above.
            let project = filter_project.unwrap();
            stmt.query_map(params![project], |row| {
                Ok(serde_json::json!({
                    "id": row.get::<_, String>(0)?,
                    "content": row.get::<_, String>(1)?,
                    "category": row.get::<_, String>(2)?,
                    "project": row.get::<_, String>(3)?,
                    "archived": row.get::<_, i32>(4)? != 0,
                    "created_at": row.get::<_, i64>(5)?,
                }))
            })
            .map_err(|e| format!("query export_json: {}", e))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("row export_json: {}", e))?
        } else {
            stmt.query_map([], |row| {
                Ok(serde_json::json!({
                    "id": row.get::<_, String>(0)?,
                    "content": row.get::<_, String>(1)?,
                    "category": row.get::<_, String>(2)?,
                    "project": row.get::<_, String>(3)?,
                    "archived": row.get::<_, i32>(4)? != 0,
                    "created_at": row.get::<_, i64>(5)?,
                }))
            })
            .map_err(|e| format!("query export_json: {}", e))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("row export_json: {}", e))?
        };
        serde_json::to_string(&rows).map_err(|e| format!("serialize: {}", e))
    }

    /// Find all entries whose importance is within the given [min, max] range (inclusive).
    /// Results are ordered by importance DESC.
    pub fn find_by_importance_range(&self, min: i32, max: i32) -> Result<Vec<GlobalMemoryEntry>, String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {}", e))?;
        let mut stmt = conn
            .prepare(
                "SELECT id, project, content, category, tags, importance, archived, created_at, updated_at
                 FROM entries WHERE importance >= ?1 AND importance <= ?2 ORDER BY importance DESC",
            )
            .map_err(|e| format!("prepare find_by_importance_range: {}", e))?;
        let rows = stmt
            .query_map(params![min, max], map_entry)
            .map_err(|e| format!("query find_by_importance_range: {}", e))?;
        let mut results = Vec::new();
        for r in rows {
            results.push(r.map_err(|e| format!("row find_by_importance_range: {}", e))?);
        }
        Ok(results)
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

/// 生成 UUID v4 字符串
fn uuid_str() -> String {
    Uuid::new_v4().to_string()
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
        let id2 = store.save("第二条条", "note", "t", "p", 5).unwrap();
        assert_ne!(id1, id2, "IDs must be unique");
    }

    #[test]
    fn test_search_chinese_single_char() {
        // Bug 1 回归: word.len() 返回字节数，单个中文字 word.len()=3 会通过 < 2 检查。
        // 应该用 chars().count() < 2 判断字符数，确保单字被跳过。
        let store = test_store();
        store.save("编程语言对比分析", "note", "t", "p", 5).unwrap();
        store.save("深度学习与神经网络", "note", "t", "p", 5).unwrap();

        // 搜索 "编"（单字），应走 LIKE fallback 且被跳过，不返回噪音结果
        let results = store.search("编", None).unwrap();
        assert!(
            results.is_empty(),
            "单个中文字应被跳过（chars().count() < 2），但返回了 {} 条结果",
            results.len()
        );

        // 搜索 "编程"（2 字），应能匹配
        let results = store.search("编程", None).unwrap();
        assert_eq!(results.len(), 1, "双字中文搜索应返回 1 条结果");
        assert_eq!(results[0].content, "编程语言对比分析");
    }

    #[test]
    fn test_search_fts_escape() {
        // Bug 2 回归: FTS5 MATCH 查询没有转义特殊字符，含 OR/AND/NOT/双引号的 query
        // 会被当 FTS5 语法处理。应该用双引号包裹 + 内部双引号双写转义。
        let store = test_store();
        store.save("Use OR keyword in content", "note", "t", "p", 5).unwrap();
        store.save("Normal entry without keywords", "note", "t", "p", 5).unwrap();

        // 搜索 "OR" —— 修复前会被 FTS5 当布尔运算符，导致报错或返回全部结果
        let results = store.search("OR", None).unwrap();
        assert_eq!(
            results.len(),
            1,
            "搜索 'OR' 应只返回包含该字面词的条目，而非被当 FTS5 运算符"
        );
        assert!(results[0].content.contains("OR"));

        // 搜索含双引号的内容
        store.save("Say \"hello world\" loudly", "note", "t", "p", 5).unwrap();
        let results = store.search("\"hello world\"", None).unwrap();
        assert_eq!(
            results.len(),
            1,
            "含双引号的 query 应被正确转义并匹配字面内容"
        );
    }

    #[test]
    fn test_count_all_archived() {
        let store = test_store();

        // 初始状态：无归档
        assert_eq!(store.count_all_archived().unwrap(), 0);

        // 保存 3 条活跃 + 2 条待归档
        let id1 = store.save("active one", "note", "t", "p", 5).unwrap();
        let _ = store.save("active two", "note", "t", "p", 5).unwrap();
        let _ = store.save("active three", "note", "t", "p", 5).unwrap();
        let id2 = store.save("to archive one", "note", "t", "p", 5).unwrap();
        let id3 = store.save("to archive two", "note", "t", "p", 5).unwrap();

        // 归档前：archived 数应为 0
        assert_eq!(store.count_all_archived().unwrap(), 0);
        // 活跃数应为 5
        assert_eq!(store.count().unwrap(), 5);

        // 归档 2 条
        store.forget(&id2).unwrap();
        store.forget(&id3).unwrap();

        // 归档后：archived 数应为 2
        assert_eq!(store.count_all_archived().unwrap(), 2);
        // 活跃数应为 3
        assert_eq!(store.count().unwrap(), 3);

        // 再归档 1 条
        store.forget(&id1).unwrap();
        assert_eq!(store.count_all_archived().unwrap(), 3);
        assert_eq!(store.count().unwrap(), 2);
    }

    
    #[test]
    fn test_memory_count() {
        let store = test_store();

        // 初始空库：总数为 0
        assert_eq!(store.memory_count().unwrap(), 0);

        // 保存 3 条
        store.save("entry one", "note", "t", "p", 5).unwrap();
        store.save("entry two", "note", "t", "p", 5).unwrap();
        store.save("entry three", "note", "t", "p", 5).unwrap();
        assert_eq!(store.memory_count().unwrap(), 3);

        // 归档 1 条（memory_count 仍应包含归档条目）
        let entries = store.list(None).unwrap();
        store.forget(&entries[0].id).unwrap();
        assert_eq!(
            store.memory_count().unwrap(),
            3,
            "memory_count 应包含归档条目，总数仍为 3"
        );

        // 归档 2 条后，总数仍为 3
        store.forget(&entries[1].id).unwrap();
        assert_eq!(store.memory_count().unwrap(), 3);

        // 清空所有后总数为 0
        store.clear_all().unwrap();
        assert_eq!(store.memory_count().unwrap(), 0);
    }

    #[test]
    fn test_count_by_project() {
        let store = test_store();

        // 初始空库：所有项目数值为 0
        assert_eq!(store.count_by_project("project-a").unwrap(), 0);
        assert_eq!(store.count_by_project("project-b").unwrap(), 0);
        assert_eq!(store.count().unwrap(), 0);

        // 只向 project-a 插入条目
        store.save("project a content 1", "note", "t", "project-a", 5).unwrap();
        store.save("project a content 2", "note", "t", "project-a", 5).unwrap();
        store.save("project a content 3", "note", "t", "project-a", 5).unwrap();
        assert_eq!(store.count_by_project("project-a").unwrap(), 3, "project-a 应有 3 条");
        assert_eq!(store.count_by_project("project-b").unwrap(), 0, "project-b 应有 0 条");

        // 向 project-b 插入条目
        store.save("project b content", "note", "t", "project-b", 5).unwrap();
        assert_eq!(store.count_by_project("project-a").unwrap(), 3);
        assert_eq!(store.count_by_project("project-b").unwrap(), 1, "project-b 应有 1 条");

        // 向 project-a 再插入条目
        store.save("project a content 4", "note", "t", "project-a", 5).unwrap();
        assert_eq!(store.count_by_project("project-a").unwrap(), 4);
        assert_eq!(store.count_by_project("project-b").unwrap(), 1);

        // 归档一条 project-a 的条目后，count_by_project 应减少
        let entries = store.list(Some("project-a")).unwrap();
        let oldest_id = entries.last().unwrap().id.clone();
        store.forget(&oldest_id).unwrap();
        assert_eq!(store.count_by_project("project-a").unwrap(), 3, "归档后 project-a 应为 3 条");
        assert_eq!(store.count_by_project("project-b").unwrap(), 1, "归档不影响 project-b");

        // 总活跃条目数 = 3 + 1 = 4
        assert_eq!(store.count().unwrap(), 4);
    }

    #[test]
    fn test_count_active_by_project() {
        let store = test_store();

        // 初始空库：各项目的活跃数为 0
        assert_eq!(store.count_active_by_project("project-a").unwrap(), 0);
        assert_eq!(store.count_active_by_project("project-b").unwrap(), 0);

        // 向 project-a 插入 3 条，project-b 插入 1 条
        store.save("pa content 1", "note", "t", "project-a", 5).unwrap();
        store.save("pa content 2", "note", "t", "project-a", 5).unwrap();
        store.save("pa content 3", "note", "t", "project-a", 5).unwrap();
        store.save("pb content 1", "note", "t", "project-b", 5).unwrap();

        assert_eq!(store.count_active_by_project("project-a").unwrap(), 3, "project-a 应有 3 条活跃");
        assert_eq!(store.count_active_by_project("project-b").unwrap(), 1, "project-b 应有 1 条活跃");
        assert_eq!(store.count().unwrap(), 4, "总跃条目数为 4");

        // 归档一条 project-a 的条目，活跃数应减少
        let pa_entries = store.list(Some("project-a")).unwrap();
        store.forget(&pa_entries[0].id).unwrap();
        assert_eq!(store.count_active_by_project("project-a").unwrap(), 2, "归档后 project-a 活跃数应为 2");
        assert_eq!(store.count_active_by_project("project-b").unwrap(), 1, "归档不影响 project-b");
        assert_eq!(store.count().unwrap(), 3, "归档后总活跃数应为 3");

        // 归档不影响已归档项目的活跃计数
        store.forget(&pa_entries[1].id).unwrap();
        assert_eq!(store.count_active_by_project("project-a").unwrap(), 1, "再归档一条后 project-a 活跃数应为 1");
        assert_eq!(store.count_active_by_project("project-b").unwrap(), 1, "project-b 仍为 1");

        // 全部归档后，活跃数为 0
        store.forget(&pa_entries[2].id).unwrap();
        let pb_entries = store.list(Some("project-b")).unwrap();
        store.forget(&pb_entries[0].id).unwrap();
        assert_eq!(store.count_active_by_project("project-a").unwrap(), 0);
        assert_eq!(store.count_active_by_project("project-b").unwrap(), 0);
        assert_eq!(store.count().unwrap(), 0);
    }

    #[test]
    fn test_count_archived_by_project() {
        let store = test_store();

        // 初始空库：各项目的活跃数为 0
        assert_eq!(store.count_archived_by_project("project-a").unwrap(), 0);
        assert_eq!(store.count_archived_by_project("project-b").unwrap(), 0);

        // 向 project-b 插入条目
        store.save("pa active 1", "note", "t", "project-a", 5).unwrap();
        store.save("pa active 2", "note", "t", "project-a", 5).unwrap();
        store.save("pb active 1", "note", "t", "project-b", 5).unwrap();
        assert_eq!(store.count_archived_by_project("project-a").unwrap(), 0, "归档前为 0");
        assert_eq!(store.count_archived_by_project("project-b").unwrap(), 0, "归档前为 0");

        // 归档一条 project-a 的条目
        let pa_entries = store.list(Some("project-a")).unwrap();
        store.forget(&pa_entries[0].id).unwrap();
        assert_eq!(store.count_archived_by_project("project-a").unwrap(), 1, "project-a 应有 1 条归档");
        assert_eq!(store.count_archived_by_project("project-b").unwrap(), 0, "project-b 应仍为 0");

        // 再归档另一条 project-a 的条目
        store.forget(&pa_entries[1].id).unwrap();
        assert_eq!(store.count_archived_by_project("project-a").unwrap(), 2, "project-a 应有 2 条归档");
        assert_eq!(store.count_archived_by_project("project-b").unwrap(), 0);

        // 归档一条 project-b 的条目
        let pb_entries = store.list(Some("project-b")).unwrap();
        store.forget(&pb_entries[0].id).unwrap();
        assert_eq!(store.count_archived_by_project("project-a").unwrap(), 2);
        assert_eq!(store.count_archived_by_project("project-b").unwrap(), 1, "project-b 应有 1 条归档");

        // 总归档数应等于 3
        assert_eq!(store.count_all_archived().unwrap(), 3);
    }

    #[test]
    fn test_archived_total() {
        let store = test_store();

        // 初始：归档数为 0
        assert_eq!(store.archived_total().unwrap(), 0);

        // 存 3 条活跃
        store.save("active a", "note", "t", "p", 5).unwrap();
        store.save("active b", "note", "t", "p", 5).unwrap();
        store.save("active c", "note", "t", "p", 5).unwrap();
        assert_eq!(store.archived_total().unwrap(), 0);

        // 归档 2 条
        let entries = store.list(None).unwrap();
        store.forget(&entries[0].id).unwrap();
        store.forget(&entries[1].id).unwrap();
        assert_eq!(store.archived_total().unwrap(), 2);

        // 再归档 1 条
        store.forget(&entries[2].id).unwrap();
        assert_eq!(store.archived_total().unwrap(), 3);
    }

    #[test]
    fn test_entry_exists() {
        let store = test_store();

        // 不存在的 ID
        assert!(!store.entry_exists("gmem_nonexistent").unwrap());

        // 保存一条
        let id = store.save("test entry for exists check", "note", "t", "p", 5).unwrap();

        // 存在的 ID
        assert!(store.entry_exists(&id).unwrap());

        // 软删除后仍然存在（entry_exists 检所有条目，含 archived
        store.forget(&id).unwrap();
        assert!(
            store.entry_exists(&id).unwrap(),
            "软删除后条目仍存在于表中，entry_exists 应返回 true"
        );
    }

    /// 回归测试：consolidate() 重建 outlines 时，多 project 的 outline ID 必须唯一，
    /// 不能用 project 名作主键（会与现有 entries 冲突），也不能让 randomblob 退化成同一值。
    #[test]
    fn test_outline_ids_unique_after_consolidate() {
        let store = test_store();
        // 3 个 project，每个多条
        for p in &["alpha", "beta", "gamma"] {
            for i in 0..3 {
                store
                    .save(&format!("{}-content-{}", p, i), "note", "t", p, 5)
                    .unwrap();
            }
        }
        store.consolidate().unwrap();

        let outlines = store.list_outlines().unwrap();
        assert_eq!(outlines.len(), 3, "应有 3 个 project 大纲");

        // 关键断言：所有 outline 的 id 必须唯一，且都以 outl_ 前缀开头（来自 randomblob）
        let ids: Vec<_> = outlines.iter().map(|o| o.id.as_str()).collect();
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(
            unique.len(),
            ids.len(),
            "outline ID 碰撞！ids = {:?}",
            ids
        );
        for id in &ids {
            assert!(
                id.starts_with("outl_") && id.len() == "outl_".len() + 32,
                "outline id 应为 outl_ + 32 hex 字符，实际 = {}",
                id
            );
        }
    }

    /// 测试 clear_active() 方法：
    /// 1) 清空前有活跃条目
    /// 2) 清空后 count() 为 0
    /// 3) 清空后已 archived 的条目不受影响
    /// 4) 清空后 memory_count() 应等于 archived 数
    #[test]
    fn test_clear_active() {
        let store = test_store();

        // 保存 3 条活跃 + 2 条待归档
        let id1 = store.save("active one", "note", "t", "p", 5).unwrap();
        let id2 = store.save("active two", "note", "t", "p", 5).unwrap();
        let id3 = store.save("active three", "note", "t", "p", 5).unwrap();
        let id4 = store.save("to archive one", "note", "t", "p", 5).unwrap();
        let id5 = store.save("to archive two", "note", "t", "p", 5).unwrap();

        // 归档 2 条
        store.forget(&id4).unwrap();
        store.forget(&id5).unwrap();

        // 验证 1) 清空前有活跃条目
        assert_eq!(store.count().unwrap(), 3, "清空前应有 3 条活跃");
        assert_eq!(store.archived_total().unwrap(), 2, "清空前应有 2 条归档");
        assert_eq!(store.memory_count().unwrap(), 5, "清空前总条数为 5");

        // 执行 clear_active()
        store.clear_active().unwrap();

    /// 2) 清空后 count() 为 0
        assert_eq!(store.count().unwrap(), 0, "clear_active 后活跃条目应为 0");

        // 验证 3) 清空后已 archived 的条目不受影响
        assert_eq!(store.archived_total().unwrap(), 2, "clear_active 后归档条目仍为 2");

        // 验证 4) 清空后 memory_count() 应等于 archived 数
        assert_eq!(
            store.memory_count().unwrap(),
            store.archived_total().unwrap(),
            "clear_active 后 memory_count 应等于 archived 数"
        );

        // 验证归档的条目仍然以查到存在
        assert!(store.entry_exists(&id4).unwrap(), "已归档条目 id4 应仍存在");
        assert!(store.entry_exists(&id5).unwrap(), "已归档条目 id5 应仍存在");

        // 验证活跃条目已被删除
        assert!(!store.entry_exists(&id1).unwrap(), "活跃条目 id1 应已删除");
        assert!(!store.entry_exists(&id2).unwrap(), "活跃条目 id2 应已删除");
        assert!(!store.entry_exists(&id3).unwrap(), "活跃条目 id3 应已删除");
    }

    #[test]
    fn test_entries_summary() {
        let store = test_store();

        // 初始空库
        let summary = store.entries_summary().unwrap();
        assert_eq!(summary, "Total: 0, Active: 0, Archived: 0");

        // 保存 5 条活跃条目
        let ids: Vec<String> = (0..5)
            .map(|i| store.save(&format!("entry {}", i), "note", "t", "p", 5).unwrap())
            .collect();
        let summary = store.entries_summary().unwrap();
        assert_eq!(summary, "Total: 5, Active: 5, Archived: 0");

        // 归档 2 条
        store.forget(&ids[0]).unwrap();
        store.forget(&ids[1]).unwrap();
        let summary = store.entries_summary().unwrap();
        assert_eq!(summary, "Total: 5, Active: 3, Archived: 2");
    }

    #[test]
    fn test_project_list() {
        let store = test_store();

        // 空库返回空列表
        let projects = store.project_list().unwrap();
        assert!(projects.is_empty(), "空库应返回空列表");

        // 向不同项目插入条目
        store.save("content a1", "note", "t", "project-alpha", 5).unwrap();
        store.save("content a2", "note", "t", "project-alpha", 5).unwrap();
        store.save("content b1", "note", "t", "project-beta", 5).unwrap();
        store.save("content g1", "note", "t", "project-gamma", 5).unwrap();

        // 验证返回唯一项目名称列表（按字母序）
        let projects = store.project_list().unwrap();
        assert_eq!(projects.len(), 3, "应有 3 个唯一项目");
        assert_eq!(projects[0], "project-alpha");
        assert_eq!(projects[1], "project-beta");
        assert_eq!(projects[2], "project-gamma");

        // 再插入一个现有项目的新条目，不增加项目数
        store.save("content a3", "note", "t", "project-alpha", 5).unwrap();
        let projects = store.project_list().unwrap();
        assert_eq!(projects.len(), 3, "插入已存在项目后应仍为 3 个");

        // 归项目 beta 的所有条目后，project_list 应不再包含 project-beta
        let beta_entries = store.list(Some("project-beta")).unwrap();
        for e in &beta_entries {
            store.forget(&e.id).unwrap();
        }
        let projects = store.project_list().unwrap();
        assert_eq!(projects.len(), 2, "归档 project-beta 后应只剩 2 个项目");
        assert_eq!(projects[0], "project-alpha");
        assert_eq!(projects[1], "project-gamma");
    }

    #[test]
    fn test_project_count() {
        let store = test_store();

        // 空库应返回 0
        assert_eq!(store.project_count().unwrap(), 0, "空库应返回 0");

        // 向不同项目插入条目
        store.save("content a1", "note", "t", "project-alpha", 5).unwrap();
        store.save("content a2", "note", "t", "project-alpha", 5).unwrap();
        store.save("content b1", "note", "t", "project-beta", 5).unwrap();
        store.save("content g1", "note", "t", "project-gamma", 5).unwrap();

        // 应有 3 个唯一项目
        assert_eq!(store.project_count().unwrap(), 3);

        // 再插入已存在项目目，项目数不变
        store.save("content a3", "note", "t", "project-alpha", 5).unwrap();
        assert_eq!(store.project_count().unwrap(), 3);

        // 归档 project-beta 所有条目后，应剩 2 个项目
        let beta_entries = store.list(Some("project-beta")).unwrap();
        for e in &beta_entries {
            store.forget(&e.id).unwrap();
        }
        assert_eq!(store.project_count().unwrap(), 2);
    }

    #[test]
    fn test_tag_count() {
        let store = test_store();

        // 初始空库
        assert_eq!(store.tag_count("rust").unwrap(), 0);
        assert_eq!(store.tag_count("python").unwrap(), 0);

        // 插入带标签的记忆
        store.save("async rust info", "note", "rust,async", "p", 5).unwrap();
        store.save("tokio runtime", "note", "rust,tokio", "p", 5).unwrap();
        store.save("python web framework", "note", "python,django", "p", 5).unwrap();
        store.save("python data science", "note", "python,numpy", "p", 5).unwrap();
        store.save("typescript types", "note", "ts,type", "p", 5).unwrap();

        // 按标签计数
        assert_eq!(store.tag_count("rust").unwrap(), 2, "rust 标签应有 2 条");
        assert_eq!(store.tag_count("python").unwrap(), 2, "python 标签应有 2 条");
        assert_eq!(store.tag_count("ts").unwrap(), 1, "ts 标签应有 1 条");
        assert_eq!(store.tag_count("tokio").unwrap(), 1, "tokio 标签应有 1 条");
        assert_eq!(store.tag_count("nonexistent").unwrap(), 0, "不存在的标签应返回 0");

        // 归档后不影响 tag_count
        let entries = store.list(Some("p")).unwrap();
        let to_forget = entries.iter().find(|e| e.tags.contains("ts")).unwrap();
        store.forget(&to_forget.id).unwrap();
        assert_eq!(store.tag_count("ts").unwrap(), 0, "归档后 ts 标签应返回 0");
        assert_eq!(store.tag_count("rust").unwrap(), 2, "归档不影响 rust 标签");
    }

    #[test]
    fn test_recent_entries() {
        let store = test_store();

        // 先清空
        store.clear_all().unwrap();

        // 保存 3 条，故意让时间戳不同
        store.save("entry one",   "note", "t", "p", 5).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        store.save("entry two",   "note", "t", "p", 5).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        store.save("entry three", "note", "t", "p", 5).unwrap();

        // 获取最近 2 条
        let recent = store.recent_entries(2).unwrap();
        assert_eq!(recent.len(), 2, "应返回 2 条");
        // 验证降序：先 three 后 two
        assert_eq!(recent[0].content, "entry three", "第 1 条应为最近保存的 three");
        assert_eq!(recent[1].content, "entry two",    "第 2 条应为 two");
        assert!(recent[0].created_at >= recent[1].created_at, "时间应降序");
    }

    #[test]
    fn test_has_content() {
        let store = test_store();

        // 先清空
        store.clear_all().unwrap();

        // 保存一条 content="hello world"
        store.save("hello world", "note", "t", "p", 5).unwrap();

        // 验证存在与不存在
        assert!(store.has_content("hello world").unwrap(), "已保存的内容应返回 true");
        assert!(!store.has_content("not exist").unwrap(), "不存在的 content 应返回 false");
    }

    #[test]
    fn test_archive_count() {
        let store = test_store();
        store.clear_all().unwrap();

        // 初始：archived=1 的数量为 0
        assert_eq!(store.archive_count().unwrap(), 0);

        // 保存 2 条
        let id1 = store.save("entry one", "note", "t", "p", 5).unwrap();
        let id2 = store.save("entry two", "note", "t", "p", 5).unwrap();

        // 归档前 archive_count 应为 0
        assert_eq!(store.archive_count().unwrap(), 0);

        // forget 一条 → archived=1
        store.forget(&id1).unwrap();

        // 断言 archive_count == 1
        assert_eq!(store.archive_count().unwrap(), 1);
    }

    #[test]
    fn test_oldest_entry_age() {
        let store = test_store();
        store.clear_all().unwrap();

        // 空表应返回 None
        assert_eq!(store.oldest_entry_age().unwrap(), None);

        // 保存一条，获取其 created_at
        store.save("first entry", "note", "t", "p", 5).unwrap();
        let ts = store.oldest_entry_age().unwrap();
        assert!(ts.is_some(), "保存一条后应返回 Some(ts)");
        let ts_val = ts.unwrap();
        assert!(ts_val > 0, "created_at 应为正数时间戳");

        // 再保存第二条条，最早时间戳不变
        store.save("second entry", "note", "t", "p", 5).unwrap();
        let ts2 = store.oldest_entry_age().unwrap().unwrap();
        assert_eq!(ts2, ts_val, "第二条条插入后最 created_at 不应改变");

        // clear_all 后应返回 None
        store.clear_all().unwrap();
        assert_eq!(store.oldest_entry_age().unwrap(), None, "清空后应返回 None");
    }

    #[test]
    fn test_count_by_tags() {
        let store = test_store();
        store.clear_all().unwrap();
        // save 3 条带不同 tags
        store.save("entry 1", "note", "rust,sqlite", "p", 5).unwrap();
        store.save("entry 2", "note", "rust,memory", "p", 5).unwrap();
        store.save("entry 3", "note", "python", "p", 5).unwrap();
        // 验证
        assert_eq!(store.count_by_tags("rust").unwrap(), 2, "含 rust tag 应有 2 条");
        assert_eq!(store.count_by_tags("sqlite").unwrap(), 1, "含 sqlite tag 应有 1 条");
        assert_eq!(store.count_by_tags("java").unwrap(), 0, "含 java tag 应有 0 条");
    }

    #[test]
    fn test_count_by_category() {
        let store = test_store();
        store.clear_all().unwrap();
        store.save("entry 1", "note", "t", "p", 5).unwrap();
        store.save("entry 2", "code", "t", "p", 5).unwrap();
        store.save("entry 3", "note", "t", "p", 5).unwrap();
        assert_eq!(store.count_by_category("note").unwrap(), 2, "category=note 应有 2 条");
        assert_eq!(store.count_by_category("code").unwrap(), 1, "category=code 应有 1 条");
        assert_eq!(store.count_by_category("doc").unwrap(), 0, "category=doc 应有 0 条");
    }

    #[test]
    fn test_find_oldest_by_project() {
        let store = test_store();
        store.clear_all().unwrap();

        // save 2 entries to project-a (sleep 1 sec between them)
        let id_a1 = store.save("project-a first", "note", "t", "project-a", 5).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(1));
        let id_a2 = store.save("project-a second", "note", "t", "project-a", 5).unwrap();

        // save 1 entry to project-b
        let id_b1 = store.save("project-b first", "note", "t", "project-b", 5).unwrap();

        // verify project-a returns the first one
        let oldest_a = store.find_oldest_by_project("project-a").unwrap().expect("project-a should have an entry");
        assert_eq!(oldest_a.id, id_a1, "oldest entry in project-a should be the first saved");
        assert_eq!(oldest_a.content, "project-a first");

        // verify project-b returns its only entry
        let oldest_b = store.find_oldest_by_project("project-b").unwrap().expect("project-b should have an entry");
        assert_eq!(oldest_b.id, id_b1, "oldest entry in project-b should be its only entry");
        assert_eq!(oldest_b.content, "project-b first");

        // verify non-existent project-c returns None
        let oldest_c = store.find_oldest_by_project("project-c").unwrap();
        assert!(oldest_c.is_none(), "project-c has no entries, should return None");
    }

    #[test]
    fn test_delete_by_project() {
        let store = test_store();
        store.clear_all().unwrap();

        // save 3 entries to project-a
        store.save("pa content 1", "note", "t", "project-a", 5).unwrap();
        store.save("pa content 2", "note", "t", "project-a", 5).unwrap();
        store.save("pa content 3", "note", "t", "project-a", 5).unwrap();

        // save 1 entry to project-b
        store.save("pb content 1", "note", "t", "project-b", 5).unwrap();

        // verify total count == 4
        assert_eq!(store.count().unwrap(), 4);

        // delete project-a, expect 3 rows deleted
        let deleted = store.delete_by_project("project-a").unwrap();
        assert_eq!(deleted, 3, "should delete 3 entries for project-a");

        // verify count() == 1 (project-b still there)
        assert_eq!(store.count().unwrap(), 1, "only project-b entries should remain");

        // verify count_by_project('project-a') == 0
        assert_eq!(store.count_by_project("project-a").unwrap(), 0, "project-a should have 0 active entries");
    }

    /// Test has_tag method:
    /// 1) clear_all, save entry with tags='rust,sqlite'
    /// 2) assert has_tag('rust') == true
    /// 3) assert has_tag('java') == false
    #[test]
    fn test_has_tag() {
        let store = test_store();
        store.clear_all().unwrap();
        store.save("entry with tags", "note", "rust,sqlite", "p", 5).unwrap();
        assert!(store.has_tag("rust").unwrap(), "has_tag('rust') should be true");
        assert!(!store.has_tag("java").unwrap(), "has_tag('java') should be false");
    }

    /// Test find_by_content_prefix method:
    /// 1) clear_all, save 'hello world', 'hello rust', 'goodbye'
    /// 2) assert find_by_content_prefix('hello').len() == 2
    /// 3) assert find_by_content_prefix('xyz').is_empty()
    #[test]
    fn test_find_by_content_prefix() {
        let store = test_store();
        store.clear_all().unwrap();
        store.save("hello world", "note", "t", "p", 5).unwrap();
        store.save("hello rust", "note", "t", "p", 5).unwrap();
        store.save("goodbye", "note", "t", "p", 5).unwrap();
        let results = store.find_by_content_prefix("hello").unwrap();
        assert_eq!(results.len(), 2, "should find 2 entries with 'hello' prefix");
        let results_empty = store.find_by_content_prefix("xyz").unwrap();
        assert!(results_empty.is_empty(), "should be empty for 'xyz' prefix");
    }

    /// Test update_importance method:
    /// 1) clear_all, save an entry, update its importance, re-fetch and verify
    /// 2) update_importance on non-existent id returns Err
    #[test]
    fn test_update_importance() {
        let store = test_store();
        store.clear_all().unwrap();

        // Save an entry and get its id
        let id = store.save("test importance update", "note", "t", "p", 5).unwrap();

        // Update importance to 9
        store.update_importance(&id, 9).unwrap();

        // Re-fetch and verify importance == 9
        let entries = store.list(None).unwrap();
        let entry = entries.iter().find(|e| e.id == id).expect("entry should exist");
        assert_eq!(entry.importance, 9, "importance should be updated to 9");

        // Update importance on non-existent id should return Err
        let result = store.update_importance("nonexistent", 5);
        assert!(result.is_err(), "update_importance on nonexistent id should return Err");
    }

    /// Test archive_by_project method:
    /// 1) clear_all; save 2 entries to proj-a, save 1 entry to proj-b
    /// 2) archive_by_project('proj-a') == 2
    /// 3) count_active_by_project('proj-a') == 0
    /// 4) count_active_by_project('proj-b') == 1
    #[test]
    fn test_archive_by_project() {
        let store = test_store();
        store.clear_all().unwrap();

        // save 2 to proj-a
        store.save("pa entry 1", "note", "t", "proj-a", 5).unwrap();
        store.save("pa entry 2", "note", "t", "proj-a", 5).unwrap();

        // save 1 to proj-b
        store.save("pb entry 1", "note", "t", "proj-b", 5).unwrap();

        // archive_by_project('proj-a') == 2
        let archived = store.archive_by_project("proj-a").unwrap();
        assert_eq!(archived, 2, "should archive 2 entries for proj-a");

        // count_active_by_project('proj-a') == 0
        assert_eq!(store.count_active_by_project("proj-a").unwrap(), 0, "proj-a should have 0 active entries");

        // count_active_by_project('proj-b') == 1
        assert_eq!(store.count_active_by_project("proj-b").unwrap(), 1, "proj-b should have 1 active entry");
    }

    /// Test count_archived method:
    /// clear_all; save 3; archive first (use forget); count_archived()==1;
    /// forget second; count_archived()==2
    #[test]
    fn test_count_archived() {
        let store = test_store();
        store.clear_all().unwrap();

        // Initial: no archived entries
        assert_eq!(store.count_archived().unwrap(), 0);

        // Save 3 entries
        let id1 = store.save("entry one", "note", "t", "p", 5).unwrap();
        let id2 = store.save("entry two", "note", "t", "p", 5).unwrap();
        let id3 = store.save("entry three", "note", "t", "p", 5).unwrap();

        // Archive first entry
        store.forget(&id1).unwrap();
        assert_eq!(store.count_archived().unwrap(), 1);

        // Archive second entry
        store.forget(&id2).unwrap();
        assert_eq!(store.count_archived().unwrap(), 2);

        // Third entry still active
        assert_eq!(store.count().unwrap(), 1);
    }

    /// Test find_duplicates method:
    /// 1) clear_all; save 'same content' twice; save 'unique' once
    /// 2) find_duplicates().len() >= 1 (at least one duplicate group)
    /// 3) each result entry's content == 'same content'
    #[test]
    fn test_find_duplicates() {
        let store = test_store();
        store.clear_all().unwrap();

        // Save duplicate content twice
        store.save("same content", "note", "t", "p", 5).unwrap();
        store.save("same content", "note", "t", "p", 5).unwrap();
        // Save unique content
        store.save("unique", "note", "t", "p", 5).unwrap();

        let result = store.find_duplicates().unwrap();
        assert!(result.len() >= 1, "should find at least one duplicate group");
        for entry in &result {
            assert_eq!(entry.content, "same content", "each duplicate entry should have content 'same content'");
        }
    }

    /// Test list_recent_by_project method:
    /// clear_all; save 3 to proj-a with 1s sleep between;
    /// result = list_recent_by_project('proj-a', 2);
    /// result.len()==2; result[0].created_at >= result[1].created_at (DESC order)
    #[test]
    fn test_list_recent_by_project() {
        let store = test_store();
        store.clear_all().unwrap();

        // Save 3 entries to proj-a with sleep between each
        store.save("entry 1", "note", "t", "proj-a", 5).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(1));
        store.save("entry 2", "note", "t", "proj-a", 5).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(1));
        store.save("entry 3", "note", "t", "proj-a", 5).unwrap();

        // Get recent 2 entries
        let result = store.list_recent_by_project("proj-a", 2).unwrap();
        assert_eq!(result.len(), 2, "should return 2 entries");
        // Verify DESC order: most recent first
        assert!(
            result[0].created_at >= result[1].created_at,
            "created_at should be in DESC order: {:?} >= {:?}",
            result[0].created_at,
            result[1].created_at
        );
        // Entry 3 was saved last, so it should be first
        assert_eq!(result[0].content, "entry 3", "most recent entry should be entry 3");
        // Entry 2 was saved before entry 3
        assert_eq!(result[1].content, "entry 2", "second most recent entry should be entry 2");
    }

    /// Test batch_save method:
    /// 1) clear_all; batch_save 3 entries
    /// 2) count() == 3; returned ids.len() == 3
    #[test]
    fn test_batch_save() {
        let store = test_store();
        store.clear_all().unwrap();

        let entries: Vec<(&str, &str, &str, &str, i32)> = vec![
            ("a", "note", "t", "p", 5),
            ("b", "note", "t", "p", 5),
            ("c", "note", "t", "p", 5),
        ];
        let ids = store.batch_save(entries).unwrap();

        assert_eq!(store.count().unwrap(), 3, "batch_save should save 3 entries");
        assert_eq!(ids.len(), 3, "batch_save should return 3 IDs");
    }

    /// Test import_json method:
    /// 1) clear_all; import valid JSON array
    /// 2) returned count == 1; count() == 1
    /// 3) import_json('invalid') returns Err
    #[test]
    fn test_import_json() {
        let store = test_store();
        store.clear_all().unwrap();

        // Valid JSON with one entry
        let json = r#"[{"content":"a","category":"note","tags":"t","project":"p","importance":5}]"#;
        let count = store.import_json(json).unwrap();
        assert_eq!(count, 1, "should import 1 entry");
        assert_eq!(store.count().unwrap(), 1, "store should have 1 entry");

        // Invalid JSON should return Err
        let result = store.import_json("invalid");
        assert!(result.is_err(), "invalid JSON should return Err");
    }

    /// Test export_json method:
    /// 1) clear_all; save 2 to proj-a; save 1 to proj-b
    /// 2) export_json(Some('proj-a')) returns JSON array of 2 entries
    /// 3) export_json(None) returns JSON array of 3 entries
    #[test]
    fn test_export_json() {
        let store = test_store();
        store.clear_all().unwrap();

        // Save 2 entries to proj-a
        store.save("content a1", "note", "t", "proj-a", 5).unwrap();
        store.save("content a2", "note", "t", "proj-a", 5).unwrap();

        // Save 1 entry to proj-b
        store.save("content b1", "note", "t", "proj-b", 5).unwrap();

        // Filter by proj-a: should get 2 entries
        let json = store.export_json(Some("proj-a")).unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.len(), 2, "proj-a should have 2 entries");

        // No filter: should get all 3 entries
        let all = store.export_json(None).unwrap();
        let parsed_all: Vec<serde_json::Value> = serde_json::from_str(&all).unwrap();
        assert_eq!(parsed_all.len(), 3, "no filter should return all 3 entries");

        // Verify field structure
        for entry in &parsed_all {
            assert!(entry.get("id").is_some(), "entry should have id field");
            assert!(entry.get("content").is_some(), "entry should have content field");
            assert!(entry.get("category").is_some(), "entry should have category field");
            assert!(entry.get("project").is_some(), "entry should have project field");
            assert!(entry.get("archived").is_some(), "entry should have archived field");
            assert!(entry.get("created_at").is_some(), "entry should have created_at field");
        }
    }

    /// Test oldest_by_importance:
    /// - clear_all
    /// - save 3 entries with importance 5, 1, 9
    /// - call oldest_by_importance, verify returned entry has importance == 1
    #[test]
    fn test_oldest_by_importance() {
        let store = test_store();
        store.clear_all().unwrap();

        // Save 3 entries with different importance values
        store.save("entry with importance 5", "note", "t", "p", 5).unwrap();
        store.save("entry with importance 1", "note", "t", "p", 1).unwrap();
        store.save("entry with importance 9", "note", "t", "p", 9).unwrap();

        // Call oldest_by_importance — should return the entry with importance == 1
        let result = store.oldest_by_importance().unwrap();
        assert!(result.is_some(), "should return an entry when table is non-empty");
        let entry = result.unwrap();
        assert_eq!(entry.importance, 1, "lowest importance entry should have importance == 1");
    }

    /// Test find_by_importance_range method:
    /// - clear_all
    /// - save entries with importance 1, 5, 5, 9, 10
    /// - find_by_importance_range(5, 9) should return 3 entries (5, 5, 9) in DESC order
    /// - find_by_importance_range(1, 10) should return all 5 entries in DESC order
    /// - find_by_importance_range(0, 0) should return 0 entries (no entry has importance 0)
    #[test]
    fn test_find_by_importance_range() {
        let store = test_store();
        store.clear_all().unwrap();

        // Save 5 entries with various importance values
        store.save("entry importance 1", "note", "t", "p", 1).unwrap();
        store.save("entry importance 5 first", "note", "t", "p", 5).unwrap();
        store.save("entry importance 5 second", "note", "t", "p", 5).unwrap();
        store.save("entry importance 9", "note", "t", "p", 9).unwrap();
        store.save("entry importance 10", "note", "t", "p", 10).unwrap();

        // Range [5, 9] should return 3 entries: 9, 5, 5 (DESC order)
        let results = store.find_by_importance_range(5, 9).unwrap();
        assert_eq!(results.len(), 3, "range [5,9] should return 3 entries");
        assert_eq!(results[0].importance, 9, "first result should have importance 9");
        assert_eq!(results[1].importance, 5, "second result should have importance 5");
        assert_eq!(results[2].importance, 5, "third result should have importance 5");

        // Range [1, 10] should return all 5 entries in DESC order
        let results_all = store.find_by_importance_range(1, 10).unwrap();
        assert_eq!(results_all.len(), 5, "range [1,10] should return all 5 entries");
        assert_eq!(results_all[0].importance, 10, "first result should have importance 10");
        assert_eq!(results_all[4].importance, 1, "last result should have importance 1");

        // Range [0, 0] should return 0 entries (no entry has importance 0)
        let results_empty = store.find_by_importance_range(0, 0).unwrap();
        assert_eq!(results_empty.len(), 0, "range [0,0] should return 0 entries");

        // Single-value range [9, 9] should return exactly 1 entry
        let results_single = store.find_by_importance_range(9, 9).unwrap();
        assert_eq!(results_single.len(), 1, "range [9,9] should return 1 entry");
        assert_eq!(results_single[0].importance, 9);
    }

    /// Test update_category method:
    /// 1) clear_all; save an entry with category "old_cat"
    /// 2) update_category to "new_cat"
    /// 3) re-fetch and verify category == "new_cat"
    /// 4) update_category on non-existent id returns Err
    #[test]
    fn test_update_category() {
        let store = test_store();
        store.clear_all().unwrap();

        // Save an entry with initial category
        let id = store.save("test category update", "old_cat", "t", "p", 5).unwrap();

        // Update category to "new_cat"
        store.update_category(&id, "new_cat").unwrap();

        // Re-fetch and verify category == "new_cat"
        let entries = store.list(None).unwrap();
        let entry = entries.iter().find(|e| e.id == id).expect("entry should exist");
        assert_eq!(entry.category, "new_cat", "category should be updated to 'new_cat'");

        // update_category on non-existent id should return Err
        let result = store.update_category("nonexistent", "whatever");
        assert!(result.is_err(), "update_category on nonexistent id should return Err");
    }
}
