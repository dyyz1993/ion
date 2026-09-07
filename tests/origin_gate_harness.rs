//! OriginGate harness（INPUT_ORIGIN 消费侧）。
//!
//! 证明端到端行为：monitor 轮的工具调用被 OriginGate 拒绝 → 转错误 ToolResult
//! （agent 不中断）→ faux 收到错误后正常收尾；user 轮同一工具正常执行。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use ion::agent::agent_loop::{Agent, AgentConfig};
use ion::agent::error::AgentResult;
use ion::agent::extension::{Extension, ExtensionRunner};
use ion::agent::messages::Message;
use ion::agent::tool::{Tool, ToolRegistry};
use ion_provider::faux;
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
        context_window: 128_000,
        max_tokens: 4096,
        compat: None,
        headers: None,
    }
}

struct CountingTool {
    executions: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for CountingTool {
    fn name(&self) -> &str {
        "origin_probe"
    }

    fn description(&self) -> &str {
        "counts executions"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }

    async fn execute(
        &self,
        _args: serde_json::Value,
        _rt: &dyn ion::runtime::Runtime,
    ) -> AgentResult<String> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        Ok("executed".into())
    }
}

fn tool_call_step() -> faux::FauxResponseStep {
    faux::FauxResponseStep::Static(faux::faux_assistant_message(
        faux::FauxContent::Single(faux::faux_tool_call("origin_probe", serde_json::json!({}))),
        faux::FauxMessageOptions {
            stop_reason: Some(StopReason::ToolUse),
            error_message: None,
        },
    ))
}

fn final_step() -> faux::FauxResponseStep {
    faux::FauxResponseStep::Static(faux::faux_assistant_message(
        faux::FauxContent::Text("done".into()),
        faux::FauxMessageOptions {
            stop_reason: None,
            error_message: None,
        },
    ))
}

#[tokio::test]
async fn monitor_round_denies_tool_user_round_executes() {
    // 响应序列：user 轮 tool_call→final；monitor 轮 tool_call→final
    let mut registry = ApiRegistry::new();
    let faux_handle = faux::register_faux(&mut registry);
    faux_handle.set_responses(vec![
        tool_call_step(),
        final_step(),
        tool_call_step(),
        final_step(),
    ]);

    let executions = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::new();
    tools.register(Box::new(CountingTool {
        executions: executions.clone(),
    }));

    let mut extensions = ExtensionRunner::new();
    let mut deny_map = HashMap::new();
    deny_map.insert("monitor".to_string(), vec!["origin_probe".to_string()]);
    extensions.register(Box::new(ion::agent::origin_gate::OriginGate::from_map(
        deny_map,
    )));

    let config = AgentConfig {
        max_turns: Some(5),
        max_retries: 0,
        retry_on_no_tool_use: 0,
        ..Default::default()
    };
    let mut agent = Agent::new(Arc::new(registry), faux_model(), None, tools, config)
        .with_extensions(extensions);

    // ── user 轮：origin_probe 正常执行 ──
    agent.input_origin = "user".to_string();
    agent
        .run("user round: please call origin_probe")
        .await
        .expect("user round run");
    assert_eq!(
        executions.load(Ordering::SeqCst),
        1,
        "user 轮工具应执行一次"
    );

    // ── monitor 轮：同一工具被 OriginGate 拒绝（不执行），agent 继续收尾 ──
    agent.input_origin = "monitor".to_string();
    agent
        .run("monitor round: please call origin_probe")
        .await
        .expect("monitor round run must not abort agent");
    assert_eq!(
        executions.load(Ordering::SeqCst),
        1,
        "monitor 轮工具被拒，执行数不应增长"
    );

    // ── 拒绝转错误 ToolResult 的结构断言：monitor 轮后有 toolResult 且 content 带 denied ──
    let denied_tool_results: Vec<String> = agent
        .messages()
        .iter()
        .filter_map(|m| match m {
            Message::ToolResult(tr) => Some(
                tr.content
                    .iter()
                    .filter_map(|c| match c {
                        ContentBlock::Text(t) => Some(t.text.clone()),
                        _ => None,
                    })
                    .collect::<String>(),
            ),
            _ => None,
        })
        .filter(|text| text.contains("OriginGate"))
        .collect();
    assert_eq!(
        denied_tool_results.len(),
        1,
        "monitor 轮应产生一条 OriginGate 拒绝的 ToolResult"
    );
}
