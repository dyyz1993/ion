//! Dev Server Detector E2E integration test
//!
//! Directly tests the extension's lifecycle hooks (on_tool_execution_end +
//! on_system_prompt) via Rust API — no host process needed.
//! Verifies the full chain: bash stdout → port detection → XML injection.

use ion::agent::extension::{Extension, ToolExecutionContext};
use ion::dev_server_detector::DevServerDetectorExtension;
use serde_json::json;

/// Build a ToolExecutionContext simulating a bash tool call with given stdout.
fn bash_ctx(stdout: &str, command: &str) -> ToolExecutionContext {
    ToolExecutionContext {
        tool_call_id: "test_call".to_string(),
        tool_name: "bash".to_string(),
        args: json!({"command": command}),
        is_error: false,
        duration_ms: 10,
        result: stdout.to_string(),
        is_interrupted: false,
    }
}

#[tokio::test]
async fn test_vite_port_detection_and_injection() {
    let ext = DevServerDetectorExtension::new();

    // Simulate bash outputting a Vite dev server URL
    let ctx = bash_ctx(
        "VITE v5.0.0 ready in 300 ms\n➜  Local:   http://localhost:5173/\n",
        "npm run dev",
    );
    ext.on_tool_execution_end(&ctx).await.unwrap();

    // Now on_system_prompt should inject <dev_servers> XML
    let mut prompt = String::from("You are a helpful assistant.");
    ext.on_system_prompt(&mut prompt).await.unwrap();

    assert!(
        prompt.contains("<dev_servers"),
        "system prompt should contain <dev_servers> XML, got: {prompt}"
    );
    assert!(
        prompt.contains(r#"port="5173"#),
        "XML should contain port 5173, got: {prompt}"
    );
    println!("✅ Vite detection + injection:\n{prompt}");
}

#[tokio::test]
async fn test_multiple_frameworks() {
    let ext = DevServerDetectorExtension::new();

    // Next.js
    ext.on_tool_execution_end(&bash_ctx(
        "▲ Next.js 14.0.0\n▲ Local:   http://localhost:3000\n",
        "next dev",
    )).await.unwrap();

    // Python http.server
    ext.on_tool_execution_end(&bash_ctx(
        "Serving HTTP on 0.0.0.0 port 8000 ...",
        "python3 -m http.server",
    )).await.unwrap();

    let mut prompt = String::new();
    ext.on_system_prompt(&mut prompt).await.unwrap();

    assert!(prompt.contains(r#"port="3000"#), "should detect 3000: {prompt}");
    assert!(prompt.contains(r#"port="8000"#), "should detect 8000: {prompt}");
    assert!(prompt.contains(r#"count="2""#), "should show count=2: {prompt}");
    println!("✅ Multiple frameworks:\n{prompt}");
}

#[tokio::test]
async fn test_non_server_command_no_injection() {
    let ext = DevServerDetectorExtension::new();

    // ls command — should NOT trigger detection
    ext.on_tool_execution_end(&bash_ctx(
        "file1.rs\nfile2.rs\nREADME.md",
        "ls -la",
    )).await.unwrap();

    let mut prompt = String::from("base prompt");
    ext.on_system_prompt(&mut prompt).await.unwrap();

    assert!(
        !prompt.contains("<dev_servers"),
        "non-server command should NOT inject XML: {prompt}"
    );
    assert_eq!(prompt, "base prompt", "prompt should be unchanged");
    println!("✅ Non-server command correctly skipped");
}

#[tokio::test]
async fn test_dedup_same_signature_no_reinject() {
    let ext = DevServerDetectorExtension::new();

    // First detection
    ext.on_tool_execution_end(&bash_ctx(
        "Local: http://localhost:5173/",
        "vite",
    )).await.unwrap();

    let mut prompt1 = String::new();
    ext.on_system_prompt(&mut prompt1).await.unwrap();
    let len1 = prompt1.len();
    assert!(prompt1.contains("<dev_servers"));

    // Second call with same ports — signature unchanged, should NOT re-append
    let mut prompt2 = String::new();
    ext.on_system_prompt(&mut prompt2).await.unwrap();

    // prompt2 starts empty; if dedup works, it stays empty (nothing to inject)
    assert!(
        prompt2.is_empty(),
        "same signature should not re-inject, but got: {prompt2}"
    );
    println!("✅ Dedup: same signature not re-injected");
}

#[tokio::test]
async fn test_non_bash_tool_ignored() {
    let ext = DevServerDetectorExtension::new();

    // A read tool that happens to contain localhost in output
    let ctx = ToolExecutionContext {
        tool_call_id: "test".to_string(),
        tool_name: "read".to_string(),  // NOT bash
        args: json!({"file_path": "/tmp/x"}),
        is_error: false,
        duration_ms: 5,
        result: "server runs on localhost:3000".to_string(),
        is_interrupted: false,
    };
    ext.on_tool_execution_end(&ctx).await.unwrap();

    let mut prompt = String::new();
    ext.on_system_prompt(&mut prompt).await.unwrap();

    assert!(
        !prompt.contains("<dev_servers"),
        "non-bash tool should be ignored even if output has port: {prompt}"
    );
    println!("✅ Non-bash tool correctly ignored");
}

#[tokio::test]
async fn test_flask_format() {
    let ext = DevServerDetectorExtension::new();

    ext.on_tool_execution_end(&bash_ctx(
        " * Running on http://127.0.0.1:5000",
        "flask run",
    )).await.unwrap();

    let mut prompt = String::new();
    ext.on_system_prompt(&mut prompt).await.unwrap();

    assert!(prompt.contains(r#"port="5000"#), "should detect Flask 5000: {prompt}");
    println!("✅ Flask format (127.0.0.1): {prompt}");
}

#[tokio::test]
async fn test_extension_rpc_list() {
    let ext = DevServerDetectorExtension::new();

    // Detect a port first
    ext.on_tool_execution_end(&bash_ctx(
        "Local: http://localhost:3000/",
        "npm start",
    )).await.unwrap();

    // Query via extension_rpc "list"
    let result = ext
        .on_extension_rpc("list", json!({}))
        .await
        .unwrap();

    let result_str = result.to_string();
    assert!(
        result_str.contains("3000"),
        "list RPC should return port 3000: {result_str}"
    );
    assert!(
        result_str.contains("\"count\":1") || result_str.contains("\"count\":\"1\""),
        "should show count 1: {result_str}"
    );
    println!("✅ extension_rpc list: {result_str}");
}

#[tokio::test]
async fn test_extension_rpc_clear() {
    let ext = DevServerDetectorExtension::new();

    // Detect then clear
    ext.on_tool_execution_end(&bash_ctx(
        "Local: http://localhost:3000/",
        "npm start",
    )).await.unwrap();

    let _ = ext.on_extension_rpc("clear", json!({})).await.unwrap();

    // List should now be empty
    let result = ext.on_extension_rpc("list", json!({})).await.unwrap();
    let result_str = result.to_string();
    assert!(
        result_str.contains("\"count\":0") || result_str.contains("\"count\":\"0\""),
        "after clear, count should be 0: {result_str}"
    );
    println!("✅ extension_rpc clear works");
}
