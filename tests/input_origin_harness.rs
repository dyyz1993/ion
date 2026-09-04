//! INPUT_ORIGIN harness（docs/design/INPUT_ORIGIN.md）。
//!
//! 验证两条：
//! 1. Agent.input_origin 缺省 "user"，经 InputContext.origin 暴露给 on_input 钩子；
//! 2. prompt 处理赋值后（模拟 worker_rpc：agent.input_origin = "monitor"），
//!    钩子读到 "monitor"——扩展可据此对 monitor 轮做差异化处理（改写提示词等）。

use std::sync::Arc;

use async_trait::async_trait;
use ion::agent::agent_loop::{Agent, AgentConfig};
use ion::agent::error::AgentResult;
use ion::agent::extension::{Extension, ExtensionRunner};
use ion::agent::tool::ToolRegistry;
use ion_provider::faux;
use ion_provider::registry::ApiRegistry;
use std::sync::Mutex;

/// 记录 on_input 观察到的 origin 序列
struct OriginProbe {
    seen: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Extension for OriginProbe {
    fn name(&self) -> &str {
        "origin_probe"
    }

    async fn on_input(&self, ctx: &mut ion::agent::extension::InputContext) -> AgentResult<()> {
        self.seen.lock().unwrap().push(ctx.origin.clone());
        // monitor 轮改写提示词的示例：给文本加前缀（诉求④的能力证明）
        if ctx.origin == "monitor" {
            ctx.text = format!("[定时任务] {}", ctx.text);
        }
        Ok(())
    }
}

fn faux_model() -> ion_provider::types::Model {
    ion_provider::types::Model {
        id: "faux-test".into(),
        name: "Faux Test".into(),
        api: "faux".into(),
        provider: "faux".into(),
        base_url: "".into(),
        reasoning: false,
        input: vec!["text".into()],
        cost: ion_provider::types::Cost::default(),
        context_window: 128_000,
        max_tokens: 4096,
        compat: None,
        headers: None,
    }
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
async fn input_origin_flows_to_on_input_hook() {
    let mut registry = ApiRegistry::new();
    let faux_handle = faux::register_faux(&mut registry);
    faux_handle.set_responses(vec![final_step(), final_step()]);

    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut extensions = ExtensionRunner::new();
    extensions.register(Box::new(OriginProbe {
        seen: seen.clone(),
    }));

    let config = AgentConfig {
        max_turns: Some(5),
        max_retries: 0,
        retry_on_no_tool_use: 0,
        ..Default::default()
    };
    let mut agent = Agent::new(
        Arc::new(registry),
        faux_model(),
        None,
        ToolRegistry::new(),
        config,
    )
    .with_extensions(extensions);

    // ① 缺省 user：不设置时钩子应读到 "user"
    agent.run("first").await.expect("run 1");

    // ② 模拟 worker_rpc prompt(params.origin=monitor) 的赋值
    agent.input_origin = "monitor".to_string();
    agent.run("second").await.expect("run 2");

    let seen = seen.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec!["user".to_string(), "monitor".to_string()],
        "on_input 应依次读到 user（缺省）与 monitor（显式赋值）"
    );

    // ③ monitor 轮改写提示词：user 消息文本带前缀（钩子可改 ctx.text 的既有能力）
    let user_texts: Vec<String> = agent
        .messages()
        .iter()
        .filter_map(|m| match m {
            ion::agent::messages::Message::User(u) => Some(
                u.content
                    .iter()
                    .filter_map(|c| match c {
                        ion_provider::types::ContentBlock::Text(t) => Some(t.text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(""),
            ),
            _ => None,
        })
        .collect();
    assert!(
        user_texts.iter().any(|t| t.starts_with("[定时任务] ")),
        "monitor 轮的 user 消息应带改写前缀，实际: {:?}",
        user_texts
    );
    // user 轮（第一轮）不带前缀
    assert!(
        user_texts.first().map(|t| !t.starts_with("[定时任务]")).unwrap_or(false),
        "user 轮不应被改写: {:?}",
        user_texts
    );
}
