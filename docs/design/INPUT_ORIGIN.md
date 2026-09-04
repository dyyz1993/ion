# INPUT_ORIGIN — 输入来源标识（user / monitor / system）

> **状态：开发中** — origin 贯穿（入口→钩子→落盘）本期实现；per-turn 工具过滤等消费侧为后续。

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

### 2.4 消费侧（本期不实现，接口已就绪）

- **扩展改写提示词**：`on_input` 里 `if ctx.origin == "monitor" { ctx.text = ... }`
- **hook 拒绝工具**：PreToolUse handler 读 turn origin（需把 origin 暴露给 hook 引擎——后续小改）
- **隐藏工具**：agent loop per-turn 工具过滤（后续迭代）

## 3. CLI 验证（Group A）

### A1 带 origin 的 prompt 落 custom 条目

```bash
ion rpc --session <sid> --method prompt \
  --params '{"text":"hello","origin":"monitor"}'
# 然后查会话 JSONL 尾部：
tail -5 ~/.ion/agent/sessions/<cwd_hash>/<sid>.jsonl | grep input_origin
```

**验证点**：
- ✅ 存在 `{"type":"custom","customType":"input_origin","data":{"origin":"monitor",...}}`
- ✅ agent 正常应答（origin 不影响主流程）

### A2 缺省 origin 不落盘

```bash
ion rpc --session <sid> --method prompt --params '{"text":"hi"}'
tail -5 <jsonl> | grep input_origin   # 应无新增
```

### A3 异步通知带 system

触发一次子 worker 完成通知后，父会话 JSONL 出现 `input_origin` 且 `origin=system`。

### 自动化

`tests/origin_ci.sh`：起 host → create_session → A1/A2 → 断言 → 清理。harness 测试（FauxProvider + 测试扩展断言 `InputContext.origin`）见 `cargo test --lib input_origin`。

## 4. 兼容性

- `origin` 参数可选，所有现有调用方（UI/网关/CLI/旧脚本）零改动即缺省 `user`，行为不变；
- custom 条目仅非 user 时落盘，JSONL 增量可忽略；
- HTML 导出：custom 目录已展示 `input_origin` 类型（运行时 Extension Custom 统一展示机制覆盖）。
