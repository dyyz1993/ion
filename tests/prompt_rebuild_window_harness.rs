//! P0 复现 harness：prompt 撞上 worker 重建窗口被吞。
//!
//! Bug（J7 e2e 三个 case 撞上）：worker 死亡（kill -9）触发重建窗口（Dead 标记 /
//! AUTO-RECOVERY 原地重建期间），此时客户端发 prompt → RPC 返回 success，但
//! 新 worker 起来后这条 prompt 永远不执行（无 agent_start、user 消息不落盘）。
//!
//! 复现策略（确定性，不依赖真 LLM）：
//!   1. 进程内 mock OpenAI SSE server（固定文本回复，300ms 延迟）；
//!   2. 隔离 host：私有 HOME + ION_HOST_SOCKET + ION_SESSION_DIR，spawn `ion serve`；
//!   3. 脚本化时序：warm-up prompt（验证管道通畅）→ kill -9 worker（只杀自己
//!      spawn 的 host 的子进程，精确 PID）→ 窗口期内（0/100/500/1000ms）发探测
//!      prompt（唯一码词）→ 观察会话 JSONL 是否出现码词（15s 观察窗）；
//!   4. 契约断言（修复后必须成立）：**prompt 不许静默丢**——RPC success 就必须
//!      真的执行（码词落盘）；否则必须返回明确错误。修复前：success && 码词
//!      永不落盘 = 吞没（红）。
//!
//! 两组场景：
//!   - respawn off（默认）：Dead 记录留在 registry 的窗口；
//!   - respawn on（runtime.auto_respawn_local=true）：AUTO-RECOVERY 同 sid 原地
//!     重建窗口（Dead→替补 spawn→register 的竞态窗口最宽）。
//!
//! 隔离纪律：绝不读写真实 ~/.ion；进程清理只 kill 自己 spawn 的子进程（精确 PID）。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// 观察窗：探测 prompt 发出后等码词落盘的上限（重建 + bootstrap 轮 + mock 延迟都要兜住）。
const OBSERVE_SECS: u64 = 20;
/// RPC 客户端单帧超时（对齐 `ion rpc` 的 30s）。
const RPC_TIMEOUT: Duration = Duration::from_secs(30);

fn find_ion_bin() -> String {
    std::env::var("ION_WORKER_BIN").unwrap_or_else(|_| {
        let current_exe = std::env::current_exe().ok();
        if let Some(exe) = current_exe
            && let Some(parent) = exe.parent()
        {
            let sibling = parent.join("ion");
            if sibling.exists() {
                return sibling.to_string_lossy().to_string();
            }
            if let Some(grandparent) = parent.parent() {
                let alt = grandparent.join("ion");
                if alt.exists() {
                    return alt.to_string_lossy().to_string();
                }
            }
        }
        "ion".to_string()
    })
}

fn rand_suffix() -> String {
    use std::time::SystemTime;
    format!(
        "{:x}{:x}",
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos(),
        std::process::id()
    )
}

// ─────────────────────────────────────────────────────────────
// mock OpenAI SSE server
// ─────────────────────────────────────────────────────────────

/// 每个请求回一条固定文本补全（无 tool calls → 一轮即结束）。
/// `delay_ms`：响应前 sleep，模拟慢 LLM（拖住 run）。
fn spawn_mock_sse_server(delay_ms: u64) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock sse");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || handle_sse(stream, delay_ms));
        }
    });
    port
}

fn handle_sse(mut stream: TcpStream, delay_ms: u64) {
    // 读掉请求头 + body（Content-Length 内），别把 body 留在内核缓冲里
    let mut buf = String::new();
    let mut chunk = [0u8; 4096];
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if buf.contains("\r\n\r\n") {
            let clen = buf
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
                .and_then(|l| l.split(':').nth(1))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            let header_end = buf.find("\r\n\r\n").unwrap() + 4;
            if buf.len() >= header_end + clen {
                break;
            }
        }
        if Instant::now() > deadline {
            break;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.push_str(&String::from_utf8_lossy(&chunk[..n])),
        }
    }
    std::thread::sleep(Duration::from_millis(delay_ms));
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"MOCK_ACK\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.flush();
}

// ─────────────────────────────────────────────────────────────
// host 生命周期（精确 PID；只杀自己 spawn 的 host）
// ─────────────────────────────────────────────────────────────

struct TestHost {
    child: std::process::Child,
    pid: u32,
    sock: std::path::PathBuf,
    log: std::path::PathBuf,
}

impl Drop for TestHost {
    fn drop(&mut self) {
        let _ = Command::new("/bin/kill").args(["-15", &self.pid.to_string()]).status();
        for _ in 0..50 {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                _ => std::thread::sleep(Duration::from_millis(100)),
            }
        }
        let _ = Command::new("/bin/kill").args(["-9", &self.pid.to_string()]).status();
        let _ = self.child.wait();
    }
}

fn start_host(root: &std::path::Path, port: u16, auto_respawn: bool) -> TestHost {
    let home = root.join("home");
    let sessions = root.join("sessions");
    let proj = root.join("proj");
    std::fs::create_dir_all(home.join(".ion")).unwrap();
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(proj.join("README.md"), "# prsw harness project\n").unwrap();

    let respawn_field = if auto_respawn {
        "\"auto_respawn_local\": true"
    } else {
        "\"auto_respawn_local\": false"
    };
    let config = format!(
        r#"{{
  "default_provider": "mock",
  "default_model": "mock-1",
  "runtime": {{{respawn_field}}},
  "providers": {{
    "mock": {{
      "name": "mock",
      "api": "openai-completions",
      "base_url": "http://127.0.0.1:{port}/v4",
      "api_key": "harness-key-not-real",
      "models": [
        {{"id": "mock-1", "name": "Mock 1"}}
      ]
    }}
  }}
}}"#
    );
    std::fs::write(home.join(".ion").join("config.json"), config).unwrap();

    let sock = root.join("host.sock");
    let log_path = root.join("host.log");
    let log = std::fs::File::create(&log_path).unwrap();
    let log_err = log.try_clone().unwrap();
    let mut cmd = Command::new(find_ion_bin());
    cmd.arg("serve")
        .current_dir(&proj)
        .env("HOME", &home)
        .env("ION_HOST_SOCKET", &sock)
        .env("ION_SESSION_DIR", &sessions)
        .env("RUST_LOG", "info")
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    let child = cmd.spawn().expect("spawn ion serve failed");
    let pid = child.id();
    let mut host = TestHost { child, pid, sock, log: log_path };

    // 等 socket 就绪（list_sessions 可答，30s 上限）
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if matches!(host.child.try_wait(), Ok(Some(_))) {
            panic!("host 提前退出。log:\n{}", host_log_tail(&host, 30));
        }
        if let Some(v) = host_rpc(&host, "list_sessions", "{}") {
            if v.get("success").and_then(|s| s.as_bool()) == Some(true) {
                break;
            }
        }
        assert!(Instant::now() < deadline, "host 30s 未就绪。log:\n{}", host_log_tail(&host, 30));
        std::thread::sleep(Duration::from_millis(300));
    }
    host
}

fn host_log_tail(host: &TestHost, n: usize) -> String {
    let c = std::fs::read_to_string(&host.log).unwrap_or_default();
    let lines: Vec<&str> = c.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

// ─────────────────────────────────────────────────────────────
// Unix socket RPC 客户端（一问一答，跳过事件帧）
// ─────────────────────────────────────────────────────────────

fn unix_rpc(sock: &std::path::Path, id: &str, method: &str, params: &str, session: Option<&str>) -> Option<serde_json::Value> {
    let mut stream = std::os::unix::net::UnixStream::connect(sock).ok()?;
    stream.set_read_timeout(Some(RPC_TIMEOUT)).ok()?;
    let mut req = serde_json::json!({"id": id, "method": method, "params": serde_json::from_str::<serde_json::Value>(params).unwrap_or_default()});
    if let Some(sid) = session {
        req["session"] = serde_json::json!(sid);
    }
    stream.write_all(format!("{req}\n").as_bytes()).ok()?;
    stream.flush().ok()?;
    let mut reader = BufReader::new(stream);
    let deadline = Instant::now() + RPC_TIMEOUT;
    loop {
        if Instant::now() > deadline {
            return Some(serde_json::json!({"type": "response", "id": id, "success": false, "error": "client-side read timeout"}));
        }
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => return None,
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) else { continue };
                // 响应帧：type=response（事件帧跳过）
                if v.get("type").and_then(|t| t.as_str()) == Some("response") {
                    return Some(v);
                }
            }
            Err(_) => return None,
        }
    }
}

fn host_rpc(host: &TestHost, method: &str, params: &str) -> Option<serde_json::Value> {
    let id = format!("h{}", rand_suffix());
    unix_rpc(&host.sock, &id, method, params, None)
}

fn session_rpc(host: &TestHost, sid: &str, method: &str, params: &str) -> Option<serde_json::Value> {
    let id = format!("s{}", rand_suffix());
    unix_rpc(&host.sock, &id, method, params, Some(sid))
}

// ─────────────────────────────────────────────────────────────
// worker 进程定位 / 精确 kill
// ─────────────────────────────────────────────────────────────

fn find_worker_pid(host_pid: u32, sid: &str) -> Option<u32> {
    let out = Command::new("/usr/bin/pgrep")
        .args(["-P", &host_pid.to_string(), "-f", &format!("rpc --session {sid}")])
        .output()
        .ok()?;
    let line = String::from_utf8_lossy(&out.stdout);
    line.split_whitespace().next().and_then(|p| p.parse().ok())
}

fn pid_alive(pid: u32) -> bool {
    Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn kill_worker_sigkill(pid: u32) {
    let _ = Command::new("/bin/kill").args(["-9", &pid.to_string()]).status();
    for _ in 0..50 {
        if !pid_alive(pid) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("kill -9 后 pid {pid} 仍存活");
}

// ─────────────────────────────────────────────────────────────
// 会话 JSONL 观测
// ─────────────────────────────────────────────────────────────

fn session_jsonl(root: &std::path::Path, sid: &str) -> Option<std::path::PathBuf> {
    let target = format!("{sid}.jsonl");
    let mut stack = vec![root.join("sessions")];
    while let Some(d) = stack.pop() {
        if let Ok(rd) = std::fs::read_dir(&d) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.file_name().map(|n| n.to_string_lossy() == target).unwrap_or(false) {
                    return Some(p);
                }
            }
        }
    }
    None
}

fn jsonl_contains(root: &std::path::Path, sid: &str, needle: &str) -> bool {
    session_jsonl(root, sid)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|c| c.contains(needle))
        .unwrap_or(false)
}

// ─────────────────────────────────────────────────────────────
// 单窗口采样：一个 offset 一次完整生命周期
// ─────────────────────────────────────────────────────────────

struct ProbeResult {
    /// RPC 响应帧（None = 连接断/无帧；success 字段判定诚实性）
    rpc: Option<serde_json::Value>,
    rpc_raw: String,
    /// 观察窗结束时码词是否落盘
    delivered: bool,
    /// 观察窗内是否出现过新 worker 进程
    new_worker_spawned: bool,
}

fn probe_offset(offset_ms: u64, auto_respawn: bool) -> ProbeResult {
    let port = spawn_mock_sse_server(300);
    let root = std::env::temp_dir().join(format!("ion-prsw-{}-{}", std::process::id(), rand_suffix()));
    let host = start_host(&root, port, auto_respawn);

    // 1) create session + 等 worker 进程出现
    let create = host_rpc(&host, "create_session", r#"{"agent":"build"}"#)
        .expect("create_session 无响应");
    assert!(
        create["success"].as_bool() == Some(true),
        "create_session 失败: {create}"
    );
    let sid = create["data"]["session_id"].as_str().expect("无 session_id").to_string();
    let deadline = Instant::now() + Duration::from_secs(15);
    let worker_pid = loop {
        if let Some(p) = find_worker_pid(host.pid, &sid) {
            break p;
        }
        assert!(Instant::now() < deadline, "15s 未见 worker 进程。log:\n{}", host_log_tail(&host, 30));
        std::thread::sleep(Duration::from_millis(200));
    };

    // 2) warm-up prompt（验证管道通畅；码词落盘 = 前置条件）
    let warm_marker = format!("WARMUP_{}", rand_suffix());
    let warm = session_rpc(&host, &sid, "prompt", &format!(r#"{{"text":"{warm_marker}"}}"#))
        .expect("warm-up prompt 无响应");
    assert!(
        warm["success"].as_bool() == Some(true),
        "warm-up prompt 失败（前置条件不成立）: {warm}"
    );
    let deadline = Instant::now() + Duration::from_secs(OBSERVE_SECS);
    while !jsonl_contains(&root, &sid, &warm_marker) {
        assert!(
            Instant::now() < deadline,
            "warm-up 轮 {OBSERVE_SECS}s 未落盘（管道前置失败）。log:\n{}",
            host_log_tail(&host, 40)
        );
        std::thread::sleep(Duration::from_millis(500));
    }
    // 等轮次收尾（agent idle），避免 kill 打在 warm-up run 中间引入无关变量
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let info = session_rpc(&host, &sid, "get_session_info", "{}").unwrap_or_default();
        if !info["data"]["is_running"].as_bool().unwrap_or(true) {
            break;
        }
        assert!(Instant::now() < deadline, "warm-up 轮 15s 未结束。log:\n{}", host_log_tail(&host, 40));
        std::thread::sleep(Duration::from_millis(500));
    }

    // 3) kill -9 worker（精确 PID，只杀 host 自己的子进程）
    let old_worker = find_worker_pid(host.pid, &sid).unwrap_or(worker_pid);
    kill_worker_sigkill(old_worker);

    // 4) 窗口期探测：等 offset 后发 prompt
    std::thread::sleep(Duration::from_millis(offset_ms));
    let probe_marker = format!("PROBE_{}", rand_suffix());
    let resp = session_rpc(&host, &sid, "prompt", &format!(r#"{{"text":"{probe_marker}"}}"#));
    let rpc_raw = serde_json::to_string(&resp).unwrap_or_default();

    // 5) 观察窗：码词是否落盘 / 新 worker 是否出现
    let deadline = Instant::now() + Duration::from_secs(OBSERVE_SECS);
    let mut delivered = false;
    let mut new_worker_spawned = false;
    while Instant::now() < deadline {
        if jsonl_contains(&root, &sid, &probe_marker) {
            delivered = true;
            break;
        }
        if let Some(p) = find_worker_pid(host.pid, &sid) {
            if p != old_worker && pid_alive(p) {
                new_worker_spawned = true;
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    let _ = std::fs::remove_dir_all(&root);
    ProbeResult { rpc: resp, rpc_raw, delivered, new_worker_spawned }
}

/// 契约断言（修复后）：
/// 1. RPC 必须 success=true 且码词落盘（投递语义诚实——harness 受控环境下
///    ensure_live_worker_for_session 保证补拉 + 投递）；
/// 2. 不得出现误导性错误 "worker not found: sess_xxx"（session 明明存在，
///    只是 worker 死了——这是 P0 吞没的表象之一）。
fn assert_no_swallow(label: &str, r: &ProbeResult) -> Option<String> {
    let success = r.rpc.as_ref().and_then(|v| v["success"].as_bool()).unwrap_or(false);
    if r.rpc_raw.contains("worker not found: sess_") {
        return Some(format!(
            "{label}: 出现误导性错误 \"worker not found: sess_*\"（session 存在，worker 死亡不应报 worker 不存在）。resp={}",
            r.rpc_raw
        ));
    }
    if success && !r.delivered {
        Some(format!(
            "{label}: prompt 被吞——RPC 返回 success 但码词 {OBSERVE_SECS}s 未落盘。resp={} new_worker={}",
            r.rpc_raw, r.new_worker_spawned
        ))
    } else if !success {
        Some(format!(
            "{label}: 受控环境下 prompt 必须 success（ensure+投递保证）；得到失败响应: {}",
            r.rpc_raw
        ))
    } else {
        println!("  [{label}] delivered ✓ (resp={}, new_worker={})", r.rpc_raw, r.new_worker_spawned);
        None
    }
}

// ─────────────────────────────────────────────────────────────
// 测试：多点位采样（respawn off：Dead 记录窗口）
// ─────────────────────────────────────────────────────────────

static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn run_case(label: &str, offset_ms: u64, auto_respawn: bool) {
    let _guard = SERIAL.lock().unwrap();
    println!("== {label}: kill -9 后 {offset_ms}ms 发 prompt (auto_respawn={auto_respawn}) ==");
    let r = probe_offset(offset_ms, auto_respawn);
    println!(
        "  [{label}] rpc={} delivered={} new_worker={}",
        r.rpc_raw, r.delivered, r.new_worker_spawned
    );
    if let Some(evidence) = assert_no_swallow(label, &r) {
        panic!("{evidence}");
    }
}

#[test]
fn probe_kill_then_prompt_0ms_no_respawn() {
    run_case("off/0ms", 0, false);
}

#[test]
fn probe_kill_then_prompt_100ms_no_respawn() {
    run_case("off/100ms", 100, false);
}

#[test]
fn probe_kill_then_prompt_500ms_no_respawn() {
    run_case("off/500ms", 500, false);
}

#[test]
fn probe_kill_then_prompt_1s_no_respawn() {
    run_case("off/1s", 1000, false);
}

// ─────────────────────────────────────────────────────────────
// 测试：AUTO-RECOVERY 原地重建窗口（respawn on）
// ─────────────────────────────────────────────────────────────

#[test]
fn probe_kill_then_prompt_100ms_respawn() {
    run_case("on/100ms", 100, true);
}

#[test]
fn probe_kill_then_prompt_500ms_respawn() {
    run_case("on/500ms", 500, true);
}

#[test]
fn probe_kill_then_prompt_1s_respawn() {
    run_case("on/1s", 1000, true);
}
