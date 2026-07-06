//! Runtime trait — 工具执行抽象层
//!
//! 三种模式，切换只需改配置：
//! - `LocalRuntime` → 本地直接执行（当前行为）
//! - `SandboxRuntime` → macOS sandbox-exec / Docker（未来）
//! - `RemoteRuntime` → RPC 到远程执行（未来）
//!
//! `SecuredRuntime` 是中间件包装，自动在 Runtime 外层加：
//! 1. PermissionEngine.check()
//! 2. CommandGuard.check()
//! 3. 审计日志

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::io::AsyncWriteExt;
use tokio::sync::oneshot;

/// 全局待处理的 UI 确认请求（request_id → 回复通道）
/// SecuredRuntime 写入，ion Manager 的 ui_respond handler 读取并回复。
pub static PENDING_UI: OnceLock<Mutex<HashMap<String, oneshot::Sender<String>>>> = OnceLock::new();
pub fn pending_ui() -> &'static Mutex<HashMap<String, oneshot::Sender<String>>> {
    PENDING_UI.get_or_init(|| Mutex::new(HashMap::new()))
}

// ---------------------------------------------------------------------------
// Runtime trait
// ---------------------------------------------------------------------------

#[async_trait]
pub trait Runtime: Send + Sync {
    /// 执行 shell 命令
    async fn execute_command(&self, command: &str, timeout_secs: u64)
        -> Result<(String, String, i32), String>;

    /// 安全预检：检查命令是否允许执行（不实际执行）
    /// 默认：放行。SecuredRuntime 重写此方法以检查 CommandGuard。
    async fn check_command(&self, _command: &str) -> Result<(), String> {
        Ok(())
    }

    /// 流式执行 shell 命令（逐行回调 on_update）
    /// 默认实现：调 execute_command 后一次性回调
    async fn execute_command_stream(
        &self,
        command: &str,
        timeout_secs: u64,
        on_update: &(dyn Fn(String) + Send + Sync),
    ) -> Result<String, String> {
        let (stdout, stderr, exit_code) = self.execute_command(command, timeout_secs).await?;
        let output = if stderr.is_empty() { stdout } else { format!("{stdout}{stderr}") };
        on_update(output.clone());
        if exit_code != 0 {
            Err(format!("exit code {exit_code}: {output}"))
        } else {
            Ok(output)
        }
    }

    /// 读文件
    async fn read_file(&self, path: &str) -> Result<String, String>;

    /// 写文件
    async fn write_file(&self, path: &str, content: &str) -> Result<(), String>;

    /// 替换文件内容
    async fn edit_file(&self, path: &str, old: &str, new: &str) -> Result<(), String>;

    /// 文件是否存在
    async fn path_exists(&self, path: &str) -> bool;

    /// 列出目录
    async fn list_dir(&self, path: &str) -> Result<Vec<String>, String>;

    /// 删除文件
    async fn remove_file(&self, path: &str) -> Result<(), String>;

    /// grep 搜索
    async fn grep_search(&self, pattern: &str, path: &str) -> Result<Vec<String>, String>;

    /// find 查找
    async fn find_files(&self, path: &str, name: &str) -> Result<Vec<String>, String>;

    /// 文件/目录信息（ls -la 等效）
    async fn file_info(&self, path: &str) -> Result<Vec<FileEntry>, String>;

    /// Runtime 类型名
    fn runtime_type(&self) -> String;

    // ── 进程管理能力（bash 工具的后台进程管理通过这里走，而不是直接 tokio::process）──

    /// 启动一个进程（前台同步或后台异步）。
    async fn spawn_process(&self, _req: SpawnProcessRequest) -> Result<ProcessHandle, String> {
        Err("runtime does not support process spawning".into())
    }

    /// 终止一个进程。
    async fn kill_process(&self, _os_pid: u32) -> Result<(), String> {
        Err("runtime does not support kill_process".into())
    }

    /// 向进程 stdin 写入。
    async fn send_stdin(&self, _os_pid: u32, _input: &str) -> Result<(), String> {
        Err("runtime does not support send_stdin".into())
    }

    // ── Worker 编排能力（默认返回 Err，保持 LocalRuntime 单进程行为）──
    //
    // 这两个方法把"内核能力"暴露给 Tool（进而暴露给 LLM）。
    // 设计原则对齐 AGENTS.md 第 4 条：能力在内核实现，通过 Runtime（Tool 的把手）暴露。
    //
    // - LocalRuntime（ion CLI 用）：默认实现返回 Err，单进程模式不支持多 Worker。
    // - WorkerRuntime（ion-worker 用）：包装 LocalRuntime，通过 ManagerBridge
    //   把请求转发给 Manager，由 Manager 创建子/同级 Worker。

    /// 创建一个 child 或 peer Worker。
    /// - relation=Child + wait=true：阻塞直到子 Worker 首轮 agent_end，返回 first_turn_output。
    /// - relation=Child + wait=false：立即返回 worker_id，后续用 await_worker 收结果（支持并行）。
    /// - relation=Peer：立即返回 worker_id，peer 完成后内核自动 follow_up 给 creator。
    async fn spawn_worker(&self, _req: SpawnWorkerRequest) -> Result<SpawnWorkerResponse, String> {
        Err("runtime does not support worker spawning (LocalRuntime default)".into())
    }

    /// 给指定 Worker 发消息（fire-and-forget，不等对方完成）。
    /// 用于：父→子追加指令、同级对话、creator→peer 追加指令。
    async fn send_to_worker(&self, _worker_id: &str, _text: &str) -> Result<(), String> {
        Err("runtime does not support worker messaging (LocalRuntime default)".into())
    }

    /// 给指定 Worker 发消息并阻塞等下一轮 agent_end（同步 resume 语义）。
    /// 返回目标 Worker 这一轮的输出。
    async fn resume_worker(&self, _worker_id: &str, _text: &str) -> Result<String, String> {
        Err("runtime does not support resume_worker (LocalRuntime default)".into())
    }

    /// 阻塞等指定 Worker 下一轮 agent_end，返回输出（用于 spawn_worker(wait=false) 后收结果）。
    async fn await_worker(&self, _worker_id: &str) -> Result<String, String> {
        Err("runtime does not support await_worker (LocalRuntime default)".into())
    }

    /// 频道广播（结构化，不靠文本协议）。
    async fn channel_send(&self, _channel: &str, _text: &str) -> Result<(), String> {
        Err("runtime does not support channel_send (LocalRuntime default)".into())
    }

    /// 终止指定 Worker。
    async fn kill_worker(&self, _worker_id: &str) -> Result<(), String> {
        Err("runtime does not support kill_worker (LocalRuntime default)".into())
    }
}

/// 文件/目录条目（ls 输出）
#[derive(Clone, Debug)]
pub struct FileEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: String,
}

// ---------------------------------------------------------------------------
// Worker 编排类型 — spawn_worker 工具的请求/响应
// ---------------------------------------------------------------------------

/// 与创建者的关系（与 WorkerRegistry::WorkerRelation 对齐）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpawnRelation {
    /// 父→子，同步。spawn_worker 阻塞到 child 首轮 agent_end。
    Child,
    /// creator→peer，异步。立即返回 worker_id。
    Peer,
}

/// spawn_worker 工具的请求参数。
#[derive(Clone, Debug)]
pub struct SpawnWorkerRequest {
    pub relation: SpawnRelation,
    /// 加载哪个 agent（对应 .ion/agents/<agent>.md）。
    pub agent: String,
    /// 创建后立即注入的初始任务描述。
    pub task: String,
    /// 可选：worker 名称（用于日志/引用）。
    pub name: Option<String>,
    /// Peer 模式下汇报频道（默认 "main"）。
    pub report_channel: Option<String>,
    /// Child 模式下是否阻塞等首轮完成。
    /// - true（默认）：阻塞，返回 first_turn_output
    /// - false：立即返回 worker_id，后续用 await_worker 收结果（支持并行）
    pub wait: bool,
}

/// spawn_worker 工具的响应。
#[derive(Clone, Debug)]
pub struct SpawnWorkerResponse {
    pub worker_id: String,
    pub relation: SpawnRelation,
    /// "first_turn_completed"（child+wait）/ "running_in_background"（child+!wait 或 peer）
    pub status: String,
    /// Child+wait 模式下，子 Worker 首轮的完整输出。其他模式为 None。
    pub first_turn_output: Option<String>,
    /// Peer 模式下汇报频道。
    pub report_channel: Option<String>,
}

// ---------------------------------------------------------------------------
// 进程管理类型 — spawn_process 工具的请求/响应
// ---------------------------------------------------------------------------

/// spawn_process 工具的请求参数。
#[derive(Clone, Debug)]
pub struct SpawnProcessRequest {
    /// shell 命令
    pub command: String,
    /// 超时秒数（仅 background=false 时生效）
    pub timeout_secs: u64,
    /// true=后台立即返回；false=前台等待完成
    pub background: bool,
    /// 日志文件路径（如 /tmp/ion-bash/{bid}.log），None 表示不写日志
    pub log_path: Option<String>,
}

/// spawn_process 的响应 / 进程句柄。
#[derive(Clone, Debug)]
pub struct ProcessHandle {
    /// 分配的进程 ID（bash.rs 的 bid，6 位 hex）
    pub bid: String,
    /// 真实的 OS PID（用于 kill 信号）
    pub os_pid: u32,
    /// 前台模式下的 stdout（background=false）
    pub stdout: String,
    /// 前台模式下的 stderr
    pub stderr: String,
    /// 退出码（前台模式填充；后台模式为 None 直到进程结束）
    pub exit_code: Option<i32>,
}

// ---------------------------------------------------------------------------
// ManagerBridgeHandle — Worker 进程与 Manager 通信的 trait（运行时抽象）
// ---------------------------------------------------------------------------
//
// WorkerRuntime 通过这个 trait 把 manager_command 转发出去。
// 实现可以是真实的 stdout JSON 通道（ion_worker），也可以是测试桩。
//
// 这层抽象让 runtime.rs（在 ion 库里）不依赖 ion_worker 二进制的具体类型。

#[async_trait]
pub trait ManagerBridgeHandle: Send + Sync {
    /// 发送 manager_command 并 await 响应。
    /// params 由调用方提供，handle 实现负责注入 _reply_to / _from_worker。
    async fn send_command(
        &self,
        command: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String>;
}

// ---------------------------------------------------------------------------
// WorkerRuntime — 包装任意 Runtime，加 Worker 编排能力
// ---------------------------------------------------------------------------
//
// ion-worker 用 `WorkerRuntime::new(LocalRuntime::new(), bridge)`。
// ion CLI 直接用 `LocalRuntime`（无编排能力）。

pub struct WorkerRuntime<R: Runtime> {
    inner: R,
    bridge: std::sync::Arc<dyn ManagerBridgeHandle>,
}

impl<R: Runtime> WorkerRuntime<R> {
    pub fn new(inner: R, bridge: std::sync::Arc<dyn ManagerBridgeHandle>) -> Self {
        Self { inner, bridge }
    }
}

#[async_trait]
impl<R: Runtime + 'static> Runtime for WorkerRuntime<R> {
    async fn execute_command(&self, command: &str, timeout_secs: u64)
        -> Result<(String, String, i32), String>
    { self.inner.execute_command(command, timeout_secs).await }

    async fn execute_command_stream(
        &self, command: &str, timeout_secs: u64,
        on_update: &(dyn Fn(String) + Send + Sync),
    ) -> Result<String, String> {
        self.inner.execute_command_stream(command, timeout_secs, on_update).await
    }

    async fn read_file(&self, path: &str) -> Result<String, String> {
        self.inner.read_file(path).await
    }
    async fn write_file(&self, path: &str, content: &str) -> Result<(), String> {
        self.inner.write_file(path, content).await
    }
    async fn edit_file(&self, path: &str, old: &str, new: &str) -> Result<(), String> {
        self.inner.edit_file(path, old, new).await
    }
    async fn path_exists(&self, path: &str) -> bool { self.inner.path_exists(path).await }
    async fn list_dir(&self, path: &str) -> Result<Vec<String>, String> {
        self.inner.list_dir(path).await
    }
    async fn remove_file(&self, path: &str) -> Result<(), String> {
        self.inner.remove_file(path).await
    }
    async fn grep_search(&self, pattern: &str, path: &str) -> Result<Vec<String>, String> {
        self.inner.grep_search(pattern, path).await
    }
    async fn find_files(&self, path: &str, name: &str) -> Result<Vec<String>, String> {
        self.inner.find_files(path, name).await
    }
    async fn file_info(&self, path: &str) -> Result<Vec<FileEntry>, String> {
        self.inner.file_info(path).await
    }
    fn runtime_type(&self) -> String {
        format!("worker({})", self.inner.runtime_type())
    }

    async fn spawn_worker(&self, req: SpawnWorkerRequest) -> Result<SpawnWorkerResponse, String> {
        let relation_str = match req.relation {
            SpawnRelation::Child => "child",
            SpawnRelation::Peer => "peer",
        };
        let params = serde_json::json!({
            "relation": relation_str,
            "agent": req.agent,
            "initial_prompt": req.task,
            "name": req.name,
            "report_channel": req.report_channel,
            "wait": req.wait,           // Child 模式下：true=阻塞, false=立即返回
            "creator": null,            // Manager 会用 _from_worker 填充
        });
        let resp = self.bridge.send_command("create_worker", params).await?;

        let success = resp.get("success").and_then(|v| v.as_bool()).unwrap_or(false);
        if !success {
            let err = resp.get("error").and_then(|v| v.as_str()).unwrap_or("unknown error");
            return Err(err.to_string());
        }

        let data = resp.get("data").cloned().unwrap_or_default();
        let worker_id = data.get("worker_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let relation_str_back = data.get("relation").and_then(|v| v.as_str()).unwrap_or(relation_str);
        let status = data.get("status").and_then(|v| v.as_str()).unwrap_or("running_in_background").to_string();
        let first_turn_output = data.get("first_turn_output").and_then(|v| v.as_str()).map(String::from);
        let report_channel = data.get("report_channel").and_then(|v| v.as_str()).map(String::from);

        let relation = match relation_str_back {
            "peer" => SpawnRelation::Peer,
            _ => SpawnRelation::Child,
        };

        Ok(SpawnWorkerResponse { worker_id, relation, status, first_turn_output, report_channel })
    }

    async fn send_to_worker(&self, worker_id: &str, text: &str) -> Result<(), String> {
        let params = serde_json::json!({ "target": worker_id, "text": text });
        let resp = self.bridge.send_command("send_to_worker", params).await?;
        let success = resp.get("success").and_then(|v| v.as_bool()).unwrap_or(false);
        if !success {
            let err = resp.get("error").and_then(|v| v.as_str()).unwrap_or("unknown error");
            return Err(err.to_string());
        }
        Ok(())
    }

    async fn resume_worker(&self, worker_id: &str, text: &str) -> Result<String, String> {
        // 同步 resume：发消息 + 阻塞等下一轮 agent_end
        let params = serde_json::json!({ "target": worker_id, "text": text });
        let resp = self.bridge.send_command("resume_worker", params).await?;
        let success = resp.get("success").and_then(|v| v.as_bool()).unwrap_or(false);
        if !success {
            let err = resp.get("error").and_then(|v| v.as_str()).unwrap_or("unknown error");
            return Err(err.to_string());
        }
        let data = resp.get("data").cloned().unwrap_or_default();
        Ok(data.get("response_output").and_then(|v| v.as_str()).unwrap_or("").to_string())
    }

    async fn await_worker(&self, worker_id: &str) -> Result<String, String> {
        let params = serde_json::json!({ "target": worker_id });
        let resp = self.bridge.send_command("await_worker", params).await?;
        let success = resp.get("success").and_then(|v| v.as_bool()).unwrap_or(false);
        if !success {
            let err = resp.get("error").and_then(|v| v.as_str()).unwrap_or("unknown error");
            return Err(err.to_string());
        }
        let data = resp.get("data").cloned().unwrap_or_default();
        Ok(data.get("first_turn_output").and_then(|v| v.as_str()).unwrap_or("").to_string())
    }

    async fn channel_send(&self, channel: &str, text: &str) -> Result<(), String> {
        let params = serde_json::json!({ "channel": channel, "msg": {"text": text} });
        let resp = self.bridge.send_command("channel_send", params).await?;
        let success = resp.get("success").and_then(|v| v.as_bool()).unwrap_or(false);
        if !success {
            let err = resp.get("error").and_then(|v| v.as_str()).unwrap_or("unknown error");
            return Err(err.to_string());
        }
        Ok(())
    }

    async fn kill_worker(&self, worker_id: &str) -> Result<(), String> {
        let params = serde_json::json!({ "target": worker_id });
        let resp = self.bridge.send_command("kill_worker", params).await?;
        let success = resp.get("success").and_then(|v| v.as_bool()).unwrap_or(false);
        if !success {
            let err = resp.get("error").and_then(|v| v.as_str()).unwrap_or("unknown error");
            return Err(err.to_string());
        }
        Ok(())
    }

    async fn spawn_process(&self, req: SpawnProcessRequest) -> Result<ProcessHandle, String> {
        self.inner.spawn_process(req).await
    }

    async fn kill_process(&self, os_pid: u32) -> Result<(), String> {
        self.inner.kill_process(os_pid).await
    }

    async fn send_stdin(&self, os_pid: u32, input: &str) -> Result<(), String> {
        self.inner.send_stdin(os_pid, input).await
    }
}

// ---------------------------------------------------------------------------
// LocalRuntime — 本地直接执行（当前行为封装）
// ---------------------------------------------------------------------------

pub struct LocalRuntime {
    /// 追踪后台进程：os_pid → (child, stdin, log_path)
    processes: Arc<Mutex<HashMap<u32, ProcessEntry>>>,
}

struct ProcessEntry {
    child: tokio::process::Child,
    stdin: Option<tokio::process::ChildStdin>,
    log_path: Option<String>,
}

impl LocalRuntime {
    pub fn new() -> Self {
        Self {
            processes: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl Default for LocalRuntime { fn default() -> Self { Self::new() } }

#[async_trait]
impl Runtime for LocalRuntime {
    fn runtime_type(&self) -> String { "local".into() }

    async fn execute_command(&self, command: &str, timeout_secs: u64) -> Result<(String, String, i32), String> {
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs),
            tokio::process::Command::new("sh").args(["-c", command]).output(),
        ).await.map_err(|_| format!("timeout after {timeout_secs}s"))?
         .map_err(|e| format!("spawn failed: {e}"))?;
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        Ok((stdout, stderr, output.status.code().unwrap_or(-1)))
    }

    /// 流式执行：逐行读到 stdout，每行调 on_update
    async fn execute_command_stream(
        &self,
        command: &str,
        timeout_secs: u64,
        on_update: &(dyn Fn(String) + Send + Sync),
    ) -> Result<String, String> {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let mut child = tokio::process::Command::new("sh")
            .args(["-c", &format!("{command} 2>&1")])
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("spawn failed: {e}"))?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let mut reader = BufReader::new(stdout).lines();
        let mut full = String::new();
        loop {
            tokio::select! {
                line = reader.next_line() => {
                    match line {
                        Ok(Some(l)) => { full.push_str(&l); full.push('\n'); on_update(full.clone()); }
                        Ok(None) => break,
                        Err(e) => return Err(format!("read: {e}")),
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_secs(timeout_secs)) => {
                    let _ = child.start_kill();
                    return Err(format!("timeout after {timeout_secs}s"));
                }
            }
        }
        let status = child.wait().await.map_err(|e| format!("wait: {e}"))?;
        if !status.success() {
            return Err(format!("exit code {}: {}", status.code().unwrap_or(-1), full));
        }
        Ok(full)
    }

    async fn read_file(&self, path: &str) -> Result<String, String> {
        tokio::fs::read_to_string(path).await.map_err(|e| format!("read {path}: {e}"))
    }

    async fn write_file(&self, path: &str, content: &str) -> Result<(), String> {
        if let Some(parent) = std::path::Path::new(path).parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| format!("mkdir: {e}"))?;
        }
        tokio::fs::write(path, content).await.map_err(|e| format!("write {path}: {e}"))
    }

    async fn edit_file(&self, path: &str, old: &str, new: &str) -> Result<(), String> {
        let content = self.read_file(path).await?;
        if !content.contains(old) {
            return Err(format!("edit {path}: pattern not found"));
        }
        let new_content = content.replace(old, new);
        self.write_file(path, &new_content).await
    }

    async fn path_exists(&self, path: &str) -> bool {
        tokio::fs::metadata(path).await.is_ok()
    }

    async fn list_dir(&self, path: &str) -> Result<Vec<String>, String> {
        let mut entries = tokio::fs::read_dir(path).await.map_err(|e| format!("ls {path}: {e}"))?;
        let mut names = Vec::new();
        while let Some(entry) = entries.next_entry().await.map_err(|e| format!("ls {path}: {e}"))? {
            names.push(entry.file_name().to_string_lossy().to_string());
        }
        names.sort();
        Ok(names)
    }

    async fn remove_file(&self, path: &str) -> Result<(), String> {
        tokio::fs::remove_file(path).await.map_err(|e| format!("rm {path}: {e}"))
    }

    async fn grep_search(&self, pattern: &str, path: &str) -> Result<Vec<String>, String> {
        let (stdout, _, _) = self.execute_command(&format!("grep -rn '{pattern}' '{path}' 2>/dev/null || true"), 30).await?;
        Ok(stdout.lines().map(String::from).collect())
    }

    async fn find_files(&self, path: &str, name: &str) -> Result<Vec<String>, String> {
        let (stdout, _, _) = self.execute_command(&format!("find '{path}' -name '{name}' 2>/dev/null || true"), 30).await?;
        Ok(stdout.lines().map(String::from).filter(|l| !l.is_empty()).collect())
    }

    async fn file_info(&self, path: &str) -> Result<Vec<FileEntry>, String> {
        let (stdout, _, _) = self.execute_command(&format!("ls -la '{path}' 2>/dev/null || true"), 30).await?;
        let mut entries = Vec::new();
        for line in stdout.lines().skip(1) {
            if line.is_empty() { continue; }
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 9 {
                let name = parts[8..].join(" ");
                let is_dir = line.starts_with('d');
                let size = parts[4].parse().unwrap_or(0);
                entries.push(FileEntry { name, is_dir, size, modified: parts[5..8].join(" ") });
            }
        }
        Ok(entries)
    }

    async fn spawn_process(&self, req: SpawnProcessRequest) -> Result<ProcessHandle, String> {
        let mut child = tokio::process::Command::new("sh")
            .args(["-c", &req.command])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn failed: {e}"))?;

        let os_pid = child.id().ok_or("no pid assigned")?;
        // 生成 bid（单调递增，6 位 hex）
        static NEXT_BID: AtomicU32 = AtomicU32::new(1);
        let bid = format!("{:06x}", NEXT_BID.fetch_add(1, Ordering::Relaxed) & 0xFFFFFF);

        if req.background {
            // ── 后台模式：立即返回，存储 child 供后续 kill/send_stdin ──
            let stdin = child.stdin.take();
            let log_path = req.log_path.clone();
            let mut map = self.processes.lock().map_err(|e| e.to_string())?;
            map.insert(os_pid, ProcessEntry { child, stdin, log_path });
            Ok(ProcessHandle {
                bid,
                os_pid,
                stdout: String::new(),
                stderr: String::new(),
                exit_code: None,
            })
        } else {
            // ── 前台模式：等待进程退出，收集完整输出 ──
            let timeout = std::time::Duration::from_secs(req.timeout_secs);
            let output = tokio::time::timeout(timeout, child.wait_with_output())
                .await
                .map_err(|_| format!("timeout after {}s", req.timeout_secs))?
                .map_err(|e| format!("wait failed: {e}"))?;
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            Ok(ProcessHandle {
                bid,
                os_pid,
                stdout,
                stderr,
                exit_code: output.status.code(),
            })
        }
    }

    async fn kill_process(&self, os_pid: u32) -> Result<(), String> {
        // 先从进程表查，有托管进程则优雅终止
        let should_remove = {
            let mut map = self.processes.lock().map_err(|e| e.to_string())?;
            if let Some(entry) = map.get_mut(&os_pid) {
                let _ = entry.child.start_kill();
                true
            } else {
                false
            }
        };
        if should_remove {
            self.processes.lock().map_err(|e| e.to_string())?.remove(&os_pid);
        }
        // fallback: 用系统 kill 命令确保终止
        let output = std::process::Command::new("kill")
            .args([&os_pid.to_string()])
            .output()
            .map_err(|e| format!("kill failed: {e}"))?;
        if output.status.success() {
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // 如果进程已不存在也算成功
            if stderr.contains("No such process") {
                Ok(())
            } else {
                Err(format!("kill failed: {stderr}"))
            }
        }
    }

    async fn send_stdin(&self, os_pid: u32, input: &str) -> Result<(), String> {
        let mut stdin = {
            let mut map = self.processes.lock().map_err(|e| e.to_string())?;
            let entry = map.get_mut(&os_pid).ok_or_else(|| format!("process {os_pid} not found"))?;
            entry.stdin.take().ok_or_else(|| format!("process {os_pid} has no stdin"))?
        };
        stdin.write_all(input.as_bytes()).await.map_err(|e| format!("stdin write: {e}"))?;
        stdin.flush().await.map_err(|e| format!("stdin flush: {e}"))?;
        // 把 stdin 放回去（cat 需要多次写入）
        let mut map = self.processes.lock().map_err(|e| e.to_string())?;
        if let Some(entry) = map.get_mut(&os_pid) {
            entry.stdin = Some(stdin);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// SecuredRuntime — 中间件包装（权限 + 守卫 + 审计）
// ---------------------------------------------------------------------------

pub struct SecuredRuntime<R: Runtime> {
    inner: R,
    permission_engine: Option<std::sync::Arc<crate::kernel::PermissionEngine>>,
    command_guard: Option<crate::command_guard::CommandGuard>,
    ui_system: Option<std::sync::Arc<crate::kernel::UiSystem>>,
    /// EventBus，用于异步 Ask 推送 UI 事件
    event_bus: Option<std::sync::Arc<tokio::sync::Mutex<crate::event_bus::ExtensionEventBus>>>,
}

impl<R: Runtime> SecuredRuntime<R> {
    pub fn new(inner: R) -> Self {
        Self { inner, permission_engine: None, command_guard: None, ui_system: None, event_bus: None }
    }

    pub fn with_permissions(mut self, engine: std::sync::Arc<crate::kernel::PermissionEngine>) -> Self {
        self.permission_engine = Some(engine);
        self
    }

    pub fn with_command_guard(mut self, guard: crate::command_guard::CommandGuard) -> Self {
        self.command_guard = Some(guard);
        self
    }

    pub fn with_ui(mut self, ui: std::sync::Arc<crate::kernel::UiSystem>) -> Self {
        self.ui_system = Some(ui);
        self
    }

    pub fn with_event_bus(mut self, bus: std::sync::Arc<tokio::sync::Mutex<crate::event_bus::ExtensionEventBus>>) -> Self {
        self.event_bus = Some(bus);
        self
    }

    /// 按安全配置文件一键设置权限引擎 + 命令守卫
    pub fn with_profile(mut self, profile: crate::kernel::SecurityProfile) -> Self {
        let engine = std::sync::Arc::new(crate::kernel::PermissionEngine::new());
        let mut guard = crate::command_guard::CommandGuard::default();
        profile.setup(&engine, &mut guard);
        self.permission_engine = Some(engine);
        self.command_guard = Some(guard);
        // 如果还没设置 UI，建一个默认的
        if self.ui_system.is_none() {
            self.ui_system = Some(std::sync::Arc::new(crate::kernel::UiSystem::new()));
        }
        self
    }

    /// 获取内部 Runtime 引用
    pub fn inner(&self) -> &R { &self.inner }

    /// 处理 Ask 结果：
    /// 1. 有 UiSystem 且有 confirm_handler → 同步确认（现有路径）
    /// 2. 有 EventBus → 异步 Ask（发 UI 事件 → 等回复 → 推 AskResolved）
    /// 3. 都没有 → 安全优先，拒绝
    async fn resolve_ask(&self, title: &str, message: &str) -> bool {
        // 路径 1：同步确认
        if let Some(ref ui) = self.ui_system {
            if ui.has_confirm_handler() {
                return ui.confirm(title, message);
            }
        }
        // 路径 2：异步 Ask 走 UI 通道
        if let Some(ref bus_arc) = self.event_bus {
            let request_id = format!("req_{}", &uuid::Uuid::new_v4().to_string()[..8]);
            
            // 注册 pending request
            let (tx, rx) = oneshot::channel();
            pending_ui().lock().unwrap().insert(request_id.clone(), tx);

            // 推 Ask 事件
            let ask_event = crate::event_bus::ExtensionEvent::new_ui("Ask", title, message)
                .with_data(serde_json::json!({"request_id": request_id, "title": title, "message": message}));
            {
                let mut bus = bus_arc.lock().await;
                bus.broadcast(&ask_event);
            }

            // 等待回复（带超时）
            let timeout = std::time::Duration::from_secs(120);
            let result = tokio::time::timeout(timeout, rx).await;

            let (allowed, response_str) = match result {
                Ok(Ok(resp)) => (resp == "allow", resp),
                _ => (false, "timeout".into()),
            };

            // 推 AskResolved / AskTimedOut 事件
            let resolved_type = if response_str == "timeout" { "AskTimedOut" } else { "AskResolved" };
            let resolved_event = crate::event_bus::ExtensionEvent::new_ui(resolved_type, title, message)
                .with_data(serde_json::json!({"request_id": request_id, "response": response_str}));
            {
                let mut bus = bus_arc.lock().await;
                bus.broadcast(&resolved_event);
            }

            return allowed;
        }
        // 路径 3：都没有 → 拒绝
        false
    }
}

#[async_trait]
impl<R: Runtime + Send + Sync> Runtime for SecuredRuntime<R> {
    fn runtime_type(&self) -> String {
        format!("secured({})", self.inner.runtime_type())
    }

    async fn execute_command(&self, command: &str, timeout_secs: u64) -> Result<(String, String, i32), String> {
        // CommandGuard 检查
        if let Some(ref guard) = self.command_guard {
            match guard.check(command) {
                crate::command_guard::GuardDecision::Deny(p) => {
                    let msg = if let Some(ref sug) = p.suggestion {
                        format!("[CommandGuard] 高危命令被拦截: {} 建议: {}", p.message, sug)
                    } else {
                        format!("[CommandGuard] 高危命令被拦截: {}", p.message)
                    };
                    return Err(msg);
                }
                crate::command_guard::GuardDecision::Ask(p) => {
                    let msg = format!("{}\n\n命令: `{}`", p.message, command);
                    if !self.resolve_ask("高危命令", &msg).await {
                        let hint = p.suggestion.as_ref().map(|s| format!(" 建议: {}", s)).unwrap_or_default();
                        return Err(format!("[CommandGuard] 用户拒绝了高危命令: {}{}", p.message, hint));
                    }
                    // 用户允许 → 放行
                }
                crate::command_guard::GuardDecision::Allow => {}
            }
        }
        self.inner.execute_command(command, timeout_secs).await
    }

    /// 流式执行也走 CommandGuard 检查
    async fn execute_command_stream(
        &self, command: &str, timeout_secs: u64,
        on_update: &(dyn Fn(String) + Send + Sync),
    ) -> Result<String, String> {
        if let Some(ref guard) = self.command_guard {
            match guard.check(command) {
                crate::command_guard::GuardDecision::Deny(p) => {
                    let msg = if let Some(ref sug) = p.suggestion {
                        format!("[CommandGuard] 高危命令被拦截: {} 建议: {}", p.message, sug)
                    } else {
                        format!("[CommandGuard] 高危命令被拦截: {}", p.message)
                    };
                    return Err(msg);
                }
                crate::command_guard::GuardDecision::Ask(p) => {
                    let msg = format!("{}\n\n命令: `{}`", p.message, command);
                    if !self.resolve_ask("高危命令", &msg).await {
                        let hint = p.suggestion.as_ref().map(|s| format!(" 建议: {}", s)).unwrap_or_default();
                        return Err(format!("[CommandGuard] 用户拒绝了高危命令: {}{}", p.message, hint));
                    }
                }
                crate::command_guard::GuardDecision::Allow => {}
            }
        }
        self.inner.execute_command_stream(command, timeout_secs, on_update).await
    }

    /// 安全预检：检查命令是否允许（CommandGuard）
    async fn check_command(&self, command: &str) -> Result<(), String> {
        if let Some(ref guard) = self.command_guard {
            match guard.check(command) {
                crate::command_guard::GuardDecision::Deny(p) => {
                    let msg = if let Some(ref sug) = p.suggestion {
                        format!("[CommandGuard] 高危命令被拦截: {} 建议: {}", p.message, sug)
                    } else {
                        format!("[CommandGuard] 高危命令被拦截: {}", p.message)
                    };
                    return Err(msg);
                }
                crate::command_guard::GuardDecision::Ask(p) => {
                    let msg = format!("{}\n\n命令: `{}`", p.message, command);
                    if !self.resolve_ask("高危命令", &msg).await {
                        let hint = p.suggestion.as_ref().map(|s| format!(" 建议: {}", s)).unwrap_or_default();
                        return Err(format!("[CommandGuard] 用户拒绝了高危命令: {}{}", p.message, hint));
                    }
                }
                crate::command_guard::GuardDecision::Allow => {}
            }
        }
        Ok(())
    }

    async fn read_file(&self, path: &str) -> Result<String, String> {
        if let Some(ref engine) = self.permission_engine {
            match engine.check(path, crate::kernel::Action::Read) {
                crate::kernel::PermissionResult::Allow => {}
                crate::kernel::PermissionResult::Deny(reason) => return Err(format!("[Permission] {reason}")),
                crate::kernel::PermissionResult::Ask { title, message } => {
                    if !self.resolve_ask(&title, &message).await {
                        return Err(format!("[Permission] 用户拒绝了读文件: {path}"));
                    }
                }
            }
        }
        self.inner.read_file(path).await
    }

    async fn write_file(&self, path: &str, content: &str) -> Result<(), String> {
        if let Some(ref engine) = self.permission_engine {
            match engine.check(path, crate::kernel::Action::Write) {
                crate::kernel::PermissionResult::Allow => {}
                crate::kernel::PermissionResult::Deny(reason) => return Err(format!("[Permission] {reason}")),
                crate::kernel::PermissionResult::Ask { title, message } => {
                    if !self.resolve_ask(&title, &message).await {
                        return Err(format!("[Permission] 用户拒绝了写文件: {path}"));
                    }
                }
            }
        }
        self.inner.write_file(path, content).await
    }

    async fn edit_file(&self, path: &str, old: &str, new: &str) -> Result<(), String> {
        if let Some(ref engine) = self.permission_engine {
            match engine.check(path, crate::kernel::Action::Edit) {
                crate::kernel::PermissionResult::Allow => {}
                crate::kernel::PermissionResult::Deny(reason) => return Err(format!("[Permission] {reason}")),
                crate::kernel::PermissionResult::Ask { title, message } => {
                    if !self.resolve_ask(&title, &message).await {
                        return Err(format!("[Permission] 用户拒绝了编辑文件: {path}"));
                    }
                }
            }
        }
        self.inner.edit_file(path, old, new).await
    }

    async fn path_exists(&self, path: &str) -> bool { self.inner.path_exists(path).await }

    async fn list_dir(&self, path: &str) -> Result<Vec<String>, String> {
        if let Some(ref engine) = self.permission_engine {
            match engine.check(path, crate::kernel::Action::Read) {
                crate::kernel::PermissionResult::Allow => {}
                crate::kernel::PermissionResult::Deny(reason) => return Err(format!("[Permission] {reason}")),
                crate::kernel::PermissionResult::Ask { title, message } => {
                    if !self.resolve_ask(&title, &message).await {
                        return Err(format!("[Permission] 用户拒绝了: {path}"));
                    }
                }
            }
        }
        self.inner.list_dir(path).await
    }

    async fn remove_file(&self, path: &str) -> Result<(), String> {
        if let Some(ref engine) = self.permission_engine {
            match engine.check(path, crate::kernel::Action::Delete) {
                crate::kernel::PermissionResult::Allow => {}
                crate::kernel::PermissionResult::Deny(reason) => return Err(format!("[Permission] {reason}")),
                crate::kernel::PermissionResult::Ask { title, message } => {
                    if !self.resolve_ask(&title, &message).await {
                        return Err(format!("[Permission] 用户拒绝了删除: {path}"));
                    }
                }
            }
        }
        self.inner.remove_file(path).await
    }

    async fn grep_search(&self, pattern: &str, path: &str) -> Result<Vec<String>, String> {
        if let Some(ref engine) = self.permission_engine {
            match engine.check(path, crate::kernel::Action::Read) {
                crate::kernel::PermissionResult::Allow => {}
                crate::kernel::PermissionResult::Deny(reason) => return Err(format!("[Permission] {reason}")),
                crate::kernel::PermissionResult::Ask { title, message } => {
                    if !self.resolve_ask(&title, &message).await {
                        return Err(format!("[Permission] 用户拒绝了搜索: {path}"));
                    }
                }
            }
        }
        self.inner.grep_search(pattern, path).await
    }

    async fn find_files(&self, path: &str, name: &str) -> Result<Vec<String>, String> {
        self.inner.find_files(path, name).await
    }

    async fn file_info(&self, path: &str) -> Result<Vec<FileEntry>, String> {
        self.inner.file_info(path).await
    }

    async fn spawn_process(&self, req: SpawnProcessRequest) -> Result<ProcessHandle, String> {
        // CommandGuard 检查命令
        if let Some(ref guard) = self.command_guard {
            match guard.check(&req.command) {
                crate::command_guard::GuardDecision::Deny(p) => {
                    let sug = p.suggestion.as_deref().unwrap_or("");
                    return Err(format!("spawn rejected: {} ({})", p.message, sug));
                }
                crate::command_guard::GuardDecision::Ask(p) => {
                    let allowed = self.resolve_ask("command", &p.message).await;
                    if !allowed {
                        let sug = p.suggestion.as_deref().unwrap_or("");
                        return Err(format!("spawn denied by user: {} ({})", p.message, sug));
                    }
                }
                crate::command_guard::GuardDecision::Allow => {}
            }
        }
        self.inner.spawn_process(req).await
    }

    async fn kill_process(&self, os_pid: u32) -> Result<(), String> {
        // PermissionEngine 检查 kill 操作
        if let Some(ref engine) = self.permission_engine {
            let path = format!("/proc/{}", os_pid);
            match engine.check(&path, crate::kernel::Action::Execute) {
                crate::kernel::PermissionResult::Deny(reason) => {
                    return Err(format!("kill denied: {reason}"));
                }
                crate::kernel::PermissionResult::Ask { title, message } => {
                    let allowed = self.resolve_ask(&title, &message).await;
                    if !allowed {
                        return Err(format!("kill denied by user: {message}"));
                    }
                }
                crate::kernel::PermissionResult::Allow => {}
            }
        }
        self.inner.kill_process(os_pid).await
    }

    async fn send_stdin(&self, os_pid: u32, input: &str) -> Result<(), String> {
        self.inner.send_stdin(os_pid, input).await
    }
}

// ---------------------------------------------------------------------------
// RemoteRuntime — 远程执行（SSH / HTTP / gRPC）
// ---------------------------------------------------------------------------

/// 远程执行 Runtime：将命令通过 SSH 转发到远程主机执行。
pub struct RemoteRuntime<R: Runtime> {
    inner: R,
    host_user: String,
    host_hostname: String,
    host_port: u16,
    host_key: String,
    host_proxy_jump: String,
}

impl<R: Runtime> RemoteRuntime<R> {
    pub fn new(inner: R, user: &str, hostname: &str, port: u16, key: &str, proxy_jump: &str) -> Self {
        Self { inner, host_user: user.to_string(), host_hostname: hostname.to_string(), host_port: port, host_key: key.to_string(), host_proxy_jump: proxy_jump.to_string() }
    }
    pub fn from_config(inner: R, cfg: &crate::config::RemoteHost) -> Self {
        Self::new(inner, &cfg.user, &cfg.hostname, cfg.port, &cfg.key, &cfg.proxy_jump)
    }
    fn ssh_base(&self) -> String {
        let mut b = format!("ssh {}@{} -p {}", self.host_user, self.host_hostname, self.host_port);
        if !self.host_key.is_empty() { b.push_str(&format!(" -i {}", self.host_key)); }
        if !self.host_proxy_jump.is_empty() { b.push_str(&format!(" -J {}", self.host_proxy_jump)); }
        b
    }
    fn ssh_cmd(&self, remote_cmd: &str) -> String {
        format!("{} '{}'", self.ssh_base(), remote_cmd.replace('\'', "'\\''"))
    }
}

#[async_trait]
impl<R: Runtime + 'static> Runtime for RemoteRuntime<R> {
    fn runtime_type(&self) -> String { format!("remote({}@{})", self.host_user, self.host_hostname) }

    async fn execute_command(&self, command: &str, timeout_secs: u64) -> Result<(String, String, i32), String> {
        self.inner.execute_command(&self.ssh_cmd(command), timeout_secs).await
    }
    async fn read_file(&self, path: &str) -> Result<String, String> {
        let (o, e, c) = self.inner.execute_command(&self.ssh_cmd(&format!("cat \"{path}\"")), 30).await?;
        if c != 0 { Err(format!("remote read: {e}")) } else { Ok(o) }
    }
    async fn write_file(&self, path: &str, content: &str) -> Result<(), String> {
        let e = content.replace('\'', "'\\''");
        let (_, s, c) = self.inner.execute_command(&self.ssh_cmd(&format!("cat > \"{path}\" << 'EOF'\n{e}\nEOF")), 30).await?;
        if c != 0 { Err(format!("remote write: {s}")) } else { Ok(()) }
    }
    async fn edit_file(&self, path: &str, old: &str, new: &str) -> Result<(), String> {
        let c = self.read_file(path).await?; self.write_file(path, &c.replace(old, new)).await
    }
    async fn path_exists(&self, path: &str) -> bool {
        self.inner.execute_command(&self.ssh_cmd(&format!("test -e \"{path}\"")), 10).await.is_ok()
    }
    async fn list_dir(&self, path: &str) -> Result<Vec<String>, String> {
        let (o, _, _) = self.inner.execute_command(&self.ssh_cmd(&format!("ls -1 \"{path}\"")), 15).await?;
        Ok(o.lines().map(String::from).collect())
    }
    async fn remove_file(&self, path: &str) -> Result<(), String> {
        let (_, s, c) = self.inner.execute_command(&self.ssh_cmd(&format!("rm -f \"{path}\"")), 15).await?;
        if c != 0 { Err(format!("remote rm: {s}")) } else { Ok(()) }
    }
    async fn grep_search(&self, pattern: &str, path: &str) -> Result<Vec<String>, String> {
        let (o, _, _) = self.inner.execute_command(&self.ssh_cmd(&format!("grep -rn '{pattern}' '{path}' 2>/dev/null || true")), 30).await?;
        Ok(o.lines().map(String::from).collect())
    }
    async fn find_files(&self, path: &str, name: &str) -> Result<Vec<String>, String> {
        let (o, _, _) = self.inner.execute_command(&self.ssh_cmd(&format!("find '{path}' -name '{name}' 2>/dev/null || true")), 30).await?;
        Ok(o.lines().map(String::from).filter(|l| !l.is_empty()).collect())
    }
    async fn file_info(&self, path: &str) -> Result<Vec<FileEntry>, String> {
        let (o, _, _) = self.inner.execute_command(&self.ssh_cmd(&format!("ls -la '{path}' 2>/dev/null || true")), 15).await?;
        let mut v = Vec::new();
        for line in o.lines().skip(1) { if !line.is_empty() { let p: Vec<&str> = line.split_whitespace().collect(); if p.len() >= 9 { v.push(FileEntry { name: p[8..].join(" "), is_dir: line.starts_with('d'), size: p[4].parse().unwrap_or(0), modified: p[5..8].join(" ") }); } } }
        Ok(v)
    }
    async fn check_command(&self, cmd: &str) -> Result<(), String> { self.inner.check_command(cmd).await }
    async fn spawn_process(&self, req: SpawnProcessRequest) -> Result<ProcessHandle, String> { self.inner.spawn_process(req).await }
    async fn kill_process(&self, pid: u32) -> Result<(), String> { self.inner.kill_process(pid).await }
    async fn send_stdin(&self, pid: u32, input: &str) -> Result<(), String> { self.inner.send_stdin(pid, input).await }
    async fn spawn_worker(&self, req: SpawnWorkerRequest) -> Result<SpawnWorkerResponse, String> { self.inner.spawn_worker(req).await }
    async fn send_to_worker(&self, wid: &str, text: &str) -> Result<(), String> { self.inner.send_to_worker(wid, text).await }
    async fn resume_worker(&self, wid: &str, text: &str) -> Result<String, String> { self.inner.resume_worker(wid, text).await }
    async fn await_worker(&self, wid: &str) -> Result<String, String> { self.inner.await_worker(wid).await }
    async fn channel_send(&self, ch: &str, text: &str) -> Result<(), String> { self.inner.channel_send(ch, text).await }
    async fn kill_worker(&self, wid: &str) -> Result<(), String> { self.inner.kill_worker(wid).await }
}

// ---------------------------------------------------------------------------
// RouterRuntime — 命令级路由，根据配置选择 Runtime
// ---------------------------------------------------------------------------

/// 根据 `runtime.routes` 配置将不同命令路由到不同后端 Runtime。
///
/// ```json
/// {"routes": [
///   {"tool": "bash", "pattern": "kubectl *", "runtime": "remote", "host": "xyz-mac"},
///   {"tool": "bash", "pattern": "npm install *", "runtime": "sandbox"}
/// ]}
/// ```
pub struct RouterRuntime {
    default: LocalRuntime,
    remote_hosts: std::collections::HashMap<String, RemoteRuntime<LocalRuntime>>,
    routes: Vec<crate::config::RouteRule>,
}

impl RouterRuntime {
    pub fn new(routes: Vec<crate::config::RouteRule>, remote_cfg: &crate::config::RemoteConfig) -> Self {
        let mut remote_hosts = std::collections::HashMap::new();
        for (name, host_cfg) in &remote_cfg.hosts {
            let rt = RemoteRuntime::from_config(LocalRuntime::new(), host_cfg);
            remote_hosts.insert(name.clone(), rt);
        }
        Self { default: LocalRuntime::new(), remote_hosts, routes }
    }

    /// 匹配命令对应的 Runtime（默认返回 &LocalRuntime）
    fn select<'a>(&'a self, tool: &str, command: &str) -> &'a dyn Runtime {
        for rule in &self.routes {
            if !rule.tool.is_empty() && rule.tool != tool && rule.tool != "*" {
                continue;
            }
            if !rule.pattern.is_empty() && !glob_match(&rule.pattern, command) {
                continue;
            }
            match rule.runtime.as_str() {
                "remote" => {
                    if let Some(rt) = self.remote_hosts.get(&rule.host) {
                        return rt as &dyn Runtime;
                    }
                    // host not found, try default host
                    for rt in self.remote_hosts.values() {
                        return rt as &dyn Runtime;
                    }
                }
                "sandbox" => { /* TODO: SandboxRuntime */ }
                _ => {} // "local" or unknown → use default
            }
            break;
        }
        &self.default as &dyn Runtime
    }
}

// Simple glob: "*" matches anything, "prefix*" matches prefix, "*suffix" matches suffix
fn glob_match(pattern: &str, s: &str) -> bool {
    if pattern == "*" { return true; }
    if let Some(p) = pattern.strip_suffix('*') { return s.starts_with(p); }
    if let Some(p) = pattern.strip_prefix('*') { return s.ends_with(p); }
    pattern == s
}

#[async_trait]
impl Runtime for RouterRuntime {
    fn runtime_type(&self) -> String { "router".into() }

    async fn execute_command(&self, command: &str, timeout_secs: u64) -> Result<(String, String, i32), String> {
        self.select("bash", command).execute_command(command, timeout_secs).await
    }
    async fn read_file(&self, path: &str) -> Result<String, String> {
        self.select("read", path).read_file(path).await
    }
    async fn write_file(&self, path: &str, content: &str) -> Result<(), String> {
        self.select("write", path).write_file(path, content).await
    }
    async fn edit_file(&self, path: &str, old: &str, new: &str) -> Result<(), String> {
        let rt = self.select("edit", path); rt.edit_file(path, old, new).await
    }
    async fn path_exists(&self, path: &str) -> bool { self.select("read", path).path_exists(path).await }
    async fn list_dir(&self, path: &str) -> Result<Vec<String>, String> { self.select("ls", path).list_dir(path).await }
    async fn remove_file(&self, path: &str) -> Result<(), String> { self.select("remove_file", path).remove_file(path).await }
    async fn grep_search(&self, pattern: &str, path: &str) -> Result<Vec<String>, String> { self.select("grep", path).grep_search(pattern, path).await }
    async fn find_files(&self, path: &str, name: &str) -> Result<Vec<String>, String> { self.select("find", path).find_files(path, name).await }
    async fn file_info(&self, path: &str) -> Result<Vec<FileEntry>, String> { self.select("ls", path).file_info(path).await }

    async fn check_command(&self, command: &str) -> Result<(), String> { self.default.check_command(command).await }
    async fn spawn_process(&self, req: SpawnProcessRequest) -> Result<ProcessHandle, String> { self.default.spawn_process(req).await }
    async fn kill_process(&self, pid: u32) -> Result<(), String> { self.default.kill_process(pid).await }
    async fn send_stdin(&self, pid: u32, input: &str) -> Result<(), String> { self.default.send_stdin(pid, input).await }
}

// ---------------------------------------------------------------------------
// SandboxRuntime — macOS sandbox-exec 隔离
// ---------------------------------------------------------------------------

/// 通过 macOS `sandbox-exec` 在沙箱内执行命令。
///
/// 根据配置生成 Seatbelt `.sb` profile，控制文件系统和网络访问。
/// 支持三种内置 profile: readonly / workspace / full-access
///
/// ```json
/// {"runtime": {"sandbox": {"profile": "workspace"}}}
/// ```
pub struct SandboxRuntime<R: Runtime> {
    inner: R,
    profile: String,        // "readonly" | "workspace" | "full-access"
    workspace: String,      // 工作区根路径
}

impl<R: Runtime> SandboxRuntime<R> {
    pub fn new(inner: R, profile: &str, workspace: &str) -> Self {
        Self { inner, profile: profile.to_string(), workspace: workspace.to_string() }
    }

    /// 生成 Seatbelt .sb profile 内容
    fn generate_profile(&self) -> String {
        let mut sb = String::from("(version 1)\n(allow default)\n");
        match self.profile.as_str() {
            "readonly" => {
                sb.push_str(&format!("(allow file-read* (subpath \"/\"))\n"));
                sb.push_str("(deny file-write* (subpath \"/\"))\n");
                sb.push_str("(allow file-write* (subpath \"/tmp\"))\n");
                sb.push_str("(allow file-write* (subpath \"/private/tmp\"))\n");
                sb.push_str("(deny network*)\n");
            }
            "workspace" => {
                sb.push_str(&format!("(allow file-read* (subpath \"/\"))\n"));
                sb.push_str(&format!("(allow file-write* (subpath \"{}\"))\n", self.workspace));
                sb.push_str("(allow file-write* (subpath \"/tmp\"))\n");
                sb.push_str("(allow file-write* (subpath \"/private/tmp\"))\n");
                sb.push_str("(deny file-write* (subpath \"/etc\"))\n");
                sb.push_str("(deny file-write* (subpath \"/usr\"))\n");
                // 网络默认允许（实际由 network.domains 控制）
            }
            _ => {} // "full-access" 或未知 → 全部允许
        }
        sb
    }

    /// 写入 profile 到临时文件，返回路径
    fn write_profile(&self) -> Result<String, String> {
        let content = self.generate_profile();
        let path = format!("/tmp/ion-sandbox-{}.sb", std::process::id());
        std::fs::write(&path, &content).map_err(|e| format!("write sb profile: {e}"))?;
        Ok(path)
    }

    /// 用 sandbox-exec 包装命令
    fn sandbox_cmd(&self, cmd: &str) -> String {
        let profile_path = self.write_profile().unwrap_or_default();
        format!("sandbox-exec -f {} /bin/sh -c '{}'", profile_path, cmd.replace('\'', "'\\''"))
    }
}

#[async_trait]
impl<R: Runtime + 'static> Runtime for SandboxRuntime<R> {
    fn runtime_type(&self) -> String { format!("sandbox({})", self.profile) }

    async fn execute_command(&self, command: &str, timeout_secs: u64) -> Result<(String, String, i32), String> {
        let sb_cmd = self.sandbox_cmd(command);
        self.inner.execute_command(&sb_cmd, timeout_secs).await
    }
    async fn read_file(&self, path: &str) -> Result<String, String> { self.inner.read_file(path).await }
    async fn write_file(&self, path: &str, content: &str) -> Result<(), String> { self.inner.write_file(path, content).await }
    async fn edit_file(&self, path: &str, old: &str, new: &str) -> Result<(), String> { self.inner.edit_file(path, old, new).await }
    async fn path_exists(&self, path: &str) -> bool { self.inner.path_exists(path).await }
    async fn list_dir(&self, path: &str) -> Result<Vec<String>, String> { self.inner.list_dir(path).await }
    async fn remove_file(&self, path: &str) -> Result<(), String> { self.inner.remove_file(path).await }
    async fn grep_search(&self, pattern: &str, path: &str) -> Result<Vec<String>, String> { self.inner.grep_search(pattern, path).await }
    async fn find_files(&self, path: &str, name: &str) -> Result<Vec<String>, String> { self.inner.find_files(path, name).await }
    async fn file_info(&self, path: &str) -> Result<Vec<FileEntry>, String> { self.inner.file_info(path).await }
    async fn check_command(&self, cmd: &str) -> Result<(), String> { self.inner.check_command(cmd).await }
    async fn spawn_process(&self, req: SpawnProcessRequest) -> Result<ProcessHandle, String> { self.inner.spawn_process(req).await }
    async fn kill_process(&self, pid: u32) -> Result<(), String> { self.inner.kill_process(pid).await }
    async fn send_stdin(&self, pid: u32, input: &str) -> Result<(), String> { self.inner.send_stdin(pid, input).await }
    async fn spawn_worker(&self, req: SpawnWorkerRequest) -> Result<SpawnWorkerResponse, String> { self.inner.spawn_worker(req).await }
    async fn send_to_worker(&self, wid: &str, text: &str) -> Result<(), String> { self.inner.send_to_worker(wid, text).await }
    async fn resume_worker(&self, wid: &str, text: &str) -> Result<String, String> { self.inner.resume_worker(wid, text).await }
    async fn await_worker(&self, wid: &str) -> Result<String, String> { self.inner.await_worker(wid).await }
    async fn channel_send(&self, ch: &str, text: &str) -> Result<(), String> { self.inner.channel_send(ch, text).await }
    async fn kill_worker(&self, wid: &str) -> Result<(), String> { self.inner.kill_worker(wid).await }
}
#[cfg(test)]
mod tests {
    use super::*;
    use tokio::test;

    #[tokio::test]
    async fn local_runtime_execute() {
        let r = LocalRuntime::new();
        let (stdout, stderr, code) = r.execute_command("echo hello", 10).await.unwrap();
        assert_eq!(stdout.trim(), "hello");
        assert!(stderr.is_empty());
        assert_eq!(code, 0);
    }

    #[tokio::test]
    async fn local_runtime_read_write_file() {
        let r = LocalRuntime::new();
        let tmp = std::env::temp_dir().join(format!("runtime_test_{}", std::process::id()));
        let path = tmp.to_string_lossy().to_string();

        r.write_file(&path, "test content").await.unwrap();
        assert!(r.path_exists(&path).await);

        let content = r.read_file(&path).await.unwrap();
        assert_eq!(content, "test content");

        r.edit_file(&path, "test", "edited").await.unwrap();
        let edited = r.read_file(&path).await.unwrap();
        assert_eq!(edited, "edited content");

        r.remove_file(&path).await.unwrap();
        assert!(!r.path_exists(&path).await);
    }

    #[tokio::test]
    async fn local_runtime_list_dir() {
        let r = LocalRuntime::new();
        let entries = r.list_dir(".").await.unwrap();
        assert!(!entries.is_empty());
        assert!(entries.contains(&"src".to_string()) || entries.contains(&"Cargo.toml".to_string()));
    }

    #[tokio::test]
    async fn secured_runtime_blocks_write_by_permission() {
        use crate::kernel::*;
        let engine = std::sync::Arc::new(PermissionEngine::new());
        engine.register_rule(PermissionRule {
            name: "block-all-writes".into(),
            actions: vec![Action::Write],
            pattern: "**".into(),
            policy: PermissionPolicy::Deny,
            priority: 100,
        });

        let secured = SecuredRuntime::new(LocalRuntime::new())
            .with_permissions(engine);

        let result = secured.write_file("/tmp/test_blocked.txt", "should fail").await;
        assert!(result.is_err(), "write should be blocked");
        assert!(result.unwrap_err().contains("Permission"), "should mention Permission");
    }

    #[tokio::test]
    async fn secured_runtime_blocks_high_risk_command() {
        let guard = crate::command_guard::CommandGuard::default();
        let secured = SecuredRuntime::new(LocalRuntime::new())
            .with_command_guard(guard);

        let result = secured.execute_command("rm -rf / ", 10).await;
        assert!(result.is_err(), "rm -rf / should be blocked");
        assert!(result.unwrap_err().contains("CommandGuard"), "should mention CommandGuard");
    }

    #[test]
    async fn runtime_type_strings() {
        let local = LocalRuntime::new();
        assert_eq!(local.runtime_type(), "local");

        let secured = SecuredRuntime::new(LocalRuntime::new());
        assert_eq!(secured.runtime_type(), "secured(local)");
    }

    #[test]
    async fn remote_runtime_ssh_command_format() {
        let rt = RemoteRuntime::new(LocalRuntime::new(), "admin", "xyz-mac.local", 22, "", "");
        let cmd = rt.ssh_cmd("echo hello");
        assert_eq!(cmd, "ssh admin@xyz-mac.local -p 22 'echo hello'");
    }

    #[test]
    async fn remote_runtime_ssh_command_with_key() {
        let rt = RemoteRuntime::new(LocalRuntime::new(), "deploy", "10.0.0.1", 2222, "~/.ssh/deploy_key", "");
        let cmd = rt.ssh_cmd("kubectl get pods");
        assert_eq!(cmd, "ssh deploy@10.0.0.1 -p 2222 -i ~/.ssh/deploy_key 'kubectl get pods'");
    }

    #[test]
    async fn glob_matching_works() {
        assert!(glob_match("npm *", "npm install"));
        assert!(!glob_match("npm *", "pip install"));
        assert!(glob_match("*", "anything"));
    }
}
