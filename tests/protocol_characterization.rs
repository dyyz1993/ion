//! 协议信封特征测试（characterization tests）
//!
//! 捕获**当前线上**的 RPC 线协议信封形状——重构（抽出 ion-protocol crate）前后
//! 都必须全绿，它们就是"行为保持"的证明。断言的是真实二进制（`ion --mode rpc`）
//! 在 stdout 上的字节形状，不是内部函数：
//!
//! - worker 响应信封：`{"id","type":"response","command","success","data"|"error"}`
//! - 事件外壳：`{"type":"event","event":{...}}`（worker_ready / rpc_response）
//! - 解析失败帧：`{"type":"error","error":{"message":"invalid JSON: ..."}}`
//! - 请求信封（CLI `ion rpc` 形状）：`{"id","method","params","session"?}`
//! - host 级 hello 握手：`data.protocolVersion` + `data.hostId`（逻辑实例身份）
//!
//! 隔离纪律：私有 HOME + ION_SESSION_DIR + ION_HOST_SOCKET，绝不触碰真实 ~/.ion。
//! 子进程用精确 PID 清理（drop stdin → wait → kill），绝不 pkill。

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// 起一个隔离环境的 worker 子进程，返回（child, 行接收信道）。
fn spawn_worker() -> (Child, mpsc::Receiver<String>) {
    let tmp = std::env::temp_dir().join(format!(
        "ion-proto-char-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let home = tmp.join("home");
    let sessions = tmp.join("sessions");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&sessions).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_ion"))
        .args(["--mode", "rpc", "--session", "proto-char-test"])
        .env("HOME", &home)
        .env("ION_SESSION_DIR", &sessions)
        .env("ION_HOST_SOCKET", tmp.join("unused.sock"))
        .current_dir(&tmp)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn ion --mode rpc");

    // 读线程：stdout 每行推入信道（无阻塞主测试）
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            match line {
                Ok(l) => {
                    if tx.send(l).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    (child, rx)
}

/// 单行读取（带总 deadline 的阻塞读，超时/EOF 即 panic 带诊断）。
/// 120s 上限：worker 启动含 MCP 探测 3s 超时；多测试并行时留足余量。
fn next_line_before(rx: &mpsc::Receiver<String>, deadline: std::time::Instant) -> String {
    let remain = deadline.saturating_duration_since(std::time::Instant::now());
    assert!(!remain.is_zero(), "timeout waiting for worker stdout line");
    rx.recv_timeout(remain).expect("worker stdout closed early")
}

/// 读下一行 JSON（跳过空行与非 JSON 行，如 tracing 噪声）。
fn next_json(rx: &mpsc::Receiver<String>) -> serde_json::Value {
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    loop {
        let line = next_line_before(rx, deadline);
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
            return v;
        }
    }
}

/// 读下一行 JSON，直到谓词命中（跳过无关帧），返回命中的帧。
fn next_json_matching(
    rx: &mpsc::Receiver<String>,
    pred: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    next_json_matching_before(rx, deadline, pred)
}

/// 同 next_json_matching，但 deadline 由调用方控制（行长上限测试用短 deadline：
/// 红态下不应长时间等待，绿态下正常秒级命中）。
fn next_json_matching_before(
    rx: &mpsc::Receiver<String>,
    deadline: std::time::Instant,
    pred: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    loop {
        let line = next_line_before(rx, deadline);
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed)
            && pred(&v)
        {
            return v;
        }
    }
}

/// 发送一行请求（CLI `ion rpc` 的请求信封形状）。
fn send(stdin: &mut std::process::ChildStdin, v: serde_json::Value) {
    writeln!(stdin, "{v}").expect("write to worker stdin");
    stdin.flush().expect("flush worker stdin");
}

/// 发送一行裸文本（非 JSON）。
fn send_raw(stdin: &mut std::process::ChildStdin, line: &str) {
    writeln!(stdin, "{line}").expect("write to worker stdin");
    stdin.flush().expect("flush worker stdin");
}

/// 精确 PID 清理：drop stdin → 等退出 → 超时才 kill（绝不 pkill）。
fn shutdown(mut child: Child) {
    drop(child.stdin.take());
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        match child.try_wait().expect("try_wait") {
            Some(_) => return,
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// 起一个隔离环境的 host（`ion serve`），返回（child, socket 路径）。
/// 隔离三件套：私有 HOME / ION_HOST_SOCKET / ION_SESSION_DIR，绝不触碰真实 ~/.ion。
fn spawn_host() -> (Child, std::path::PathBuf, std::path::PathBuf) {
    // 🔴 macOS SUN_LEN=104：temp_dir() 是 /var/folders/.../T/ 长前缀，
    // 目录名必须极短（曾用 ion-proto-char-host-{pid}-{19位纳秒} 压线越界，
    // PID 位数变化即随机 bind 失败"path must be shorter than SUN_LEN"）
    let tmp = std::env::temp_dir().join(format!(
        "ipc-h-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    ));
    let home = tmp.join("home");
    let sessions = tmp.join("sessions");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&sessions).unwrap();
    let sock = tmp.join("host.sock");

    let child = Command::new(env!("CARGO_BIN_EXE_ion"))
        .arg("serve")
        .env("HOME", &home)
        .env("ION_SESSION_DIR", &sessions)
        .env("ION_HOST_SOCKET", &sock)
        .current_dir(&tmp)
        .stdout(Stdio::null())
        .stderr(std::process::Stdio::from(
            std::fs::File::create(tmp.join("host_stderr.log")).unwrap(),
        ))
        .spawn()
        .expect("spawn ion serve");

    // 等 socket 就绪（30s 上限）
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if std::os::unix::net::UnixStream::connect(&sock).is_ok() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            let dbg = std::fs::read_to_string(tmp.join("host_stderr.log")).unwrap_or_default();
            panic!("host socket 30s 未就绪 | tmp={tmp:?} | host_stderr:\n{dbg}");  
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    (child, sock, tmp)
}

/// 单连接 hello 往返：发送请求行，读首帧 JSON（跳过空行/非 JSON）。
fn hello_roundtrip(sock: &std::path::Path, id: &str) -> serde_json::Value {
    use std::io::{BufRead, BufReader, Write};
    let mut stream = std::os::unix::net::UnixStream::connect(sock).expect("connect host sock");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set timeout");
    writeln!(stream, r#"{{"id":"{id}","method":"hello"}}"#).expect("send hello");
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).expect("read hello reply");
        assert!(n > 0, "host 在 hello 响应前关闭了连接");
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) {
            return v;
        }
    }
}

// ---------------------------------------------------------------------------
// 特征 7：host 级 hello 握手 —— protocolVersion:1 + hostId（逻辑实例身份，
// 规范小写 UUIDv4；同一 host 进程跨连接稳定；host 不落盘，重启即换）
// ---------------------------------------------------------------------------

#[test]
fn host_hello_carries_protocol_version_and_stable_host_id() {
    let (child, sock, tmp) = spawn_host();
    let r1 = hello_roundtrip(&sock, "h1");
    assert_eq!(r1["type"], "response", "hello 响应信封: {r1}");
    assert_eq!(r1["id"], "h1");
    assert_eq!(r1["success"], true);
    assert_eq!(r1["data"]["protocolVersion"], 1);
    let host_id = r1["data"]["hostId"].as_str().expect("data.hostId 存在");
    assert_eq!(host_id.len(), 36, "hostId 是 UUID 形状: {host_id}");
    for (i, part) in host_id.split('-').enumerate() {
        match i {
            0 => assert_eq!(part.len(), 8),
            1 | 2 | 3 => assert_eq!(part.len(), 4),
            4 => assert_eq!(part.len(), 12),
            _ => panic!("UUID 段数异常: {host_id}"),
        }
    }
    assert!(host_id.chars().all(|c| c == '-' || c.is_ascii_lowercase() || c.is_ascii_digit()),
        "hostId 全小写 hex: {host_id}");
    assert_eq!(&host_id[14..15], "4", "UUIDv4 version 位: {host_id}");
    assert!(matches!(&host_id[19..20], "8" | "9" | "a" | "b"), "UUIDv4 variant 位: {host_id}");

    // 新连接再次 hello：同一 host 进程 hostId 稳定（pin 校验的前提）
    let r2 = hello_roundtrip(&sock, "h2");
    assert_eq!(r2["data"]["hostId"].as_str().expect("hostId2"), host_id);

    shutdown(child);
    let _ = std::fs::remove_dir_all(&tmp);
}

// ---------------------------------------------------------------------------
// 特征 1：启动序列 —— 首帧是 ready 公告（无外壳），随后 worker_ready 走事件外壳
// ready: {"type":"ready","session","model","provider","channels","version"}
// ready 后: {"type":"event","event":{"type":"worker_ready"}}
// ---------------------------------------------------------------------------

#[test]
fn startup_sequence_ready_then_worker_ready_shell() {
    let (child, rx) = spawn_worker();
    // 首帧：ready 公告（裸帧，不带事件外壳）
    let first = next_json(&rx);
    assert_eq!(first["type"], "ready", "worker 首帧必须是 ready 公告: {first}");
    assert_eq!(first["session"], "proto-char-test");
    assert!(first["model"].is_string());
    assert!(first["provider"].is_string());
    assert!(first["channels"].is_array());
    assert!(first["version"].is_string());
    // 随后：worker_ready 必须包事件外壳
    let ready_ev = next_json_matching(&rx, |v| {
        v["type"] == "event" && v["event"]["type"] == "worker_ready"
    });
    assert_eq!(ready_ev["type"], "event", "worker_ready 外壳 type 必须是 event: {ready_ev}");
    assert_eq!(ready_ev["event"]["type"], "worker_ready");
    shutdown(child);
}

// ---------------------------------------------------------------------------
// 特征 2：成功响应信封 —— {"id","type":"response","command","success","data"}
// （health 命令：零 LLM 依赖，纯状态回显）
// ---------------------------------------------------------------------------

#[test]
fn success_response_envelope_shape() {
    let (mut child, rx) = spawn_worker();
    let mut stdin = child.stdin.take().unwrap();
    // 等 worker_ready（主循环就绪）再发请求
    next_json_matching(&rx, |v| v["event"]["type"] == "worker_ready");

    // 请求形状 = CLI `ion rpc` 的构造：{id, method, params}（可带 session）
    send(
        &mut stdin,
        serde_json::json!({"id": "1", "method": "health", "params": {}}),
    );
    let resp = next_json_matching(&rx, |v| {
        v.get("id").is_some() && v["type"] == "response"
    });
    assert_eq!(resp["id"], "1");
    assert_eq!(resp["type"], "response");
    assert_eq!(resp["command"], "health", "worker 响应必须带 command 字段: {resp}");
    assert_eq!(resp["success"], true);
    assert_eq!(resp["data"]["status"], "ok");
    assert!(resp["data"]["uptime_secs"].is_u64());
    assert!(resp["data"]["pid"].is_u64());
    assert!(resp["data"]["version"].is_string());
    // 成功响应不出现 error 字段
    assert!(resp.get("error").is_none(), "成功信封不应有 error 字段: {resp}");
    shutdown(child);
}

// ---------------------------------------------------------------------------
// 特征 3：失败响应信封 —— error 字段 + Unknown command 文本
// ---------------------------------------------------------------------------

#[test]
fn error_response_envelope_shape() {
    let (mut child, rx) = spawn_worker();
    let mut stdin = child.stdin.take().unwrap();
    next_json_matching(&rx, |v| v["event"]["type"] == "worker_ready");

    send(
        &mut stdin,
        serde_json::json!({"id": "2", "method": "definitely_not_a_real_method", "params": {}}),
    );
    let resp = next_json_matching(&rx, |v| {
        v.get("id").is_some() && v["type"] == "response"
    });
    assert_eq!(resp["id"], "2");
    assert_eq!(resp["type"], "response");
    assert_eq!(resp["command"], "definitely_not_a_real_method");
    assert_eq!(resp["success"], false);
    let err = resp["error"].as_str().expect("error 必须是字符串");
    assert!(err.contains("Unknown command"), "错误文本要可诊断: {err}");
    assert!(resp.get("data").is_none(), "失败信封不应有 data 字段: {resp}");
    shutdown(child);
}

// ---------------------------------------------------------------------------
// 特征 4：每条用户 RPC 都广播 rpc_response 事件（事件外壳内）
// {"type":"event","event":{"type":"rpc_response","id","method","success","sessionId","timestamp"}}
// ---------------------------------------------------------------------------

#[test]
fn rpc_response_event_shell_shape() {
    let (mut child, rx) = spawn_worker();
    let mut stdin = child.stdin.take().unwrap();
    next_json_matching(&rx, |v| v["event"]["type"] == "worker_ready");

    send(
        &mut stdin,
        serde_json::json!({"id": "3", "method": "health", "params": {}}),
    );
    let ev = next_json_matching(&rx, |v| v["event"]["type"] == "rpc_response");
    assert_eq!(ev["type"], "event", "rpc_response 必须包事件外壳: {ev}");
    let inner = &ev["event"];
    assert_eq!(inner["type"], "rpc_response");
    assert_eq!(inner["id"], "3");
    assert_eq!(inner["method"], "health");
    assert_eq!(inner["success"], true);
    // sessionId / timestamp 摘要字段存在（直 spawn worker 时 sessionId 可为空串）
    assert!(inner.get("sessionId").is_some(), "事件要带 sessionId 键: {inner}");
    assert!(inner["timestamp"].is_u64(), "事件要带 timestamp: {inner}");
    shutdown(child);
}

// ---------------------------------------------------------------------------
// 特征 5：非法 JSON 行 → error 帧 {"type":"error","error":{"message":"invalid JSON: ..."}}
// ---------------------------------------------------------------------------

#[test]
fn invalid_json_yields_error_frame() {
    let (mut child, rx) = spawn_worker();
    let mut stdin = child.stdin.take().unwrap();
    next_json_matching(&rx, |v| v["event"]["type"] == "worker_ready");

    send_raw(&mut stdin, "{ this is really not json");
    let frame = next_json_matching(&rx, |v| v["type"] == "error");
    assert!(frame.get("id").is_none(), "error 帧不应带 id: {frame}");
    let msg = frame["error"]["message"].as_str().expect("error.message 字符串");
    assert!(msg.contains("invalid JSON"), "错误信息: {msg}");
    shutdown(child);
}

// ---------------------------------------------------------------------------
// 特征 6：请求信封兼容 CLI 带 session 的形状（{"id","method","params","session"}）
// ---------------------------------------------------------------------------

#[test]
fn request_with_session_field_is_accepted() {
    let (mut child, rx) = spawn_worker();
    let mut stdin = child.stdin.take().unwrap();
    next_json_matching(&rx, |v| v["event"]["type"] == "worker_ready");

    send(
        &mut stdin,
        serde_json::json!({
            "id": "rpc-client",
            "method": "health",
            "params": {},
            "session": "proto-char-test"
        }),
    );
    let resp = next_json_matching(&rx, |v| {
        v.get("id").is_some() && v["type"] == "response"
    });
    assert_eq!(resp["id"], "rpc-client");
    assert_eq!(resp["success"], true);
    shutdown(child);
}

// ---------------------------------------------------------------------------
// 特征 7（P0.1 对标 pi framing 行长上限）：>16MiB 的行 → 错误帧 + 退出
// {"type":"error","error":"line too large: ...","limitBytes":16777216,"actualBytes":...}
//
// 防滥用：恶意/异常客户端发超大行不允许被缓冲进内存（OOM 风险）。上限检查必须
// 先于 JSON 解析；超限后 worker 发错误帧并按现有 EOF 模式优雅退出。
// ---------------------------------------------------------------------------

#[test]
fn oversized_line_rejected_with_error_frame() {
    let (mut child, rx) = spawn_worker();
    next_json_matching(&rx, |v| v["event"]["type"] == "worker_ready");

    // 写线程：17MiB 超限行分块写入。绿态下 worker 消费到 ~16MiB 即拒绝并退出，
    // 剩余写入会 EPIPE——写线程必须吞掉写错误退出而非 panic/永久阻塞。
    let stdin = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || {
        let mut w = stdin;
        let chunk = "x".repeat(1024 * 1024);
        for _ in 0..17 {
            if w.write_all(chunk.as_bytes()).is_err() {
                return; // worker 已断开（预期：超限拒绝后退出）
            }
        }
        let _ = w.write_all(b"\n");
        let _ = w.flush();
    });

    // 错误帧（短 deadline：红态下 20s 内拿不到 → 失败，而非挂 120s）
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let frame = next_json_matching_before(&rx, deadline, |v| {
        v["type"] == "error" && v.get("limitBytes").is_some()
    });
    let err = frame["error"].as_str().expect("error 必须是字符串");
    assert!(
        err.contains("line too large"),
        "错误信息要可诊断: {frame}"
    );
    assert_eq!(
        frame["limitBytes"].as_u64(),
        Some(16 * 1024 * 1024),
        "上限必须是 16MiB: {frame}"
    );
    let actual = frame["actualBytes"].as_u64().expect("actualBytes");
    assert!(
        actual > 16 * 1024 * 1024,
        "actualBytes 应报告超限时的实际字节: {frame}"
    );

    // 超限 → 退出（现有 EOF 处理模式），不留在无读端僵尸状态
    let exit_deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait().expect("try_wait") {
            Some(_) => break,
            None if std::time::Instant::now() < exit_deadline => {
                std::thread::sleep(Duration::from_millis(100));
            }
            None => panic!("worker 收到超限行后应在 10s 内退出"),
        }
    }
    // 写线程收尾（worker 已退出，write 应已失败返回）
    let _ = writer.join();
    shutdown(child);
}
