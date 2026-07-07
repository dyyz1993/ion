use crate::agent::error::{AgentError, AgentResult};
use crate::agent::extension::*;
use crate::agent::tool::Tool;
use async_trait::async_trait;
use ion_provider::types::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

// ============================================================================
// ProcessInfo
// ============================================================================

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub bid: String,             // 8-char hex, e.g. "0000000a"
    #[serde(default)]
    pub os_pid: u32,             // real OS PID (for kill signal)
    pub command: String,
    pub description: String,
    pub status: String,          // "running" | "completed" | "killed" | "error"
    pub exit_code: Option<i32>,
    pub output: String,
    pub background: bool,
    pub started_at: i64,
    pub elapsed_secs: u64,
}

/// Shared mutable process state. Key = hex PID string.
pub type ProcessMap = Arc<Mutex<HashMap<String, ProcessInfo>>>;

/// Stdin channels keyed by hex PID.
type StdinMap = Arc<Mutex<HashMap<String, tokio::sync::mpsc::Sender<String>>>>;

/// Background notify channels keyed by hex PID.
type NotifyMap = Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<()>>>>;

fn new_stdin_map() -> StdinMap {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Path to processes.json.
fn processes_json_path() -> PathBuf {
    crate::paths::system_tmp_dir().join("ion-bash").join("processes.json")
}

fn save_processes(map: &HashMap<String, ProcessInfo>) {
    let path = processes_json_path();
    if let Some(parent) = path.parent() { let _ = std::fs::create_dir_all(parent); }
    if let Ok(content) = serde_json::to_string(map) {
        let _ = std::fs::write(&path, &content);
    }
}

fn save_process_map_arc(map: &ProcessMap) {
    if let Ok(locked) = map.try_lock() { save_processes(&locked); }
}

fn load_processes() -> HashMap<String, ProcessInfo> {
    let path = processes_json_path();
    if !path.exists() { return HashMap::new(); }
    std::fs::read_to_string(&path)
        .ok().and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Allocate a unique bash ID (6-char hex hash, lowercase, letter+number mixed).
/// Allocate a unique bash ID (6-char base36, no leading zeros, e.g. "100000", "a3f1c2").
fn allocate_pid(map: &HashMap<String, ProcessInfo>) -> String {
    const BASE: u64 = 36u64.pow(5); // 60,466,176 — ensures 6 chars with first char being 1-9/a-z
    let max_n = map.keys()
        .filter_map(|k| u64::from_str_radix(k, 36).ok())
        .max().unwrap_or(0);
    let n = if max_n < BASE { BASE } else { max_n + 1 };
    let chars = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut remaining = n;
    let mut result = ['0'; 6];
    for i in (0..6).rev() {
        result[i] = chars[(remaining % 36) as usize] as char;
        remaining /= 36;
    }
    result.iter().collect()
}

// ============================================================================
// Tools
// ============================================================================

/// bash_run — run a shell command (sync / background / timeout-background).
pub struct BashRunTool {
    pub process_map: ProcessMap,
    pub stdin_map: StdinMap,
    pub notify_map: NotifyMap,
    pub follow_up_tx: Option<tokio::sync::mpsc::UnboundedSender<Message>>,
    pub session_id: String,
}

#[async_trait]
impl Tool for BashRunTool {
    fn name(&self) -> &str { "bash_run" }
    fn description(&self) -> &str {
        "Execute a bash command. Use `background=true` for long-running commands. Always provide a clear `description`."
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "The shell command to execute"},
                "description": {"type": "string", "description": "Human-readable description"},
                "timeout": {"type": "number", "description": "Timeout in seconds", "default": 30},
                "background": {"type": "boolean", "default": false},
                "timeoutBackground": {"type": "boolean", "default": false}
            },
            "required": ["command", "description"]
        })
    }

    async fn execute(&self, args: serde_json::Value, rt: &dyn crate::runtime::Runtime) -> AgentResult<String> {
        let command = args.get("command").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let description = args.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let timeout = args.get("timeout").and_then(|v| v.as_u64()).unwrap_or(30);
        let background = args.get("background").and_then(|v| v.as_bool()).unwrap_or(false);
        let timeout_bg = args.get("timeoutBackground").and_then(|v| v.as_bool()).unwrap_or(false);
        if command.is_empty() {
            return Err(AgentError::Tool("bash_run: missing 'command'".into()));
        }

        // ── Common setup ──
        let now = now_ms();
        let pid: String = {
            let mut map = self.process_map.lock().await;
            let pid = allocate_pid(&map);
            map.insert(pid.clone(), ProcessInfo {
                bid: pid.clone(), os_pid: 0, command: command.clone(),
                description: description.clone(), status: "running".into(),
                exit_code: None, output: String::new(),
                background: background || timeout_bg, started_at: now, elapsed_secs: 0,
            });
            pid
        };
        save_process_map_arc(&self.process_map);

        emit_extension_event("process_started", &serde_json::json!({
            "bid": pid, "command": &command, "description": &description,
            "background": background || timeout_bg, "session": &self.session_id,
        }));

        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::channel::<String>(64);
        { let mut sm = self.stdin_map.lock().await; sm.insert(pid.clone(), stdin_tx); }

        // ── 后台模式：先安全预检，再用 spawn_watcher（保持流式输出和 stdin 转发）──
        if background || timeout_bg {
            // 安全预检：走 Runtime check_command（经过 SecuredRuntime CommandGuard）
            rt.check_command(&command).await.map_err(|e| AgentError::Tool(e))?;

            let child = match tokio::process::Command::new("sh")
                .args(["-c", &command])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped()).spawn()
            {
                Ok(c) => c,
                Err(e) => {
                    let mut map = self.process_map.lock().await; map.remove(&pid);
                    return Err(AgentError::Tool(format!("spawn error: {e}")));
                }
            };
            let os_pid = child.id().unwrap_or(0);
            { let mut map = self.process_map.lock().await;
              if let Some(entry) = map.get_mut(&pid) { entry.os_pid = os_pid; } }
            save_process_map_arc(&self.process_map);

            let (notify_tx, notify_rx) = tokio::sync::oneshot::channel::<()>();
            { let mut nm = self.notify_map.lock().await; nm.insert(pid.clone(), notify_tx); }

            tokio::spawn(spawn_watcher(
                self.process_map.clone(), self.stdin_map.clone(),
                self.notify_map.clone(), self.follow_up_tx.clone(),
                pid.clone(), command.clone(), description.clone(), child, stdin_rx, timeout,
            ));

            if background {
                Ok(format!("✅ Process #{pid} started in background: {description}"))
            } else {
                // timeoutBackground: 等超时或完成
                let result = tokio::select! {
                    result = notify_rx => match result {
                        Ok(()) => Ok(format!("⏱️ Process #{pid} moved to background.")),
                        Err(_) => {
                            let map = self.process_map.lock().await;
                            match map.get(&pid) {
                                Some(info) if info.exit_code == Some(0) => Ok(info.output.clone()),
                                Some(info) => Err(AgentError::Tool(format!("failed (exit={:?}): {}", info.exit_code, info.output))),
                                None => Ok(String::new()),
                            }
                        }
                    },
                    _ = tokio::time::sleep(std::time::Duration::from_secs(timeout)) => {
                        Ok(format!("⏱️ Process #{pid} moved to background."))
                    }
                };
                { let mut nm = self.notify_map.lock().await; nm.remove(&pid); }
                result
            }
        } else {
            // ── 前台模式：走 Runtime（经过 SecuredRuntime CommandGuard 检查）──
            let (stdout, stderr, exit_code) = rt.execute_command(&command, timeout)
                .await
                .map_err(|e| AgentError::Tool(format!("bash_run: {e}")))?;
            let os_pid = 0; // execute_command 不返回 pid

            // 更新进程状态
            let mut map = self.process_map.lock().await;
            if let Some(entry) = map.get_mut(&pid) {
                entry.os_pid = os_pid;
                entry.status = if exit_code == 0 { "completed".into() } else { "error".into() };
                entry.exit_code = Some(exit_code);
                let output = if stderr.is_empty() { stdout.clone() } else { format!("{stdout}\n{stderr}") };
                entry.output = output.clone();
                entry.elapsed_secs = ((now_ms() - now) / 1000) as u64;
            }
            drop(map);
            save_process_map_arc(&self.process_map);

            emit_extension_event("process_completed", &serde_json::json!({
                "bid": pid, "exit_code": exit_code, "session": &self.session_id,
            }));

            if exit_code != 0 {
                let output = if stderr.is_empty() { stdout } else { format!("{stdout}\n{stderr}") };
                Err(AgentError::Tool(format!("exit code {exit_code}:\n{output}")))
            } else {
                Ok(stdout)
            }
        }
    }
}

/// Shared watcher task for background and foreground modes.
/// Reads stdout line by line, emits `process_output` events every ~1s,
/// writes to log file, and sends completion notification.
fn spawn_watcher(
    map: ProcessMap, smap: StdinMap, nmap: NotifyMap,
    tx: Option<tokio::sync::mpsc::UnboundedSender<Message>>,
    pid: String, command: String, description: String,
    mut child: tokio::process::Child,
    mut stdin_rx: tokio::sync::mpsc::Receiver<String>,
    timeout: u64,
) -> impl std::future::Future<Output = ()> + Send {
    async move {
        let started = std::time::Instant::now();
        let log_dir = std::path::Path::new("/tmp").join("ion-bash");
        let _ = std::fs::create_dir_all(&log_dir);
        let log_path = log_dir.join(format!("{pid}.log"));

        // Forward stdin
        if let Some(mut child_stdin) = child.stdin.take() {
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                while let Some(input) = stdin_rx.recv().await {
                    let _ = child_stdin.write_all(input.as_bytes()).await;
                    let _ = child_stdin.write_all(b"\n").await;
                }
            });
        }

        // Read stdout line by line via BufReader
        use tokio::io::{AsyncBufReadExt, BufReader};
        let mut full_output = String::new();
        let mut line_buf: Vec<String> = Vec::new();
        let mut last_flush = std::time::Instant::now();
        let mut log_f = std::fs::OpenOptions::new().create(true).append(true).open(&log_path).ok();

	        if let Some(stdout) = child.stdout.take() {
            let mut reader = BufReader::new(stdout).lines();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout);

            loop {
                if std::time::Instant::now() >= deadline {
                    break; // overall timeout
                }

                // Try to read a line with 200ms timeout
                let line = tokio::time::timeout(
                    std::time::Duration::from_millis(200),
                    reader.next_line(),
                ).await;

                match line {
                    Ok(Ok(Some(text))) => {
                        full_output.push_str(&text);
                        full_output.push('\n');
                        if let Some(ref mut f) = log_f {
                            use std::io::Write;
                            let _ = writeln!(f, "{text}");
                        }
                        line_buf.push(text);
	                        last_flush = std::time::Instant::now();
                    }
                    Ok(Ok(None)) => break, // EOF
                    Ok(Err(_)) => break,    // read error
                    Err(_) => {
                        // Timeout: flush pending output and continue
                        if !line_buf.is_empty() && last_flush.elapsed().as_secs() >= 1 {
                            let batch = line_buf.join("\n");
                            emit_extension_event("process_output", &serde_json::json!({
                                "bid": pid, "output": batch, "lines": line_buf.len(),
                            }));
                            line_buf.clear();
                        }
                        continue;
                    }
                }
            }
        }

        // Flush remaining output
        if !line_buf.is_empty() {
            let batch = line_buf.join("\n");
            emit_extension_event("process_output", &serde_json::json!({
                "bid": pid, "output": batch, "lines": line_buf.len(),
            }));
            line_buf.clear();
        }

        // Wait for the process to fully exit (collect exit code)
        smap.lock().await.remove(&pid);
        let elapsed = started.elapsed().as_secs();
        let exit_status = child.wait().await;
        let (exit_code, event_type) = match exit_status {
            Ok(status) => (status.code(), if status.success() { "process_completed" } else { "process_completed" }),
            Err(_) => (None, "process_error"),
        };
        let stdout_stderr = full_output.clone();

        // Write full output (should be redundant but safe)
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&log_path) {
            use std::io::Write;
            let _ = write!(f, "{}", stdout_stderr);
        }
        {
            let mut pm = map.lock().await;
            if let Some(entry) = pm.get_mut(&pid) {
                // 不要覆盖 bash_kill 标记的 killed 状态
                if entry.status != "killed" {
                    entry.status = event_type.trim_start_matches("process_").to_string();
                }
                entry.exit_code = exit_code; entry.output = stdout_stderr.clone(); entry.elapsed_secs = elapsed;
            }
        }
        save_process_map_arc(&map);
        nmap.lock().await.remove(&pid);

        emit_extension_event(event_type, &serde_json::json!({
            "bid": pid, "command": command, "description": description,
            "exit_code": exit_code, "elapsed_secs": elapsed, "log_path": log_path.to_string_lossy(),
            "reason": if exit_code == Some(0) { "completed" } else if exit_code.is_some() { "abnormal" } else { event_type.trim_start_matches("process_") },
        }));

        if let Some(ref tx) = tx {
            let content = format!(
                "<bash_result>\n✅ `{}` completed (pid={}, exit_code={:?}, {}s)\n{}\n</bash_result>",
                command, pid, exit_code, elapsed,
                if stdout_stderr.len() > 500 { format!("{}...[truncated]", &stdout_stderr[..500]) } else { stdout_stderr }
            );
            let msg = Message::Custom(CustomMessage {
                role: "custom".into(), custom_type: "bash_result".into(),
                content: CustomContent::Text(content), display: true, details: None, timestamp: now_ms(),
            });
            let _ = tx.send(msg);
        }
    }
}

/// bash_kill — kill a process by hex PID.
pub struct BashKillTool {
    pub process_map: ProcessMap,
    pub follow_up_tx: Option<tokio::sync::mpsc::UnboundedSender<Message>>,
    pub session_id: String,
}

#[async_trait]
impl Tool for BashKillTool {
    fn name(&self) -> &str { "bash_kill" }
    fn description(&self) -> &str { "Kill a running process by PID." }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"bid":{"type":"string","description":"Process bash ID (bid)"}},"required":["pid"]})
    }
    async fn execute(&self, args: serde_json::Value, rt: &dyn crate::runtime::Runtime) -> AgentResult<String> {
        let pid = args.get("bid").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if pid.is_empty() { return Err(AgentError::Tool("bash_kill: missing 'pid'".into())); }
        // 先标记为 killed（防止 watcher 竞争覆盖成 completed）
        let os_pid = {
            let mut map = self.process_map.lock().await;
            if let Some(info) = map.get_mut(&pid) {
                info.status = "killed".into();
                info.os_pid
            } else { 0 }
        };
        if os_pid == 0 { return Err(AgentError::Tool(format!("Process #{pid} has no OS PID"))); }
        // 走 Runtime 的 kill_process（经过 SecuredRuntime 检查）
        let killed = rt.kill_process(os_pid).await.is_ok();
        if killed {
            let mut map = self.process_map.lock().await;
            if let Some(info) = map.get_mut(&pid) { info.status = "killed".into(); }
            let _ = std::fs::write(format!("/tmp/ion-bash/{pid}.log"), format!("[killed by bash_kill]\n"));
            // Notify LLM: inject a custom message into conversation history
            if let Some(ref tx) = self.follow_up_tx {
                let content = format!(
                    "<bash_result>\n🛑 Process #{} (`{}`) was killed by user.\n</bash_result>",
                    pid, os_pid,
                );
                let msg = Message::Custom(CustomMessage {
                    role: "custom".into(), custom_type: "bash_result".into(),
                    content: CustomContent::Text(content), display: true,
                    details: None, timestamp: now_ms(),
                });
                let _ = tx.send(msg);
            }
            Ok(format!("✅ Process #{pid} killed"))
        } else {
            Err(AgentError::Tool(format!("Failed to kill process #{pid} (os_pid={os_pid})")))
        }
    }
}

/// bash_send — send stdin to a background process.
pub struct BashSendTool {
    pub stdin_map: StdinMap,
}

#[async_trait]
impl Tool for BashSendTool {
    fn name(&self) -> &str { "bash_send" }
    fn description(&self) -> &str { "Send input to the stdin of a running background process." }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{
            "bid":{"type":"string","description":"Process bash ID (bid)"},
            "input":{"type":"string","description":"Input text"}
        },"required":["pid","input"]})
    }
    async fn execute(&self, args: serde_json::Value, _rt: &dyn crate::runtime::Runtime) -> AgentResult<String> {
        let pid = args.get("bid").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let input = args.get("input").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if pid.is_empty() { return Err(AgentError::Tool("bash_send: missing 'pid'".into())); }
        if input.is_empty() { return Err(AgentError::Tool("bash_send: missing 'input'".into())); }
        let mut sm = self.stdin_map.lock().await;
        match sm.get(&pid) {
            Some(tx) => {
                if tx.send(input.clone()).await.is_err() {
                    sm.remove(&pid);
                    Err(AgentError::Tool(format!("Process #{pid} has ended (stdin closed)")))
                } else {
                    Ok(format!("✅ Sent to process #{pid}: {input}"))
                }
            }
            None => Err(AgentError::Tool(format!("Process #{pid} not found or has no stdin channel"))),
        }
    }
}

/// bash_background — move a running foreground process to background.
pub struct BashBackgroundTool {
    pub notify_map: NotifyMap,
    pub process_map: ProcessMap,
}

#[async_trait]
impl Tool for BashBackgroundTool {
    fn name(&self) -> &str { "bash_background" }
    fn description(&self) -> &str { "Move a running foreground process to background." }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"bid":{"type":"string","description":"Process bash ID (bid)"}},"required":["pid"]})
    }
    async fn execute(&self, args: serde_json::Value, _rt: &dyn crate::runtime::Runtime) -> AgentResult<String> {
        let pid = args.get("bid").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if pid.is_empty() { return Err(AgentError::Tool("bash_background: missing 'pid'".into())); }
        let mut nm = self.notify_map.lock().await;
        match nm.remove(&pid) {
            Some(tx) => {
                let _ = tx.send(());
                let mut map = self.process_map.lock().await;
                if let Some(info) = map.get_mut(&pid) { info.background = true; }
                save_processes(&map);
                Ok(format!("✅ Process #{pid} moved to background"))
            }
            None => {
                let map = self.process_map.lock().await;
                if map.contains_key(&pid) {
                    Err(AgentError::Tool(format!("Process #{pid} is not waiting (already bg/completed)")))
                } else {
                    Err(AgentError::Tool(format!("Process #{pid} not found")))
                }
            }
        }
    }
}

// ============================================================================
// BashExtension — plugin_rpc
// ============================================================================

pub struct BashExtension {
    pub process_map: ProcessMap,
    pub stdin_map: StdinMap,
    pub notify_map: NotifyMap,
    pub follow_up_tx: Option<tokio::sync::mpsc::UnboundedSender<Message>>,
    pub session_id: String,
}

impl BashExtension {
    pub fn new(session_id: &str) -> Self {
        let processes = load_processes();
        Self {
            process_map: Arc::new(Mutex::new(processes)),
            stdin_map: new_stdin_map(),
            notify_map: Arc::new(Mutex::new(HashMap::new())),
            follow_up_tx: None,
            session_id: session_id.to_string(),
        }
    }
}

fn parse_pid(params: &serde_json::Value) -> String {
    params.get("bid").and_then(|v| v.as_str()).unwrap_or("").to_string()
}

#[async_trait]
impl Extension for BashExtension {
    fn name(&self) -> &str { "bash" }

    async fn on_extension_rpc(&self, method: &str, params: serde_json::Value) -> AgentResult<serde_json::Value> {
        match method {
            "list" => {
                let map = self.process_map.lock().await;
                let processes: Vec<serde_json::Value> = map.values().map(|p| serde_json::json!({
                    "bid": p.bid, "command": p.command,
                    "description": p.description, "status": p.status, "background": p.background,
                    "elapsed_secs": p.elapsed_secs,
                })).collect();
                Ok(serde_json::json!({"processes": processes, "count": processes.len()}))
            }
            "inspect" => {
                let pid = parse_pid(&params);
                let tail = params.get("tail").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let offset = params.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(2000) as usize;
                let map = self.process_map.lock().await;
                match map.get(&pid) {
                    Some(info) => {
                        let output = &info.output;
                        let preview = if tail > 0 && output.len() > tail {
                            // tail mode: return last N bytes
                            format!("...[truncated {} bytes]\n{}", output.len() - tail, &output[output.len().saturating_sub(tail)..])
                        } else if offset < output.len() {
                            // offset+limit mode: "seek" from start
                            let end = (offset + limit).min(output.len());
                            let snippet = &output[offset..end];
                            if offset > 0 {
                                format!("[offset {offset}]\n{snippet}")
                            } else {
                                snippet.to_string()
                            }
                        } else {
                            String::new()
                        };
                        Ok(serde_json::json!({
                            "bid": info.bid, "command": info.command,
                            "description": info.description, "status": info.status,
                            "exit_code": info.exit_code, "background": info.background,
                            "elapsed_secs": info.elapsed_secs, "output_preview": preview,
                            "output_len": output.len(),
                        }))
                    }
                    None => Ok(serde_json::json!({"error": "process not found"})),
                }
            }
            "kill" => {
                let pid = parse_pid(&params);
                if pid.is_empty() { return Ok(serde_json::json!({"error": "missing pid"})); }
                let os_pid = {
                    let mut map = self.process_map.lock().await;
                    if let Some(info) = map.get_mut(&pid) {
                        info.status = "killed".into();  // 标记防止 watcher 覆盖
                        info.os_pid
                    } else { 0 }
                };
                if os_pid == 0 { return Ok(serde_json::json!({"error": "no OS PID"})); }
                let killed = std::process::Command::new("kill").args([&os_pid.to_string()]).output().map(|o| o.status.success()).unwrap_or(false);
                if killed {
                    let mut map = self.process_map.lock().await;
                    if let Some(info) = map.get_mut(&pid) { info.status = "killed".into(); }
                    save_processes(&map);
                    let mut sm = self.stdin_map.lock().await; sm.remove(&pid);
                    // Notify LLM
                    if let Some(ref tx) = self.follow_up_tx {
                        let content = format!(
                            "<bash_result>\n🛑 Process #{} (`{}`) was killed by user.\n</bash_result>",
                            pid, os_pid,
                        );
                        let msg = Message::Custom(CustomMessage {
                            role: "custom".into(), custom_type: "bash_result".into(),
                            content: CustomContent::Text(content), display: true,
                            details: None, timestamp: now_ms(),
                        });
                        let _ = tx.send(msg);
                    }
                    Ok(serde_json::json!({"status": "killed"}))
                } else { Ok(serde_json::json!({"error": "kill failed"})) }
            }
            "send" => {
                let pid = parse_pid(&params);
                let input = params.get("input").and_then(|v| v.as_str()).unwrap_or("").to_string();
                if pid.is_empty() { return Ok(serde_json::json!({"error": "missing pid"})); }
                if input.is_empty() { return Ok(serde_json::json!({"error": "missing input"})); }
                let mut sm = self.stdin_map.lock().await;
                match sm.get(&pid) {
                    Some(tx) => {
                        if tx.send(input.clone()).await.is_err() {
                            sm.remove(&pid);
                            Ok(serde_json::json!({"error": "stdin closed"}))
                        } else { Ok(serde_json::json!({"status": "delivered", "bid": pid, "input": input})) }
                    }
                    None => Ok(serde_json::json!({"error": "process not found or no stdin channel"})),
                }
            }
            "clean" => {
                let mut map = self.process_map.lock().await;
                let mut sm = self.stdin_map.lock().await;
                let before = map.len();
                map.retain(|_, p| p.status == "running");
                sm.retain(|pid, _| map.contains_key(pid));
                let cleaned = before - map.len();
                save_processes(&map);
                Ok(serde_json::json!({"cleaned": cleaned}))
            }
            "remove" => {
                let bid = params.get("bid").or_else(|| params.get("pid"))
                    .and_then(|v| v.as_str()).unwrap_or("").to_string();
                if bid.is_empty() {
                    return Err(AgentError::Tool("remove requires 'bid' or 'pid'".into()));
                }
                let mut map = self.process_map.lock().await;
                let mut sm = self.stdin_map.lock().await;
                let removed = map.remove(&bid).is_some();
                sm.remove(&bid);
                save_processes(&map);
                Ok(serde_json::json!({"removed": removed, "bid": bid}))
            }
            _ => Err(AgentError::Tool(format!("bash extension_rpc: unknown method {method}"))),
        }
    }
}

// ============================================================================
// Helpers
// ============================================================================

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as i64
}

fn emit_extension_event(event_type: &str, data: &serde_json::Value) {
    // 注意：Manager 的 stdout 路由只识别 "type":"event"，
    // 所以 plugin_event 需要嵌在 event.type 里才能到达 subscriber
    let msg = serde_json::json!({
        "type": "event",
        "event": {
            "type": "extension_event",
            "extension": "bash",
            "customType": event_type,
            "visibility": "llm_and_ui",
            "data": data,
        },
    });
    println!("{}", serde_json::to_string(&msg).unwrap_or_default());
}
