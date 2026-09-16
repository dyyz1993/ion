//! ModelFallback worker stdout 发射 harness（修复批 4 / H4 遗留补测）。
//!
//! 背景：fix4 加的模型自动降级（永久错误 → tier_models 候选档）此前只在
//! Extension trait 层有测试（tests/tier_fallback_harness.rs，进程内捕获），
//! worker stdout 实际发射的 ModelFallback 事件帧没有断言
//!（"output() 写 stdout 无法进程内断言"）。
//!
//! 本 harness 走真实进程边界：
//!   1. 进程内起一个只回 401 的本地 HTTP server（无需外网/真实 key）；
//!   2. 私有 HOME + ION_SESSION_DIR 隔离，spawn `ion --mode rpc` 子进程，
//!      config.json 指向该 server（openai-completions 协议），tier_models
//!      配一个不同档的 backup 模型（同样打 401 server）；
//!   3. 发 prompt → 永久错误 → agent 降级 → worker stdout 事件流出现
//!      ModelFallback 帧；
//!   4. 断言帧形状（type/extension/customType/visibility + data.from/to/reason）
//!      以及 SessionIndex.set_model 副作用（model 已切成 backup）。
//!
//! 隔离：私有 HOME（mktemp 风格临时目录），绝不读写真实 ~/.ion；
//! 进程清理：只 kill 测试自己 spawn 的 worker 子进程（Drop 里 kill + wait）。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const READY_TIMEOUT: Duration = Duration::from_secs(60);
const RUN_TIMEOUT: Duration = Duration::from_secs(90);

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

/// 对任何请求都回 401 的本地 server（openai-completions 协议只需拿到非 2xx）。
/// 返回端口；accept 线程随测试进程退出（测试进程结束即回收，无需显式停）。
fn spawn_always_401_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || handle_401(stream));
        }
    });
    port
}

fn handle_401(mut stream: TcpStream) {
    // 读掉请求头（读到缓冲即够——测试请求很小），然后回 401 + 立即关闭
    let mut buf = [0u8; 4096];
    let _ = stream.read(&mut buf);
    let body = r#"{"error":{"message":"401 Unauthorized (harness fake server)"}}"#;
    let resp = format!(
        "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.flush();
}

struct TestWorker {
    stdin: std::process::ChildStdin,
    reader: BufReader<std::process::ChildStdout>,
    child: std::process::Child,
}

impl Drop for TestWorker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl TestWorker {
    fn spawn(home: &std::path::Path, session: &str) -> Self {
        let mut child = Command::new(find_worker_bin())
            .arg("--mode")
            .arg("rpc")
            .arg("--session")
            .arg(session)
            .arg("--provider")
            .arg("fail401")
            .arg("--model")
            .arg("primary-model")
            .current_dir(home)
            .env("HOME", home)
            .env("ION_SESSION_DIR", home.join("sessions"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn ion worker");

        let stdin = child.stdin.take().expect("no stdin");
        let reader = BufReader::new(child.stdout.take().expect("no stdout"));
        let mut worker = Self {
            stdin,
            reader,
            child,
        };

        // 等 ready 帧
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            assert!(Instant::now() < deadline, "timeout waiting worker ready");
            let mut line = String::new();
            let n = worker
                .reader
                .read_line(&mut line)
                .expect("read worker stdout");
            assert!(n > 0, "worker closed stdout before ready");
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) {
                if v.get("type").and_then(|t| t.as_str()) == Some("ready") {
                    break;
                }
            }
        }
        worker
    }

    /// 发 prompt 并一路读 stdout：收集途中出现的 ModelFallback 事件帧，
    /// 直到 agent_end（run 真正结束）。⚠️ prompt 的 response 帧是**立刻**返回的
    /// （agent 异步跑），不能当作读停止条件——实测 ModelFallback 在 response
    /// 之后、agent_end 之前才出现。
    fn prompt_and_collect(&mut self, text: &str) -> Vec<serde_json::Value> {
        let id = "fb_prompt_1";
        let cmd = serde_json::json!({
            "id": id,
            "method": "prompt",
            "params": {"text": text}
        });
        writeln!(self.stdin, "{}", cmd).expect("write prompt to worker stdin");
        self.stdin.flush().expect("flush worker stdin");

        let mut fallbacks = Vec::new();
        let deadline = Instant::now() + RUN_TIMEOUT;
        loop {
            assert!(Instant::now() < deadline, "timeout waiting agent_end");
            let mut line = String::new();
            let n = self.reader.read_line(&mut line).expect("read stdout line");
            if n == 0 {
                // worker 关闭 stdout（异常退出）——返回已收集的帧让断言给出清晰信息
                return fallbacks;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                continue;
            };
            if v.get("type").and_then(|t| t.as_str()) == Some("event") {
                let event = &v["event"];
                match event.get("type").and_then(|t| t.as_str()) {
                    // 收集 ModelFallback 事件帧
                    _ if event.get("customType").and_then(|c| c.as_str())
                        == Some("ModelFallback") =>
                    {
                        fallbacks.push(event.clone());
                    }
                    // run 终点：agent_end（ModelFallback 恒在其之前发射）
                    Some("agent_end") => return fallbacks,
                    _ => {}
                }
            }
        }
    }
}

#[test]
fn model_fallback_event_is_emitted_on_worker_stdout() {
    let port = spawn_always_401_server();

    let tmp = std::env::temp_dir().join(format!("ion_fb_stdout_{}", rand_hex()));
    let ion_dir = tmp.join(".ion");
    std::fs::create_dir_all(&ion_dir).expect("create isolated .ion dir");
    std::fs::create_dir_all(tmp.join("sessions")).expect("create isolated sessions dir");

    // 主模型与降级候选都指向 401 server：
    // primary 401（永久）→ 降级 backup → backup 也 401 → 无更多档，run 报错收尾。
    // 我们只关心降级瞬间的 ModelFallback 事件帧。
    let config = format!(
        r#"{{
  "default_provider": "fail401",
  "default_model": "primary-model",
  "tier_models": {{"fast": "fail401/backup-model"}},
  "providers": {{
    "fail401": {{
      "name": "fail401",
      "api": "openai-completions",
      "base_url": "http://127.0.0.1:{port}/v4",
      "api_key": "harness-key-not-real",
      "models": [
        {{"id": "primary-model", "name": "Primary"}},
        {{"id": "backup-model", "name": "Backup"}}
      ]
    }}
  }}
}}"#
    );
    std::fs::write(ion_dir.join("config.json"), config).expect("write isolated config");

    let session = format!("fb_stdout_{}", rand_hex());
    let mut worker = TestWorker::spawn(&tmp, &session);

    let fallbacks = worker.prompt_and_collect("trigger permanent 401");

    // ── 帧形状断言（对齐 worker_rpc.rs on_model_fallback 的 output 形状）──
    assert_eq!(
        fallbacks.len(),
        1,
        "应恰好发射一次 ModelFallback 帧: {fallbacks:?}"
    );
    let event = &fallbacks[0];
    assert_eq!(
        event["type"].as_str(),
        Some("extension_event"),
        "事件类型应为 extension_event"
    );
    assert_eq!(
        event["extension"].as_str(),
        Some("agent-core"),
        "发射方应为内核 agent-core"
    );
    assert_eq!(event["customType"].as_str(), Some("ModelFallback"));
    assert_eq!(event["visibility"].as_str(), Some("llm_and_ui"));
    assert_eq!(
        event["sessionId"].as_str(),
        Some(session.as_str()),
        "帧应带当前 sessionId"
    );
    assert!(
        event["timestamp"].as_u64().unwrap_or(0) > 0,
        "帧应带 timestamp"
    );
    // data 三元组：from/to/reason
    assert_eq!(
        event["data"]["from"].as_str(),
        Some("fail401/primary-model"),
        "from 应是触发降级的原模型"
    );
    assert_eq!(
        event["data"]["to"].as_str(),
        Some("fail401/backup-model"),
        "to 应是 tier 候选模型"
    );
    let reason = event["data"]["reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains("401"),
        "reason 应携带原始永久错误（含 401）: {reason}"
    );

    // ── 副作用断言：SessionIndex.set_model 已把索引里的当前模型切成 backup ──
    drop(worker); // 先收 worker（flush 由 worker 侧负责，这里只需等文件已写）
    let index_path = ion_dir.join("agent").join("sessions.index.json");
    let index_raw = std::fs::read_to_string(&index_path)
        .expect("sessions.index.json 应已由 set_model 写出");
    let index: serde_json::Value = serde_json::from_str(&index_raw).expect("index 是合法 JSON");
    let meta = &index["sessions"][&session];
    assert_eq!(
        meta["model"].as_str(),
        Some("backup-model"),
        "SessionIndex 应记录降级后的模型: {meta}"
    );
    assert_eq!(meta["provider"].as_str(), Some("fail401"));

    let _ = std::fs::remove_dir_all(&tmp);
}
