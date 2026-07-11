# MCP 方案 C 实现规格 — Host 级共享池

> **状态：设计定稿** — 方案 C（入口 host 持有 MCP 连接，所有 Worker 共享代理调用）

## 概览

把 McpManager 从 Worker 进程移到 host 进程。所有 Worker（含入口 + 子 Worker）通过 ManagerBridge 代理调用 MCP 工具。

### 当前问题

```
当前（方案 B）：
  入口 Worker → 持有 stdio + HTTP MCP 连接
  子 Worker  → 只持有 HTTP（stdio 被 skip）← developer 看不到 stdio 工具
  host       → 没有 McpManager（完全不参与 MCP）
```

### 方案 C 目标

```
方案 C：
  host       → 持有 McpManager（唯一一份连接，含 stdio + HTTP）
  入口 Worker → 不自己连（ION_SKIP_MCP=1），通过 bridge 代理调
  子 Worker  → 同上，也通过 bridge 代理调
  所有 Worker 的 LLM 都能看到全部 mcp__* 工具
```

---

## 改动点（6 个文件）

### 1. `src/bin/ion.rs` — host 创建 McpManager

在 `cmd_serve_start` 和 `cmd_host` 里：

```rust
// 创建 McpManager（host 级单例）
let mcp_config = ion_cfg.mcp_servers.clone();
let mcp_manager = Arc::new(ion::mcp::McpManager::new(mcp_config));
if !mcp_manager.is_empty() {
    tracing::info!("[mcp] host connecting {} server(s)...", mcp_manager.server_count());
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        mcp_manager.connect_all(),
    ).await;
    mcp_manager.spawn_reconnect_monitor();
}

// 存进 WorkerRegistry
registry.lock().await.set_mcp_manager(mcp_manager.clone());

// 所有 Worker（含入口）都设 ION_SKIP_MCP=1（不自己连，走 host 代理）
```

### 2. `src/worker_registry.rs` — 加 mcp_manager 字段 + 3 个命令

```rust
pub struct WorkerRegistry {
    workers: HashMap<String, WorkerHandle>,
    // ... 现有字段 ...
    /// Host 级 MCP 管理器（方案 C：所有 Worker 共享）
    mcp_manager: Option<Arc<ion::mcp::McpManager>>,
}
```

`process_pending_commands` 加 3 个 case：

```rust
"mcp_call_tool" => {
    // 子 Worker → host 代理调 MCP 工具
    let server = params.get("server").and_then(|v| v.as_str()).unwrap_or("");
    let tool = params.get("tool").and_then(|v| v.as_str()).unwrap_or("");
    let args = params.get("args").cloned().unwrap_or_default();
    if let Some(ref mgr) = self.mcp_manager {
        match mgr.call_tool(server, tool, args).await {
            Ok(output) => self.write_manager_response(from_worker, reply_to,
                json!({"success":true,"data":{"output":output}})),
            Err(e) => self.write_manager_response(from_worker, reply_to,
                json!({"success":false,"error":e})),
        }
    } else {
        self.write_manager_response(from_worker, reply_to,
            json!({"success":false,"error":"mcp not available on host"}))
    }
}

"mcp_list_tools" => {
    // 子 Worker 启动时拉取工具列表
    if let Some(ref mgr) = self.mcp_manager {
        let tools = mgr.all_discovered_tools().await;
        self.write_manager_response(from_worker, reply_to,
            json!({"success":true,"data":{"tools":tools}}))
    }
}

"mcp_get_servers" => {
    // 子 Worker 查 MCP server 状态（转发 host 的真实状态）
    if let Some(ref mgr) = self.mcp_manager {
        let servers = mgr.server_list_json().await;
        self.write_manager_response(from_worker, reply_to,
            json!({"success":true,"data":servers}))
    }
}
```

### 3. `src/mcp/tool.rs` — McpProxyTool（走 bridge 代理）

```rust
/// MCP 工具代理适配器。
/// execute() 不直连 rmcp，而是通过 ManagerBridge 发 mcp_call_tool 给 host。
pub struct McpProxyTool {
    full_name: String,
    description: String,
    parameters: serde_json::Value,
    server_name: String,
    tool_name: String,
    bridge: Arc<ManagerBridge>,
}

#[async_trait]
impl Tool for McpProxyTool {
    fn name(&self) -> &str { &self.full_name }
    fn description(&self) -> &str { &self.description }
    fn parameters(&self) -> serde_json::Value { self.parameters.clone() }

    async fn execute(&self, args: Value, _rt: &dyn Runtime) -> AgentResult<String> {
        let resp = self.bridge.send_command("mcp_call_tool", json!({
            "server": self.server_name,
            "tool": self.tool_name,
            "args": args,
        })).await.map_err(|e| AgentError::Tool(e))?;

        if resp.get("success").and_then(|v| v.as_bool()).unwrap_or(false) {
            Ok(resp.get("data").and_then(|d| d.get("output"))
               .and_then(|o| o.as_str()).unwrap_or("").to_string())
        } else {
            Err(AgentError::Tool(
                resp.get("error").and_then(|e| e.as_str()).unwrap_or("unknown").into()
            ))
        }
    }
}
```

### 4. `src/bin/ion_worker.rs` — Worker 启动时拉取工具列表

Worker 不再自己 connect_all，而是在启动后通过 bridge 拉 host 的工具列表，注册 McpProxyTool：

```rust
// 方案 C：不自己连 MCP，从 host 拉工具列表注册代理
if skip_mcp && /* 有 bridge */ {
    let resp = bridge.send_command("mcp_list_tools", json!({})).await;
    if let Ok(tools_json) = resp {
        let tools: Vec<DiscoveredTool> = serde_json::from_value(tools_json["data"]["tools"]).unwrap_or_default();
        for tool in &tools {
            tools.register(Box::new(McpProxyTool::new(tool, bridge.clone())));
        }
    }
}
```

### 5. `src/runtime.rs` — spawn_worker 不再传 skip_mcp

所有 Worker 统一 `ION_SKIP_MCP=1`（host 持有连接）：

```rust
// 方案 C：所有 Worker（入口 + 子）都 skip，走 host 代理
"skip_mcp": "1",
```

### 6. `src/bin/ion.rs` — 场景 1（cmd_run）MCP 支持

cmd_run 不经过 host，直接在主进程跑。需要自己持有 McpManager：

```rust
// 场景 1：直接 McpManager + McpTool（不经过 bridge）
let mcp_manager = Arc::new(McpManager::new(cfg.mcp_servers.clone()));
if !mcp_manager.is_empty() {
    mcp_manager.connect_all().await;
    for tool in mcp_manager.all_discovered_tools().await {
        tools.register(Box::new(McpTool::new(&tool, mcp_manager.clone())));  // 直连版
    }
}
```

---

## 数据流对比

### 方案 C 下 Worker 调 MCP 工具的完整链路

```
子 Worker LLM 调 mcp__everything__echo
    ↓
McpProxyTool.execute()
    ↓
bridge.send_command("mcp_call_tool", {server:"everything", tool:"echo", args:{...}})
    ↓ (stdout JSONL)
host reader task → cmd_tx → process_pending_commands
    ↓
self.mcp_manager.call_tool("everything", "echo", args)
    ↓ (rmcp 直连)
MCP server 进程
    ↓ (响应)
host → write_manager_response → 子 Worker stdin
    ↓
bridge.deliver_response → McpProxyTool 返回结果
```

### 延迟分析

| 环节 | 延迟 |
|------|------|
| bridge stdout 写入 | ~0 |
| host 50ms 轮询 | 0-50ms（均值 25ms） |
| rmcp call_tool | server 本身执行时间 |
| host → Worker stdin 写回 | ~0 |
| **总额外开销** | **~25ms 均值** |

对比直连（方案 B 入口 Worker）的 0ms 额外开销，多了 ~25ms。对 LLM 工具调用（通常 100ms-数秒）无感。

---

## 场景适配

| 场景 | MCP 持有者 | Worker 怎么调 |
|------|-----------|--------------|
| **场景 1** `ion "xxx"` | 主进程自己（cmd_run 里建 McpManager） | 直连（McpTool，不走 bridge） |
| **场景 2** `ion --host` | host 进程 | bridge 代理（McpProxyTool） |
| **场景 3** `ion serve` | host 进程 | bridge 代理（McpProxyTool） |

---

## 实现顺序

1. `WorkerRegistry` 加 `mcp_manager` 字段 + setter
2. `process_pending_commands` 加 3 个 MCP 命令 case
3. `McpProxyTool`（mcp/tool.rs）
4. `ion_worker.rs` Worker 启动拉工具列表 + 注册 McpProxyTool
5. `ion.rs` host 创建 McpManager（cmd_serve_start + cmd_host）
6. `cmd_run` 场景 1 MCP 支持
7. CI 测试 Group F/G
