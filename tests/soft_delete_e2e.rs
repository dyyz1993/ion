//! SOFT_DELETE_COMPACT 集成测试 — Agent mark_deleted/mark_summarized/restore 验证
//!
//! 测试 Agent 直接操作 self.messages 的软删除/折叠/恢复链路。

use std::sync::Arc;

use ion::agent::agent_loop::{Agent, AgentConfig};
use ion::agent::messages::Message;
use ion::agent::tool::ToolRegistry;
use ion_provider::registry::ApiRegistry;
use ion_provider::types::*;

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

fn build_agent() -> Agent {
    let mut registry = ApiRegistry::new();
    let _ = ion_provider::faux::register_faux(&mut registry);
    let registry = Arc::new(registry);
    let tools = ToolRegistry::new();
    let config = AgentConfig {
        max_retries: 0,
        ..Default::default()
    };
    Agent::new(registry, faux_model(), None, tools, config)
}

fn user_msg(text: &str) -> Message {
    Message::User(UserMessage {
        role: "user".into(),
        content: vec![ContentBlock::Text(TextContent {
            text: text.into(),
            text_signature: None,
        })],
        timestamp: 0,
    })
}

fn asst_msg(text: &str) -> Message {
    Message::Assistant(AssistantMessage {
        role: "assistant".into(),
        content: vec![AssistantContentBlock::Text(TextContent {
            text: text.into(),
            text_signature: None,
        })],
        api: "faux".into(),
        provider: "faux".into(),
        model: "faux-test".into(),
        response_model: None,
        response_id: None,
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        timestamp: 0,
    })
}

#[tokio::test]
async fn mark_deleted_removes_messages() {
    let mut agent = build_agent();

    // Push 4 条消息
    agent.push_message(user_msg("hello"));
    agent.push_message(asst_msg("hi"));
    agent.push_message(user_msg("bye"));
    agent.push_message(asst_msg("goodbye"));

    assert_eq!(agent.messages().len(), 4);

    // 软删除 index 1 和 3（两条 assistant）
    agent.mark_deleted(&[1, 3], &["e1".into(), "e3".into()]).await;

    assert_eq!(agent.messages().len(), 2);
    // 剩余应该是 index 0 (user "hello") 和 index 2 (user "bye")
    let remaining: Vec<&str> = agent.messages().iter().filter_map(|m| {
        if let Message::User(u) = m {
            if let ContentBlock::Text(t) = &u.content[0] { return Some(t.text.as_str()); }
        }
        None
    }).collect();
    assert!(remaining.contains(&"hello"));
    assert!(remaining.contains(&"bye"));
}

#[tokio::test]
async fn mark_summarized_replaces_with_branch_summary() {
    let mut agent = build_agent();

    agent.push_message(user_msg("讨论1"));
    agent.push_message(asst_msg("回复1"));
    agent.push_message(user_msg("讨论2"));
    agent.push_message(asst_msg("回复2"));
    agent.push_message(user_msg("后续"));

    assert_eq!(agent.messages().len(), 5);

    // 折叠 index 1-3（3 条消息）
    agent.mark_summarized(&[1, 2, 3], &["e1".into(), "e2".into(), "e3".into()], "这段讨论已折叠").await;

    assert_eq!(agent.messages().len(), 3); // 5 - 3 + 1(BranchSummary) = 3

    // 应该有 BranchSummary
    let has_branch = agent.messages().iter().any(|m| matches!(m, Message::BranchSummary(_)));
    assert!(has_branch, "should have BranchSummary");

    // BranchSummary 的 summary 正确
    let summary_text = agent.messages().iter().find_map(|m| {
        if let Message::BranchSummary(bs) = m { Some(bs.summary.as_str()) } else { None }
    });
    assert_eq!(summary_text, Some("这段讨论已折叠"));
}

#[tokio::test]
async fn restore_entries_clears_state() {
    let mut agent = build_agent();

    agent.push_message(user_msg("keep"));
    agent.push_message(asst_msg("delete me"));

    // 软删除
    agent.mark_deleted(&[1], &["e1".into()]).await;
    assert_eq!(agent.messages().len(), 1);
    assert!(agent.deleted_ids().contains("e1"));

    // 恢复（清除 deleted_entry_ids，但消息已被 remove 了——需 reload）
    agent.restore_entries(&["e1".into()]);
    assert!(!agent.deleted_ids().contains("e1"));
}

#[tokio::test]
async fn deleted_ids_tracks_all_deleted() {
    let mut agent = build_agent();

    agent.push_message(user_msg("a"));
    agent.push_message(asst_msg("b"));
    agent.push_message(user_msg("c"));

    agent.mark_deleted(&[0, 2], &["id_a".into(), "id_c".into()]).await;

    assert_eq!(agent.messages().len(), 1);
    assert!(agent.deleted_ids().contains("id_a"));
    assert!(agent.deleted_ids().contains("id_c"));
    assert!(!agent.deleted_ids().contains("id_b"));
}

#[tokio::test]
async fn mark_deleted_out_of_range_index_ignored() {
    let mut agent = build_agent();
    agent.push_message(user_msg("only"));

    // index 5 超出范围，应该被安全跳过
    agent.mark_deleted(&[0, 5], &["e0".into(), "e5".into()]).await;

    assert_eq!(agent.messages().len(), 0); // 只有 index 0 被删
}

#[tokio::test]
async fn mark_summarized_empty_indices_noop() {
    let mut agent = build_agent();
    agent.push_message(user_msg("keep"));

    agent.mark_summarized(&[], &[], "nothing").await;

    assert_eq!(agent.messages().len(), 1); // 没有变化
}
