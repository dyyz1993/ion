//! Worker 级 get_settings 密钥脱敏回归测试（W4 key masking）
//!
//! 路径：spawn `ion --mode rpc`（HOME 指向临时目录，config.json 埋假密钥）
//! → 发 `get_settings`（无 key 参数，全量配置回传）
//! → 断言响应 JSON 里 grep 不到任何明文密钥、各密钥位均为 "***"。
//!
//! 隔离：HOME=临时目录，绝不读写真实 ~/.ion。

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const READY_TIMEOUT: Duration = Duration::from_secs(30);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(15);

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

/// 埋满各层假密钥的 config.json（密钥串带 GSRED 标记便于断言）。
const SECRET_CONFIG: &str = r#"{
  "default_provider": "zai",
  "default_model": "glm-5.2",
  "api_key": "sk-top-GSRED1",
  "provider_api_keys": { "zai": "pk-zai-GSRED2", "opencode": "pk-oc-GSRED3" },
  "providers": {
    "zai": {
      "name": "zai",
      "api": "openai-completions",
      "base_url": "https://api.zai.example/v4",
      "api_key": "prov-zai-GSRED4",
      "headers": { "Authorization": "Bearer hdr-GSRED5" },
      "models": [{ "id": "glm-5.2", "name": "GLM-5.2", "reasoning": true }],
      "model_overrides": { "glm-5.2": { "api_key": "ovr-GSRED6" } }
    }
  },
  "mcp_servers": {
    "github": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-github"],
      "env": { "GITHUB_TOKEN": "env-GSRED7", "HOME": "/Users/someone" }
    },
    "remote": {
      "type": "streamable-http",
      "url": "https://mcp.example/sse",
      "headers": { "Authorization": "Bearer mcp-GSRED8" }
    }
  }
}"#;

const SECRETS: &[&str] = &[
    "sk-top-GSRED1",
    "pk-zai-GSRED2",
    "pk-oc-GSRED3",
    "prov-zai-GSRED4",
    "hdr-GSRED5",
    "ovr-GSRED6",
    "env-GSRED7",
    "mcp-GSRED8",
];

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
            .current_dir(home)
            .env("HOME", home)
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

        // 等 ready 信号
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

    fn request(&mut self, method: &str) -> serde_json::Value {
        let id = format!("gs_{method}");
        let cmd = serde_json::json!({ "id": id, "method": method });
        writeln!(self.stdin, "{}", cmd).expect("write to worker stdin");
        self.stdin.flush().expect("flush worker stdin");

        let deadline = Instant::now() + RESPONSE_TIMEOUT;
        loop {
            assert!(Instant::now() < deadline, "timeout waiting {method} response");
            let mut line = String::new();
            let n = worker_read(&mut self.reader, &mut line);
            assert!(n > 0, "worker closed stdout during {method}");
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) {
                if v.get("id").and_then(|i| i.as_str()) == Some(id.as_str())
                    && v.get("type").and_then(|t| t.as_str()) == Some("response")
                {
                    return v;
                }
            }
        }
    }
}

/// BufReader::read_line 需要 &mut BufReader，包一层避免借用在结构体字段上打架。
fn worker_read(reader: &mut BufReader<std::process::ChildStdout>, line: &mut String) -> usize {
    reader.read_line(line).unwrap_or(0)
}

#[test]
fn worker_get_settings_masks_all_secret_locations() {
    let tmp = std::env::temp_dir().join(format!("ion_gs_redact_{}", rand_hex()));
    let ion_dir = tmp.join(".ion");
    std::fs::create_dir_all(&ion_dir).expect("create isolated .ion dir");
    std::fs::write(ion_dir.join("config.json"), SECRET_CONFIG).expect("write fake config");

    let session = format!("gs_redact_{}", rand_hex());
    let mut worker = TestWorker::spawn(&tmp, &session);

    let resp = worker.request("get_settings");
    assert_eq!(
        resp.get("success").and_then(|s| s.as_bool()),
        Some(true),
        "get_settings 应成功: {resp}"
    );

    // 响应整体（含 data）里不得出现任何明文密钥
    let raw = serde_json::to_string(&resp).unwrap();
    for secret in SECRETS {
        assert!(!raw.contains(secret), "明文密钥泄漏: {secret}\n{raw}");
    }

    // 各密钥位 = "***"（信封 data 字段）
    let data = resp.get("data").cloned().unwrap_or_default();
    assert_eq!(data["api_key"], "***", "顶层 api_key");
    assert_eq!(data["provider_api_keys"]["zai"], "***", "provider_api_keys");
    assert_eq!(
        data["providers"]["zai"]["api_key"], "***",
        "provider api_key"
    );
    assert_eq!(
        data["providers"]["zai"]["headers"]["Authorization"], "***",
        "provider headers"
    );
    assert_eq!(
        data["providers"]["zai"]["model_overrides"]["glm-5.2"]["api_key"], "***",
        "model_overrides api_key"
    );
    assert_eq!(
        data["mcp_servers"]["github"]["env"]["GITHUB_TOKEN"], "***",
        "mcp stdio env"
    );
    assert_eq!(
        data["mcp_servers"]["remote"]["headers"]["Authorization"], "***",
        "mcp http headers"
    );

    // 非密钥字段不受脱敏误伤
    assert_eq!(data["default_model"], "glm-5.2");
    assert_eq!(
        data["mcp_servers"]["remote"]["url"], "https://mcp.example/sse",
        "mcp url 不受影响"
    );
}

/// 单键查询路径（key=api_key）保持既有 "***" 行为（既有语义，防回归）。
#[test]
fn worker_get_settings_single_key_api_key_masks() {
    let tmp = std::env::temp_dir().join(format!("ion_gs_redact_k_{}", rand_hex()));
    let ion_dir = tmp.join(".ion");
    std::fs::create_dir_all(&ion_dir).expect("create isolated .ion dir");
    std::fs::write(ion_dir.join("config.json"), SECRET_CONFIG).expect("write fake config");

    let session = format!("gs_redact_k_{}", rand_hex());
    let mut worker = TestWorker::spawn(&tmp, &session);

    let resp = worker.request_with_params(
        "get_settings",
        serde_json::json!({ "key": "api_key" }),
    );
    assert_eq!(resp.get("success").and_then(|s| s.as_bool()), Some(true));
    let raw = serde_json::to_string(&resp).unwrap();
    assert!(!raw.contains("sk-top-GSRED1"), "单键查询泄漏明文: {raw}");
    let data = resp.get("data").cloned().unwrap_or_default();
    assert_eq!(data["value"], "***", "key=api_key 查询返回打码值");
}

impl TestWorker {
    fn request_with_params(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let id = format!("gs_{method}_p");
        let cmd = serde_json::json!({ "id": id, "method": method, "params": params });
        writeln!(self.stdin, "{}", cmd).expect("write to worker stdin");
        self.stdin.flush().expect("flush worker stdin");

        let deadline = Instant::now() + RESPONSE_TIMEOUT;
        loop {
            assert!(Instant::now() < deadline, "timeout waiting {method}(params) response");
            let mut line = String::new();
            let n = worker_read(&mut self.reader, &mut line);
            assert!(n > 0, "worker closed stdout during {method}(params)");
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) {
                if v.get("id").and_then(|i| i.as_str()) == Some(id.as_str())
                    && v.get("type").and_then(|t| t.as_str()) == Some("response")
                {
                    return v;
                }
            }
        }
    }
}
