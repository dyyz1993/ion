# MCP 系统（Model Context Protocol）设计文档

> **状态：设计稿** — Phase 1（配置 + RPC 框架）待实现，Phase 2（rmcp 真实连接）待实现

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
| `McpServerConfig`（配置 struct，加入 IonConfig） | ❌ 待实现 | 1 |
| `get_mcp_servers` / `mcp_toggle_server` / `mcp_restart_server` RPC | ⚠️ 空桩已存在 | 1 填充 |
| `McpManager`（连接管理 + 工具发现） | ❌ 待实现 | 2 |
| `McpTool: Tool`（适配 Tool trait） | ❌ 待实现 | 2 |
| rmcp 真实 stdio / HTTP 连接 | ❌ 待实现 | 2 |
| 自动重连 + 事件推送 + 子进程隔离 | ❌ 待实现 | 3 |
| 项目维度配置（`~/.ion/projects/<key>/config.json`，worktree 共享） | ❌ 待实现 | 1 |

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
use std::sync::{Arc, RwLock};

pub struct McpManager {
    /// server name → 连接条目
    servers: Arc<RwLock<HashMap<String, ServerEntry>>>,
    /// 原始配置（用于 restart / toggle）
    config: HashMap<String, crate::config::McpServerConfig>,
}

struct ServerEntry {
    status: ServerStatus,
    /// 发现到的工具（连接成功后填充）
    tools: Vec<DiscoveredTool>,
    /// 连接/调用错误（status=Error 时填充）
    error: Option<String>,
    // Phase 2: client: Option<rmcp::model::Client>,
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
```

### 2.3 McpTool（Phase 2）

MCP server 提供的工具通过 `McpTool` 适配成 `Tool` trait，与 WASM 扩展的 `ToolAdapter` 模式完全一致。

位置：`src/mcp/tool.rs`

```rust
use crate::agent::tool::{Tool, AgentResult};
use crate::runtime::Runtime;
use async_trait::async_trait;

pub struct McpTool {
    full_name: String,         // mcp__server__tool
    description: String,
    parameters: serde_json::Value,
    manager: Arc<McpManager>,
}

impl McpTool {
    pub fn new(tool: &DiscoveredTool, manager: Arc<McpManager>) -> Self {
        Self {
            full_name: tool.full_name.clone(),
            description: tool.description.clone(),
            parameters: tool.input_schema.clone(),
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
        // Phase 2: manager.call_tool(&self.full_name, args).await
        //   → 反查 {server, tool} → rmcp client.call_tool(tool, args)
        //   → format_result（text 拼接 / image 占位）
        todo!("Phase 2")
    }
}
```

### 2.4 启动注册流程

位置：`src/bin/ion_worker.rs`（worker 启动时，在工具注册之后）

```rust
// 步骤 1: 读配置
let mcp_config = cfg.mcp_servers.clone();

// 步骤 2: 构建 manager（Phase 1 仅加载配置，不连接）
let mcp_manager = Arc::new(McpManager::new(mcp_config));

// 步骤 3 (Phase 2): 连接 + 注册工具
if std::env::var("ION_SKIP_MCP").ok().as_deref() != Some("1") {
    mcp_manager.connect_all().await;
    for tool in mcp_manager.all_tools() {
        tools.register(Box::new(McpTool::new(&tool, mcp_manager.clone())));
    }
}
// 把 mcp_manager 存起来，RPC 命令要用
```

### 2.5 关键决策点

| 决策 | 选择 | 理由 |
|------|------|------|
| **内核 vs 扩展** | **内核**（独立模块 `src/mcp/`） | 按 AGENTS.md 准则 4：多个无关扩展（权限扩展要识别 `mcp__.*`、file-snapshot 要追踪 mcp 工具改动）都会用到，属共用能力 |
| **客户端库** | rmcp（官方 Rust SDK） | 官方维护，跟协议演进；手写 JSON-RPC 易踩 protocol version 兼容坑 |
| **工具命名** | `mcp__<server>__<tool>` | 对齐 pi / Claude Code / Cursor；HOOK_SYSTEM.md 已预留 `mcp__.*` 权限匹配规则 |
| **启动时机** | eager（agent 启动时全连接） | 对齐 pi，避免 LLM 第一次调用 mcp 工具时才连接导致长延迟 |
| **子 Worker 隔离** | `ION_SKIP_MCP=1` 跳过 | 对齐 pi `PI_SKIP_MCP`；多 Worker 抢同一 stdio server 会死锁 |
| **resources/prompts** | ❌ 暂不实现 | pi 也没实现；tools 是 MCP 核心价值，resources/prompts 留 Phase 4 |
| **错误处理** | tool 失败返回错误文本，不中断 agent | 对齐 pi：MCP server 故障不应阻断主 agent 流程 |
| **工具 schema** | 原样保留 MCP 返回的 JSON Schema | MCP schema 不一定符合严格格式，原样传给 LLM（对齐 pi `Type.Unsafe`） |

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

> 完整测试脚本：`tests/mcp_ci.sh`（Phase 1 实现后编写）。以下为用例大纲。

### Group A — 配置加载（Phase 1）

| # | 用例 | 预期 |
|---|------|------|
| A1 | 空 `mcp_servers` 调 `get_mcp_servers` | `data: []`，success:true |
| A2 | 配置 1 个 stdio server（disabled:false）调 `get_mcp_servers` | 返回该 server，transport:"stdio"，status:"disconnected"，tools:[] |
| A3 | 配置 1 个 http server 调 `get_mcp_servers` | transport:"streamable-http" |
| A4 | 配置 disabled:true 的 server | 返回该 server 但 disabled:true |
| A5 | 同时配置 stdio + http 两个 server | data 数组含两条 |

### Group A2 — 配置层级合并（Phase 1）

| # | 用例 | 预期 |
|---|------|------|
| A2-1 | 全局配 `kb` + 项目维度配 `linter`，get_mcp_servers | data 含 2 个 server（浅合并，不同名全保留） |
| A2-2 | 全局配 `kb`(enabled) + 项目维度 `kb`(disabled:true)，get_mcp_servers | data 只有 1 个 `kb`，disabled:true（项目维度覆盖全局） |
| A2-3 | 仅项目维度有 mcp_servers，全局没有 | 读到项目维度的 server（全局未配置=空 map） |
| A2-4 | 主仓库和其 worktree 跑 `get_mcp_servers` | 两边返回**相同**的 server 列表（同一 `<project_key>`，天然共享） |
| A2-5 | **仓库内 `.ion/` 被 gitignore，worktree 目录无 `.ion/`**，get_mcp_servers | 仍读到项目维度配置（存 `~/.ion/projects/<key>/`，不依赖 git 同步） |
| A2-6 | 项目维度配置文件 `~/.ion/projects/<key>/config.json` 不存在 | 回退到仅全局配置，不报错（`get_mcp_servers` 返回全局的 server） |
| A2-7 | 两个不同 git 仓库的项目维度配置 | `<project_key>` 不同，配置互不干扰（隔离验证） |

### Group B — 运行时 toggle（Phase 1）

| # | 用例 | 预期 |
|---|------|------|
| B1 | toggle 关闭已配置的 server | success:true，status:"disconnected" |
| B2 | toggle 开启已关闭的 server | success:true |
| B3 | toggle 不存在的 server | success:false，error:"unknown mcp server: X" |
| B4 | 缺少 `enabled` 参数 | success:false，error 提示缺字段 |

### Group C — restart（Phase 1）

| # | 用例 | 预期 |
|---|------|------|
| C1 | restart 已配置的 server | success:true（Phase 1 status:"disconnected"） |
| C2 | restart 不存在的 server | success:false |

### Group D — 真实连接（Phase 2，依赖 rmcp）

| # | 用例 | 预期 |
|---|------|------|
| D1 | 配置 echo server（本地 stdio 测试 server） | status:"connected"，tools 非空 |
| D2 | LLM 调用 `mcp__echo__echo` 工具 | 工具执行成功，返回结果 |
| D3 | kill server 进程后调工具 | 返回错误文本，agent 不中断 |
| D4 | toggle 关闭再开启 | status 变化 connected → disconnected → connected |
| D5 | restart server | 工具列表重新发现 |

### Group E — 隔离与边界（Phase 3）

| # | 用例 | 预期 |
|---|------|------|
| E1 | `ION_SKIP_MCP=1` 启动 Worker | `get_mcp_servers` 返回 `[]`（即便配置了） |
| E2 | 配置错误的 command（不存在的命令） | status:"error"，error 字段含错误信息 |
| E3 | 连接超时的 http server | status:"error"，30s 内返回 |

---

## 6. 后续工作

| Phase | 内容 | 依赖 | 预估 |
|-------|------|------|------|
| **1** | `McpServerConfig` + IonConfig 字段 + **全局(`~/.ion/`)+项目维度(`~/.ion/projects/<key>/`)两级配置（server name 浅合并，worktree 共享）** + 3 RPC 命令填充 | 无 | ~2.5h |
| **2** | rmcp 接入 + `McpManager` 真实连接（stdio/http）+ `McpTool` 注册 + 工具发现 | rmcp crate | ~6h |
| **3** | 自动重连（指数退避）+ 连接变更事件推送 + `ION_SKIP_MCP` 子进程隔离 | Phase 2 | ~3h |
| **4** | resources/prompts 探索 | Phase 3 | ~2h |

### Phase 1 验收标准

- [ ] `McpServerConfig` 加入 IonConfig，`cargo build` 通过
- [ ] **全局 + 项目维度两级配置合并**（server name 浅合并）正确生效
- [ ] **项目维度配置存 `~/.ion/projects/<key>/config.json`**（不放进仓库）
- [ ] **worktree 场景**：主仓库和 worktree 读到同一份项目维度配置（`<key>` 一致 + 不依赖 git 同步）
- [ ] 3 个 RPC 命令返回正确的配置数据（不真实连接）
- [ ] Group A + A2 + B + C 测试用例通过（`tests/mcp_ci.sh`）
- [ ] PI_RPC_ALIGNMENT.md 的 MCP 三件套从 ❌ 改为 ✅（Phase 1）

### rmcp feature 组合（Phase 2 引入时）

```toml
[dependencies]
rmcp = { version = "0.x", features = [
    "client",                                  # Client 模式（连 MCP server）
    "transport-child-process",                 # stdio 传输（spawn 子进程）
    "transport-streamable-http-client-reqwest", # HTTP 传输（远程 server）
] }
```

3 个 feature 覆盖 ION 需要的全部传输方式。

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
