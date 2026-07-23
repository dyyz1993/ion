//! File Time Guard Extension
//!
//! Tracks file modification times (mtime + size) whenever the agent reads a
//! file, and warns or blocks writes/edits to files that were modified
//! externally (e.g. by the user's IDE) since the agent's last read.
//!
//! Aligned with pi's `extensions/file-time-guard/` design:
//! - Records file mtime/size when the agent reads a file (after `read` tool).
//! - Before `write`/`edit`, checks if the file changed since the last read.
//! - Three modes: `Block` (reject the write), `Warn` (allow but log), `Ignore`.
//! - Ignores configured paths (target/, .git/, node_modules/).
//!
//! Safety: if a file does not exist or `std::fs::metadata` fails, the write is
//! always allowed (fail-open). This keeps the agent from getting stuck on
//! transient filesystem errors.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::agent::error::{AgentError, AgentResult};
use crate::agent::extension::Extension;
use crate::agent::messages::ToolCall;
use ion_provider::types::ToolResult;

// ---------------------------------------------------------------------------
// Data structures
// ---------------------------------------------------------------------------

/// A recorded snapshot of a file's mtime and size at the moment the agent
/// last read it. Used to detect external modifications before a write.
#[derive(Clone, Debug)]
pub struct FileSnapshot {
    /// The file path this snapshot refers to.
    pub path: String,
    /// Modified time in seconds since the UNIX epoch.
    pub mtime: u64,
    /// File size in bytes.
    pub size: u64,
    /// Wall-clock time (secs since epoch) when this snapshot was recorded.
    pub recorded_at: u64,
}

/// The guard mode controls how the extension reacts to a stale file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuardMode {
    /// Reject the write with an error so the agent must re-read first.
    Block,
    /// Allow the write but emit a warning to stderr.
    Warn,
    /// Do nothing — effectively disables the guard.
    Ignore,
}

impl GuardMode {
    /// Lowercase string representation used in the `status` RPC response.
    fn as_str(&self) -> &'static str {
        match self {
            GuardMode::Block => "block",
            GuardMode::Warn => "warn",
            GuardMode::Ignore => "ignore",
        }
    }
}

/// V1 configuration with hardcoded defaults. No config file is needed.
#[derive(Clone, Debug)]
pub struct FileTimeGuardConfig {
    /// How the guard reacts to externally-modified files.
    pub mode: GuardMode,
    /// Path fragments that are never checked (substring match).
    /// Defaults to common generated/vendored directories.
    pub ignore_paths: Vec<String>,
}

impl Default for FileTimeGuardConfig {
    fn default() -> Self {
        Self {
            // Warn is the safe default: surface the problem without breaking
            // the agent loop in existing workflows.
            mode: GuardMode::Warn,
            ignore_paths: vec![
                "target/".into(),
                ".git/".into(),
                "node_modules/".into(),
            ],
        }
    }
}

/// File Time Guard Extension.
///
/// Holds an in-memory map of file path -> last-known snapshot. The map is
/// populated from `read` results and consulted before `write`/`edit`.
pub struct FileTimeGuardExtension {
    /// file path -> last-known mtime/size snapshot.
    snapshots: Arc<Mutex<HashMap<String, FileSnapshot>>>,
    /// Hardcoded V1 configuration.
    config: FileTimeGuardConfig,
    /// Extension name used for RPC routing.
    name: String,
}

impl FileTimeGuardExtension {
    /// Create a new extension with the default (Warn) configuration.
    pub fn new() -> Self {
        Self::with_config(FileTimeGuardConfig::default())
    }

    /// Create a new extension with an explicit configuration (used by tests).
    pub fn with_config(config: FileTimeGuardConfig) -> Self {
        Self {
            snapshots: Arc::new(Mutex::new(HashMap::new())),
            config,
            name: "file-time-guard".into(),
        }
    }

    /// Return true if `path` matches any of the configured ignore fragments.
    /// Matching is a simple substring check (e.g. "target/", ".git/").
    fn is_ignored(&self, path: &str) -> bool {
        self.config
            .ignore_paths
            .iter()
            .any(|frag| path.contains(frag.as_str()))
    }

    /// Read the current mtime (secs since epoch) and size (bytes) of `path`.
    /// Returns `None` if the file does not exist or metadata cannot be read
    /// (fail-open: callers treat `None` as "no information, allow the write").
    fn current_meta(path: &str) -> Option<(u64, u64)> {
        let meta = std::fs::metadata(path).ok()?;
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Some((mtime, meta.len()))
    }

    /// Record a snapshot for `path` from the live filesystem.
    /// Silently skips if the file cannot be stat'd.
    pub async fn record(&self, path: &str) {
        let Some((mtime, size)) = Self::current_meta(path) else {
            return;
        };
        let recorded_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut map = self.snapshots.lock().await;
        map.insert(
            path.to_string(),
            FileSnapshot {
                path: path.to_string(),
                mtime,
                size,
                recorded_at,
            },
        );
    }

    /// Check whether `path` is stale relative to its last recorded snapshot.
    ///
    /// Returns `Some(reason)` when the file changed since the last read,
    /// `None` when the file is fresh, untracked, or cannot be stat'd
    /// (fail-open).
    pub async fn check_stale(&self, path: &str) -> Option<String> {
        if self.is_ignored(path) {
            return None;
        }
        let map = self.snapshots.lock().await;
        let snap = match map.get(path) {
            Some(s) => s.clone(),
            None => return None, // never read by the agent -> nothing to compare
        };
        drop(map); // release the lock before touching the filesystem

        let Some((cur_mtime, cur_size)) = Self::current_meta(path) else {
            return None; // file gone or unreadable -> allow the write
        };
        if cur_mtime != snap.mtime {
            Some(format!(
                "mtime changed ({} -> {})",
                snap.mtime, cur_mtime
            ))
        } else if cur_size != snap.size {
            Some(format!(
                "size changed ({} -> {})",
                snap.size, cur_size
            ))
        } else {
            None
        }
    }
}

impl Default for FileTimeGuardExtension {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Extension for FileTimeGuardExtension {
    fn name(&self) -> &str {
        &self.name
    }

    /// Before `write`/`edit`: detect external modifications and apply the
    /// configured guard mode (Block / Warn / Ignore).
    async fn before_tool_call(&self, call: &ToolCall) -> AgentResult<()> {
        // Only intercept tools that mutate files.
        if call.name != "write" && call.name != "edit" {
            return Ok(());
        }
        let Some(path) = extract_path(&call.arguments) else {
            return Ok(()); // no path to guard -> allow
        };

        // Skip ignore-listed paths entirely.
        if self.is_ignored(path) {
            return Ok(());
        }

        let Some(reason) = self.check_stale(path).await else {
            return Ok(()); // fresh or untracked -> allow
        };

        let msg = format!(
            "file-time-guard: '{}' was modified externally since last read ({}); \
             re-read it first to avoid clobbering fresh edits",
            path, reason
        );
        match self.config.mode {
            GuardMode::Block => Err(AgentError::Tool(msg)),
            GuardMode::Warn => {
                eprintln!("[file-time-guard] WARNING: {msg}");
                Ok(())
            }
            GuardMode::Ignore => Ok(()),
        }
    }

    /// After `read`: record the file's current mtime/size so future writes
    /// can detect external modifications.
    async fn after_tool_call(&self, call: &ToolCall, _result: &ToolResult) -> AgentResult<()> {
        if call.name != "read" {
            return Ok(());
        }
        if let Some(path) = extract_path(&call.arguments) {
            self.record(path).await;
        }
        Ok(())
    }

    /// RPC methods:
    /// - `"status"`: return the current guard mode + number of tracked files.
    /// - `"check"`: given `{"path": "..."}`, return whether the file is stale.
    async fn on_extension_rpc(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> AgentResult<serde_json::Value> {
        match method {
            "status" => {
                let count = self.snapshots.lock().await.len();
                Ok(serde_json::json!({
                    "mode": self.config.mode.as_str(),
                    "tracked_files": count,
                }))
            }
            "check" => {
                let path = params
                    .get("path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let stale = self.check_stale(path).await;
                Ok(serde_json::json!({
                    "path": path,
                    "stale": stale.is_some(),
                    "reason": stale.unwrap_or_default(),
                }))
            }
            // Unknown method: return the sentinel error so the registry can
            // continue dispatching to the next extension.
            _ => Err(AgentError::Tool(
                "extension rpc method not found".into(),
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract a file path from tool arguments.
///
/// Tools use either `file_path` (read/write/edit) or `path` as the argument
/// key. Returns the first one that is present and a string.
fn extract_path(args: &serde_json::Value) -> Option<&str> {
    args.get("file_path")
        .and_then(|v| v.as_str())
        .or_else(|| args.get("path").and_then(|v| v.as_str()))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a unique temp file path and write initial content to it.
    fn make_tmp_file(tag: &str, content: &str) -> String {
        let id = uuid::Uuid::new_v4().to_string()[..8].to_string();
        let path = format!("/tmp/ftg_{tag}_{id}.txt");
        std::fs::write(&path, content).expect("write temp file");
        path
    }

    /// Force a distinct mtime by sleeping past the filesystem's mtime
    /// granularity (commonly 1 second).
    fn bump_mtime() {
        std::thread::sleep(std::time::Duration::from_secs(2));
    }

    #[tokio::test]
    async fn test_record_and_check_fresh() {
        let path = make_tmp_file("fresh", "hello");
        let ext = FileTimeGuardExtension::new();

        // Record the snapshot, then immediately check — file is unchanged.
        ext.record(&path).await;
        let stale = ext.check_stale(&path).await;
        assert!(stale.is_none(), "freshly recorded file should not be stale");

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn test_detect_external_modification() {
        let path = make_tmp_file("mod", "v1");
        let ext = FileTimeGuardExtension::new();

        ext.record(&path).await;

        // Simulate an external edit (different content + new mtime).
        bump_mtime();
        std::fs::write(&path, "v2 with more bytes").unwrap();

        let stale = ext.check_stale(&path).await;
        assert!(stale.is_some(), "externally modified file should be stale");
        let reason = stale.unwrap();
        assert!(
            reason.contains("mtime") || reason.contains("size"),
            "reason should mention mtime or size: {reason}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn test_ignore_paths() {
        // A path under target/ should never be flagged even if it changed.
        let dir = "/tmp/ftg_ignore_target";
        let _ = std::fs::create_dir_all(dir);
        let path = format!("{}/dummy.rs", dir);
        std::fs::write(&path, "a").unwrap();

        let ext = FileTimeGuardExtension::new();
        ext.record(&path).await;
        bump_mtime();
        std::fs::write(&path, "much longer content to change size").unwrap();

        // The default ignore list contains "target/", and the path contains
        // "target" as a substring of the directory name.
        assert!(ext.is_ignored(&path), "path under target/ should be ignored");
        let stale = ext.check_stale(&path).await;
        assert!(stale.is_none(), "ignored path should never be stale");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn test_guard_mode_block() {
        let path = make_tmp_file("block", "original");
        let ext = FileTimeGuardExtension::with_config(FileTimeGuardConfig {
            mode: GuardMode::Block,
            ignore_paths: FileTimeGuardConfig::default().ignore_paths,
        });

        ext.record(&path).await;
        bump_mtime();
        std::fs::write(&path, "externally changed content").unwrap();

        // Build a write ToolCall targeting the now-stale file.
        let call = ToolCall {
            call_type: "function".into(),
            id: "tc_block".into(),
            name: "write".into(),
            arguments: serde_json::json!({ "file_path": path }),
            thought_signature: None,
        };
        let result = ext.before_tool_call(&call).await;
        assert!(result.is_err(), "Block mode should reject a stale write");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("modified externally"),
            "error should explain the staleness: {err}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn test_guard_mode_warn() {
        let path = make_tmp_file("warn", "original");
        let ext = FileTimeGuardExtension::with_config(FileTimeGuardConfig {
            mode: GuardMode::Warn,
            ignore_paths: FileTimeGuardConfig::default().ignore_paths,
        });

        ext.record(&path).await;
        bump_mtime();
        std::fs::write(&path, "externally changed content").unwrap();

        let call = ToolCall {
            call_type: "function".into(),
            id: "tc_warn".into(),
            name: "edit".into(),
            arguments: serde_json::json!({ "file_path": path }),
            thought_signature: None,
        };
        // Warn mode must allow the write (Ok) even though the file is stale.
        let result = ext.before_tool_call(&call).await;
        assert!(result.is_ok(), "Warn mode should allow a stale write");

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn test_after_tool_call_records_read() {
        let path = make_tmp_file("read", "read me");
        let ext = FileTimeGuardExtension::new();

        let call = ToolCall {
            call_type: "function".into(),
            id: "tc_read".into(),
            name: "read".into(),
            arguments: serde_json::json!({ "file_path": path }),
            thought_signature: None,
        };
        let result = ToolResult {
            tool_call_id: "tc_read".into(),
            output: "read me".into(),
        };
        ext.after_tool_call(&call, &result).await.unwrap();

        // The file should now be tracked.
        let status = ext.on_extension_rpc("status", serde_json::json!({})).await.unwrap();
        assert_eq!(status["tracked_files"], 1, "read should track the file");

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn test_status_and_check_rpc() {
        let path = make_tmp_file("rpc", "data");
        let ext = FileTimeGuardExtension::new();
        ext.record(&path).await;

        let status = ext.on_extension_rpc("status", serde_json::json!({})).await.unwrap();
        assert_eq!(status["mode"], "warn");
        assert_eq!(status["tracked_files"], 1);

        // Fresh check via RPC.
        let chk = ext
            .on_extension_rpc("check", serde_json::json!({ "path": path }))
            .await
            .unwrap();
        assert_eq!(chk["stale"], false);

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn test_unknown_rpc_returns_not_found() {
        let ext = FileTimeGuardExtension::new();
        let res = ext.on_extension_rpc("nope", serde_json::json!({})).await;
        // The sentinel error lets the registry try the next extension.
        assert!(res.is_err());
        assert_eq!(res.unwrap_err().to_string(), "Tool call failed: extension rpc method not found");
    }
}
