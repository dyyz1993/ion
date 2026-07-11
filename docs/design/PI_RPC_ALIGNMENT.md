# pi RPC CLI 对齐文档

> **状态：开发中** — 本文记录 pi 当前 RPC 调试能力现状，以及 ion 的对齐方案。
> 关联 issue：https://github.com/dyyz1993/pi-mono/issues/53
>
> **P0 调试 RPC 已完成 (2026-07-05)**：11 个 RPC 全部实现，ion-worker 编译通过，91 个 lib 测试全过。
>
> **术语约定**：pi 的扩展系统叫 Extension（不叫 Plugin）。本文统一使用 extension 术语。
> ion 代码里的方法名 `extension_rpc` / flag `--extension` 是历史命名，对应 pi 的 extension channel。

## 一、背景

ion 的 `ion rpc` / `ion subscribe` / `ion rpc --method extension_rpc` 三件套已经实现并验证（见 [CLI_USAGE.md](./CLI_USAGE.md)）。调研发现 pi **完全没有命令行 RPC 客户端**，所有调试必须写 Node.js 脚本。本文件记录 pi 现状、ion 已有能力、以及 ion 这边需要补的对齐项。

## 二、pi 当前状态

### 2.1 pi 没有命令行 RPC 客户端

| 能力 | pi 现状 | ion 现状 |
|---|---|---|
| 调实例级 RPC 方法 | ❌ 必须写 Node 脚本用 `RpcClient` | ✅ `ion rpc --method xxx` |
| 调 extension channel 方法 | ❌ 必须写 `ClientChannel<T>` 类型化客户端 | ✅ `ion rpc --method extension_rpc` |
| 订阅事件流 | ❌ 必须写脚本用 `client.onEvent()` | ✅ `ion subscribe --session x` |
| 列出所有 RPC 命令 | ❌ 翻 8 个文档文件 | ❌ 也缺（见对齐项） |

### 2.2 pi 的 RPC 协议（ion 已对齐部分）

pi 的 RPC 服务端通过 `pi --mode rpc` 启动，stdin/stdout 走 JSONL：

**请求**（[rpc-types.ts:28-200](file:///Users/xuyingzhou/Project/temporary/pi-momo-fork/packages/coding-agent/src/modes/rpc/rpc-types.ts)）：
```json
{"id":"req_1","type":"prompt","message":"hello"}
```

**响应**（[rpc-types.ts:401-690](file:///Users/xuyingzhou/Project/temporary/pi-momo-fork/packages/coding-agent/src/modes/rpc/rpc-types.ts)）：
```json
{"id":"req_1","type":"response","command":"prompt","success":true}
```

**事件**（[rpc-events.md](file:///Users/xuyingzhou/Project/temporary/pi-momo-fork/packages/coding-agent/docs/rpc.md)）：
```json
{"type":"agent_start"}
{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"hello"}}
```

### 2.3 pi 的 75 个 RPC 命令（按域分组）

完整列表见 [rpc-types.ts:28-200](file:///Users/xuyingzhou/Project/temporary/pi-momo-fork/packages/coding-agent/src/modes/rpc/rpc-types.ts)，按域：

| 域 | 命令数 | 代表命令 |
|---|---|---|
| Prompting | 6 | prompt, steer, follow_up, continue, abort, new_session |
| State | 1 | get_state |
| Model | 5 | set_model, cycle_model, get_available_models, get_tier_models, set_tier_models |
| Thinking | 2 | set_thinking_level, cycle_thinking_level |
| Queue modes | 2 | set_steering_mode, set_follow_up_mode |
| Compaction | 2 | compact, set_auto_compaction |
| Retry | 2 | set_auto_retry, abort_retry |
| Bash | 2 | bash, abort_bash |
| Session | 14 | switch_session, fork, navigate_tree, delete_entries, summarize_entries, clone, ... |
| Messages | 8 | get_messages, get_full_messages, get_tree, get_tree_with_leaf, get_modified_files, get_file_diff, ... |
| Commands/Resources | 4 | get_commands, get_skills, get_extensions, get_tools |
| Settings | 2 | get_settings, set_settings |
| Context | 2 | get_context_usage, get_system_prompt |
| Tools | 2 | get_active_tools, set_active_tools |
| Queue ops | 3 | get_queue, clear_queue, promote_follow_up |
| Flags | 3 | get_flags, get_flag_values, set_flag |
| Reload | 1 | reload |
| Cwd | 1 | set_cwd |
| Agents | 7 | get_agents, switch_agent, get_current_agent, get_agent_detail, ... |
| Permission | 1 | set_permission_mode |
| MCP | 3 | get_mcp_servers, mcp_toggle_server, mcp_restart_server |
| Remote tools | 2 | register_remote_tool, unregister_remote_tool |

### 2.4 pi 的 Channel（extension RPC）机制

extension通过 `registerChannel(name)` 注册 channel（[channel-manager.ts:15-21](file:///Users/xuyingzhou/Project/temporary/pi-momo-fork/packages/coding-agent/src/core/extensions/channel-manager.ts)）：

```ts
channel.call(method, params, timeoutMs)  // 请求-响应
channel.send(data)                       // 单向发送
channel.onReceive(handler)               // 注册接收处理器
```

底层 `call` = `invoke({__call:method, ...params})`，通过 `channel_data` 消息传输。

**类型化包装**：`ServerChannel<T>` / `ClientChannel<T>` 基于 `ChannelContract` 接口提供类型安全。

**调试痛点**：调一个extension方法必须写完整类型化客户端，没有 `pi channel call todo getTodos '{}'` 这种快速调试方式。

## 三、ion 当前状态

### 3.1 ion 已实现的 RPC CLI（[CLI_USAGE.md](./CLI_USAGE.md)）

```bash
# Manager 级
ion rpc --method list_sessions
ion rpc --method create_session --params '{"agent":"developer"}'
ion rpc --method create_worker --params '{...}'

# Instance 级
ion rpc --session x --method get_messages
ion rpc --session x --method prompt --params '{"text":"hi"}'
ion rpc --session x --method get_state

# Tool 级（直接调工具，不经过 LLM）
ion rpc --session x --method call_tool --params '{"tool":"read","args":{"path":"Cargo.toml"}}'

# Extension 级（调 extension 私有方法）
ion rpc --session x --method extension_rpc --params '{"method":"save","args":{...}}'

# Subscribe（实时事件流）
ion subscribe --session x
ion subscribe --session x --extension memory
```

### 3.2 ion 的 RPC 命令清单（51 个 Instance 级）

来源：[src/bin/ion_worker.rs](./src/bin/ion_worker.rs) 的 `match method.as_str()`：

**已实现**：get_state / get_session_stats / get_messages / get_last_assistant_text / get_tools / set_model / set_thinking_level / set_session_name / prompt / steer / abort / promote_follow_up / channel_msg / create_worker / channel_send / send_to_worker / kill / get_system_prompt / get_agents / get_current_agent / compact / delete_entries / summarize_entries / switch_agent / bash / call_tool / extension_rpc / reload / get_agent_detail / continue / follow_up / extension_add / extension_remove / extension_list / extension_reload / append_system_event / append_custom_message / append_custom_entry / send_custom_message / append_model_change / append_thinking_level_change / append_agent_change / append_session_name / append_label / append_active_tools_change / bash_command / manager_response

### 3.3 ion vs pi RPC 对比矩阵

| pi 命令 | ion 对应 | 对齐状态 |
|---|---|---|
| `prompt` | `prompt` | ✅ |
| `steer` | `steer` | ✅ |
| `follow_up` | `follow_up` | ✅ |
| `continue` | `continue` | ✅ |
| `abort` | `abort` | ✅ |
| `new_session` | Manager 级 `create_session` | ⚠️ 分级不同 |
| `get_state` | `get_state` | ✅ |
| `set_model` | `set_model` | ✅ |
| `cycle_model` | ✅ | ✅ 已实现（同 provider 内循环 + 写 session_index） |
| `get_available_models` | ✅ | ✅ 已实现（从 ModelRegistry.list_models() 读） |
| `get_tier_models` / `set_tier_models` | ✅ | ✅ 已实现（config.json tier_models + fast/pro/max 别名解析） |
| `set_thinking_level` | `set_thinking_level` | ✅ |
| `cycle_thinking_level` | ✅ | ✅ 已实现（6 档循环 + 写 session_index） |
| `compact` | `compact` | ✅ |
| `set_auto_compaction` | ✅ | ✅ 已实现（调 agent.set_auto_compact()） |
| `set_auto_retry` / `abort_retry` | ✅ | ✅ 已实现（set_max_retries + agent.stop()） |
| `bash` / `abort_bash` | `bash` / ✅ | ✅ 全部（abort_bash 通过 process_map kill SIGTERM） |
| `get_messages` | `get_messages` | ✅ |
| `get_full_messages` | ✅ | ✅ 已实现（返回 messages + count + note） |
| `get_tree` / `get_tree_with_leaf` | ✅ / ✅ | ✅ 全部（structure/full 双模式 + pathToLeaf + branches） |
| `get_modified_files` / `get_file_diff` | ✅ | ✅ 已实现（File Snapshot 双路快照） |
| `switch_session` / `fork` / `clone` | Manager 级 | ⚠️ 分级不同 |
| `navigate_tree` | ✅ | ✅ 已实现（线性节点列表 + onLeafPath/isCurrentLeaf 标记） |
| `delete_entries` / `summarize_entries` | `delete_entries` / `summarize_entries` | ✅ |
| `get_session_stats` | `get_session_stats` | ✅ |
| `get_commands` / `get_skills` / `get_extensions` / `get_tools` | ✅ / ✅ / ✅ / ✅ | ✅ 全部 |
| `get_settings` / `set_settings` | ✅ | ✅ 已实现（IonConfig load/save + api_key 脱敏） |
| `get_context_usage` | ✅ | ✅ 已实现（估算 tokens + usagePercent + autoCompaction） |
| `get_system_prompt` | `get_system_prompt` | ✅ |
| `get_active_tools` / `set_active_tools` | ✅ | ✅ 已实现（agent.list_tool_names / restrict_tools） |
| `get_queue` / `clear_queue` | ✅ | ✅ 已实现（队列内容快照 / 清空 steering+follow_up） |
| `promote_follow_up` | `promote_follow_up` | ✅ |
| `get_flags` / `set_flag` | ✅ | ✅ 已实现（ExtensionRegistry 运行时 flag 存储 + 所有 JSON 类型） |
| `reload` | `reload` | ✅ |
| `set_cwd` | ✅ | ✅ 已实现（agent.set_session_cwd + 路径验证） |
| `get_agents` / `switch_agent` / `get_current_agent` / `get_agent_detail` | 全部对应 | ✅ |
| `set_permission_mode` | ✅ | ✅ 已实现（Runtime::set_guard_mode，CommandGuard 改 Arc<RwLock>） |
| `get_mcp_servers` / `mcp_toggle_server` / `mcp_restart_server` | ❌ | ❌ 缺 |
| `register_remote_tool` / `unregister_remote_tool` | ❌ | ❌ 缺 |
| — | `call_tool`（Tool 级直调） | ion 原创 |
| — | `extension_rpc`（Extension 级直调） | ion 原创 |
| — | `extension_add/remove/list/reload` | ion 原创 |
| — | `append_*`（10 个 entry 追加命令） | ion 原创 |

**统计**：
- ✅ 已对齐：**21 个**
- ⚠️ 模式不同：**6 个**
- ❌ 缺失：**约 25 个**

## 四、ion 这边需要对齐的 RPC（按优先级）

### 🔴 P0 — 调试必备，实现简单 ✅ 已完成 (2026-07-05)

| RPC | 调试用途 | 实现状态 |
|---|---|---|
| `get_queue` | 看 steering/follow_up 队列状态 | ✅ 真实实现（返回队列内容快照） |
| `clear_queue` | 清空队列 | ✅ 新增（清空 steering + follow_up） |
| `get_context_usage` | 看 context token 用量 | ✅ 新增（估算 tokens + usagePercent + autoCompaction 状态） |
| `get_active_tools` | 看当前工具集 | ✅ 新增（调 agent.list_tool_names()） |
| `set_active_tools` | 改工具集 | ✅ 真实实现（调 agent.restrict_tools()） |
| `get_full_messages` | 拿完整消息（含 thinking） | ✅ 新增（返回 messages + count + note） |
| `get_available_models` | 列出可用模型 | ✅ 真实实现（从 ModelRegistry.list_models() 读） |
| `cycle_model` | 循环切模型 | ✅ 真实实现（同 provider 内循环 + 写 session_index） |
| `cycle_thinking_level` | 循环切 thinking | ✅ 真实实现（6 档循环 + 写 session_index） |
| `set_auto_compaction` | 自动压缩开关 | ✅ 新增（调 agent.set_auto_compact()） |

**改动文件**：
- [src/agent/agent_loop.rs](./src/agent/agent_loop.rs) — Agent 加 9 个 getter/setter
- [ion-provider/src/registry.rs](../ion-provider/src/registry.rs) — ModelRegistry 加 list_models + models_by_provider
- [src/bin/ion_worker.rs](./src/bin/ion_worker.rs) — 11 个 RPC 实现 + 删除 2 个重复 stub

### 🟡 P1 — 重要，实现中等

| RPC | 调试用途 | 状态 |
|---|---|---|
| ~~`set_permission_mode`~~ | 切权限模式 | ✅ 已实现（Runtime::set_guard_mode） |
| ~~`set_cwd`~~ | 切工作目录 | ✅ 已实现（agent.set_session_cwd + 路径验证） |
| ~~`set_auto_retry`~~ / ~~`abort_retry`~~ | 重试控制 | ✅ 已实现（set_max_retries + agent.stop()） |
| ~~`abort_bash`~~ | 中断 bash 执行 | ✅ 已实现（process_map kill SIGTERM） |
| ~~`get_settings`~~ / ~~`set_settings`~~ | 统一设置管理 | ✅ 已实现（IonConfig load/save + api_key 脱敏） |
| ~~`get_modified_files`~~ / ~~`get_file_diff`~~ | 看本次 session 改了哪些文件 | ✅ 已实现（File Snapshot 双路快照） |
| ~~`get_batch_diffs`~~ / ~~`get_file_history`~~ | 批量 diff + 单文件历史 | ✅ 已实现（按 path 分组聚合 + 按 turn 时间线） |

### 🟢 P2 — 依赖底层能力

| RPC | 依赖 | 状态 |
|---|---|---|
| ~~`get_tree`~~ / ~~`get_tree_with_leaf`~~ | Session Tree | ✅ 已实现 |
| ~~`navigate_tree`~~ | Session Tree | ✅ 已实现 |
| ~~`get_flags`~~ / ~~`set_flag`~~ | 扩展 flag 系统 | ✅ 已实现（ExtensionRegistry 运行时存储） |
| ~~`get_commands`~~ / ~~`get_skills`~~ | slash 命令 + skill 系统 | ✅ 已实现（RPC 命令列表 + skill 文件扫描） |
| MCP 三件套 | MCP client | ❌ 待实现 |
| Remote tools 三件套 | 远程工具协议 | ❌ 待实现 |
| MCP 三件套 | 依赖 MCP client 实现 |
| Remote tools 三件套 | 依赖远程工具协议 |

## 五、ion 这边的改造方案

### 5.1 第 1 步：补 P0 调试 RPC（优先级最高）

9 个 RPC，约 200 行代码。每个都是 `ion_worker.rs` 里加一个 `match` 分支。

**示例**（`get_queue` 实现）：

```rust
"get_queue" => {
    let steering = agent.steering_queue();
    let follow_up = agent.follow_up_queue();
    let queue = serde_json::json!({
        "steering": steering.iter().map(|m| message_to_json(m)).collect::<Vec<_>>(),
        "follow_up": follow_up.iter().map(|m| message_to_json(m)).collect::<Vec<_>>(),
        "steering_mode": agent.steering_mode(),
        "follow_up_mode": agent.follow_up_mode(),
    });
    output_response(&id, "get_queue", &serde_json::json!({"success": true, "data": queue}));
}
```

### 5.2 第 2 步：补 Compaction 自动压缩

长对话必备。配合 `set_auto_compaction` RPC 开关。需要：
- `shouldCompact()` 触发判断
- `findCutPoint()` 找压缩切点
- LLM 生成 summary（用 `## Goal`/`## Progress`/`## Key Decisions` 模板）
- 写 `CompactionEntry` 到 JSONL

### 5.3 第 3 步：补多 Provider 协议

按使用频率排序：
1. `anthropic-messages` — Claude 系列
2. `google-generative-ai` — Gemini 系列
3. `openai-responses` — OpenAI 新协议
4. `bedrock-converse-stream` — AWS Bedrock
5. 其余 4 个

## 六、调试速查（ion 当前可用）

### 6.1 验证 RPC 链路

```bash
ion serve start
ion rpc --method create_session --params '{"agent":"developer"}'
# → {"session_id":"sess_xxx",...}

ion rpc --session sess_xxx --method get_state
# → {"model":"glm-4.7","provider":"zhipuai",...}
```

### 6.2 调试 extension 方法

```bash
# Memory extension RPC 直调（ion 方法名沿用 extension_rpc，对应 pi 的 extension channel）
ion rpc --session x --method extension_rpc --params '{"method":"save","args":{"content":"偏好 Rust","tags":["rust"]}}'
ion rpc --session x --method extension_rpc --params '{"method":"list","args":{"outline":"preferences"}}'
ion rpc --session x --method extension_rpc --params '{"method":"search","args":{"query":"rust"}}'
```

### 6.3 实时事件流

```bash
# 终端 1：订阅
ion subscribe --session sess_xxx

# 终端 2：触发
ion rpc --session sess_xxx --method prompt --params '{"text":"hello"}'

# 终端 1 实时看到：
# {"type":"subscribed","session":"sess_xxx","stream":"instance"}
# {"type":"instance_event","event":{"type":"agent_start","sessionId":"sess_xxx"}}
# {"type":"instance_event","event":{"type":"text_delta","delta":"..."}}
# {"type":"instance_event","event":{"type":"agent_end","sessionId":"sess_xxx"}}
```

### 6.4 调试 Memory extension事件

```bash
ion subscribe --session sess_xxx --extension memory
# → 收到 memory_saved / memory_injected / memory_consolidated 等事件
```

## 七、参考文件

### pi 侧

| 用途 | 文件 |
|---|---|
| CLI 参数解析 | `packages/coding-agent/src/cli/args.ts` |
| RPC 服务端 | `packages/coding-agent/src/modes/rpc/rpc-mode.ts` |
| RPC 客户端 SDK | `packages/coding-agent/src/modes/rpc/rpc-client.ts` |
| RPC 协议类型 | `packages/coding-agent/src/modes/rpc/rpc-types.ts` |
| Channel 管理器 | `packages/coding-agent/src/core/extensions/channel-manager.ts` |
| RPC 协议文档 | `packages/coding-agent/docs/rpc.md` |

### ion 侧

| 用途 | 文件 |
|---|---|
| CLI 主入口 | [src/bin/ion.rs](./src/bin/ion.rs) |
| Worker RPC 服务端 | [src/bin/ion_worker.rs](./src/bin/ion_worker.rs) |
| CLI 用法文档 | [CLI_USAGE.md](./CLI_USAGE.md) |
| Manager 命令处理 | [src/bin/ion.rs](./src/bin/ion.rs) `cmd_rpc` |
| Subscribe 实现 | [src/bin/ion.rs](./src/bin/ion.rs) `cmd_subscribe` |

## 八、推进顺序

1. **给 pi fork 提 issue**（dyyz1993/pi-mono）— 已起草，待提
2. **ion 补 P0 调试 RPC**（9 个，~200 行）— 下一个 sprint
3. **ion 补 Compaction 自动压缩** — 长对话刚需
4. **ion 补多 Provider 协议** — 最大工程量
5. **pi 那边根据 issue 讨论结果决定是否提 PR** — 等 maintainer 反馈
