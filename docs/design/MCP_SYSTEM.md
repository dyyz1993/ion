# MCP 系统（Model Context Protocol）设计文档

> **状态：Phase 1-3 已实现** — Phase 1（配置 + RPC）、Phase 2（rmcp 真实连接 + 工具发现/调用）、Phase 3（自动重连 + HTTP 多 Worker 直连 + 事件推送）均已实现并验证。Phase 4（resources/prompts）待定。

## 何时使用这个文档

- **触发场景**：接入第三方 MCP server，把 server 提供的 tool 暴露给 LLM 使用
- **参考样本**：
  - pi 实现：`file:///Users/xuyingzhou/Project/temporary/pi-momo-fork/packages/coding-agent/src/core/mcp/mcp-manager.ts`
  - rmcp（官方 Rust SDK）：https://docs.rs/rmcp / https://github.com/modelcontextprotocol/rust-sdk
  - pi 对齐追踪：[PI_RPC_ALIGNMENT.md](./PI_RPC_ALIGNMENT.md)（MCP 三件套）

---

## 概览

MCP（Model Context Protocol）是一个开放协议，让 AI 助手连接外部工具/数据源。ION 作为 MCP **client**，连接用户配置的 MCP server，把 server 提供的 tool 暴露给 LLM 使用——与内置工具（read/write/bash）、WASM 扩展工具一视同仁。

### 能力清单

| 能力 | 说明 |
|------|------|
| **配置加载** | `~/.ion/config.json` 的 `mcp_servers` 字段（stdio + Streamable HTTP） |
| **多传输** | stdio（spawn 子进程）+ Streamable HTTP（远程 server） |
| **工具发现** | 连接 server 后 `listTools`，自动注册到 ToolRegistry |
| **命名隔离** | `mcp__<server>__<tool>` 防止跨 server 命名冲突 |
| **运行时管理** | `get_mcp_servers` / `mcp_toggle_server` / `mcp_restart_server` RPC |
| **自动重连** | 指数退避（base 1s → max 30s，默认 3 次） |
| **子进程隔离** | `ION_SKIP_MCP=1` 环境变量，子 Worker 跳过 MCP 初始化 |
| **零开销** | 未配置任何 server 时完全不初始化（场景 1 默认无感知） |

### 实现状态

| 模块 | 状态 | Phase |
|------|------|-------|
| `McpServerConfig`（配置 struct，加入 IonConfig） | ✅ 已实现 | 1 |
| `get_mcp_servers` / `mcp_toggle_server` / `mcp_restart_server` RPC | ✅ 已实现（14 个 CI 测试通过） | 1 |
| 项目维度配置（`~/.ion/projects/<key>/config.json`，worktree 共享） | ✅ 已实现 | 1 |
| `McpManager`（连接管理 + 工具发现） | ✅ 已实现（真实 E2E 验证通过） | 2 |
| `McpTool: Tool`（适配 Tool trait） | ✅ 已实现 | 2 |
| rmcp 真实 stdio / HTTP 连接 | ✅ 已实现（server-everything 13 工具 + echo/get-sum 调用验证） | 2 |
| 自动重连（指数退避） | ✅ 已实现（base 1s → max 30s，最多 3 次） | 3 |
| HTTP 多 Worker 直连（方案 B） | ✅ 已实现（ION_SKIP_MCP=stdio 只跳过 stdio） | 3 |
| 连接变更事件推送 | ✅ 已实现（mcp_connection_change 事件） | 3 |
| resources/prompts 探索 | ❌ 待定 | 4 |

---

## 1. 配置

### 1.1 配置层级：全局 + 项目维度（worktree 共享）

MCP 配置支持**两级**，遵循 [AGENTS.md §「项目级」三类存储](../../AGENTS.md)：

| 层级 | 文件位置 | 适用范围 |
|------|---------|---------|
| **全局（①）** | `~/.ion/config.json` 的 `mcp_servers` 字段 | 所有项目 |
| **项目维度（②）** | **`~/.ion/projects/<project_key>/config.json`** 的 `mcp_servers` 字段 | 仅当前项目（含其所有 worktree） |

> ⚠️ **为什么不放 `<project>/.ion/config.json`（仓库内）？** 因为 MCP 配置常含本地路径（如 `/Users/xxx/tools/kb-mcp`）、本地 HTTP URL、Bearer token——这些既不该提交 git，也常因 `.ion/` 被 gitignore 而**无法随 worktree 同步**。放 `~/.ion/projects/<key>/` 后，`<key>` 用 git common dir 求解（主仓库和所有 worktree 算出同一个 key），**天然共享、不依赖 git 同步**。

**`<project_key>` 算法**（复用 file-snapshot 已有实现 `object_store.rs:213-232`）：

```
git rev-parse --absolute-git-dir
    → /ion/.git                 （主仓库，直接用）
    → /ion/.git/worktrees/xxx   （worktree，裁剪成 /ion/.git）
取裁剪后路径的 hash → <project_key>
```

主仓库和它名下所有 worktree 算出**同一个 key** → 共享 `~/.ion/projects/<key>/`。

**合并规则**：项目维度 `mcp_servers` 与全局**按 server name 浅合并**（不是深度合并）——同名 server 项目维度覆盖全局；不同名 server 全部生效。这与 pi 的 deepMerge 行为一致。

**两级配置示例：**

全局 `~/.ion/config.json`：
```json
{
  "mcp_servers": {
    "knowledge-base": {
      "command": "kb-mcp",
      "args": ["--stdio"],
      "disabled": false
    }
  }
}
```

项目维度 `~/.ion/projects/<project_key>/config.json`：
```json
{
  "mcp_servers": {
    "project-linter": {
      "command": "npx",
      "args": ["linter-mcp"],
      "disabled": false
    },
    "knowledge-base": {
      "disabled": true
    }
  }
}
```

**合并结果**（该项目的 worker 看到的有效配置）：

| server | 来源 | 最终状态 |
|--------|------|---------|
| `knowledge-base` | 全局被项目维度覆盖 | **disabled**（项目维度关闭了全局的） |
| `project-linter` | 项目维度新增 | enabled |

### 1.2 配置类型定义

位置：`src/config.rs`

```rust
use std::collections::HashMap;

/// 单个 MCP server 的配置。两种传输方式用 untagged enum 区分，
/// 兼容 pi 的配置格式（stdio 靠 `command` 字段判定，无需 `type`）。
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum McpServerConfig {
    /// stdio 传输：spawn 子进程（最常用）
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: HashMap<String, String>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        disabled: bool,
    },
    /// Streamable HTTP 传输：远程 server
    Http {
        /// 必须是 "streamable-http"（serde rename）
        #[serde(rename = "type")]
        kind: String,
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
        #[serde(default)]
        disabled: bool,
    },
}

impl McpServerConfig {
    /// 是否被禁用
    pub fn is_disabled(&self) -> bool {
        match self {
            McpServerConfig::Stdio { disabled, .. } => *disabled,
            McpServerConfig::Http { disabled, .. } => *disabled,
        }
    }

    /// 传输方式（"stdio" / "http"），用于 get_mcp_servers 展示
    pub fn transport(&self) -> &'static str {
        match self {
            McpServerConfig::Stdio { .. } => "stdio",
            McpServerConfig::Http { .. } => "streamable-http",
        }
    }
}
```

IonConfig 新增字段（`src/config.rs`）：

```rust
pub struct IonConfig {
    // ... 现有字段 ...

    /// MCP server 配置。默认空 —— 未配置时 MCP 模块完全不初始化（零开销）。
    #[serde(default)]
    pub mcp_servers: HashMap<String, McpServerConfig>,
}
```

### 1.3 关键决策点

| 决策 | 选择 | 理由 |
|------|------|------|
| 配置位置 | config.json 顶层 `mcp_servers` | 与 extensions/tier_models 平级，复用现有加载链 |
| 传输识别 | untagged enum（`command` 字段判定 stdio） | 兼容 pi 配置格式，用户可直接复用 pi 的 settings |
| SSE 支持 | ❌ 不实现 | rmcp 用 Streamable HTTP 统一了 SSE，协议演进方向 |
| 全局/项目维度 | **Phase 1 就支持两级** | 项目维度放 `~/.ion/projects/<key>/`（不放仓库内），避免 `.ion/` 被 gitignore 后 worktree 读不到；`<key>` 复用 file-snapshot 的 project_key |
| 配置合并 | server name 浅合并 | 同名 server 项目维度覆盖全局；不同名全部保留 |
| `disabled` 默认 | false（未声明 = 启用） | 与 pi 一致，用户写了配置就期望生效 |

---

## 2. 主流程

### 2.1 McpManager 架构

```
ion_worker 启动
    │
    ▼
读 cfg.mcp_servers
    │
    ├─ 为空？ ──────────────────────────────► 跳过（零开销，场景 1 默认无 MCP）
    │
    ├─ 子 Worker？(ION_SKIP_MCP=1) ─────────► 跳过（防 stdio server 竞争死锁）
    │
    ▼
McpManager::new(config)
    │
    ▼
connect_all() —— 并发连接所有 enabled server（Phase 2）
    │
    ├─ stdio: spawn 子进程 + JSON-RPC over stdin/stdout
    ├─ http:  HTTP 长连接
    │
    ▼
每个 server: list_tools() → 注册 McpTool 到 ToolRegistry
    │
    ▼
Agent 循环正常跑，LLM 可调 mcp__server__tool（与内置工具无差别）
    │
    ▼
进程退出 / session shutdown: disconnect_all()
```

### 2.2 数据结构

位置：`src/mcp/mod.rs`

```rust
use std::collections::HashMap;
use std::sync::{Arc, tokio::Mutex};

pub struct McpManager {
    /// server name → 连接条目
    servers: Arc<Mutex<HashMap<String, ServerEntry>>>,
    /// 原始配置（用于 restart / toggle）
    config: HashMap<String, crate::config::McpServerConfig>,
}

struct ServerEntry {
    status: ServerStatus,
    /// 发现到的工具（连接成功后填充）
    tools: Vec<DiscoveredTool>,
    /// 连接/调用错误（status=Error 时填充）
    error: Option<String>,
    /// rmcp 客户端句柄（连接成功后填充，用于 call_tool）
    /// rmcp 的 RunningService 不是 Send + Sync 安全的传统 Client，
    /// 用 Arc<Mutex<>> 包裹以在 Tool::execute 里安全访问。
    client: Option<Arc<tokio::sync::Mutex<rmcp::service::RunningService<rmcp::RoleClient, ()>>>>,
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
    /// 工具参数 JSON Schema（原样保留，从 rmcp Tool.input_schema 转换）
    pub input_schema: serde_json::Value,
}
```

**rmcp 依赖配置**（`Cargo.toml`）：

```toml
[dependencies]
rmcp = { version = "1", default-features = false, features = [
    "client",                                  # Client 模式（list_tools / call_tool）
    "transport-child-process",                 # stdio 传输（TokioChildProcess）
    "transport-streamable-http-client-reqwest", # HTTP 传输
    # 注意：default-features = false 避免拉入 server / macros
    # 连 https MCP server 需额外加 "reqwest"（rustls TLS）
] }
```

> **版本选择**：用 rmcp **1.x**（不是 2.x）。因为 2.x 需要 Rust 1.96+（edition 2024 + rust-toolchain 1.96），当前项目用 Rust 1.92。1.x 的 API 与 2.x 核心一致（`ServiceExt`/`ClientInfo`/`CallToolRequestParams`/`TokioChildProcess`），已通过编译验证。

### 2.3 connect_all + 单 server 连接（Phase 2 核心）

```rust
impl McpManager {
    /// 并发连接所有 enabled server（Promise.allSettled 模式，单台失败不影响其它）
    pub async fn connect_all(&self) {
        let servers: Vec<(String, McpServerConfig)> = self.config.iter()
            .filter(|(_, c)| !c.is_disabled())
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        // 并发连接，每个 server 独立处理错误
        let futures: Vec<_> = servers.into_iter()
            .map(|(name, cfg)| {
                let servers = self.servers.clone();
                async move {
                    // 标记 connecting
                    {
                        let mut map = servers.lock().await;
                        if let Some(entry) = map.get_mut(&name) {
                            entry.status = ServerStatus::Connecting;
                        }
                    }
                    // 尝试连接
                    match Self::connect_one(&name, &cfg).await {
                        Ok((client, tools)) => {
                            let mut map = servers.lock().await;
                            if let Some(entry) = map.get_mut(&name) {
                                entry.status = ServerStatus::Connected;
                                entry.tools = tools;
                                entry.error = None;
                                entry.client = Some(Arc::new(tokio::sync::Mutex::new(client)));
                            }
                        }
                        Err(e) => {
                            let mut map = servers.lock().await;
                            if let Some(entry) = map.get_mut(&name) {
                                entry.status = ServerStatus::Error;
                                entry.error = Some(format!("{e}"));
                            }
                        }
                        }
                }
            })
            .collect();
        futures::future::join_all(futures).await;
    }

    /// 连接单个 server（stdio 或 http），返回 (client, discovered_tools)
    async fn connect_one(
        name: &str,
        cfg: &McpServerConfig,
    ) -> Result<
        (
            rmcp::service::RunningService<rmcp::RoleClient, ()>,
            Vec<DiscoveredTool>,
        ),
        Box<dyn std::error::Error + Send + Sync>,
    > {
        use rmcp::ServiceExt;

        // 1. 构造传输 + 连接
        let client = match cfg {
            McpServerConfig::Stdio { command, args, env, cwd, .. } => {
                use rmcp::transport::{TokioChildProcess, ConfigureCommandExt};
                let mut cmd = tokio::process::Command::new(command);
                cmd.args(args);
                for (k, v) in env { cmd.env(k, v); }
                if let Some(cwd) = cwd { cmd.current_dir(cwd); }
                let transport = TokioChildProcess::new(cmd)?;
                ().serve(transport).await?
            }
            McpServerConfig::Http { url, .. } => {
                use rmcp::transport::StreamableHttpClientTransport;
                let transport = StreamableHttpClientTransport::from_uri(url);
                ().serve(transport).await?
            }
        };

        // 2. 发现工具（list_tools）
        let tools_result = client.list_tools(Default::default()).await?;
        let tools: Vec<DiscoveredTool> = tools_result.tools.into_iter()
            .map(|t| {
                let original_name = t.name.to_string();
                let full_name = format!("mcp__{name}__{original_name}");
                DiscoveredTool {
                    full_name,
                    original_name,
                    description: t.description
                        .map(|d| d.to_string())
                        .unwrap_or_else(|| format!("MCP tool: {original_name} (from {name})")),
                    // rmcp 的 input_schema 是 JsonObject (Map<String, Value>)，
                    // 转成 Value::Object 给 LLM
                    input_schema: serde_json::Value::Object(t.input_schema.as_ref().clone()),
                }
            })
            .collect();

        Ok((client, tools))
    }
}
```

### 2.4 McpTool（Phase 2）

MCP server 提供的工具通过 `McpTool` 适配成 `Tool` trait。`execute()` 内部走 rmcp 的 `call_tool`。

位置：`src/mcp/tool.rs`

```rust
use crate::agent::tool::{Tool, AgentResult};
use crate::runtime::Runtime;
use async_trait::async_trait;
use rmcp::model::{CallToolRequestParams, ContentBlock};

pub struct McpTool {
    full_name: String,         // mcp__server__tool
    description: String,
    parameters: serde_json::Value,
    /// server 名 + 原始工具名（execute 时反查 client 用）
    server_name: String,
    tool_name: String,
    manager: Arc<McpManager>,
}

impl McpTool {
    pub fn new(tool: &DiscoveredTool, manager: Arc<McpManager>) -> Self {
        // 从 full_name "mcp__server__tool" 拆出 server 和 tool
        let parts: Vec<&str> = tool.full_name.splitn(3, "__").collect();
        // parts = ["mcp", server, tool]
        let server_name = parts.get(1).unwrap_or("").to_string();
        let tool_name = parts.get(2).unwrap_or("").to_string();
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
    fn name(&self) -> &str { &self.full_name }
    fn description(&self) -> &str { &self.description }
    fn parameters(&self) -> serde_json::Value { self.parameters.clone() }

    async fn execute(
        &self,
        args: serde_json::Value,
        _rt: &dyn Runtime,
    ) -> AgentResult<String> {
        // 1. 从 manager 取 server 的 client
        let client_arc = {
            let servers = self.manager.servers.lock().await;
            let entry = servers.get(&self.server_name)
                .ok_or_else(|| AgentError::Other(format!(
                    "mcp server '{}' not found", self.server_name
                )))?;
            entry.client.clone()
        };
        let client_arc = client_arc.ok_or_else(|| AgentError::Other(format!(
            "mcp server '{}' not connected", self.server_name
        )))?;
        let client = client_arc.lock().await;

        // 2. 构造 CallToolRequestParams
        let mut params = CallToolRequestParams::new(self.tool_name.clone());
        // args 转 JsonObject（Map<String, Value>）
        if let serde_json::Value::Object(map) = args {
            params = params.with_arguments(map);
        }

        // 3. 调用（超时 60s，对齐 pi callTimeoutMs）
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            client.call_tool(params),
        ).await
            .map_err(|_| AgentError::Other(format!(
                "mcp tool '{}' timeout (60s)", self.full_name
            )))??;

        // 4. 格式化结果（text 拼接，image 占位，对齐 pi formatResult）
        let text: String = result.content.iter().map(|block| {
            match block {
                ContentBlock::Text(t) => t.text.clone(),
                ContentBlock::Image(_) => "[image]".to_string(),
                ContentBlock::Audio(_) => "[audio]".to_string(),
                ContentBlock::Resource(r) => format!("[resource: {:?}]", r),
                ContentBlock::ResourceLink(r) => format!("[resource_link: {}]", r.uri),
                _ => format!("[unknown content: {:?}]", block),
            }
        }).collect::<Vec<_>>().join("\n");

        // is_error 时加前缀（不中断 agent，对齐 pi）
        if result.is_error == Some(true) {
            Ok(format!("MCP error: {text}"))
        } else {
            Ok(text)
        }
    }
}
```

### 2.5 启动注册流程

位置：`src/bin/ion_worker.rs`（worker 启动时，在工具注册之后）

```rust
// 步骤 1: 读配置
let mcp_config = cfg.mcp_servers.clone();

// 步骤 2: 构建 manager
let mcp_manager = Arc::new(McpManager::new(mcp_config));

// 步骤 3: 连接 + 注册工具
// ION_SKIP_MCP=1 时跳过（子 Worker 防止 stdio server 竞争死锁）
if std::env::var("ION_SKIP_MCP").ok().as_deref() != Some("1") && !mcp_manager.is_empty() {
    // 并发连接（超时 30s/server，单台失败不阻断）
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        mcp_manager.connect_all(),
    ).await.ok();

    // 注册发现的工具到 ToolRegistry
    for tool in mcp_manager.all_discovered_tools() {
        tools.register(Box::new(McpTool::new(&tool, mcp_manager.clone())));
    }
}
```

**关键时序决策**：
- `connect_all` 在 agent 循环**启动前**执行（eager 连接，对齐 pi）
- 连接超时 30s/server（对齐 pi connectTimeoutMs），超时不阻断启动
- 单台 server 连接失败 → status=Error + error 消息，其余 server 照常工作
- 工具注册后，LLM 在同一 agent 循环里就能用 `mcp__server__tool`

### 2.6 关键决策点

| 决策 | 选择 | 理由 |
|------|------|------|
| **内核 vs 扩展** | **内核**（独立模块 `src/mcp/`） | 按 AGENTS.md 准则 4：多个无关扩展（权限扩展要识别 `mcp__.*`、file-snapshot 要追踪 mcp 工具改动）都会用到，属共用能力 |
| **客户端库** | rmcp **1.x**（官方 Rust SDK） | 官方维护；2.x 需要 Rust 1.96+，当前项目用 1.92，故选 1.x。已验证 1.x API 与 2.x 核心一致 |
| **工具命名** | `mcp__<server>__<tool>` | 对齐 pi / Claude Code / Cursor；HOOK_SYSTEM.md 已预留 `mcp__.*` 权限匹配规则 |
| **启动时机** | eager（agent 启动时全连接） | 对齐 pi，避免 LLM 第一次调用 mcp 工具时才连接导致长延迟 |
| **连接超时** | 30s/server | 对齐 pi connectTimeoutMs |
| **调用超时** | 60s/tool | 对齐 pi callTimeoutMs |
| **子 Worker 隔离** | `ION_SKIP_MCP=1` 跳过 | 对齐 pi `PI_SKIP_MCP`；多 Worker 抢同一 stdio server 会死锁 |
| **resources/prompts** | ❌ 暂不实现 | pi 也没实现；tools 是 MCP 核心价值，resources/prompts 留 Phase 4 |
| **错误处理** | tool 失败返回错误文本，不中断 agent | 对齐 pi：MCP server 故障不应阻断主 agent 流程 |
| **工具 schema** | 原样保留 MCP 返回的 JSON Schema | MCP schema 不一定符合严格格式，原样传给 LLM（对齐 pi `Type.Unsafe`） |
| **并发连接** | `join_all`（allSettled 模式） | 单台失败不影响其它 server |
| **rmcp client 存储** | `Arc<Mutex<RunningService>>` | rmcp 的 RunningService 需要在 McpTool::execute 中安全访问 |

---

### 2.7 进程共享机制（多 Worker 共用 MCP）

> **核心约束**：stdio MCP server 是**独占的**——一个 server 进程只有一个 stdin/stdout，不能被多个 client 同时连接。这不是 rmcp 的限制，是 MCP 协议 + Unix 进程模型的硬约束。

#### 问题场景

```
ion --host "用 worktree 开发"
    │
    ├─ coordinator worker（主进程）
    │   └─ connect_all() → spawn stdio MCP server 进程 A
    │
    ├─ developer worker 1（子进程）
    │   └─ connect_all() → spawn stdio MCP server 进程 B  ← 重复！资源浪费
    │
    └─ developer worker 2（子进程）
        └─ connect_all() → spawn stdio MCP server 进程 C  ← 又一个！
```

每个 worker 独立 spawn 一个 MCP server 子进程——**3 份内存、3 份状态、可能冲突**。

#### 两种传输方式的共享性差异

| 传输 | 多 client 共享？ | 原因 | 多 Worker 场景处理 |
|------|----------------|------|-------------------|
| **stdio** | ❌ **不可能** | 进程只有一个 stdin/stdout | 只有主进程连；子 Worker 跳过（`ION_SKIP_MCP=1`） |
| **Streamable HTTP** | ✅ **天然支持** | HTTP server 多客户端，每 client 独立 session（`Mcp-Session-Id`） | 每个 Worker 独立连，server 端自动多路复用 |

#### 三层方案（渐进式）

**方案 A（Phase 2，对齐 pi）——主进程独占 + 子 Worker 跳过**

```
coordinator worker（主进程）
    └─ connect_all() → 持有所有 MCP server 连接
    └─ ION_SKIP_MCP=1 不设（主进程正常连）

developer worker（子进程）
    └─ 继承 ION_SKIP_MCP=1 → connect_all() 跳过
    └─ 需要 MCP 工具时 → 通过 channel 回传 coordinator 代执行
```

- 实现成本：低（只需 spawn 时设 `ION_SKIP_MCP=1`）
- 缺点：子 Worker 的 LLM 看不到 `mcp__*` 工具，必须通过 coordinator 中转

**方案 B（Phase 3，HTTP 天然共享）——HTTP server 多 Worker 直连**

```
coordinator worker + developer workers
    └─ 各自 connect HTTP MCP server（不同 session，同一 server 进程）
    └─ stdio server 仍走方案 A（主进程独占）
```

- HTTP server 天然支持多 client，不需要特殊处理
- 每个 Worker 的 LLM 都能看到 HTTP 类的 `mcp__*` 工具

**方案 C（Phase 3+，host 级 MCP 池）——单例扩展持有共享连接**

```
WorkerRegistry（host 层）
    └─ SingletonEntry<McpPool>（引用计数，所有 Worker 共享）
        └─ 持有 Arc<Mutex<HashMap<server_name, RunningService>>>
        └─ Worker 通过 ExtensionApi 访问共享连接

developer worker
    └─ 不自己 spawn，调 host 的 McpPool.call_tool()
```

- 实现成本：高（需要改 WorkerRegistry + ExtensionApi + singleton 生命周期）
- 优点：所有 Worker 的 LLM 都能看到全部 `mcp__*` 工具（含 stdio）
- 复用 ION 已有的 singleton 扩展机制（`Extension::is_singleton()` / `on_singleton_init()`）

#### Phase 2 采用方案 A

| 场景 | stdio server | HTTP server |
|------|-------------|-------------|
| **主进程**（coordinator） | ✅ connect_all 正常连 | ✅ connect_all 正常连 |
| **子 Worker**（developer） | ❌ `ION_SKIP_MCP=1` 跳过 | ⚠️ Phase 2 也跳过（Phase 3 改为直连） |

**实现**：`worker_registry.rs` spawn 子 worker 时设 `ION_SKIP_MCP=1`（对齐 pi 的 `PI_SKIP_MCP=1`）。主进程不设。

---

## 3. 关键 bug / 设计风险记录

> Phase 1 尚未实现，暂无 bug。此节预留，实现后记录：

- **（预留）stdio server 多进程竞争**：多个 Worker 同时连同一个 stdio server 会导致 stdin/stdout 错乱。缓解：`ION_SKIP_MCP=1` 让子 Worker 跳过。根本解决需共享单例（Phase 3+ 考虑 host 级 MCP pool）。
- **（预留）MCP 协议版本兼容**：rmcp 升级后 protocolVersion 协商失败。缓解：锁定 rmcp 版本，CI 烟测。

---

## 4. 接口规格

> 所有 RPC 命令通过 `ion rpc` 调用。三个 MCP 命令的空桩已在 `src/bin/ion_worker.rs:2134-2136` 存在，Phase 1 填充实现。

### 4.1 get_mcp_servers — 列出所有 MCP server

**请求：**
```bash
ion rpc --worker <id> --method get_mcp_servers
```

**参数：** 无

**成功响应（有 server，已连接）：**
```json
{
  "id": "1",
  "type": "response",
  "command": "get_mcp_servers",
  "success": true,
  "data": [
    {
      "name": "knowledge-base",
      "transport": "stdio",
      "status": "connected",
      "disabled": false,
      "tools": [
        {
          "full_name": "mcp__knowledge-base__search",
          "original_name": "search",
          "description": "Search the knowledge base"
        }
      ],
      "error": null
    }
  ]
}
```

**成功响应（Phase 1 未连接，仅返回配置）：**
```json
{
  "id": "1",
  "type": "response",
  "command": "get_mcp_servers",
  "success": true,
  "data": [
    {
      "name": "knowledge-base",
      "transport": "stdio",
      "status": "disconnected",
      "disabled": false,
      "tools": [],
      "error": null
    }
  ]
}
```

**成功响应（无配置）：**
```json
{
  "id": "1",
  "type": "response",
  "command": "get_mcp_servers",
  "success": true,
  "data": []
}
```

**验证点：**
- `data` 始终是数组（空配置返回 `[]`，不报错）
- `transport` 取值为 `"stdio"` 或 `"streamable-http"`
- `status` 取值为 `disconnected` / `connecting` / `connected` / `error`
- Phase 1 `tools` 始终为 `[]`（未真实连接）；Phase 2 连接后填充

### 4.2 mcp_toggle_server — 启用/禁用 server

**请求：**
```bash
ion rpc --worker <id> --method mcp_toggle_server \
  --params '{"name":"knowledge-base","enabled":false}'
```

**参数：**

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `name` | string | ✅ | server 名（config 里的 key） |
| `enabled` | bool | ✅ | `true`=启用并连接，`false`=禁用并断开 |

**成功响应（禁用）：**
```json
{
  "id": "1",
  "type": "response",
  "command": "mcp_toggle_server",
  "success": true,
  "data": {
    "name": "knowledge-base",
    "enabled": false,
    "status": "disconnected"
  }
}
```

**失败响应（server 不存在）：**
```json
{
  "id": "1",
  "type": "response",
  "command": "mcp_toggle_server",
  "success": false,
  "error": "unknown mcp server: nonexistent"
}
```

**验证点：**
- `name` 必须在 config 中存在，否则报错
- Phase 1：仅修改内存中的 disabled 标志，`status` 固定返回 `disconnected`
- Phase 2：`enabled:true` 触发真实连接，`status` 反映连接结果

### 4.3 mcp_restart_server — 重启 server

**请求：**
```bash
ion rpc --worker <id> --method mcp_restart_server \
  --params '{"name":"knowledge-base"}'
```

**参数：**

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `name` | string | ✅ | server 名 |

**成功响应：**
```json
{
  "id": "1",
  "type": "response",
  "command": "mcp_restart_server",
  "success": true,
  "data": {
    "name": "knowledge-base",
    "status": "connected"
  }
}
```

**失败响应（server 不存在）：**
```json
{
  "id": "1",
  "type": "response",
  "command": "mcp_restart_server",
  "success": false,
  "error": "unknown mcp server: nonexistent"
}
```

**验证点：**
- restart = disconnect → 重新 connect（Phase 2）
- Phase 1：返回当前 status（`disconnected`）
- 用于 server 进程卡死、配置变更后重连

---

## 5. CLI 测试指南

> 完整自动化脚本：`tests/mcp_ci.sh`（Phase 1 已实现，15 个 case 全过）。
> 以下为每个 case 的完整 CLI 命令 + 请求/响应 JSON + 验证点，对齐 [CLI_TEST_TEMPLATE.md](../templates/CLI_TEST_TEMPLATE.md) 格式。

### Group A — 配置加载（Phase 1）✅

> 验证 MCP 配置加载到 IonConfig 后，`get_mcp_servers` 能正确返回 server 列表。

#### A1 空配置

```bash
# 准备：~/.ion/config.json 不含 mcp_servers
ion rpc --session <sid> --method get_mcp_servers
```

**预期响应：**
```json
{"type":"response","id":"1","command":"get_mcp_servers","success":true,"data":[]}
```

**验证点：**
- ✅ `data` 是空数组（不是 null 或报错）
- ✅ `success: true`

#### A2 单个 stdio server

```bash
# 准备：~/.ion/config.json 含
# {"mcp_servers":{"test-kb":{"command":"echo","args":["hello"],"disabled":false}}}
ion rpc --session <sid> --method get_mcp_servers
```

**预期响应：**
```json
{
  "type": "response", "id": "1", "command": "get_mcp_servers", "success": true,
  "data": [{
    "name": "test-kb", "transport": "stdio",
    "status": "disconnected", "disabled": false, "tools": [], "error": null
  }]
}
```

**验证点：**
- ✅ `transport` = `"stdio"`（有 command 字段 → stdio）
- ✅ `status` = `"disconnected"`（Phase 1 未连接）
- ✅ `tools` = `[]`（Phase 2 连接后才填充）

#### A3 HTTP server

```bash
# 准备：config.json 含
# {"mcp_servers":{"remote-api":{"type":"streamable-http","url":"http://x/mcp","disabled":false}}}
ion rpc --session <sid> --method get_mcp_servers
```

**预期响应：** data 含 `{"name":"remote-api","transport":"streamable-http",...}`

**验证点：**
- ✅ `transport` = `"streamable-http"`（有 type 字段 → HTTP）

#### A4 disabled server

```bash
# 准备：config.json 含
# {"mcp_servers":{"x":{"command":"echo","disabled":true}}}
ion rpc --session <sid> --method get_mcp_servers
```

**验证点：**
- ✅ server 出现在列表中，但 `disabled: true`

#### A5 stdio + http 同时

**验证点：**
- ✅ `data` 数组含 2 条（不同名 server 全保留）

### Group A2 — 配置层级合并（Phase 1）

> 验证全局 + 项目维度（`~/.ion/projects/<key>/config.json`）按 server name 浅合并。

#### A2-1 全局 + 项目维度合并

```bash
# 全局 ~/.ion/config.json: {"mcp_servers":{"kb":{...}}}
# 项目维度 ~/.ion/projects/<key>/config.json: {"mcp_servers":{"linter":{...}}}
ion rpc --session <sid> --method get_mcp_servers
```

**验证点：**
- ✅ data 含 2 个 server（kb + linter），不同名全保留

#### A2-2 项目维度覆盖全局

```bash
# 全局: kb (enabled)
# 项目维度: kb (disabled:true)
ion rpc --session <sid> --method get_mcp_servers
```

**验证点：**
- ✅ data 只有 1 个 `kb`，`disabled: true`（项目维度覆盖）

#### A2-3 仅项目维度有配置

**验证点：**
- ✅ 全局没有 mcp_servers 时，读到项目维度的 server

#### A2-4 worktree 共享

```bash
# 在主仓库和 worktree 各跑一次
ion rpc --session <sid> --method get_mcp_servers
```

**验证点：**
- ✅ 两边返回相同的 server 列表（同一 `<project_key>`）

#### A2-5 gitignore 不影响

**验证点：**
- ✅ 仓库内 `.ion/` 被 gitignore，worktree 无 `.ion/`，仍读到项目维度配置（存 `~/.ion/projects/<key>/`）

#### A2-6 项目维度文件不存在

**验证点：**
- ✅ `~/.ion/projects/<key>/config.json` 不存在 → 回退仅全局，不报错

#### A2-7 不同项目隔离

**验证点：**
- ✅ 两个不同 git 仓库的 `<project_key>` 不同，配置互不干扰

### Group B — 运行时 toggle（Phase 1）✅

> 验证 `mcp_toggle_server` 运行时启用/禁用（不持久化到配置文件）。

#### B1 toggle 关闭

```bash
ion rpc --session <sid> --method mcp_toggle_server \
  --params '{"name":"test-kb","enabled":false}'
```

**预期响应：**
```json
{
  "type": "response", "id": "1", "command": "mcp_toggle_server", "success": true,
  "data": {"name": "test-kb", "enabled": false, "status": "disconnected"}
}
```

**验证点：**
- ✅ `success: true`
- ✅ `enabled: false`

#### B2 toggle 后 get 反映状态

```bash
ion rpc --session <sid> --method get_mcp_servers
```

**验证点：**
- ✅ test-kb 的 `disabled: true`

#### B3 toggle 不存在的 server

```bash
ion rpc --session <sid> --method mcp_toggle_server \
  --params '{"name":"ghost","enabled":true}'
```

**预期响应：**
```json
{
  "type": "response", "id": "1", "command": "mcp_toggle_server", "success": false,
  "error": "unknown mcp server: ghost"
}
```

#### B4 缺 enabled 参数

```bash
ion rpc --session <sid> --method mcp_toggle_server \
  --params '{"name":"test-kb"}'
```

**预期响应：**
```json
{
  "type": "response", "id": "1", "command": "mcp_toggle_server", "success": false,
  "error": "missing 'enabled'"
}
```

### Group C — restart（Phase 1）✅

#### C1 restart 已配置 server

```bash
ion rpc --session <sid> --method mcp_restart_server \
  --params '{"name":"test-kb"}'
```

**预期响应：**
```json
{
  "type": "response", "id": "1", "command": "mcp_restart_server", "success": true,
  "data": {"name": "test-kb", "status": "disconnected"}
}
```

**验证点：**
- ✅ Phase 1 返回 `status: "disconnected"`（Phase 2 改为真实 disconnect→connect）

#### C2 restart 不存在 server

```bash
ion rpc --session <sid> --method mcp_restart_server \
  --params '{"name":"ghost"}'
```

**预期响应：** `success: false, error: "unknown mcp server: ghost"`

### Group D — 真实连接：stdio 类型（Phase 2，依赖 rmcp，待实现）

> 前置：有可用的 stdio MCP server（如 `npx @modelcontextprotocol/server-everything`）。

#### D1 stdio server 连接 + 工具发现

```bash
# config.json: {"mcp_servers":{"echo":{"command":"npx","args":["@modelcontextprotocol/server-everything"]}}}
# Phase 2: connect_all 在 worker 启动时自动执行
ion rpc --session <sid> --method get_mcp_servers
```

**预期响应：**
```json
{
  "data": [{
    "name": "echo", "transport": "stdio", "status": "connected",
    "disabled": false, "tools": [
      {"full_name": "mcp__echo__echo", "original_name": "echo", "description": "..."},
      {"full_name": "mcp__echo__add", "original_name": "add", "description": "..."}
    ], "error": null
  }]
}
```

**验证点：**
- ✅ `status` = `"connected"`（不是 disconnected）
- ✅ `tools` 非空（list_tools 成功发现）
- ✅ 工具名格式 `mcp__echo__echo`（双下划线 + server 名 + 工具名）

#### D2 stdio 工具调用

```bash
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"mcp__echo__echo","args":{"message":"hello"}}'
```

**验证点：**
- ✅ 工具执行成功，返回 MCP server 的响应文本
- ✅ 结果不是 `"MCP error: ..."` 前缀

#### D3 stdio server 进程被 kill

```bash
# 手动 kill MCP server 子进程，然后调工具
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"mcp__echo__echo","args":{"message":"test"}}'
```

**验证点：**
- ✅ 返回错误文本（如 `"MCP error: connection closed"`），**agent 不中断**
- ✅ `get_mcp_servers` 显示 `status: "error"`

#### D4 stdio toggle 关闭→重启→开启

```bash
# 关闭
ion rpc --session <sid> --method mcp_toggle_server --params '{"name":"echo","enabled":false}'
# → status: "disconnected"
# 重启
ion rpc --session <sid> --method mcp_restart_server --params '{"name":"echo"}'
# → status: "connected"，tools 重新发现
# 开启
ion rpc --session <sid> --method mcp_toggle_server --params '{"name":"echo","enabled":true}'
```

**验证点：**
- ✅ toggle 关闭 → disconnect → `status: "disconnected"`
- ✅ restart → 重新 connect + list_tools → `status: "connected"`，tools 非空
- ✅ toggle 开启 → 同上

### Group D2 — 真实连接：HTTP 类型（Phase 2）

> 前置：有可用的 HTTP MCP server（如本地启动 `@modelcontextprotocol/server-everything --transport http`）。

#### D2-1 HTTP server 连接

```bash
# config.json: {"mcp_servers":{"remote":{"type":"streamable-http","url":"http://localhost:3001/mcp"}}}
ion rpc --session <sid> --method get_mcp_servers
```

**验证点：**
- ✅ `status` = `"connected"`
- ✅ `transport` = `"streamable-http"`

#### D2-2 HTTP 工具调用

```bash
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"mcp__remote__echo","args":{"message":"hi"}}'
```

**验证点：**
- ✅ 与 D2（stdio）行为一致，结果正确

#### D2-3 HTTP 连接超时

```bash
# config.json url 指向不存在的地址
# {"mcp_servers":{"dead":{"type":"streamable-http","url":"http://localhost:1/mcp"}}}
ion rpc --session <sid> --method get_mcp_servers
```

**验证点：**
- ✅ `status` = `"error"`，error 含超时/连接拒绝信息
- ✅ 30s 内返回（不卡死整个 worker 启动）

### Group D3 — 真实连接：disabled + 混合类型（Phase 2）

#### D3-1 disabled server 不连接

```bash
# config.json: {"mcp_servers":{"off":{"command":"npx","args":["xxx"],"disabled":true}}}
ion rpc --session <sid> --method get_mcp_servers
```

**验证点：**
- ✅ `status` = `"disconnected"`（不尝试连接）
- ✅ 没有 spawn 子进程

#### D3-2 stdio + HTTP + disabled 三种混合

```bash
# config.json: 1 个 stdio + 1 个 HTTP + 1 个 disabled
ion rpc --session <sid> --method get_mcp_servers
```

**验证点：**
- ✅ data 含 3 个 server
- ✅ stdio 和 HTTP 的 `status: "connected"`，disabled 的 `status: "disconnected"`
- ✅ 单台失败（如 HTTP 超时）不影响其它 server

### Group E — 重连与恢复（Phase 3）

> 验证自动重连（指数退避）和手动恢复机制。

#### E1 自动重连（server 意外退出后）

```bash
# 1. 连接 stdio server（status: connected）
# 2. kill -9 MCP server 子进程（模拟崩溃）
# 3. 等待自动重连（base 1s → max 30s，最多 3 次）
ion rpc --session <sid> --method get_mcp_servers
```

**验证点：**
- ✅ 重连期间 `status` 经历 `error` → `connecting` → `connected`
- ✅ 重连成功后 tools 重新发现
- ✅ 重连间隔指数增长（1s → 2s → 4s）

#### E2 重连次数耗尽

```bash
# 1. 连接 stdio server
# 2. 持续 kill server（让它每次重连后立刻又崩）
# 3. 观察 3 次重连后放弃
```

**验证点：**
- ✅ 3 次重连失败后 `status` 停在 `"error"`，不再重试
- ✅ `error` 字段含 `"max reconnect attempts (3) exceeded"`

#### E3 手动 restart 恢复

```bash
# E2 状态后（status: error，已放弃重连）
ion rpc --session <sid> --method mcp_restart_server --params '{"name":"echo"}'
```

**验证点：**
- ✅ restart 清除重连计数，重新连接
- ✅ `status` 回到 `"connected"`

### Group F — 进程共享：入口 Worker 持有共享池（方案 C，Phase 4）

> 验证方案 C：入口 Worker 持有一份 MCP 连接（所有协议），所有子 Worker 通过共享池代理调用。
> 不再有"子 Worker 跳过 stdio"的概念——所有 Worker 都能看到全部 `mcp__*` 工具。

#### F1 入口 Worker 持有 MCP 连接

```bash
# config.json 配了 stdio + http 两种 server
# ion --host "做这个" → 入口 Worker 启动 → connect_all
ion rpc --session <entry_sid> --method get_mcp_servers
```

**预期响应：**
```json
{
  "data": [
    {"name":"filesystem","transport":"stdio","status":"connected","tools":[...]},
    {"name":"remote-api","transport":"streamable-http","status":"connected","tools":[...]}
  ]
}
```

**验证点：**
- ✅ 入口 Worker 的所有 server（含 stdio + http）都 `status: connected`
- ✅ 每个 server 只 spawn 了 **1 份进程**（不重复）

#### F2 子 Worker 共享 MCP 连接（不自己 spawn）

```bash
# 入口 Worker 调 spawn_worker → 创建 developer Worker
# developer Worker 不自己 connect_all，而是共享入口 Worker 的 MCP 池
ion rpc --session <developer_sid> --method get_mcp_servers
```

**预期响应：**
```json
{
  "data": [
    {"name":"filesystem","transport":"stdio","status":"connected","tools":[...]},
    {"name":"remote-api","transport":"streamable-http","status":"connected","tools":[...]}
  ]
}
```

**验证点：**
- ✅ developer Worker 看到**全部 server**（含 stdio！不再是方案 A/B 的"只有 http"）
- ✅ developer Worker **没有 spawn 新的 server 进程**（进程数不变）
- ✅ status 全部 `connected`（共享入口 Worker 的连接状态）

#### F3 子 Worker 的 LLM 能调 MCP 工具

```bash
# developer Worker 的 LLM 调用 stdio MCP 工具
ion rpc --session <developer_sid> --method call_tool \
  --params '{"tool":"mcp__filesystem__echo","args":{"message":"from developer"}}'
```

**预期响应：**
```json
{
  "success": true,
  "data": {"tool": "mcp__filesystem__echo", "output": "Echo: from developer"}
}
```

**验证点：**
- ✅ developer Worker 能调 **stdio** MCP 工具（方案 A/B 下做不到）
- ✅ 调用走共享池代理（不直连 server 进程）
- ✅ 返回结果正确

#### F4 agent.md 过滤 MCP 工具

```bash
# developer.md 配了 disallowed_tools: ["mcp__filesystem__delete_file"]
# developer Worker 的 LLM 不应看到 delete_file 工具
ion rpc --session <developer_sid> --method get_active_tools
```

**验证点：**
- ✅ 工具列表**不含** `mcp__filesystem__delete_file`（被 agent.md 过滤）
- ✅ 其他 MCP 工具仍在（`mcp__filesystem__echo` 等）
- ✅ `call_tool mcp__filesystem__delete_file` 返回 `tool not found`

#### F5 场景 2 退出时 MCP 连接关闭

```bash
# ion --host "做这个" → 任务完成 → 递归 idle → host 关闭
# 验证 MCP server 子进程被清理
ps aux | grep mcp-server   # 应无残留进程
```

**验证点：**
- ✅ host 退出后，MCP server 子进程**已终止**（无残留）
- ✅ 场景 2 是临时性的，MCP 连接随 host 退出自动断开

#### F6 场景 3 常驻时 MCP 连接保持

```bash
# ion serve → create_session → Worker 持有 MCP 连接
# session 销毁后，MCP 连接是否保持取决于是否还有其他 session
ion rpc --session <sid1> --method get_mcp_servers  # connected
# 销毁 sid1（最后一个 session）
# 再创建 sid2
ion rpc --session <sid2> --method get_mcp_servers   # 仍 connected（连接保持）
```

**验证点：**
- ✅ 场景 3 下 MCP 连接**常驻**（不随单个 session 销毁而断）
- ✅ 新 session 能看到已连接的 MCP server

#### F7 多 Worker 并发调用同一 MCP 工具

```bash
# coordinator 和 developer 同时调 mcp__filesystem__echo
# Terminal 1: ion rpc --session <coord_sid> --method call_tool --params '{"tool":"mcp__filesystem__echo",...}'
# Terminal 2: ion rpc --session <dev_sid> --method call_tool --params '{"tool":"mcp__filesystem__echo",...}'
```

**验证点：**
- ✅ 两个 Worker 并发调用同一 MCP 工具，**不冲突**（rmcp Peer 支持并发）
- ✅ 各自返回正确结果（不串号）

### Group G — 场景 1 MCP 支持（Phase 4）

> 验证场景 1（`ion "xxx"` 直接执行）也能用 MCP。当前缺口：cmd_run 没有 McpManager 初始化。

#### G1 场景 1 配了 MCP server 能用

```bash
# config.json 配了 mcp server
ion "用 mcp__filesystem__echo 工具回复 hello"
```

**验证点：**
- ✅ 场景 1 的 Agent 能看到 `mcp__*` 工具
- ✅ LLM 调用 MCP 工具成功

#### G2 场景 1 没配 MCP 零开销

```bash
# config.json 不含 mcp_servers
ion "hello"
```

**验证点：**
- ✅ 正常执行，无 MCP 初始化开销
- ✅ `is_empty()` 检查跳过

### Group H — MCP 工具权限控制（Phase 4）

> 验证 permission rules 能控制 MCP 工具调用（HOOK_SYSTEM.md 已预留 `mcp__.*` 匹配规则）。

#### H1 全局禁用某 MCP 工具

```bash
# ~/.ion/settings.json permissions.rules 加：
# {"subject":"*","pattern":"mcp__filesystem__delete_file","decision":"Deny"}
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"mcp__filesystem__delete_file","args":{...}}'
```

**验证点：**
- ✅ 调用被 Deny（permission 扩展拦截）
- ✅ 返回权限拒绝错误，不执行

#### H2 项目级规则覆盖

```bash
# 项目级 settings.json 允许某个被全局禁用的工具
# 全局: Deny mcp__filesystem__delete_file
# 项目: Allow mcp__filesystem__delete_file
```

**验证点：**
- ✅ 项目级 Allow 覆盖全局 Deny

#### H3 通配符匹配

```bash
# 规则: {"pattern":"mcp__filesystem__*","decision":"Deny"}
# 禁止 filesystem server 的所有工具
```

**验证点：**
- ✅ 所有 `mcp__filesystem__*` 工具被禁用
- ✅ 其他 server 的工具不受影响

### 用例覆盖矩阵

| 维度 | 覆盖 Group | case 数 | Phase | CI 脚本 |
|------|-----------|--------|-------|---------|
| **配置加载** | A (5) + A2 (7) | 12 | 1 ✅ | 7/12 已写 |
| **运行时 toggle** | B (4) | 4 | 1 ✅ | 3/4 已写 |
| **restart** | C (2) | 2 | 1 ✅ | 2/2 已写 |
| **stdio 真实连接** | D (5) | 5 | 2 ✅ | 5/5 已写（E1-E5） |
| **HTTP 真实连接** | D2 (3) | 3 | 2 ✅ | 未写 |
| **混合类型** | D3 (2) | 2 | 2 ✅ | 部分 |
| **重连恢复** | E (3) | 3 | 3 ✅ | 未写 |
| **进程共享（方案 C）** | F (7) | 7 | 4 待做 | 未写 |
| **场景 1 MCP** | G (2) | 2 | 4 待做 | 未写 |
| **权限控制** | H (3) | 3 | 4 待做 | 未写 |
| **合计** | | **43** | | **19/43 已写** |

---

## 6. 后续工作

| Phase | 内容 | 依赖 | 预估 |
|-------|------|------|------|
| **1** | `McpServerConfig` + IonConfig 字段 + **全局(`~/.ion/`)+项目维度(`~/.ion/projects/<key>/`)两级配置（server name 浅合并，worktree 共享）** + 3 RPC 命令填充 | 无 | ~2.5h |
| **2** | rmcp **1.x** 接入 + `McpManager` 真实连接（stdio/http）+ `McpTool` 注册 + 工具发现 + LLM 可调 | rmcp 1.x（已验证兼容 Rust 1.92） | ~4h |
| **3** | 自动重连（指数退避）+ 连接变更事件推送 + `ION_SKIP_MCP` 子进程隔离 | Phase 2 | ~3h |
| **4** | resources/prompts 探索 | Phase 3 | ~2h |

### Phase 1 验收标准 ✅

- [x] `McpServerConfig` 加入 IonConfig，`cargo build` 通过
- [x] **全局 + 项目维度两级配置合并**（server name 浅合并）正确生效
- [x] **项目维度配置存 `~/.ion/projects/<key>/config.json`**（不放进仓库）
- [x] 3 个 RPC 命令返回正确的配置数据（不真实连接）
- [x] Group A + B + C 测试用例通过（`tests/mcp_ci.sh`，15 个全过）
- [x] PI_RPC_ALIGNMENT.md 的 MCP 三件套改为 ✅ Phase 1

### Phase 2 验收标准

- [ ] rmcp 1.x 加入 Cargo.toml，`cargo build` 通过
- [ ] `McpManager::connect_all()` 并发连接多个 server，单台失败不阻断
- [ ] stdio 传输：spawn 子进程 + `list_tools` 发现工具
- [ ] HTTP 传输：连接远程 server + 工具发现
- [ ] `McpTool::execute()` 调 rmcp `call_tool`，结果正确格式化
- [ ] LLM 通过 `mcp__server__tool` 调用 MCP 工具，与内置工具无差别
- [ ] 连接超时（30s）+ 调用超时（60s）正确触发
- [ ] Group D 真实连接测试用例通过（`tests/mcp_ci.sh` 扩展）

### rmcp 依赖配置（Phase 2 引入时）

```toml
[dependencies]
rmcp = { version = "1", default-features = false, features = [
    "client",                                  # Client 模式（连 MCP server）
    "transport-child-process",                 # stdio 传输（spawn 子进程）
    "transport-streamable-http-client-reqwest", # HTTP 传输（远程 server）
] }
```

3 个 feature 覆盖 ION 需要的全部传输方式。连 `https://` 的 MCP server 需额外加 `"reqwest"`（rustls TLS provider）；只连 `http://` 本地 server 不需要。

---

## 7. 内核模块结构

```
src/mcp/
├── mod.rs           ← McpManager + ServerEntry + ServerStatus + DiscoveredTool
├── config.rs        ← McpServerConfig（或直接放 src/config.rs）
└── tool.rs          ← McpTool: Tool 适配器（Phase 2）

src/config.rs        ← IonConfig.mcp_servers 字段
src/bin/ion_worker.rs ← 3 个 RPC 命令实现 + 启动时 connect_all
src/agent/tool.rs    ← Tool trait（已有，McpTool impl 它）
```

---

## 决策索引

| 决策 | 章节 | 结论 |
|------|------|------|
| MCP 是内核还是扩展 | §2.5 | **内核**（独立模块，多扩展共用） |
| 用什么库 | §2.5 | **rmcp**（官方 Rust SDK） |
| 工具命名规范 | §2.5 | `mcp__<server>__<tool>` |
| 支持哪些传输 | §1.3 | stdio + Streamable HTTP（不支持 SSE） |
| 启动时机 | §2.5 | eager（agent 启动时全连接） |
| 子进程隔离 | §2.5 | `ION_SKIP_MCP=1` 跳过 |
| resources/prompts | §2.5 | Phase 4 再考虑 |
