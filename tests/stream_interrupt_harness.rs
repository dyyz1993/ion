//! 流中断（mid-stream EOF）harness — 治 zai 长流无声断。
//!
//! 生产实测（zai 网关，2026-09-15/16 多夜）：SSE 连接在收到 finish_reason 之前
//! 被掐断（代理断连/上游重启/网络抖动）。openai/cloudflare/mistral provider 在
//! EOF 无 finish_reason 时发 Error 事件（error_message="stream ended unexpectedly"），
//! 但 agent_loop 的 stream_with_retry 对 Ok 路径的 Error 收尾【不重试】——直接
//! 返回 inner_loop → 非溢出 Error 一律终止整个 run → worker 阵亡 → AUTO-RECOVERY
//! 重派抽签（前棒半成品靠运气接力）。
//!
//! 修复语义：EOF 签名错误（"stream ended unexpectedly"）视同空补全——走既有
//! retry 通路（计 attempt、计 retry 预算、退避），整段回答重来。
//! 非 EOF 语义的 in-stream 错误（配额/鉴权等）保持原路径不重试（防误伤）。
//!
//! 本 harness 用 FauxProvider 驱动完整 Agent 循环（零 LLM 成本、确定性）：
//!   红阶段（修复前）：流中断被当 run 级错误 → 只烧 1 次 LLM 调用、真实补全
//!                     留在队列里没人消费、会话无 assistant 条目
//!   绿阶段（修复后）：流中断触发重试 → 第 2 次调用换得真实补全 → run 正常收尾

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

/// 流中断形状：provider 层 EOF 无 finish_reason 时发 Error 事件，
/// error_message 用 openai/cloudflare/mistral 共用的签名字面量。
fn stream_eof_step() -> faux::FauxResponseStep {
    faux::FauxResponseStep::Static(faux::faux_assistant_message(
        faux::FauxContent::Many(vec![]),
        faux::FauxMessageOptions {
            stop_reason: Some(StopReason::Error),
            error_message: Some("stream ended unexpectedly".into()),
        },
    ))
}

/// 非 EOF 语义的 in-stream 错误（如配额/鉴权）——不应触发重试
fn other_stream_error_step() -> faux::FauxResponseStep {
    faux::FauxResponseStep::Static(faux::faux_assistant_message(
        faux::FauxContent::Many(vec![]),
        faux::FauxMessageOptions {
            stop_reason: Some(StopReason::Error),
            error_message: Some("quota exceeded for this key".into()),
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

fn test_config(max_retries: u32) -> AgentConfig {
    AgentConfig {
        max_turns: Some(5),
        max_retries,
        retry_base_delay_ms: 1,
        retry_on_no_tool_use: 0,
        ..Default::default()
    }
}

#[tokio::test]
async fn mid_stream_eof_retries_and_lands_real_answer() {
    let mut registry = ApiRegistry::new();
    let faux_handle = faux::register_faux(&mut registry);
    // 队列：流中断 → 真实补全。修复后第 1 次调用被判 EOF 签名错误并重试，
    // 第 2 次调用消费真实补全。
    faux_handle.set_responses(vec![
        stream_eof_step(),
        real_step("recovered answer"),
    ]);

    let mut agent = Agent::new(
        Arc::new(registry),
        faux_model("faux-eof"),
        None,
        ToolRegistry::new(),
        test_config(3),
    );

    agent
        .run("hello")
        .await
        .expect("run 应成功（流中断重试后拿到真实补全）");

    // 核心断言：两次 LLM 调用（中断重试 + 真实补全），而不是只烧一次就终止 run
    assert_eq!(
        faux_handle.call_count(),
        2,
        "流中断应触发重试：第 1 次中断、第 2 次真实"
    );
    let texts = assistant_texts(&agent);
    assert!(
        texts.iter().any(|t| t.contains("recovered answer")),
        "会话应包含真实补全的 assistant 条目，实际: {texts:?}"
    );
}

#[tokio::test]
async fn persistent_stream_eof_exhausts_retries_then_fails() {
    // 重试耗尽：队列里全是流中断 → 计入 attempt 用完 max_retries → 报协议错误
    // （不无限重试；agent_end error → AUTO-RECOVERY 既有通路接管）
    let mut registry = ApiRegistry::new();
    let faux_handle = faux::register_faux(&mut registry);
    faux_handle.set_responses(vec![
        stream_eof_step(),
        stream_eof_step(),
        stream_eof_step(),
    ]);

    let mut agent = Agent::new(
        Arc::new(registry),
        faux_model("faux-eof-exhaust"),
        None,
        ToolRegistry::new(),
        test_config(2),
    );

    let result = agent.run("hello").await;
    assert!(result.is_err(), "流中断重试耗尽应报错而非静默接受截断回答");
    // 首次 + 2 次重试 = 3 次调用（受 max_retries 约束，不无限烧）
    assert!(
        faux_handle.call_count() <= 3,
        "调用次数应受 max_retries 约束，实际: {}",
        faux_handle.call_count()
    );
}

#[tokio::test]
async fn non_eof_stream_error_keeps_old_no_retry_behavior() {
    // 防误伤：非 EOF 语义的 in-stream 错误（配额/鉴权等）保持原路径——
    // 不重试、直接终止 run（其重试/降级语义归 Err 路径的 tier fallback 管）
    let mut registry = ApiRegistry::new();
    let faux_handle = faux::register_faux(&mut registry);
    faux_handle.set_responses(vec![
        other_stream_error_step(),
        real_step("should not be consumed"),
    ]);

    let mut agent = Agent::new(
        Arc::new(registry),
        faux_model("faux-quota"),
        None,
        ToolRegistry::new(),
        test_config(3),
    );

    // 修复前后该行为都不变：非 EOF 错误不重试（可能 Err 也可能 Ok(Error) 收尾，
    // 都不算失败——只断言"没有多烧调用"）
    let _ = agent.run("hello").await;
    assert_eq!(
        faux_handle.call_count(),
        1,
        "非 EOF 语义错误不应触发重试（保持既有 tier-fallback 路径）"
    );
}
