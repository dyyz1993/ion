//! Session GC — clean up old session files to prevent unbounded growth.
//!
//! Triggered once at session start (async, non-blocking), mirroring
//! `file_snapshot/gc.rs`. Strategy, applied per cwd session dir:
//!   1. Delete `*.jsonl` files older than `max_age_days` (by mtime).
//!   2. LRU: keep at most `max_sessions_per_cwd`, delete oldest beyond that.
//!   3. Remove orphaned empty cwd dirs.
//!   4. Sync `sessions.index.json` (drop entries whose files were deleted).
//!
//! The active cwd's session dir is always protected. All age decisions use
//! file mtime (robust against stale index entries).

use crate::session_index::{SessionIndex, SessionMeta};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// 索引逆向对账（H2）：磁盘 JSONL → 索引缺失条目重建
//
// GC 只做"索引→文件"的单向删除；sessions.index.json 只会落后于 JSONL
// （tmp+rename 原子写，被杀最多丢最后几次统计），但索引损坏/被删/落后
// 大量条目时，磁盘上的会话成为幽灵——UI list_sessions 永远不可见。
// `reconcile_missing` 补上反方向：启动时扫磁盘，把索引缺失的会话从
// JSONL header 行重建回索引。三条纪律：
//   1. 只补缺不删多（删除仍归 GC，避免与 GC 竞态）
//   2. 尊重墓碑（removed_sessions 里的 id 不重建——防幽灵复活机制不被绕过）
//   3. header-only：只读每个文件的第一行，不全量解析（2000+ 文件时差异集
//      通常为空，未命中索引的文件才付一次 read_line 的代价）
// ---------------------------------------------------------------------------

/// 逆向对账结果（可观察：serve 启动日志 / 未来可挂 get_index_health）。
#[derive(Debug, Default, Clone)]
pub struct ReconcileReport {
    /// 磁盘上扫描到的 *.jsonl 总数
    pub disk_files: usize,
    /// 重建（补进索引）的条数
    pub rebuilt: usize,
    /// 命中墓碑被跳过的候选数
    pub skipped_tombstoned: usize,
    /// 首行不是合法 session header 的文件数（不 panic、不进索引）
    pub unreadable: usize,
}

/// 逆向对账：扫 `sessions_dir`（含各 cwd 子目录与根下平铺文件）的 *.jsonl，
/// 对索引缺失的会话读 header 行重建最小 SessionMeta 并写事务落盘。
///
/// 幂等：差异集为空时不触发写事务（二次跑索引文件逐字节不变）。
/// 幂等 + 只补缺：与 GC（索引→文件删除）方向互补，互不越界。
pub fn reconcile_missing(sessions_dir: &Path) -> ReconcileReport {
    let mut report = ReconcileReport::default();
    let index = SessionIndex::load();
    let known: std::collections::HashSet<&str> =
        index.sessions.keys().map(|s| s.as_str()).collect();

    let mut candidates: Vec<(crate::session_jsonl::SessionHeader, i64)> = Vec::new();
    for (path, stem) in iter_jsonl_files(sessions_dir) {
        report.disk_files += 1;
        // 性能：文件名即 sid（<sid>.jsonl）且已在索引 → 连 header 都不用读。
        // 共享名 session.jsonl 的 id 只能从 header 拿，总是读一行。
        if let Some(stem) = stem.as_deref() {
            if stem != "session" && known.contains(stem) {
                continue;
            }
        }
        let mtime_ms = file_mtime_ms(&path);
        match crate::session_jsonl::read_session_header(&path) {
            Some(h) => {
                if known.contains(h.id.as_str()) {
                    continue;
                }
                candidates.push((h, mtime_ms));
            }
            None => {
                report.unreadable += 1;
            }
        }
    }

    // 零差异 → 不进写事务（幂等的强形式：连落盘都不发生）
    if candidates.is_empty() {
        return report;
    }

    SessionIndex::write_txn(|idx| {
        for (h, mtime_ms) in &candidates {
            // 墓碑守卫：被 session_remove/GC 显式删除的 sid 不重建
            if idx.removed_sessions.contains(&h.id) {
                report.skipped_tombstoned += 1;
                continue;
            }
            // 竞态兜底：load 快照之后、写事务之前有并发创建（flock 内重查）
            if idx.sessions.contains_key(&h.id) {
                continue;
            }
            let meta = meta_from_header(h, *mtime_ms);
            idx.sessions.insert(h.id.clone(), meta);
            report.rebuilt += 1;
        }
    });

    if report.rebuilt > 0 || report.skipped_tombstoned > 0 {
        tracing::info!(
            "[session-reconcile] rebuilt {} missing index entries (disk_files={} tombstoned_skipped={} unreadable={})",
            report.rebuilt,
            report.disk_files,
            report.skipped_tombstoned,
            report.unreadable
        );
    }
    report
}

/// 枚举 sessions_dir 下的 *.jsonl：各 cwd 子目录顶层 + 根下平铺文件。
/// 不递归进 data/（扩展 session 级数据，无会话 JSONL）。返回 (路径, 文件名 stem)。
fn iter_jsonl_files(sessions_dir: &Path) -> Vec<(PathBuf, Option<String>)> {
    let mut out = Vec::new();
    let Ok(top) = std::fs::read_dir(sessions_dir) else {
        return out;
    };
    for entry in top.flatten() {
        let p = entry.path();
        if p.is_dir() {
            if let Ok(files) = std::fs::read_dir(&p) {
                for f in files.flatten() {
                    let fp = f.path();
                    if fp.extension().is_some_and(|e| e == "jsonl") {
                        let stem =
                            fp.file_stem().and_then(|s| s.to_str()).map(String::from);
                        out.push((fp, stem));
                    }
                }
            }
        } else if p.extension().is_some_and(|e| e == "jsonl") {
            let stem = p.file_stem().and_then(|s| s.to_str()).map(String::from);
            out.push((p, stem));
        }
    }
    out.sort();
    out
}

/// 从 JSONL header 行派生最小 SessionMeta。header 里没有的字段留空/default
/// （name/branch/统计全 0）——对账只承诺"会话可见可定位"，统计类字段由
/// 后续正常使用路径（patch_meta / list_all_sessions heal）逐步补全。
fn meta_from_header(h: &crate::session_jsonl::SessionHeader, mtime_ms: i64) -> SessionMeta {
    let created = parse_iso_ms(&h.timestamp).unwrap_or(mtime_ms);
    let project_name = std::path::Path::new(&h.cwd)
        .file_name()
        .map(|n| n.to_string_lossy().to_string());
    SessionMeta {
        name: None,
        first_name: None,
        project: Some(h.cwd.clone()),
        project_name,
        worktree: false,
        branch: None,
        model: h.model.clone().unwrap_or_default(),
        agent: h
            .agent
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "default".to_string()),
        provider: h.provider.clone().unwrap_or_default(),
        token_input: 0,
        token_output: 0,
        token_cache_read: 0,
        token_cache_write: 0,
        user_prompt_count: 0,
        llm_request_count: 0,
        total_duration_ms: 0,
        compress_count: 0,
        message_count: 0,
        turn_count: 0,
        created_at: created,
        updated_at: mtime_ms.max(created),
        error_count: 0,
        last_thinking_level: None,
        last_active_tools: None,
        last_entry_id: None,
        parent_session: h.parent_session.clone(),
        parent_type: None,
        initial_cwd: None,
        last_cwd: None,
        extra_cwds: Vec::new(),
        tier_models: None,
        security_profile: None,
        workspace_path: None,
        workspace_status: None,
        goal_status: None,
        goal_deadline_ms: None,
    }
}

/// 文件 mtime → epoch ms（失败回落到当前时间，保证 updated_at 有界）。
fn file_mtime_ms(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64
        })
}

/// 解析 ISO-8601 时间戳（`YYYY-MM-DDTHH:MM:SS[.fff…][Z|±HH[:MM]]`）→ epoch ms。
/// 覆盖 session_jsonl::timestamp_iso 的产出形态与常见 RFC3339 变体；
/// 解析失败返回 None（调用方回落 mtime）。
fn parse_iso_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, sec) = (
        num(0..4)?,
        num(5..7)?,
        num(8..10)?,
        num(11..13)?,
        num(14..16)?,
        num(17..19)?,
    );
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    // days_from_civil（Howard Hinnant 算法）：civil 日期 → 自 1970-01-01 的天数
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let mut total_ms = days * 86_400_000 + h * 3_600_000 + mi * 60_000 + sec * 1000;

    let mut i = 19usize;
    // 小数秒（截到毫秒；更细精度跳过）
    if i < b.len() && b[i] == b'.' {
        i += 1;
        let mut frac = 0i64;
        let mut nd = 0usize;
        while i < b.len() && b[i].is_ascii_digit() && nd < 3 {
            frac = frac * 10 + (b[i] - b'0') as i64;
            i += 1;
            nd += 1;
        }
        if nd == 0 {
            return None;
        }
        while nd < 3 {
            frac *= 10;
            nd += 1;
        }
        total_ms += frac;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    }
    // 时区：Z / ±HH[:MM]（无后缀按 UTC）
    if i < b.len() {
        match b[i] {
            b'Z' | b'z' => i += 1,
            b'+' | b'-' => {
                let sign = if b[i] == b'-' { -1 } else { 1 };
                i += 1;
                let oh: i64 = s.get(i..i + 2)?.parse().ok()?;
                i += 2;
                let mut om = 0i64;
                if i < b.len() && b[i] == b':' {
                    om = s.get(i + 1..i + 3)?.parse().ok()?;
                    i += 3;
                }
                total_ms -= sign * (oh * 3_600_000 + om * 60_000);
            }
            _ => return None,
        }
    }
    if i != b.len() {
        return None;
    }
    Some(total_ms)
}

/// Session GC configuration (mirrors the `session` block in config.json).
#[derive(Clone, Debug)]
pub struct SessionGcConfig {
    /// Max age in days; files older than this (by mtime) are deleted.
    pub max_age_days: u32,
    /// Max sessions per cwd dir; oldest beyond this are LRU-deleted.
    pub max_sessions_per_cwd: u32,
    /// If false, skip GC entirely.
    pub gc_on_start: bool,
}

impl Default for SessionGcConfig {
    fn default() -> Self {
        Self {
            max_age_days: 30,
            max_sessions_per_cwd: 50,
            gc_on_start: true,
        }
    }
}

/// Run session GC. `active_cwd` is protected: its session files are never deleted.
/// Safe to call from a background thread; best-effort, logs errors but never panics.
pub fn run_gc(config: &SessionGcConfig, active_cwd: &str) {
    if !config.gc_on_start {
        return;
    }

    let sessions_dir = crate::paths::sessions_dir();
    if !sessions_dir.is_dir() {
        return;
    }

    let active_dir_name = crate::paths::encode_path(active_cwd);
    let max_age = std::time::Duration::from_secs((config.max_age_days as u64) * 86400);
    let now = std::time::SystemTime::now();

    let mut deleted_sids: Vec<String> = Vec::new();
    let mut total_deleted = 0usize;

    let cwd_dirs = match std::fs::read_dir(&sessions_dir) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("[session-gc] cannot read sessions dir: {e}");
            return;
        }
    };

    for entry in cwd_dirs.flatten() {
        let cwd_dir = entry.path();
        if !cwd_dir.is_dir() {
            continue;
        }
        // Never touch the active cwd's session dir.
        if cwd_dir
            .file_name()
            .map(|n| n == active_dir_name.as_str())
            .unwrap_or(false)
        {
            continue;
        }

        // Collect all *.jsonl: (path, mtime, header_id).
        let mut files = collect_jsonl_files(&cwd_dir);

        if files.is_empty() {
            // Empty cwd dir → remove it (orphan cleanup).
            let _ = std::fs::remove_dir(&cwd_dir);
            continue;
        }

        // Sort oldest-first (mtime asc) so LRU deletion of the leading slice is correct.
        files.sort_by(|a, b| a.1.cmp(&b.1));

        // Single pass: delete a file if (a) older than max_age, OR
        // (b) it's beyond the per-cwd LRU cap (keep the newest max_sessions_per_cwd).
        let keep_cap = config.max_sessions_per_cwd as usize;
        // Index in the oldest-first list; files at the TAIL (newest) are kept.
        let total = files.len();
        for (idx, (fp, mtime, sid)) in files.iter().enumerate() {
            let age = now.duration_since(*mtime).unwrap_or_default();
            let beyond_cap = total > keep_cap && idx < total - keep_cap;
            if age > max_age || beyond_cap {
                if std::fs::remove_file(fp).is_ok() {
                    total_deleted += 1;
                    if let Some(s) = sid {
                        deleted_sids.push(s.clone());
                    }
                }
            }
        }

        // If dir is now empty, remove it.
        if std::fs::read_dir(&cwd_dir)
            .map(|mut d| d.next().is_none())
            .unwrap_or(false)
        {
            let _ = std::fs::remove_dir(&cwd_dir);
        }
    }

    // Sync index: remove entries for deleted sessions.
    // 写事务 + 墓碑：GC 删除同样不能被迟到的 patch 复活。
    if !deleted_sids.is_empty() {
        SessionIndex::write_txn(|index| {
            for sid in &deleted_sids {
                index.sessions.remove(sid);
                index.removed_sessions.insert(sid.clone());
            }
        });
    }

    if total_deleted > 0 {
        tracing::info!(
            "[session-gc] deleted {} session files ({} index entries), max_age={}d max_per_cwd={}",
            total_deleted,
            deleted_sids.len(),
            config.max_age_days,
            config.max_sessions_per_cwd
        );
    }
}

/// Collect (path, mtime, header_id) for all *.jsonl in a dir.
fn collect_jsonl_files(dir: &Path) -> Vec<(PathBuf, std::time::SystemTime, Option<String>)> {
    let mut out = Vec::new();
    if let Ok(files) = std::fs::read_dir(dir) {
        for f in files.flatten() {
            let fp = f.path();
            if !fp.extension().is_some_and(|e| e == "jsonl") {
                continue;
            }
            let mtime = fp
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            let sid = read_header_id(&fp);
            out.push((fp, mtime, sid));
        }
    }
    out
}

/// Best-effort read of the header `id` field from a JSONL file's first line.
fn read_header_id(path: &Path) -> Option<String> {
    use std::io::{BufRead, BufReader};
    let file = std::fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut first_line = String::new();
    if reader.read_line(&mut first_line).ok()? == 0 {
        return None;
    }
    let val: serde_json::Value = serde_json::from_str(first_line.trim()).ok()?;
    val.get("id")?.as_str().map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_iso_ms_handles_timestamp_iso_output_shape() {
        // timestamp_iso() 产出形态："YYYY-MM-DDTHH:MM:SS.mmmZ"
        assert_eq!(
            parse_iso_ms("2025-06-01T12:00:00.000Z"),
            Some(1_748_779_200_000)
        );
        // 无小数秒
        assert_eq!(parse_iso_ms("2025-06-01T12:00:00Z"), Some(1_748_779_200_000));
        // epoch 起点
        assert_eq!(parse_iso_ms("1970-01-01T00:00:00Z"), Some(0));
        // 时区偏移：+08:00 的 20:00 == UTC 12:00
        assert_eq!(
            parse_iso_ms("2025-06-01T20:00:00+08:00"),
            Some(1_748_779_200_000)
        );
        // 负偏移：UTC 12:00 == -05:00 的 07:00
        assert_eq!(
            parse_iso_ms("2025-06-01T07:00:00-05:00"),
            Some(1_748_779_200_000)
        );
        // 亚毫秒精度截断
        assert_eq!(
            parse_iso_ms("2025-06-01T12:00:00.123456Z"),
            Some(1_748_779_200_123)
        );
        // 闰日
        assert_eq!(parse_iso_ms("2024-02-29T00:00:00Z"), Some(1_709_164_800_000));
        // 非法输入
        assert_eq!(parse_iso_ms("not-a-timestamp"), None);
        assert_eq!(parse_iso_ms("2025-13-01T00:00:00Z"), None);
        assert_eq!(parse_iso_ms("2025-06-01T12:00:00."), None); // 小数点后无数字
        assert_eq!(parse_iso_ms("2025-06-01T12:00:00ZZ"), None); // 尾部多余字符
    }

    #[test]
    fn parse_iso_ms_accepts_no_suffix_utc() {
        // 19 字符裸日期时间按 UTC 处理（宽松变体）
        assert_eq!(parse_iso_ms("2025-06-01T12:00:00"), Some(1_748_779_200_000));
    }

    fn make_test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ion_session_gc_{}_{}_{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    /// Write a fake session JSONL. `age_secs` backdates mtime via std::fs (touch).
    /// We avoid the `filetime` crate (not a dep); instead we just don't backdate
    /// when age_secs==0, and for age tests we set a very small max_age to force
    /// deletion of freshly-written files.
    fn write_session(dir: &Path, sid: &str) -> PathBuf {
        let path = dir.join(format!("{sid}.jsonl"));
        let header = format!("{{\"type\":\"session\",\"id\":\"{sid}\",\"cwd\":\"test\"}}\nbody\n");
        std::fs::write(&path, header).unwrap();
        path
    }

    #[test]
    fn collect_jsonl_files_lists_all_jsonl() {
        let dir = make_test_dir("collect");
        write_session(&dir, "sess_a");
        write_session(&dir, "sess_b");
        // Non-jsonl file should be ignored.
        std::fs::write(dir.join("notes.txt"), "ignore me").unwrap();

        let files = collect_jsonl_files(&dir);
        assert_eq!(files.len(), 2, "should list exactly the 2 .jsonl files");
        for (_, _, sid) in &files {
            assert!(sid.is_some(), "header id should be parsed");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_header_id_parses_session_id() {
        let dir = make_test_dir("header");
        let path = write_session(&dir, "sess_abc123");
        let sid = read_header_id(&path);
        assert_eq!(sid.as_deref(), Some("sess_abc123"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_header_id_returns_none_for_missing_file() {
        assert_eq!(read_header_id(Path::new("/nonexistent/never.jsonl")), None);
    }

    #[test]
    fn gc_skips_when_disabled() {
        // gc_on_start=false → run_gc is a no-op (no panic, no file access needed).
        let config = SessionGcConfig {
            gc_on_start: false,
            ..Default::default()
        };
        run_gc(&config, "/nonexistent/cwd/that/is/protected");
        // No panic = pass. (We can't assert deletion since sessions_dir may be live.)
    }

    #[test]
    fn lru_keeps_newest_n_in_sorted_list() {
        // Validate the core LRU indexing logic without real files.
        // Simulate 5 files sorted oldest-first; cap=3 → delete idx 0,1; keep 2,3,4.
        let total = 5usize;
        let keep_cap = 3usize;
        let mut delete_count = 0;
        for idx in 0..total {
            let beyond_cap = total > keep_cap && idx < total - keep_cap;
            if beyond_cap {
                delete_count += 1;
            }
        }
        assert_eq!(delete_count, total - keep_cap);
        assert_eq!(delete_count, 2);
    }

    #[test]
    fn run_gc_deletes_old_and_lru_in_isolated_dir() {
        let _guard = crate::paths::env_test_lock();
        // End-to-end run_gc test against an isolated sessions dir (via
        // ION_SESSION_DIR). Each test gets a unique dir to avoid parallel runs
        // clobbering each other; we restore the env var at the end.
        let tmp = std::env::temp_dir().join(format!(
            "ion_gc_e2e_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let sessions = tmp.join("sessions");
        let cwd1 = sessions.join("cwd1");
        let cwd2 = sessions.join("cwd2");
        std::fs::create_dir_all(&cwd1).unwrap();
        std::fs::create_dir_all(&cwd2).unwrap();

        // cwd1: 1 ancient file (year 2000) + 1 fresh.
        std::fs::write(
            cwd1.join("sess_ancient.jsonl"),
            "{\"type\":\"session\",\"id\":\"sess_ancient\",\"cwd\":\"/x\"}\n",
        )
        .unwrap();
        let ancient = cwd1.join("sess_ancient.jsonl");
        // Backdate to year 2000 via std::time → set_modified.
        let y2k = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(946684800);
        let _ = std::fs::File::options()
            .write(true)
            .open(&ancient)
            .and_then(|f| {
                f.set_modified(y2k.into())
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
            });
        std::fs::write(
            cwd1.join("sess_fresh.jsonl"),
            "{\"type\":\"session\",\"id\":\"sess_fresh\",\"cwd\":\"/x\"}\n",
        )
        .unwrap();

        // cwd2: 3 recent files, cap at 2 → oldest 1 LRU-deleted. All same recent
        // mtime (written sequentially, so slight ordering by write time).
        for i in 0..3u32 {
            std::fs::write(
                cwd2.join(format!("sess_c2_{i}.jsonl")),
                format!("{{\"type\":\"session\",\"id\":\"sess_c2_{i}\",\"cwd\":\"/x\"}}\n"),
            )
            .unwrap();
        }

        // Point sessions_dir at our tmp root.
        let prev = std::env::var("ION_SESSION_DIR").ok();
        unsafe {
            std::env::set_var("ION_SESSION_DIR", &sessions);
        }

        let config = SessionGcConfig {
            max_age_days: 30,
            max_sessions_per_cwd: 2,
            gc_on_start: true,
        };
        // active_cwd encoded won't match cwd1/cwd2, so neither is "protected".
        run_gc(&config, "/some/other/cwd");

        // Restore env.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("ION_SESSION_DIR", v),
                None => std::env::remove_var("ION_SESSION_DIR"),
            }
        }

        // cwd1: ancient deleted, fresh kept.
        assert!(!ancient.exists(), "ancient file should be deleted by age");
        assert!(
            cwd1.join("sess_fresh.jsonl").exists(),
            "fresh file should survive"
        );
        // cwd2: 3 → 2 (LRU kept newest 2).
        let remaining = collect_jsonl_files(&cwd2);
        assert_eq!(remaining.len(), 2, "cwd2 should be LRU-trimmed to 2");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
