//! rpc_schema_exec_test.rs — 执行域（exec）RPC JSON Schema 契约一致性测试（G5 缺口批次）
//!
//! 两层校验：
//!   静态层：`schemas/rpc/exec/_index.json` 与 schema 文件一一对应；
//!           每个 schema 都是合法且可编译的 draft 2020-12；
//!           prompt schema 的关键契约位（images[mimeType]/origin/behavior/回执变体）独立断言。
//!   动态层：隔离起真实 worker（`ion --mode rpc`，假 HOME + 私有 ION_SESSION_DIR，
//!           不碰真实 ~/.ion），拿真实响应对着 schema 验证。其中 prompt 用
//!           ION_FAUX_SCRIPT 驱动真实 agent 轮（🔴 必须 ION_FAUX_REPEAT=1：
//!           静态脚本只排有限条响应，repeat 防 steer 消费轮把队列掏空后 run 挂死）。
//!
//! schema 约定：worker 级 schema 校验【完整响应信封】{id,type,command,success,data|error}；
//! host 级信封不带 command。请求参数在各 schema 的 $defs/requestParams。
//!
//! 用法：
//!   cargo build --bin ion
//!   cargo test --test rpc_schema_exec
//!
//! 🔴 不修改 src/ 下任何文件；辅助函数自包含在本文件内。

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use jsonschema::Validator;
use serde_json::Value;

const READY_TIMEOUT: Duration = Duration::from_secs(60);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Schema 目录（静态层）
// ---------------------------------------------------------------------------

fn schema_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("schemas/rpc/exec")
}

fn read_json(path: &Path) -> Value {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("failed to parse {}: {e}", path.display()))
}

fn compile_schema(path: &Path) -> Validator {
    let schema_value = read_json(path);
    jsonschema::validator_for(&schema_value)
        .unwrap_or_else(|e| panic!("schema {} failed to compile: {e}", path.display()))
}

#[test]
fn static_index_and_schema_files_match() {
    let dir = schema_dir();
    let index_path = dir.join("_index.json");
    assert!(index_path.exists(), "_index.json missing under {}", dir.display());
    let index = read_json(&index_path);

    let commands = index["commands"]
        .as_array()
        .expect("_index.json must have a commands array");

    let mut indexed: Vec<String> = Vec::new();
    for cmd in commands {
        let name = cmd["command"].as_str().expect("command name must be a string");
        let file = cmd["file"].as_str().expect("file must be a string");
        let path = dir.join(file);
        assert!(path.exists(), "index lists '{name}' but {} is missing", path.display());
        indexed.push(name.to_string());
    }

    let mut on_disk: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("read schema dir") {
        let entry = entry.expect("dir entry");
        let name = entry.file_name().to_string_lossy().to_string();
        if name == "_index.json" || !name.ends_with(".json") {
            continue;
        }
        on_disk.push(name.trim_end_matches(".json").to_string());
    }
    on_disk.sort();
    indexed.sort();
    assert_eq!(indexed, on_disk, "_index.json and schema files are out of sync");

    // 对账结论字段必须在位（G5 批次规格）
    assert!(
        index.get("reconciliation").is_some(),
        "_index.json must carry the reconciliation block"
    );
    let recon = &index["reconciliation"];
    assert_eq!(
        recon["remaining"].as_str(),
        Some("0 对外 RPC 命令未覆盖；永久 outOfScope 见 outOfScope 段"),
        "reconciliation.remaining must state zero uncovered external commands"
    );
    assert!(index.get("outOfScope").is_some(), "outOfScope block required");
}

#[test]
fn static_all_schemas_compile_as_draft_2020_12() {
    let dir = schema_dir();
    let mut count = 0;
    for entry in std::fs::read_dir(&dir).expect("read schema dir") {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let value = read_json(&path);
        assert_eq!(
            value["$schema"].as_str(),
            Some("https://json-schema.org/draft/2020-12/schema"),
            "{} must declare draft 2020-12",
            path.display()
        );
        assert!(value.get("$id").is_some(), "{} must carry an $id", path.display());
        assert!(value.get("title").is_some(), "{} must carry a title", path.display());
        compile_schema(&path); // panics on invalid schema
        count += 1;
    }
    assert!(count >= 43, "expected >=43 exec-domain schemas, found {count}");
}

#[test]
fn static_prompt_schema_shape_contract() {
    let dir = schema_dir();
    let schema = read_json(&dir.join("prompt.json"));

    // 请求面：images[].data + images[].mimeType（serde alias mimeType）必填
    let images = &schema["$defs"]["requestParams"]["properties"]["images"]["items"];
    assert_eq!(
        images["properties"]["mimeType"]["type"].as_str(),
        Some("string"),
        "prompt images[].mimeType must be a string (serde alias mimeType)"
    );
    assert_eq!(
        images["properties"]["data"]["type"].as_str(),
        Some("string"),
        "prompt images[].data must be a string (base64)"
    );
    assert_eq!(
        images["required"].as_array().map(|a| a.len()),
        Some(2),
        "prompt images items require data+mimeType"
    );

    // origin 四值域（INPUT_ORIGIN）
    let origin = &schema["$defs"]["requestParams"]["properties"]["origin"];
    let origin_vals: Vec<&str> = origin["enum"]
        .as_array()
        .expect("origin enum")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(origin_vals, vec!["user", "monitor", "system", "peer"]);

    // behavior 三态（steer 缺省）
    let behavior = &schema["$defs"]["requestParams"]["properties"]["behavior"];
    let b_vals: Vec<&str> = behavior["enum"]
        .as_array()
        .expect("behavior enum")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(b_vals, vec!["interrupt", "steer", "followUp"]);

    // data 四变体：null 直跑 / queued 排队 / bash_executed / bash_error
    let variants = schema["oneOf"][0]["properties"]["data"]["oneOf"]
        .as_array()
        .expect("prompt data oneOf variants");
    assert_eq!(variants.len(), 4, "prompt data must have 4 variants");
    assert!(variants.iter().any(|v| v.get("type") == Some(&Value::String("null".into()))));
    assert!(variants.iter().any(|v| v["properties"]["status"]["const"] == "queued"));
    assert!(variants
        .iter()
        .any(|v| v["properties"]["status"]["const"] == "bash_executed"));
    assert!(variants.iter().any(|v| v["properties"]["status"]["const"] == "bash_error"));
}

// ---------------------------------------------------------------------------
// 动态层 harness：隔离起真实 worker
// ---------------------------------------------------------------------------

fn find_worker_bin() -> PathBuf {
    if let Ok(env_bin) = std::env::var("ION_WORKER_BIN") {
        return PathBuf::from(env_bin);
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(parent) = exe.parent()
    {
        let sibling = parent.join("ion");
        if sibling.exists() {
            return sibling;
        }
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/debug/ion");
    if manifest.exists() {
        return manifest;
    }
    panic!("ion binary not found; run `cargo build --bin ion` first (or set ION_WORKER_BIN)");
}

struct WorkerProc {
    stdin: std::process::ChildStdin,
    /// reader 线程送来的行（解析后的 JSON；read 侧阻塞→线程侧，主测试线程用
    /// recv_timeout 才能真正实现超时——直接 read_line 静默时 deadline 永不生效）
    rx: std::sync::mpsc::Receiver<Value>,
    /// 收到但尚未消费的响应（按 id 匹配；不匹配的行缓存而非丢弃）
    pending: Vec<Value>,
    child: Child,
    _session_file: PathBuf,
    _home: PathBuf,
    _proj: PathBuf,
    _sessions: PathBuf,
    _reader: std::thread::JoinHandle<()>,
    // faux 脚本文件（需要时）
    _faux_script: Option<PathBuf>,
}

impl Drop for WorkerProc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(
            self._home.parent().unwrap_or(&self._home).parent().unwrap_or(&self._home),
        );
    }
}

impl WorkerProc {
    /// 起一个完全隔离的 worker：假 HOME + 私有 ION_SESSION_DIR + 独立 cwd。
    /// faux_script = Some 时以 ION_FAUX_SCRIPT + ION_FAUX_REPEAT=1 驱动确定性 LLM 轮。
    fn spawn(faux_script: Option<&str>) -> Self {
        let bin = find_worker_bin();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let uniq = format!("g5_{}_{nanos:x}", std::process::id());
        let tmp = std::env::temp_dir().join(&uniq);
        let home = tmp.join("home");
        let proj = tmp.join("proj");
        let sessions = tmp.join("sessions");
        for d in [&home, &proj, &sessions] {
            std::fs::create_dir_all(d).expect("create isolated dirs");
        }

        let mut faux_path = None;
        if let Some(script) = faux_script {
            let p = tmp.join("faux_script.jsonl");
            std::fs::write(&p, script).expect("write faux script");
            faux_path = Some(p);
        }

        let session_id = format!("sess_g5_{:x}", std::process::id() as u64);
        let mut cmd = Command::new(&bin);
        cmd.arg("--mode")
            .arg("rpc")
            .arg("--session")
            .arg(&session_id)
            .current_dir(&proj)
            .env("HOME", &home) // 隔离：假 HOME（~/.ion → tmp/home/.ion）
            .env("ION_SESSION_DIR", &sessions) // 隔离：私有会话目录
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        // 🔴 ION_FAUX_REPEAT=1：静态脚本只排有限条响应，steer/follow_up 消费轮
        // 会继续向 faux 要响应——repeat 复用最后一条 Static，防 run 挂死。
        if let Some(p) = &faux_path {
            cmd.env("ION_FAUX_SCRIPT", p).env("ION_FAUX_REPEAT", "1");
        }

        let mut child = cmd.spawn().expect("failed to spawn ion --mode rpc");
        let stdin = child.stdin.take().expect("no stdin");
        let stdout = child.stdout.take().expect("no stdout");

        // reader 线程：逐行解析 JSON 送入 channel（Panics/EOF 以 Value::Null 收尾）
        let (tx, rx) = std::sync::mpsc::channel::<Value>();
        let reader = std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        if let Ok(v) = serde_json::from_str::<Value>(line.trim()) {
                            if tx.send(v).is_err() {
                                break;
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        // 等 ready 信号
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            if Instant::now() > deadline {
                let _ = child.kill();
                panic!("timed out waiting for worker ready signal");
            }
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(v) => {
                    if v.get("type").and_then(|t| t.as_str()) == Some("ready") {
                        break;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    let _ = child.kill();
                    panic!("worker stdout closed before ready");
                }
            }
        }

        // 等 session 文件落盘
        let session_file = {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if let Some(p) = find_file_recursive(&sessions, "session.jsonl") {
                    break p;
                }
                if Instant::now() > deadline {
                    let _ = child.kill();
                    panic!("session.jsonl never appeared under {}", sessions.display());
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        };

        Self {
            stdin,
            rx,
            pending: Vec::new(),
            child,
            _session_file: session_file,            _home: home,
            _proj: proj,
            _sessions: sessions,
            _reader: reader,
            _faux_script: faux_path,
        }
    }

    fn send_only(&mut self, id: &str, method: &str, params: &Value) {
        let cmd = serde_json::json!({"id": id, "method": method, "params": params});
        writeln!(self.stdin, "{}", serde_json::to_string(&cmd).unwrap())
            .expect("write to worker stdin");
        self.stdin.flush().ok();
    }

    /// 等 id 匹配的 response。先扫 pending 缓冲（此前为等别的 id 而收到的行），
    /// 再从 channel 带超时读——worker 静默时也能真正超时，不会永久挂死。
    fn wait_response(&mut self, id: &str, label: &str) -> Value {
        let deadline = Instant::now() + RESPONSE_TIMEOUT;
        loop {
            // 1) pending 缓冲命中
            if let Some(pos) = self
                .pending
                .iter()
                .position(|v| v.get("id").and_then(|i| i.as_str()) == Some(id))
            {
                return self.pending.remove(pos);
            }
            if Instant::now() > deadline {
                let _ = self.child.kill();
                panic!("timed out waiting for response to {label}");
            }
            match self.rx.recv_timeout(Duration::from_secs(1)) {
                Ok(v) => {
                    if v.get("type").and_then(|t| t.as_str()) == Some("response")
                        && v.get("id").and_then(|i| i.as_str()) == Some(id)
                    {
                        return v;
                    }
                    // 事件/其他 id 的响应：缓存（事件丢弃，响应保留待匹配）
                    if v.get("type").and_then(|t| t.as_str()) == Some("response") {
                        self.pending.push(v);
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    let _ = self.child.kill();
                    panic!("worker stdout closed while waiting for {label}");
                }
            }
        }
    }

    fn send_command(&mut self, method: &str, params: Value) -> Value {
        let id = format!("g5_{method}");
        self.send_only(&id, method, &params);
        self.wait_response(&id, method)
    }

    /// 轮询直到 worker 空闲（get_state.is_running=false）或超时。
    fn wait_idle(&mut self, label: &str) {
        let deadline = Instant::now() + RESPONSE_TIMEOUT;
        loop {
            if Instant::now() > deadline {
                let _ = self.child.kill();
                panic!("timed out waiting for idle ({label})");
            }
            let st = self.send_command("get_state", serde_json::json!({}));
            if st["data"]["is_running"].as_bool() == Some(false) {
                return;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

fn find_file_recursive(root: &Path, name: &str) -> Option<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.file_name().and_then(|n| n.to_str()) == Some(name) {
                    return Some(p);
                }
            }
        }
    }
    None
}

fn grep_in_files(root: &Path, needle: &str) -> bool {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().and_then(|x| x.to_str()) == Some("jsonl")
                    && let Ok(content) = std::fs::read_to_string(&p)
                    && content.contains(needle)
                {
                    return true;
                }
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// 动态层断言辅助
// ---------------------------------------------------------------------------

/// 校验【完整响应信封】过 schema（worker 级 schema 含 command 契约）。
/// 返回 data 供链式取值。
fn expect_valid(
    proc: &mut WorkerProc,
    validator: &Validator,
    schema_name: &str,
    method: &str,
    params: Value,
) -> Value {
    let resp = proc.send_command(method, params);
    assert_eq!(
        resp["success"].as_bool(),
        Some(true),
        "{method} must succeed, got: {resp}"
    );
    assert_eq!(resp["type"].as_str(), Some("response"), "{method} envelope type");
    let errors: Vec<String> = validator
        .iter_errors(&resp)
        .map(|e| format!("{}: {}", e.instance_path(), e))
        .collect();
    assert!(
        errors.is_empty(),
        "{method} response violates schema {schema_name}:\n  resp={resp}\n  {}",
        errors.join("\n  ")
    );
    resp["data"].clone()
}

// ---------------------------------------------------------------------------
// 动态层主测试 1：便宜命令全扫（不触发 LLM）
// ---------------------------------------------------------------------------

#[test]
fn dynamic_worker_exec_commands_validate_against_schemas() {
    let dir = schema_dir();
    let mut proc = WorkerProc::spawn(None);

    // ── 存根族（恒定形状：data=null）──
    for name in [
        "continue",
        "set_steering_mode",
        "set_follow_up_mode",
        "switch_session",
        "rollback_preview",
        "get_latest_agent_change",
    ] {
        let v = compile_schema(&dir.join(format!("{name}.json")));
        expect_valid(&mut proc, &v, name, name, serde_json::json!({}));
    }

    let v = compile_schema(&dir.join("get_agents_files.json"));
    let data = expect_valid(&mut proc, &v, "get_agents_files", "get_agents_files", serde_json::json!({}));
    assert_eq!(data.as_array().map(|a| a.len()), Some(0), "stub returns empty array");

    let v = compile_schema(&dir.join("get_process_snapshot.json"));
    expect_valid(&mut proc, &v, "get_process_snapshot", "get_process_snapshot", serde_json::json!({}));

    let v = compile_schema(&dir.join("clone.json"));
    let data = expect_valid(&mut proc, &v, "clone", "clone", serde_json::json!({}));
    assert!(data["sessionId"].is_string(), "clone echoes session id: {data}");

    let v = compile_schema(&dir.join("copy_fork.json"));
    expect_valid(&mut proc, &v, "copy_fork", "copy_fork", serde_json::json!({}));

    // ── agent 元信息族 ──
    let v = compile_schema(&dir.join("get_commands.json"));
    let data = expect_valid(&mut proc, &v, "get_commands", "get_commands", serde_json::json!({}));
    assert!(data.as_array().map(|a| !a.is_empty()).unwrap_or(false), "commands table non-empty");

    let v = compile_schema(&dir.join("get_system_prompt.json"));
    let data = expect_valid(&mut proc, &v, "get_system_prompt", "get_system_prompt", serde_json::json!({}));
    assert!(data.is_string(), "bare string payload, got {data}");

    let v = compile_schema(&dir.join("set_session_name.json"));
    let data = expect_valid(&mut proc, &v, "set_session_name", "set_session_name", serde_json::json!({"name": "g5 schema batch"}));
    assert_eq!(data["name"].as_str(), Some("g5 schema batch"));

    // 用 get_agents 第一项驱动 switch_agent / get_agent_detail（不硬编码内置名）
    let v_agents = compile_schema(&dir.join("../worker/get_agents.json"));
    let agents = expect_valid(&mut proc, &v_agents, "get_agents(scout)", "get_agents", serde_json::json!({}));
    let first_agent = agents[0]["name"].as_str().expect("agent name").to_string();

    let v = compile_schema(&dir.join("switch_agent.json"));
    let data = expect_valid(&mut proc, &v, "switch_agent", "switch_agent", serde_json::json!({"agentName": first_agent}));
    assert_eq!(data["agent"].as_str(), Some(first_agent.as_str()));

    // not-found 变体（quirk：success:true + data.error）
    let resp = proc.send_command("switch_agent", serde_json::json!({"agentName": "no-such-agent-g5"}));
    assert_eq!(resp["success"].as_bool(), Some(true));
    assert!(resp["data"]["error"].as_str().unwrap_or_default().contains("not found"));

    let v = compile_schema(&dir.join("get_agent_detail.json"));
    let data = expect_valid(&mut proc, &v, "get_agent_detail", "get_agent_detail", serde_json::json!({"agentName": first_agent}));
    assert_eq!(data["name"].as_str(), Some(first_agent.as_str()));
    assert!(data.get("system_prompt").is_some(), "detail carries system_prompt key");

    // ── 队列/重试/重载 ──
    let v = compile_schema(&dir.join("clear_queue.json"));
    let data = expect_valid(&mut proc, &v, "clear_queue", "clear_queue", serde_json::json!({}));
    assert_eq!(data["cleared"].as_bool(), Some(true));

    let v = compile_schema(&dir.join("drain_follow_ups.json"));
    expect_valid(&mut proc, &v, "drain_follow_ups", "drain_follow_ups", serde_json::json!({"wait_ms": 0}));

    let v = compile_schema(&dir.join("abort_retry.json"));
    expect_valid(&mut proc, &v, "abort_retry", "abort_retry", serde_json::json!({}));

    let v = compile_schema(&dir.join("reload.json"));
    let data = expect_valid(&mut proc, &v, "reload", "reload", serde_json::json!({}));
    assert!(
        data.get("message").is_some() || data.get("reloaded").is_some(),
        "reload two variants: {data}"
    );

    // ── bash 直执族 ──
    let v = compile_schema(&dir.join("bash.json"));
    let data = expect_valid(&mut proc, &v, "bash", "bash", serde_json::json!({"command": "echo g5ok"}));
    assert_eq!(data["exitCode"].as_i64(), Some(0));
    assert!(data["output"].as_str().unwrap_or_default().contains("g5ok"));

    let v = compile_schema(&dir.join("bash_command.json"));
    let data = expect_valid(&mut proc, &v, "bash_command", "bash_command", serde_json::json!({"command": "echo g5bc"}));
    assert_eq!(data["status"].as_str(), Some("ok"));
    let data = expect_valid(&mut proc, &v, "bash_command(err)", "bash_command", serde_json::json!({"command": "exit 3"}));
    assert_eq!(data["status"].as_str(), Some("ok"), "exit!=0 is still status:ok with exitCode:3");
    assert_eq!(data["exitCode"].as_i64(), Some(3));

    // bash_command 缺 command → error 信封
    let resp = proc.send_command("bash_command", serde_json::json!({}));
    assert_eq!(resp["success"].as_bool(), Some(false), "missing command must error: {resp}");

    // abort_bash 动态验证（P0-1 修复后恢复）：修复前 bid 非空即走 process_map
    // 锁路径 → blocking_lock 在 tokio runtime 内 panic → 一发命令杀死 worker
    // 进程（master@6003182 实测复现，见 abort_bash.json x-quirks）。修复后
    // lock().await，未知 bid 正常返回 data.error + available 列表。
    let v = compile_schema(&dir.join("abort_bash.json"));
    // ① 缺 bid 变体（不触锁路径）
    let data = expect_valid(
        &mut proc, &v, "abort_bash(missing bid)", "abort_bash", serde_json::json!({}),
    );
    assert_eq!(data["error"], "missing 'bid' parameter");
    // ② 未知 bid 变体：走 process_map 锁路径（修复前此处即杀死 worker）
    let data = expect_valid(
        &mut proc, &v, "abort_bash(not found)", "abort_bash",
        serde_json::json!({"bid": "g5ghost"}),
    );
    assert!(data["error"].as_str().unwrap_or_default().contains("not found"));
    assert!(data["available"].is_array(), "not-found 变体附 available: {data}");
    // ③ 存活证明：abort_bash 之后 worker 仍正常服务（修复前此处拿不到响应）
    let v_continue = compile_schema(&dir.join("continue.json"));
    expect_valid(&mut proc, &v_continue, "continue", "continue", serde_json::json!({}));

    // ── 远程工具注册 ──
    let v = compile_schema(&dir.join("register_remote_tool.json"));
    let data = expect_valid(
        &mut proc, &v, "register_remote_tool", "register_remote_tool",
        serde_json::json!({
            "name": "g5_ping",
            "url": "https://example.invalid/ping",
            "description": "schema test tool",
            "parameters": {"type": "object"}
        }),
    );
    assert_eq!(data["status"].as_str(), Some("registered"));

    let v = compile_schema(&dir.join("unregister_remote_tool.json"));
    let data = expect_valid(&mut proc, &v, "unregister_remote_tool", "unregister_remote_tool", serde_json::json!({"name": "g5_ping"}));
    assert_eq!(data["status"].as_str(), Some("removed"));

    // ── goal evolver（缺 data_dir 变体：data.success=false）──
    let v = compile_schema(&dir.join("goal_evolver_run_once.json"));
    let data = expect_valid(&mut proc, &v, "goal_evolver_run_once(missing dir)", "goal_evolver_run_once", serde_json::json!({}));
    assert_eq!(data["success"].as_bool(), Some(false));
    assert!(data["error"].as_str().is_some());

    // ── legacy diff（快照未启用变体）──
    let v = compile_schema(&dir.join("get_file_diff.json"));
    expect_valid(&mut proc, &v, "get_file_diff", "get_file_diff", serde_json::json!({"filePath": "a.txt"}));

    // ── 会话 JSONL 追加族 ──
    for name in [
        "append_entry",
        "append_custom_entry",
        "append_custom_message",
        "append_system_event",
        "append_model_change",
        "append_thinking_level_change",
        "append_agent_change",
        "append_session_name",
        "append_label",
        "append_active_tools_change",
    ] {
        let v = compile_schema(&dir.join(format!("{name}.json")));
        let params = match name {
            "append_entry" => serde_json::json!({"type": "custom", "customType": "g5_test", "data": {"k": 1}}),
            "append_custom_entry" => serde_json::json!({"type": "g5_probe", "data": {"via": "schema-test"}}),
            "append_custom_message" => serde_json::json!({"type": "g5_note", "content": "hello", "display": false}),
            "append_system_event" => serde_json::json!({"type": "g5_event", "label": "L", "display": true}),
            "append_model_change" => serde_json::json!({"provider": "faux", "modelId": "faux-test"}),
            "append_thinking_level_change" => serde_json::json!({"level": "off"}),
            "append_agent_change" => serde_json::json!({"name": first_agent}),
            "append_session_name" => serde_json::json!({"name": "g5-named"}),
            "append_label" => serde_json::json!({"targetId": "e1", "label": "g5"}),
            "append_active_tools_change" => serde_json::json!({"activeToolNames": ["read", "bash"]}),
            _ => unreachable!(),
        };
        let data = expect_valid(&mut proc, &v, name, name, params);
        assert_eq!(data["status"].as_str(), Some("appended"), "{name} appended receipt");
    }

    // append_entry 白名单拒绝（W6）→ error 信封
    let resp = proc.send_command("append_entry", serde_json::json!({"type": "message", "content": "forge"}));
    assert_eq!(resp["success"].as_bool(), Some(false), "whitelist must reject 'message' type: {resp}");

    let v = compile_schema(&dir.join("send_custom_message.json"));
    let data = expect_valid(&mut proc, &v, "send_custom_message", "send_custom_message", serde_json::json!({"type": "g5", "content": "hi", "deliverAs": "followUp"}));
    assert_eq!(data["status"].as_str(), Some("queued"));

    // request_restart 不做动态验证：它会真实写 /tmp/.ion-evolve-restart 哨兵
    // （自进化 watchdog 轮询文件），在共享开发机上可能误触发生产 watchdog 重启。
    // 契约由静态层覆盖（draft 2020-12 编译 + _index 登记）。
}

// ---------------------------------------------------------------------------
// 动态层主测试 2：prompt 真实 agent 轮（faux 驱动）+ 忙时排队回执 + '!' 拦截 + origin
// ---------------------------------------------------------------------------

#[test]
fn dynamic_prompt_faux_turn_busy_receipts_bash_and_origin() {
    let dir = schema_dir();
    let prompt_validator = compile_schema(&dir.join("prompt.json"));

    // faux 脚本：第 1 轮调 bash sleep 2（拉长 busy 窗口），第 2 条收尾。
    // ION_FAUX_REPEAT=1 使后续 steer/follow_up 消费轮复用最后一条 Static。
    let script = "{\"tool_call\":{\"name\":\"bash\",\"input\":{\"command\":\"sleep 2\"}}}\n\
                  {\"text\":\"g5 busy test done\"}\n";
    let mut proc = WorkerProc::spawn(Some(script));

    // ── 1. 直跑 ACK（worker 在 agent.run 前立即回 data=null，输出走事件流）──
    proc.send_only("g5_p1", "prompt", &serde_json::json!({"text": "启动长任务"}));
    let p1 = proc.wait_response("g5_p1", "prompt#1");
    assert_eq!(p1["success"].as_bool(), Some(true), "prompt#1 must succeed: {p1}");
    assert!(p1["data"].is_null(), "idle-run prompt returns data=null: {p1}");
    let errors: Vec<String> = prompt_validator
        .iter_errors(&p1)
        .map(|e| format!("{}: {}", e.instance_path(), e))
        .collect();
    assert!(errors.is_empty(), "prompt#1 violates prompt schema: {}", errors.join("; "));

    // 此刻 agent.run 进行中（faux 立刻吐 tool_call → bash sleep 2 执行）。
    // 等 600ms 确保 is_running 后发忙时 prompt。
    std::thread::sleep(Duration::from_millis(600));

    // ── 2. 忙时 steer 排队回执 ──
    let steer_receipt = proc.send_command("prompt", serde_json::json!({"text": "插队消息"}));
    assert_eq!(
        steer_receipt["data"]["status"].as_str(),
        Some("queued"),
        "busy prompt with default steer behavior must queue: {steer_receipt}"
    );
    assert_eq!(steer_receipt["data"]["queue"].as_str(), Some("steering"));
    let errors: Vec<String> = prompt_validator
        .iter_errors(&steer_receipt)
        .map(|e| format!("{}: {}", e.instance_path(), e))
        .collect();
    assert!(errors.is_empty(), "steer receipt violates prompt schema: {}", errors.join("; "));

    // ── 3. 忙时 followUp 排队回执 ──
    let follow_receipt = proc.send_command("prompt", serde_json::json!({"text": "稍后处理", "behavior": "followUp"}));
    assert_eq!(follow_receipt["data"]["status"].as_str(), Some("queued"));
    assert_eq!(follow_receipt["data"]["queue"].as_str(), Some("followUp"));
    let errors: Vec<String> = prompt_validator
        .iter_errors(&follow_receipt)
        .map(|e| format!("{}: {}", e.instance_path(), e))
        .collect();
    assert!(errors.is_empty(), "followUp receipt violates prompt schema: {}", errors.join("; "));

    // 排队消息被后续轮消化（steer 消息落盘 source=steer；ION_FAUX_REPEAT=1 供消费轮取响应）
    {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if grep_in_files(&proc._sessions, "插队消息") {
                break;
            }
            if Instant::now() > deadline {
                let _ = proc.child.kill();
                panic!("steered message never landed in session JSONL");
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    proc.wait_idle("after busy turn");

    // ── 4. 空闲轮 + images/origin 参数（schema 校验真实直跑响应）──
    let p4 = proc.send_command(
        "prompt",
        serde_json::json!({
            "text": "再看一眼",
            "images": [{"data": "aGk=", "mimeType": "image/png"}],
            "origin": "monitor"
        }),
    );
    assert_eq!(p4["success"].as_bool(), Some(true), "prompt#4 with images+origin: {p4}");
    assert!(p4["data"].is_null());
    let errors: Vec<String> = prompt_validator
        .iter_errors(&p4)
        .map(|e| format!("{}: {}", e.instance_path(), e))
        .collect();
    assert!(errors.is_empty(), "prompt#4 violates prompt schema: {}", errors.join("; "));

    // p4 的 ACK 在 run 前发出，run（faux repeat 轮）仍在后台——等空闲再发 '!cmd'，
    // 否则 busy 分派会把 '!' 消息排进队列（返回 queued 而非 bash_executed）
    proc.wait_idle("before bang-cmd");

    // origin=monitor → 旁路 custom(input_origin) 条目落盘（INPUT_ORIGIN 契约）
    assert!(
        grep_in_files(&proc._sessions, "input_origin"),
        "origin=monitor must append a custom(input_origin) entry into session JSONL"
    );

    // ── 5. '!' 前缀拦截：直发 bash，不进 agent loop ──
    let p5 = proc.send_command("prompt", serde_json::json!({"text": "!echo g5exec_probe"}));
    assert_eq!(p5["success"].as_bool(), Some(true), "'!cmd' prompt: {p5}");
    assert_eq!(p5["data"]["status"].as_str(), Some("bash_executed"));
    assert_eq!(p5["data"]["exitCode"].as_i64(), Some(0));
    assert!(p5["data"]["output"].as_str().unwrap_or_default().contains("g5exec_probe"));
    let errors: Vec<String> = prompt_validator
        .iter_errors(&p5)
        .map(|e| format!("{}: {}", e.instance_path(), e))
        .collect();
    assert!(errors.is_empty(), "bash_executed receipt violates prompt schema: {}", errors.join("; "));

    // '!' 执行失败变体
    let p6 = proc.send_command("prompt", serde_json::json!({"text": "!exit 7"}));
    assert_eq!(p6["data"]["status"].as_str(), Some("bash_executed"), "exit 7 is executed, exitCode carries 7: {p6}");
    assert_eq!(p6["data"]["exitCode"].as_i64(), Some(7));
}
