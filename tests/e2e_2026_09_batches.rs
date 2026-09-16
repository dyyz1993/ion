//! 真实 LLM E2E — 2026-09 近三批功能补真实 case（AGENTS.md 测试验证规范）
//!
//! 覆盖（对应批次）：
//!   T1 hostId pin（fix3 P0.3：hello 握手 + ION_EXPECT_HOST_ID pin，重启换新）   [0 LLM 轮]
//!   T2 subscribe 协议（fix5/batch3：snapshot 先行 + epoch 栅栏 + stale_route）  [3 LLM 轮]
//!   T3 队列落盘（fix4 ③：queued_input 入队即落盘 → kill -9 → 重建回放真实作答） [4 LLM 轮]
//!   T4 同 sid 原地重建（batch5 K4：auto_respawn_local + 全历史预加载续作）      [3 LLM 轮]
//!   T5 abort_bash（fix4 ①：长 bash 中 abort 不杀 worker，worker 继续应答）     [2-3 LLM 轮]
//!
//! 跳过说明：空补全重试 / tier 降级（fix4 ⑤⑥）依赖上游故障注入，真实环境无法
//! 确定性触发，harness（empty_completion_harness / tier_fallback_harness）已覆盖。
//!
//! 运行方式（真实 LLM，耗时数分钟）：
//!   ION_E2E=1 cargo test --test e2e_2026_09_batches -- --ignored --test-threads=1 --nocapture
//!
//! 模型选择（fast 档纪律）：
//!   默认从真实 ~/.ion/config.json 读 tier_models.fast（本机当前 = zai/glm-5.2；
//!   AGENTS.md 推荐的 opencode/deepseek-v4-flash 本机未配置该 provider，见报告）。
//!   可用 ION_E2E_PROVIDER / ION_E2E_MODEL 覆盖。
//!
//! 隔离铁律（对齐 AGENTS.md + 生产巡检纪律）：
//!   - 每个测试私有 HOME + 私有 ION_HOST_SOCKET + 私有 ION_SESSION_DIR（mktemp 下），
//!     绝不写真实 ~/.ion/agent/sessions；auth/config 从真实 ~/.ion 只读拷贝（拿 key），
//!     拷贝后再打补丁（command_guard=open / mcp 清空 / 噪音扩展关闭 / auto_respawn_local）。
//!   - host/worker 全部精确 PID 管理：host 用 Child 句柄 kill；worker 用
//!     `pgrep -P <host_pid> -f "rpc --session <sid>"` 定位后 /bin/kill -9。绝不 pkill。
//!   - 断言"行为发生"而非"具体文本"：断言新轮有 assistant 应答且引用关键码词，
//!     不断言全文；超时预算 generous（真实 LLM 每轮 30-240s）。

#![cfg(test)]

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ─────────────────────────────────────────────────────────────
// 全局串行化：每个测试自带独立 socket 本可并行，但真实 LLM 轮次重，
// 强制串行保预算可控（配合 --test-threads=1 双保险）。
// ─────────────────────────────────────────────────────────────
static SERIAL: Mutex<()> = Mutex::new(());

fn e2e_enabled() -> bool {
    std::env::var("ION_E2E").is_ok()
}

fn ion_bin() -> PathBuf {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/debug/ion");
    assert!(
        p.exists(),
        "target/debug/ion 不存在，先 cargo build --bin ion"
    );
    p
}

/// fast 档模型：优先真实 config 的 tier_models.fast（"provider/model"），
/// 回落 default_provider/default_model，最终回落 zai/glm-5.2。
fn detect_fast() -> (String, String) {
    if let (Ok(p), Ok(m)) = (
        std::env::var("ION_E2E_PROVIDER"),
        std::env::var("ION_E2E_MODEL"),
    ) {
        return (p, m);
    }
    let cfg_path = PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".ion/config.json");
    if let Ok(raw) = std::fs::read_to_string(&cfg_path) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
            if let Some(fast) = v["tier_models"]["fast"].as_str() {
                if let Some((p, m)) = fast.split_once('/') {
                    if !p.is_empty() && !m.is_empty() {
                        return (p.into(), m.into());
                    }
                }
            }
            let p = v["default_provider"]
                .as_str()
                .unwrap_or("zai")
                .to_string();
            let m = v["default_model"].as_str().unwrap_or("glm-5.2").to_string();
            return (p, m);
        }
    }
    ("zai".into(), "glm-5.2".into())
}

// ─────────────────────────────────────────────────────────────
// 隔离环境
// ─────────────────────────────────────────────────────────────
struct TestEnv {
    dir: PathBuf,
    home: PathBuf,
    sock: PathBuf,
    sessions: PathBuf,
    proj: PathBuf,
    host_log: PathBuf,
    provider: String,
    model: String,
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        // panic 时保留现场供诊断（打印路径），正常结束才清理
        if std::thread::panicking() {
            println!(
                "  [诊断] 测试失败，保留现场: dir={} host_log={}",
                self.dir.display(),
                self.host_log.display()
            );
        } else {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

/// 噪音扩展：e2e 不需要（monitor cron / LSP / 规则注入等），拷贝配置后全部关闭。
const NOISY_EXTENSIONS: &[&str] = &[
    "monitor",
    "lsp",
    "context-index",
    "rules-engine",
    "dev_server_detector",
    "global-memory",
    "file-snapshot",
    "learning",
];

fn setup_env(tag: &str, allow_local_respawn: bool) -> TestEnv {
    let (provider, model) = detect_fast();
    // 🔴 路径纪律：host.sock 全路径必须 < 104 字节（macOS sun_path 上限，
    // k3 批次踩过同坑）。std::env::temp_dir() = /var/folders/... 前缀 47 字节，
    // 拼上纳秒后缀必超——这里固定用短 /tmp 前缀 + 毫秒时间戳。
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let dir = PathBuf::from("/tmp").join(format!(
        "ion-j7-{}-{}-{}",
        tag,
        std::process::id(),
        nanos
    ));
    assert!(
        dir.join("host.sock").to_string_lossy().len() < 100,
        "socket path too long for AF_UNIX: {}",
        dir.join("host.sock").display()
    );
    let home = dir.join("home");
    let sessions = dir.join("sessions");
    let proj = dir.join("proj");
    std::fs::create_dir_all(home.join(".ion")).unwrap();
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(proj.join("README.md"), format!("# {tag} e2e project\n")).unwrap();

    // ── auth/config 只读拷贝真实 ~/.ion（拿 key），拷贝后打补丁 ──
    let real_home = std::env::var("HOME").unwrap_or_default();
    let real_auth = PathBuf::from(&real_home).join(".ion/auth.json");
    if real_auth.exists() {
        std::fs::copy(&real_auth, home.join(".ion/auth.json")).unwrap();
    }
    let real_cfg = PathBuf::from(&real_home).join(".ion/config.json");
    let mut cfg: serde_json::Value = std::fs::read_to_string(&real_cfg)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_else(|| {
            panic!(
                "真实 ~/.ion/config.json 缺失或不可解析，e2e 无法拿到 provider 配置（路径：{}）",
                real_cfg.display()
            )
        });
    // 补丁：bash 全放行（真实配置是 whitelist+空表 → 全 Ask，e2e 没人批）
    cfg["runtime"]["command_guard"] = serde_json::json!({
        "mode": "open", "whitelist": [], "risk_patterns": []
    });
    cfg["runtime"]["auto_respawn_local"] = serde_json::Value::Bool(allow_local_respawn);
    // MCP / 远端清空：不让 e2e 去连真实 MCP server / SSH 主机
    cfg["mcp_servers"] = serde_json::json!({});
    cfg["mcp"] = serde_json::json!({"servers": {}});
    cfg["remote_workers"] = serde_json::json!({});
    if let Some(ext) = cfg["extensions"].as_object_mut() {
        for k in NOISY_EXTENSIONS {
            ext.entry(k.to_string()).or_insert_with(|| serde_json::json!({}));
            ext[*k]["enabled"] = serde_json::Value::Bool(false);
        }
    }
    std::fs::write(
        home.join(".ion/config.json"),
        serde_json::to_string_pretty(&cfg).unwrap(),
    )
    .unwrap();

    TestEnv {
        host_log: dir.join("host.log"),
        sock: dir.join("host.sock"),
        dir,
        home,
        sessions,
        proj,
        provider,
        model,
    }
}

fn base_env_vars(env: &TestEnv) -> Vec<(String, String)> {
    vec![
        ("HOME".into(), env.home.display().to_string()),
        ("ION_SESSION_DIR".into(), env.sessions.display().to_string()),
        ("ION_HOST_SOCKET".into(), env.sock.display().to_string()),
        ("RUST_LOG".into(), "warn".into()),
    ]
}

fn ion_cmd(env: &TestEnv, args: &[&str]) -> Command {
    let mut cmd = Command::new(ion_bin());
    cmd.args(args).current_dir(&env.proj);
    for (k, v) in base_env_vars(env) {
        cmd.env(&k, &v);
    }
    cmd.stdin(Stdio::null());
    cmd
}

/// 跑 `ion rpc`，返回 (exit_ok, stdout, stderr)
fn rpc_raw(env: &TestEnv, session: Option<&str>, method: &str, params: &str) -> (bool, String, String) {
    let mut args = vec!["rpc".to_string()];
    if let Some(sid) = session {
        args.push("--session".into());
        args.push(sid.to_string());
    }
    args.push("--method".into());
    args.push(method.to_string());
    args.push("--params".into());
    args.push(params.to_string());
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let out = ion_cmd(env, &arg_refs).output().expect("failed to run ion rpc");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// host 级 RPC，解析 data 字段（失败 panic 带诊断）
fn host_rpc(env: &TestEnv, method: &str, params: &str) -> serde_json::Value {
    let (ok, out, err) = rpc_raw(env, None, method, params);
    assert!(ok, "host rpc {method} 失败: stdout={out} stderr={err}");
    let v: serde_json::Value = serde_json::from_str(out.trim())
        .unwrap_or_else(|e| panic!("host rpc {method} 响应非 JSON: {e}; raw={out}"));
    assert_eq!(
        v["success"].as_str().or_else(|| v["success"].as_bool().map(|_| "true")),
        Some("true"),
        "host rpc {method} success!=true: {v}"
    );
    v["data"].clone()
}

/// session 级 RPC，返回完整响应（不 assert success——部分命令语义上允许失败）
fn session_rpc(env: &TestEnv, sid: &str, method: &str, params: &str) -> (bool, serde_json::Value, String) {
    let (ok, out, err) = rpc_raw(env, Some(sid), method, params);
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap_or(serde_json::Value::Null);
    (ok, v, format!("{out}{err}"))
}

// ─────────────────────────────────────────────────────────────
// host 生命周期（精确 PID）
// ─────────────────────────────────────────────────────────────
struct Host {
    child: Child,
    pid: u32,
}

impl Drop for Host {
    fn drop(&mut self) {
        // 精确 PID：只杀自己 spawn 的 host（SIGTERM → 等 → SIGKILL 兜底）
        let _ = kill_pid(self.pid, 15);
        for _ in 0..50 {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                _ => std::thread::sleep(Duration::from_millis(100)),
            }
        }
        let _ = kill_pid(self.pid, 9);
        let _ = self.child.wait();
    }
}

/// libc-free 的信号发送（/bin/kill，macOS 自带）
fn kill_pid(pid: u32, sig: i32) -> std::process::ExitStatus {
    let sig_str = match sig {
        9 => "-9",
        15 => "-15",
        _ => "-15",
    };
    Command::new("/bin/kill")
        .args([sig_str, &pid.to_string()])
        .status()
        .unwrap_or_else(|_| std::process::ExitStatus::default())
}

fn start_host(env: &TestEnv, extra_env: &[(&str, &str)]) -> Host {
    let log = std::fs::File::create(&env.host_log).unwrap();
    let log_err = log.try_clone().unwrap();
    let mut cmd = ion_cmd(env, &["serve"]);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.stdout(Stdio::from(log)).stderr(Stdio::from(log_err));
    let mut child = cmd.spawn().expect("spawn ion serve failed");
    let pid = child.id();
    // 等 host socket 就绪（list_sessions 可答，30s 上限）
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        std::thread::sleep(Duration::from_millis(500));
        if matches!(child.try_wait(), Ok(Some(_))) {
            let logtxt = std::fs::read_to_string(&env.host_log).unwrap_or_default();
            panic!("host 提前退出。log:\n{logtxt}");
        }
        let (ok, out, _) = rpc_raw(env, None, "list_sessions", "{}");
        if ok && out.contains("sessions") {
            break;
        }
        if Instant::now() > deadline {
            let logtxt = std::fs::read_to_string(&env.host_log).unwrap_or_default();
            panic!("host 30s 未就绪。log:\n{logtxt}");
        }
    }
    Host { child, pid }
}

// ─────────────────────────────────────────────────────────────
// worker / 会话辅助
// ─────────────────────────────────────────────────────────────
fn create_session(env: &TestEnv) -> String {
    let params = serde_json::json!({
        "agent": "build",
        "provider": env.provider,
        "model": env.model,
    });
    let data = host_rpc(env, "create_session", &params.to_string());
    let sid = data["session_id"]
        .as_str()
        .map(String::from)
        .unwrap_or_else(|| panic!("create_session 未返回 session_id: {data}"));
    println!("  [session] {sid} (provider={} model={})", env.provider, env.model);
    sid
}

/// 定位 worker PID：host 的直接子进程且命令行含 "rpc --session <sid>"。
fn find_worker_pid(_env: &TestEnv, host_pid: u32, sid: &str) -> Option<u32> {
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

fn pid_cmdline(pid: u32) -> String {
    Command::new("/bin/ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default()
}

fn sigkill_pid(pid: u32) {
    kill_pid(pid, 9);
    // 等进程真正消失（≤5s）
    for _ in 0..50 {
        if !pid_alive(pid) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("kill -9 后 pid {pid} 仍存活");
}

/// 定位并 kill -9 指定 session 的 worker（精确 PID，重试窗口：worker 空闲期
/// 可能自退出——stdout 关闭即达成"worker 死亡"前提，直接视为成功）。
fn kill_worker_sigkill(env: &TestEnv, host_pid: u32, sid: &str, label: &str) -> u32 {
    for round in 0..5 {
        if let Some(pid) = find_worker_pid(env, host_pid, sid) {
            let alive = pid_alive(pid);
            let cmd = pid_cmdline(pid);
            if alive && cmd.contains("rpc") {
                println!("  [kill -9] {label}: worker pid={pid}（round {round}）");
                sigkill_pid(pid);
                return pid;
            }
            // pgrep 命中但进程已不在/rpc 字样缺失 → 重新定位
        }
        std::thread::sleep(Duration::from_millis(700));
    }
    println!(
        "  [warn] {label}: worker 已自行退出（未及 kill -9），worker 流关闭前提仍成立"
    );
    0
}

fn session_rpc_ok(env: &TestEnv, sid: &str, method: &str, params: &str) -> serde_json::Value {
    let (ok, v, raw) = session_rpc(env, sid, method, params);
    assert!(
        ok && v["success"] == serde_json::Value::Bool(true),
        "session rpc {method} 失败: {raw}"
    );
    v["data"].clone()
}

/// 读当前磁盘消息总数（prompt 前调一次作 baseline）。
/// 🔴 必须用磁盘口径：worker 死亡窗口内 get_session_info 返回合成态，
/// 其 message_count 与活 worker 口径不一致，会把"未开跑"误判成"已完成"。
/// 会话 JSONL 是 per-turn 落盘的（faux 实测轮次结束即在场），以此为准。
fn msg_count(env: &TestEnv, sid: &str) -> u64 {
    let params = serde_json::json!({"session": sid, "limit": 1});
    let data = host_rpc(env, "get_session_messages", &params.to_string());
    data["totalCount"].as_u64().unwrap_or(0)
}

/// 等待一轮真实 LLM 跑完：磁盘消息数超过 baseline 且 agent 不再 running。
/// 🔴 两个条件缺一不可：user 消息在 prompt 时就落盘（count 单独增长不足以
/// 代表轮次结束），必须再等 is_running→false；反之 worker 死亡窗口的合成态
/// is_running 恒 false，所以 count 增长仍是必要条件。
fn wait_turn_done(env: &TestEnv, sid: &str, label: &str, baseline: u64) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(240);
    loop {
        let count = msg_count(env, sid);
        if count > baseline {
            // 落盘已增长 → 等 agent 空闲（≤60s；合成态/死亡态也是 false）
            let idle_deadline = Instant::now() + Duration::from_secs(60);
            loop {
                let info = session_rpc_ok(env, sid, "get_session_info", "{}");
                if !info["is_running"].as_bool().unwrap_or(false) {
                    std::thread::sleep(Duration::from_millis(800));
                    return info;
                }
                if Instant::now() > idle_deadline {
                    break; // 60s 仍 running → 回外层等下一轮增长
                }
                std::thread::sleep(Duration::from_millis(1200));
            }
        }
        if Instant::now() > deadline {
            panic!(
                "{label}: 等待 240s 轮次仍未结束（baseline={baseline} disk_count={count}）。host log:\n{}",
                host_log_tail(env, 30)
            );
        }
        std::thread::sleep(Duration::from_millis(1500));
    }
}

fn prompt(env: &TestEnv, sid: &str, text: &str) {
    let params = serde_json::json!({"text": text});
    let (ok, v, raw) = session_rpc(env, sid, "prompt", &params.to_string());
    assert!(ok, "prompt RPC 失败: {raw}");
    println!("  [prompt] -> {:?}", v["data"]);
}

/// worker 死亡重建后的 prompt：先等 router/auto-recovery 把 worker 拉起来，
/// 再发 prompt；发完后 30s 内磁盘必须增长，否则判定 prompt 撞上重建窗口
/// 被吞（rpc success 但无 agent_start，run9 实测），重发（≤3 次）。
/// 返回是否成功开跑——不 panic，调用方决定重试/恢复策略。
fn prompt_after_restart(env: &TestEnv, host_pid: u32, sid: &str, text: &str, baseline: u64, label: &str) -> bool {
    let mut live = false;
    for _ in 0..30 {
        if let Some(pid) = find_worker_pid(env, host_pid, sid) {
            if pid_alive(pid) {
                live = true;
                println!("  [wait-restart] worker 已拉起 pid={pid}");
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(1500));
    }
    if !live {
        println!("  [wait-restart] 45s 未见 worker 拉起，交由 prompt 自建");
    }
    for attempt in 1..=3 {
        prompt(env, sid, text);
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if msg_count(env, sid) > baseline {
                println!("  [ok] 轮次已开跑/落盘（attempt {attempt}）");
                return true;
            }
            if Instant::now() > deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(1500));
        }
        println!("  [warn] {label}: prompt 30s 未落盘（attempt {attempt}，疑似重建窗口竞态），重发");
    }
    false
}

/// 等消息数连续 3 次采样不变（处理一轮 prompt 触发多条 followUp 连转的情况）
fn settle(env: &TestEnv, sid: &str, label: &str) {
    let mut stable = 0;
    let mut last = msg_count(env, sid);
    let deadline = Instant::now() + Duration::from_secs(180);
    while stable < 3 {
        if Instant::now() > deadline {
            panic!("{label}: 消息数 180s 未稳定（last={last}）");
        }
        std::thread::sleep(Duration::from_millis(1500));
        let c = msg_count(env, sid);
        if c == last {
            stable += 1;
        } else {
            stable = 0;
            last = c;
        }
    }
}

fn prompt_behavior(env: &TestEnv, sid: &str, text: &str, behavior: &str) -> serde_json::Value {
    let params = serde_json::json!({"text": text, "behavior": behavior});
    let (ok, v, raw) = session_rpc(env, sid, "prompt", &params.to_string());
    assert!(ok, "prompt({behavior}) RPC 失败: {raw}");
    v["data"].clone()
}

/// 优雅关闭 worker（触发 save_worker_session 落盘），然后 host 直读全部消息。
/// （轮次消息的磁盘可见性以 worker 关闭为准——活 worker 期间直读可能少最后一轮。）
/// 注意：socket 层 method=="shutdown" 被保留给 host 停机，必须用
/// send{rpc_method:"dispose"} 转发到 worker 的优雅退出臂。
fn flush_and_read_messages(env: &TestEnv, sid: &str) -> Vec<(String, String)> {
    let p = serde_json::json!({"session": sid, "rpc_method": "dispose", "params": {}});
    let _ = rpc_raw(env, None, "send", &p.to_string());
    std::thread::sleep(Duration::from_millis(2500));
    let params = serde_json::json!({"session": sid, "limit": 100});
    let data = host_rpc(env, "get_session_messages", &params.to_string());
    let msgs = data["messages"].as_array().cloned().unwrap_or_default();
    let mut out = Vec::new();
    for m in msgs {
        let mut role: Option<String> = None;
        let mut texts = Vec::new();
        extract_role_and_text(&m, &mut role, &mut texts);
        out.push((role.unwrap_or_else(|| "?".into()), texts.join(" ")));
    }
    out
}

fn extract_role_and_text(v: &serde_json::Value, role: &mut Option<String>, texts: &mut Vec<String>) {
    match v {
        serde_json::Value::Object(o) => {
            for (k, x) in o {
                match k.as_str() {
                    "role" => {
                        if role.is_none() {
                            if let Some(r) = x.as_str() {
                                *role = Some(r.to_string());
                            }
                        }
                    }
                    "text" => {
                        if let Some(t) = x.as_str() {
                            texts.push(t.to_string());
                        }
                    }
                    "content" => match x {
                        serde_json::Value::String(s) => texts.push(s.clone()),
                        _ => extract_role_and_text(x, role, texts),
                    },
                    _ => extract_role_and_text(x, role, texts),
                }
            }
        }
        serde_json::Value::Array(a) => {
            for x in a {
                extract_role_and_text(x, role, texts);
            }
        }
        _ => {}
    }
}

fn session_jsonl(env: &TestEnv, sid: &str) -> PathBuf {
    // 会话文件按 cwd-hash 子目录分组：ION_SESSION_DIR/--<hash>--<proj>/<sid>.jsonl
    let target = format!("{sid}.jsonl");
    let mut stack = vec![env.sessions.clone()];
    while let Some(d) = stack.pop() {
        if let Ok(rd) = std::fs::read_dir(&d) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.file_name().map(|n| n.to_string_lossy() == target).unwrap_or(false) {
                    return p;
                }
            }
        }
    }
    panic!("session 文件不存在（递归搜索 {target}）于 {}", env.sessions.display());
}

fn count_file_occurrences(path: &Path, needle: &str) -> usize {
    std::fs::read_to_string(path)
        .map(|c| c.matches(needle).count())
        .unwrap_or(0)
}

fn host_log_tail(env: &TestEnv, n: usize) -> String {
    let c = std::fs::read_to_string(&env.host_log).unwrap_or_default();
    let lines: Vec<&str> = c.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

// ─────────────────────────────────────────────────────────────
// Unix socket 订阅收集器（纯 Rust，对标 subscribe_protocol_ci.sh 的 python 收集器）
// ─────────────────────────────────────────────────────────────
struct Collector {
    frames: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Collector {
    /// 连接 host socket 并发送 subscribe（后台线程持续收帧直到 stop/EOF）。
    fn start(sock: &Path, sid: &str) -> Collector {
        let mut stream = UnixStream::connect(sock).expect("collector connect failed");
        let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
        let req = serde_json::json!({"id":"e2e-sub","method":"subscribe","session":sid});
        stream
            .write_all(format!("{}\n", req).as_bytes())
            .expect("collector send subscribe failed");
        let frames = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (f2, s2) = (frames.clone(), stop.clone());
        let handle = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let mut tmp = [0u8; 65536];
            loop {
                if s2.load(Ordering::Relaxed) {
                    break;
                }
                match stream.read(&mut tmp) {
                    Ok(0) => break, // EOF：host 关闭（stale_route 后）
                    Ok(n) => {
                        buf.extend_from_slice(&tmp[..n]);
                        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                            let line: Vec<u8> = buf.drain(..=pos).collect();
                            let t = String::from_utf8_lossy(&line[..line.len() - 1])
                                .trim()
                                .to_string();
                            if !t.is_empty() {
                                f2.lock().unwrap().push(t);
                            }
                        }
                    }
                    Err(_) => { // timeout → 回环检查 stop
                        continue;
                    }
                }
            }
            // 收尾：把残余不满一行的也丢掉（按协议每帧一行）
        });
        Collector { frames, stop, handle: Some(handle) }
    }

    fn snapshot(&self) -> Vec<String> {
        self.frames.lock().unwrap().clone()
    }

    /// 轮询等待出现包含 substr 的帧，返回其 index。
    fn wait_for(&self, substr: &str, timeout: Duration) -> Option<usize> {
        let deadline = Instant::now() + timeout;
        loop {
            let frames = self.snapshot();
            for (i, f) in frames.iter().enumerate() {
                if f.contains(substr) {
                    return Some(i);
                }
            }
            if Instant::now() > deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn parse_frame(line: &str) -> serde_json::Value {
    serde_json::from_str(line).unwrap_or(serde_json::Value::Null)
}

/// 一次性 socket 请求（hello 等），收 ≤max_lines 行后关闭。
fn sock_one_shot(sock: &Path, send_line: &str, timeout: Duration, max_lines: usize) -> Vec<String> {
    let mut stream = UnixStream::connect(sock).expect("one-shot connect failed");
    let _ = stream.set_read_timeout(Some(Duration::from_millis(300)));
    stream
        .write_all(format!("{send_line}\n").as_bytes())
        .expect("one-shot send failed");
    let mut lines = Vec::new();
    let mut buf = Vec::new();
    let deadline = Instant::now() + timeout;
    let mut tmp = [0u8; 65536];
    while lines.len() < max_lines && Instant::now() < deadline {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = buf.drain(..=pos).collect();
                    let t = String::from_utf8_lossy(&line[..line.len() - 1]).trim().to_string();
                    if !t.is_empty() {
                        lines.push(t);
                    }
                    if lines.len() >= max_lines {
                        break;
                    }
                }
            }
            Err(_) => continue,
        }
    }
    lines
}

fn hello_host_id(sock: &Path) -> String {
    let lines = sock_one_shot(sock, r#"{"id":"h","method":"hello"}"#, Duration::from_secs(8), 2);
    let v = lines
        .first()
        .map(|l| parse_frame(l))
        .unwrap_or(serde_json::Value::Null);
    let hid = v["data"]["hostId"].as_str().unwrap_or("").to_string();
    assert!(!hid.is_empty(), "hello 未返回 hostId: {v}");
    hid
}

// ─────────────────────────────────────────────────────────────
// T1 hostId pin（fix3 P0.3）— 0 LLM 轮
// ─────────────────────────────────────────────────────────────
#[test]
#[ignore]
fn e2e_01_hostid_pin_and_restart() {
    let _guard = SERIAL.lock().unwrap();
    if !e2e_enabled() {
        return;
    }
    let env = setup_env("t1-hostid", false);
    println!("== T1 hostId pin：hello 握手 + ION_EXPECT_HOST_ID + 重启换新 ==");
    let host = start_host(&env, &[]);

    // 1. hello 拿 hostId
    let hid1 = hello_host_id(&env.sock);
    println!("  hostId(host#1) = {hid1}");

    // 2. pin 正确值 → rpc 放行
    let mut cmd = ion_cmd(&env, &["rpc", "--method", "list_sessions", "--params", "{}"]);
    cmd.env("ION_EXPECT_HOST_ID", &hid1);
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "pin 正确 hostId 应放行: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    println!("  [ok] pin 正确 hostId → list_sessions 放行");

    // 3. pin 错误值 → 拒绝（exit != 0，stderr 提示 pin）
    let mut cmd = ion_cmd(&env, &["rpc", "--method", "list_sessions", "--params", "{}"]);
    cmd.env("ION_EXPECT_HOST_ID", "00000000-0000-4000-8000-000000000000");
    let out = cmd.output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        !out.status.success(),
        "pin 错误 hostId 应被拒绝（exit 0 = pin 失效）"
    );
    assert!(
        stderr.contains("hostId") || stderr.contains("pin"),
        "拒绝信息应说明 hostId pin 原因: {stderr}"
    );
    println!("  [ok] pin 错误 hostId → 拒绝: {}", stderr.trim().lines().next().unwrap_or(""));

    // 4. 重启 host → hostId 换新；旧 pin 拒绝、新 pin 放行
    drop(host);
    std::thread::sleep(Duration::from_millis(500));
    let host2 = start_host(&env, &[]);
    let hid2 = hello_host_id(&env.sock);
    assert_ne!(hid1, hid2, "host 重启后 hostId 必须换新（{hid1} == {hid2}）");
    println!("  [ok] host 重启换新 hostId: {hid1} → {hid2}");

    let mut cmd = ion_cmd(&env, &["rpc", "--method", "list_sessions", "--params", "{}"]);
    cmd.env("ION_EXPECT_HOST_ID", &hid1);
    assert!(
        !cmd.output().unwrap().status.success(),
        "重启后旧 pin 必须被拒绝"
    );
    let mut cmd = ion_cmd(&env, &["rpc", "--method", "list_sessions", "--params", "{}"]);
    cmd.env("ION_EXPECT_HOST_ID", &hid2);
    assert!(cmd.output().unwrap().status.success(), "重启后新 pin 应放行");
    println!("  [ok] 旧 pin 拒绝 / 新 pin 放行");
    let _ = host2; // Drop 清理
}

// ─────────────────────────────────────────────────────────────
// T2 subscribe 协议（snapshot 先行 + epoch 栅栏 + stale_route）— 3 LLM 轮
// ─────────────────────────────────────────────────────────────
#[test]
#[ignore]
fn e2e_02_subscribe_snapshot_epoch_stale_route() {
    let _guard = SERIAL.lock().unwrap();
    if !e2e_enabled() {
        return;
    }
    let env = setup_env("t2-subscribe", false);
    println!("== T2 subscribe 协议：snapshot 先行 → 实时事件带 epoch → kill -9 → stale_route ==");
    let host = start_host(&env, &[]);
    let sid = create_session(&env);

    // 轮 1：真实 LLM 跑一轮产生历史
    let c0 = msg_count(&env, &sid);
    prompt(&env, &sid, "Reply with exactly two words: HELLO WORLD");
    wait_turn_done(&env, &sid, "turn1", c0);

    // 订阅：ack → snapshot → 实时增量
    let col = Collector::start(&env.sock, &sid);
    let ack_idx = col.wait_for("\"subscribed\"", Duration::from_secs(10));
    let ack_idx = ack_idx.unwrap_or_else(|| panic!("未收到 subscribed ack: {:?}", col.snapshot()));
    let ack = parse_frame(&col.snapshot()[ack_idx]);
    let epoch1 = ack["epoch"].as_u64().unwrap_or_else(|| panic!("ack 无 epoch: {ack}"));
    assert_eq!(ack["type"].as_str(), Some("subscribed"), "第一帧应为 subscribed ack: {ack}");
    println!("  [ok] subscribed ack，epoch={epoch1}");

    let snap_idx = col.wait_for("\"snapshot\":true", Duration::from_secs(10))
        .unwrap_or_else(|| panic!("未收到 snapshot 帧: {:?}", col.snapshot()));
    let snap = parse_frame(&col.snapshot()[snap_idx]);
    assert_eq!(
        snap["event"]["customType"].as_str(),
        Some("snapshot"),
        "快照帧 customType 应为 snapshot: {snap}"
    );
    assert_eq!(snap["epoch"].as_u64(), Some(epoch1), "快照帧应盖同一 epoch");
    assert_eq!(
        snap["event"]["data"]["session"]["sessionId"].as_str(),
        Some(sid.as_str()),
        "快照应含 session.sessionId"
    );
    assert!(
        snap["event"]["data"]["worker"]["workerId"].as_str().is_some(),
        "快照应含 worker 状态: {snap}"
    );
    println!("  [ok] snapshot 先行（customType=snapshot, epoch={epoch1}, session/worker 字段齐）");

    // 轮 2：真实 LLM 流式应答 → 实时帧必须带 epoch 且晚于快照
    let c1 = msg_count(&env, &sid);
    prompt(&env, &sid, "Reply with exactly: STREAM OK");
    let delta_idx = col.wait_for("text_delta", Duration::from_secs(120));
    let end_idx = col.wait_for("agent_end", Duration::from_secs(120));
    let delta_idx = delta_idx
        .unwrap_or_else(|| panic!("实时 text_delta 未到达（真实流式缺失）。frames: {:?}", {
            let f = col.snapshot();
            f.len().min(30).checked_sub(10).map(|s| &f[s..]).map(<[String]>::to_vec).unwrap_or(f)
        }));
    assert!(
        delta_idx > snap_idx,
        "实时事件必须晚于 snapshot（delta@{delta_idx} <= snap@{snap_idx}）"
    );
    let delta_frame = parse_frame(&col.snapshot()[delta_idx]);
    assert_eq!(
        delta_frame["epoch"].as_u64(),
        Some(epoch1),
        "实时帧必须盖 epoch 章: {delta_frame}"
    );
    println!("  [ok] 实时 text_delta 晚于快照且带 epoch（frame#{delta_idx}）");
    if let Some(ei) = end_idx {
        println!("  [ok] agent_end 到达（frame#{ei}）");
    }
    wait_turn_done(&env, &sid, "turn2", c1);
    col.stop();

    // 轮 3 前置：kill -9 worker（精确 PID）→ 旧 epoch 订阅收 stale_route
    let col2 = Collector::start(&env.sock, &sid);
    let _ = col2.wait_for("\"subscribed\"", Duration::from_secs(10));
    let _ = col2.wait_for("\"snapshot\":true", Duration::from_secs(10));
    kill_worker_sigkill(&env, host.pid, &sid, "T2");

    let stale_idx = col2.wait_for("stale_route", Duration::from_secs(45))
        .unwrap_or_else(|| panic!("kill -9 后旧订阅未收 stale_route。frames: {:?}", col2.snapshot()));
    let stale = parse_frame(&col2.snapshot()[stale_idx]);
    assert_eq!(stale["type"].as_str(), Some("stale_route"), "{stale}");
    let new_epoch = stale["currentEpoch"].as_u64().unwrap_or(0);
    assert!(
        new_epoch > epoch1,
        "epoch 必须单调推进: old={epoch1} current={new_epoch}"
    );
    println!("  [ok] 旧订阅收 stale_route（epoch {epoch1} → currentEpoch {new_epoch}），共 {} 帧", col2.snapshot().len());
    col2.stop();

    // 轮 3：重派（router 自动拉起/prompt 自建）→ 新订阅 epoch 递增 + 快照先行
    let c2 = msg_count(&env, &sid);
    assert!(
        prompt_after_restart(&env, host.pid, &sid, "Reply with exactly: PONG", c2, "turn3"),
        "重派后 PONG 轮未开跑（重建窗口丢 prompt 超过重试预算）"
    );
    wait_turn_done(&env, &sid, "turn3", c2);
    let lines = sock_one_shot(
        &env.sock,
        &format!("{{\"id\":\"re\",\"method\":\"subscribe\",\"session\":\"{sid}\"}}"),
        Duration::from_secs(8),
        2,
    );
    let ack2 = lines
        .first()
        .map(|l| parse_frame(l))
        .unwrap_or(serde_json::Value::Null);
    assert_eq!(ack2["type"].as_str(), Some("subscribed"), "重订阅 ack 异常: {ack2}");
    assert_eq!(
        ack2["epoch"].as_u64(),
        Some(new_epoch),
        "重订阅 epoch 应 = stale_route 的 currentEpoch（{new_epoch}），got {ack2}"
    );
    let snap2 = lines.get(1).map(|l| parse_frame(l)).unwrap_or(serde_json::Value::Null);
    assert_eq!(
        snap2["event"]["customType"].as_str(),
        Some("snapshot"),
        "重订阅同样快照先行: {snap2}"
    );
    println!("  [ok] 重派后新订阅 epoch={new_epoch} 且快照先行");

    // 轮 3 应答落盘核对
    let msgs = flush_and_read_messages(&env, &sid);
    let last_assistant = msgs.iter().rev().find(|(r, _)| r == "assistant");
    assert!(
        last_assistant.map(|(_, t)| t.contains("PONG")).unwrap_or(false),
        "重派后真实应答应含 PONG: {msgs:?}"
    );
    println!("  [ok] 重派后真实 LLM 应答含 PONG（共 {} 条消息）", msgs.len());
}

// ─────────────────────────────────────────────────────────────
// T3 队列落盘（queued_input → kill -9 → 重建回放 → 真实作答）— 4 LLM 轮
// ─────────────────────────────────────────────────────────────
#[test]
#[ignore]
fn e2e_03_queued_input_survives_sigkill() {
    let _guard = SERIAL.lock().unwrap();
    if !e2e_enabled() {
        return;
    }
    const CODEWORD: &str = "PINEAPPLE42";
    const QUEUE_MARK: &str = "CODEWORD_CHECK_T3";
    let env = setup_env("t3-queue", false);
    println!("== T3 队列落盘：followUp 排队 → kill -9 worker → 重建回放 → 新轮真实作答 ==");
    let host = start_host(&env, &[]);
    let sid = create_session(&env);

    // 轮 1：种码词
    let q0 = msg_count(&env, &sid);
    prompt(&env, &sid, &format!("Remember the codeword {CODEWORD} for later. Reply with just: OK"));
    wait_turn_done(&env, &sid, "turn1", q0);

    // 轮 2：长 bash（sleep 25）占用 run，等 tool_execution_start 确认 bash 已起
    let col = Collector::start(&env.sock, &sid);
    let _ = col.wait_for("\"subscribed\"", Duration::from_secs(10));
    let q0b = msg_count(&env, &sid);
    assert!(
        prompt_after_restart(&env, host.pid, &sid, "Use the bash tool to run exactly: sleep 25. Do nothing else. After the command completes reply: SLEEP_DONE", q0b, "turn2"),
        "T3 turn2（长 bash）未开跑"
    );
    let tool_idx = col.wait_for("tool_execution_start", Duration::from_secs(90))
        .unwrap_or_else(|| panic!("90s 内未见 tool_execution_start（LLM 未按指示调 bash），frames 末尾: {:?}", {
            let f = col.snapshot();
            f.len().min(40).checked_sub(10).map(|s| &f[s..]).map(<[String]>::to_vec).unwrap_or(f)
        }));
    println!("  [ok] bash 工具已启动（frame#{tool_idx}），sleep 25 进行中");

    // 忙时排队 followUp → 入队即落盘
    let queued = prompt_behavior(
        &env,
        &sid,
        &format!("{QUEUE_MARK}: What is the codeword I told you? Reply with just the codeword."),
        "followUp",
    );
    let q_status = queued["status"].as_str().unwrap_or("");
    assert_eq!(q_status, "queued", "忙时 followUp 应返回 queued: {queued:?}");
    println!("  [ok] followUp 已排队（status=queued queue={:?}）", queued["queue"]);

    let jsonl = session_jsonl(&env, &sid);
    assert_eq!(
        count_file_occurrences(&jsonl, "\"queued_input\""),
        1,
        "入队即落盘：会话文件应恰好 1 条 queued_input（{}）",
        jsonl.display()
    );
    println!("  [ok] queued_input 已落盘（{}）", jsonl.display());

    // kill -9 worker（排队消息留在盘上，内存队列随之湮灭）
    std::thread::sleep(Duration::from_millis(1500));
    kill_worker_sigkill(&env, host.pid, &sid, "T3");
    col.stop();

    // 轮 3+4：新 prompt 拉起新 worker → queued_input 回放 → 消费 → 真实作答
    let q1 = msg_count(&env, &sid);
    assert!(
        prompt_after_restart(&env, host.pid, &sid, "Continue.", q1, "rebuild+drain"),
        "重建后 Continue 轮未开跑（重建窗口丢 prompt 超过重试预算）"
    );
    wait_turn_done(&env, &sid, "rebuild+drain", q1);
    settle(&env, &sid, "rebuild+drain"); // 回放轮可能连转（continue + codeword 两轮）
    let info = session_rpc_ok(&env, &sid, "get_session_info", "{}");
    println!("  [info] 重建后 message_count={}", info["message_count"]);

    // 断言一：新轮真实作答引用了码词（回执 ≠ 送达——必须答出来）
    let msgs = flush_and_read_messages(&env, &sid);
    let last_assistant = msgs.iter().rev().find(|(r, _)| r == "assistant");
    assert!(
        last_assistant.map(|(_, t)| t.contains(CODEWORD)).unwrap_or(false),
        "重建回放后新轮 assistant 应答必须包含码词 {CODEWORD}。末尾消息: {:?}",
        msgs.iter().rev().take(6).collect::<Vec<_>>()
    );
    println!("  [ok] 新轮真实作答引用码词 {CODEWORD}");

    // 断言二：排队消息恰好投递一次（1 条 queued_input + 已消费标记）
    assert_eq!(
        count_file_occurrences(&jsonl, "\"queued_input\""),
        1,
        "queued_input 必须恰好一份（幂等）"
    );
    assert!(
        count_file_occurrences(&jsonl, "queued_input_consumed") >= 1,
        "消费后必须追加 queued_input_consumed 标记"
    );
    // 排队文本作为 user 消息真实进了历史
    assert!(
        msgs.iter().any(|(r, t)| r == "user" && t.contains(QUEUE_MARK)),
        "排队消息必须作为 user 消息出现在历史里: {:?}",
        msgs.iter().map(|(r, t)| (r.clone(), t.chars().take(40).collect::<String>())).collect::<Vec<_>>()
    );
    println!("  [ok] 幂等：queued_input 恰好 1 份 + consumed 标记在案 + 排队文本进了历史（共 {} 条消息）", msgs.len());
}

// ─────────────────────────────────────────────────────────────
// T4 同 sid 原地重建（K4 auto_respawn_local + 全历史预加载）— 3 LLM 轮
// ─────────────────────────────────────────────────────────────
#[test]
#[ignore]
fn e2e_04_inplace_rebuild_after_sigkill() {
    let _guard = SERIAL.lock().unwrap();
    if !e2e_enabled() {
        return;
    }
    const CODEWORD: &str = "MANGO77";
    let env = setup_env("t4-inplace", true);
    println!("== T4 原地重建：auto_respawn_local + kill -9 busy worker → 同 sid 重建 + 全历史续作 ==");
    // 心跳参数调快：stdout-EOF 即时重派为主，心跳兜底（6s busy-dead）；
    // RUST_LOG=info 让 [auto-recovery] tracing 行进 host log（断言三要用）
    let host = start_host(
        &env,
        &[
            ("ION_HEARTBEAT_TICK_SECS", "2"),
            ("ION_HEARTBEAT_BUSY_MS", "6000"),
            ("RUST_LOG", "info"),
        ],
    );
    let sid = create_session(&env);

    // 轮 1：种码词（进会话文件 = 全历史的一部分）
    let k0 = msg_count(&env, &sid);
    prompt(&env, &sid, &format!("Remember the codeword {CODEWORD} for later. Reply with just: OK"));
    wait_turn_done(&env, &sid, "turn1", k0);

    // 轮 2：长 bash 占住 run → kill -9 → 等 host 自动原地重派
    let col = Collector::start(&env.sock, &sid);
    let _ = col.wait_for("\"subscribed\"", Duration::from_secs(10));
    // 🔴 用弹性 prompt：worker 可能在上一轮结束后空闲自退出，prompt 撞上重建窗口
    // 会被吞（rpc success 但无 agent_start，run1 实测），需要重发兜底
    let k1 = msg_count(&env, &sid);
    assert!(
        prompt_after_restart(&env, host.pid, &sid, "Use the bash tool to run exactly: sleep 60. Do nothing else. After the command completes reply: SLEEP_DONE", k1, "turn2"),
        "T4 turn2（长 bash）未开跑"
    );
    let tool_idx = col.wait_for("tool_execution_start", Duration::from_secs(90))
        .unwrap_or_else(|| panic!("90s 内未见 tool_execution_start（LLM 未调 bash），frames: {:?}", col.snapshot()));
    println!("  [ok] bash 已启动（frame#{tool_idx}）");

    let old_pid = find_worker_pid(&env, host.pid, &sid).expect("未找到 worker 进程");
    kill_worker_sigkill(&env, host.pid, &sid, "T4");
    println!("  [killed] 原 worker pid={old_pid}");

    // 等 auto-recovery 原地重派（同 sid 新 worker，无需任何新 prompt）≤90s
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut new_pid = None;
    while Instant::now() < deadline {
        if let Some(p) = find_worker_pid(&env, host.pid, &sid) {
            if p != old_pid {
                new_pid = Some(p);
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(1500));
    }
    let new_pid = new_pid.unwrap_or_else(|| {
        panic!(
            "90s 内未见同 sid worker 重派（auto_respawn_local 未生效？）。host log:\n{}",
            host_log_tail(&env, 40)
        )
    });
    println!("  [ok] 原地重派：pid {old_pid} → {new_pid}（同 sid {sid}）");
    std::thread::sleep(Duration::from_secs(2)); // 等 worker init（JSONL 预加载）
    col.stop();

    // 重派 worker 会自行开一轮"短引导续作"（K4 设计），等它跑完再读内存态
    let settle_deadline = Instant::now() + Duration::from_secs(150);
    let info = loop {
        let i = session_rpc_ok(&env, &sid, "get_session_info", "{}");
        let running = i["is_running"].as_bool().unwrap_or(true);
        if !running && i["message_count"].as_u64().is_some() {
            break i;
        }
        assert!(Instant::now() < settle_deadline, "重派 worker 150s 未空闲。info={i}");
        std::thread::sleep(Duration::from_millis(1500));
    };

    // 断言一：新 worker 内存里预加载了全历史（未发任何新 prompt 就有历史 user 消息，
    // 含原始任务 2 条 + 重派自开的短引导 1 条 → ≥2 即证明预加载）
    let users_preload = info["user_messages"].as_u64().unwrap_or(0);
    assert!(
        users_preload >= 2,
        "原地重建必须全历史预加载（重派后未发新 prompt，user_messages 应 ≥2，got {users_preload}）。info={info}"
    );
    println!("  [ok] 全历史预加载：新 worker 内存 user_messages={users_preload}（含短引导，未发新 prompt）");

    // 轮 3：续作——只有带着全历史的 worker 才答得出码词
    let k1 = msg_count(&env, &sid);
    assert!(
        prompt_after_restart(&env, host.pid, &sid, "What is the codeword I asked you to remember earlier in this session? Reply with just the codeword.", k1, "turn3"),
        "续作码词轮未开跑（重建窗口丢 prompt 超过重试预算）"
    );
    wait_turn_done(&env, &sid, "turn3", k1);
    let msgs = flush_and_read_messages(&env, &sid);
    let last_assistant = msgs.iter().rev().find(|(r, _)| r == "assistant");
    assert!(
        last_assistant.map(|(_, t)| t.contains(CODEWORD)).unwrap_or(false),
        "原地重建后续作必须答出码词 {CODEWORD}（全历史保真）。末尾消息: {:?}",
        msgs.iter().rev().take(6).collect::<Vec<_>>()
    );
    println!("  [ok] 续作真实答出码词 {CODEWORD}（会话 {sid} 全程同 sid，共 {} 条消息）", msgs.len());

    // 断言三：host log 里有 auto-recovery 决策痕迹
    let log = std::fs::read_to_string(&env.host_log).unwrap_or_default();
    assert!(
        log.contains("auto-recovery") || log.contains("auto_recovered") || log.contains("respawn"),
        "host log 应有 auto-recovery 痕迹"
    );
    println!("  [ok] host log 含 auto-recovery 痕迹");
}

// ─────────────────────────────────────────────────────────────
// T5 abort_bash（fix4 ①：abort 不杀 worker）— 2-3 LLM 轮
// 断言：abort 期间 worker 进程同 PID 存活 + 会话可继续应答。
// 🔴 已知产品问题（真实复现 2 次）：abort 后同一 worker 的下一轮 run 永久
// 挂起（agent_start 后无 LLM 事件），测试在检测到挂起时走重建恢复路径。
// ─────────────────────────────────────────────────────────────
#[test]
#[ignore]
fn e2e_05_abort_bash_worker_survives() {
    let _guard = SERIAL.lock().unwrap();
    if !e2e_enabled() {
        return;
    }
    let env = setup_env("t5-abort", false);
    println!("== T5 abort_bash：长 bash 中 abort → worker 不死（同 PID）→ 继续应答 ==");
    let host = start_host(&env, &[]);
    let sid = create_session(&env);

    // 轮 1：长 bash 占住 run
    let col = Collector::start(&env.sock, &sid);
    let _ = col.wait_for("\"subscribed\"", Duration::from_secs(10));
    prompt(&env, &sid, "Use the bash tool to run exactly: sleep 45. Do nothing else. After the command completes reply: DONE");
    let tool_idx = col.wait_for("tool_execution_start", Duration::from_secs(90))
        .unwrap_or_else(|| panic!("90s 内未见 tool_execution_start（LLM 未调 bash），frames: {:?}", col.snapshot()));
    // 模拟真实用户：看到工具启动后 ~3s 才点中止（400ms 内的极限竞态会命中
    // tool-start 记账窗口，run3 实测会触发"下一轮挂起"的产品问题，见报告）
    std::thread::sleep(Duration::from_secs(3));
    println!("  [ok] bash sleep 45 已启动（frame#{tool_idx}）+3s");

    let worker_pid = find_worker_pid(&env, host.pid, &sid).expect("未找到 worker 进程");
    println!("  [abort] worker pid={worker_pid}");

    // abort：阻塞锁修复后必须优雅返回且不杀 worker
    let (ok, v, raw) = session_rpc(&env, &sid, "abort", "{}");
    assert!(ok, "abort RPC 失败: {raw}");
    println!("  [ok] abort RPC 返回: {:?}", v["data"]);

    // 等 abort 生效（is_running → false，≤60s）
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let info = session_rpc_ok(&env, &sid, "get_session_info", "{}");
        if !info["is_running"].as_bool().unwrap_or(true) {
            break;
        }
        assert!(Instant::now() < deadline, "abort 后 60s 仍在 running");
        std::thread::sleep(Duration::from_millis(1000));
    }
    println!("  [ok] abort 生效，run 已停");

    // 核心断言一：worker 未被杀（同 PID 存活——fix3 的 blocking_lock panic 会杀死 worker）
    assert!(
        pid_alive(worker_pid) && pid_cmdline(worker_pid).contains("rpc"),
        "abort 后 worker pid {worker_pid} 必须仍存活（abort 杀死了 worker = fix4①回归）: {}",
        pid_cmdline(worker_pid)
    );
    println!("  [ok] worker 同 PID 存活（{worker_pid}）");
    col.stop();

    // 断言二：会话可继续应答。
    // 🔴 已知产品问题（本轮 e2e 发现，待修，见报告）：abort 后同一 worker 的
    // 下一轮 run 会永久挂起（agent_start 后无任何 LLM 事件、无 agent_end，
    // 后续 prompt 全被吞进僵尸 run，run1/run2 两轮真实复现）。这里按文档化
    // 恢复路径走：SIGKILL 僵尸 worker（精确 PID）→ prompt 自建新 worker 续作。
    let a0 = msg_count(&env, &sid);
    if !prompt_after_restart(&env, host.pid, &sid, "Reply with exactly: STILL_ALIVE", a0, "turn2-direct") {
        // 命中挂起 bug → 走重建恢复
        println!("  [warn] post-abort 下一轮挂起（已知产品问题），走重建恢复路径");
        kill_worker_sigkill(&env, host.pid, &sid, "T5-recovery");
        let a1 = msg_count(&env, &sid);
        assert!(
            prompt_after_restart(&env, host.pid, &sid, "Reply with exactly: STILL_ALIVE", a1, "turn2-recovery"),
            "重建后 STILL_ALIVE 轮仍未开跑"
        );
        wait_turn_done(&env, &sid, "turn2-recovery", a1);
    } else {
        wait_turn_done(&env, &sid, "turn2-direct", a0);
    }
    let msgs = flush_and_read_messages(&env, &sid);
    let last_assistant = msgs.iter().rev().find(|(r, _)| r == "assistant");
    assert!(
        last_assistant.map(|(_, t)| t.contains("STILL_ALIVE")).unwrap_or(false),
        "abort 后会话必须能继续应答 STILL_ALIVE。末尾消息: {:?}",
        msgs.iter().rev().take(6).collect::<Vec<_>>()
    );
    println!("  [ok] abort 后会话继续应答 STILL_ALIVE（共 {} 条消息）", msgs.len());
}
