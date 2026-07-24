//! file-time-guard — WASM extension that tracks file modification markers.
//!
//! This extension records a lightweight "freshness marker" for every file
//! targeted by a `write` or `edit` tool call, then exposes that information
//! over the extension RPC channel so callers can detect stale writes.
//!
//! ABI: ION WASM extension v1
//! Target: wasm32-wasip1 (std-enabled)
//!
//! Build:
//!   cargo build --target wasm32-wasip1 --release
//! Install:
//!   cp target/wasm32-wasip1/release/file_time_guard_wasm.wasm \
//!      ~/.ion/agent/extensions/

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

// ── Host functions provided by the ION WASM runtime ─────────────────────────
//
// These are imported from the "env" module. The runtime links them before the
// extension's `_start` / init runs.

extern "C" {
    /// Check whether a path exists inside the allowed roots.
    /// Return codes: 1 = exists, 0 = not found, 2 = traversal blocked.
    fn host_path_exists(path_ptr: *const u8, path_len: u32) -> u32;

    /// Read up to `out_cap` bytes of a file into `out_buf`.
    /// Returns the number of bytes written; 0 means not found / error.
    fn host_read_file(
        path_ptr: *const u8,
        path_len: u32,
        out_buf: *mut u8,
        out_cap: u32,
    ) -> u32;

    /// Emit a textual message to the host event stream (no return value).
    fn host_send_message(msg_ptr: *const u8, msg_len: u32);
}

// ── Global guard state ───────────────────────────────────────────────────────
//
// A process-wide map from absolute/canonical file path to a freshness marker.
// The marker is currently the byte length of the file's contents at the time
// it was first observed; this is enough to detect "the file changed since we
// last looked" in the common case. A proper hash can replace this later.

/// A recorded freshness snapshot for a single file.
#[derive(Clone, Copy, Debug)]
struct FileMarker {
    /// Number of bytes the file had when we first recorded it.
    size: u64,
    /// Number of times before_tool_call observed this path.
    observations: u64,
}

fn state() -> &'static Mutex<HashMap<String, FileMarker>> {
    static STATE: OnceLock<Mutex<HashMap<String, FileMarker>>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(HashMap::new()))
}

// ── Memory helpers ───────────────────────────────────────────────────────────
//
// The host hands us WASM-linear-memory offsets as u32. We treat them as raw
// pointers (valid inside this module's own memory) and copy bytes around.

/// Read a UTF-8 string from a (ptr, len) pair inside WASM memory.
fn read_wasm_string(ptr: u32, len: u32) -> String {
    unsafe {
        let slice = std::slice::from_raw_parts(ptr as *const u8, len as usize);
        String::from_utf8_lossy(slice).to_string()
    }
}

/// Copy `s` into the host-provided output buffer, truncating to `out_cap`.
/// Returns the number of bytes actually written.
fn write_wasm_string(s: &str, out_buf: u32, out_cap: u32) -> u32 {
    let bytes = s.as_bytes();
    let len = bytes.len().min(out_cap as usize);
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), out_buf as *mut u8, len);
    }
    len as u32
}

/// Thin wrapper around the raw `host_send_message` import.
fn send_message(msg: &str) {
    unsafe { host_send_message(msg.as_ptr(), msg.len() as u32); }
}

/// Thin wrapper around `host_path_exists`. Returns true if the path exists.
fn path_exists(path: &str) -> bool {
    unsafe { host_path_exists(path.as_ptr(), path.len() as u32) == 1 }
}

/// Read a file via the host and return its contents as a `String`.
/// Returns `None` if the file is missing, unreadable, or larger than the
/// scratch buffer.
fn read_host_file(path: &str) -> Option<String> {
    // Scratch buffer for file contents. 256 KiB is plenty for source files;
    // larger files simply won't be tracked (we treat them as "no marker").
    let mut buf = [0u8; 262_144];
    let n = unsafe {
        host_read_file(
            path.as_ptr(),
            path.len() as u32,
            buf.as_mut_ptr(),
            buf.len() as u32,
        )
    };
    if n == 0 {
        return None;
    }
    Some(String::from_utf8_lossy(&buf[..n as usize]).to_string())
}

/// Compute a cheap, deterministic marker for a file's contents.
///
/// We combine the byte length with a folded sum of all bytes. This is not
/// cryptographically strong, but it is stable, allocation-free, and good
/// enough to flag "the file changed since we last saw it".
fn content_marker(contents: &str) -> u64 {
    let bytes = contents.as_bytes();
    let mut acc: u64 = bytes.len() as u64;
    // Mix in every byte. Wrapping add keeps this branch-free and panic-free.
    for &b in bytes {
        acc = acc.wrapping_mul(31).wrapping_add(b as u64);
    }
    acc
}

// ── ABI entry points ─────────────────────────────────────────────────────────

/// Return the extension ABI version. Must be `1`.
#[no_mangle]
pub extern "C" fn extension_version() -> u32 {
    1
}

/// Called once when the host loads the extension.
#[no_mangle]
pub extern "C" fn extension_init() {
    send_message("file-time-guard initialized");
}

/// before_tool_call hook — record freshness markers for write/edit targets.
///
/// Signature: `extension_before_tool_call(json_ptr, json_len) -> u32`
/// Input JSON shape:
///   {"name":"write","arguments":{"path":"/some/file.rs"}, ...}
/// Return codes:
///   0 = allow (always — this hook only records, never blocks)
///   non-zero would mean block, but we never use it in this version.
///
/// For every `write`/`edit` call we:
///   1. extract the target path from `arguments.path`,
///   2. check via the host whether the file currently exists,
///   3. if it does, read it and store a content marker + observation count.
/// New files (path does not exist yet) are also recorded with an empty marker
/// so that a later `check` RPC can confirm whether they were created.
#[no_mangle]
pub extern "C" fn extension_before_tool_call(json_ptr: u32, json_len: u32) -> u32 {
    let raw = read_wasm_string(json_ptr, json_len);

    // Parse the hook payload. Malformed JSON is ignored (we never block).
    let parsed: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return 0,
    };

    // Only write/edit tools carry a file path we care about.
    let name = parsed.get("name").and_then(|v| v.as_str()).unwrap_or("");
    if name != "write" && name != "edit" {
        return 0;
    }

    // The path may live under "arguments.path" or directly under "path".
    let path = parsed
        .get("arguments")
        .and_then(|a| a.get("path"))
        .or_else(|| parsed.get("path"))
        .and_then(|p| p.as_str());
    let path = match path {
        Some(p) if !p.is_empty() => p,
        _ => return 0, // nothing to record
    };

    // Probe the host filesystem for the current state of the file.
    let exists = path_exists(path);
    let marker = if exists {
        // File is present — snapshot its contents so we can detect later edits.
        match read_host_file(path) {
            Some(contents) => content_marker(&contents),
            None => 0, // exists but unreadable; treat as empty marker
        }
    } else {
        // File does not exist yet — this write will create it.
        0
    };

    // Record / update the entry. We bump the observation counter every time.
    if let Ok(mut map) = state().lock() {
        let entry = map.entry(path.to_string()).or_insert(FileMarker {
            size: 0,
            observations: 0,
        });
        entry.size = marker;
        entry.observations = entry.observations.saturating_add(1);
    }

    // Never block — this is a recording-only phase.
    0
}

/// on_rpc hook — query guard status.
///
/// Signature:
///   extension_on_rpc(method_ptr, method_len, params_ptr, params_len,
///                    out_buf, out_cap) -> u32
/// Methods:
///   "status" — returns {"tracked": <count>}
///   "check"  — given {"path": "..."}, returns {"path": "...", "tracked": bool,
///                 "modified": bool, "observations": u64}
///   "reset"  — clears all tracked files, returns {"cleared": <count>}
/// The return value is the number of bytes written to `out_buf`; 0 on error.
#[no_mangle]
pub extern "C" fn extension_on_rpc(
    method_ptr: u32,
    method_len: u32,
    params_ptr: u32,
    params_len: u32,
    out_buf: u32,
    out_cap: u32,
) -> u32 {
    let method = read_wasm_string(method_ptr, method_len);
    let params_raw = read_wasm_string(params_ptr, params_len);
    let params: serde_json::Value =
        serde_json::from_str(&params_raw).unwrap_or(serde_json::Value::Null);

    let response = match method.as_str() {
        "status" => {
            let count = state()
                .lock()
                .map(|m| m.len())
                .unwrap_or(0);
            serde_json::json!({ "tracked": count })
        }

        "check" => {
            let path = params.get("path").and_then(|v| v.as_str()).unwrap_or("");
            if path.is_empty() {
                serde_json::json!({ "error": "missing 'path'" })
            } else {
                // Look up our last recorded marker for this file.
                let known = state()
                    .lock()
                    .ok()
                    .and_then(|m| m.get(path).copied());

                match known {
                    Some(rec) => {
                        // Re-read the file and compare markers to detect change.
                        let current = if path_exists(path) {
                            read_host_file(path)
                                .map(|c| content_marker(&c))
                                .unwrap_or(rec.size)
                        } else {
                            // File vanished since we recorded it.
                            0
                        };
                        serde_json::json!({
                            "path": path,
                            "tracked": true,
                            "modified": current != rec.size,
                            "observations": rec.observations,
                        })
                    }
                    None => {
                        serde_json::json!({
                            "path": path,
                            "tracked": false,
                            "modified": false,
                            "observations": 0,
                        })
                    }
                }
            }
        }

        "reset" => {
            let cleared = state()
                .lock()
                .map(|mut m| {
                    let n = m.len();
                    m.clear();
                    n
                })
                .unwrap_or(0);
            serde_json::json!({ "cleared": cleared })
        }

        _ => serde_json::json!({ "error": format!("unknown method: {}", method) }),
    };

    let body = match serde_json::to_string(&response) {
        Ok(s) => s,
        Err(_) => return 0,
    };
    write_wasm_string(&body, out_buf, out_cap)
}

// ── Unit tests ───────────────────────────────────────────────────────────────
//
// These run under the native target (cargo test) and exercise the pure
// helper logic without touching host imports.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_marker_is_stable() {
        let a = content_marker("hello world");
        let b = content_marker("hello world");
        assert_eq!(a, b, "identical inputs must produce identical markers");
    }

    #[test]
    fn content_marker_detects_change() {
        let a = content_marker("hello world");
        let b = content_marker("hello WORLD");
        assert_ne!(a, b, "different inputs must produce different markers");
    }

    #[test]
    fn content_marker_empty() {
        // Empty input should still yield a deterministic, non-panicking value.
        let m = content_marker("");
        assert_eq!(m, 0, "empty string marker should be zero (len 0, no bytes)");
    }

    #[test]
    fn state_is_shared() {
        // The OnceLock guarantees a single global map across all callers.
        let a = state() as *const _;
        let b = state() as *const _;
        assert_eq!(a, b, "state() must return the same singleton each call");
    }

    #[test]
    fn state_records_and_clears() {
        let mut map = state().lock().unwrap();
        let before = map.len();
        map.insert(
            "/tmp/file-time-guard-test.rs".to_string(),
            FileMarker { size: 42, observations: 1 },
        );
        assert_eq!(map.len(), before + 1);
        map.clear();
        assert_eq!(map.len(), 0);
    }

    // NOTE: write_wasm_string / read_wasm_string treat their first argument
    // as a 32-bit WASM linear-memory offset, so they can only be exercised
    // meaningfully under the wasm32 target. The pure-logic helpers above
    // (content_marker, state) are covered by the native tests.
}
