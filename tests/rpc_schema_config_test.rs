//! S4：配置/审批/快照/扩展/MCP 域 RPC JSON Schema 契约一致性测试。
//!
//! 三层：
//! 1. 静态 — schemas/rpc/config/*.json 每个都是合法的 draft 2020-12 schema；
//!    _index.json 与目录内容一致。
//! 2. fixture — 手工构造的响应样本（manager 路径 extension_rpc、get_mcp_servers
//!    成功数组、脱敏形态）过 schema；get_settings schema 必须拒绝明文密钥。
//! 3. 动态 — spawn `ion --mode rpc`（私有 HOME + ION_HOST_SOCKET + ION_SESSION_DIR
//!    三件套隔离，绝不触碰真实 ~/.ion），把真实 RPC 响应逐一过对应 schema。
//!    审批/快照链用 ION_FAUX_SCRIPT 驱动真实 agent 轮（write 工具 → step-snapshot
//!    → review_pending 有货），零 LLM 成本。
//!
//! schema 来源：以 src/worker_rpc.rs 分发 match 实际实现为准逐命令提取
//! （含 set_model 响应 command='get_state'、call_tool output 转义字符串、
//! extension_rpc 双路径分裂、get_settings 脱敏形态等历史怪癖）。

use jsonschema::Validator;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const READY_TIMEOUT: Duration = Duration::from_secs(60);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(60);

fn schemas_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("schemas/rpc/config")
}

fn load_schema(name: &str) -> (Value, Validator) {
    let path = schemas_dir().join(format!("{name}.json"));
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let v: Value = serde_json::from_str(&raw).expect("schema file is valid JSON");
    let validator = Validator::new(&v)
        .unwrap_or_else(|e| panic!("schema {name} is not a valid JSON Schema: {e}"));
    (v, validator)
}

/// 校验 instance 通过 schema；失败时打印全部 validation errors 便于修 shape。
fn assert_valid(validator: &Validator, schema_name: &str, instance: &Value) {
    if validator.is_valid(instance) {
        return;
    }
    let errs: Vec<String> = validator.iter_errors(instance).map(|e| e.to_string()).collect();
    panic!(
        "response does NOT match schema '{schema_name}':\n{}\n--- response ---\n{}",
        errs.join("\n"),
        serde_json::to_string_pretty(instance).unwrap_or_default()
    );
}

/// 校验 instance 必须不通过 schema（用于脱敏强制：明文密钥必须被拒）。
fn assert_invalid(validator: &Validator, schema_name: &str, instance: &Value) {
    assert!(
        !validator.is_valid(instance),
        "instance should FAIL schema '{schema_name}' but passed:\n{instance}"
    );
}

// ───────────────────────── 静态层 ─────────────────────────

#[test]
fn static_all_schemas_are_valid_draft_2020_12() {
    let mut count = 0;
    for entry in std::fs::read_dir(schemas_dir()).expect("schemas dir exists") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if path.file_name().and_then(|n| n.to_str()) == Some("_index.json") {
            continue;
        }
        let raw = std::fs::read_to_string(&path).expect("read schema");
        let v: Value = serde_json::from_str(&raw).expect("valid JSON");
        assert!(
            v.get("$schema").and_then(|s| s.as_str())
                == Some("https://json-schema.org/draft/2020-12/schema"),
            "{} must declare draft 2020-12",
            path.display()
        );
        Validator::new(&v)
            .unwrap_or_else(|e| panic!("{} invalid schema: {e}", path.display()));
        count += 1;
    }
    assert!(count >= 40, "expected full command coverage, got {count}");
}

#[test]
fn static_index_matches_directory() {
    let idx_raw =
        std::fs::read_to_string(schemas_dir().join("_index.json")).expect("read _index.json");
    let idx: Value = serde_json::from_str(&idx_raw).expect("valid JSON");
    let commands = idx
        .get("commands")
        .and_then(|c| c.as_array())
        .expect("commands array");

    // 每个登记项都有对应文件
    for c in commands {
        let file = c.get("file").and_then(|f| f.as_str()).expect("file field");
        let name = c.get("name").and_then(|n| n.as_str()).expect("name field");
        assert_eq!(file, &format!("{name}.json"), "file/name mismatch for {name}");
        assert!(
            schemas_dir().join(file).exists(),
            "index entry {name} has no schema file"
        );
        assert!(
            c.get("group").and_then(|g| g.as_str()).is_some(),
            "{name} missing group"
        );
    }

    // 每个文件都被登记（_index.json 除外）
    for entry in std::fs::read_dir(schemas_dir()).expect("schemas dir") {
        let name = entry
            .expect("entry")
            .file_name()
            .to_string_lossy()
            .to_string();
        if name == "_index.json" {
            continue;
        }
        assert!(
            commands.iter().any(|c| c["file"] == name.as_str()),
            "schema file {name} not registered in _index.json"
        );
    }

    let declared = idx.get("commandCount").and_then(|c| c.as_u64()).expect("count");
    assert_eq!(declared as usize, commands.len(), "commandCount mismatch");
    // 动态验证覆盖 ≥ 8 个核心命令（任务硬性要求）
    let dynamic = commands
        .iter()
        .filter(|c| c.get("dynamicTested").and_then(|d| d.as_bool()) == Some(true))
        .count();
    assert!(dynamic >= 8, "dynamic coverage {dynamic} < 8");
}

#[test]
fn static_fixtures_pass_schemas() {
    // extension_rpc manager 路径（singleton 扩展返回值直接作为 data）
    let (_, ext_rpc) = load_schema("extension_rpc");
    assert_valid(&ext_rpc, "extension_rpc(worker)", &json!({
        "id": "1", "type": "response", "command": "extension_rpc", "success": true,
        "data": {"method": "list_stored", "output": {"rules": [], "count": 0}}
    }));
    assert_valid(&ext_rpc, "extension_rpc(manager singleton)", &json!({
        "id": "2", "type": "response", "command": "extension_rpc", "success": true,
        "data": {"items": ["a", "b"], "total": 2}
    }));
    assert_valid(&ext_rpc, "extension_rpc(error)", &json!({
        "id": "3", "type": "response", "command": "extension_rpc", "success": false,
        "error": "extension_rpc ping: not found"
    }));

    // get_mcp_servers 成功数组（host McpManager.server_list_json 形状）
    let (_, mcp) = load_schema("get_mcp_servers");
    assert_valid(&mcp, "get_mcp_servers", &json!({
        "id": "4", "type": "response", "command": "get_mcp_servers", "success": true,
        "data": [{
            "name": "github", "transport": "stdio", "status": "connected",
            "disabled": false,
            "tools": [{"full_name": "mcp__github__search", "original_name": "search",
                       "description": "search repos"}],
            "resources": [], "prompts": [], "error": null
        }]
    }));

    // mcp_reload 成功形状
    let (_, reload) = load_schema("mcp_reload");
    assert_valid(&reload, "mcp_reload", &json!({
        "id": "5", "type": "response", "command": "mcp_reload", "success": true,
        "data": {"servers_loaded": 2, "connected": 1}
    }));

    // call_tool：output 必须是字符串（转义 JSON），对象形态必须被拒
    let (_, call_tool) = load_schema("call_tool");
    assert_valid(&call_tool, "call_tool", &json!({
        "id": "6", "type": "response", "command": "call_tool", "success": true,
        "data": {"tool": "write", "output": "{\"ok\":true,\"bytes\":3}"}
    }));
    assert_invalid(&call_tool, "call_tool(output-as-object)", &json!({
        "id": "7", "type": "response", "command": "call_tool", "success": true,
        "data": {"tool": "write", "output": {"ok": true}}
    }));

    // review_pending 双路径形状：空闲路径（全 summary）+ agent.run 期间 bg 路径
    // （只有 total；status 带字面引号 —— format!("{:?}") 序列化 String 的历史怪癖）
    let (_, rp) = load_schema("review_pending");
    assert_valid(&rp, "review_pending(idle path)", &json!({
        "id": "20", "type": "response", "command": "review_pending", "success": true,
        "data": {"pending": [{"path": "a.rs", "status": "modified", "diffStat": "2 +, 1 -"}],
                 "summary": {"total": 1, "added": 0, "modified": 1, "deleted": 0}}
    }));
    assert_valid(&rp, "review_pending(during-run bg path)", &json!({
        "id": "21", "type": "response", "command": "review_pending", "success": true,
        "data": {"pending": [{"path": "a.rs", "status": "\"added\"", "diffStat": "+5"}],
                 "summary": {"total": 1}}
    }));
}

#[test]
fn static_get_settings_schema_enforces_redaction() {
    let (gs, gs_v) = load_schema("get_settings");
    // 脱敏形态（api_key='***'，provider_api_keys/env/headers 全 '***'）必须通过
    assert_valid(&gs_v, "get_settings(redacted full)", &json!({
        "id": "8", "type": "response", "command": "get_settings", "success": true,
        "data": {
            "default_provider": "zai", "default_model": "glm-5.2",
            "api_key": "***", "base_url": null,
            "provider_api_keys": {"zai": "***"},
            "providers": {"zai": {"api": "openai-completions", "api_key": "***",
                                   "headers": {"Authorization": "***"}}},
            "tier_models": {"max": "zai/glm-5.2"},
            "mcp_servers": {"gh": {"command": "npx", "env": {"TOKEN": "***"}}},
            "runtime": {}, "session": {}, "skills": {}, "fetch": {}
        }
    }));
    // api_key 为 null（未配置）也必须通过
    assert_valid(&gs_v, "get_settings(null api_key)", &json!({
        "id": "9", "type": "response", "command": "get_settings", "success": true,
        "data": {"api_key": null, "provider_api_keys": {}, "providers": {},
                 "mcp_servers": {}, "runtime": {}, "session": {}, "skills": {}, "fetch": {}}
    }));
    // 单键查询形态必须通过（含 key=api_key 的脱敏 if/then 约束）
    assert_valid(&gs_v, "get_settings(keyed api_key masked)", &json!({
        "id": "10", "type": "response", "command": "get_settings", "success": true,
        "data": {"key": "api_key", "value": "***"}
    }));
    assert_valid(&gs_v, "get_settings(keyed other key)", &json!({
        "id": "10b", "type": "response", "command": "get_settings", "success": true,
        "data": {"key": "default_model", "value": "glm-5.2"}
    }));
    // 🔴 明文密钥必须被 schema 拒绝（脱敏强制是本契约的核心断言）
    assert_invalid(&gs_v, "get_settings(plaintext api_key)", &json!({
        "id": "11", "type": "response", "command": "get_settings", "success": true,
        "data": {"api_key": "sk-live-secret", "runtime": {}, "session": {}, "skills": {}, "fetch": {}}
    }));
    assert_invalid(&gs_v, "get_settings(plaintext provider_api_keys)", &json!({
        "id": "12", "type": "response", "command": "get_settings", "success": true,
        "data": {"provider_api_keys": {"zai": "pk-live"}, "runtime": {}, "session": {}, "skills": {}, "fetch": {}}
    }));
    assert_invalid(&gs_v, "get_settings(plaintext mcp env)", &json!({
        "id": "13", "type": "response", "command": "get_settings", "success": true,
        "data": {"mcp_servers": {"gh": {"env": {"TOKEN": "tok-live"}}},
                 "runtime": {}, "session": {}, "skills": {}, "fetch": {}}
    }));
    // keyed api_key 明文：整包 anyOf 无法拒绝（FULL 分支无 required + additionalProperties
    // 放行任何对象），脱敏强制下沉到 keyed 分支（anyOf[1]）级别断言
    let keyed = &gs["oneOf"][0]["properties"]["data"]["anyOf"][1];
    let kv = Validator::new(keyed).expect("keyed branch valid");
    assert_invalid(&kv, "get_settings(keyed-branch api_key plaintext)",
                   &json!({"key": "api_key", "value": "sk-live"}));
    assert_valid(&kv, "get_settings(keyed-branch api_key masked)",
                 &json!({"key": "api_key", "value": "***"}));
    assert_valid(&kv, "get_settings(keyed-branch api_key empty)",
                 &json!({"key": "api_key", "value": ""}));
    assert_valid(&kv, "get_settings(keyed-branch other key free)",
                 &json!({"key": "default_model", "value": "glm-5.2"}));
}

#[test]
fn static_set_model_envelope_command_is_get_state() {
    // 历史怪癖固化：set_model 的响应信封 command 字段是 "get_state"
    let (schema, v) = load_schema("set_model");
    assert_eq!(
        schema["properties"]["command"]["const"], "get_state",
        "set_model schema must encode the get_state envelope quirk"
    );
    assert_valid(&v, "set_model", &json!({
        "id": "15", "type": "response", "command": "get_state", "success": true,
        "data": {"model": "glm-5.2", "provider": "zai"}
    }));
    assert_invalid(&v, "set_model(wrong envelope)", &json!({
        "id": "16", "type": "response", "command": "set_model", "success": true,
        "data": {"model": "glm-5.2", "provider": "zai"}
    }));
}

// ───────────────────────── 动态层 ─────────────────────────

fn find_worker_bin() -> String {
    std::env::var("ION_WORKER_BIN").unwrap_or_else(|_| {
        let current_exe = std::env::current_exe().ok();
        if let Some(exe) = current_exe {
            if let Some(parent) = exe.parent() {
                let sibling = parent.join("ion");
                if sibling.exists() {
                    return sibling.to_string_lossy().to_string();
                }
                let alt = parent.parent().unwrap().join("ion");
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

/// 动态 worker 用的 config：不带 mcp_servers（真实 npx server 会拖慢启动几分钟），
/// 密钥位（api_key / provider_api_keys / provider.headers）由 get_settings 脱敏断言覆盖。
const SECRET_CONFIG: &str = r#"{
  "default_provider": "zai",
  "default_model": "glm-5.2",
  "api_key": "sk-top-GSRED1",
  "provider_api_keys": { "zai": "pk-zai-GSRED2" },
  "providers": {
    "zai": {
      "name": "zai",
      "api": "openai-completions",
      "base_url": "https://api.zai.example/v4",
      "api_key": "prov-zai-GSRED3",
      "headers": { "Authorization": "Bearer hdr-GSRED4" },
      "models": [{ "id": "glm-5.2", "name": "GLM-5.2", "reasoning": true }]
    }
  }
}"#;

/// 快照 worker 用的 config：开启 file-snapshot（默认禁用，须显式开启）。
/// ⚠️ 故意不带 default_provider/default_model，且禁用 memory/learning——
/// auto-session-title / skill-distill / memory-v2 的辅助 LLM 调用会偷走
/// ION_FAUX_SCRIPT 队列里的条目（实测踩坑：脚本第二个 tool_call 被标题生成吃掉）。
const SNAPSHOT_CONFIG: &str = r#"{
  "extensions": {
    "file-snapshot": { "enabled": true },
    "memory": { "enabled": false },
    "learning": { "enabled": false }
  }
}"#;

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
    /// 隔离三件套：私有 HOME + ION_HOST_SOCKET + ION_SESSION_DIR；绝不读写真实 ~/.ion。
    /// extra_env 用于注入 ION_FAUX_SCRIPT 等测试驱动变量。
    fn spawn(
        home: &Path,
        cwd: &Path,
        session: &str,
        config_json: &str,
        extra_env: &[(&str, &str)],
    ) -> Self {
        let ion_dir = home.join(".ion");
        std::fs::create_dir_all(&ion_dir).expect("create isolated .ion");
        std::fs::create_dir_all(home.join("sessions")).expect("create session dir");
        std::fs::write(ion_dir.join("config.json"), config_json).expect("write config");

        let mut cmd = Command::new(find_worker_bin());
        cmd.arg("--mode")
            .arg("rpc")
            .arg("--session")
            .arg(session)
            .current_dir(cwd)
            .env("HOME", home)
            .env("ION_HOST_SOCKET", home.join("test.sock"))
            .env("ION_SESSION_DIR", home.join("sessions"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().expect("failed to spawn ion worker");

        let stdin = child.stdin.take().expect("no stdin");
        let reader = BufReader::new(child.stdout.take().expect("no stdout"));
        let mut worker = Self {
            stdin,
            reader,
            child,
        };

        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            assert!(Instant::now() < deadline, "timeout waiting worker ready");
            let mut line = String::new();
            let n = worker
                .reader
                .read_line(&mut line)
                .expect("read worker stdout");
            assert!(n > 0, "worker closed stdout before ready");
            if let Ok(v) = serde_json::from_str::<Value>(line.trim()) {
                if v.get("type").and_then(|t| t.as_str()) == Some("ready") {
                    break;
                }
            }
        }
        worker
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = format!("s4_{}_{}", method, rand_hex());
        let mut cmd = json!({ "id": id, "method": method });
        if !params.is_null() {
            cmd["params"] = params;
        }
        writeln!(self.stdin, "{}", cmd).expect("write to worker stdin");
        self.stdin.flush().expect("flush worker stdin");

        let deadline = Instant::now() + RESPONSE_TIMEOUT;
        loop {
            assert!(Instant::now() < deadline, "timeout waiting {method} response");
            let mut line = String::new();
            let n = worker_read(&mut self.reader, &mut line);
            assert!(n > 0, "worker closed stdout during {method}");
            if let Ok(v) = serde_json::from_str::<Value>(line.trim()) {
                // 扮演 host：worker 直 spawn 时经 stdout 发 manager_command、等 manager_response；
                // 不应答会死等（get_mcp_servers 等方案 C 命令挂几分钟才超时）
                self.serve_manager_command(&v);
                if v.get("id").and_then(|i| i.as_str()) == Some(id.as_str())
                    && v.get("type").and_then(|t| t.as_str()) == Some("response")
                {
                    return v;
                }
            }
        }
    }

    /// 扮演 host 的 Manager：应答 worker 的 manager_command（方案 C 转发命令）。
    /// 返回 true 表示该行是 manager_command（已应答或消费）。
    fn serve_manager_command(&mut self, v: &Value) -> bool {
        if v.get("type").and_then(|t| t.as_str()) != Some("manager_command") {
            return false;
        }
        let cmd = v.get("command").and_then(|c| c.as_str()).unwrap_or("");
        let reply_to = v
            .pointer("/params/_reply_to")
            .and_then(|r| r.as_str())
            .unwrap_or("");
        if reply_to.is_empty() {
            return true;
        }
        let data = match cmd {
            "mcp_get_servers" => json!([]),
            "mcp_list_tools" => json!({"tools": []}),
            "mcp_reload" => json!({"servers_loaded": 0, "connected": 0}),
            "mcp_toggle_server" => json!({"name": "fake", "enabled": true}),
            "mcp_restart_server" => json!({"name": "fake", "status": "connected"}),
            "mcp_read_resource" => json!({"content": ""}),
            _ => json!(null),
        };
        let resp = json!({"type": "manager_response", "_reply_to": reply_to,
                          "success": true, "data": data});
        let _ = writeln!(self.stdin, "{}", resp);
        let _ = self.stdin.flush();
        true
    }

    /// 发请求 + 断言成功 + 响应过对应 schema。
    fn check(&mut self, schema_name: &str, method: &str, params: Value) -> Value {
        let (_v, validator) = load_schema(schema_name);
        let resp = self.request(method, params);
        assert_eq!(
            resp.get("success").and_then(|s| s.as_bool()),
            Some(true),
            "{method} should succeed: {resp}"
        );
        assert_valid(&validator, schema_name, &resp);
        resp
    }

    /// 发一轮 prompt 并等到它真正跑完。
    ///
    /// ⚠️ prompt RPC 是「先回执后执行」：空闲时立刻回 response，然后 agent.run
    /// 才在 select! 循环里跑完；忙时（默认 steer）直接回 {status:"queued"}。
    /// 所以必须同时等到 stdout 上的 agent_end 事件，才能保证 turn 落盘
    /// （step-snapshot / 审批 reset 都在 turn-end 发生），后续 review_* 才走空闲路径。
    fn prompt_turn(&mut self, text: &str) {
        let id = format!("s4_prompt_{}", rand_hex());
        let cmd = json!({ "id": id, "method": "prompt", "params": {"text": text} });
        writeln!(self.stdin, "{}", cmd).expect("write prompt to worker stdin");
        self.stdin.flush().expect("flush worker stdin");

        let deadline = Instant::now() + Duration::from_secs(60);
        let (mut got_resp, mut got_end) = (false, false);
        while !(got_resp && got_end) {
            assert!(Instant::now() < deadline, "timeout waiting prompt turn end");
            let mut line = String::new();
            let n = worker_read(&mut self.reader, &mut line);
            assert!(n > 0, "worker closed stdout during prompt turn");
            if let Ok(v) = serde_json::from_str::<Value>(line.trim()) {
                self.serve_manager_command(&v);
                if v.get("id").and_then(|i| i.as_str()) == Some(id.as_str())
                    && v.get("type").and_then(|t| t.as_str()) == Some("response")
                {
                    assert_eq!(
                        v.get("success").and_then(|s| s.as_bool()),
                        Some(true),
                        "prompt should succeed: {v}"
                    );
                    got_resp = true;
                }
                if v.get("type").and_then(|t| t.as_str()) == Some("event")
                    && v.pointer("/event/type").and_then(|t| t.as_str()) == Some("agent_end")
                {
                    got_end = true;
                }
            }
        }
    }
}

fn worker_read(reader: &mut BufReader<std::process::ChildStdout>, line: &mut String) -> usize {
    reader.read_line(line).unwrap_or(0)
}

fn tmp_root(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("ion_s4_cfg_{tag}_{}", rand_hex()))
}

/// 配置/扩展域动态验证（一个 worker 串 20+ 命令，控制 spawn 次数）。
#[test]
fn dynamic_config_extension_schemas() {
    let root = tmp_root("ext");
    let session = format!("s4_cfg_{}", rand_hex());
    let mut w = TestWorker::spawn(&root, &root, &session, SECRET_CONFIG, &[]);

    // ── 配置/模型 ──
    w.check("get_settings", "get_settings", json!({}));
    w.check("get_settings", "get_settings", json!({"key": "api_key"}));
    // 🔴 脱敏断言：全量响应里不得出现明文密钥
    let resp = w.request("get_settings", json!({}));
    let raw = serde_json::to_string(&resp).unwrap();
    for secret in ["sk-top-GSRED1", "pk-zai-GSRED2", "prov-zai-GSRED3", "hdr-GSRED4"] {
        assert!(!raw.contains(secret), "明文密钥泄漏 {secret}");
    }

    w.check("set_settings", "set_settings",
            json!({"key": "default_model", "value": "glm-5.2"}));
    w.check("set_model", "set_model",
            json!({"modelId": "glm-5.2", "provider": "zai"}));
    w.check("set_thinking_level", "set_thinking_level", json!({"level": "low"}));
    w.check("cycle_thinking_level", "cycle_thinking_level", json!({}));
    w.check("get_available_models", "get_available_models", json!({}));
    w.check("get_tier_models", "get_tier_models", json!({}));
    w.check("set_tier_models", "set_tier_models",
            json!({"tier": "fast", "model": "zai/glm-5.2"}));

    // ── 工作目录 ──
    w.check("list_dirs", "list_dirs", json!({}));
    let proj = root.join("proj");
    std::fs::create_dir_all(&proj).expect("create proj dir");
    w.check("add_dir", "add_dir", json!({"dir": proj.to_string_lossy()}));
    w.check("remove_dir", "remove_dir", json!({"dir": proj.to_string_lossy()}));
    w.check("set_cwd", "set_cwd", json!({"cwd": proj.to_string_lossy()}));

    // ── 运行时开关 ──
    w.check("set_permission_mode", "set_permission_mode", json!({"mode": "open"}));
    w.check("set_auto_compaction", "set_auto_compaction", json!({"enabled": false}));
    w.check("set_auto_retry", "set_auto_retry", json!({"enabled": true, "max_retries": 2}));

    // ── 扩展/工具 ──
    w.check("get_extensions", "get_extensions", json!({}));
    w.check("extension_list", "extension_list", json!({}));
    w.check("get_flags", "get_flags", json!({}));
    w.check("get_flags", "get_flags", json!({"extension": "permission"}));
    w.check("set_flag", "set_flag",
            json!({"extension": "permission", "flag": "verbose", "value": true}));
    w.check("get_active_tools", "get_active_tools", json!({}));
    w.check("set_active_tools", "set_active_tools",
            json!({"tools": ["read", "write", "bash"]}));
    w.check("get_skills", "get_skills", json!({}));

    // extension_rpc worker 路径（permission 是 worker 级扩展）
    let (_, validator) = load_schema("extension_rpc");
    let resp = w.request("extension_rpc",
                         json!({"extension": "permission", "method": "list_stored", "args": {}}));
    assert_eq!(resp["success"], true, "extension_rpc permission: {resp}");
    assert!(resp["data"]["method"] == "list_stored",
        "worker 路径 data 必须包 {{method, output}}：{resp}");
    assert_valid(&validator, "extension_rpc", &resp);

    // ── permission stored-decision 全链 ──
    w.check("permission_store_decision", "permission_store_decision",
            json!({"subject": "command.run", "pattern": "git status",
                   "decision": "allow", "scope": "project"}));
    w.check("permission_list_stored", "permission_list_stored", json!({}));
    // 拿到规则 id 再删掉。⚠️ 响应 data 双层包装：envelope.data = {success, data:<ext output>}
    let list = w.request("permission_list_stored", json!({}));
    let rule_id = list["data"]["data"]["rules"][0]["id"]
        .as_str()
        .or_else(|| list["data"]["rules"][0]["id"].as_str())
        .map(|s| s.to_string());
    if let Some(id) = rule_id {
        w.check("permission_remove_stored", "permission_remove_stored",
                json!({"id": id}));
    } else {
        panic!("list_stored 应返回已存规则: {list}");
    }
    w.check("permission_clear_stored", "permission_clear_stored", json!({}));
}

/// MCP 域动态验证：无 host 时 get_mcp_servers/mcp_reload 走信封 error 分支；
/// 两个分支都必须匹配 schema（error envelope 或 data 数组/null-able 对象）。
#[test]
fn dynamic_mcp_envelopes_match_schema() {
    let root = tmp_root("mcp");
    let session = format!("s4_mcp_{}", rand_hex());
    let mut w = TestWorker::spawn(&root, &root, &session, SECRET_CONFIG, &[]);

    let (_, mcp) = load_schema("get_mcp_servers");
    let resp = w.request("get_mcp_servers", json!({}));
    assert_valid(&mcp, "get_mcp_servers", &resp);

    let (_, reload) = load_schema("mcp_reload");
    let resp = w.request("mcp_reload", json!({}));
    assert_valid(&reload, "mcp_reload", &resp);
}

/// 审批/快照域动态验证。
///
/// 🔗 链路：call_tool 直调 write（call_tool schema 校验）→ ION_FAUX_SCRIPT 驱动
/// 4 个真实 agent 轮（每轮 write 一个文件 → on_tool_execution_end 建 step-snapshot
/// → review_pending 有货）→ 审批全链（approve/reject/all）→ 快照域
/// （get_modified_files → turn_changes/turn_file_diff/restore_files）。
#[test]
fn dynamic_snapshot_approval_schemas() {
    let root = tmp_root("snap");
    let proj = root.join("proj");
    std::fs::create_dir_all(&proj).expect("create proj");
    let session = format!("s4_snap_{}", rand_hex());

    // faux 脚本：warmup（纯 text，建立 session-start baseline 树）→
    // turn2 写 c（tool_call + 收尾 text）→ turn3 写 e。
    // b/d 由 call_tool 直调写入（不建 tool 快照，由下一轮 turn-end 扫描收编）。
    // faux 脚本：4 轮，每轮一个 tool_call write + 收尾 text。
    // ⚠️ 实测约束：审批 baseline 在每次 prompt 开始时重建（FileSnapshotExtension
    // on_session_start 无 once-guard），所以只有「turn 期间」写的文件才会进 pending——
    // 全部走脚本化 write，不依赖 call_tool 的写入进 diff。
    let mut faux = String::new();
    for (name, content) in [
        ("b.txt", "bravo line"),
        ("c.txt", "charlie line"),
        ("d.txt", "delta line"),
        ("e.txt", "echo line"),
    ] {
        faux.push_str(
            format!(
                "{{\"tool_call\":{{\"name\":\"write\",\"input\":{{\"file_path\":\"{}\",\"content\":\"{}\\n\"}}}}}}\n{{\"text\":\"done {name}\"}}\n",
                proj.join(name).to_string_lossy(),
                content
            )
            .as_str(),
        );
    }
    let faux_script = root.join("faux.jsonl");
    std::fs::write(&faux_script, faux).expect("write faux script");

    let mut w = TestWorker::spawn(
        &root,
        &proj,
        &session,
        SNAPSHOT_CONFIG,
        &[("ION_FAUX_SCRIPT", faux_script.to_string_lossy().as_ref())],
    );

    // call_tool 直调 write（本地 write 工具参数是 file_path）——schema 校验：output 是转义字符串。
    // z.txt 会进首轮 baseline，不影响 pending 断言。
    w.check("call_tool", "call_tool",
            json!({"tool": "write",
                   "args": {"file_path": proj.join("z.txt").to_string_lossy(),
                            "content": "zeta\n"}}));

    // turn 1：写 b → pending = {b}
    w.prompt_turn("go");

    // ── 审批链 ──
    let pending = w.check("review_pending", "review_pending", json!({}));
    assert!(serde_json::to_string(&pending).unwrap().contains("b.txt"),
        "review_pending 应含 b.txt: {pending}");

    w.check("review_file_diff", "review_file_diff", json!({"path": "b.txt"}));
    w.check("review_approve", "review_approve", json!({"path": "b.txt"}));

    w.check("review_approvals", "review_approvals", json!({}));
    let approved = w.check("review_approvals", "review_approvals", json!({"status": "approved"}));
    assert!(serde_json::to_string(&approved).unwrap().contains("b.txt"),
        "approved 过滤应含 b.txt: {approved}");

    // turn 2：写 c → reject c（回滚，c 消失）
    w.prompt_turn("go");
    w.check("review_reject", "review_reject", json!({"path": "c.txt"}));
    assert!(!proj.join("c.txt").exists(), "reject 后 c.txt 应回滚消失");

    // ── 快照域：从 get_modified_files 拿 turnId → turn_changes/turn_file_diff/restore ──
    let modified = w.check("get_modified_files", "get_modified_files", json!({}));
    let files = modified["data"]["files"].as_array().cloned().unwrap_or_default();
    let b_turn = files
        .iter()
        .find(|f| f["path"].as_str().map(|p| p.ends_with("b.txt")) == Some(true))
        .and_then(|f| f["turnId"].as_str())
        .map(|s| s.to_string());
    let Some(turn_id) = b_turn else {
        panic!("get_modified_files 应含 b.txt 的 turnId: {modified}");
    };

    w.check("turn_changes", "turn_changes", json!({"turnId": turn_id}));
    w.check("turn_file_diff", "turn_file_diff",
            json!({"turnId": turn_id, "path": "b.txt"}));
    w.check("turn_file_diff", "turn_file_diff",
            json!({"turnId": turn_id, "path": "b.txt", "base": "disk"}));
    w.check("get_batch_diffs", "get_batch_diffs", json!({}));
    w.check("get_file_history", "get_file_history", json!({"filePath": "b.txt"}));

    // restore_files 回滚到 b.txt 那一轮（store 完整时走成功形状）
    w.check("restore_files", "restore_files", json!({"toTurn": turn_id}));

    // ── 批量审批：cwd=proj 与 HOME 隔离，pending 无 .ion 噪声，批量回滚安全 ──
    w.prompt_turn("go");
    let rj_all = w.check("review_reject_all", "review_reject_all", json!({}));
    assert!(rj_all["data"]["rejected"].as_u64().unwrap_or(0) >= 1,
        "reject_all 应拒掉 d: {rj_all}");

    w.prompt_turn("go");
    let all = w.check("review_approve_all", "review_approve_all", json!({}));
    assert!(all["data"]["approved"].as_u64().unwrap_or(0) >= 1, "approve_all 应批准 e: {all}");
}
