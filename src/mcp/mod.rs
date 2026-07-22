//! MCP (Model Context Protocol) 客户端模块。
//!
//! 连接用户配置的 MCP server，发现工具，通过 `mcp__<server>__<tool>` 命名暴露给 LLM。
//! 详见 docs/design/MCP_SYSTEM.md。

pub mod tool;

use std::collections::HashMap;
use std::sync::Arc;

use crate::config::{IonConfig, McpServerConfig};

/// rmcp client 类型别名（RunningService deref 到 Peer，支持并发 call_tool）
pub type McpClient = rmcp::service::RunningService<rmcp::RoleClient, ()>;

/// 单个 MCP server 的运行时状态
struct ServerEntry {
    status: ServerStatus,
    /// 发现到的工具（连接成功后填充）
    tools: Vec<DiscoveredTool>,
    /// 发现到的资源（连接成功后填充）
    resources: Vec<DiscoveredResource>,
    /// 发现到的提示模板（连接成功后填充）
    prompts: Vec<DiscoveredPrompt>,
    /// 连接/调用错误（status=Error 时填充）
    error: Option<String>,
    /// rmcp 客户端句柄（连接成功后填充）
    client: Option<Arc<McpClient>>,
    /// 运行时 toggle 覆盖（disabled?），None=用配置默认值
    runtime_disabled: Option<bool>,
    /// 自动重连已尝试次数（连接成功后重置为 0）
    reconnect_attempts: u32,
}

impl ServerEntry {
    fn new() -> Self {
        Self {
            status: ServerStatus::Disconnected,
            tools: Vec::new(),
            resources: Vec::new(),
            prompts: Vec::new(),
            error: None,
            client: None,
            runtime_disabled: None,
            reconnect_attempts: 0,
        }
    }

    /// 当前是否 disabled（运行时覆盖 > 配置默认）
    fn effective_disabled(&self, cfg: &McpServerConfig) -> bool {
        self.runtime_disabled.unwrap_or_else(|| cfg.is_disabled())
    }
	}
#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerStatus {
    Disconnected,
    Connecting,
    Connected,
    Error,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct DiscoveredTool {
    /// 完整工具名：mcp__<server>__<tool>
    pub full_name: String,
    /// server 端原始工具名
    pub original_name: String,
    pub description: String,
    /// 工具参数 JSON Schema（原样保留）
    pub input_schema: serde_json::Value,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct DiscoveredResource {
    pub uri: String,
    pub name: String,
    pub description: Option<String>,
    pub mime_type: Option<String>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct DiscoveredPrompt {
    pub name: String,
    pub description: Option<String>,
}

/// connect_one 的返回值
struct ConnectResult {
    client: McpClient,
    tools: Vec<DiscoveredTool>,
    resources: Vec<DiscoveredResource>,
    prompts: Vec<DiscoveredPrompt>,
}

/// MCP 连接管理器。负责连接 server、发现工具、代理工具调用。
pub struct McpManager {
    /// server name → 连接条目
    servers: tokio::sync::Mutex<HashMap<String, ServerEntry>>,
    /// 连接变更回调（用于推送 mcp_connection_change 事件给外部）
    on_status_change: tokio::sync::Mutex<Option<Box<dyn Fn(&str, &ServerStatus) + Send + Sync>>>,
    /// 原始配置（用于 restart / toggle）
    config: HashMap<String, McpServerConfig>,
}

impl McpManager {
    pub fn new(config: HashMap<String, McpServerConfig>) -> Self {
        let servers: HashMap<String, ServerEntry> = config
            .keys()
            .map(|name| (name.clone(), ServerEntry::new()))
            .collect();
        Self {
            servers: tokio::sync::Mutex::new(servers),
            on_status_change: tokio::sync::Mutex::new(None),
            config,
        }
    }

    /// 设置连接变更回调（ion_worker 启动时调，用于推送事件到 stdout/subscribe）
    pub async fn set_on_status_change(&self, f: impl Fn(&str, &ServerStatus) + Send + Sync + 'static) {
        *self.on_status_change.lock().await = Some(Box::new(f));
    }

    /// 内部：通知状态变更
    async fn notify_status(&self, name: &str, status: &ServerStatus) {
        let cb = self.on_status_change.lock().await;
        if let Some(ref f) = *cb {
            f(name, status);
        }
    }

    /// 是否没有任何 server 配置（零开销判断）
    pub fn is_empty(&self) -> bool {
        self.config.is_empty()
    }

    /// 配置的 server 总数
    pub fn server_count(&self) -> usize {
        self.config.len()
    }

    /// 已成功连接的 server 数
    pub async fn connected_count(&self) -> usize {
        let servers = self.servers.lock().await;
        servers
            .values()
            .filter(|e| matches!(e.status, ServerStatus::Connected))
            .count()
    }

    /// 当前已连接的 MCP server 数量（等价于 connected_count）
    pub async fn connected_server_count(&self) -> usize {
        self.connected_count().await
    }

    // ── 自动重连（Phase 3）──
    // 参数：base 1s → max 30s，最多 3 次（对齐 pi）
    const RECONNECT_BASE_DELAY_MS: u64 = 1000;
    const RECONNECT_MAX_DELAY_MS: u64 = 30000;
    const RECONNECT_MAX_ATTEMPTS: u32 = 3;

    /// 计算第 n 次重连的延迟（指数退避：1s → 2s → 4s...，上限 30s）
    fn reconnect_delay(attempt: u32) -> std::time::Duration {
        let delay_ms = Self::RECONNECT_BASE_DELAY_MS
            .saturating_mul(1u64 << attempt.min(5)); // 防溢出，最多 2^5=32
        std::time::Duration::from_millis(delay_ms.min(Self::RECONNECT_MAX_DELAY_MS))
    }

    /// 启动后台重连监控 task。
    /// 每 2 秒检查一次所有 server：如果 client.is_closed() 且未 disabled 且未超重试上限 → 重连。
    /// 在 ion_worker 启动时调用一次（connect_all 之后）。
    pub fn spawn_reconnect_monitor(self: &Arc<Self>) {
        let manager = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                manager.check_and_reconnect().await;
            }
        });
    }

    /// 检查所有 server 的连接状态，必要时重连
    async fn check_and_reconnect(&self) {
        // 收集需要重连的 server
        let needs_reconnect: Vec<(String, McpServerConfig, u32)> = {
            let servers = self.servers.lock().await;
            servers
                .iter()
                .filter_map(|(name, entry)| {
                    // 跳过 disabled / 非 connected 的
                    let cfg = self.config.get(name)?;
                    if entry.effective_disabled(cfg) {
                        return None;
                    }
                    // 检查连接是否断开
                    let is_broken = match &entry.client {
                        Some(client) => client.is_closed(),
                        None => false, // 没 client 的不在这里处理
                    };
                    if !is_broken {
                        return None;
                    }
                    // 检查重试次数
                    if entry.reconnect_attempts >= Self::RECONNECT_MAX_ATTEMPTS {
                        return None;
                    }
                    Some((name.clone(), cfg.clone(), entry.reconnect_attempts))
                })
                .collect()
        };

        for (name, cfg, attempt) in needs_reconnect {
            // 标记 reconnecting
            let delay = Self::reconnect_delay(attempt);
            tracing::warn!(
                "[mcp] '{}' connection lost, reconnect attempt {} (delay {:?})",
                name, attempt + 1, delay
            );

            {
                let mut servers = self.servers.lock().await;
                if let Some(entry) = servers.get_mut(&name) {
                    entry.status = ServerStatus::Connecting;
                    entry.reconnect_attempts = attempt + 1;
                }
            }
            self.notify_status(&name, &ServerStatus::Connecting).await;

            // 等待退避时间
            tokio::time::sleep(delay).await;

            // 尝试重连
            match Self::connect_one(&name, &cfg).await {
                Ok(result) => {
                    let mut servers = self.servers.lock().await;
                    if let Some(entry) = servers.get_mut(&name) {
                        entry.status = ServerStatus::Connected;
                        entry.tools = result.tools;
                        entry.resources = result.resources;
                        entry.prompts = result.prompts;
                        entry.error = None;
                        entry.client = Some(Arc::new(result.client));
                        entry.reconnect_attempts = 0; // 重置计数
                    }
                    tracing::info!("[mcp] '{}' reconnected successfully", name);
                    self.notify_status(&name, &ServerStatus::Connected).await;
                }
                Err(e) => {
                    let mut servers = self.servers.lock().await;
                    if let Some(entry) = servers.get_mut(&name) {
                        if entry.reconnect_attempts >= Self::RECONNECT_MAX_ATTEMPTS {
                            entry.status = ServerStatus::Error;
                            entry.error = Some(format!(
                                "max reconnect attempts ({}) exceeded: {e}",
                                Self::RECONNECT_MAX_ATTEMPTS
                            ));
                            self.notify_status(&name, &ServerStatus::Error).await;
                        }
                    }
                    tracing::warn!("[mcp] '{}' reconnect failed: {e}", name);
                }
            }
        }
    }

    /// 并发连接所有 enabled server（allSettled 模式，单台失败不阻断）
    pub async fn connect_all(&self) {
        let to_connect: Vec<(String, McpServerConfig)> = {
            let servers = self.servers.lock().await;
            self.config
                .iter()
                .filter(|(name, cfg)| {
                    let entry = servers.get(*name);
                    match entry {
                        Some(e) => !e.effective_disabled(cfg),
                        None => !cfg.is_disabled(),
                    }
                })
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        };

        if to_connect.is_empty() {
            return;
        }

        // 标记全部 connecting
        {
            let mut servers = self.servers.lock().await;
            for (name, _) in &to_connect {
                if let Some(entry) = servers.get_mut(name) {
                    entry.status = ServerStatus::Connecting;
                }
            }
        }

        // 并发连接（用 tokio::join! + 逐个 await，避免 spawn 的 'static 约束）
        // MCP server 通常 < 5 个，逐个 await 也可接受
        for (name, cfg) in &to_connect {
            let result = Self::connect_one(name, cfg).await;
            let mut servers = self.servers.lock().await;
            if let Some(entry) = servers.get_mut(name) {
                match result {
                    Ok(cr) => {
                        entry.status = ServerStatus::Connected;
                        entry.tools = cr.tools;
                        entry.resources = cr.resources;
                        entry.prompts = cr.prompts;
                        entry.error = None;
                        entry.client = Some(Arc::new(cr.client));
                    }
                    Err(e) => {
                        entry.status = ServerStatus::Error;
                        entry.error = Some(format!("{e}"));
                    }
                }
            }
        }
    }

    /// 连接单个 server，返回 (client, discovered_tools)
    async fn connect_one(
        name: &str,
        cfg: &McpServerConfig,
    ) -> Result<
        ConnectResult,
        Box<dyn std::error::Error + Send + Sync>,
    > {
        use rmcp::ServiceExt;

        let client = match cfg {
            McpServerConfig::Stdio { command, args, env, cwd, .. } => {
                let mut cmd = tokio::process::Command::new(command);
                cmd.args(args);
                for (k, v) in env {
                    cmd.env(k, v);
                }
                if let Some(cwd) = cwd {
                    cmd.current_dir(cwd);
                }
                let transport = rmcp::transport::TokioChildProcess::new(cmd)?;
                // 连接超时 25s（留 5s 给 list_tools，总共 30s 在 connect_all 的超时内）
                tokio::time::timeout(
                    std::time::Duration::from_secs(25),
                    ().serve(transport),
                )
                .await
                .map_err(|_| -> Box<dyn std::error::Error + Send + Sync> {
                    "stdio MCP server connect timeout (25s)".into()
                })??
            }
            McpServerConfig::Http { url, .. } => {
                let transport =
                    rmcp::transport::StreamableHttpClientTransport::from_uri(url.as_str());
                tokio::time::timeout(
                    std::time::Duration::from_secs(25),
                    ().serve(transport),
                )
                .await
                .map_err(|_| -> Box<dyn std::error::Error + Send + Sync> {
                    "http MCP server connect timeout (25s)".into()
                })??
            }
        };

        // 发现工具（超时 5s）
        let tools_result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.list_tools(Default::default()),
        )
        .await
        .map_err(|_| -> Box<dyn std::error::Error + Send + Sync> {
            "MCP list_tools timeout (5s)".into()
        })??;
        let tools: Vec<DiscoveredTool> = tools_result
            .tools
            .into_iter()
            .map(|t| {
                let original_name = t.name.to_string();
                let full_name = format!("mcp__{name}__{original_name}");
                DiscoveredTool {
                    full_name,
                    original_name,
                    description: t
                        .description
                        .map(|d| d.to_string())
                        .unwrap_or_else(|| format!("MCP tool (from {name})")),
                    input_schema: serde_json::Value::Object(t.input_schema.as_ref().clone()),
                }
            })
            .collect();

        // 发现资源（超时 5s，失败不阻断——不是所有 server 都有 resources）
        let resources: Vec<DiscoveredResource> = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.list_all_resources(),
        )
        .await
        .ok()
        .and_then(|r| r.ok())
        .map(|rs| {
            rs.into_iter()
                .map(|r| DiscoveredResource {
                    uri: r.raw.uri.clone(),
                    name: r.raw.name.clone(),
                    description: r.raw.description.clone(),
                    mime_type: r.raw.mime_type.clone(),
                })
                .collect()
        })
        .unwrap_or_default();

        // 发现提示模板（超时 5s，失败不阻断）
        let prompts: Vec<DiscoveredPrompt> = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.list_all_prompts(),
        )
        .await
        .ok()
        .and_then(|r| r.ok())
        .map(|ps| {
            ps.into_iter()
                .map(|p| DiscoveredPrompt {
                    name: p.name.clone(),
                    description: p.description.clone().map(|d| d.to_string()),
                })
                .collect()
        })
        .unwrap_or_default();

        Ok(ConnectResult { client, tools, resources, prompts })
    }

    /// 断开单个 server（用于 toggle 关闭 / restart）
    async fn disconnect_one(&self, name: &str) {
        let mut servers = self.servers.lock().await;
        if let Some(entry) = servers.get_mut(name) {
            if let Some(client) = entry.client.take() {
                // RunningService::cancel 消费 self，但 Arc 可能被 McpTool 持有。
                // 用 try_unwrap 避免 cancel 正在用的 client。
                if let Ok(client) = Arc::try_unwrap(client) {
                    let _ = client.cancel().await;
                }
            }
            entry.status = ServerStatus::Disconnected;
            entry.tools.clear();
        }
    }

    /// 重启单个 server（disconnect → connect）
    pub async fn restart_server(&self, name: &str) -> Result<(), String> {
        let cfg = self
            .config
            .get(name)
            .ok_or_else(|| format!("unknown mcp server: {name}"))?
            .clone();

        self.disconnect_one(name).await;

        // 重新连接
        match Self::connect_one(name, &cfg).await {
            Ok(cr) => {
                let mut servers = self.servers.lock().await;
                if let Some(entry) = servers.get_mut(name) {
                    entry.status = ServerStatus::Connected;
                    entry.tools = cr.tools;
                    entry.resources = cr.resources;
                    entry.prompts = cr.prompts;
                    entry.error = None;
                    entry.client = Some(Arc::new(cr.client));
                    entry.reconnect_attempts = 0;
                }
                Ok(())
            }
            Err(e) => {
                let mut servers = self.servers.lock().await;
                if let Some(entry) = servers.get_mut(name) {
                    entry.status = ServerStatus::Error;
                    entry.error = Some(format!("{e}"));
                }
                Err(format!("{e}"))
            }
        }
    }

    /// 运行时 toggle（不持久化到配置文件）
    pub async fn toggle_server(&self, name: &str, enabled: bool) -> Result<(), String> {
        let cfg = self
            .config
            .get(name)
            .ok_or_else(|| format!("unknown mcp server: {name}"))?;

        {
            let mut servers = self.servers.lock().await;
            if let Some(entry) = servers.get_mut(name) {
                entry.runtime_disabled = Some(!enabled);
            }
        }

        if !enabled {
            // 关闭：断开连接
            self.disconnect_one(name).await;
        } else {
            // 开启：如果当前没连接，尝试连接
            let need_connect = {
                let servers = self.servers.lock().await;
                servers
                    .get(name)
                    .map(|e| e.client.is_none())
                    .unwrap_or(true)
            };
            if need_connect {
                let cfg = cfg.clone();
                match Self::connect_one(name, &cfg).await {
                    Ok(cr) => {
                        let mut servers = self.servers.lock().await;
                        if let Some(entry) = servers.get_mut(name) {
                            entry.status = ServerStatus::Connected;
                            entry.tools = cr.tools;
                            entry.resources = cr.resources;
                            entry.prompts = cr.prompts;
                            entry.error = None;
                            entry.client = Some(Arc::new(cr.client));
                            entry.reconnect_attempts = 0;
                        }
                    }
                    Err(e) => {
                        let mut servers = self.servers.lock().await;
                        if let Some(entry) = servers.get_mut(name) {
                            entry.status = ServerStatus::Error;
                            entry.error = Some(format!("{e}"));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// 调用 MCP 工具（供 McpTool::execute 使用）
    pub async fn call_tool(
        &self,
        server_name: &str,
        tool_name: &str,
        args: serde_json::Value,
    ) -> Result<String, String> {
        use rmcp::model::CallToolRequestParams;

        // 尝试调用（含一次自动重连重试）
        for attempt in 0..2 {
            let client = {
                let servers = self.servers.lock().await;
                let entry = servers
                    .get(server_name)
                    .ok_or_else(|| format!("mcp server '{server_name}' not found"))?;
                let client = entry
                    .client
                    .clone()
                    .ok_or_else(|| format!("mcp server '{server_name}' not connected"))?;
                // 检查连接是否已断开（is_closed 检测 JoinHandle + cancellation_token）
                if client.is_closed() {
                    drop(servers); // 释放锁
                    tracing::warn!("[mcp] '{}' connection detected as closed (attempt {})", server_name, attempt + 1);
                    // 尝试重连
                    if let Err(e) = self.reconnect_if_needed(server_name).await {
                        return Err(format!("mcp server '{server_name}' reconnect failed: {e}"));
                    }
                    continue; // 重连后重试
                }
                client
            };

            let mut params = CallToolRequestParams::new(tool_name.to_string());
            if let serde_json::Value::Object(map) = &args {
                params = params.with_arguments(map.clone());
            }

            match tokio::time::timeout(
                std::time::Duration::from_secs(60),
                client.call_tool(params),
            ).await {
                Ok(Ok(result)) => {
                    // 成功：格式化结果
                    use rmcp::model::RawContent;
                    let text: String = result
                        .content
                        .iter()
                        .map(|content| match &content.raw {
                            RawContent::Text(t) => t.text.clone(),
                            RawContent::Image(_) => "[image]".to_string(),
                            RawContent::Audio(_) => "[audio]".to_string(),
                            RawContent::Resource(_) => "[resource]".to_string(),
                            _ => "[unknown]".to_string(),
                        })
                        .collect::<Vec<_>>()
                        .join("\n");

                    if result.is_error == Some(true) {
                        return Ok(format!("MCP error: {text}"));
                    }
                    return Ok(text);
                }
                Ok(Err(e)) => {
                    // call_tool 返回错误：检查是否连接断开
                    let err_str = format!("{e}");
                    tracing::warn!("[mcp] '{}' call_tool error: {err_str}", server_name);
                    // 如果是连接类错误且第一次尝试，标记断开并重连重试
                    if attempt == 0 && Self::is_connection_error(&err_str) {
                        tracing::warn!("[mcp] '{}' connection error, attempting reconnect", server_name);
                        if let Ok(()) = self.reconnect_if_needed(server_name).await {
                            continue; // 重连成功，重试
                        }
                    }
                    return Err(format!("mcp call_tool error: {err_str}"));
                }
                Err(_) => {
                    return Err(format!("mcp tool '{server_name}__{tool_name}' timeout (60s)"));
                }
            }
        }
        Err(format!("mcp tool '{server_name}__{tool_name}' failed after reconnect retry"))
    }

    /// 判断错误是否是连接类错误（需要重连）
    fn is_connection_error(err: &str) -> bool {
        err.contains("connection")
            || err.contains("channel closed")
            || err.contains("transport")
            || err.contains("broken pipe")
            || err.contains("EOF")
            || err.contains("reset")
    }

    /// 检查并重连（如果连接已断开）
    async fn reconnect_if_needed(&self, name: &str) -> Result<(), String> {
        let cfg = self
            .config
            .get(name)
            .ok_or_else(|| format!("unknown mcp server: {name}"))?
            .clone();

        // 标记 connecting
        {
            let mut servers = self.servers.lock().await;
            if let Some(entry) = servers.get_mut(name) {
                entry.status = ServerStatus::Connecting;
                entry.client = None; // 清除旧 client
            }
        }
        self.notify_status(name, &ServerStatus::Connecting).await;

        // 尝试重连
        match Self::connect_one(name, &cfg).await {
            Ok(cr) => {
                let mut servers = self.servers.lock().await;
                if let Some(entry) = servers.get_mut(name) {
                    entry.status = ServerStatus::Connected;
                    entry.tools = cr.tools;
                    entry.resources = cr.resources;
                    entry.prompts = cr.prompts;
                    entry.error = None;
                    entry.client = Some(Arc::new(cr.client));
                    entry.reconnect_attempts = 0;
                }
                tracing::info!("[mcp] '{}' reconnected successfully (lazy)", name);
                self.notify_status(name, &ServerStatus::Connected).await;
                Ok(())
            }
            Err(e) => {
                let mut servers = self.servers.lock().await;
                if let Some(entry) = servers.get_mut(name) {
                    entry.status = ServerStatus::Error;
                    entry.error = Some(format!("{e}"));
                }
                self.notify_status(name, &ServerStatus::Error).await;
                Err(format!("{e}"))
            }
        }
    }

    /// 所有已发现的工具（用于注册到 ToolRegistry）
    pub async fn all_discovered_tools(&self) -> Vec<DiscoveredTool> {
        let servers = self.servers.lock().await;
        servers
            .values()
            .flat_map(|e| e.tools.iter().cloned())
            .collect()
    }

    /// 所有已发现的工具序列化为 JSON（方案 C：host → 子 Worker 传工具列表）
    pub async fn all_discovered_tools_serialized(&self) -> Vec<serde_json::Value> {
        let servers = self.servers.lock().await;
        servers
            .values()
            .flat_map(|e| e.tools.iter())
            .map(|t| serde_json::json!({
                "full_name": t.full_name,
                "original_name": t.original_name,
                "description": t.description,
                "input_schema": t.input_schema,
            }))
            .collect()
    }

    /// 生成 get_mcp_servers RPC 的响应数据
    pub async fn server_list_json(&self) -> Vec<serde_json::Value> {
        let servers = self.servers.lock().await;
        servers
            .iter()
            .map(|(name, entry)| {
                let cfg = self.config.get(name);
                let disabled = entry
                    .runtime_disabled
                    .or_else(|| cfg.map(|c| c.is_disabled()))
                    .unwrap_or(false);
                let transport = cfg.map(|c| c.transport()).unwrap_or("unknown");
                serde_json::json!({
                    "name": name,
                    "transport": transport,
                    "status": serde_json::to_value(&entry.status).unwrap_or(serde_json::json!("unknown")),
                    "disabled": disabled,
                    "tools": entry.tools.iter().map(|t| serde_json::json!({
                        "full_name": t.full_name,
                        "original_name": t.original_name,
                        "description": t.description,
                    })).collect::<Vec<_>>(),
                    "resources": entry.resources,
                    "prompts": entry.prompts,
                    "error": entry.error,
                })
            })
            .collect()
    }

    /// 读取 MCP 资源内容（通过 rmcp read_resource）
    pub async fn read_resource(
        &self,
        server_name: &str,
        uri: &str,
    ) -> Result<String, String> {
        use rmcp::model::ReadResourceRequestParams;

        let client = {
            let servers = self.servers.lock().await;
            let entry = servers
                .get(server_name)
                .ok_or_else(|| format!("mcp server '{server_name}' not found"))?;
            entry
                .client
                .clone()
                .ok_or_else(|| format!("mcp server '{server_name}' not connected"))?
        };

        let params = ReadResourceRequestParams::new(uri.to_string());
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            client.read_resource(params),
        )
        .await
        .map_err(|_| format!("mcp read_resource timeout (30s)"))?
        .map_err(|e| format!("mcp read_resource error: {e}"))?;

        // 提取文本内容
        let text: String = result
            .contents
            .iter()
            .map(|c| match c {
                rmcp::model::ResourceContents::TextResourceContents { text, .. } => text.clone(),
                rmcp::model::ResourceContents::BlobResourceContents { .. } => "[blob]".to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n");

        Ok(text)
    }

    /// 热重载配置（disconnect 旧的 + connect 新的）
    /// 场景：用户改了 config.json 的 mcp_servers，不想重启 worker
    pub async fn reload_config(&self, new_config: HashMap<String, McpServerConfig>) {
        // 找出要移除的 server（旧有新无）
        let old_names: Vec<String> = {
            let servers = self.servers.lock().await;
            servers.keys().cloned().collect()
        };
        for name in &old_names {
            if !new_config.contains_key(name) {
                self.disconnect_one(name).await;
                let mut servers = self.servers.lock().await;
                servers.remove(name);
            }
        }

        // 找出新增的 server（新有旧无）+ 更新已有 server 的配置
        {
            let mut servers = self.servers.lock().await;
            for (name, _cfg) in &new_config {
                if !servers.contains_key(name) {
                    servers.insert(name.clone(), ServerEntry::new());
                }
            }
        }

        // 更新配置（需要 &mut self.config，但 McpManager 用 Arc，所以用内部可变性）
        // 注意：config 是 HashMap（非 Mutex），reload 只能通过替换整个 McpManager 实现
        // 这里改为：逐个连接新 config 里的 enabled server
        for (name, cfg) in &new_config {
            let need_connect = {
                let servers = self.servers.lock().await;
                servers.get(name).map(|e| e.client.is_none() && !e.effective_disabled(cfg)).unwrap_or(false)
            };
            if need_connect {
                // 标记 connecting
                {
                    let mut servers = self.servers.lock().await;
                    if let Some(entry) = servers.get_mut(name) {
                        entry.status = ServerStatus::Connecting;
                    }
                }
                match Self::connect_one(name, cfg).await {
                    Ok(cr) => {
                        let mut servers = self.servers.lock().await;
                        if let Some(entry) = servers.get_mut(name) {
                            entry.status = ServerStatus::Connected;
                            entry.tools = cr.tools;
                            entry.resources = cr.resources;
                            entry.prompts = cr.prompts;
                            entry.error = None;
                            entry.client = Some(Arc::new(cr.client));
                        }
                    }
                    Err(e) => {
                        let mut servers = self.servers.lock().await;
                        if let Some(entry) = servers.get_mut(name) {
                            entry.status = ServerStatus::Error;
                            entry.error = Some(format!("{e}"));
                        }
                    }
                }
            }
        }
    }
}

/// Count the number of configured MCP servers in the given IonConfig.
pub fn server_count_in_config(config: &IonConfig) -> usize {
    config.mcp_servers.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::IonConfig;

    #[test]
    fn test_server_count_in_config() {
        // Empty config -> 0
        let config = IonConfig::default();
        assert_eq!(server_count_in_config(&config), 0);

        // With some mcp_servers
        let mut config = IonConfig::default();
        config.mcp_servers.insert(
            "server1".to_string(),
            McpServerConfig::Stdio {
                command: "echo".to_string(),
                args: vec![],
                env: [].into(),
                cwd: None,
                disabled: false,
            },
        );
        config.mcp_servers.insert(
            "server2".to_string(),
            McpServerConfig::Http {
                kind: "streamable-http".to_string(),
                url: "http://localhost:8080/mcp".to_string(),
                headers: [].into(),
                disabled: false,
            },
        );
        assert_eq!(server_count_in_config(&config), 2);
    }
}