//! 空补全（empty completion）harness — 治"幻影 turn"。
//!
//! 生产实测（zai 网关）：LLM 偶发返回空 content 的补全——不报错、不计会话条目，
//! 但烧掉一个 turn（turn 计数+1 而会话条目+0，remind currentTurn 37/40 vs 会话
//! 仅 4 条 assistant）。修复语义：空补全 = 可重试协议错误，走既有 retry 通路
//! （计 attempt、计 retry 预算、退避），换得真实补全，不烧 turn。
//!
//! 空补全精确定义（防误伤）：
//!   stop_reason 非 Error/Aborted 且 无 ToolCall 且 无有效 Text 且 无有效 Thinking
//!   （thinking-only 是合法响应：agent_loop 会把 thinking 作为 content 块落盘）
//!
//! 本 harness 用 FauxProvider 驱动完整 Agent 循环（零 LLM 成本、确定性）：
//!   红阶段（修复前）：空补全被当正常 turn 接受 → 只烧 1 次 LLM 调用、
//!                     会话无 assistant 条目、真实补全留在队列里没人消费
//!   绿阶段（修复后）：空补全触发重试 → 第 2 次 LLM 调用换得真实补全 →
//!                     会话有 assistant 条目，run() 正常收尾

use std::sync::Arc;

use ion::agent::agent_loop::{Agent, AgentConfig};
use ion::agent::tool::ToolRegistry;
use ion_provider::faux;
use ion_provider::registry::ApiRegistry;
use ion_provider::types::*;

fn faux_model(id: &str) -> Model {
    Model {
        id: id.into(),
        name: "Faux Test".into(),
        api: "faux".into(),
        provider: "faux".into(),
        base_url: "".into(),
        reasoning: false,
        input: vec!["text".into()],
        cost: Cost::default(),
        context_window: 128_000,
        max_tokens: 4096,
        compat: None,
        headers: None,
    }
}

/// 空补全：content 为空、stop_reason=Stop（网关"成功"返回但什么都没给）
fn empty_completion_step() -> faux::FauxResponseStep {
    faux::FauxResponseStep::Static(faux::faux_assistant_message(
        faux::FauxContent::Many(vec![]),
        faux::FauxMessageOptions {
            stop_reason: Some(StopReason::Stop),
            error_message: None,
        },
    ))
}

/// 纯空白文本补全（"   \n\t"）——与空补全同罪
fn whitespace_completion_step() -> faux::FauxResponseStep {
    faux::FauxResponseStep::Static(faux::faux_assistant_message(
        faux::FauxContent::Text("   \n\t  ".into()),
        faux::FauxMessageOptions {
            stop_reason: Some(StopReason::Stop),
            error_message: None,
        },
    ))
}

fn real_step(text: &str) -> faux::FauxResponseStep {
    faux::FauxResponseStep::Static(faux::faux_assistant_message(
        faux::FauxContent::Text(text.into()),
        faux::FauxMessageOptions {
            stop_reason: None,
            error_message: None,
        },
    ))
}

fn assistant_texts(agent: &Agent) -> Vec<String> {
    agent
        .messages()
        .iter()
        .filter_map(|m| match m {
            ion::agent::messages::Message::Assistant(a) => Some(
                a.content
                    .iter()
                    .filter_map(|c| match c {
                        AssistantContentBlock::Text(t) => Some(t.text.clone()),
                        _ => None,
                    })
                    .collect::<String>(),
            ),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn empty_completion_retries_and_lands_real_answer() {
    let mut registry = ApiRegistry::new();
    let faux_handle = faux::register_faux(&mut registry);
    // 队列：空补全 → 真实补全。修复后第 1 次调用被判协议错误并重试，
    // 第 2 次调用消费真实补全。
    faux_handle.set_responses(vec![empty_completion_step(), real_step("real answer")]);

    let config = AgentConfig {
        max_turns: Some(5),
        max_retries: 3,
        retry_base_delay_ms: 1,
        retry_on_no_tool_use: 0,
        ..Default::default()
    };
    let mut agent = Agent::new(
        Arc::new(registry),
        faux_model("faux-empty"),
        None,
        ToolRegistry::new(),
        config,
    );

    agent
        .run("hello")
        .await
        .expect("run 应成功（重试后拿到真实补全）");

    // 核心断言：两次 LLM 调用（空补全重试 + 真实补全），而不是只烧一次
    assert_eq!(
        faux_handle.call_count(),
        2,
        "空补全应触发重试：第 1 次空、第 2 次真实"
    );
    // 会话必须有 assistant 条目（不烧 turn：turn 换来了真实内容）
    let texts = assistant_texts(&agent);
    assert!(
        texts.iter().any(|t| t.contains("real answer")),
        "会话应包含真实补全的 assistant 条目，实际: {texts:?}"
    );
}

#[tokio::test]
async fn whitespace_completion_also_retries() {
    let mut registry = ApiRegistry::new();
    let faux_handle = faux::register_faux(&mut registry);
    faux_handle.set_responses(vec![whitespace_completion_step(), real_step("ok")]);

    let config = AgentConfig {
        max_turns: Some(5),
        max_retries: 3,
        retry_base_delay_ms: 1,
        retry_on_no_tool_use: 0,
        ..Default::default()
    };
    let mut agent = Agent::new(
        Arc::new(registry),
        faux_model("faux-ws"),
        None,
        ToolRegistry::new(),
        config,
    );

    agent.run("hi").await.expect("run 应成功");
    assert_eq!(
        faux_handle.call_count(),
        2,
        "纯空白补全与空补全同罪：应重试"
    );
}

#[tokio::test]
async fn tool_call_with_empty_text_is_not_empty_completion() {
    // 防误伤：纯 tool_call 响应（content 无文本）是合法的，绝不能触发重试
    let mut registry = ApiRegistry::new();
    let faux_handle = faux::register_faux(&mut registry);
    faux_handle.set_responses(vec![
        faux::FauxResponseStep::Static(faux::faux_assistant_message(
            faux::FauxContent::Single(faux::faux_tool_call("noop", serde_json::json!({}))),
            faux::FauxMessageOptions {
                stop_reason: Some(StopReason::ToolUse),
                error_message: None,
            },
        )),
        real_step("done after tool"),
    ]);

    let config = AgentConfig {
        max_turns: Some(5),
        max_retries: 3,
        retry_base_delay_ms: 1,
        retry_on_no_tool_use: 0,
        ..Default::default()
    };
    let mut agent = Agent::new(
        Arc::new(registry),
        faux_model("faux-tool"),
        None,
        ToolRegistry::new(),
        config,
    );

    agent.run("call the tool").await.expect("run 应成功");
    // tool_call 轮 + 收尾轮 = 恰好 2 次调用（若误判空补全会变 3+）
    assert_eq!(
        faux_handle.call_count(),
        2,
        "纯 tool_call 响应不应被误判为空补全"
    );
}

#[tokio::test]
async fn thinking_only_completion_is_legal() {
    // 防误伤：thinking-only（无 text、无 tool_calls）是合法响应——
    // agent_loop 会把 thinking 作为 content 块落盘，必须当正常 turn 接受
    let mut registry = ApiRegistry::new();
    let faux_handle = faux::register_faux(&mut registry);
    faux_handle.set_responses(vec![faux::FauxResponseStep::Static(
        faux::faux_assistant_message(
            faux::FauxContent::Single(faux::faux_thinking("let me think this through")),
            faux::FauxMessageOptions {
                stop_reason: Some(StopReason::Stop),
                error_message: None,
            },
        ),
    )]);

    let config = AgentConfig {
        max_turns: Some(5),
        max_retries: 3,
        retry_base_delay_ms: 1,
        retry_on_no_tool_use: 0,
        ..Default::default()
    };
    let mut agent = Agent::new(
        Arc::new(registry),
        faux_model("faux-think"),
        None,
        ToolRegistry::new(),
        config,
    );

    agent.run("think").await.expect("run 应成功");
    assert_eq!(
        faux_handle.call_count(),
        1,
        "thinking-only 响应是合法的：恰好 1 次调用，不触发重试"
    );
    // thinking 应作为 content 落盘（正常 turn）
    let has_thinking = agent.messages().iter().any(|m| {
        matches!(
            m,
            ion::agent::messages::Message::Assistant(a)
                if a.content.iter().any(|c| matches!(c, AssistantContentBlock::Thinking(_)))
        )
    });
    assert!(has_thinking, "thinking-only 响应应作为 assistant 条目落盘");
}

#[tokio::test]
async fn all_empty_completions_exhaust_retries_then_fail() {
    // 重试耗尽：队列里全是空补全 → 计入 attempt 用完 max_retries → 报协议错误
    // （不无限重试，不让 worker 空转）
    let mut registry = ApiRegistry::new();
    let faux_handle = faux::register_faux(&mut registry);
    faux_handle.set_responses(vec![
        empty_completion_step(),
        empty_completion_step(),
        empty_completion_step(),
    ]);

    let config = AgentConfig {
        max_turns: Some(5),
        max_retries: 2,
        retry_base_delay_ms: 1,
        retry_on_no_tool_use: 0,
        ..Default::default()
    };
    let mut agent = Agent::new(
        Arc::new(registry),
        faux_model("faux-exhaust"),
        None,
        ToolRegistry::new(),
        config,
    );

    let result = agent.run("hello").await;
    assert!(result.is_err(), "空补全重试耗尽应报错而非静默接受幻影 turn");
    // 首次 + 2 次重试 = 3 次调用（受 max_retries 约束，不无限烧）
    assert!(
        faux_handle.call_count() <= 3,
        "调用次数应受 max_retries 约束，实际: {}",
        faux_handle.call_count()
    );
}
