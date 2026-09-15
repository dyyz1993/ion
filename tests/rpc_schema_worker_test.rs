//! rpc_schema_worker_test — worker 编排域 RPC JSON Schema 契约测试（S2 批次）
//!
//! 两层：
//! 1. 静态：`schemas/rpc/worker/_index.json` ↔ 命令文件一一对应 + 全部 schema 可编译
//!    （draft 2020-12，jsonschema 0.46）+ 参数形状正反样本。
//! 2. 动态：在【隔离 host】（私有 HOME + ION_HOST_SOCKET + ION_SESSION_DIR +
//!    ION_WORKTREE_ROOT，绝不触碰真实 ~/.ion，绝不操作生产 host）上真实起 host /
//!    create_session / create_worker，把真实响应逐个过 schema。
//!
//! 运行：`cargo build --bin ion && cargo test --test rpc_schema_worker_test`

use jsonschema::Validator;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

// ---------------------------------------------------------------------------
// 常量 / 工具
// ---------------------------------------------------------------------------

const SCHEMA_DIR: &str = "schemas/rpc/worker";
const HOST_READY_TIMEOUT_SECS: u64 = 60;
const RPC_TIMEOUT_SECS: u64 = 120;

fn schema_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(SCHEMA_DIR)
}

fn read_json(path: &std::path::Path) -> Value {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

/// 编译一个 schema 文件（失败即 panic，带文件名与错误）
fn compile_file(path: &std::path::Path) -> Validator {
    let doc = read_json(path);
    jsonschema::validator_for(&doc)
        .unwrap_or_else(|e| panic!("compile {}: {e}", path.display()))
}

fn assert_valid(schema: &Validator, doc_name: &str, instance: &Value, label: &str) {
    if !schema.is_valid(instance) {
        let errs: Vec<String> = schema
            .iter_errors(instance)
            .map(|e| format!("  - {e} (at {})", e.instance_path()))
            .collect();
        panic!(
            "{doc_name}: {label} 未通过 schema 校验：\ninstance={}\nerrors:\n{}",
            serde_json::to_string_pretty(instance).unwrap_or_default(),
            errs.join("\n")
        );
    }
}

fn assert_invalid(schema: &Validator, doc_name: &str, instance: &Value, label: &str) {
    if schema.is_valid(instance) {
        panic!(
            "{doc_name}: {label} 本应不通过 schema（反样本），却通过了：\n{}",
            serde_json::to_string_pretty(instance).unwrap_or_default()
        );
    }
}

static REQ_SEQ: AtomicU64 = AtomicU64::new(1);
fn next_id() -> String {
    format!("schema-t-{}", REQ_SEQ.fetch_add(1, Ordering::SeqCst))
}

// ---------------------------------------------------------------------------
// 静态 1：_index.json ↔ 文件一一对应
// ---------------------------------------------------------------------------

#[test]
fn static_index_matches_files() {
    let dir = schema_dir();
    let index = read_json(&dir.join("_index.json"));
    let commands = index["commands"]
        .as_array()
        .expect("_index.json commands 必须是数组");

    let mut referenced: std::collections::BTreeSet<String> = Default::default();
    for c in commands {
        let cmd = c["command"].as_str().expect("command 名");
        let file = c["file"].as_str().expect("file 字段");
        let level = c["level"].as_str().expect("level 字段");
        assert!(
            matches!(level, "host" | "worker" | "bridge" | "host+worker" | "host+worker-bridge"),
            "{cmd}: level 非法：{level}"
        );
        let path = dir.join(file);
        assert!(path.exists(), "{cmd}: 引用的文件不存在：{file}");
        let stem = path.file_stem().unwrap().to_string_lossy();
        assert_eq!(
            stem, cmd,
            "index 条目 command='{cmd}' 与文件名 '{stem}' 不一致"
        );
        assert!(
            !referenced.contains(file),
            "{file} 在 index 里出现两次"
        );
        referenced.insert(file.to_string());
    }

    // 反向：目录里的每个 .json 都被 index 引用（_index.json 除外）
    let mut on_disk: std::collections::BTreeSet<String> = Default::default();
    for entry in std::fs::read_dir(&dir).expect("schema 目录可读") {
        let p = entry.expect("dir entry").path();
        if p.extension().and_then(|e| e.to_str()) == Some("json") {
            let name = p.file_name().unwrap().to_string_lossy().to_string();
            if name != "_index.json" {
                on_disk.insert(name);
            }
        }
    }
    let orphans: Vec<_> = on_disk.difference(&referenced).collect();
    assert!(
        orphans.is_empty(),
        "目录里存在未登记进 _index.json 的 schema：{orphans:?}"
    );
}

// ---------------------------------------------------------------------------
// 静态 2：全部 schema 可编译（draft 2020-12）
// ---------------------------------------------------------------------------

#[test]
fn static_all_schemas_compile() {
    let dir = schema_dir();
    let mut count = 0;
    for entry in std::fs::read_dir(&dir).expect("schema 目录可读") {
        let p = entry.expect("dir entry").path();
        if p.extension().and_then(|e| e.to_str()) == Some("json") {
            compile_file(&p);
            count += 1;
        }
    }
    assert!(
        count >= 26,
        "schema 文件数异常（期望 ≥26 = 25 命令 + _index）：{count}"
    );
}

// ---------------------------------------------------------------------------
// 静态 3：信封正反样本（envelope 对 success/error 的判别）
// ---------------------------------------------------------------------------

#[test]
fn static_envelope_positive_and_negative_samples() {
    let dir = schema_dir();

    // kill_worker：success / error 信封
    let schema = compile_file(&dir.join("kill_worker.json"));
    assert_valid(
        &schema,
        "kill_worker",
        &json!({"type":"response","id":"1","success":true,
                "data":{"killed":true,"cleanupWorktree":true,"deleteBranch":false}}),
        "success 信封",
    );
    assert_valid(
        &schema,
        "kill_worker",
        &json!({"type":"response","id":"1","success":false,"error":"worker not found"}),
        "error 信封",
    );
    // 反样本：success=true 却带 error；data 缺字段；status 枚举外
    assert_invalid(
        &schema,
        "kill_worker",
        &json!({"type":"response","id":"1","success":true,"error":"boom"}),
        "success 带 error",
    );
    assert_invalid(
        &schema,
        "kill_worker",
        &json!({"type":"response","id":"1","success":true,
                "data":{"killed":false,"cleanupWorktree":true,"deleteBranch":false}}),
        "killed 非 true",
    );

    // list_workers：WorkerStatus 枚举 + byStatus
    let schema = compile_file(&dir.join("list_workers.json"));
    assert_valid(
        &schema,
        "list_workers",
        &json!({"type":"response","id":7,"success":true,"data":{
            "workers":[{"workerId":"wkr_ab12cd34","sessionId":"sess_x","project":"/p",
                        "status":"Idle","model":"m","agent":"build","parent":null,
                        "channels":["main"],"host":null}],
            "total":1,"byStatus":{"Idle":1}}}),
        "success 信封",
    );
    assert_invalid(
        &schema,
        "list_workers",
        &json!({"type":"response","id":7,"success":true,"data":{
            "workers":[{"workerId":"wkr_ab12cd34","sessionId":"s","project":"/p",
                        "status":"Paused","model":"m","agent":"build","parent":null,
                        "channels":[],"host":null}],
            "total":1,"byStatus":{}}}),
        "status=Paused（已删除的变体）",
    );

    // steer：data oneOf null | {status:'promoted'}
    let schema = compile_file(&dir.join("steer.json"));
    assert_valid(
        &schema,
        "steer",
        &json!({"type":"response","id":"9","command":"steer","success":true,"data":null}),
        "worker 信封 data=null",
    );
    assert_valid(
        &schema,
        "steer",
        &json!({"type":"response","id":"9","command":"steer","success":true,
                "data":{"status":"promoted"}}),
        "promoted 形状",
    );
    assert_invalid(
        &schema,
        "steer",
        &json!({"type":"response","id":"9","command":"steer","success":true,"data":{}}),
        "data 空对象（两个分支都不匹配）",
    );

    // get_agents：data 是裸数组
    let schema = compile_file(&dir.join("get_agents.json"));
    assert_valid(
        &schema,
        "get_agents",
        &json!({"type":"response","id":"1","command":"get_agents","success":true,
                "data":[{"name":"build","description":"d","color":"#fff","tier":"fast","source":"builtin"}]}),
        "裸数组 data",
    );
    assert_invalid(
        &schema,
        "get_agents",
        &json!({"type":"response","id":"1","command":"get_agents","success":true,"data":{}}),
        "对象不是数组",
    );
}

// ---------------------------------------------------------------------------
// 静态 4：$defs.params 正反样本（params 类型边界）
// ---------------------------------------------------------------------------

fn params_schema(dir: &std::path::Path, file: &str) -> Validator {
    let doc = read_json(&dir.join(file));
    let params = doc
        .pointer("/$defs/params")
        .unwrap_or_else(|| panic!("{file}: 缺 $defs/params"))
        .clone();
    jsonschema::validator_for(&params).expect("params 子 schema 可编译")
}

#[test]
fn static_params_positive_and_negative_samples() {
    let dir = schema_dir();

    // kill_worker：类型正确的 params 通过；cleanupWorktree 传字符串不通过
    let schema = params_schema(&dir, "kill_worker.json");
    assert_valid(&schema, "kill_worker", &json!({"workerId":"wkr_ab12cd34"}), "params 正样本");
    assert_invalid(
        &schema,
        "kill_worker",
        &json!({"workerId":"wkr_ab12cd34","cleanupWorktree":"yes"}),
        "cleanupWorktree 传字符串",
    );

    // create_worker：relation 枚举 + wait 类型
    let schema = params_schema(&dir, "create_worker.json");
    assert_valid(
        &schema,
        "create_worker",
        &json!({"agent":"developer","relation":"peer","wait":false,
                "worktree":{"branch":"ion-t"}}),
        "params 正样本",
    );
    assert_invalid(
        &schema,
        "create_worker",
        &json!({"relation":"cousin"}),
        "relation=cousin",
    );
    assert_invalid(
        &schema,
        "create_worker",
        &json!({"worktree":{}}),
        "worktree 缺 branch",
    );

    // list_workers：无参命令 params 空 object
    let schema = params_schema(&dir, "list_workers.json");
    assert_valid(&schema, "list_workers", &json!({}), "空 params");
}

// ---------------------------------------------------------------------------
// 动态：隔离 host 上的真实响应过 schema
// ---------------------------------------------------------------------------

struct HostGuard {
    child: Child,
    sock: PathBuf,
    #[allow(dead_code)]
    root: PathBuf,
}

impl Drop for HostGuard {
    fn drop(&mut self) {
        // 精确 PID 清理：只杀自己拉起的 host 子进程；worker 靠 ION_HOST_PID
        // 孤儿防护在 ≤30s 内自退（host 被 SIGKILL 后的场景）。
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.sock);
    }
}

fn find_ion_bin() -> PathBuf {
    if let Ok(p) = std::env::var("ION_WORKER_BIN") {
        return PathBuf::from(p);
    }
    let exe = std::env::current_exe().expect("current_exe");
    if let Some(dir) = exe.parent() {
        let sibling = dir.join("ion");
        if sibling.exists() {
            return sibling;
        }
        if let Some(gp) = dir.parent() {
            let alt = gp.join("ion");
            if alt.exists() {
                return alt;
            }
        }
    }
    panic!(
        "找不到 ion 二进制（先 `cargo build --bin ion`，或设 ION_WORKER_BIN）；\
         动态契约测试必须有真实 host"
    );
}

fn spawn_isolated_host() -> HostGuard {
    let ion = find_ion_bin();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    let root = std::env::temp_dir().join(format!("ion_rpc_schema_s2_{}_{}", std::process::id(), nanos));
    let home = root.join("home");
    std::fs::create_dir_all(home.join(".ion")).expect("私有 HOME");
    std::fs::create_dir_all(root.join("sessions")).expect("私有 sessions");
    std::fs::create_dir_all(root.join("worktree")).expect("私有 worktree");
    std::fs::create_dir_all(root.join("proj")).expect("私有 project");
    let sock = root.join("host.sock");

    let log = std::fs::File::create(root.join("host.log")).expect("host log");
    let err_log = log.try_clone().expect("clone log");
    let child = Command::new(&ion)
        .arg("serve")
        .env("HOME", &home) // 私有 HOME：绝不读写真实 ~/.ion
        .env("ION_HOST_SOCKET", &sock)
        .env("ION_SESSION_DIR", root.join("sessions"))
        .env("ION_WORKTREE_ROOT", root.join("worktree"))
        .env("ION_FAUX_REPLY", "rpc schema test ok")
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(err_log))
        .spawn()
        .expect("spawn ion serve");

    HostGuard { child, sock, root }
}

fn rpc_once(guard: &HostGuard, method: &str, params: Value, session: Option<&str>) -> Value {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    let id = next_id();
    let mut req = json!({"id": id, "method": method, "params": params});
    if let Some(sid) = session {
        req["session"] = json!(sid);
    }
    let line = format!("{req}\n");

    let deadline = std::time::Instant::now() + Duration::from_secs(RPC_TIMEOUT_SECS);
    let mut last_err = String::new();
    while std::time::Instant::now() < deadline {
        // 连接可能早于 socket 就绪——重试 connect
        let stream = match UnixStream::connect(&guard.sock) {
            Ok(s) => s,
            Err(e) => {
                last_err = format!("connect: {e}");
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(RPC_TIMEOUT_SECS)));
        let mut writer = stream.try_clone().expect("clone stream");
        writer.write_all(line.as_bytes()).expect("write rpc");
        let _ = writer.flush();

        let mut reader = BufReader::new(stream);
        let mut buf = String::new();
        loop {
            buf.clear();
            match reader.read_line(&mut buf) {
                Ok(0) => {
                    last_err = "host closed connection".into();
                    break;
                }
                Ok(_) => {
                    let trimmed = buf.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
                        // 跳过事件帧，只认带同 id 的响应
                        if v.get("id").and_then(|i| i.as_str()) == Some(id.as_str()) {
                            return v;
                        }
                    }
                    // 事件或其他 id 的帧：继续读
                }
                Err(e) => {
                    last_err = format!("read: {e}");
                    break;
                }
            }
        }
        // 本轮没拿到响应：重连重发（幂等只读命令安全；本测试不重发 prompt 类）
    }
    panic!("rpc {method} 超时（{RPC_TIMEOUT_SECS}s）：{last_err}");
}

/// 等 session 的 worker 出现且不再是注册初态（Idle/Busy 都算已就绪运行）
fn wait_worker_for_session(guard: &HostGuard, sid: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(HOST_READY_TIMEOUT_SECS);
    while std::time::Instant::now() < deadline {
        let resp = rpc_once(guard, "list_workers", json!({}), None);
        if resp["success"].as_bool() == Some(true) {
            if let Some(ws) = resp["data"]["workers"].as_array() {
                if ws.iter().any(|w| w["sessionId"].as_str() == Some(sid)) {
                    return;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(400));
    }
    panic!("session {sid} 的 worker 未在 {HOST_READY_TIMEOUT_SECS}s 内出现");
}

#[test]
fn dynamic_worker_domain_responses_match_schemas() {
    let dir = schema_dir();
    let guard = spawn_isolated_host();

    // host 就绪：list_sessions 可答
    {
        let deadline = std::time::Instant::now() + Duration::from_secs(HOST_READY_TIMEOUT_SECS);
        loop {
            if std::time::Instant::now() > deadline {
                panic!("host 在 {HOST_READY_TIMEOUT_SECS}s 内未就绪");
            }
            let resp = rpc_once(&guard, "list_sessions", json!({}), None);
            if resp["success"].as_bool() == Some(true) {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    // 1. list_sessions
    let schema = compile_file(&dir.join("list_sessions.json"));
    let resp = rpc_once(&guard, "list_sessions", json!({}), None);
    assert_valid(&schema, "list_sessions", &resp, "真实响应");

    // 2. list_workers
    let schema = compile_file(&dir.join("list_workers.json"));
    let resp = rpc_once(&guard, "list_workers", json!({}), None);
    assert_valid(&schema, "list_workers", &resp, "真实响应（空池）");

    // 3. stats
    let schema = compile_file(&dir.join("stats.json"));
    let resp = rpc_once(&guard, "stats", json!({}), None);
    assert_valid(&schema, "stats", &resp, "真实响应");

    // 4. health（Manager 级形状）
    let schema = compile_file(&dir.join("health.json"));
    let resp = rpc_once(&guard, "health", json!({}), None);
    assert_valid(&schema, "health", &resp, "真实响应");

    // 5. get_overview
    let schema = compile_file(&dir.join("get_overview.json"));
    let resp = rpc_once(&guard, "get_overview", json!({}), None);
    assert_valid(&schema, "get_overview", &resp, "真实响应");

    // 6. channel_send（顶层字段路径）
    let schema = compile_file(&dir.join("channel_send.json"));
    let resp = rpc_once(
        &guard,
        "channel_send",
        json!({"channel":"main","from":"schema-test","msg":{"text":"ping"}}),
        None,
    );
    assert_valid(&schema, "channel_send", &resp, "真实响应");

    // 7. channel_subscribe 对不存在的 worker → error 信封
    let schema = compile_file(&dir.join("channel_subscribe.json"));
    let resp = rpc_once(
        &guard,
        "channel_subscribe",
        json!({"channel":"main","workerId":"wkr_nonexistent"}),
        None,
    );
    assert_eq!(
        resp["success"].as_bool(),
        Some(false),
        "订阅不存在的 worker 应失败：{resp}"
    );
    assert_valid(&schema, "channel_subscribe", &resp, "error 信封");

    // 8. reap_workers
    let schema = compile_file(&dir.join("reap_workers.json"));
    let resp = rpc_once(&guard, "reap_workers", json!({"maxAgeSecs":0}), None);
    assert_valid(&schema, "reap_workers", &resp, "真实响应");

    // 9. create_session → worker 起来后 session 域命令
    let schema_cs = compile_file(&dir.join("create_session.json"));
    let sid = format!("sess_schema_{:08x}", std::process::id() as u32 % 0xffff_ffff);
    let resp = rpc_once(
        &guard,
        "create_session",
        json!({"session_id": sid, "agent": "build", "project_path": guard.root.join("proj")}),
        None,
    );
    assert_eq!(resp["success"].as_bool(), Some(true), "create_session 失败：{resp}");
    assert_valid(&schema_cs, "create_session", &resp, "真实响应");
    wait_worker_for_session(&guard, &sid);

    // 10. get_state（worker 级信封，经 session 转发）
    let schema = compile_file(&dir.join("get_state.json"));
    let resp = rpc_once(&guard, "get_state", json!({}), Some(&sid));
    assert_eq!(resp["success"].as_bool(), Some(true), "get_state 失败：{resp}");
    assert_valid(&schema, "get_state", &resp, "真实响应");

    // 11. steer（data=null 分支）
    let schema = compile_file(&dir.join("steer.json"));
    let resp = rpc_once(&guard, "steer", json!({"text":"schema test steer"}), Some(&sid));
    assert_eq!(resp["success"].as_bool(), Some(true), "steer 失败：{resp}");
    assert_valid(&schema, "steer", &resp, "真实响应");

    // 12. follow_up + remove_follow_up + remove_steering（队列水位回显）
    let resp = rpc_once(&guard, "follow_up", json!({"text":"fu-1"}), Some(&sid));
    let schema = compile_file(&dir.join("follow_up.json"));
    assert_valid(&schema, "follow_up", &resp, "真实响应");

    let resp = rpc_once(&guard, "remove_follow_up", json!({"index":0}), Some(&sid));
    let schema = compile_file(&dir.join("remove_follow_up.json"));
    assert_valid(&schema, "remove_follow_up", &resp, "真实响应");

    let resp = rpc_once(&guard, "remove_steering", json!({"index":0}), Some(&sid));
    let schema = compile_file(&dir.join("remove_steering.json"));
    assert_valid(&schema, "remove_steering", &resp, "真实响应");

    // 13. abort（data=null）
    let schema = compile_file(&dir.join("abort.json"));
    let resp = rpc_once(&guard, "abort", json!({}), Some(&sid));
    assert_valid(&schema, "abort", &resp, "真实响应");

    // 14. create_worker（host 级本地 worker）+ get_agents / get_current_agent
    let schema_cw = compile_file(&dir.join("create_worker.json"));
    let resp = rpc_once(
        &guard,
        "create_worker",
        json!({"agent":"build","project_path": guard.root.join("proj").to_string_lossy()}),
        None,
    );
    assert_eq!(resp["success"].as_bool(), Some(true), "create_worker 失败：{resp}");
    assert_valid(&schema_cw, "create_worker", &resp, "真实响应");
    let spawned = resp["data"]["workerId"].as_str().expect("workerId").to_string();

    let resp = rpc_once(&guard, "get_agents", json!({}), Some(&sid));
    let schema = compile_file(&dir.join("get_agents.json"));
    assert_valid(&schema, "get_agents", &resp, "真实响应");

    let resp = rpc_once(&guard, "get_current_agent", json!({}), Some(&sid));
    let schema = compile_file(&dir.join("get_current_agent.json"));
    assert_valid(&schema, "get_current_agent", &resp, "真实响应");

    // 15. list_workers 再验（此时 ≥2 个 worker）
    let schema = compile_file(&dir.join("list_workers.json"));
    let resp = rpc_once(&guard, "list_workers", json!({}), None);
    assert_valid(&schema, "list_workers", &resp, "真实响应（有 worker）");
    let total = resp["data"]["total"].as_u64().unwrap_or(0);
    assert!(total >= 2, "期望 ≥2 个 worker：total={total}");

    // 16. kill_worker（创建出来的 + session 的）——同时是清理
    let schema = compile_file(&dir.join("kill_worker.json"));
    let resp = rpc_once(
        &guard,
        "kill_worker",
        json!({"workerId": spawned, "cleanupWorktree": true, "deleteBranch": false}),
        None,
    );
    assert_eq!(resp["success"].as_bool(), Some(true), "kill_worker 失败：{resp}");
    assert_valid(&schema, "kill_worker", &resp, "真实响应");

    // 收尾：把 session worker 也杀掉（复用 create_session 会话的 worker_id 需要
    // list_workers 查一遍；顺带对 error 信封做一次 channel_subscribe 反验已做过）
    let resp = rpc_once(&guard, "list_workers", json!({}), None);
    if let Some(ws) = resp["data"]["workers"].as_array() {
        for w in ws {
            if w["sessionId"].as_str() == Some(sid.as_str()) {
                let wid = w["workerId"].as_str().unwrap_or_default().to_string();
                let r = rpc_once(&guard, "kill_worker", json!({"workerId": wid}), None);
                assert_valid(&schema, "kill_worker", &r, "清理路径响应");
            }
        }
    }

    // guard Drop：精确 PID kill host + 删 socket；worker 靠孤儿防护自退
}
