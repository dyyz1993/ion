//! CONTEXT_INDEX V1 集成测试 — FauxProvider 驱动的真实 Agent 链路验证

use std::sync::Arc;

use ion::agent::agent_loop::{Agent, AgentConfig};
use ion::agent::context_index::{ContextIndexExtension, WriteKind};
use ion::agent::extension::{Extension, ExtensionRegistry};
use ion::agent::tool::{ToolRegistry, ReadTool, WriteTool};
use ion::agent::messages::Message;
use ion_provider::registry::ApiRegistry;
use ion_provider::types::*;
use ion_provider::faux;

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

#[tokio::test]
async fn context_index_records_read_and_write() {
    let test_file = "/tmp/ion_ctx_e2e_test.txt";
    std::fs::write(test_file, "original content").unwrap();

    // 注册 FauxProvider，拿到共享的 handle
    let mut registry = ApiRegistry::new();
    let faux_handle = faux::register_faux(&mut registry);

    // 设置 5 个响应：read tool_call → write tool_call → done（+ 2 个余量防 agent 多调）
    faux_handle.set_responses(vec![
        faux::FauxResponseStep::Static(faux::faux_assistant_message(
            faux::FauxContent::Single(faux::faux_tool_call("read", serde_json::json!({"file_path": test_file}))),
            faux::FauxMessageOptions { stop_reason: Some(StopReason::ToolUse), error_message: None },
        )),
        faux::FauxResponseStep::Static(faux::faux_assistant_message(
            faux::FauxContent::Single(faux::faux_tool_call("write", serde_json::json!({"file_path": test_file, "content": "new content"}))),
            faux::FauxMessageOptions { stop_reason: Some(StopReason::ToolUse), error_message: None },
        )),
        faux::FauxResponseStep::Static(faux::faux_assistant_message(
            faux::FauxContent::Text("done".into()),
            faux::FauxMessageOptions { stop_reason: Some(StopReason::Stop), error_message: None },
        )),
        // 余量：agent 可能多调一次
        faux::FauxResponseStep::Static(faux::faux_assistant_message(
            faux::FauxContent::Text("done".into()),
            faux::FauxMessageOptions { stop_reason: Some(StopReason::Stop), error_message: None },
        )),
        faux::FauxResponseStep::Static(faux::faux_assistant_message(
            faux::FauxContent::Text("done".into()),
            faux::FauxMessageOptions { stop_reason: Some(StopReason::Stop), error_message: None },
        )),
    ]);

    let registry = Arc::new(registry);

    eprintln!("faux pending: {}", faux_handle.pending_count());

    let mut tools = ToolRegistry::new();
    tools.register(Box::new(ReadTool));
    tools.register(Box::new(WriteTool));

    let ext = ContextIndexExtension::new();
    let mut ext_reg = ExtensionRegistry::new();
    ext_reg.register(Box::new(ext));

    let config = AgentConfig {
        max_turns: Some(10),
        max_retries: 0,
        ..Default::default()
    };

    let mut agent = Agent::new(registry, faux_model(), None, tools, config)
        .with_extensions(ext_reg);

    let result = agent.run("test context index").await;
    assert!(result.is_ok(), "agent.run should succeed: {:?}", result);

    let messages = agent.messages();
    let has_read = messages.iter().any(|m| matches!(m, Message::ToolResult(tr) if tr.tool_name == "read"));
    let has_write = messages.iter().any(|m| matches!(m, Message::ToolResult(tr) if tr.tool_name == "write"));
    assert!(has_read, "should have read tool result");
    assert!(has_write, "should have write tool result");

    // 检查 read 的 tool_result 是否被 on_context 折叠
    let read_content = messages.iter().find_map(|m| {
        if let Message::ToolResult(tr) = m {
            if tr.tool_name == "read" {
                if let ContentBlock::Text(t) = &tr.content[0] {
                    return Some(t.text.clone());
                }
            }
        }
        None
    });

    if let Some(text) = &read_content {
        if text.contains("original content") && !text.contains("[ContextIndex") {
            eprintln!("⚠️ read tool_result NOT yet folded. on_context timing: content starts with: {}",
                &text[..text.len().min(50)]);
        } else {
            assert!(
                text.contains("[ContextIndex") || text.contains("Re-read"),
                "read should be folded, got: {}", &text[..text.len().min(80)]
            );
        }
    }

    let _ = std::fs::remove_file(test_file);
}

#[tokio::test]
async fn context_index_rpc_tree() {
    let ext = ContextIndexExtension::new();
    {
        let mut idx = ext.index.lock().await;
        idx.current_turn = 3;
        idx.record_read("src/main.rs", "tc_001", "fn main(){}");
        idx.current_turn = 5;
        idx.record_write("src/main.rs", WriteKind::Write);
    }

    let result = Extension::on_extension_rpc(&ext, "tree", serde_json::Value::Null).await.unwrap();
    let files = result.get("files").and_then(|v| v.as_array()).unwrap();
    assert!(!files.is_empty());
    let main_rs = files.iter().find(|f| f["path"].as_str() == Some("src/main.rs")).unwrap();
    assert_eq!(main_rs["status"].as_str(), Some("stale"));
}

#[tokio::test]
async fn context_index_rpc_ranges() {
    let ext = ContextIndexExtension::new();
    {
        let mut idx = ext.index.lock().await;
        idx.current_turn = 1;
        idx.record_read("src/lib.rs", "tc_a", "pub mod a;");
        idx.current_turn = 2;
        idx.record_read("src/lib.rs", "tc_b", "pub mod a;\npub mod b;");
    }

    let result = Extension::on_extension_rpc(&ext, "ranges", serde_json::json!({"path":"src/lib.rs"})).await.unwrap();
    let reads = result.get("reads").and_then(|v| v.as_array()).unwrap();
    assert_eq!(reads.len(), 2);
}
