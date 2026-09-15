//! rpc_schema_session_test.rs — 会话/消息域 RPC JSON Schema 契约一致性测试
//!
//! 两层校验：
//!   静态层：`schemas/rpc/session/_index.json` 与 schema 文件一一对应；
//!           每个 schema 都是合法且可编译的 draft 2020-12。
//!   动态层：隔离起一个真实 worker（`ion --mode rpc`，假 HOME + 私有
//!           ION_SESSION_DIR，不碰真实 ~/.ion），喂 fixture 会话 JSONL，
//!           拿真实响应对着 schema 验证（25 个命令 + 错误信封形状）。
//!
//! schema 约定：每个 schema 校验成功响应的 `data` 载荷；请求参数在
//! `$defs/requestParams`。错误信封 {success:false,error} 在动态层单独断言。
//!
//! 用法：
//!   cargo build --bin ion
//!   cargo test --test rpc_schema_session
//!
//! 🔴 不修改 src/ 下任何文件；辅助函数全部自包含在本文件内。

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use jsonschema::Validator;
use serde_json::Value;

const READY_TIMEOUT: Duration = Duration::from_secs(60);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(15);
const FILE_WAIT_TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// Schema 目录
// ---------------------------------------------------------------------------

fn schema_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("schemas/rpc/session")
}

fn read_json(path: &Path) -> Value {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("failed to parse {}: {e}", path.display()))
}

/// 编译一个 schema 文件（draft 2020-12 由 $schema 声明）
fn compile_schema(path: &Path) -> Validator {
    let schema_value = read_json(path);
    jsonschema::validator_for(&schema_value)
        .unwrap_or_else(|e| panic!("schema {} failed to compile: {e}", path.display()))
}

// ---------------------------------------------------------------------------
// 静态层
// ---------------------------------------------------------------------------

#[test]
fn static_index_and_schema_files_match() {
    let dir = schema_dir();
    let index_path = dir.join("_index.json");
    assert!(index_path.exists(), "_index.json missing under {}", dir.display());
    let index = read_json(&index_path);

    let commands = index["commands"]
        .as_array()
        .expect("_index.json must have a commands array");

    // index 里每个命令都必须有对应 schema 文件
    let mut indexed: Vec<String> = Vec::new();
    for cmd in commands {
        let name = cmd["command"].as_str().expect("command name must be a string");
        let schema_ref = cmd["schema"].as_str().expect("schema ref must be a string");
        let path = dir.join(schema_ref.trim_start_matches("./"));
        assert!(path.exists(), "index lists '{name}' but {} is missing", path.display());
        indexed.push(name.to_string());
    }

    // 反向：目录里除 _index.json 外的每个 .json 都必须登记在 index 里
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
    assert_eq!(
        indexed, on_disk,
        "_index.json and schema files are out of sync"
    );
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
        assert!(
            value.get("$id").is_some(),
            "{} must carry an $id",
            path.display()
        );
        assert!(
            value.get("title").is_some(),
            "{} must carry a title",
            path.display()
        );
        compile_schema(&path); // panics on invalid schema
        count += 1;
    }
    assert!(count >= 30, "expected >=30 session-domain schemas, found {count}");
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
    panic!(
        "ion binary not found; run `cargo build --bin ion` first (or set ION_WORKER_BIN)"
    );
}

struct WorkerProc {
    stdin: std::process::ChildStdin,
    reader: BufReader<std::process::ChildStdout>,
    child: Child,
    session_file: PathBuf,
    // 保住临时目录直到进程结束
    _home: PathBuf,
    _proj: PathBuf,
    _sessions: PathBuf,
}

impl Drop for WorkerProc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(self._home.parent().unwrap_or(&self._home));
    }
}

impl WorkerProc {
    /// 起一个完全隔离的 worker：假 HOME + 私有 ION_SESSION_DIR + 独立 cwd。
    /// 绝不读写真实 ~/.ion。
    fn spawn() -> Self {
        let bin = find_worker_bin();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let uniq = format!("s1_{}_{nanos:x}", std::process::id());
        let tmp = std::env::temp_dir().join(&uniq);
        let home = tmp.join("home");
        let proj = tmp.join("proj");
        let sessions = tmp.join("sessions");
        for d in [&home, &proj, &sessions] {
            std::fs::create_dir_all(d).expect("create isolated dirs");
        }

        let session_id = format!("sess_{:x}", std::process::id() as u64);
        let mut child = Command::new(&bin)
            .arg("--mode")
            .arg("rpc")
            .arg("--session")
            .arg(&session_id)
            .current_dir(&proj)
            .env("HOME", &home) // 隔离：假 HOME（~/.ion → tmp/home/.ion）
            .env("ION_SESSION_DIR", &sessions) // 隔离：私有会话目录
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn ion --mode rpc");

        let stdin = child.stdin.take().expect("no stdin");
        let stdout = child.stdout.take().expect("no stdout");
        let mut reader = BufReader::new(stdout);

        // 等 ready 信号
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            if Instant::now() > deadline {
                let _ = child.kill();
                panic!("timed out waiting for worker ready signal");
            }
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    let _ = child.kill();
                    panic!("worker closed stdout before ready");
                }
                Ok(_) => {
                    if let Ok(v) = serde_json::from_str::<Value>(line.trim())
                        && v.get("type").and_then(|t| t.as_str()) == Some("ready")
                    {
                        break;
                    }
                }
                Err(e) => {
                    let _ = child.kill();
                    panic!("error reading worker stdout: {e}");
                }
            }
        }

        // 等 session 文件落盘（ensure_session_header 在启动早期完成）
        let session_file = {
            let deadline = Instant::now() + FILE_WAIT_TIMEOUT;
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
            reader,
            child,
            session_file,
            _home: home,
            _proj: proj,
            _sessions: sessions,
        }
    }

    fn send_command(&mut self, method: &str, params: Value) -> Value {
        let id = format!("s1_{method}");
        let cmd = serde_json::json!({"id": id, "method": method, "params": params});
        writeln!(self.stdin, "{}", serde_json::to_string(&cmd).unwrap())
            .expect("write to worker stdin");
        self.stdin.flush().ok();

        let deadline = Instant::now() + RESPONSE_TIMEOUT;
        loop {
            if Instant::now() > deadline {
                let _ = self.child.kill();
                panic!("timed out waiting for response to {method}");
            }
            let mut line = String::new();
            match self.reader.read_line(&mut line) {
                Ok(0) => {
                    let _ = self.child.kill();
                    panic!("worker closed stdout while waiting for {method}");
                }
                Ok(_) => {
                    if let Ok(v) = serde_json::from_str::<Value>(line.trim())
                        && v.get("id").and_then(|i| i.as_str()) == Some(id.as_str())
                        && v.get("type").and_then(|t| t.as_str()) == Some("response")
                    {
                        return v;
                    }
                    // 事件等其他行：跳过继续等
                }
                Err(e) => {
                    let _ = self.child.kill();
                    panic!("error reading response for {method}: {e}");
                }
            }
        }
    }

    /// 追加 fixture entry 到会话 JSONL（worker 侧靠 mtime 缓存失效感知新内容）
    fn append_fixture(&self, lines: &[String]) {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&self.session_file)
            .expect("open session file for append");
        for l in lines {
            writeln!(f, "{l}").expect("append fixture line");
        }
        f.flush().ok();
        // 轻微 sleep 让 mtime 确定变化
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn find_file_recursive(root: &Path, name: &str) -> Option<PathBuf> {
    let stack = vec![root.to_path_buf()];
    let mut stack = stack;
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

// ---------------------------------------------------------------------------
// 动态层断言辅助
// ---------------------------------------------------------------------------

/// 断言成功信封形状 + data 过 schema；返回 data 供链式取值。
fn expect_valid(proc: &mut WorkerProc, validator: &Validator, schema_name: &str, method: &str, params: Value) -> Value {
    let resp = proc.send_command(method, params);
    assert_eq!(
        resp["success"].as_bool(),
        Some(true),
        "{method} must succeed, got: {resp}"
    );
    assert_eq!(resp["type"].as_str(), Some("response"), "{method} envelope type");
    assert!(resp["id"].is_string(), "{method} envelope id");
    let data = resp["data"].clone();
    let errors: Vec<String> = validator
        .iter_errors(&data)
        .map(|e| format!("{}: {}", e.instance_path(), e))
        .collect();
    assert!(
        errors.is_empty(),
        "{method} data violates schema {schema_name}:\n  data={data}\n  {}",
        errors.join("\n  ")
    );
    data
}

/// 断言错误信封形状（schema 不覆盖错误路径，这里做信封契约断言）。
fn expect_error(proc: &mut WorkerProc, method: &str, params: Value) {
    let resp = proc.send_command(method, params);
    assert_eq!(resp["success"].as_bool(), Some(false), "{method} must fail: {resp}");
    assert_eq!(resp["type"].as_str(), Some("response"), "{method} envelope type");
    let err = resp["error"].as_str().unwrap_or_default();
    assert!(!err.is_empty(), "{method} error must be a non-empty string: {resp}");
    assert!(resp.get("data").is_none(), "{method} error must not carry data: {resp}");
}

// ---------------------------------------------------------------------------
// 动态层主测试
// ---------------------------------------------------------------------------

#[test]
fn dynamic_worker_session_commands_validate_against_schemas() {
    let dir = schema_dir();
    let mut proc = WorkerProc::spawn();

    // fixture：3 轮 user/assistant（带数字 turnId，与 fork 的数值语义匹配）
    proc.append_fixture(&[
        r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-09-15T10:00:01Z","turnId":1,"message":{"role":"user","content":"第一个问题"}}"#.into(),
        r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-09-15T10:00:02Z","turnId":1,"message":{"role":"assistant","content":"第一个回答"}}"#.into(),
        r#"{"type":"message","id":"m3","parentId":"m2","timestamp":"2026-09-15T10:00:03Z","turnId":2,"message":{"role":"user","content":"第二个问题"}}"#.into(),
        r#"{"type":"message","id":"m4","parentId":"m3","timestamp":"2026-09-15T10:00:04Z","turnId":2,"message":{"role":"assistant","content":"第二个回答"}}"#.into(),
        r#"{"type":"message","id":"m5","parentId":"m4","timestamp":"2026-09-15T10:00:05Z","turnId":3,"message":{"role":"user","content":"第三个问题"}}"#.into(),
        r#"{"type":"message","id":"m6","parentId":"m5","timestamp":"2026-09-15T10:00:06Z","turnId":3,"message":{"role":"assistant","content":"第三个回答"}}"#.into(),
    ]);

    // ── 状态/统计族 ──
    let v = compile_schema(&dir.join("get_state.json"));
    expect_valid(&mut proc, &v, "get_state", "get_state", serde_json::json!({}));

    let v = compile_schema(&dir.join("get_session_info.json"));
    expect_valid(&mut proc, &v, "get_session_info", "get_session_info", serde_json::json!({}));

    let v = compile_schema(&dir.join("get_session_stats.json"));
    expect_valid(&mut proc, &v, "get_session_stats", "get_session_stats", serde_json::json!({}));

    let v = compile_schema(&dir.join("get_children.json"));
    expect_valid(&mut proc, &v, "get_children", "get_children", serde_json::json!({}));

    let v = compile_schema(&dir.join("get_inflight_messages.json"));
    expect_valid(&mut proc, &v, "get_inflight_messages", "get_inflight_messages", serde_json::json!({"limit": 5}));

    // ── 检索族 ──
    let v = compile_schema(&dir.join("get_messages.json"));
    let msgs = expect_valid(&mut proc, &v, "get_messages", "get_messages", serde_json::json!({}));
    assert!(msgs["totalCount"].as_u64().unwrap_or(0) >= 6, "fixture messages must be visible: {msgs}");

    // get_messages 带参数（view/limit/include_custom/complete_turn）
    expect_valid(&mut proc, &v, "get_messages", "get_messages", serde_json::json!({"view":"full","limit":2,"include_custom":"display_only","complete_turn":true}));
    expect_valid(&mut proc, &v, "get_messages", "get_messages", serde_json::json!({"from":"head","limit":3}));

    let v = compile_schema(&dir.join("list_turns.json"));
    let turns = expect_valid(&mut proc, &v, "list_turns", "list_turns", serde_json::json!({}));
    assert!(
        turns["turns"].as_array().map(|a| !a.is_empty()).unwrap_or(false),
        "fixture must group into >=1 turn: {turns}"
    );
    let first_turn_id = turns["turns"][0]["turnId"].as_str().expect("turnId string").to_string();

    let v = compile_schema(&dir.join("list_inputs.json"));
    let inputs = expect_valid(&mut proc, &v, "list_inputs", "list_inputs", serde_json::json!({}));
    assert!(inputs["inputs"].as_array().map(|a| !a.is_empty()).unwrap_or(false), "inputs must be non-empty: {inputs}");

    let v = compile_schema(&dir.join("get_turn_detail.json"));
    expect_valid(&mut proc, &v, "get_turn_detail(found)", "get_turn_detail", serde_json::json!({"turnId": first_turn_id}));
    // not-found 变体：成功信封里带 {error,turnId}
    expect_valid(&mut proc, &v, "get_turn_detail(not found)", "get_turn_detail", serde_json::json!({"turnId": "no_such_turn"}));

    let v = compile_schema(&dir.join("get_full_messages.json"));
    expect_valid(&mut proc, &v, "get_full_messages", "get_full_messages", serde_json::json!({}));

    let v = compile_schema(&dir.join("get_last_assistant_text.json"));
    expect_valid(&mut proc, &v, "get_last_assistant_text", "get_last_assistant_text", serde_json::json!({}));

    let v = compile_schema(&dir.join("get_fork_messages.json"));
    expect_valid(&mut proc, &v, "get_fork_messages", "get_fork_messages", serde_json::json!({}));

    // ── 树族 ──
    let v = compile_schema(&dir.join("get_tree.json"));
    expect_valid(&mut proc, &v, "get_tree(structure)", "get_tree", serde_json::json!({}));
    expect_valid(&mut proc, &v, "get_tree(full)", "get_tree", serde_json::json!({"mode":"full"}));

    let v = compile_schema(&dir.join("get_tree_with_leaf.json"));
    expect_valid(&mut proc, &v, "get_tree_with_leaf", "get_tree_with_leaf", serde_json::json!({}));

    let v = compile_schema(&dir.join("navigate_tree.json"));
    expect_valid(&mut proc, &v, "navigate_tree", "navigate_tree", serde_json::json!({}));

    // ── 分支闭环 ──
    let v = compile_schema(&dir.join("fork.json"));
    let forked = expect_valid(&mut proc, &v, "fork", "fork", serde_json::json!({"turnId": 2, "name": "try-schema"}));
    assert_eq!(forked["branch"].as_str(), Some("try-schema"));

    let v = compile_schema(&dir.join("get_branches.json"));
    let branches = expect_valid(&mut proc, &v, "get_branches", "get_branches", serde_json::json!({}));
    assert!(branches["branches"].as_array().map(|a| !a.is_empty()).unwrap_or(false));

    let v = compile_schema(&dir.join("checkout_branch.json"));
    expect_valid(&mut proc, &v, "checkout_branch", "checkout_branch", serde_json::json!({"name": "try-schema"}));
    expect_valid(&mut proc, &v, "checkout_branch(main)", "checkout_branch", serde_json::json!({"name": "main"}));

    // 分支视图过滤（branch:* view）
    let v = compile_schema(&dir.join("get_messages.json"));
    expect_valid(&mut proc, &v, "get_messages(branch view)", "get_messages", serde_json::json!({"view":"branch:try-schema"}));

    // ── 队列/上下文/工具 ──
    let v = compile_schema(&dir.join("get_queue.json"));
    expect_valid(&mut proc, &v, "get_queue", "get_queue", serde_json::json!({}));

    let v = compile_schema(&dir.join("get_context_usage.json"));
    expect_valid(&mut proc, &v, "get_context_usage", "get_context_usage", serde_json::json!({}));

    let v = compile_schema(&dir.join("get_active_tools.json"));
    expect_valid(&mut proc, &v, "get_active_tools", "get_active_tools", serde_json::json!({}));

    // ── 软删除族（空 targetIds 走 soft-failure 变体，零副作用）──
    let v = compile_schema(&dir.join("delete_entries.json"));
    expect_valid(&mut proc, &v, "delete_entries(empty)", "delete_entries", serde_json::json!({"targetIds": []}));

    let v = compile_schema(&dir.join("summarize_entries.json"));
    expect_valid(&mut proc, &v, "summarize_entries(empty)", "summarize_entries", serde_json::json!({"targetIds": []}));

    let v = compile_schema(&dir.join("restore_entries.json"));
    expect_valid(&mut proc, &v, "restore_entries(empty)", "restore_entries", serde_json::json!({"targetIds": []}));

    // ── 存根 ──
    let v = compile_schema(&dir.join("new_session.json"));
    expect_valid(&mut proc, &v, "new_session", "new_session", serde_json::json!({}));

    // ── 错误信封契约（schema 之外的信封断言）──
    expect_error(&mut proc, "fork", serde_json::json!({})); // 缺 turnId
    expect_error(&mut proc, "checkout_branch", serde_json::json!({"name": "no-such-branch"}));
}

/// host 直读面（get_session_messages / list_session_turns 的 host 级 data 形状）
/// 与 worker 面 schema 相同 —— 这里用 fixture 文件 + 相同 data schema 做
/// 纯函数级对照（不经 host 进程，host 进程面由 tests/rpc_schema_session_ci.sh 覆盖）。
#[test]
fn static_retrieval_schemas_accept_fixture_shape() {
    let dir = schema_dir();
    let fixture = r#"[
        {"type":"message","id":"m1","parentId":null,"turnId":1,"message":{"role":"user","content":"q1"}},
        {"type":"message","id":"m2","parentId":"m1","turnId":1,"message":{"role":"assistant","content":"a1"}}
    ]"#;
    // data 形状采样：手工构造与 bin/ion.rs:6971/6906 输出同形的载荷
    let messages_data = serde_json::json!({
        "messages": serde_json::from_str::<Value>(fixture).unwrap(),
        "hasMore": false,
        "totalCount": 2,
        "nextCursor": null,
        "view": "live",
        "compactionPoints": [],
    });
    let v = compile_schema(&dir.join("get_session_messages.json"));
    assert!(v.is_valid(&messages_data), "host messages shape must match get_session_messages schema");

    let turns_data = serde_json::json!({
        "turns": [{
            "turnId": "m1", "userContent": "q1", "assistantContent": "a1",
            "keySteps": [], "toolCallCount": 0,
            "tokens": {"input": 0, "output": 0},
            "status": "", "summary": "", "durationMs": 0, "source": ""
        }],
        "hasMore": false,
        "totalCount": 1,
        "nextCursor": null,
    });
    let v = compile_schema(&dir.join("list_session_turns.json"));
    assert!(v.is_valid(&turns_data), "host turns shape must match list_session_turns schema");
}
