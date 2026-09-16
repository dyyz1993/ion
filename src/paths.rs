//! ion 文件系统路径管理 —— 完全对齐 pi 的目录结构
//!
//! ## 全局目录 `~/.ion/`
//!
//! ```text
//! ~/.ion/
//! ├── agent/                     ← getAgentDir()
//! │   ├── settings.json          ← 用户设置
//! │   ├── auth.json              ← API Key 存储（权限 600）
//! │   ├── models.json            ← 用户自定义模型定义
//! │   ├── sessions/              ← 会话文件存储（按 cwd 分组）
//! │   │   ├── --hash--cwd--/     ← 每个 cwd 一个子目录
//! │   │   │   ├── session.jsonl  ← 主会话文件（JSONL v3）
//! │   │   │   ├── session.lock   ← 会话锁文件
//! │   │   │   └── data/          ← 扩展的 session 级数据
//! │   │   │       └── <sessionId>/
//! │   │   │           └── <extName>/
//! │   │   ├── --hash--cwd2--/
//! │   │   └── ...
//! │   ├── sessions.index.json    ← 全局会话元数据索引
//! │   ├── extensions-data/       ← 扩展的全局数据（globalDataDir）
//! │   │   └── <extName>/
//! │   ├── project-data/          ← 扩展的项目级数据（projectDataDir）
//! │   │   └── <hash>--<name>/
//! │   │       └── <extName>/
//! │   ├── cwd-data/              ← 扩展的 cwd 级数据（cwdDataDir）
//! │   │   └── <hash>--<name>/
//! │   │       └── <extName>/
//! │   ├── projects/              ← 项目用户状态（skills 等）
//! │   │   └── <hash>--<name>/
//! │   │       └── skills/
//! │   ├── cache/                 ← 缓存
//! │   ├── extensions/            ← 全局扩展
//! │   ├── skills/                ← 全局技能
//! │   ├── prompts/               ← 全局提示模板
//! │   ├── themes/                ← 全局主题
//! │   ├── tools/                 ← 工具目录
//! │   ├── bin/                   ← 托管二进制文件（fd, rg）
//! │   ├── tmp/                   ← 临时文件
//! │   │   ├── extensions/        ← 扩展临时文件
//! │   │   ├── ion-bash-<id>.log  ← Bash 输出溢出
//! │   │   ├── ion-input-<id>.txt ← 大输入溢出
//! │   │   └── ion-tool-results/  ← 工具结果预算溢出
//! │   │       └── <slug>/
//! │   └── last_session           ← 上次会话 ID（纯文本）
//! ├── worktrees/                 ← Git worktree 隔离
//! │   └── <rand8hex>/<projectName>/  ← 实际结构（worker_registry.rs:create_worktree_advanced）
//! └── pi-debug.log               ← 调试日志（兼容 pi 命名）
//! ```
//!
//! ## 项目级目录 `<project>/.ion/`
//!
//! ```text
//! <project>/.ion/
//! ├── settings.json              ← 项目级设置（与全局深度合并）
//! ├── extensions/                ← 项目级扩展
//! ├── skills/                    ← 项目级技能
//! ├── prompts/                   ← 项目级提示模板
//! ├── rules/                     ← 规则文件
//! ├── rules-config.json          ← 规则配置
//! └── memory/                    ← 会话记忆
//! ```

use std::path::PathBuf;

/// ion 根目录名称
const ION_DIR: &str = ".ion";

/// 项目级配置目录名称（可通过 package.json 自定义）
const CONFIG_DIR_NAME: &str = ".ion";

// ---------------------------------------------------------------------------
// 全局根目录
// ---------------------------------------------------------------------------

/// ~/.ion/
pub fn root() -> PathBuf {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(ION_DIR)
}

// ---------------------------------------------------------------------------
// Host 运行时文件（Unix socket / PID）
// ---------------------------------------------------------------------------

/// ~/.ion/host.sock — Host 的 Unix socket 入口。
///
/// 支持并发：设置 `ION_HOST_SOCKET` 环境变量可覆盖默认路径，
/// 让多个 `ion serve` / `ion --host` 实例同时运行（各自独立 socket）。
/// 用法：`ION_HOST_SOCKET=/tmp/ion_taskA.sock ion --host "task A"`
pub fn host_socket_path() -> PathBuf {
    if let Ok(custom) = std::env::var("ION_HOST_SOCKET") {
        let p = std::path::PathBuf::from(&custom);
        if p.parent().map(|p| p.as_os_str().is_empty()).unwrap_or(true) {
            return root().join(&custom);
        }
        return p;
    }
    root().join("host.sock")
}

/// Host 的 PID 文件（防重复启动）。
///
/// 默认 `~/.ion/host.pid`。当 `ION_HOST_SOCKET` 自定义时，pid 文件名
/// 基于 socket 路径派生（每个并发 host 有独立 pid 文件）。
pub fn host_pid_path() -> PathBuf {
    if std::env::var("ION_HOST_SOCKET").is_ok() {
        let sock = host_socket_path();
        let pid_name = sock
            .file_stem()
            .map(|s| format!("{}.pid", s.to_string_lossy()))
            .unwrap_or_else(|| "host.pid".to_string());
        if let Some(parent) = sock.parent() {
            if !parent.as_os_str().is_empty() {
                return parent.join(&pid_name);
            }
        }
        return root().join(&pid_name);
    }
    root().join("host.pid")
}

/// pid 文件内容（安全加固 G1：pid 文件带 hostId 身份）。
///
/// JSON 形态：`{"pid":1234,"hostId":"<uuid>","startedAtMs":169…}`。
/// `hostId` 与 hello RPC 回报的 `data.hostId` 同源（`ion_protocol::host_id()`，
/// 进程内存态随机，重启即换）——`host_running_verified` 靠它做"同一个 host
/// 在跑"的身份判定，防 pid 复用误判（陈旧 pid 文件 + 无关进程复用了那个 pid）。
/// `startedAtMs` 是为将来进程级启动时间比对预留的元数据（当前身份判定走
/// hello hostId，跨平台取得进程真实启动时间需要 /proc 或 sysctl FFI，暂不做）。
/// 兼容旧格式：纯数字 pid 文件读出 `host_id: None`。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HostPidInfo {
    pub pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<u64>,
}

/// 写 pid 文件（原子：同目录临时名 + rename）。**必须在 socket bind 成功之后
/// 调用**（G1：先 bind 后写 pid——bind 失败的进程绝不留下"host 在跑"的假象）。
pub fn write_host_pid_file_at(path: &std::path::Path, info: &HostPidInfo) -> std::io::Result<()> {
    let mut tmp_name = path
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    let json = serde_json::to_vec(info)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&tmp, json)?;
    // rename 原子落位：读者要么看到旧文件（完整），要么看到新文件（完整）
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// 写 pid 文件（默认路径，内容 = 本进程 pid + 本进程 hostId + 当前时刻）。
pub fn write_host_pid_file() -> std::io::Result<()> {
    write_host_pid_file_at(
        &host_pid_path(),
        &HostPidInfo {
            pid: std::process::id(),
            host_id: Some(ion_protocol::host_id().to_string()),
            started_at_ms: Some(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0),
            ),
        },
    )
}

/// 读 pid 文件（路径参数版）。JSON 形态优先；旧纯数字格式兜底
/// （`host_id: None`，verified 路径对它退化为 pid 存活判定）。
/// 内容不可解析 → None（与旧行为一致：不删文件，留给下一次启动诊断）。
pub fn read_host_pid_info_at(path: &std::path::Path) -> Option<HostPidInfo> {
    let content = std::fs::read_to_string(path).ok()?;
    let trimmed = content.trim();
    if let Ok(info) = serde_json::from_str::<HostPidInfo>(trimmed) {
        return Some(info);
    }
    trimmed.parse::<u32>().ok().map(|pid| HostPidInfo {
        pid,
        host_id: None,
        started_at_ms: None,
    })
}

/// 读 pid 文件（默认路径）。
pub fn read_host_pid_info() -> Option<HostPidInfo> {
    read_host_pid_info_at(&host_pid_path())
}

/// 检查 Host 是否在运行：读 PID 文件 + 验证进程存活。
///
/// 快速同步路径（pid + kill(0)）；需要"确认 socket 背后就是 pid 文件里那个
/// host"的身份级判定时用 [`host_running_verified`]。
pub fn host_running() -> Option<u32> {
    let pid_path = host_pid_path();
    // Fallback: check old manager.pid path for migration
    let old_pid_path = root().join("manager.pid");
    let (target, legacy) = if !pid_path.exists() && old_pid_path.exists() {
        (old_pid_path, true)
    } else {
        (pid_path, false)
    };
    let info = read_host_pid_info_at(&target)?;
    if libc_kill(info.pid, 0) == 0 {
        Some(info.pid)
    } else {
        let _ = std::fs::remove_file(&target);
        let _ = legacy;
        None
    }
}

/// 严格身份验活结果（安全加固 G1）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostIdentityCheck {
    /// pid 存活，且 socket 背后活 host 经 hello 回报的 hostId 与 pid 文件记录
    /// 一致（旧格式 pid 文件无 hostId、无法比对时保守视为通过）。
    Verified(HostPidInfo),
    /// pid 存活但身份对不上——该 pid 疑似已被无关进程复用（或 pid 文件陈旧、
    /// socket 背后是另一个 host）。**禁止按此 pid 强杀**（会杀到无辜进程）。
    IdentityMismatch { pid: u32 },
    /// 无法确认：pid 不存活（陈旧文件已顺手清理）/ pid 文件不可读 /
    /// socket 探活失败（host 未起或 accept loop 未就绪）。
    /// 启动仲裁交给 bind 本身（见 ion.rs bind_host_socket）。
    Unconfirmed,
}

/// 严格身份验活（安全加固 G1）：pid 存活 **且** 身份一致才算"同一个 host 在跑"。
///
/// 判定链：① pid 文件可读 ② kill(pid,0) 存活 ③ 经 socket 发 hello 探活 RPC，
/// 活 host 回报的 `data.hostId` 必须与 pid 文件记录的 `hostId` 一致。
/// 防 pid 复用误判：陈旧 pid 文件里的 pid 被无关进程复用时，kill(0) 通过但
/// hello 回报的 hostId 对不上 → [`HostIdentityCheck::IdentityMismatch`]。
///
/// 取舍说明：hello 探活是异步且要求 socket 已就绪（accept loop 未起的窗口内
/// 会探空 → Unconfirmed）——所以**重复启动防护不能只靠它**，serve 启动的最终
/// 仲裁是 bind 本身（ion.rs bind_host_socket）；本函数用于 serve 启动前的
/// 友好预检、serve status 展示、serve stop 强杀前的身份确认。进程级启动时间
/// 比对（/proc 或 sysctl FFI）暂不做：macOS 侧需引入 libproc FFI 且 lib 已按
/// 项目惯例避免 libc crate，hello hostId 已覆盖"同一 host"判定的实际需求；
/// pid 文件里的 `startedAtMs` 为未来升级预留了元数据。
pub async fn host_running_verified(timeout: std::time::Duration) -> HostIdentityCheck {
    let Some(info) = read_host_pid_info() else {
        return HostIdentityCheck::Unconfirmed;
    };
    if libc_kill(info.pid, 0) != 0 {
        // 死 pid：清掉陈旧文件（自愈），报无法确认
        let _ = std::fs::remove_file(host_pid_path());
        return HostIdentityCheck::Unconfirmed;
    }
    let Some(live_id) = probe_hello_host_id(timeout).await else {
        return HostIdentityCheck::Unconfirmed;
    };
    match &info.host_id {
        Some(expect) if *expect == live_id => HostIdentityCheck::Verified(info),
        Some(_) => HostIdentityCheck::IdentityMismatch { pid: info.pid },
        // 旧格式 pid 文件（无 hostId）：无法做身份比对，保守接受 pid 存活
        None => HostIdentityCheck::Verified(info),
    }
}

/// 探活 RPC：连 host socket 发 `hello`，取回活 host 的 hostId（带超时）。
/// 连不上 / 超时 / 响应缺 hostId → None。
#[cfg(unix)]
pub async fn probe_hello_host_id(timeout: std::time::Duration) -> Option<String> {
    use tokio::io::AsyncReadExt as _;
    use tokio::io::AsyncWriteExt as _;
    let mut stream = tokio::time::timeout(
        timeout,
        tokio::net::UnixStream::connect(host_socket_path()),
    )
    .await
    .ok()?
    .ok()?;
    let req = ion_protocol::Request::rpc("pid-verify", "hello", serde_json::json!({}));
    let line = serde_json::to_string(&req).ok()?;
    stream.write_all(format!("{line}\n").as_bytes()).await.ok()?;
    // 读一行响应（timeout 兜底防挂）
    let mut buf = Vec::with_capacity(256);
    let mut chunk = [0u8; 512];
    loop {
        let n = tokio::time::timeout(timeout, stream.read(&mut chunk))
            .await
            .ok()?
            .ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.ends_with(b"\n") || buf.len() > 64 * 1024 {
            break;
        }
    }
    let v: serde_json::Value = serde_json::from_slice(&buf).ok()?;
    v.get("data")?.get("hostId")?.as_str().map(str::to_string)
}

/// socket 文件属主校验（安全加固 G1）：删除既有 sock 文件前确认属主是当前
/// euid——防止误删/越权清理共享目录里不属于自己的文件（同 uid 攻击者本就
/// 可以任意 DoS，这里收紧的是"跨用户/误操作"面）。
#[cfg(unix)]
pub fn socket_file_owned_by_self(path: &std::path::Path) -> bool {
    socket_file_owned_by(path, current_euid())
}

/// 当前进程有效 uid（直调 syscall，避免引入 libc crate——与 libc_kill 同策略）。
#[cfg(unix)]
fn current_euid() -> u32 {
    unsafe {
        unsafe extern "C" {
            fn geteuid() -> u32;
        }
        geteuid()
    }
}

/// [`socket_file_owned_by_self`] 的 euid 注入版（测试跨属主分支用）。
#[cfg(unix)]
pub(crate) fn socket_file_owned_by(path: &std::path::Path, euid: u32) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::metadata(path)
        .map(|m| m.uid() == euid)
        .unwrap_or(false)
}

// 跨平台 kill(pid, 0)。libc 在所有 unix 上都有；windows 不支持 Unix socket 跳过。
#[cfg(unix)]
fn libc_kill(pid: u32, sig: i32) -> i32 {
    // 直接调 syscall，避免引入 libc crate
    unsafe {
        unsafe extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        kill(pid as i32, sig)
    }
}

#[cfg(not(unix))]
fn libc_kill(_pid: u32, _sig: i32) -> i32 {
    -1
}

// ---------------------------------------------------------------------------
// Agent 目录（核心配置）
// ---------------------------------------------------------------------------

/// ~/.ion/agent/ — 可通过 ION_AGENT_DIR 环境变量覆盖
pub fn agent_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("ION_AGENT_DIR") {
        return PathBuf::from(dir);
    }
    root().join("agent")
}

/// Check whether the agent directory exists.
pub fn agent_dir_exists() -> bool {
    agent_dir().exists()
}

/// ~/.ion/file-store/ — File Snapshot content-addressed 存储
pub fn file_store_root() -> PathBuf {
    root().join("file-store")
}

/// ~/.ion/file-store/<project_key>/ — 按项目隔离
pub fn file_store_dir(project_key: &str) -> PathBuf {
    file_store_root().join(project_key)
}

/// ~/.ion/projects/ — 项目维度配置根目录（② 项目维度，worktree 共享）
pub fn projects_root() -> PathBuf {
    root().join("projects")
}

/// ~/.ion/projects/<project_key>/ — 单个项目的维度配置目录
/// project_key 用 git common dir hash（主仓库和 worktree 一致）
pub fn project_dimension_dir(project_key: &str) -> PathBuf {
    projects_root().join(project_key)
}

/// ~/.ion/projects/<project_key>/config.json — 项目维度配置文件
/// 存放含本地路径/密钥的项目级配置（MCP server、本地 tier models 等）
pub fn project_dimension_config_path(project_key: &str) -> PathBuf {
    project_dimension_dir(project_key).join("config.json")
}

/// ~/.ion/agent/settings.json
/// ~/.ion/settings.json — 全局权限设置（permissions.rules）
/// 注意：直接在 ~/.ion/ 下，不是 ~/.ion/agent/ 下（与 permission_extension 实际使用一致）
pub fn settings_path() -> PathBuf {
    root().join("settings.json")
}

/// ~/.ion/auth.json  (直接在 ~/.ion/ 下，权限 600)
pub fn auth_path() -> PathBuf {
    root().join("auth.json")
}

/// ~/.ion/agent/models.json
pub fn models_path() -> PathBuf {
    agent_dir().join("models.json")
}

// ---------------------------------------------------------------------------
// 会话存储（按 cwd 分组）
// ---------------------------------------------------------------------------

/// ~/.ion/agent/sessions/ — 可通过 ION_SESSION_DIR 环境变量覆盖
pub fn sessions_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("ION_SESSION_DIR") {
        return PathBuf::from(dir);
    }
    agent_dir().join("sessions")
}

/// sessions/--cwd_hash--cwd_name--/
/// 每个 cwd 一个子目录
pub fn session_cwd_dir(cwd: &str) -> PathBuf {
    sessions_dir().join(encode_path(cwd))
}

/// sessions/--cwd_hash--cwd_name--/session.jsonl
/// 主会话文件（JSONL v3 格式）
pub fn session_jsonl_path(cwd: &str) -> PathBuf {
    session_cwd_dir(cwd).join("session.jsonl")
}

/// sessions/--cwd_hash--cwd_name--/<session_id>.jsonl
/// 按 session_id 区分的会话文件（fork 子 Worker 用，避免跟主 Worker 写同一文件）。
///
/// 主 Worker 用 `session.jsonl`（共享，所有同一 cwd 的主会话共用）。
/// fork 子 Worker 用 `<session_id>.jsonl`（独立文件，不污染主会话）。
/// 这样 export 可以按 session_id 精确找到子 Worker 的对话历史。
pub fn session_jsonl_path_by_id(cwd: &str, session_id: &str) -> PathBuf {
    // 安全化 session_id（防止路径穿越）
    let safe_id: String = session_id
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    session_cwd_dir(cwd).join(format!("{safe_id}.jsonl"))
}

/// sessions/--cwd_hash--cwd_name--/session.lock
/// 会话锁文件
pub fn session_lock_path(cwd: &str) -> PathBuf {
    session_cwd_dir(cwd).join("session.lock")
}

/// sessions/--cwd_hash--cwd_name--/data/<sessionId>/<extName>/
/// 扩展的 session 级数据（sessionDataDir）
pub fn session_data_dir(cwd: &str, session_id: &str, extension_id: &str) -> PathBuf {
    session_cwd_dir(cwd)
        .join("data")
        .join(session_id)
        .join(extension_id)
}

/// ~/.ion/agent/sessions.index.json
/// 全局会话元数据索引
pub fn sessions_index_path() -> PathBuf {
    agent_dir().join("sessions.index.json")
}

/// ~/.ion/agent/last_session
/// 上次使用的会话 ID（纯文本）
pub fn last_session_path() -> PathBuf {
    agent_dir().join("last_session")
}

/// session 级 bash 进程存储路径 — ④ Session 维度
///
/// `sessions/<cwd_hash>/data/<sessionId>/bash/processes.json`
/// 每个 session 独立存储后台进程，不同 session 不互相覆盖。
pub fn bash_processes_path(cwd: &str, session_id: &str) -> PathBuf {
    session_data_dir(cwd, session_id, "bash").join("processes.json")
}

// ---------------------------------------------------------------------------
// 扩展数据目录
// ---------------------------------------------------------------------------
//
// ION 存储维度约定（5 维）：
//
// ┌──────────┬────────────────────────────┬───────────────────────────────────┐
// │ 维度     │ 路径函数                   │ worktree 行为                     │
// ├──────────┼────────────────────────────┼───────────────────────────────────┤
// │ ① 全局   │ global_data_dir(ext)       │ 共享（固定路径）                   │
// │ ② 项目   │ project_data_dir(cwd,ext)  │ 共享（git common dir hash）       │
// │ ③ 仓库内 │ project_local_data_dir()   │ 各自（走 git checkout）           │
// │ ④ Session│ session_data_dir(cwd,sid)  │ 隔离（session_id 分桶）           │
// │ ⑤ 单例   │ global-memory.db           │ 共享（全局唯一）                   │
// ├──────────┼────────────────────────────┼───────────────────────────────────┤
// │ 特殊约定  │                            │                                   │
// │ Bash 进程│ bash_processes_path()      │ ④ session 级                      │
// │ File Store│ file_store_dir()          │ ② 项目级（git_key）               │
// │ 项目配置  │ project_dimension_dir()   │ ② 项目级（git_key）               │
// └──────────┴────────────────────────────┴───────────────────────────────────┘
//
// 关键：调用方只管选维度（传 cwd/session_id/extension_id），
//       路径函数内部处理 worktree 回源（project_key_git）。
//       worktree 对调用方透明。

/// ~/.ion/agent/extensions-data/<extName>/
/// 扩展的全局数据（globalDataDir）— ① 全局维度
pub fn global_data_dir(extension_id: &str) -> PathBuf {
    agent_dir().join("extensions-data").join(extension_id)
}

/// ~/.ion/agent/project-data/<git_key>/<extName>/
/// 扩展的项目级数据（projectDataDir）— ② 项目维度
///
/// 用 `project_key_git(cwd)` 做 key：主仓库和 worktree 算出同一个 key → 共享存储。
/// 非 git 目录 fallback 到 cwd hash（不共享，但没有 worktree 概念所以不影响）。
pub fn project_data_dir(cwd: &str, extension_id: &str) -> PathBuf {
    agent_dir()
        .join("project-data")
        .join(project_key_git(cwd))
        .join(extension_id)
}

/// ~/.ion/agent/cwd-data/<hash>--<name>/<extName>/
/// 扩展的 cwd 级数据（cwdDataDir）
pub fn cwd_data_dir(cwd: &str, extension_id: &str) -> PathBuf {
    agent_dir()
        .join("cwd-data")
        .join(encode_path(cwd))
        .join(extension_id)
}

/// ~/.ion/agent/projects/<hash>--<name>/
/// 项目用户状态（getProjectUserStateDir）
pub fn project_user_state_dir(project_path: &str) -> PathBuf {
    agent_dir().join("projects").join(encode_path(project_path))
}

/// ~/.ion/agent/projects/<hash>--<name>/skills/
/// 项目私有技能（getProjectPrivateSkillsDir）
pub fn project_private_skills_dir(project_path: &str) -> PathBuf {
    project_user_state_dir(project_path).join("skills")
}

/// <project>/.ion/<extName>/
/// 扩展的本地项目数据（写在项目目录里，可 git 提交）
pub fn project_local_data_dir(project_root: &str, extension_id: &str) -> PathBuf {
    project_config_dir(project_root).join(extension_id)
}

// ---------------------------------------------------------------------------
// 扩展/技能/提示/主题/工具/二进制
// ---------------------------------------------------------------------------

/// ~/.ion/agent/extensions/
pub fn extensions_dir() -> PathBuf {
    agent_dir().join("extensions")
}

/// Calculate total size in bytes of the extensions directory (~/.ion/agent/extensions/).
///
/// Walks the directory recursively and sums up all file sizes.
/// Returns `Err` if the directory does not exist.
pub fn extensions_size() -> Result<u64, String> {
    let dir = extensions_dir();
    if !dir.exists() {
        return Err(format!(
            "extensions directory does not exist: {}",
            dir.display()
        ));
    }
    let mut total: u64 = 0;
    walk_dir(&dir, &mut total).map_err(|e| format!("IO error: {}", e))?;
    Ok(total)
}

fn walk_dir(dir: &std::path::Path, total: &mut u64) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        if ty.is_dir() {
            walk_dir(&entry.path(), total)?;
        } else if ty.is_file() {
            *total += entry.metadata()?.len();
        }
    }
    Ok(())
}

/// ~/.ion/agent/skills/
pub fn skills_dir() -> PathBuf {
    agent_dir().join("skills")
}

/// ~/.ion/agent/prompts/
pub fn prompts_dir() -> PathBuf {
    agent_dir().join("prompts")
}

/// ~/.ion/agent/themes/
pub fn themes_dir() -> PathBuf {
    agent_dir().join("themes")
}

/// ~/.ion/agent/tools/
pub fn tools_dir() -> PathBuf {
    agent_dir().join("tools")
}

/// ~/.ion/agent/bin/
pub fn bin_dir() -> PathBuf {
    agent_dir().join("bin")
}

// ---------------------------------------------------------------------------
// 缓存
// ---------------------------------------------------------------------------

/// ~/.ion/agent/cache/
pub fn cache_dir() -> PathBuf {
    agent_dir().join("cache")
}

// ---------------------------------------------------------------------------
// 临时文件
// ---------------------------------------------------------------------------

/// ~/.ion/agent/tmp/
pub fn tmp_dir() -> PathBuf {
    agent_dir().join("tmp")
}

/// Returns the system temp directory (std::env::temp_dir()).
/// This is the base OS temp directory, e.g. /tmp on Linux, %TEMP% on Windows.
pub fn os_tmp_dir() -> std::path::PathBuf {
    std::env::temp_dir()
}

/// ~/.ion/agent/tmp/extensions/
pub fn tmp_extensions_dir() -> PathBuf {
    tmp_dir().join("extensions")
}

/// ~/.ion/agent/tmp/ion-bash-<uuid>.log
pub fn bash_log_path(uuid: &str) -> PathBuf {
    tmp_dir().join(format!("ion-bash-{uuid}.log"))
}

/// ~/.ion/agent/tmp/ion-input-<uuid>.txt
pub fn input_overflow_path(uuid: &str) -> PathBuf {
    tmp_dir().join(format!("ion-input-{uuid}.txt"))
}

/// ~/.ion/agent/tmp/ion-tool-results/<slug>/
pub fn tool_results_dir(slug: &str) -> PathBuf {
    tmp_dir().join("ion-tool-results").join(slug)
}

// ---------------------------------------------------------------------------
// 项目根解析（worktree 回源 + git common dir 统一）
// ---------------------------------------------------------------------------

/// 从 cwd 反推 git 主仓库根路径（worktree → 主仓库）。
///
/// 算法：调 `git rev-parse --absolute-git-dir` 得到 `.git` 或 `.git/worktrees/<name>`，
/// 裁剪掉 `/worktrees/<name>` 后缀得到 common dir（主仓库的 `.git`），其父目录即仓库根。
///
/// - 主仓库 `/A/ion` → git-dir `/A/ion/.git` → 仓库根 `/A/ion`
/// - worktree `/B/wt` → git-dir `/A/ion/.git/worktrees/wt` → common `/A/ion/.git` → 仓库根 `/A/ion`
///
/// 非 git 目录返回 None。
pub fn git_project_root(cwd: &str) -> Option<PathBuf> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--absolute-git-dir"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let git_dir = String::from_utf8_lossy(&output.stdout).trim().to_string();
    // 裁剪 worktree 后缀：/A/.git/worktrees/wt → /A/.git
    let common = git_dir.split("/worktrees/").next().unwrap_or(&git_dir);
    // common dir 的父目录就是仓库根（.git 的父目录）
    let common_path = PathBuf::from(common);
    common_path.parent().map(PathBuf::from)
}

/// 计算 git common dir 的 hash（16 位 hex），用于 project_key。
/// 主仓库和所有 worktree 算出同一个 key → 共享 `~/.ion/projects/<key>/` 存储。
/// 非 git 目录 fallback 到 cwd hash。
pub fn project_key_git(cwd: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let key_source = main_git_dir(cwd).unwrap_or_else(|| cwd.to_string());

    let mut hasher = DefaultHasher::new();
    key_source.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// 获取主仓库的 .git 目录绝对路径（规范化）。
///
/// 使用 `git rev-parse --git-common-dir` —— 这是 git 官方维护的
/// "主仓库共享目录"：在 worktree 中也直接指向主仓库的 .git，
/// 不依赖任何路径字符串约定。
///
/// - 主仓库：返回 `/path/to/repo/.git`
/// - worktree：返回 `/path/to/repo/.git`（同一个）
/// - 非 git 目录：返回 None（调用方自行 fallback）
///
/// 规范化（canonicalize）是因为 `--git-common-dir` 在主仓库返回相对
/// 路径 `.git`、在 worktree 返回绝对路径，必须统一成绝对路径才能保证
/// 主仓库和 worktree 算出相同的 hash。
fn main_git_dir(cwd: &str) -> Option<String> {
    // 先检查 cwd 本身是否是 git 仓库（有 .git 目录或文件），
    // 避免 git rev-parse 向上递归找到父级仓库（测试临时目录在项目仓库内时会误中）
    let dot_git = std::path::Path::new(cwd).join(".git");
    if !dot_git.exists() {
        return None; // 不是 git 仓库（不向上查找）
    }

    let output = std::process::Command::new("git")
        .args(["rev-parse", "--git-common-dir"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if raw.is_empty() {
        return None;
    }
    // --git-common-dir 可能返回相对路径（如主仓库里的 ".git"），
    // 需要相对于 cwd 解析成绝对路径，再 canonicalize 消除符号链接等差异。
    let resolved = if std::path::Path::new(&raw).is_absolute() {
        std::path::PathBuf::from(&raw)
    } else {
        std::path::Path::new(cwd).join(&raw)
    };
    // canonicalize 可能失败（路径不存在），失败时退回到 resolved 的绝对路径
    let canonical = std::fs::canonicalize(&resolved)
        .unwrap_or(resolved)
        .to_string_lossy()
        .to_string();
    Some(canonical)
}

/// 解析"项目根路径"——用于所有需要读项目级 `.ion/` 资源的配置加载点。
///
/// 优先级（修复缺口 #2：worktree 回源）：
/// 1. `ION_PROJECT_ROOT` 环境变量（worker_registry spawn 子 worker 时注入主仓库根，精确）
/// 2. `current_dir()`（非 worktree 场景，cwd 就是项目根）
/// 3. 回退到 current_dir（上面的 or_else 分支保证不返回 None）
///
/// 注意：此函数返回的是"应该去哪读 `.ion/` 配置"的路径。
/// 在 worktree 场景下，cwd 是 worktree 目录（无 `.ion/`），但 `ION_PROJECT_ROOT` 指向主仓库。
pub fn project_root_for_config() -> PathBuf {
    std::env::var("ION_PROJECT_ROOT")
        .map(PathBuf::from)
        .or_else(|_| std::env::current_dir())
        .unwrap_or_else(|_| PathBuf::from("."))
}

// ---------------------------------------------------------------------------
// 项目级目录
// ---------------------------------------------------------------------------

/// <project>/.ion/
pub fn project_config_dir(project_root: &str) -> PathBuf {
    PathBuf::from(project_root).join(CONFIG_DIR_NAME)
}

/// <project>/.ion/settings.json
pub fn project_settings_path(project_root: &str) -> PathBuf {
    project_config_dir(project_root).join("settings.json")
}

/// <project>/.ion/extensions/
pub fn project_extensions_dir(project_root: &str) -> PathBuf {
    project_config_dir(project_root).join("extensions")
}

/// <project>/.ion/skills/
pub fn project_skills_dir(project_root: &str) -> PathBuf {
    project_config_dir(project_root).join("skills")
}

/// <project>/.ion/prompts/
pub fn project_prompts_dir(project_root: &str) -> PathBuf {
    project_config_dir(project_root).join("prompts")
}

/// <project>/.ion/rules/
pub fn project_rules_dir(project_root: &str) -> PathBuf {
    project_config_dir(project_root).join("rules")
}

/// <project>/.ion/rules-config.json
pub fn project_rules_config_path(project_root: &str) -> PathBuf {
    project_config_dir(project_root).join("rules-config.json")
}

/// <project>/.ion/memory/
pub fn project_memory_dir(project_root: &str) -> PathBuf {
    project_config_dir(project_root).join("memory")
}

// ---------------------------------------------------------------------------
// Worktree 隔离目录
// ---------------------------------------------------------------------------

/// ~/.ion/worktrees/
pub fn worktree_root() -> PathBuf {
    std::env::var("ION_WORKTREE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| root().join("worktrees"))
}

/// ~/.ion/worktrees/<repoName>-<safeBranch>/
pub fn worktree_path(repo_name: &str, safe_branch: &str) -> PathBuf {
    worktree_root().join(format!("{repo_name}-{safe_branch}"))
}

// ---------------------------------------------------------------------------
// 调试日志
// ---------------------------------------------------------------------------

/// ~/.ion/pi-debug.log  (兼容 pi 命名)
pub fn debug_log_path() -> PathBuf {
    root().join("pi-debug.log")
}

// ---------------------------------------------------------------------------
// 系统临时目录（可回收）
// ---------------------------------------------------------------------------

/// 系统临时目录下的 ion 文件
pub fn system_tmp_dir() -> PathBuf {
    std::env::var("ION_TMP_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
}

/// <tmp>/ion-bash-<uuid>.log  (Bash 输出溢出，超过 4KB 时写文件)
pub fn system_bash_log(uuid: &str) -> PathBuf {
    system_tmp_dir().join(format!("ion-bash-{uuid}.log"))
}

/// <tmp>/ion-input-<uuid>.txt  (大输入溢出)
pub fn system_input_overflow(uuid: &str) -> PathBuf {
    system_tmp_dir().join(format!("ion-input-{uuid}.txt"))
}

/// <tmp>/ion-tool-results/<slug>/  (工具结果预算溢出)
pub fn system_tool_results_dir(slug: &str) -> PathBuf {
    system_tmp_dir().join("ion-tool-results").join(slug)
}

/// <tmp>/ion-clipboard-<uuid>.<ext>  (剪贴板粘贴图片)
pub fn system_clipboard_path(uuid: &str, ext: &str) -> PathBuf {
    system_tmp_dir().join(format!("ion-clipboard-{uuid}.{ext}"))
}

// ---------------------------------------------------------------------------
// 初始化
// ---------------------------------------------------------------------------

/// 创建所有需要的目录（首次运行调用）
pub fn ensure_dirs() {
    let dirs = [
        agent_dir(),
        sessions_dir(),
        extensions_dir(),
        skills_dir(),
        prompts_dir(),
        themes_dir(),
        tools_dir(),
        bin_dir(),
        cache_dir(),
        tmp_dir(),
        tmp_extensions_dir(),
        agent_dir().join("extensions-data"),
        agent_dir().join("project-data"),
        agent_dir().join("cwd-data"),
        agent_dir().join("projects"),
        worktree_root(),
        root().join("worktrees"),
    ];
    for dir in &dirs {
        let _ = std::fs::create_dir_all(dir);
    }
}

// ---------------------------------------------------------------------------
// 路径编码（对齐 pi 的 --hash--name-- 格式）
// ---------------------------------------------------------------------------

/// 编码路径名为安全的目录名（对齐 pi 的 --hash--name-- 格式）
pub fn encode_path(path: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    let hash = hasher.finish();

    let name = std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");

    format!("--{hash:x}--{name}--")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// 跨模块共享的 env 测试串行锁。
///
/// `ION_AGENT_DIR` / `ION_SESSION_DIR` / `ION_PROJECT_ROOT` / `SESSION_FILE_OVERRIDE`
/// 等都是进程级状态——并发测试下互相污染会导致随机失败。
/// 任何修改这些状态的测试都应 `let _guard = env_test_lock();` 拿锁。
#[cfg(test)]
pub fn env_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ION_AGENT_DIR / ION_SESSION_DIR / ION_PROJECT_ROOT 等环境变量是进程级状态。
    // 任何修改这些 env 的测试必须串行执行，否则并发下互相污染。
    // 跨模块共享：用相同的 OnceLock key 保证全局唯一。
    fn env_test_lock() -> std::sync::MutexGuard<'static, ()> {
        super::env_test_lock()
    }

    #[test]
    fn root_is_under_dot_ion() {
        let r = root();
        assert!(r.to_str().unwrap().contains(".ion"));
    }

    #[test]
    fn agent_dir_ends_with_agent() {
        let d = agent_dir();
        assert!(d.to_str().unwrap().ends_with("agent"));
    }

    #[test]
    fn session_path_has_cwd_hash() {
        let p = session_jsonl_path("/Users/test/my-project");
        let s = p.to_str().unwrap();
        assert!(s.contains("--"));
        assert!(s.ends_with("session.jsonl"));
        assert!(s.contains("sessions"));
    }

    #[test]
    fn session_cwd_dir_format() {
        let d = session_cwd_dir("/tmp/foo");
        let s = d.to_str().unwrap();
        assert!(s.starts_with(&sessions_dir().to_str().unwrap()));
        assert!(s.contains("--"));
        assert!(s.ends_with("--foo--"));
    }

    #[test]
    fn worktree_path_format() {
        let p = worktree_path("my-repo", "feature/abc");
        assert!(p.to_str().unwrap().ends_with("my-repo-feature/abc"));
    }

    #[test]
    fn encode_path_is_deterministic() {
        let a = encode_path("/Users/test/project");
        let b = encode_path("/Users/test/project");
        assert_eq!(a, b);
        assert!(a.starts_with("--"));
    }

    #[test]
    fn project_config_under_dot_ion() {
        let p = project_settings_path("/tmp/myproject");
        assert!(p.to_str().unwrap().ends_with(".ion/settings.json"));
    }

    #[test]
    fn global_data_dir_format() {
        let d = global_data_dir("my-ext");
        assert!(d.to_str().unwrap().ends_with("my-ext"));
        assert!(d.to_str().unwrap().contains("extensions-data"));
    }

    #[test]
    fn project_data_dir_format() {
        let d = project_data_dir("/root/proj", "ext1");
        assert!(d.to_str().unwrap().contains("ext1"));
        // 应含 project-data（② 项目维度）
        assert!(d.to_str().unwrap().contains("project-data"));
    }

    #[test]
    fn bash_processes_path_is_session_level() {
        // bash 进程存储应在 session 维度下（④ Session 级别隔离）
        let p = bash_processes_path("/tmp/proj", "sess_001");
        assert!(p.to_str().unwrap().ends_with("processes.json"));
        assert!(
            p.to_str().unwrap().contains("sess_001"),
            "应含 session_id 做隔离"
        );
        assert!(p.to_str().unwrap().contains("bash"));
    }

    #[test]
    fn bash_processes_path_different_sessions_isolated() {
        // 不同 session 的 bash 进程路径不同
        let p1 = bash_processes_path("/tmp/proj", "sess_001");
        let p2 = bash_processes_path("/tmp/proj", "sess_002");
        assert_ne!(p1, p2, "不同 session 的进程存储应隔离");
    }

    #[test]
    fn cwd_data_dir_format() {
        let d = cwd_data_dir("/tmp/work", "ext1");
        assert!(d.to_str().unwrap().contains("ext1"));
    }

    #[test]
    fn project_user_state_has_skills() {
        let d = project_private_skills_dir("/p");
        assert!(d.to_str().unwrap().ends_with("skills"));
    }

    #[test]
    fn project_has_rules_and_memory() {
        assert!(project_rules_dir("/p").to_str().unwrap().ends_with("rules"));
        assert!(
            project_memory_dir("/p")
                .to_str()
                .unwrap()
                .ends_with("memory")
        );
    }

    #[test]
    fn themes_tools_bin_dirs() {
        assert!(themes_dir().to_str().unwrap().ends_with("themes"));
        assert!(tools_dir().to_str().unwrap().ends_with("tools"));
        assert!(bin_dir().to_str().unwrap().ends_with("bin"));
    }

    #[test]
    fn system_tmp_has_ion_prefix() {
        let log = system_bash_log("abc123");
        assert!(log.to_str().unwrap().contains("ion-bash-abc123"));
    }

    #[test]
    fn debug_log_is_debug_log() {
        assert!(debug_log_path().to_str().unwrap().ends_with("pi-debug.log"));
    }

    #[test]
    fn session_data_dir_format() {
        let d = session_data_dir("/p", "sess-1", "ext-a");
        let s = d.to_str().unwrap();
        assert!(s.contains("sess-1"));
        assert!(s.contains("ext-a"));
        assert!(s.contains("data"));
    }

    #[test]
    fn extensions_size_returns_err_if_dir_missing() {
        let _guard = env_test_lock();
        // Use a non-existent temp path by overriding the agent dir
        let tmp =
            std::env::temp_dir().join(format!("ion_test_ext_size_missing_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        unsafe {
            std::env::set_var("ION_AGENT_DIR", tmp.to_str().unwrap());
        }
        let result = extensions_size();
        unsafe {
            std::env::remove_var("ION_AGENT_DIR");
        }
        assert!(
            result.is_err(),
            "extensions_size should return Err if dir missing"
        );
    }

    #[test]
    fn extensions_size_matches_known_files() {
        let _guard = env_test_lock();
        let tmp = std::env::temp_dir().join(format!("ion_test_ext_size_{}", std::process::id()));
        let ext_dir = tmp.join("extensions");
        std::fs::create_dir_all(&ext_dir).unwrap();

        // Create known files with known sizes
        std::fs::write(ext_dir.join("a.txt"), "hello").unwrap(); // 5 bytes
        std::fs::write(ext_dir.join("b.txt"), "world!").unwrap(); // 6 bytes
        std::fs::create_dir(ext_dir.join("sub")).unwrap();
        std::fs::write(ext_dir.join("sub").join("c.txt"), "test123").unwrap(); // 7 bytes

        let expected: u64 = 5 + 6 + 7;

        unsafe {
            std::env::set_var("ION_AGENT_DIR", tmp.to_str().unwrap());
        }
        let size = extensions_size().expect("extensions_size should succeed");
        unsafe {
            std::env::remove_var("ION_AGENT_DIR");
        }

        assert_eq!(size, expected, "extensions_size should sum all file sizes");

        // Cleanup
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn ensure_dirs_creates_all() {
        ensure_dirs();
        assert!(agent_dir().exists());
        assert!(sessions_dir().exists());
        assert!(cache_dir().exists());
        assert!(tmp_dir().exists());
        assert!(worktree_root().exists());
        assert!(extensions_dir().exists());
        assert!(themes_dir().exists());
        assert!(tools_dir().exists());
        assert!(bin_dir().exists());
    }

    // ── 缺口 #2/#3：git_project_root + project_key_git + project_root_for_config ──

    #[test]
    fn git_project_root_returns_main_repo() {
        // 当前目录（ion 项目本身）是 git 仓库
        let cwd = std::env::current_dir()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let root = git_project_root(&cwd);
        assert!(root.is_some(), "git_project_root 应返回 Some");
        let root = root.unwrap();
        // 主仓库根应该包含 Cargo.toml（ion 项目根）
        assert!(
            root.join("Cargo.toml").exists(),
            "git_project_root 应指向仓库根（含 Cargo.toml），实际: {:?}",
            root
        );
    }

    #[test]
    fn git_project_root_worktree_shares_main() {
        // 创建临时 worktree，验证 git_project_root 返回主仓库根
        let main_cwd = std::env::current_dir()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let main_root = git_project_root(&main_cwd).expect("主仓库应有 root");

        let wt_path = format!("/tmp/ion_wt_root_test_{}", std::process::id());
        let output = std::process::Command::new("git")
            .args(["worktree", "add", &wt_path])
            .current_dir(&main_cwd)
            .output();

        if output.is_ok() && output.as_ref().unwrap().status.success() {
            let wt_root = git_project_root(&wt_path);
            assert!(
                wt_root.is_some(),
                "worktree 的 git_project_root 应返回 Some"
            );
            assert_eq!(
                wt_root.unwrap(),
                main_root,
                "worktree 和主仓库的 git_project_root 应相同"
            );
            // 清理
            let _ = std::process::Command::new("git")
                .args(["worktree", "remove", "--force", &wt_path])
                .current_dir(&main_cwd)
                .output();
        }
    }

    #[test]
    fn project_key_git_worktree_consistency() {
        // project_key_git 在主仓库和 worktree 下应返回相同 hash
        let main_cwd = std::env::current_dir()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let main_key = project_key_git(&main_cwd);
        assert_eq!(main_key.len(), 16, "project_key_git 应返回 16 位 hex");

        let wt_path = format!("/tmp/ion_wt_key_test_{}", std::process::id());
        let output = std::process::Command::new("git")
            .args(["worktree", "add", &wt_path])
            .current_dir(&main_cwd)
            .output();

        if output.is_ok() && output.as_ref().unwrap().status.success() {
            let wt_key = project_key_git(&wt_path);
            assert_eq!(
                main_key, wt_key,
                "主仓库和 worktree 的 project_key_git 应一致"
            );
            let _ = std::process::Command::new("git")
                .args(["worktree", "remove", "--force", &wt_path])
                .current_dir(&main_cwd)
                .output();
        }
    }

    #[test]
    fn os_tmp_dir_returns_existing_path() {
        let tmp = os_tmp_dir();
        assert!(
            tmp.exists(),
            "os_tmp_dir should return an existing directory"
        );
    }

    #[test]
    fn os_tmp_dir_is_system_temp() {
        assert_eq!(os_tmp_dir(), std::env::temp_dir());
    }

    #[test]
    fn project_root_for_config_env_and_cwd_fallback() {
        let _guard = env_test_lock();
        // 合并成一个测试避免并行竞态（两个测试操作同一个 env var）
        // 步骤 1：未设置 ION_PROJECT_ROOT 时，回退到 current_dir
        // SAFETY: 测试单线程内顺序操作 env var
        unsafe {
            std::env::remove_var("ION_PROJECT_ROOT");
        }
        let root = project_root_for_config();
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(root, cwd, "未设 ION_PROJECT_ROOT 时应回退到 current_dir");

        // 步骤 2：设置 ION_PROJECT_ROOT 后，优先用它
        let test_path = "/tmp/ion_config_root_test_xyz";
        unsafe {
            std::env::set_var("ION_PROJECT_ROOT", test_path);
        }
        let root = project_root_for_config();
        assert_eq!(
            root,
            PathBuf::from(test_path),
            "设置 ION_PROJECT_ROOT 后应优先用它"
        );
        // 清理
        unsafe {
            std::env::remove_var("ION_PROJECT_ROOT");
        }
    }

    #[test]
    fn test_agent_dir_exists() {
        // agent_dir_exists returns a bool — just verify it doesn't panic
        let _result = agent_dir_exists();
        // The result depends on the environment, so we just ensure it runs
    }

    // ── G1 加固：pid 文件身份 + host_running 判定矩阵（文件系统级模拟）──────

    fn pid_tmp_path(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "ion-pid-test-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&d);
        d.join("host.pid")
    }

    #[test]
    fn test_host_pid_file_json_roundtrip() {
        let path = pid_tmp_path("roundtrip");
        let info = HostPidInfo {
            pid: 4242,
            host_id: Some("abc-uuid-123".into()),
            started_at_ms: Some(1_700_000_000_000),
        };
        write_host_pid_file_at(&path, &info).expect("write");
        // 原子性：rename 落位后不留 .tmp 残渣
        assert!(!path.with_file_name("host.pid.tmp").exists());
        let got = read_host_pid_info_at(&path).expect("read");
        assert_eq!(got, info);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn test_host_pid_file_legacy_number_and_garbage() {
        let legacy = pid_tmp_path("legacy");
        std::fs::write(&legacy, "9999\n").unwrap();
        let got = read_host_pid_info_at(&legacy).expect("legacy parse");
        assert_eq!(got.pid, 9999);
        assert_eq!(got.host_id, None, "旧格式无 hostId");
        assert_eq!(got.started_at_ms, None);

        let garbage = pid_tmp_path("garbage");
        std::fs::write(&garbage, "not a pid at all").unwrap();
        assert!(read_host_pid_info_at(&garbage).is_none(), "垃圾内容 → None");
        // 与旧行为一致：不可解析不删文件（留给启动诊断）
        assert!(garbage.exists());
        let _ = std::fs::remove_dir_all(garbage.parent().unwrap());
    }

    #[test]
    fn test_host_running_matrix() {
        let _guard = env_test_lock(); // host_running 读 ION_HOST_SOCKET，串行
        let dir = std::env::temp_dir().join(format!("ion-running-matrix-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let sock = dir.join("m.sock");
        let pid_file = dir.join("m.pid");
        // host_pid_path 由 ION_HOST_SOCKET 派生 → 用 env 把矩阵隔离到临时目录
        unsafe {
            std::env::set_var("ION_HOST_SOCKET", &sock);
        }

        // 场景 1：无 pid 文件 → None
        let _ = std::fs::remove_file(&pid_file);
        assert_eq!(host_running(), None);

        // 场景 2：本进程 pid（存活）+ JSON 带 hostId → Some(own pid)
        write_host_pid_file_at(&pid_file, &HostPidInfo {
            pid: std::process::id(),
            host_id: Some("live-host-id".into()),
            started_at_ms: Some(1),
        })
        .unwrap();
        assert_eq!(host_running(), Some(std::process::id()));

        // 场景 3：本进程 pid + 旧格式纯数字 → Some（兼容）
        std::fs::write(&pid_file, format!("{}\n", std::process::id())).unwrap();
        assert_eq!(host_running(), Some(std::process::id()));

        // 场景 4：死 pid → None + 陈旧文件被顺手清理
        // 造一个确定已死的 pid：fork 出后立即退出的子进程
        let dead = {
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg("echo $$; exit 0")
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout)
                .trim()
                .parse::<u32>()
                .unwrap()
        };
        std::thread::sleep(std::time::Duration::from_millis(50)); // 确保 reaped
        write_host_pid_file_at(&pid_file, &HostPidInfo {
            pid: dead,
            host_id: Some("ghost".into()),
            started_at_ms: Some(1),
        })
        .unwrap();
        assert_eq!(host_running(), None, "死 pid 必须判不在跑");
        assert!(!pid_file.exists(), "陈旧 pid 文件应被清理（自愈）");

        // 场景 5：垃圾内容 → None（不删，留诊断）
        std::fs::write(&pid_file, "garbage").unwrap();
        assert_eq!(host_running(), None);
        assert!(pid_file.exists());

        unsafe {
            std::env::remove_var("ION_HOST_SOCKET");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_host_running_verified_identity() {
        let _guard = env_test_lock();
        let dir = std::env::temp_dir().join(format!("ion-verified-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let sock = dir.join("v.sock");
        let pid_file = dir.join("v.pid");
        unsafe {
            std::env::set_var("ION_HOST_SOCKET", &sock);
        }

        // 场景 A：pid 存活 + socket 背后 host 的 hostId 与 pid 文件一致 → Verified
        let live_id = ion_protocol::generate_host_id();
        write_host_pid_file_at(&pid_file, &HostPidInfo {
            pid: std::process::id(),
            host_id: Some(live_id.clone()),
            started_at_ms: Some(1),
        })
        .unwrap();
        // 起一个真 socket 假装 host：accept 后按 hello 协议回报 hostId
        let listener = std::sync::Arc::new(tokio::net::UnixListener::bind(&sock).unwrap());
        let server_id = live_id.clone();
        let listener2 = std::sync::Arc::clone(&listener);
        let server = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
            let (mut s, _) = listener2.accept().await.unwrap();
            let mut buf = vec![0u8; 512];
            let n = s.read(&mut buf).await.unwrap_or(0);
            if n > 0 {
                let reply = serde_json::json!({
                    "id": "pid-verify",
                    "type": "response",
                    "command": "hello",
                    "success": true,
                    "data": {"protocolVersion": 1, "hostId": server_id},
                });
                let _ = s
                    .write_all(format!("{}\n", reply).as_bytes())
                    .await;
            }
        });
        let checked = host_running_verified(std::time::Duration::from_secs(2)).await;
        assert!(
            matches!(checked, HostIdentityCheck::Verified(_)),
            "身份一致必须 Verified: {checked:?}"
        );
        server.await.unwrap();

        // 场景 B：pid 存活 + socket 背后 hostId 不一致 → IdentityMismatch
        let listener3 = std::sync::Arc::clone(&listener);
        let server = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
            let (mut s, _) = listener3.accept().await.unwrap();
            let mut buf = vec![0u8; 512];
            let n = s.read(&mut buf).await.unwrap_or(0);
            if n > 0 {
                let reply = serde_json::json!({
                    "id": "pid-verify",
                    "type": "response",
                    "command": "hello",
                    "success": true,
                    "data": {"protocolVersion": 1, "hostId": "a-different-host"},
                });
                let _ = s
                    .write_all(format!("{}\n", reply).as_bytes())
                    .await;
            }
        });
        let checked = host_running_verified(std::time::Duration::from_secs(2)).await;
        assert_eq!(
            checked,
            HostIdentityCheck::IdentityMismatch { pid: std::process::id() },
            "身份不一致必须拒绝（pid 复用防线）"
        );
        server.await.unwrap();

        // 场景 C：socket 探活失败 → Unconfirmed。删掉 sock 文件造"socket 不存在"。
        drop(listener);
        let _ = std::fs::remove_file(&sock);
        let checked = host_running_verified(std::time::Duration::from_millis(300)).await;
        assert_eq!(checked, HostIdentityCheck::Unconfirmed, "探活失败 → Unconfirmed");
        // 场景 D：死 pid → Unconfirmed + 陈旧文件清理
        write_host_pid_file_at(&pid_file, &HostPidInfo {
            pid: 2_000_000_000, // 远超正常 pid 上限的值，确定无此进程
            host_id: Some("ghost".into()),
            started_at_ms: Some(1),
        })
        .unwrap();
        let checked = host_running_verified(std::time::Duration::from_millis(100)).await;
        assert_eq!(checked, HostIdentityCheck::Unconfirmed);
        assert!(!pid_file.exists(), "死 pid 的陈旧文件应被清理");

        unsafe {
            std::env::remove_var("ION_HOST_SOCKET");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_socket_file_ownership_check() {
        let dir = std::env::temp_dir().join(format!("ion-owner-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("s.sock");
        std::fs::write(&f, b"placeholder").unwrap();
        // 本进程创建的文件属主 = 自己
        assert!(socket_file_owned_by_self(&f));
        // 注入别人的 euid → 拒绝
        assert!(!socket_file_owned_by(&f, current_euid() + 1));
        // 不存在的文件 → 拒绝（fail-closed）
        assert!(!socket_file_owned_by_self(&dir.join("nope.sock")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
