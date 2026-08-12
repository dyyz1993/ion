use crate::agent::agent_loop::DeliverAs;
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

/// follow_up channel 的消息 + 投递时机。
/// 类型别名避免 `UnboundedSender<(Message, DeliverAs)>` 嵌套尖括号 `>>` 解析问题。
pub type FollowUpSender = tokio::sync::mpsc::UnboundedSender<(Message, DeliverAs)>;

// ============================================================================
// ProcessInfo
// ============================================================================

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub bid: String, // 8-char hex, e.g. "0000000a"
    #[serde(default)]
    pub os_pid: u32, // real OS PID (for kill signal)
    pub command: String,
    pub description: String,
    pub status: String, // "running" | "completed" | "killed" | "error"
    pub exit_code: Option<i32>,
    pub output: String,
    pub background: bool,
    pub started_at: i64,
    pub elapsed_secs: u64,
}

/// Shared mutable process state. Key = hex PID string.
pub type ProcessMap = Arc<Mutex<HashMap<String, ProcessInfo>>>;

/// Stdin channels keyed by hex PID.
pub(super) type StdinMap = Arc<Mutex<HashMap<String, tokio::sync::mpsc::Sender<String>>>>;

/// Background notify channels keyed by hex PID.
pub(super) type NotifyMap = Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<()>>>>;

fn new_stdin_map() -> StdinMap {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Wrap a shell command so stderr is merged into stdout (commit a699d58).
///
/// Why: spawn_watcher only reads child.stdout. stderr was piped but
/// discarded, so commands like `python3 -m http.server 9999` (port
/// already in use) returned exit=1 with empty body — the OSError
/// traceback was on stderr.
///
/// Fix: prepend `exec 2>&1 ;` so the shell redirects its own stderr fd
/// to stdout before running the command. Shell-layer merge is simpler
/// than Rust Arc<Mutex> parallel reader and preserves original order.
///
/// Public(crate) so tests can verify the wrapping logic without spawning
/// real processes.
pub(crate) fn merge_stderr_to_stdout(command: &str) -> String {
    format!("exec 2>&1 ; {}", command)
}

/// Path to processes.json — session 级别（每个 session 独立存储）。
fn processes_json_path(cwd: &str, session_id: &str) -> PathBuf {
    crate::paths::bash_processes_path(cwd, session_id)
}

fn save_processes(map: &HashMap<String, ProcessInfo>, cwd: &str, session_id: &str) {
    let path = processes_json_path(cwd, session_id);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(content) = serde_json::to_string(map) {
        let _ = std::fs::write(&path, &content);
    }
}

pub(super) fn save_process_map_arc(map: &ProcessMap, cwd: &str, session_id: &str) {
    if let Ok(locked) = map.try_lock() {
        save_processes(&locked, cwd, session_id);
    }
}

fn load_processes(cwd: &str, session_id: &str) -> HashMap<String, ProcessInfo> {
    let path = processes_json_path(cwd, session_id);
    if !path.exists() {
        return HashMap::new();
    }
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Allocate a unique bash ID (6-char hex hash, lowercase, letter+number mixed).
/// Allocate a unique bash ID (6-char base36, no leading zeros, e.g. "100000", "a3f1c2").
fn allocate_pid(map: &HashMap<String, ProcessInfo>) -> String {
    const BASE: u64 = 36u64.pow(5); // 60,466,176 — ensures 6 chars with first char being 1-9/a-z
    let max_n = map
        .keys()
        .filter_map(|k| u64::from_str_radix(k, 36).ok())
        .max()
        .unwrap_or(0);
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

/// bash — run a shell command (sync / background / timeout-background).
/// Unified tool: replaces the old separate bash_run. Management operations
/// (kill/send/inspect) are exposed via extension_rpc(bash, ...).
pub struct BashRunTool {
    pub process_map: ProcessMap,
    pub stdin_map: StdinMap,
    pub notify_map: NotifyMap,
    pub follow_up_tx: Option<FollowUpSender>,
    pub storage: crate::storage_context::StorageContext,
}

/// BashManageTool — shared engine for background process management.
/// Not exposed to LLM directly; wrapped by GetBackgroundProcessTool /
/// KillProcessTool / WriteStdinTool (pi-aligned names).
#[derive(Clone)]
pub struct BashManageTool {
    pub process_map: ProcessMap,
    pub stdin_map: StdinMap,
    pub follow_up_tx: Option<FollowUpSender>,
    pub storage: crate::storage_context::StorageContext,
}

#[async_trait]
impl Tool for BashRunTool {
    fn name(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        "Execute a shell command and return its output. For long-running commands (dev servers, builds, watches), set background=true to return immediately with a process bid. To manage background processes, use get_background_process (view status/output), kill_process (terminate), or write_stdin (send input)."
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "The shell command to execute"},
                "description": {"type": "string", "description": "A clear, human-readable description of what this command does. ALWAYS provide this."},
                "timeout": {"type": "number", "description": "Timeout in seconds for foreground execution", "default": 30},
                "background": {"type": "boolean", "description": "If true, run in background and return immediately with a process bid", "default": false},
                "timeoutBackground": {"type": "boolean", "description": "If true, start foreground but auto-move to background on timeout", "default": false},
                "bgTimeout": {"type": "number", "description": "Background timeout in seconds. Only with background=true. 0=no timeout (run until natural exit). >0=kill process after N seconds and report exit=timeout. Default: 0", "default": 0},
                "deliverAs": {
                    "type": "string",
                    "enum": ["steer", "followUp", "nextTurn"],
                    "description": "How to deliver the <bash_result> notification when this background process completes. Only effective with background=true or timeoutBackground=true. 'steer'=interrupt current LLM turn immediately (use when the result is urgent and the current task should yield); 'followUp'=inject at the start of the next turn (default, non-interrupting); 'nextTurn'=wait until agent.run completes before triggering a new run (lowest priority).",
                    "default": "followUp"
                }
            },
            "required": ["command", "description"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        rt: &dyn crate::runtime::Runtime,
    ) -> AgentResult<String> {
        let command = args
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let description = args
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let timeout = args
            .get("timeout")
            .and_then(|v| v.as_u64())
            .unwrap_or_else(|| {
                std::env::var("ION_BASH_RUN_TIMEOUT")
                    .ok()
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(30)
            });
        let background = args
            .get("background")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let timeout_bg = args
            .get("timeoutBackground")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        // 解析 deliverAs：调用方控制 background 完成通知的投递时机。
        // 仅在 background=true 或 timeoutBackground=true 时生效。
        // 前台同步执行（background=false）不发 follow_up，deliverAs 无意义。
        // bgTimeout: 后台进程超时秒数。0=无限（默认），>0=N 秒后杀进程报 exit=timeout。
        let bg_timeout = args.get("bgTimeout").and_then(|v| v.as_u64()).unwrap_or(0);
        let deliver_as: DeliverAs = args
            .get("deliverAs")
            .and_then(|v| v.as_str())
            .and_then(|s| match s {
                "steer" => Some(DeliverAs::Steer),
                "followUp" => Some(DeliverAs::FollowUp),
                "nextTurn" => Some(DeliverAs::NextTurn),
                _ => None,
            })
            .unwrap_or(DeliverAs::FollowUp);
        if command.is_empty() {
            return Err(AgentError::Tool("bash: missing 'command'".into()));
        }

        // ── Common setup ──
        let now = now_ms();
        let pid: String = {
            let mut map = self.process_map.lock().await;
            let pid = allocate_pid(&map);
            map.insert(
                pid.clone(),
                ProcessInfo {
                    bid: pid.clone(),
                    os_pid: 0,
                    command: command.clone(),
                    description: description.clone(),
                    status: "running".into(),
                    exit_code: None,
                    output: String::new(),
                    background: background || timeout_bg,
                    started_at: now,
                    elapsed_secs: 0,
                },
            );
            pid
        };
        save_process_map_arc(
            &self.process_map,
            &self.storage.cwd,
            &self.storage.session_id,
        );

        emit_extension_event(
            "process_started",
            &serde_json::json!({
                "bid": pid, "command": &command, "description": &description,
                "background": background || timeout_bg, "session": &self.storage.session_id,
            }),
        );

        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::channel::<String>(64);
        {
            let mut sm = self.stdin_map.lock().await;
            sm.insert(pid.clone(), stdin_tx);
        }

        // ── 后台模式：先安全预检，再用 spawn_watcher（保持流式输出和 stdin 转发）──
        if background || timeout_bg {
            // 安全预检：走 Runtime check_command（经过 SecuredRuntime CommandGuard）
            rt.check_command(&command).await.map_err(AgentError::Tool)?;

            // ★ 在 command 前加 `exec 2>&1` 让 stderr 重定向到 stdout。
            // 之前 stderr 被 piped 但 spawn_watcher 不读 → 错误信息全丢
            // （exit=1 时端口占用/命令不存在的错误全在 stderr）。
            // Shell 层合并比 Rust 层 Arc<Mutex> 简单，且保持原始输出顺序。
            let merged_command = merge_stderr_to_stdout(&command);
            // 使用 process_group(0) 让子进程有自己的进程组，
            // 这样 kill 时可以用 kill -- -$PID 杀整个进程组（含子进程）
            let mut std_cmd = std::process::Command::new("sh");
            std_cmd
                .args(["-c", &merged_command])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            // Unix: 设置独立进程组
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                std_cmd.process_group(0);
            }
            let child = match tokio::process::Command::from(std_cmd)
                .spawn()
            {
                Ok(c) => c,
                Err(e) => {
                    let mut map = self.process_map.lock().await;
                    map.remove(&pid);
                    return Err(AgentError::Tool(format!("spawn error: {e}")));
                }
            };
            let os_pid = child.id().unwrap_or(0);
            {
                let mut map = self.process_map.lock().await;
                if let Some(entry) = map.get_mut(&pid) {
                    entry.os_pid = os_pid;
                }
            }
            save_process_map_arc(
                &self.process_map,
                &self.storage.cwd,
                &self.storage.session_id,
            );

            let (notify_tx, notify_rx) = tokio::sync::oneshot::channel::<()>();
            {
                let mut nm = self.notify_map.lock().await;
                nm.insert(pid.clone(), notify_tx);
            }

            tokio::spawn(crate::agent::bash_executor::spawn_watcher(
                self.process_map.clone(),
                self.stdin_map.clone(),
                self.notify_map.clone(),
                self.follow_up_tx.clone(),
                pid.clone(),
                command.clone(),
                description.clone(),
                child,
                stdin_rx,
                // ★ background=true 时的 timeout 策略：
                // - bgTimeout=0（默认）：用 86400（实质无限，让进程跑到自然结束）
                // - bgTimeout>0：用 bgTimeout（N 秒后 spawn_watcher 超时 break，进程被杀）
                // timeoutBackground 路径保留 timeout（用于触发"超时切后台"语义）。
                if background {
                    if bg_timeout > 0 { bg_timeout } else { 86400 }
                } else {
                    timeout
                },
                self.storage.cwd.clone(),
                self.storage.session_id.clone(),
                deliver_as,
            ));

            if background {
                Ok(format!(
                    "✅ Process #{pid} started in background: {description}"
                ))
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
                {
                    let mut nm = self.notify_map.lock().await;
                    nm.remove(&pid);
                }
                result
            }
        } else {
            // ── 前台模式：走 Runtime（经过 SecuredRuntime CommandGuard 检查）──
            let (stdout, stderr, exit_code) = rt
                .execute_command(&command, timeout)
                .await
                .map_err(|e| AgentError::Tool(format!("bash: {e}")))?;
            let os_pid = 0; // execute_command 不返回 pid

            // 更新进程状态（用于 emit process_completed 事件载荷完整性）
            let output_for_event = {
                let mut map = self.process_map.lock().await;
                if let Some(entry) = map.get_mut(&pid) {
                    entry.os_pid = os_pid;
                    entry.status = if exit_code == 0 {
                        "completed".into()
                    } else {
                        "error".into()
                    };
                    entry.exit_code = Some(exit_code);
                    let output = if stderr.is_empty() {
                        stdout.clone()
                    } else {
                        format!("{stdout}\n{stderr}")
                    };
                    entry.output = output.clone();
                    entry.elapsed_secs = ((now_ms() - now) / 1000) as u64;
                }
                if stderr.is_empty() {
                    stdout.clone()
                } else {
                    format!("{stdout}\n{stderr}")
                }
            };

            emit_extension_event(
                "process_completed",
                &serde_json::json!({
                    "bid": pid, "exit_code": exit_code, "session": &self.storage.session_id,
                }),
            );

            // 前台同步执行完毕：从 process_map / stdin_map 移除，避免出现在 list / inspect / processes.json。
            // 仅 background=true 或 timeoutBackground=true 的进程才应留在 map 里供后续管理。
            {
                let mut map = self.process_map.lock().await;
                map.remove(&pid);
            }
            {
                let mut sm = self.stdin_map.lock().await;
                sm.remove(&pid);
            }
            save_process_map_arc(
                &self.process_map,
                &self.storage.cwd,
                &self.storage.session_id,
            );

            if exit_code != 0 {
                Err(AgentError::Tool(format!(
                    "exit code {exit_code}:\n{output_for_event}"
                )))
            } else {
                Ok(stdout)
            }
        }
    }
}

// ============================================================================
// BashExtension — plugin_rpc
// ============================================================================

// ════════════════════════════════════════════════════════════════════════════
// 3 个独立 LLM 工具（对标 pi），包装 BashManageTool 的 on_manage 逻辑
// ════════════════════════════════════════════════════════════════════════════

/// get_background_process — 查询后台进程状态/输出（对标 pi get_background_process）。
/// 不传 bid 时列所有进程；传 bid 时查单个进程详情（含头尾行截断 + 输出大小）。
pub struct GetBackgroundProcessTool {
    manage: BashManageTool,
}

#[async_trait]
impl Tool for GetBackgroundProcessTool {
    fn name(&self) -> &str {
        "get_background_process"
    }
    fn description(&self) -> &str {
        "Get background process status and output. Without bid: list all processes. With bid: inspect a single process (returns status, exit code, elapsed, started_at, output size, head/tail lines with truncation)."
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "bid": {"type": "string", "description": "Process bash ID. Omit to list all processes."},
                "head": {"type": "number", "description": "Number of head lines to show (default 5)", "default": 5},
                "tailLines": {"type": "number", "description": "Number of tail lines to show (default 5)", "default": 5},
                "tail": {"type": "number", "description": "Tail mode: return last N bytes of output", "default": 0},
                "offset": {"type": "number", "description": "Offset mode: seek from start (bytes)", "default": 0},
                "limit": {"type": "number", "description": "Max bytes to return in offset mode", "default": 2000}
            }
        })
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        _rt: &dyn crate::runtime::Runtime,
    ) -> AgentResult<String> {
        let action = if args.get("bid").is_some() {
            "inspect"
        } else {
            "list"
        };
        let result = self.manage.on_manage(action, &args).await;
        Ok(serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".into()))
    }
}

/// kill_process — 终止后台进程（对标 pi channel kill）。
pub struct KillProcessTool {
    manage: BashManageTool,
}

#[async_trait]
impl Tool for KillProcessTool {
    fn name(&self) -> &str {
        "kill_process"
    }
    fn description(&self) -> &str {
        "Kill a background process by its bash ID."
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "bid": {"type": "string", "description": "Process bash ID to kill"}
            },
            "required": ["bid"]
        })
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        _rt: &dyn crate::runtime::Runtime,
    ) -> AgentResult<String> {
        let result = self.manage.on_manage("kill", &args).await;
        Ok(serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".into()))
    }
}

/// write_stdin — 向后台进程的标准输入写入数据（对标 pi channel write_stdin）。
pub struct WriteStdinTool {
    manage: BashManageTool,
}

#[async_trait]
impl Tool for WriteStdinTool {
    fn name(&self) -> &str {
        "write_stdin"
    }
    fn description(&self) -> &str {
        "Write text to the stdin of a running background process."
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "bid": {"type": "string", "description": "Process bash ID"},
                "input": {"type": "string", "description": "Text to send to process stdin"}
            },
            "required": ["bid", "input"]
        })
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        _rt: &dyn crate::runtime::Runtime,
    ) -> AgentResult<String> {
        let result = self.manage.on_manage("send", &args).await;
        Ok(serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".into()))
    }
}

impl BashManageTool {
    /// Dispatch management actions. Mirrors BashExtension::on_extension_rpc logic,
    /// but operates on the shared Arc state directly.
    async fn on_manage(&self, action: &str, params: &serde_json::Value) -> serde_json::Value {
        match action {
            "list" => {
                let map = self.process_map.lock().await;
                let processes: Vec<serde_json::Value> = map
                    .values()
                    .map(|p| {
                        serde_json::json!({
                            "bid": p.bid, "command": p.command,
                            "description": p.description, "status": p.status,
                            "background": p.background, "elapsed_secs": p.elapsed_secs,
                        })
                    })
                    .collect();
                serde_json::json!({"processes": processes, "count": processes.len()})
            }
            "inspect" => {
                let pid = parse_pid(params);
                let tail = params.get("tail").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let offset = params.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(2000) as usize;
                let head_lines = params.get("head").and_then(|v| v.as_u64()).unwrap_or(5) as usize;
                let tail_lines = params
                    .get("tailLines")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(5) as usize;
                let map = self.process_map.lock().await;
                match map.get(&pid) {
                    Some(info) => {
                        let output = &info.output;
                        let output_bytes = output.len();

                        // ── output_preview（保留向后兼容，支持 tail/offset 模式）──
                        let preview = if tail > 0 && output_bytes > tail {
                            format!(
                                "...[truncated {} bytes]\n{}",
                                output_bytes - tail,
                                &output[output_bytes.saturating_sub(tail)..]
                            )
                        } else if offset < output_bytes {
                            let end = (offset + limit).min(output_bytes);
                            let snippet = &output[offset..end];
                            if offset > 0 {
                                format!("[offset {offset}]\n{snippet}")
                            } else {
                                snippet.to_string()
                            }
                        } else {
                            String::new()
                        };

                        // ── 头尾行预览（用户期望：头几行 + 尾几行 + 中间截断标记）──
                        let all_lines: Vec<&str> = output.lines().collect();
                        let total_lines = all_lines.len();
                        let (output_head, output_tail, output_truncated) =
                            if total_lines > head_lines + tail_lines {
                                // 超长：头 N 行 + ...[truncated M lines]... + 尾 N 行
                                let head_joined = all_lines[..head_lines].join("\n");
                                let tail_joined = all_lines[total_lines - tail_lines..].join("\n");
                                let middle_lines = total_lines - head_lines - tail_lines;
                                (
                                    head_joined,
                                    format!(
                                        "...[truncated {} lines]...\n{}",
                                        middle_lines, tail_joined
                                    ),
                                    true,
                                )
                            } else {
                                (output.clone(), String::new(), false)
                            };

                        // ── 友好的输出大小 ──
                        let output_size_human = if output_bytes < 1024 {
                            format!("{} B", output_bytes)
                        } else if output_bytes < 1024 * 1024 {
                            format!("{:.1} KB", output_bytes as f64 / 1024.0)
                        } else {
                            format!("{:.1} MB", output_bytes as f64 / (1024.0 * 1024.0))
                        };

                        // ── started_at 转 ISO（从 unix ms）──
                        let started_at_iso = {
                            let secs = info.started_at / 1000;
                            let millis = info.started_at % 1000;
                            let days_since_epoch = secs / 86400;
                            let time_secs = secs % 86400;
                            let h = time_secs / 3600;
                            let m = (time_secs % 3600) / 60;
                            let s = time_secs % 60;
                            let mut y = 2025i64;
                            let mut days_remaining = days_since_epoch.saturating_sub(20089);
                            loop {
                                let diy = if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                                    366
                                } else {
                                    365
                                };
                                if days_remaining < diy {
                                    break;
                                }
                                days_remaining -= diy;
                                y += 1;
                            }
                            let md = if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                                [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
                            } else {
                                [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
                            };
                            let mut mo = 1i64;
                            for &d in &md {
                                if days_remaining < d {
                                    break;
                                }
                                days_remaining -= d;
                                mo += 1;
                            }
                            format!(
                                "{y:04}-{mo:02}-{}T{h:02}:{m:02}:{s:02}.{millis:03}Z",
                                days_remaining + 1
                            )
                        };

                        serde_json::json!({
                            "bid": info.bid, "command": info.command,
                            "description": info.description, "status": info.status,
                            "exit_code": info.exit_code, "background": info.background,
                            "elapsed_secs": info.elapsed_secs,
                            "started_at": started_at_iso,
                            // 输出大小
                            "output_bytes": output_bytes,
                            "output_size": output_size_human,
                            "output_lines": total_lines,
                            // 头尾预览（新格式：头 N 行 + 尾 N 行 + 中间截断）
                            "output_head": output_head,
                            "output_tail": output_tail,
                            "output_truncated": output_truncated,
                            // 向后兼容

                        })
                    }
                    None => serde_json::json!({"error": "process not found"}),
                }
            }
            "kill" => {
                let pid = parse_pid(params);
                if pid.is_empty() {
                    return serde_json::json!({"error": "missing bid"});
                }
                let os_pid = {
                    let mut map = self.process_map.lock().await;
                    if let Some(info) = map.get_mut(&pid) {
                        info.status = "killed".into();
                        info.os_pid
                    } else {
                        0
                    }
                };
                if os_pid == 0 {
                    return serde_json::json!({"error": "no OS PID"});
                }
                // kill 整个进程组（负 PID = kill -- -$PGID）
                let pgid_arg = format!("-{}", os_pid);
                let killed = std::process::Command::new("kill")
                    .args([&pgid_arg])
                    .output()
                    .map(|o| o.status.success())
                    .unwrap_or(false);
                if killed {
                    let mut map = self.process_map.lock().await;
                    if let Some(info) = map.get_mut(&pid) {
                        info.status = "killed".into();
                    }
                    save_processes(&map, &self.storage.cwd, &self.storage.session_id);
                    let mut sm = self.stdin_map.lock().await;
                    sm.remove(&pid);
                    serde_json::json!({"status": "killed", "bid": pid})
                } else {
                    serde_json::json!({"error": "kill failed"})
                }
            }
            "send" => {
                let pid = parse_pid(params);
                let input = params
                    .get("input")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if pid.is_empty() {
                    return serde_json::json!({"error": "missing bid"});
                }
                if input.is_empty() {
                    return serde_json::json!({"error": "missing input"});
                }
                let mut sm = self.stdin_map.lock().await;
                match sm.get(&pid) {
                    Some(tx) => {
                        if tx.send(input.clone()).await.is_err() {
                            sm.remove(&pid);
                            serde_json::json!({"error": "stdin closed"})
                        } else {
                            serde_json::json!({"status": "delivered", "bid": pid, "input": input})
                        }
                    }
                    None => serde_json::json!({"error": "process not found or no stdin channel"}),
                }
            }
            _ => {
                serde_json::json!({"error": format!("unknown action: {action}. Use list/inspect/kill/send.")})
            }
        }
    }
}

pub struct BashExtension {
    pub process_map: ProcessMap,
    pub stdin_map: StdinMap,
    pub notify_map: NotifyMap,
    pub follow_up_tx: Option<FollowUpSender>,
    pub storage: crate::storage_context::StorageContext,
}

impl BashExtension {
    pub fn new(storage: crate::storage_context::StorageContext) -> Self {
        let processes = load_processes(&storage.cwd, &storage.session_id);
        Self {
            process_map: Arc::new(Mutex::new(processes)),
            stdin_map: new_stdin_map(),
            notify_map: Arc::new(Mutex::new(HashMap::new())),
            follow_up_tx: None,
            storage,
        }
    }

    /// 兼容旧签名（测试用）
    pub fn new_with_cwd(session_id: &str, cwd: &str) -> Self {
        Self::new(crate::storage_context::StorageContext::new(
            cwd, session_id, cwd,
        ))
    }

    /// Set the follow_up channel sender. Background processes (spawn_watcher)
    /// use this to inject <bash_result> messages back into the agent loop
    /// when they complete. Must be called after new() in worker startup,
    /// before register_tools() so that BashRunTool/BashManageTool get the tx.
    pub fn set_follow_up_tx(&mut self, tx: FollowUpSender) {
        self.follow_up_tx = Some(tx);
    }

    /// Dummy constructor for export-only contexts (no live worker loop).
    /// `register_tools` only clones the Arc fields, so a fresh empty state is
    /// enough — the resulting tools are never executed during export.
    pub fn new_for_export() -> Self {
        Self {
            process_map: Arc::new(Mutex::new(HashMap::new())),
            stdin_map: new_stdin_map(),
            notify_map: Arc::new(Mutex::new(HashMap::new())),
            follow_up_tx: None,
            storage: crate::storage_context::StorageContext::new(".", "export", "."),
        }
    }
}

fn parse_pid(params: &serde_json::Value) -> String {
    params
        .get("bid")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

#[async_trait]
impl Extension for BashExtension {
    /// Self-describing tool registration. Registers two tools:
    /// 1. `bash` — run commands (sync + background via parameter)
    /// 2. `get_background_process` / `kill_process` / `write_stdin` — manage background processes
    /// Management is a separate tool because extension_rpc is CLI-only and
    /// not available as an LLM tool inside worker sessions.
    fn register_tools(&self, registry: &mut crate::agent::tool::ToolRegistry) {
        registry.register(Box::new(BashRunTool {
            process_map: self.process_map.clone(),
            stdin_map: self.stdin_map.clone(),
            notify_map: self.notify_map.clone(),
            follow_up_tx: self.follow_up_tx.clone(),
            storage: self.storage.clone(),
        }));
        // 3 个独立管理工具（对标 pi），共享 BashManageTool 引擎
        let manage = BashManageTool {
            process_map: self.process_map.clone(),
            stdin_map: self.stdin_map.clone(),
            follow_up_tx: self.follow_up_tx.clone(),
            storage: self.storage.clone(),
        };
        registry.register(Box::new(GetBackgroundProcessTool {
            manage: manage.clone(),
        }));
        registry.register(Box::new(KillProcessTool {
            manage: manage.clone(),
        }));
        registry.register(Box::new(WriteStdinTool { manage }));
    }

    fn name(&self) -> &str {
        "bash"
    }

    async fn on_extension_rpc(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> AgentResult<serde_json::Value> {
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
                let head_lines = params.get("head").and_then(|v| v.as_u64()).unwrap_or(5) as usize;
                let tail_lines = params
                    .get("tailLines")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(5) as usize;
                let map = self.process_map.lock().await;
                match map.get(&pid) {
                    Some(info) => {
                        let output = &info.output;
                        let output_bytes = output.len();

                        // ── output_preview（保留向后兼容）──
                        let preview = if tail > 0 && output_bytes > tail {
                            format!(
                                "...[truncated {} bytes]\n{}",
                                output_bytes - tail,
                                &output[output_bytes.saturating_sub(tail)..]
                            )
                        } else if offset < output_bytes {
                            let end = (offset + limit).min(output_bytes);
                            let snippet = &output[offset..end];
                            if offset > 0 {
                                format!("[offset {offset}]\n{snippet}")
                            } else {
                                snippet.to_string()
                            }
                        } else {
                            String::new()
                        };

                        // ── 头尾行预览 ──
                        let all_lines: Vec<&str> = output.lines().collect();
                        let total_lines = all_lines.len();
                        let (output_head, output_tail, output_truncated) =
                            if total_lines > head_lines + tail_lines {
                                let head_joined = all_lines[..head_lines].join("\n");
                                let tail_joined = all_lines[total_lines - tail_lines..].join("\n");
                                let middle_lines = total_lines - head_lines - tail_lines;
                                (
                                    head_joined,
                                    format!(
                                        "...[truncated {} lines]...\n{}",
                                        middle_lines, tail_joined
                                    ),
                                    true,
                                )
                            } else {
                                (output.clone(), String::new(), false)
                            };

                        // ── 友好的输出大小 ──
                        let output_size_human = if output_bytes < 1024 {
                            format!("{} B", output_bytes)
                        } else if output_bytes < 1024 * 1024 {
                            format!("{:.1} KB", output_bytes as f64 / 1024.0)
                        } else {
                            format!("{:.1} MB", output_bytes as f64 / (1024.0 * 1024.0))
                        };

                        // ── started_at ISO ──
                        let started_at_iso = {
                            let secs = info.started_at / 1000;
                            let millis = info.started_at % 1000;
                            let days_since_epoch = secs / 86400;
                            let time_secs = secs % 86400;
                            let h = time_secs / 3600;
                            let m = (time_secs % 3600) / 60;
                            let s = time_secs % 60;
                            let mut y = 2025i64;
                            let mut days_remaining = days_since_epoch.saturating_sub(20089);
                            loop {
                                let diy = if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                                    366
                                } else {
                                    365
                                };
                                if days_remaining < diy {
                                    break;
                                }
                                days_remaining -= diy;
                                y += 1;
                            }
                            let md = if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                                [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
                            } else {
                                [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
                            };
                            let mut mo = 1i64;
                            for &d in &md {
                                if days_remaining < d {
                                    break;
                                }
                                days_remaining -= d;
                                mo += 1;
                            }
                            format!(
                                "{y:04}-{mo:02}-{}T{h:02}:{m:02}:{s:02}.{millis:03}Z",
                                days_remaining + 1
                            )
                        };

                        Ok(serde_json::json!({
                            "bid": info.bid, "command": info.command,
                            "description": info.description, "status": info.status,
                            "exit_code": info.exit_code, "background": info.background,
                            "elapsed_secs": info.elapsed_secs,
                            "started_at": started_at_iso,
                            "output_bytes": output_bytes,
                            "output_size": output_size_human,
                            "output_lines": total_lines,
                            "output_head": output_head,
                            "output_tail": output_tail,
                            "output_truncated": output_truncated,

                        }))
                    }
                    None => Ok(serde_json::json!({"error": "process not found"})),
                }
            }
            "kill" => {
                let pid = parse_pid(&params);
                if pid.is_empty() {
                    return Ok(serde_json::json!({"error": "missing pid"}));
                }
                let os_pid = {
                    let mut map = self.process_map.lock().await;
                    if let Some(info) = map.get_mut(&pid) {
                        info.status = "killed".into(); // 标记防止 watcher 覆盖
                        info.os_pid
                    } else {
                        0
                    }
                };
                if os_pid == 0 {
                    return Ok(serde_json::json!({"error": "no OS PID"}));
                }
                // kill 整个进程组（负 PID = kill -- -$PGID）
                let pgid_arg = format!("-{}", os_pid);
                let killed = std::process::Command::new("kill")
                    .args([&pgid_arg])
                    .output()
                    .map(|o| o.status.success())
                    .unwrap_or(false);
                if killed {
                    let mut map = self.process_map.lock().await;
                    if let Some(info) = map.get_mut(&pid) {
                        info.status = "killed".into();
                    }
                    save_processes(&map, &self.storage.cwd, &self.storage.session_id);
                    let mut sm = self.stdin_map.lock().await;
                    sm.remove(&pid);
                    // Notify LLM: kill 通知也精简，bid 放属性，content 只放状态
                    if let Some(ref tx) = self.follow_up_tx {
                        let content = format!(
                            "<bash_result bid=\"{}\" exit=\"killed\">\n🛑 Process killed by user\n</bash_result>",
                            pid,
                        );
                        let msg = Message::Custom(CustomMessage {
                            role: "custom".into(),
                            custom_type: "bash_result".into(),
                            content: CustomContent::Text(content),
                            display: true,
                            details: None,
                            timestamp: now_ms(),
                        });
                        // kill 是用户主动操作，用 Steer 中断当前 turn（让 LLM 立即看到 kill 通知）
                        let _ = tx.send((msg, DeliverAs::Steer));
                    }
                    Ok(serde_json::json!({"status": "killed"}))
                } else {
                    Ok(serde_json::json!({"error": "kill failed"}))
                }
            }
            "send" => {
                let pid = parse_pid(&params);
                let input = params
                    .get("input")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if pid.is_empty() {
                    return Ok(serde_json::json!({"error": "missing pid"}));
                }
                if input.is_empty() {
                    return Ok(serde_json::json!({"error": "missing input"}));
                }
                let mut sm = self.stdin_map.lock().await;
                match sm.get(&pid) {
                    Some(tx) => {
                        if tx.send(input.clone()).await.is_err() {
                            sm.remove(&pid);
                            Ok(serde_json::json!({"error": "stdin closed"}))
                        } else {
                            Ok(
                                serde_json::json!({"status": "delivered", "bid": pid, "input": input}),
                            )
                        }
                    }
                    None => {
                        Ok(serde_json::json!({"error": "process not found or no stdin channel"}))
                    }
                }
            }
            "clean" => {
                let mut map = self.process_map.lock().await;
                let mut sm = self.stdin_map.lock().await;
                let before = map.len();
                map.retain(|_, p| p.status == "running");
                sm.retain(|pid, _| map.contains_key(pid));
                let cleaned = before - map.len();
                save_processes(&map, &self.storage.cwd, &self.storage.session_id);
                Ok(serde_json::json!({"cleaned": cleaned}))
            }
            "remove" => {
                let bid = params
                    .get("bid")
                    .or_else(|| params.get("pid"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if bid.is_empty() {
                    return Err(AgentError::Tool("remove requires 'bid' or 'pid'".into()));
                }
                let mut map = self.process_map.lock().await;
                let mut sm = self.stdin_map.lock().await;
                let removed = map.remove(&bid).is_some();
                sm.remove(&bid);
                save_processes(&map, &self.storage.cwd, &self.storage.session_id);
                Ok(serde_json::json!({"removed": removed, "bid": bid}))
            }
            _ => Err(AgentError::Tool(format!(
                "bash extension_rpc: unknown method {method}"
            ))),
        }
    }

    /// 注入 bash 使用说明 + 当前后台进程摘要到 system prompt。
    /// 进程摘要只含 bid/command/elapsed/status（不含 output，避免占 token + 泄隐私），
    /// 让 LLM 感知到有哪些后台进程、运行多久、怎么管理（kill/inspect）。
    async fn on_system_prompt(&self, prompt: &mut String) -> AgentResult<()> {
        prompt.push_str(&bash_tool_guide());
        // 追加当前后台进程摘要（动态，每轮 turn 刷新）
        let map = self.process_map.lock().await;
        let active: Vec<_> = map
            .values()
            .filter(|p| p.status == "running" || p.status == "background")
            .collect();
        if !active.is_empty() {
            prompt.push_str(&format!(
                "\n### Active Background Processes ({})\n",
                active.len()
            ));
            prompt.push_str(
                "| bid | command | elapsed | status |\n|-----|---------|---------|--------|\n",
            );
            for p in &active {
                let cmd_short = if p.command.len() > 50 {
                    format!("{}...", &p.command[..50])
                } else {
                    p.command.clone()
                };
                prompt.push_str(&format!(
                    "| {} | `{}` | {:.0}s | {} |\n",
                    p.bid, cmd_short, p.elapsed_secs, p.status
                ));
            }
            prompt.push_str("\nManage via `extension_rpc(bash, inspect|kill|send)`. Use `inspect` to view output (truncated).\n");
        }
        Ok(())
    }
}

/// Bash 工具指南（注入 system prompt）。pub 让 cmd_run / export 都能调用。
/// 只给工具概述 + 管理方法（不含具体进程，进程摘要在 on_system_prompt 动态注入）。
pub fn bash_tool_guide() -> String {
    let bash_timeout = std::env::var("ION_BASH_TIMEOUT")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(180);
    format!(
        "\n\n--- bash-tool-guide ---\n\
## Bash Tool\n\
- `bash` tool: execute shell command. Supports `background=true` for long-running\n\
  commands (dev servers, builds, watches) — returns immediately with a process `bid`.\n\
  Foreground timeout: {bash_timeout}s (override via `ION_BASH_TIMEOUT`), or set `timeout` param.\n\
  Use `timeoutBackground=true` to auto-move to background on timeout.\n\
- Background process management via 3 dedicated tools:\n\
  - `get_background_process(bid=...)`: view status + output (head/tail truncation). Omit bid to list all.\n\
  - `kill_process(bid=...)`: kill a background process\n\
  - `write_stdin(bid=..., input=...)`: write text to process stdin\n\
- Commands run in cwd; use absolute paths for files outside cwd.\n"
    )
}

// ============================================================================
// Helpers
// ============================================================================

pub(super) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

pub(super) fn emit_extension_event(event_type: &str, data: &serde_json::Value) {
    // 注意：Manager 的 stdout 路由只识别 "type":"event"，
    // 所以 plugin_event 需要嵌在 event.type 里才能到达 subscriber
    let msg = serde_json::json!({
        "type": "event",
        "event": {
            "type": "extension_event",
            "extension": "bash",
            "customType": event_type,
            "visibility": "llm_and_ui",
            "timestamp": now_ms(),
            "data": data,
        },
    });
    println!("{}", serde_json::to_string(&msg).unwrap_or_default());
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ── 1. ProcessInfo struct construction ────────────────────────────────

    #[test]
    fn test_process_info() {
        let info = ProcessInfo {
            bid: "100000".to_string(),
            os_pid: 12345,
            command: "echo hello".to_string(),
            description: "say hello".to_string(),
            status: "completed".to_string(),
            exit_code: Some(0),
            output: "hello\n".to_string(),
            background: false,
            started_at: 1_700_000_000_000,
            elapsed_secs: 1,
        };
        assert_eq!(info.bid, "100000");
        assert_eq!(info.os_pid, 12345);
        assert_eq!(info.command, "echo hello");
        assert_eq!(info.description, "say hello");
        assert_eq!(info.status, "completed");
        assert_eq!(info.exit_code, Some(0));
        assert_eq!(info.output, "hello\n");
        assert!(!info.background);
        assert_eq!(info.started_at, 1_700_000_000_000);
        assert_eq!(info.elapsed_secs, 1);
    }

    // ── 2. BashExtension::new_with_cwd() — storage context session_id ─────

    #[test]
    fn test_bash_extension_new() {
        let ext = BashExtension::new_with_cwd("test-sid", "/tmp");
        assert_eq!(ext.storage.session_id, "test-sid");
        assert_eq!(ext.storage.cwd, "/tmp");
        // Maps should start empty for a fresh session.
        assert!(ext.process_map.try_lock().unwrap().is_empty());
        assert!(ext.stdin_map.try_lock().unwrap().is_empty());
        assert!(ext.notify_map.try_lock().unwrap().is_empty());
        assert!(ext.follow_up_tx.is_none());
    }

    // ── 3-6. Tool name methods ────────────────────────────────────────────

    #[test]
    fn test_bash_run_tool_name() {
        let tool = BashRunTool {
            process_map: Arc::new(Mutex::new(HashMap::new())),
            stdin_map: new_stdin_map(),
            notify_map: Arc::new(Mutex::new(HashMap::new())),
            follow_up_tx: None,
            storage: crate::storage_context::StorageContext::new("/tmp", "sid", "/tmp"),
        };
        assert_eq!(tool.name(), "bash");
    }

    // ── 7. Pure utility functions ─────────────────────────────────────────

    #[test]
    fn test_allocate_pid_empty_map() {
        // Empty map → first PID should be BASE = 36^5 = 60466176 → "100000" in base36.
        let map: HashMap<String, ProcessInfo> = HashMap::new();
        let pid = allocate_pid(&map);
        assert_eq!(pid, "100000");
        assert_eq!(pid.len(), 6);
    }

    #[test]
    fn test_allocate_pid_monotonic() {
        // With an existing PID at BASE, next allocation must be BASE+1.
        let mut map: HashMap<String, ProcessInfo> = HashMap::new();
        map.insert(
            "100000".to_string(),
            ProcessInfo {
                bid: "100000".to_string(),
                os_pid: 0,
                command: String::new(),
                description: String::new(),
                status: "running".into(),
                exit_code: None,
                output: String::new(),
                background: false,
                started_at: 0,
                elapsed_secs: 0,
            },
        );
        let pid = allocate_pid(&map);
        assert_eq!(pid, "100001");
    }

    #[test]
    fn test_parse_pid_present() {
        let params = serde_json::json!({"bid": "100005"});
        assert_eq!(parse_pid(&params), "100005");
    }

    #[test]
    fn test_parse_pid_missing() {
        // No "bid" key → empty string.
        let params = serde_json::json!({"foo": "bar"});
        assert_eq!(parse_pid(&params), "");
    }

    #[test]
    fn test_parse_pid_wrong_type() {
        // Non-string bid → empty string.
        let params = serde_json::json!({"bid": 42});
        assert_eq!(parse_pid(&params), "");
    }

    // ── stderr merge tests (commit a699d58) ──

    #[test]
    fn test_merge_stderr_to_stdout_basic() {
        let result = merge_stderr_to_stdout("echo hello");
        assert_eq!(result, "exec 2>&1 ; echo hello");
    }

    #[test]
    fn test_merge_stderr_to_stdout_preserves_complex_commands() {
        // 含 ; & | 等特殊字符的 command 也能正确拼接
        let cmd = "python3 -m http.server 9999 ; echo done";
        let result = merge_stderr_to_stdout(cmd);
        assert_eq!(result, format!("exec 2>&1 ; {}", cmd));
    }

    #[test]
    fn test_merge_stderr_to_stdout_empty_command() {
        // 空 command 也应该能拼接（边界情况）
        let result = merge_stderr_to_stdout("");
        assert_eq!(result, "exec 2>&1 ; ");
    }

    /// End-to-end: 用 tokio spawn 真实验证 stderr 被 redirect 到 stdout。
    /// 这是「为什么这个函数存在」的回归测试——如果有人移除了 exec 2>&1，
    /// 这测试会立刻 fail（因为 stderr 不会出现在 stdout pipe 里）。
    #[tokio::test]
    async fn test_stderr_actually_merged_in_real_spawn() {
        let merged = merge_stderr_to_stdout("echo STDOUT_LINE; echo STDERR_LINE >&2");
        let output = tokio::process::Command::new("sh")
            .args(["-c", &merged])
            .output()
            .await
            .expect("sh should spawn");
        let combined = String::from_utf8_lossy(&output.stdout);
        assert!(
            combined.contains("STDOUT_LINE"),
            "stdout should contain STDOUT_LINE: {combined:?}"
        );
        // ★ 关键断言：STDERR_LINE 也在 stdout pipe 里（被 exec 2>&1 重定向）
        assert!(
            combined.contains("STDERR_LINE"),
            "stderr should be merged into stdout: {combined:?}"
        );
    }

    /// 反向验证：不加 exec 2>&1 时，stderr 不会出现在 stdout（证明 merge 起作用）。
    #[tokio::test]
    async fn test_stderr_not_merged_without_wrapper() {
        let output = tokio::process::Command::new("sh")
            .args(["-c", "echo STDOUT_LINE; echo STDERR_LINE >&2"])
            .output()
            .await
            .expect("sh should spawn");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stdout.contains("STDOUT_LINE"));
        assert!(
            !stdout.contains("STDERR_LINE"),
            "without merge, stderr should NOT be in stdout"
        );
        assert!(
            stderr.contains("STDERR_LINE"),
            "without merge, stderr should be in stderr pipe"
        );
    }
}
