# mcp_tool handler 实现（hooks 第 5 种 handler，从 stub 变真实现）

## 背景
hooks 的 5 种 handler 里，command/http/prompt/agent 都已实现且有 CI。只有 mcp_tool 是 stub（`run_mcp_tool` 只打日志返回空）。原因之前是 worker 没有 McpManager——但方案 C 架构下 worker 通过 `manager_bridge` 转发给 host 的 McpManager，这条路已通（`McpProxyTool` 就是这么做的，`mcp_call_tool` RPC host 端已有）。

## 实现（3 个文件，~60 行改动）

### 1. `src/hooks/handler_runner.rs`
- `HookExecContext` 加字段：`pub manager_bridge: Option<Arc<dyn crate::runtime::ManagerBridgeHandle>>`
- dispatcher（line 58）：`McpTool => run_mcp_tool(handler, stdin_data, ctx)`（加 ctx 参数）
- `run_mcp_tool` 实现替换 stub：
  - 从 `handler.server` / `handler.tool` 取 server+tool（缺则返回 default）
  - args 合并：`handler.input` 为底 + `stdin.tool_input` 覆盖（不 merge 整个 stdin，避免 session_id 等污染）
  - `ctx.manager_bridge.send_command("mcp_call_tool", {server, tool, args}}`
  - 解析响应：success→interpret_stdout(output)；失败→HookOutcome::default()
  - 无 bridge 时返回 default + warn（场景 1 没 bridge，graceful 降级）

### 2. `src/hooks/extension.rs`
- HookExtension struct 加 `manager_bridge: Option<Arc<dyn ManagerBridgeHandle>>` 字段
- `new()` 加同名参数
- 构造 HookExecContext 处（line 112）塞 `manager_bridge: self.manager_bridge.clone()`

### 3. `src/bin/ion_worker.rs`
- `HookExtension::new()` 调用处（line 585）加参数：`Some(manager_bridge.clone() as Arc<dyn ion::runtime::ManagerBridgeHandle>)`

## 不改的
- `emit_handler_executed` 已处理 McpTool→"mcp_tool"（上次加的，不用改）
- hooks.json 配置格式已定义（server/tool/input 字段已存在）
- host 端 `mcp_call_tool` RPC 已实现（worker_registry.rs:1555）

## 测试
mcp_tool handler 的 CI 需要一个真实的 MCP server（mcp-server-everything），这在 CI 环境里不一定有。所以：
- **单元测试**（handler_runner）：mock bridge（实现 ManagerBridgeHandle trait 返回固定响应），验证 args 合并 + 响应解析
- **hooks_handler_ci.sh 加 Group D**：如果 mcp-server-everything 可用，测真实调用；不可用则 skip（不阻塞 CI）

## 验证
- cargo build --lib --bin ion --bin ion-worker
- cargo test --lib（含新增 mock bridge 单元测试）
- hooks_handler_ci.sh（确认 command/http/prompt 不回归）
- 文档：HOOKS_AND_OUTLINE_SYNC.md 的 mcp_tool 🔧→✅，5/5 完整