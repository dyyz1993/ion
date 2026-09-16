//! Ask-hosting worker harness（M2 来源 1：worker 级 Ask 托管全链路）。
//!
//! 背景（J8 缺口）：worker 进程内权限引擎（SecuredRuntime::resolve_ask）发 Ask →
//! 事件流转发到订阅者（webui 显示 ⏸）→ 但没有任何应答通道——Ask 之前在 worker
//! 模式下事件甚至发不出去（event_bus 从未接线 → 路径 3 直接 deny）。
//!
//! M2 修复链路（本 harness 验证 worker 侧半段；host 侧半段见 approval_bus_ci.sh）：
//!   1. worker（--mode rpc）启动即置 runtime::set_worker_rpc_mode(true)。
//!   2. CommandGuard 中危命令 → resolve_ask → **stdout 上报** `Ask`
//!      extension_event（带 request_id）+ 注册 runtime::pending_ui oneshot。
//!   3. harness 从 worker stdout 读到 Ask 帧（拿 request_id）→ 模拟 host 转发
//!      approval_respond：给 worker 发 `ask_respond {request_id, response:"allow"}`。
//!   4. worker 从 pending_ui 取 oneshot 放行 → 命令真执行 → LLM 第 2 轮 →
//!      agent_end；stdout 还应出现 `AskResolved`（供 host 消除总线条目）。
//!   5. deny 用例：ask_respond response:"deny" → 工具结果含"用户拒绝"。
//!
//! 隔离：私有 HOME（临时目录）+ ION_SESSION_DIR，绝不读写真实 ~/.ion；
//! LLM 走进程内脚本化 mock OpenAI SSE server（不调真实 LLM）；
//! 进程清理：只 kill 测试自己 spawn 的 worker 子进程（Drop 里 kill + wait）。

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const READY_TIMEOUT: Duration = Duration::from_secs(60);
const STEP_TIMEOUT: Duration = Duration::from_secs(30);

fn find_worker_bin() -> String {
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

fn rand_hex() -> String {
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

// ============================================================================
// 脚本化 mock OpenAI SSE server（按请求 body 内容路由响应）
// ============================================================================

#[derive(Clone)]
struct MockState {
    requests: Arc<AtomicUsize>,
    bodies: Arc<Mutex<Vec<String>>>,
    /// 首个"首轮"请求是否已发放 bash 工具调用（防旁路调用重复拿工具轮）
    tool_given: Arc<std::sync::atomic::AtomicBool>,
}

/// 起 mock server。响应脚本（按 body 内容路由，全部真实 SSE）：
/// - body 含 M2ASKMARKER_OUT_42（allow 后的工具结果）→ 文本 ALLOW_CASE_DONE
/// - body 含 "用户拒绝"（deny 后的错误 ToolResult）→ 文本 DENY_CASE_DONE
/// - 其余首个请求 → bash 工具调用（命令含 M2ASKMARKER 触发 CommandGuard 中危
///   → resolve_ask → stdout 上报 Ask）；按 prompt 关键词选 allow/deny 变体
/// - 后续旁路调用 → 通用文本
fn spawn_mock_openai_server() -> (u16, MockState) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
    let port = listener.local_addr().unwrap().port();
    let state = MockState {
        requests: Arc::new(AtomicUsize::new(0)),
        bodies: Arc::new(Mutex::new(Vec::new())),
        tool_given: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    let st = state.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let st = st.clone();
            std::thread::spawn(move || handle_llm_request(stream, st));
        }
    });
    (port, state)
}

fn handle_llm_request(mut stream: TcpStream, st: MockState) {
    // 逐字节读到 \r\n\r\n（请求头结束）
    let mut header = Vec::new();
    let mut one = [0u8; 1];
    loop {
        match std::io::Read::read(&mut stream, &mut one) {
            Ok(0) => return,
            Ok(_) => {
                header.push(one[0]);
                if header.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => return,
        }
    }
    let header_str = String::from_utf8_lossy(&header);
    let content_length: usize = header_str
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            if k.trim().eq_ignore_ascii_case("content-length") {
                v.trim().parse().ok()
            } else {
                None
            }
        })
        .unwrap_or(0);
    let mut body = vec![0u8; content_length];
    if content_length > 0 && std::io::Read::read_exact(&mut stream, &mut body).is_err() {
        return;
    }
    let body_str = String::from_utf8_lossy(&body).to_string();
    st.bodies.lock().unwrap().push(body_str.clone());
    st.requests.fetch_add(1, Ordering::SeqCst);

    let sse_body: Vec<u8> = if body_str.contains("M2ASKMARKER_OUT_42") {
        // allow 后的第 2 轮：工具结果已回传
        sse_text_response("ALLOW_CASE_DONE").into_bytes()
    } else if body_str.contains("用户拒绝") {
        // deny 后的第 2 轮：错误 ToolResult 已回传
        sse_text_response("DENY_CASE_DONE").into_bytes()
    } else if !st.tool_given.swap(true, Ordering::SeqCst) {
        // 首轮：bash 工具调用（命令必须含 M2ASKMARKER 才触发 CommandGuard Ask）
        let cmd = if body_str.contains("deny case") {
            "echo M2ASKMARKER_DENY"
        } else {
            "echo M2ASKMARKER_OUT_42"
        };
        sse_tool_response("bash", &serde_json::json!({"command": cmd})).into_bytes()
    } else {
        sse_text_response("ALLOW_CASE_DONE").into_bytes()
    };
    // 完整原始 HTTP 响应（带 Content-Length + Connection: close——
    // 只写 SSE body 不写头会被 hyper 当畸形响应丢弃，表现为请求错误）
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        sse_body.len()
    );
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.write_all(&sse_body);
    let _ = stream.flush();
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

/// OpenAI chat/completions SSE：纯文本响应
fn sse_text_response(text: &str) -> String {
    let payload = serde_json::json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": {"role": "assistant", "content": text},
            "finish_reason": null,
        }],
    });
    let done = serde_json::json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": {},
            "finish_reason": "stop",
        }],
    });
    format!("data: {payload}\n\ndata: {done}\n\ndata: [DONE]\n\n")
}

/// OpenAI chat/completions SSE：工具调用响应
fn sse_tool_response(name: &str, input: &serde_json::Value) -> String {
    let args_str = serde_json::to_string(input).unwrap_or_default();
    let call_id = format!("call_m2{}", std::process::id());
    let first = serde_json::json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": {"role": "assistant", "tool_calls": [{
                "index": 0,
                "id": call_id,
                "type": "function",
                "function": {"name": name, "arguments": args_str},
            }]},
            "finish_reason": null,
        }],
    });
    let done = serde_json::json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": {},
            "finish_reason": "tool_calls",
        }],
    });
    format!("data: {first}\n\ndata: {done}\n\ndata: [DONE]\n\n")
}

// ============================================================================
// Worker 进程封装（JSONL over stdin/stdout）
// ============================================================================

struct TestWorker {
    stdin: std::process::ChildStdin,
    lines_rx: std::sync::mpsc::Receiver<serde_json::Value>,
    child: std::process::Child,
    stderr_path: std::path::PathBuf,
    /// 收集到的全部事件（供断言事后检索）
    seen_events: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl Drop for TestWorker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl TestWorker {
    fn spawn(home: &std::path::Path, session: &str) -> Self {
        let stderr_path = home.join("worker.stderr.log");
        let stderr_file = std::fs::File::create(&stderr_path).expect("create stderr log");
        let mut child = Command::new(find_worker_bin())
            .arg("--mode")
            .arg("rpc")
            .arg("--session")
            .arg(session)
            .arg("--provider")
            .arg("mockllm")
            .arg("--model")
            .arg("mock-model")
            .current_dir(home)
            .env("HOME", home)
            .env("ION_SESSION_DIR", home.join("sessions"))
            .env("RUST_LOG", "info")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(stderr_file))
            .spawn()
            .expect("failed to spawn ion worker");

        let stdin = child.stdin.take().expect("no stdin");
        let stdout = child.stdout.take().expect("no stdout");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_writer = seen.clone();
        let (tx, rx) = std::sync::mpsc::channel::<serde_json::Value>();
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                    continue;
                };
                seen_writer.lock().unwrap().push(v.clone());
                if tx.send(v).is_err() {
                    break;
                }
            }
        });

        let mut worker = Self {
            stdin,
            lines_rx: rx,
            child,
            stderr_path,
            seen_events: seen,
        };
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            let v = worker.recv_until(deadline, "worker ready");
            if v.get("type").and_then(|t| t.as_str()) == Some("ready") {
                break;
            }
        }
        worker
    }

    fn recv_until(&mut self, deadline: Instant, what: &str) -> serde_json::Value {
        let remain = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or(Duration::from_millis(1));
        match self.lines_rx.recv_timeout(remain) {
            Ok(v) => v,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                self.dump_stderr();
                panic!("timeout ({remain:?}) waiting for: {what}")
            }
            Err(_) => {
                self.dump_stderr();
                panic!("worker stdout closed while waiting for: {what}")
            }
        }
    }

    fn dump_stderr(&self) {
        if let Ok(raw) = std::fs::read_to_string(&self.stderr_path) {
            let tail: String = raw.lines().rev().take(40).collect::<Vec<_>>().join("\n");
            eprintln!("===== worker stderr tail =====\n{tail}\n==============================");
        }
    }

    fn send(&mut self, id: &str, method: &str, params: serde_json::Value) {
        let cmd = serde_json::json!({"id": id, "method": method, "params": params});
        writeln!(self.stdin, "{}", cmd)
            .unwrap_or_else(|e| panic!("write {method} to worker stdin: {e}"));
        self.stdin
            .flush()
            .unwrap_or_else(|e| panic!("flush worker stdin after {method}: {e}"));
    }

    /// 等指定 method 的 response 帧并返回 data/整个对象
    fn wait_response(&mut self, id: &str, what: &str) -> serde_json::Value {
        let deadline = Instant::now() + STEP_TIMEOUT;
        loop {
            let v = self.recv_until(deadline, &format!("response for {what}"));
            if v.get("type").and_then(|t| t.as_str()) == Some("response")
                && v.get("id").and_then(|i| i.as_str()) == Some(id)
            {
                return v;
            }
        }
    }

    /// 等下一个 customType 匹配的 extension_event，返回其 data。
    /// worker 侧 extension_event 是包裹形 {"type":"event","event":{...}}。
    fn wait_extension_event(&mut self, custom_type: &str, what: &str) -> serde_json::Value {
        let deadline = Instant::now() + STEP_TIMEOUT;
        loop {
            let v = self.recv_until(deadline, &format!("extension_event {what}"));
            let inner = v.get("event").cloned().unwrap_or(v.clone());
            if inner.get("type").and_then(|t| t.as_str()) == Some("extension_event")
                && inner.get("customType").and_then(|c| c.as_str()) == Some(custom_type)
            {
                return inner.get("data").cloned().unwrap_or_default();
            }
        }
    }

    /// 等 agent_end，返回本轮拼接的 assistant 文本（text_delta 聚合）
    fn wait_agent_end(&mut self, what: &str) -> String {
        let deadline = Instant::now() + STEP_TIMEOUT;
        let mut text = String::new();
        loop {
            let v = self.recv_until(deadline, &format!("agent_end for {what}"));
            match v.get("type").and_then(|t| t.as_str()) {
                Some("event") => {
                    let ev = v.get("event").cloned().unwrap_or_default();
                    match ev.get("type").and_then(|t| t.as_str()) {
                        Some("text_delta") => {
                            if let Some(d) = ev.get("delta").and_then(|d| d.as_str()) {
                                text.push_str(d);
                            }
                        }
                        Some("agent_end") => return text,
                        _ => {}
                    }
                }
                Some("agent_end") => return text,
                _ => {}
            }
        }
    }
}

fn make_isolated_env(port: u16) -> std::path::PathBuf {
    let tmp = std::path::PathBuf::from("/tmp").join(format!(
        "ion_ask_host_{}_{}",
        std::process::id(),
        rand_hex()
    ));
    let ion_dir = tmp.join(".ion");
    std::fs::create_dir_all(&ion_dir).expect("create isolated .ion dir");
    std::fs::create_dir_all(tmp.join("sessions")).expect("create isolated sessions dir");

    let config = format!(
        r#"{{
  "default_provider": "mockllm",
  "default_model": "mock-model",
  "providers": {{
    "mockllm": {{
      "name": "mockllm",
      "api": "openai-completions",
      "base_url": "http://127.0.0.1:{port}/v4",
      "api_key": "harness-key-not-real",
      "models": [
        {{"id": "mock-model", "name": "Mock", "context_window": 128000}}
      ]
    }}
  }},
  "runtime": {{
    "command_guard": {{
      "mode": "blacklist",
      "whitelist": [],
      "risk_patterns": [
        {{"pattern": "M2ASKMARKER", "level": "medium", "message": "M2 harness ask trigger"}}
      ]
    }}
  }}
}}"#
    );
    std::fs::write(ion_dir.join("config.json"), config).expect("write isolated config");
    tmp
}

// ============================================================================
// 用例 1：allow 全链路（Ask 上报 → ask_respond(allow) → 工具真执行 → AskResolved）
// ============================================================================

#[test]
fn ask_allow_releases_tool_execution() {
    let (port, llm) = spawn_mock_openai_server();
    let tmp = make_isolated_env(port);

    let session = format!("ask_allow_{}", rand_hex());
    let mut worker = TestWorker::spawn(&tmp, &session);

    worker.send("p1", "prompt", serde_json::json!({"text": "run the marked command"}));
    worker.wait_agent_start_or_event("run 1 agent_start");
    // （agent_start 可能已被 wait_extension_event 消费前到达——用宽容的等待）

    // ── 核心 1：Ask 事件必须从 worker stdout 上报（带 request_id）──
    let ask = worker.wait_extension_event("Ask", "Ask (M2 source-1 report)");
    let request_id = ask
        .get("request_id")
        .and_then(|v| v.as_str())
        .expect("Ask event must carry request_id")
        .to_string();
    assert!(
        !request_id.is_empty(),
            "Ask event request_id must be non-empty, got: {ask}"
    );
    assert!(
        ask.get("title").is_some() && ask.get("message").is_some(),
        "Ask event should carry title/message for UI rendering: {ask}"
    );

    // ── 核心 2：ask_respond(allow) → 工具放行 → 命令真执行 ──
    worker.send(
        "r1",
        "ask_respond",
        serde_json::json!({"request_id": request_id, "response": "allow"}),
    );
    let resp = worker.wait_response("r1", "ask_respond allow");
    assert!(
        resp.get("success").and_then(|s| s.as_bool()).unwrap_or(false)
            || resp.get("data").map(|d| d.get("delivered").and_then(|v| v.as_bool()).unwrap_or(false)).unwrap_or(false),
        "ask_respond should report delivered, got: {resp}"
    );

    // run 1 完成：最终文本是第 2 轮 LLM 给的（说明工具执行完并回传了）
    let text = worker.wait_agent_end("run 1 after allow");
    assert!(
        text.contains("ALLOW_CASE_DONE"),
        "run 1 should finish with ALLOW_CASE_DONE (tool result fed round 2), got: {text:?}"
    );

    // ── 核心 3：AskResolved 事件上报（host 消除总线条目的依据）──
    // AskResolved 在 allow 后立即发射（早于 agent_end）——wait_agent_end 可能已把它
    // 消费掉，所以从全程留痕的 seen_events 里检索（等一小会儿容忍时序）。
    let deadline = Instant::now() + STEP_TIMEOUT;
    let resolved_data = loop {
        let hit = worker.seen_events.lock().unwrap().iter().find_map(|v| {
            let inner = v.get("event").cloned().unwrap_or(v.clone());
            if inner.get("type").and_then(|t| t.as_str()) == Some("extension_event")
                && inner.get("customType").and_then(|c| c.as_str()) == Some("AskResolved")
            {
                Some(inner.get("data").cloned().unwrap_or_default())
            } else {
                None
            }
        });
        if let Some(d) = hit {
            break d;
        }
        assert!(
            Instant::now() < deadline,
            "AskResolved extension_event (request_id={request_id}) should be reported to stdout"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(
        resolved_data.get("request_id").and_then(|v| v.as_str()),
        Some(request_id.as_str())
    );

    // 工具真跑了：bash 输出 M2ASKMARKER_OUT_42 回传给了第 2 轮 LLM
    let bodies = llm.bodies.lock().unwrap().clone();
    assert!(
        bodies.iter().any(|b| b.contains("M2ASKMARKER_OUT_42")),
        "round-2 LLM request body should contain the executed bash output M2ASKMARKER_OUT_42"
    );

    drop(worker);
    let _ = std::fs::remove_dir_all(&tmp);
}

// ============================================================================
// 用例 2：deny 路径（ask_respond(deny) → 工具被拒 → 错误 ToolResult 回传）
// ============================================================================

#[test]
fn ask_deny_blocks_tool_execution() {
    let (port, llm) = spawn_mock_openai_server();
    let tmp = make_isolated_env(port);

    let session = format!("ask_deny_{}", rand_hex());
    let mut worker = TestWorker::spawn(&tmp, &session);

    worker.send("p1", "prompt", serde_json::json!({"text": "deny case: run the marked command"}));

    let ask = worker.wait_extension_event("Ask", "Ask (deny case)");
    let request_id = ask
        .get("request_id")
        .and_then(|v| v.as_str())
        .expect("request_id")
        .to_string();

    worker.send(
        "r1",
        "ask_respond",
        serde_json::json!({"request_id": request_id, "response": "deny"}),
    );
    worker.wait_response("r1", "ask_respond deny");

    let text = worker.wait_agent_end("run 1 after deny");
    assert!(
        text.contains("DENY_CASE_DONE"),
        "run 1 should still finish (deny converts to error ToolResult), got: {text:?}"
    );

    // 拒绝语义：第 2 轮 LLM body 里应有"用户拒绝"错误 ToolResult
    let deadline = Instant::now() + STEP_TIMEOUT;
    loop {
        let bodies = llm.bodies.lock().unwrap().clone();
        if bodies.iter().any(|b| b.contains("用户拒绝")) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "round-2 LLM body should contain 用户拒绝 error ToolResult"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    drop(worker);
    let _ = std::fs::remove_dir_all(&tmp);
}

// ============================================================================
// 用例 3：ask_respond 未知 id → "request not found or already expired"
// ============================================================================

#[test]
fn ask_respond_unknown_id_reports_not_found() {
    let (port, _llm) = spawn_mock_openai_server();
    let tmp = make_isolated_env(port);

    let session = format!("ask_nf_{}", rand_hex());
    let mut worker = TestWorker::spawn(&tmp, &session);

    worker.send(
        "r1",
        "ask_respond",
        serde_json::json!({"request_id": "req_does_not_exist", "response": "allow"}),
    );
    let resp = worker.wait_response("r1", "ask_respond unknown id");
    let err = resp
        .get("data")
        .and_then(|d| d.get("error"))
        .and_then(|e| e.as_str())
        .unwrap_or("");
    assert!(
        err.contains("request not found or already expired"),
        "unknown ask id should return the host-aligned error, got: {resp}"
    );

    drop(worker);
    let _ = std::fs::remove_dir_all(&tmp);
}

impl TestWorker {
    /// 等 agent_start（宽容：事件帧形状两种都认）
    fn wait_agent_start_or_event(&mut self, what: &str) {
        let deadline = Instant::now() + STEP_TIMEOUT;
        loop {
            let v = self.recv_until(deadline, what);
            if v.get("type").and_then(|t| t.as_str()) == Some("event") {
                let ev = v.get("event").cloned().unwrap_or_default();
                if ev.get("type").and_then(|t| t.as_str()) == Some("agent_start") {
                    return;
                }
            }
        }
    }
}
