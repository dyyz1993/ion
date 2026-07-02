use std::sync::Arc;

use crate::agent::agent_loop::{Agent, AgentConfig};
use crate::agent::extension::ExtensionRegistry;
use crate::agent::tool::ToolRegistry;
use crate::error::IonError;
use crate::types::{SessionState, TaskResult};
use crate::worker::Worker;
use async_trait::async_trait;
use ion_provider::registry::ApiRegistry;
use ion_provider::types::*;

pub struct AgentWorker {
    agent: Option<Agent>,
    registry: Arc<ApiRegistry>,
    model: Model,
    tools: ToolRegistry,
    config: AgentConfig,
    extensions: ExtensionRegistry,
    system_prompt: Option<String>,
}

impl AgentWorker {
    pub fn new(registry: Arc<ApiRegistry>, model: Model, system_prompt: Option<String>) -> Self {
        Self {
            agent: None,
            registry,
            model,
            tools: ToolRegistry::new(),
            config: AgentConfig::default(),
            extensions: ExtensionRegistry::new(),
            system_prompt,
        }
    }
    pub fn with_tools(mut self, tools: ToolRegistry) -> Self {
        self.tools = tools;
        self
    }
    pub fn with_extensions(mut self, extensions: ExtensionRegistry) -> Self {
        self.extensions = extensions;
        self
    }
    pub fn with_config(mut self, config: AgentConfig) -> Self {
        self.config = config;
        self
    }
}

#[async_trait]
impl Worker for AgentWorker {
    async fn connect(&mut self) -> crate::error::IonResult<()> {
        let tools = std::mem::take(&mut self.tools);
        let extensions = std::mem::take(&mut self.extensions);
        let agent = Agent::new(
            Arc::clone(&self.registry),
            self.model.clone(),
            self.system_prompt.clone(),
            tools,
            self.config.clone(),
        )
        .with_extensions(extensions);
        self.agent = Some(agent);
        Ok(())
    }

    async fn prompt(&mut self, text: String) -> crate::error::IonResult<TaskResult> {
        let agent = self
            .agent
            .as_mut()
            .ok_or_else(|| IonError::Worker("not connected".into()))?;
        agent
            .run(text)
            .await
            .map_err(|e| IonError::Worker(e.to_string()))?;
        let output = agent
            .messages()
            .iter()
            .rev()
            .find_map(|msg| match msg {
                Message::Assistant(a) => a.content.iter().find_map(|b| match b {
                    AssistantContentBlock::Text(t) if !t.text.is_empty() => Some(t.text.clone()),
                    _ => None,
                }),
                _ => None,
            })
            .unwrap_or_default();
        Ok(TaskResult::ok(output))
    }

    async fn steer(&mut self, msg: String) -> crate::error::IonResult<()> {
        if let Some(agent) = self.agent.as_mut() {
            agent.steer(Message::User(UserMessage {
                role: "user".into(),
                content: vec![ContentBlock::Text(TextContent {
                    text: msg,
                    text_signature: None,
                })],
                timestamp: now_ms(),
            }));
        }
        Ok(())
    }

    async fn state(&mut self) -> crate::error::IonResult<SessionState> {
        if let Some(ref agent) = self.agent {
            Ok(SessionState {
                message_count: agent.messages().len() as u64,
                turn_index: 0,
                summary: Some(format!("{} msgs", agent.messages().len())),
            })
        } else {
            Ok(SessionState::default())
        }
    }

    async fn dispose(&mut self) -> crate::error::IonResult<()> {
        self.agent = None;
        Ok(())
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
