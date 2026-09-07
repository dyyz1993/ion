//! INPUT_ORIGIN 消费侧① harness：按 origin 隐藏工具（schema 级过滤）。
//!
//! 证明：monitor 轮 provider 请求的 tools 列表不含被隐藏工具
//! （faux Factory 捕获 Context 断言）；user 轮同一工具仍在列表中。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use ion::agent::agent_loop::{Agent, AgentConfig};
use ion::agent::error::AgentResult;
use ion::agent::extension::ExtensionRunner;
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

struct MarkerTool(&'static str);

#[async_trait]
impl Tool for MarkerTool {
    fn name(&self) -> &str {
        self.0
    }
    fn description(&self) -> &str {
        "marker"
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        _rt: &dyn ion::runtime::Runtime,
    ) -> AgentResult<String> {
        Ok("ok".into())
    }
}

#[tokio::test]
async fn monitor_round_hides_tool_from_provider_request() {
    let mut registry = ApiRegistry::new();
    let handle = faux::register_faux(&mut registry);

    // Factory 捕获每次请求的 Context.tools（经 set_responses 装载）
    let seen_tools: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_c = seen_tools.clone();
    let make_step = || {
        let seen_c = seen_c.clone();
        faux::FauxResponseStep::Factory(Box::new(move |ctx, _, _, _| {
            let names: Vec<String> = ctx
                .tools
                .as_ref()
                .map(|ts| ts.iter().map(|t| t.name.clone()).collect())
                .unwrap_or_default();
            seen_c.lock().unwrap().push(names);
            faux::faux_assistant_message(
                faux::FauxContent::Text("done".into()),
                faux::FauxMessageOptions {
                    stop_reason: None,
                    error_message: None,
                },
            )
        }))
    };
    handle.set_responses(vec![make_step(), make_step()]);

    let mut tools = ToolRegistry::new();
    tools.register(Box::new(MarkerTool("keep_me")));
    tools.register(Box::new(MarkerTool("hide_me")));

    let mut hide = HashMap::new();
    hide.insert("monitor".to_string(), vec!["hide_me".to_string()]);

    let config = AgentConfig {
        max_turns: Some(3),
        max_retries: 0,
        retry_on_no_tool_use: 0,
        ..Default::default()
    };
    let mut agent = Agent::new(Arc::new(registry), faux_model(), None, tools, config)
        .with_extensions(ExtensionRunner::new());
    agent.set_origin_hide_tools(hide);

    // user 轮：两个工具都在
    agent.input_origin = "user".to_string();
    agent.run("round one").await.expect("user round");

    // monitor 轮：hide_me 从 schema 消失
    agent.input_origin = "monitor".to_string();
    agent.run("round two").await.expect("monitor round");

    let seen = seen_tools.lock().unwrap().clone();
    assert_eq!(seen.len(), 2, "应有两次 provider 请求，got {}", seen.len());
    assert!(
        seen[0].contains(&"hide_me".to_string()),
        "user 轮应看到 hide_me: {:?}",
        seen[0]
    );
    assert!(
        !seen[1].contains(&"hide_me".to_string()),
        "monitor 轮不应看到 hide_me: {:?}",
        seen[1]
    );
    assert!(
        seen[1].contains(&"keep_me".to_string()),
        "monitor 轮保留未隐藏工具: {:?}",
        seen[1]
    );
}
