# INPUT_ORIGIN — 输入来源标识（user / monitor / system）

> **状态：已实现（内核闭环）** — origin 贯穿 + 四诉求消费侧全部落地：④ 改写提示词（on_input）、②③ 按 origin 拒绝工具（OriginGate）、① 按 origin 隐藏工具（schema 级过滤，隐藏即拒）。

## 1. 背景与问题

消息进入 agent 循环的三种触发路径目前**无法在钩子层区分**：

| 触发方 | 路径 | 现状标识 |
|--------|------|---------|
| 用户主动发送 | UI/CLI → `prompt` RPC | 无标识（默认即用户） |
| 定时任务（monitor） | auto_spawn 的 `initial_prompt` / `channel_notify` | spawn 层有 `WorkerRelation::System`（不随消息走）；channel sender 带 `monitor:` 前缀（半结构化） |
| 内核注入（异步委派通知等） | `prompt` RPC + `behavior: followUp`，文本"（系统）…"前缀 | 仅文本约定，非结构化 |

后果：扩展的 `on_input`（`InputContext { text, handled }`）拿不到来源，无法针对 monitor/system 轮次做差异化处理（改写提示词、拒绝某些工具等）。JSONL 里也没有结构化留痕，回放/审计无法还原触发方。

**用户诉求**（按实现优先级）：
1. 修改提示词（monitor 轮改写输入）→ `on_input` 可直接做
2. 禁止工具执行（monitor 轮拒绝某工具）→ PreToolUse 拒绝闭环已上线，缺 origin 条件
3. 执行时看不到（结果不进上下文）→ 拒绝式 PreToolUse 近似
4. 隐藏工具（工具列表就没有）→ 需 per-turn 工具过滤，内核 agent loop 增强，**后续迭代**

## 2. 设计

### 2.1 origin 值域

| 值 | 含义 | 设置方 |
|----|------|--------|
| `user`（缺省） | 用户主动发送 | 不传即 user |
| `monitor` | 定时任务触发 | monitor_extension spawn / channel 路径 |
| `system` | 内核注入（异步委派完成通知等） | worker_registry 通知路径 |
| `peer` | peer_follow_up 汇报 | worker_registry peer 路径 |

非法值回落 `user`（宽容处理，不拒绝消息）。

### 2.2 传递链路（三层）

```
发送方（prompt params.origin / WorkerCreateConfig.input_origin）
    ↓
worker_rpc prompt 处理：agent.input_origin = origin（per-turn）
    ↓
agent_loop run_with_images：InputContext { text, handled, origin } → on_input 钩子可读
    ↓
落盘：origin ≠ "user" 时，user 条目旁追加一条 custom 条目（旁路，不进 LLM 上下文）
    {"type":"custom","customType":"input_origin","data":{"origin":"monitor","behavior":"...","ts":...}}
```

**与现有机制的关系**：
- `MessageSource`（Prompt/Steer/FollowUp/Interrupt）标**投递行为**，origin 标**触发来源**——正交，互不替代；
- JSONL `custom` 条目（AGENTS.md 定位：旁路/审计）是落盘槽位——本设计不发明新存储形态；
- `WorkerRelation::System` 保持 spawn 记账用途不变，`WorkerCreateConfig.input_origin` 负责消息层标识。

### 2.3 改动点清单

| # | 文件 | 改动 |
|---|------|------|
| 1 | `src/agent/extension.rs` | `InputContext` 加 `pub origin: String` |
| 2 | `src/agent/agent_loop.rs` | Agent 加 `pub input_origin: String`（默认 "user"）；`run_with_images` 构造 InputContext 时带上 |
| 3 | `src/worker_rpc.rs` | prompt 处理：读 `params.origin`（缺省 user，非法回落 user）→ 赋 `agent.input_origin`；**origin ≠ user 时**调 `append_custom_entry(cwd, "input_origin", {...})` |
| 4 | `src/worker_registry.rs` | `WorkerCreateConfig` 加 `input_origin: Option<String>`；两处 initial_prompt 注入点 params 带 `"origin"`；异步通知（1276/1989）params 加 `"origin":"system"`；peer_follow_up 加 `"origin":"peer"` |
| 5 | `src/monitor_extension.rs` | auto_spawn 的 `WorkerCreateConfig` 填 `input_origin: Some("monitor")` |
| 6 | `src/bin/ion.rs` | host_idle_session_read 等合成路径无需改（合成不进 agent loop） |

**不改动**：steer/followUp/interrupt 忙时臂（它们复用 prompt params，若带 origin 同样会被 #3 处理并落盘；MessageSource 照旧标投递行为）。

### 2.4 消费侧（已实现）

**配置总览（config.json，全局或项目级深度合并）**：

```json
{
  "extensions": { "origin_gate": { "enabled": true } },
  "origin_tools":     { "monitor": ["spawn_worker", "write"] },
  "origin_hide_tools": { "monitor": ["write"] }
}
```

| 诉求 | 机制 | 验证 |
|------|------|------|
| ④ 修改提示词 | `on_input` 读 `ctx.origin` 改写 `ctx.text` | input_origin_harness（`[定时任务]` 前缀落 user 消息） |
| ③② 拒绝执行/结果不可达 | **OriginGate 扩展**（`origin_tools` 配置）：`before_tool_call_with_origin`（Extension trait 新钩子，runner 分发时传本轮 origin）拒绝 → 错误 ToolResult → agent 继续 | origin_gate 单元 4 + harness（user 执行 1 次/monitor 0 次/拒绝条目）+ 端到端（同一 faux 脚本：monitor 轮 write 被拒、user 轮正常写入） |
| ① 隐藏工具 | **schema 级过滤**（`origin_hide_tools` 配置）：agent_loop 构建 provider 请求的 tool_defs 时按本轮 origin 剔除（不动注册表，下轮恢复）；**隐藏即拒**（幻觉调用也被 OriginGate 拒） | origin_hide_harness（Factory 捕获 Context.tools：monitor 轮无 hide_me、user 轮有） |

**规则**：`user` 不允许配置（防误锁用户轮）；非法 origin 回落 user；OriginGate 默认 enabled。

## 3. CLI 验证（实测数据，2026-09-04）

### 3.0 RPC 接口规格

**命令：**

```bash
ion rpc --session <sid> --method prompt --params '{"text":"...","origin":"monitor"}'
```

**请求参数表：**

| 字段 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `text` | string | 必填 | 用户提示词 |
| `origin` | string | `"user"` | 输入来源标识：`user` / `monitor` / `system` / `peer`；非法值回落 `user` |
| `behavior` | string | `"steer"` | 投递行为（既有字段，与 origin 正交） |
| `images` | array | `[]` | 视觉输入（既有字段） |

**响应 JSON（成功，空闲新轮）：**

```json
{"command":"prompt","data":null,"id":"rpc-client","success":true,"type":"response"}
```

**响应 JSON（忙时入队 steering）：**

```json
{"command":"prompt","data":{"queue":"steering","status":"queued"},"id":"rpc-client","success":true,"type":"response"}
```

### A 组：origin=monitor 落盘 + 正常应答

**步骤 1 — 创建会话：**

```bash
ion rpc --method create_session --params '{"session_id":"sess_origin_doc","cwd":"/tmp/ion-origin-doc"}'
```

真实响应：

```json
{"data":{"agent":"build","session_id":"sess_origin_doc_17351","status":"created"},"id":"rpc-client","success":true,"type":"response"}
```

**步骤 2 — 发送带 origin 的提示词：**

```bash
ion rpc --session sess_origin_doc_17351 --method prompt \
  --params '{"text":"doc demo round","origin":"monitor"}'
```

真实响应：

```json
{"command":"prompt","data":null,"id":"rpc-client","success":true,"type":"response"}
```

**步骤 3 — 查会话 JSONL 的留痕条目：**

```bash
grep "input_origin" ~/.ion/agent/sessions/<cwd_hash>/sess_origin_doc_17351.jsonl
```

实测落盘条目（真实原文）：

```json
{"customType":"input_origin","data":{"behavior":"steer","origin":"monitor","textPreview":"doc demo round","ts":1788497179639},"id":"82362f28","parentId":"sess_origin_doc_17351","timestamp":"2026-09-04T04:46:19.639Z","type":"custom"}
```

验证点：
- ✅ `type:"custom"` + `customType:"input_origin"`（旁路条目，不进 LLM 上下文）
- ✅ `data.origin:"monitor"`；`data.behavior` 记录实际投递行为；`data.textPreview` 前 80 字符留痕

**步骤 4 — 确认消息流正常（get_messages）：**

```bash
ion rpc --session sess_origin_doc_17351 --method get_messages --params '{"limit":10}'
```

实测该轮 `messages` 含 `User("doc demo round")` 与 Assistant 应答——origin 纯旁路，对话内容零变化。

验证点：
- ✅ agent 正常应答（origin 不影响主流程）

### B 组：缺省 origin 不落盘

```bash
ion rpc --session sess_origin_doc_17351 --method prompt --params '{"text":"plain user round"}'
# 响应 success:true
grep -c "input_origin" ~/.ion/agent/sessions/<cwd_hash>/sess_origin_doc_17351.jsonl
```

实测：条目数**保持 1**（A 组写入的那条，本轮未新增）。

验证点：
- ✅ `user` 是常态，不落留痕条目（JSONL 零噪音）

### C 组：非法 origin 回落 user

```bash
ion rpc --session sess_origin_doc_17351 --method prompt --params '{"text":"bad origin round","origin":"hacker"}'
# 响应 success:true（忙时 {"status":"queued"}，宽容接受不拒绝）
grep -c "input_origin" ~/.ion/agent/sessions/<cwd_hash>/sess_origin_doc_17351.jsonl
```

实测：条目数**保持 1**。

验证点：
- ✅ 非法值回落 `user`，消息本身不被拒绝

### 自动化

- `tests/origin_ci.sh`：起 host → create_session → A/B/C 三组 → 断言（实测 **8 ok / 0 FAIL**）
- `cargo test --test input_origin_harness`：FauxProvider harness——断言 `on_input` 钩子依次读到 `user`（缺省）与 `monitor`（显式），且 monitor 轮在钩子内改写提示词生效（`[定时任务]` 前缀写入 user 消息）
- `cargo test --test origin_gate_harness`：user 轮工具执行 1 次 / monitor 轮同一工具被拒 0 次 / 拒绝转错误 ToolResult 且 agent 不中断
- `cargo test --test origin_hide_harness`：Factory 捕获 provider 请求——monitor 轮 tools 列表不含被隐藏工具，user 轮含

### 调试提示

- 留痕条目只在 `origin ≠ "user"` 时落盘；查不到先确认发送时带了 `origin` 参数
- `data.behavior` 显示该次输入的实际投递行为；忙时发送响应为 `{"status":"queued"}`
- 钩子侧：扩展在 `on_input` 里读 `ctx.origin`；harness 测试展示完整读法

## 4. 兼容性

- `origin` 参数可选，所有现有调用方（UI/网关/CLI/旧脚本）零改动即缺省 `user`，行为不变；
- custom 条目仅非 user 时落盘，JSONL 增量可忽略；
- HTML 导出：custom 目录已展示 `input_origin` 类型（运行时 Extension Custom 统一展示机制覆盖）。
