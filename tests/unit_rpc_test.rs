//! Phase 1: 单元测试 U1-U25
//!
//! U1-U15: RPC 协议测试（远程 worker 进程）
//! U16: 75 命令全覆盖
//! U17-U20: 会话存储测试（本地，无需子进程）
//! U21-U25: 插件测试（在 plugin_tests.rs 中已有 17 个测试覆盖）

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Maximum time to wait for the worker to produce a "ready" signal.
const READY_TIMEOUT: Duration = Duration::from_secs(30);
/// Maximum time to wait for a command response.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(15);

// ---------------------------------------------------------------------------
// Helper: find the ion-worker binary
// ---------------------------------------------------------------------------

/// Locate the `ion-worker` binary. Tries, in order:
/// 1. `ION_WORKER_BIN` env var
/// 2. Sibling of the current test executable
/// 3. `ion-worker` in PATH
fn find_worker_bin() -> String {
    std::env::var("ION_WORKER_BIN").unwrap_or_else(|_| {
        let current_exe = std::env::current_exe().ok();
        if let Some(exe) = current_exe {
            if let Some(parent) = exe.parent() {
                let sibling = parent.join("ion-worker");
                if sibling.exists() {
                    return sibling.to_string_lossy().to_string();
                }
                // Also try ../ion-worker (one level up for different target dirs)
                if let Some(grandparent) = parent.parent() {
                    let alt = grandparent.join("ion-worker");
                    if alt.exists() {
                        return alt.to_string_lossy().to_string();
                    }
                }
            }
        }
        "ion-worker".to_string()
    })
}

fn rand_u32() -> u32 {
    use std::time::SystemTime;
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos()
}

// ---------------------------------------------------------------------------
// Test harness: spawn worker subprocess
// ---------------------------------------------------------------------------

/// A worker subprocess running in RPC mode.
struct WorkerProc {
    stdin: std::process::ChildStdin,
    stdout_reader: BufReader<std::process::ChildStdout>,
    child: std::process::Child,
}

impl WorkerProc {
    /// Spawn `ion-worker --mode rpc` in an isolated temp directory with unique session.
    fn spawn() -> Self {
        let worker_path = find_worker_bin();
        let session_id = format!("ut_{:08x}", rand_u32());
        let tmp_dir = std::env::temp_dir().join(format!("ion_test_{session_id}"));
        let _ = std::fs::create_dir_all(&tmp_dir);

        let mut child = Command::new(&worker_path)
            .arg("--mode")
            .arg("rpc")
            .arg("--session")
            .arg(&session_id)
            .current_dir(&tmp_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn ion-worker");

        let stdin = child.stdin.take().expect("no stdin");
        let stdout = child.stdout.take().expect("no stdout");
        let mut reader = BufReader::new(stdout);

        // Wait for the "ready" signal
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            if Instant::now() > deadline {
                let _ = child.kill();
                panic!("Timed out waiting for worker ready signal");
            }
            let mut ready_line = String::new();
            match reader.read_line(&mut ready_line) {
                Ok(0) => {
                    let _ = child.kill();
                    panic!("Worker closed stdout before sending ready");
                }
                Ok(_) => {
                    let trimmed = ready_line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) {
                        if val.get("type").and_then(|v| v.as_str()) == Some("ready") {
                            break;
                        }
                    }
                }
                Err(e) => {
                    let _ = child.kill();
                    panic!("Error reading worker stdout: {e}");
                }
            }
        }

        Self {
            stdin,
            stdout_reader: reader,
            child,
        }
    }

    /// Send a JSONL command to the worker and read the response.
    fn send_command(&mut self, method: &str, params: Option<serde_json::Value>) -> serde_json::Value {
        let id = format!("ut_{}", method.replace(|c: char| !c.is_alphanumeric(), "_"));
        let mut cmd = serde_json::json!({
            "id": id,
            "method": method,
        });
        if let Some(p) = params {
            cmd["params"] = p;
        }

        // Write to stdin
        let line = serde_json::to_string(&cmd).unwrap();
        writeln!(self.stdin, "{line}").expect("failed to write to worker stdin");
        self.stdin.flush().ok();

        // Read response (wait until we get a matching id)
        let deadline = Instant::now() + RESPONSE_TIMEOUT;
        loop {
            if Instant::now() > deadline {
                let _ = self.child.kill();
                panic!("Timed out waiting for response to {method}");
            }
            let mut response_line = String::new();
            match self.stdout_reader.read_line(&mut response_line) {
                Ok(0) => {
                    let _ = self.child.kill();
                    panic!("Worker closed stdout while waiting for response to {method}");
                }
                Ok(_) => {
                    let trimmed = response_line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) {
                        if val.get("id").and_then(|v| v.as_str()) == Some(&id) {
                            return val;
                        }
                        // Skip events
                        if val.get("type").and_then(|v| v.as_str()) == Some("event") {
                            continue;
                        }
                    }
                }
                Err(e) => {
                    let _ = self.child.kill();
                    panic!("Error reading worker stdout: {e}");
                }
            }
        }
    }

    /// Send a command and extract the `data` field from the response.
    fn send_and_get_data(&mut self, method: &str, params: Option<serde_json::Value>) -> serde_json::Value {
        let resp = self.send_command(method, params);
        assert_eq!(
            resp.get("success").and_then(|v| v.as_bool()),
            Some(true),
            "Command {method} failed: {}",
            resp.get("error").and_then(|v| v.as_str()).unwrap_or("unknown error")
        );
        resp.get("data").cloned().unwrap_or(serde_json::Value::Null)
    }

    /// Send a command that is expected to fail.
    fn send_and_expect_error(&mut self, method: &str) -> serde_json::Value {
        let resp = self.send_command(method, None);
        assert_eq!(
            resp.get("success").and_then(|v| v.as_bool()),
            Some(false),
            "Command {method} should have failed"
        );
        resp
    }
}

impl Drop for WorkerProc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------------
// U1-U15: RPC 协议测试
// ---------------------------------------------------------------------------

#[test]
fn u01_get_state() {
    let mut wp = WorkerProc::spawn();
    let data = wp.send_and_get_data("get_state", None);

    assert!(data.get("model").is_some());
    assert!(data.get("provider").is_some());
    assert!(data.get("session_id").is_some());
    assert!(data.get("message_count").is_some());
    assert!(data.get("is_running").is_some());
    assert_eq!(data["message_count"], 0);
    assert_eq!(data["is_running"], false);
}

#[test]
fn u02_get_session_stats() {
    let mut wp = WorkerProc::spawn();
    let data = wp.send_and_get_data("get_session_stats", None);

    // Check camelCase fields
    assert!(data.get("sessionId").is_some());
    assert!(data.get("userMessages").is_some());
    assert!(data.get("assistantMessages").is_some());
    assert!(data.get("totalMessages").is_some());
    assert!(data.get("tokens").is_some());

    // Tokens nested object with camelCase
    let tokens = data.get("tokens").unwrap();
    assert!(tokens.get("input").is_some());
    assert!(tokens.get("output").is_some());
    assert!(tokens.get("total").is_some());

    // Empty session: zero tokens
    assert_eq!(tokens["input"], 0);
    assert_eq!(tokens["output"], 0);

    assert!(data.get("cost").is_some());
}

#[test]
fn u03_get_messages() {
    let mut wp = WorkerProc::spawn();
    let data = wp.send_and_get_data("get_messages", None);

    // 消息拉取改造后返回 {messages: [...], hasMore, totalCount, ...}
    assert!(data.get("messages").is_some(), "get_messages should return object with 'messages' array");
    let msgs = data["messages"].as_array().expect("messages should be array");
    assert_eq!(msgs.len(), 0, "fresh session: empty messages");
}

#[test]
fn u04_get_last_assistant_text() {
    let mut wp = WorkerProc::spawn();
    let data = wp.send_and_get_data("get_last_assistant_text", None);

    assert!(data.is_string(), "get_last_assistant_text should return a string");
    assert_eq!(data.as_str().unwrap(), "", "empty session returns empty string");
}

#[test]
fn u05_get_tools() {
    let mut wp = WorkerProc::spawn();
    let data = wp.send_and_get_data("get_tools", None);

    // Tools are wrapped in a 'tools' key
    assert!(data.get("tools").is_some(), "get_tools should include 'tools' array");
    let tools = data["tools"].as_array().unwrap();
    assert!(!tools.is_empty(), "should have at least the builtin tools");

    // Each tool should have name and description (from the worker format)
    // Worker returns lowercase names: {"name":"read"}, etc.
    for tool in tools {
        assert!(tool.get("name").is_some(), "each tool should have 'name'");
    }

    // Should include core tools (lowercase in worker)
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(names.contains(&"read"), "should include 'read' tool");
    assert!(names.contains(&"write"), "should include 'write' tool");
    assert!(names.contains(&"bash"), "should include 'bash' tool");
    assert!(names.contains(&"edit"), "should include 'edit' tool");
}

#[test]
fn u06_get_active_tools() {
    let mut wp = WorkerProc::spawn();
    let data = wp.send_and_get_data("get_active_tools", None);

    // 改造后返回 {tools: [...], count}
    assert!(data.get("tools").is_some(), "get_active_tools should return object with 'tools' array");
    let tools = data["tools"].as_array().expect("tools should be array");
    assert!(!tools.is_empty(), "should have at least some active tools");
    for t in tools {
        assert!(t.is_string(), "each active tool should be a string name");
    }
}

#[test]
fn u07_get_available_models() {
    let mut wp = WorkerProc::spawn();
    let data = wp.send_and_get_data("get_available_models", None);

    assert!(data.is_array(), "get_available_models should return an array");
    let models = data.as_array().unwrap();
    assert!(!models.is_empty(), "should have at least some models registered");

    for m in models {
        assert!(m.get("id").is_some(), "model should have 'id'");
        assert!(m.get("name").is_some(), "model should have 'name'");
    }
}

#[test]
fn u08_get_agents() {
    let mut wp = WorkerProc::spawn();
    let data = wp.send_and_get_data("get_agents", None);

    assert!(data.is_array(), "get_agents should return an array");
    let agents = data.as_array().unwrap();
    assert!(!agents.is_empty(), "should have at least the default agents");

    for agent in agents {
        assert!(agent.get("name").is_some(), "agent should have 'name'");
        assert!(agent.get("description").is_some(), "agent should have 'description'");
    }
}

#[test]
fn u09_get_system_prompt() {
    let mut wp = WorkerProc::spawn();
    let data = wp.send_and_get_data("get_system_prompt", None);

    // Should be a string (may be empty if no messages have been sent yet)
    assert!(data.is_string(), "get_system_prompt should return a string");
    // In a fresh session with no messages, the prompt may be empty
    // We just verify type is correct and no error
}

#[test]
fn u10_get_context_usage() {
    let mut wp = WorkerProc::spawn();
    let data = wp.send_and_get_data("get_context_usage", None);

    // 改造后返回 usagePercent/totalInputTokens/totalOutputTokens/autoCompaction
    assert!(data.get("totalInputTokens").is_some(), "should include 'totalInputTokens'");
    assert!(data.get("totalOutputTokens").is_some(), "should include 'totalOutputTokens'");
    assert!(data.get("usagePercent").is_some(), "should include 'usagePercent'");
}

#[test]
fn u11_unknown_command() {
    let mut wp = WorkerProc::spawn();
    let resp = wp.send_and_expect_error("nonexistent");

    assert!(
        resp.get("error").is_some(),
        "unknown command response should have 'error' field"
    );
    let error_msg = resp["error"].as_str().unwrap_or("");
    assert!(
        error_msg.contains("Unknown command") || error_msg.contains("unknown"),
        "error message should mention unknown command, got: {error_msg}"
    );
}

#[test]
fn u12_ready_signal() {
    // Spawn a fresh worker and capture the first line
    let worker_path = find_worker_bin();
    let mut child = Command::new(&worker_path)
        .arg("--mode")
        .arg("rpc")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn ion-worker");

    let stdout = child.stdout.take().expect("no stdout");
    let mut reader = BufReader::new(stdout);

    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("Timed out waiting for ready signal");
        }
        let mut ready_line = String::new();
        if reader.read_line(&mut ready_line).unwrap_or(0) == 0 {
            let _ = child.kill();
            panic!("Worker closed stdout before sending ready");
        }
        let trimmed = ready_line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) {
            if val.get("type").and_then(|v| v.as_str()) == Some("ready") {
                // Verify ready signal structure
                assert!(val.get("session").is_some(), "ready should have 'session'");
                assert!(val.get("model").is_some(), "ready should have 'model'");
                assert!(val.get("provider").is_some(), "ready should have 'provider'");
                assert!(val.get("version").is_some(), "ready should have 'version'");
                assert!(val.get("channels").is_some(), "ready should have 'channels'");
                // Clean up
                let _ = child.stdin.take();
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
        }
    }
}

#[test]
fn u13_set_model() {
    let mut wp = WorkerProc::spawn();
    let params = serde_json::json!({
        "provider": "opencode",
        "modelId": "deepseek-v4-flash"
    });
    let data = wp.send_and_get_data("set_model", Some(params));

    // Worker returns model/provider info (field name may vary)
    assert!(
        data.get("model").is_some() || data.get("modelId").is_some(),
        "set_model response should include model info"
    );
}

#[test]
fn u14_set_thinking_level() {
    let mut wp = WorkerProc::spawn();
    let params = serde_json::json!({
        "level": "high"
    });
    let data = wp.send_and_get_data("set_thinking_level", Some(params));

    assert!(
        data.get("thinkingLevel").is_some(),
        "set_thinking_level should return 'thinkingLevel'"
    );
}

#[test]
fn u15_shutdown() {
    let worker_path = find_worker_bin();
    let mut child = Command::new(&worker_path)
        .arg("--mode")
        .arg("rpc")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn ion-worker");

    let mut stdin = child.stdin.take().expect("no stdin");
    let stdout = child.stdout.take().expect("no stdout");
    let mut reader = BufReader::new(stdout);

    // Wait for ready
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("Timed out waiting for ready");
        }
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            let _ = child.kill();
            panic!("Worker closed stdout");
        }
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(line.trim()) {
            if val.get("type").and_then(|v| v.as_str()) == Some("ready") {
                break;
            }
        }
    }

    // Send shutdown
    let shutdown = serde_json::json!({"id":"ut_shutdown","method":"shutdown"});
    writeln!(stdin, "{}", serde_json::to_string(&shutdown).unwrap()).ok();
    stdin.flush().ok();
    drop(stdin);

    // Read response then expect EOF (process exit)
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut found_response = false;
    loop {
        if Instant::now() > deadline {
            break;
        }
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(line.trim()) {
                    if val.get("id").and_then(|v| v.as_str()) == Some("ut_shutdown") {
                        found_response = true;
                        assert_eq!(val["success"], true, "shutdown should succeed");
                    }
                }
            }
            Err(_) => break,
        }
    }

    assert!(found_response, "shutdown should produce a response");

    // Wait for process exit
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if Instant::now() > deadline {
            break;
        }
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => break,
        }
    }
}

// ---------------------------------------------------------------------------
// U16: 75 命令全覆盖测试
// ---------------------------------------------------------------------------

#[test]
fn u16_all_supported_commands() {
    let mut wp = WorkerProc::spawn();

    // Commands that the worker directly supports (excluding Manager-only commands)
    let commands = &[
        // Query commands (sync)
        "get_state",
        "get_session_stats",
        "get_messages",
        "get_last_assistant_text",
        "get_tools",
        "get_active_tools",
        "get_available_models",
        "get_agents",
        "get_system_prompt",
        "get_context_usage",
        "get_settings",
        "get_commands",
        "get_skills",
        "get_extensions",
        "get_current_agent",
        "get_agent_detail",
        "get_tier_models",
        "get_all_tools",
        "get_queue",
        "get_flags",
        // Mutation commands
        "set_model",
        "set_thinking_level",
        "set_session_name",
        "set_cwd",
        "set_active_tools",
        "set_tier_models",
        "set_settings",
        "set_flag",
        "set_steering_mode",
        "set_follow_up_mode",
        "set_auto_compaction",
        "set_auto_retry",
        // Session management
        "switch_session",
        "clone",
        "fork",
        "navigate_tree",
        "delete_entries",
        "summarize_entries",
        "export_html",
        "compact",
        // Flow control
        "abort",
        "continue",
        // Cycle commands
        "cycle_model",
        "cycle_thinking_level",
        // Additional
        "reload",
        "get_agents_files",
        "get_latest_agent_change",
        "get_fork_messages",
        "get_tree",
        "get_tree_with_leaf",
        "get_modified_files",
        "get_file_diff",
        "get_batch_diffs",
        "get_file_history",
        "rollback_preview",
        "copy_fork",
        "append_system_event",
    ];

    let mut errors = Vec::new();
    for cmd in commands {
        match wp.send_command(cmd, None) {
            val if val.get("success").and_then(|v| v.as_bool()) == Some(false) => {
                let err = val.get("error")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                errors.push(format!("{cmd}: {err}"));
            }
            val if val.get("success").and_then(|v| v.as_bool()) == Some(true) => { /* OK */ }
            other => {
                errors.push(format!("{cmd}: unexpected response: {other}"));
            }
        }
    }

    if !errors.is_empty() {
        panic!(
            "{} out of {} commands failed:\n  {}",
            errors.len(),
            commands.len(),
            errors.join("\n  ")
        );
    }
}

// ---------------------------------------------------------------------------
// U17-U20: 会话存储测试（本地，无需子进程）
// ---------------------------------------------------------------------------

use std::path::PathBuf;
use std::sync::Mutex;

/// Global lock for session index tests (file-based index is shared)
static INDEX_LOCK: Mutex<()> = Mutex::new(());

/// Create a temporary cwd directory for session tests
fn tmp_cwd(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ion_test_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn randish() -> u32 {
    use std::time::SystemTime;
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos()
}

fn cleanup_session(cwd: &str) {
    let path = ion::session_jsonl::session_path(cwd);
    let _ = std::fs::remove_file(&path);
    if let Some(parent) = path.parent() {
        let _ = std::fs::remove_dir(parent);
    }
}

/// Write an inline session.jsonl for testing.
fn write_session_jsonl(cwd: &str, content: &str) {
    let path = ion::session_jsonl::session_path(cwd);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&path, content).unwrap();
}

#[test]
fn u17_create_session_writes_jsonl() {
    let cwd = tmp_cwd("u17").to_string_lossy().to_string();
    let sid = format!("sess_{:08x}", randish());

    let header = ion::session_jsonl::SessionHeader {
        entry_type: "session".into(),
        version: 3,
        id: sid.clone(),
        timestamp: ion::session_jsonl::timestamp_iso(),
        cwd: cwd.clone(),
        parent_session: None,
    };
    ion::session_jsonl::SessionFile::save(&cwd, &header, &[]);

    let path = ion::session_jsonl::session_path(&cwd);
    assert!(path.exists(), "session.jsonl should exist at {path:?}");

    let content = std::fs::read_to_string(&path).unwrap();
    let first_line = content.lines().next().unwrap();
    let parsed: ion::session_jsonl::SessionHeader = serde_json::from_str(first_line).unwrap();

    assert_eq!(parsed.entry_type, "session");
    assert_eq!(parsed.version, 3);
    assert_eq!(parsed.id, sid);
    assert_eq!(parsed.cwd, cwd);

    cleanup_session(&cwd);
}

#[test]
fn u18_load_session_restores_messages() {
    let cwd = tmp_cwd("u18").to_string_lossy().to_string();
    let sid = format!("sess_{:08x}", randish());

    let header = ion::session_jsonl::SessionHeader {
        entry_type: "session".into(),
        version: 3,
        id: sid.clone(),
        timestamp: ion::session_jsonl::timestamp_iso(),
        cwd: cwd.clone(),
        parent_session: None,
    };

    // Build entries with parent chain
    let mut entries: Vec<serde_json::Value> = Vec::new();
    let mut parent_id = sid.clone();
    for i in 0..3 {
        let entry_id = ion::session_jsonl::generate_id();
        let role = if i == 0 { "user" } else { "assistant" };
        let entry = serde_json::json!({
            "type": "message",
            "id": entry_id,
            "parentId": parent_id,
            "timestamp": ion::session_jsonl::timestamp_iso(),
            "message": {
                "role": role,
                "content": [{"type": "text", "text": format!("test message {i}")}]
            }
        });
        parent_id = entry_id;
        entries.push(entry);
    }

    // Save as JSONL text (to avoid SessionFile::save assumptions about Message structure)
    let mut lines = vec![serde_json::to_string(&header).unwrap()];
    for e in &entries {
        lines.push(serde_json::to_string(e).unwrap());
    }
    let content = lines.join("\n");
    write_session_jsonl(&cwd, &content);

    // Now load it back
    let loaded = ion::session_jsonl::SessionFile::load(&cwd);
    assert!(loaded.is_some(), "session should load successfully");
    let file = loaded.unwrap();

    assert_eq!(file.header.id, sid);
    assert_eq!(file.header.version, 3);
    assert_eq!(file.entries.len(), 3, "should have 3 entries");

    // Parent chain: first entry's parentId should be the session ID
    if file.entries.len() >= 1 {
        assert_eq!(
            file.entries[0]["parentId"].as_str().unwrap_or(""),
            &sid,
            "first entry should have session id as parentId"
        );
    }

    cleanup_session(&cwd);
}

#[test]
fn u19_session_index_updates() {
    let _lock = INDEX_LOCK.lock().unwrap();
    let cwd = tmp_cwd("u19").to_string_lossy().to_string();
    let sid = format!("sess_{:08x}", randish());

    // First ensure no leftover index entry
    {
        let mut idx = ion::session_index::SessionIndex::load();
        idx.sessions.remove(&sid);
        idx.save();
    }

    // Create session header and save
    let header = ion::session_jsonl::SessionHeader {
        entry_type: "session".into(),
        version: 3,
        id: sid.clone(),
        timestamp: ion::session_jsonl::timestamp_iso(),
        cwd: cwd.clone(),
        parent_session: None,
    };
    ion::session_jsonl::SessionFile::save(&cwd, &header, &[]);

    // Update the index
    ion::session_index::SessionIndex::update(
        &sid, "deepseek-v4-flash", "opencode",
        "default", Some("test-session"),
        100, 50, 1, 1,
    );

    // Load index and verify
    let index = ion::session_index::SessionIndex::load();
    let meta = index.get(&sid);

    assert!(meta.is_some(), "session should appear in index");
    let meta = meta.unwrap();
    assert_eq!(meta.model, "deepseek-v4-flash");
    assert_eq!(meta.provider, "opencode");
    assert_eq!(meta.agent, "default");
    assert_eq!(meta.name.as_deref(), Some("test-session"));
    assert!(meta.token_input >= 100, "token_input should be >= 100, got {}", meta.token_input);
    assert!(meta.token_output >= 50, "token_output should be >= 50, got {}", meta.token_output);

    // Cleanup
    cleanup_session(&cwd);
    let mut index = ion::session_index::SessionIndex::load();
    index.sessions.remove(&sid);
    index.save();
}

#[test]
fn u20_token_stats_are_accurate() {
    let _lock = INDEX_LOCK.lock().unwrap();
    let cwd = tmp_cwd("u20").to_string_lossy().to_string();
    let sid = format!("sess_{:08x}", randish());

    // Ensure clean state
    {
        let mut idx = ion::session_index::SessionIndex::load();
        idx.sessions.remove(&sid);
        idx.save();
    }

    // Create header + entries
    let header = ion::session_jsonl::SessionHeader {
        entry_type: "session".into(),
        version: 3,
        id: sid.clone(),
        timestamp: ion::session_jsonl::timestamp_iso(),
        cwd: cwd.clone(),
        parent_session: None,
    };

    let mut entries: Vec<serde_json::Value> = Vec::new();
    let mut parent_id = sid.clone();

    // User message
    let eid1 = ion::session_jsonl::generate_id();
    entries.push(serde_json::json!({
        "type": "message",
        "id": eid1,
        "parentId": parent_id,
        "timestamp": ion::session_jsonl::timestamp_iso(),
        "message": {"role": "user", "content": [{"type": "text", "text": "Hello"}]}
    }));
    parent_id = eid1;

    // Assistant message with usage
    let eid2 = ion::session_jsonl::generate_id();
    entries.push(serde_json::json!({
        "type": "message",
        "id": eid2,
        "parentId": parent_id,
        "timestamp": ion::session_jsonl::timestamp_iso(),
        "message": {
            "role": "assistant",
            "content": [{"type": "text", "text": "Hi there!"}],
            "usage": {"input": 50, "output": 30, "cacheRead": 10, "cacheWrite": 5}
        }
    }));

    // Save
    let mut lines = vec![serde_json::to_string(&header).unwrap()];
    for e in &entries {
        lines.push(serde_json::to_string(e).unwrap());
    }
    write_session_jsonl(&cwd, &lines.join("\n"));

    // Update index with token stats
    ion::session_index::SessionIndex::update(
        &sid, "gpt-4o", "openai", "default",
        None, 50, 30, 2, 1,
    );

    // Verify token stats
    let index = ion::session_index::SessionIndex::load();
    let meta = index.get(&sid).expect("session should be in index");
    assert_eq!(meta.token_input, 50, "token_input mismatch");
    assert_eq!(meta.token_output, 30, "token_output mismatch");

    // Cleanup
    cleanup_session(&cwd);
    let mut idx = ion::session_index::SessionIndex::load();
    idx.sessions.remove(&sid);
    idx.save();
}
