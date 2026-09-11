//! Browser Fetch Harness — FauxProvider 驱动真实 Agent loop 验证 fetch 工具闭环
//!
//! 参照 AGENTS.md「测试验证规范」：Harness 层验证 agent 真实行为（LLM 调 fetch 工具 →
//! 结果进入对话），不调真 LLM。
//!
//! 依赖：sibling 仓库 ~/Project/study-rust/browser 的 release 二进制（优先 ION_BROWSER_PATH；
//! 不存在则 SKIP——CI 脚本负责先构建）。网络：全程本地 HTTP fixture，不访问外网。
//!
//! ⚠️ 测试会改进程级 env（ION_BROWSER_PATH/HOME），用 ENV_LOCK 串行化防竞态。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use ion::agent::agent_loop::{Agent, AgentConfig};
use ion::agent::tool::{EchoTool, ToolRegistry};
use ion::browser_fetch::FetchTool;
use ion_provider::faux;
use ion_provider::registry::ApiRegistry;
use ion_provider::types::*;

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// 最小 HTTP 服务：单线程 accept。首页含 XHR 动态渲染（完整链路验证），/api.json 返回 JSON。
/// ⚠️ 调用方【不要 join】返回的线程：macOS/Linux 上关闭 fd 副本唤不醒阻塞在 accept 的
/// 线程，join 必死锁。断言完成前线程保持服务即可，随测试进程退出回收。
fn spawn_fixture_server() -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
    let addr = listener.local_addr().unwrap().to_string();
    let handle = std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = match stream {
                Ok(s) => s,
                Err(_) => break,
            };
            let mut buf = [0u8; 2048];
            let _ = stream.read(&mut buf);
            let path = String::from_utf8_lossy(&buf).to_string();
            let (ctype, body) = if path.starts_with("GET /api.json") {
                (
                    "application/json",
                    "{\"marker\":\"ION_FETCH_FIXTURE_MARKER_42\"}".to_string(),
                )
            } else {
                (
                    "text/html",
                    "<html><head><title>ION Fetch Fixture</title></head><body>\
                     <p>loading</p><div id=\"list\"></div>\
                     <script>\
                     var x = new XMLHttpRequest();\
                     x.open('GET', '/api.json', true);\
                     x.onreadystatechange = function(){\
                       if (x.readyState === 4 && x.status === 200) {\
                         document.getElementById('list').textContent = JSON.parse(x.responseText).marker;\
                       }\
                     };\
                     x.send();\
                     </script></body></html>"
                        .to_string(),
                )
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
        }
    });
    (addr, handle)
}

fn browser_binary_path() -> Option<String> {
    if let Ok(p) = std::env::var("ION_BROWSER_PATH") {
        // 显式指定但缺失 = 配置错误，必须炸出来而不是静默 SKIP（防假绿）
        assert!(
            std::path::Path::new(&p).exists(),
            "ION_BROWSER_PATH points to missing binary: {p}"
        );
        return Some(p);
    }
    let cand = format!(
        "{}/../browser/target/release/browser",
        env!("CARGO_MANIFEST_DIR")
    );
    let pb = std::path::PathBuf::from(&cand);
    if pb.exists() {
        return Some(pb.canonicalize().unwrap().to_string_lossy().to_string());
    }
    None
}

fn build_agent(responses: Vec<faux::FauxResponseStep>) -> Agent {
    let mut registry = ApiRegistry::new();
    let faux_handle = faux::register_faux(&mut registry);
    faux_handle.set_responses(responses);

    let mut tools = ToolRegistry::new();
    tools.register(Box::new(FetchTool));
    tools.register(Box::new(EchoTool));

    let config = AgentConfig {
        max_turns: Some(4),
        max_retries: 0,
        retry_on_no_tool_use: 0,
        ..Default::default()
    };
    Agent::new(Arc::new(registry), faux_model(), None, tools, config)
}
/// H1：LLM 调 fetch 工具 → 本地 XHR fixture 渲染内容 + warnings 字段进入 ToolResult
#[tokio::test]
async fn h1_fetch_tool_renders_xhr_fixture_into_conversation() {
    let _env = ENV_LOCK.lock().unwrap();
    let Some(browser) = browser_binary_path() else {
        eprintln!("SKIP: browser binary not built (../browser/target/release/browser)");
        return;
    };
    unsafe { std::env::set_var("ION_BROWSER_PATH", &browser) };

    let (addr, _server) = spawn_fixture_server();
    let url = format!("http://{addr}/index.html");

    let captured_url = url.clone();
    let responses = vec![
        faux::FauxResponseStep::Factory(Box::new(move |_context, _, _state, _model| {
            faux::faux_assistant_message(
                faux::FauxContent::Single(faux::faux_tool_call(
                    "fetch",
                    serde_json::json!({"url": captured_url, "format": "text", "timeout_ms": 15000}),
                )),
                faux::FauxMessageOptions {
                    stop_reason: Some(StopReason::ToolUse),
                    error_message: None,
                },
            )
        })),
        faux::FauxResponseStep::Factory(Box::new(|context, _, _, _| {
            // ToolResult 必须包含 XHR 渲染出的 marker + 结构化响应的 warnings 字段
            let got = context.messages.iter().any(|message| match message {
                Message::ToolResult(result) => {
                    let text = tool_result_text(result);
                    text.contains("ION_FETCH_FIXTURE_MARKER_42") && text.contains("\"warnings\"")
                }
                _ => false,
            });
            assert!(got, "ToolResult should contain XHR-rendered fixture marker");
            faux::faux_assistant_message(
                faux::FauxContent::Text("fetch done".into()),
                faux::FauxMessageOptions {
                    stop_reason: Some(StopReason::Stop),
                    error_message: None,
                },
            )
        })),
    ];

    let mut agent = build_agent(responses);
    agent
        .run(&format!("fetch {url} and summarize"))
        .await
        .unwrap();
    // 不 join fixture 线程（join = 死锁，见 spawn_fixture_server 注释）
    unsafe { std::env::remove_var("ION_BROWSER_PATH") };
}

/// H2：URL 白名单拒绝 → 拒绝原因进入 ToolResult（agent 能看到并自行调整）
#[tokio::test]
async fn h2_url_whitelist_denial_reaches_conversation() {
    let _env = ENV_LOCK.lock().unwrap();
    let Some(browser) = browser_binary_path() else {
        eprintln!("SKIP: browser binary not built");
        return;
    };
    unsafe { std::env::set_var("ION_BROWSER_PATH", &browser) };

    // 白名单经 config 注入：临时 HOME 指向含 allow_urls 的 config
    let tmp_home = std::env::temp_dir().join(format!("ion_fetch_harness_{}", std::process::id()));
    let _ = std::fs::create_dir_all(tmp_home.join(".ion"));
    std::fs::write(
        tmp_home.join(".ion/config.json"),
        r#"{"fetch": {"allow_urls": ["https://*.example.com/*"]}}"#,
    )
    .unwrap();
    let saved_home = std::env::var("HOME").ok();
    unsafe { std::env::set_var("HOME", &tmp_home) };

    let responses = vec![
        faux::FauxResponseStep::Factory(Box::new(|_context, _, _, _| {
            faux::faux_assistant_message(
                faux::FauxContent::Single(faux::faux_tool_call(
                    "fetch",
                    serde_json::json!({"url": "https://not-allowed.example.org/x"}),
                )),
                faux::FauxMessageOptions {
                    stop_reason: Some(StopReason::ToolUse),
                    error_message: None,
                },
            )
        })),
        faux::FauxResponseStep::Factory(Box::new(|context, _, _, _| {
            let got = context.messages.iter().any(|message| match message {
                Message::ToolResult(result) => {
                    tool_result_text(result).contains("not allowed by fetch.allow_urls")
                }
                _ => false,
            });
            assert!(got, "denial message should reach ToolResult");
            faux::faux_assistant_message(
                faux::FauxContent::Text("denied as expected".into()),
                faux::FauxMessageOptions {
                    stop_reason: Some(StopReason::Stop),
                    error_message: None,
                },
            )
        })),
    ];

    let mut agent = build_agent(responses);
    agent.run("fetch the page").await.unwrap();

    match saved_home {
        Some(h) => unsafe { std::env::set_var("HOME", h) },
        None => unsafe { std::env::remove_var("HOME") },
    }
    let _ = std::fs::remove_dir_all(&tmp_home);
    drop(_env);
    unsafe { std::env::remove_var("ION_BROWSER_PATH") };
}

/// E2E（真实外网，默认忽略）：ION_E2E=1 cargo test --test browser_fetch_harness -- --ignored
#[tokio::test]
#[ignore]
async fn e2e_real_example_com() {
    if std::env::var("ION_E2E").as_deref() != Ok("1") {
        eprintln!("SKIP: set ION_E2E=1 to run");
        return;
    }
    let Some(browser) = browser_binary_path() else {
        eprintln!("SKIP: browser binary not built");
        return;
    };
    unsafe { std::env::set_var("ION_BROWSER_PATH", &browser) };
    let cfg = ion::config::FetchConfig::default();
    let out = ion::browser_fetch::run_fetch(
        &serde_json::json!({"url": "https://example.com/", "format": "text", "timeout_ms": 30000}),
        &cfg,
    )
    .await
    .expect("real fetch should succeed");
    assert!(out.contains("Example Domain"), "got: {out}");
    unsafe { std::env::remove_var("ION_BROWSER_PATH") };
}

/// 从 ToolResultMessage 提取文本
fn tool_result_text(result: &ToolResultMessage) -> String {
    result
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// faux 模型定义（照 file_snapshot_harness 的 faux_model helper）
fn faux_model() -> Model {
    Model {
        id: "faux-test".into(),
        name: "Faux Test".into(),
        api: "faux".into(),
        provider: "faux".into(),
        base_url: "".into(),
        reasoning: false,
        input: vec!["text".into()],
        cost: Cost::default(),
        context_window: 128000,
        max_tokens: 4096,
        compat: None,
        headers: None,
    }
}
