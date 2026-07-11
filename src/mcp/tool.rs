//! McpTool — 把 MCP server 提供的工具适配成 ION 的 Tool trait。

use std::sync::Arc;

use async_trait::async_trait;

use crate::agent::error::{AgentError, AgentResult};
use crate::agent::tool::Tool;
use crate::mcp::{DiscoveredTool, McpManager};
use crate::runtime::Runtime;

/// MCP 工具适配器。每个 MCP server 工具对应一个 McpTool 实例。
pub struct McpTool {
    full_name: String,
    description: String,
    parameters: serde_json::Value,
    server_name: String,
    tool_name: String,
    manager: Arc<McpManager>,
}

impl McpTool {
    pub fn new(tool: &DiscoveredTool, manager: Arc<McpManager>) -> Self {
        // 从 full_name "mcp__server__tool" 拆出 server 和 tool
        let parts: Vec<&str> = tool.full_name.splitn(3, "__").collect();
        // parts = ["mcp", server, tool]
        let server_name = parts.get(1).copied().unwrap_or("").to_string();
        let tool_name = parts.get(2).copied().unwrap_or("").to_string();
        Self {
            full_name: tool.full_name.clone(),
            description: tool.description.clone(),
            parameters: tool.input_schema.clone(),
            server_name,
            tool_name,
            manager,
        }
    }
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.full_name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn parameters(&self) -> serde_json::Value {
        self.parameters.clone()
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _rt: &dyn Runtime,
    ) -> AgentResult<String> {
        self.manager
            .call_tool(&self.server_name, &self.tool_name, args)
            .await
            .map_err(|e| AgentError::Tool(e))
    }
}
