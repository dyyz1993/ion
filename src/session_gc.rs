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

use crate::session_index::SessionIndex;
use std::path::{Path, PathBuf};

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
    if !deleted_sids.is_empty() {
        let mut index = SessionIndex::load();
        let before = index.len();
        for sid in &deleted_sids {
            index.remove(sid);
        }
        if index.len() != before {
            index.save();
        }
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
