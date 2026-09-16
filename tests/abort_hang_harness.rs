//! Abort-hang worker harness（P0：abort 后同 worker 下一轮 run 永久挂起）。
//!
//! 背景（J7 e2e T5 真实环境复现 2 次）：
//!   worker 正在跑（LLM 轮中 bash 工具执行）→ `abort` RPC → abort 生效
//!   （工具结果 "Error: Agent aborted"、agent_end、worker 同 PID 存活）→
//!   之后新 prompt → agent_start 出现但无任何 LLM 事件、无 agent_end（僵尸 run），
//!   后续 prompt 全被当 steer 吞进僵尸 run，≥300s 不恢复。
//!
//! 为什么 FauxProvider 测不出：FauxProvider 不走真实 HTTP/SSE 取消路径。
//! 本 harness 走真实进程边界 + 真实 openai-completions HTTP 流：
//!   1. 进程内起**脚本化** mock OpenAI SSE server（按请求序号回不同补全）：
//!      - 请求 #1 → 带 bash 工具调用（sleep 60，长时间执行给 abort 留窗口）
//!      - 请求 #2 → 纯文本 SECOND_RUN_OK
//!      - 请求 #3+ → 纯文本 THIRD_RUN_OK...
//!   2. 私有 HOME + ION_SESSION_DIR 隔离，spawn `ion --mode rpc` 子进程，
//!      config.json 指向 mock server；command_guard=open（无人审批）。
//!   3. prompt#1 → 等 agent_start + server 收到请求#1（bash sleep 执行中）→
//!      发 abort → 等 agent_end（abort 生效）。
//!   4. prompt#2 → **断言**：限时内 mock server 收到第 2 个 LLM 请求，且
//!      stdout 出现 text_delta(SECOND_RUN_OK) + agent_end。
//!      僵尸 run 的特征恰好是：agent_start 有，但 server 永远等不到请求#2。
//!   5. prompt#3 → 同样限时完成（验证后续 prompt 不被吞，不再需要 kill 重建）。
//!
//! 隔离：私有 HOME（临时目录），绝不读写真实 ~/.ion；
//! 进程清理：只 kill 测试自己 spawn 的 worker 子进程（Drop 里 kill + wait）。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const READY_TIMEOUT: Duration = Duration::from_secs(60);
const STEP_TIMEOUT: Duration = Duration::from_secs(30);
/// abort 后新 run 必须在这个窗口内打到 mock server（红态 = 永远打不到）。
const POST_ABORT_LLM_WINDOW: Duration = Duration::from_secs(30);

fn find_worker_bin() -> String {
    std::env::var("ION_WORKER_BIN").unwrap_or_else(|_| {
        let current_exe = std::env::current_exe().ok();
        if let Some(exe) = current_exe {
            if let Some(parent) = exe.parent() {
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
// 脚本化 mock OpenAI SSE server
// ============================================================================

#[derive(Clone)]
struct MockState {
    /// 已收到的 /chat/completions 请求数（LLM 轮次序号）
    requests: Arc<AtomicUsize>,
    /// 每个请求的到达时间（ms since test start）
    arrivals: Arc<Mutex<Vec<u128>>>,
    /// 每个请求的 body（用于内容匹配断言）
    bodies: Arc<Mutex<Vec<String>>>,
    started_at: Instant,
}

impl MockState {
    fn count(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

/// 起 mock server。返回 (端口, 状态)。
/// 响应脚本按请求内容（不是序号——auto-session-title 等旁路 LLM 调用也会打进来）：
/// - 请求体含 "second prompt after abort" → SECOND_RUN_OK
/// - 请求体含 "third prompt" → THIRD_RUN_OK
/// - 其它（首跑 + 旁路调用）中**首个** → bash(sleep 60) 工具调用；后续 → 通用文本。
/// 全部走真实 SSE（data: 行 + [DONE]）。
fn spawn_mock_openai_server() -> (u16, MockState) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
    let port = listener.local_addr().unwrap().port();
    let state = MockState {
        requests: Arc::new(AtomicUsize::new(0)),
        arrivals: Arc::new(Mutex::new(Vec::new())),
        bodies: Arc::new(Mutex::new(Vec::new())),
        started_at: Instant::now(),
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

/// 读完整 HTTP 请求（头 + Content-Length body），按序号回 SSE 脚本响应。
fn handle_llm_request(mut stream: TcpStream, st: MockState) {
    // 逐字节读到 \r\n\r\n（请求头结束）——不按 4KB 快照读，避免把 body 读丢一半
    let mut buf: Vec<u8> = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                buf.push(byte[0]);
                if buf.len() >= 4 && &buf[buf.len() - 4..] == b"\r\n\r\n" {
                    break;
                }
            }
        }
    }
    let headers = String::from_utf8_lossy(&buf).to_string();
    let content_length: usize = headers
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
    // 读完 body（reqwest 等完整响应；读干净避免 RST 截断）
    let mut body_remaining = content_length;
    let mut body_buf = vec![0u8; content_length.min(1 << 20)];
    while body_remaining > 0 {
        let cap = body_buf.len();
        let n = match stream.read(&mut body_buf[..body_remaining.min(cap)]) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        body_remaining -= n;
    }

    // 记账：这是第几个 LLM 请求 + 请求体（供脚本判定与断言匹配）
    let n = st.requests.fetch_add(1, Ordering::SeqCst) + 1;
    st.arrivals
        .lock()
        .unwrap()
        .push(st.started_at.elapsed().as_millis());
    st.bodies
        .lock()
        .unwrap()
        .push(String::from_utf8_lossy(&body_buf).to_string());

    // 按请求内容选脚本（auto-session-title 等旁路调用不按序号走）。
    // ⚠️ 请求体带完整对话历史——run 3 的 body 同时含 "second prompt..." 和
    // "third prompt"，必须按**最后出现位置**判定（新 prompt 追加在尾部）。
    let body_str = String::from_utf8_lossy(&body_buf).to_string();
    let pos_second = body_str.rfind("second prompt after abort");
    let pos_third = body_str.rfind("third prompt");
    let sse_body: Vec<u8> = if pos_third > pos_second {
        sse_plain_text("THIRD_RUN_OK")
    } else if pos_second.is_some() {
        sse_plain_text("SECOND_RUN_OK")
    } else if n == 1 {
        sse_tool_call_bash_sleep()
    } else {
        // 旁路调用（标题生成等）——给个短文本，别让它跑工具
        sse_plain_text("SIDECALL_OK")
    };

    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        sse_body.len()
    );
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.write_all(&sse_body);
    let _ = stream.flush();
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

fn sse_body_from(events: Vec<serde_json::Value>) -> Vec<u8> {
    let mut body = String::new();
    for e in events {
        body.push_str("data: ");
        body.push_str(&serde_json::to_string(&e).unwrap());
        body.push_str("\n\n");
    }
    body.push_str("data: [DONE]\n\n");
    body.into_bytes()
}

/// 请求 #1 的脚本：bash 工具调用 sleep 60（timeout 60s 前台，abort 窗口充足）。
fn sse_tool_call_bash_sleep() -> Vec<u8> {
    let args = serde_json::json!({
        "command": "sleep 60",
        "description": "long sleep so the harness can abort mid-tool",
        "timeout": 60
    });
    let tool_call_chunk = serde_json::json!({
        "choices": [{
            "index": 0,
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": "call_sleep_1",
                    "type": "function",
                    "function": {
                        "name": "bash",
                        "arguments": serde_json::to_string(&args).unwrap()
                    }
                }]
            },
            "finish_reason": null
        }]
    });
    let finish_chunk = serde_json::json!({
        "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]
    });
    sse_body_from(vec![tool_call_chunk, finish_chunk])
}

/// 请求 #2/#3 的脚本：纯文本补全（finish_reason=stop，单 turn 收尾）。
fn sse_plain_text(text: &str) -> Vec<u8> {
    let text_chunk = serde_json::json!({
        "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": null}]
    });
    let finish_chunk = serde_json::json!({
        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
    });
    sse_body_from(vec![text_chunk, finish_chunk])
}

// ============================================================================
// Worker 子进程 + 限时 stdout 读取
// ============================================================================

struct TestWorker {
    stdin: std::process::ChildStdin,
    lines_rx: std::sync::mpsc::Receiver<serde_json::Value>,
    child: std::process::Child,
    /// worker stderr 落盘（tracing [abort] 日志在 stderr）——失败时打印辅助定位
    stderr_path: std::path::PathBuf,
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
            // tracing 走 stderr（EnvFilter 默认 warn 看不到 [abort] info 日志）
            .env("RUST_LOG", "info")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(stderr_file))
            .spawn()
            .expect("failed to spawn ion worker");

        let stdin = child.stdin.take().expect("no stdin");
        let stdout = child.stdout.take().expect("no stdout");
        // 读线程：stdout 行 → mpsc；进程退出时发 EOF 标记（lines_rx 关闭）
        let (tx, rx) = std::sync::mpsc::channel::<serde_json::Value>();
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                    continue;
                };
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
        };
        // 等 ready 帧（限时，绝不无限阻塞）
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            let v = worker.recv_until(deadline, "worker ready");
            if v.get("type").and_then(|t| t.as_str()) == Some("ready") {
                break;
            }
        }
        worker
    }

    /// 限时读一行 JSON（超时/断流 panic 带上下文——测试必须快速失败，不能挂）。
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

    /// 失败诊断：打印 worker stderr 尾部（tracing [abort] 等日志）。
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

    /// 等 method 的 response 帧，返回 data。
    fn wait_response(&mut self, id: &str, method: &str) -> serde_json::Value {
        let deadline = Instant::now() + STEP_TIMEOUT;
        loop {
            let v = self.recv_until(deadline, &format!("response of {method}"));
            if v.get("type").and_then(|t| t.as_str()) == Some("response")
                && v.get("id").and_then(|i| i.as_str()) == Some(id)
            {
                assert!(
                    v.get("success").and_then(|s| s.as_bool()).unwrap_or(false),
                    "{method} response not success: {v}"
                );
                return v.get("data").cloned().unwrap_or(serde_json::Value::Null);
            }
        }
    }

    /// 从当前流位置等到下一个 agent_end（顺带收集期间 text_delta 文本）。
    fn wait_agent_end(&mut self, what: &str) -> String {
        let deadline = Instant::now() + STEP_TIMEOUT;
        let mut text = String::new();
        loop {
            let v = self.recv_until(deadline, &format!("agent_end ({what})"));
            if v.get("type").and_then(|t| t.as_str()) != Some("event") {
                continue;
            }
            let ev = &v["event"];
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
    }

    /// 等下一个 agent_start（丢弃之前的非目标帧）。
    fn wait_agent_start(&mut self, what: &str) {
        let deadline = Instant::now() + STEP_TIMEOUT;
        loop {
            let v = self.recv_until(deadline, &format!("agent_start ({what})"));
            if v.get("type").and_then(|t| t.as_str()) == Some("event")
                && v["event"].get("type").and_then(|t| t.as_str()) == Some("agent_start")
            {
                return;
            }
        }
    }
}

/// 限时轮询 mock server 请求数达到 want。
fn wait_llm_request_count(state: &MockState, want: usize, window: Duration) {
    let deadline = Instant::now() + window;
    while Instant::now() < deadline {
        if state.count() >= want {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!(
        "mock LLM server 只收到 {}/{} 个请求",
        state.count(),
        want
    );
}
/// 限时轮询：等一个 body 包含 marker 的 LLM 请求到达（内容匹配，不受旁路 LLM 调用干扰）。
fn wait_llm_request_with(state: &MockState, marker: &str, window: Duration) {
    let deadline = Instant::now() + window;
    while Instant::now() < deadline {
        if state
            .bodies
            .lock()
            .unwrap()
            .iter()
            .any(|b| b.contains(marker))
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!(
        "mock LLM server 在 {window:?} 内没收到 body 含 {marker:?} 的请求 \
         （僵尸 run：新 prompt 的 LLM 请求从未发出；总请求数 {}）",
        state.count()
    );
}

// ============================================================================
// 主测试
// ============================================================================

#[test]
fn next_run_after_abort_must_not_hang() {
    let (port, llm) = spawn_mock_openai_server();

    // macOS sun_path 纪律：路径用短前缀
    let tmp = std::path::PathBuf::from("/tmp").join(format!(
        "ion_abort_hang_{}_{}",
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
    "command_guard": {{"mode": "open", "whitelist": [], "risk_patterns": []}}
  }}
}}"#
    );
    std::fs::write(ion_dir.join("config.json"), config).expect("write isolated config");

    let session = format!("abort_hang_{}", rand_hex());
    let mut worker = TestWorker::spawn(&tmp, &session);

    // ── 第 1 轮：prompt → LLM 请求#1（bash sleep 60 脚本）→ 工具执行中 abort ──
    worker.send("p1", "prompt", serde_json::json!({"text": "run the long sleep"}));
    worker.wait_agent_start("run 1");
    wait_llm_request_count(&llm, 1, STEP_TIMEOUT);
    // 给工具执行留出进入窗口（SSE 响应是即时的，sleep 60 已在跑）
    std::thread::sleep(Duration::from_millis(1500));

    // abort：必须生效——run 1 以 agent_end 收尾（工具被 stopped 轮询打断）
    worker.send("a1", "abort", serde_json::Value::Null);
    worker.wait_response("a1", "abort");
    worker.wait_agent_end("run 1 aborted");

    // ── 第 2 轮（红态在这里暴露）：新 prompt 必须真的发出 LLM 请求并完成 ──
    worker.send("p2", "prompt", serde_json::json!({"text": "second prompt after abort"}));
    worker.wait_agent_start("run 2 (zombie candidate)");
    // 核心断言：限时内 run 2 自己的 LLM 请求（body 含 second prompt）到达 mock server。
    // 僵尸 run 特征 = agent_start 出现但该请求永远不发（红态在这里 panic）。
    wait_llm_request_with(&llm, "second prompt after abort", POST_ABORT_LLM_WINDOW);
    // 且新 run 正常收尾，assistant 文本可见
    let text2 = worker.wait_agent_end("run 2");
    assert!(
        text2.contains("SECOND_RUN_OK"),
        "run 2 应收到 SECOND_RUN_OK 文本，实际: {text2:?}"
    );

    // ── 第 3 轮（回归护栏）：后续 prompt 不被当 steer 吞进上一轮 ──
    worker.send("p3", "prompt", serde_json::json!({"text": "third prompt"}));
    wait_llm_request_with(&llm, "third prompt", STEP_TIMEOUT);
    let text3 = worker.wait_agent_end("run 3");
    assert!(
        text3.contains("THIRD_RUN_OK"),
        "run 3 应收到 THIRD_RUN_OK 文本，实际: {text3:?}"
    );

    drop(worker);
    let _ = std::fs::remove_dir_all(&tmp);
}
