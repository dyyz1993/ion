# Hooks 系统 CLI 测试指南

> **状态：已实现** — Group A（create_worker）✅ + Group B（配置/热重载）✅ + Group D（agent handler）✅ 均已验证。
> Group C/E/F（ion hooks test/watch/trace 内置 CLI）待实现（目前用 `scripts/hooks_test.sh` 替代）。
>
> 本文档是**纯 CLI 验证用例**（给 QA/写验证脚本的人看），含完整命令 + 请求/响应 JSON + 验证点。
>
> - 想看"是什么/怎么用" → [HOOKS_GUIDE.md](../design/HOOKS_GUIDE.md)
> - 想看实现规格 → [HOOKS_AND_OUTLINE_SYNC.md](../design/HOOKS_AND_OUTLINE_SYNC.md)

---

## RPC 接口规格

### hooks_list

**请求：**
```bash
ion rpc --session <sid> --method hooks_list
```

**请求参数：** 无

**响应 JSON（成功）：**
```json
{
  "type": "response",
  "id": "1",
  "command": "hooks_list",
  "success": true,
  "data": {
    "global": {
      "path": "~/.ion/hooks.json",
      "event_count": 1,
      "events": ["UserPromptSubmit"]
    },
    "project": {
      "path": "<project>/.ion/hooks.json",
      "event_count": 2,
      "events": ["UserPromptSubmit", "SubagentStop"]
    },
    "disabled": false
  }
}
```

**响应 JSON（失败）：**
```json
{"type":"response","id":"1","command":"hooks_list","success":false,"error":"hooks not loaded"}
```

---

### hooks_show

**请求：**
```bash
ion rpc --session <sid> --method hooks_show \
  --params '{"event":"SubagentStop"}'
```

**请求参数：**

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `event` | string | 是 | 事件名 |

**响应 JSON（成功）：**
```json
{
  "type": "response",
  "id": "1",
  "command": "hooks_show",
  "success": true,
  "data": {
    "event": "SubagentStop",
    "handlers": [
      {
        "type": "agent",
        "prompt": "检查 docs/ 下 MD 同步状态...",
        "model": "fast",
        "max_turns": 100,
        "allowed_tools": ["read","write","edit","bash"]
      }
    ],
    "last_executed": "2026-07-13T10:23:15Z"
  }
}
```

---

### hooks_test（模拟触发，核心调试命令）

**请求：**
```bash
ion rpc --session <sid> --method hooks_test \
  --params '{"event":"UserPromptSubmit","stdin":{"prompt":"测试输入"}}'
```

**请求参数：**

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `event` | string | 是 | 要模拟的事件名 |
| `stdin` | object | 否 | 模拟的 stdin JSON（不填用默认模板） |
| `handler_index` | number | 否 | 只跑第 n 个 handler（调试单个） |

**响应 JSON（成功）：**
```json
{
  "type": "response",
  "id": "1",
  "command": "hooks_test",
  "success": true,
  "data": {
    "event": "UserPromptSubmit",
    "handlers_run": 1,
    "outcome": "continue",
    "additional_context": "=== 项目文档大纲 ===...",
    "exit_code": 0,
    "duration_ms": 120
  }
}
```

**响应 JSON（被 block）：**
```json
{
  "type": "response",
  "id": "1",
  "command": "hooks_test",
  "success": true,
  "data": {
    "event": "UserPromptSubmit",
    "outcome": "block",
    "block_reason": "输入包含禁止关键词",
    "exit_code": 2
  }
}
```

---

### hooks_trace（链路追踪）

**请求：**
```bash
ion rpc --session <sid> --method hooks_trace \
  --params '{"event":"SubagentStop","exec_id":"last"}'
```

**请求参数：**

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `event` | string | 是 | 事件名 |
| `exec_id` | string | 否 | 指定某次执行 ID，`"last"` 取最近一次 |

**响应 JSON（成功）：**
```json
{
  "type": "response",
  "id": "1",
  "command": "hooks_trace",
  "success": true,
  "data": {
    "exec_id": "exec_abc123",
    "event": "SubagentStop",
    "triggered_at": "2026-07-13T10:23:15Z",
    "duration_ms": 2300,
    "upstream": {"last_message": "我改完了"},
    "handlers": [
      {
        "index": 0,
        "type": "agent",
        "child_worker_id": "wkr_yy",
        "child_turns": 8,
        "exit_code": 0,
        "output": "更新了 2 个文件"
      }
    ],
    "downstream": {
      "block": false,
      "data_changed": [".ion/outline.json"],
      "event_emitted": "outline_synced"
    }
  }
}
```

---

### hooks_stats（聚合统计）

**请求：**
```bash
ion rpc --session <sid> --method hooks_stats \
  --params '{"since":"1h"}'
```

**请求参数：**

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `since` | string | 否 | 时间窗口（如 `"1h"`/`"24h"`），默认 1h |

**响应 JSON（成功）：**
```json
{
  "type": "response",
  "id": "1",
  "command": "hooks_stats",
  "success": true,
  "data": {
    "window": "1h",
    "total_triggers": 397,
    "total_blocks": 12,
    "block_rate": 0.03,
    "by_event": [
      {"event": "UserPromptSubmit", "count": 42, "avg_ms": 100, "blocks": 0},
      {"event": "PreToolUse", "count": 156, "avg_ms": 80, "blocks": 2},
      {"event": "SubagentStop", "count": 8, "avg_ms": 2100, "blocks": 0}
    ]
  }
}
```

---

### hooks_set_enabled（开关）

**请求：**
```bash
ion rpc --session <sid> --method hooks_set_enabled \
  --params '{"enabled": false}'
```

**请求参数：**

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `enabled` | bool | 是 | true 启用 / false 禁用 |
| `scope` | string | 否 | `"session"`（默认，当前会话）/ `"global"`（持久化） |

**响应 JSON（成功）：**
```json
{"type":"response","id":"1","command":"hooks_set_enabled","success":true,"data":{"enabled":false}}
```

---


> **outline_status / outline_diff / outline_sync 不是内核 RPC**
> ——大纲同步是用户用 hooks 搭的用例，这些命令是示例脚本，见 [HOOKS_GUIDE.md §7 附录](../design/HOOKS_GUIDE.md)。

---

## Group A：补丁 1 create_worker 增强（✅ 已实现可验证）

> 验证 `ExtensionWorkerConfig` 新字段（agent/allowed_tools/max_turns）能正确透传到子 Worker。

### A1 数据结构字段序列化

```bash
cargo test --test patch1_worker_config t01 -- --nocapture
```

**预期：** `1 passed`

**验证点：**
- ✅ `agent`/`initial_prompt`/`allowed_tools`/`disallowed_tools`/`max_turns` 能序列化
- ✅ JSON 里字段值正确

### A2 ExtensionWorkerConfig → WorkerCreateConfig 透传

```bash
cargo test --test patch1_worker_config t02 -- --nocapture
```

**预期：** `1 passed`

**验证点：**
- ✅ 模拟 create_worker 的 json!({...}) 透传链路
- ✅ Manager 端 `serde_json::from_value` 反序列化后字段都在

### A3 默认值向后兼容

```bash
cargo test --test patch1_worker_config t03 -- --nocapture
```

**验证点：**
- ✅ 不带新字段的老配置能正常反序列化
- ✅ 新字段默认 None

### A4 既有 create_worker 测试未破坏

```bash
cargo test --test manager_integration -- --nocapture
```

**预期：** `25 passed`

**验证点：**
- ✅ 既有 WorkerCreateConfig 初始化（加了 `..Default::default()`）全部通过
- ✅ 无 E0063 编译错误

---

## Group B：配置校验与热重载（✅ 已验证 — hooks_ci.sh Group A）

> 验证 hooks.json 校验 + 改完即生效。

### B1 校验 hooks.json

```bash
# 准备：写一个格式正确的 .ion/hooks.json
cat > .ion/hooks.json <<'EOF'
{
  "version": 1,
  "hooks": {
    "UserPromptSubmit": [{"type":"command","command":"echo hi","timeout":3}]
  }
}
EOF

ion hooks validate .ion/hooks.json
```

**预期：**
```
Hooks valid: .ion/hooks.json (1 event, 1 handler)
  UserPromptSubmit: 1 handler (command)
```

**验证点：**
- ✅ 格式正确时输出 event/handler 统计
- ✅ 格式错误时精确报错到字段路径

### B2 热重载——改完即生效

```bash
# 初始配置只有 UserPromptSubmit
ion hooks list
# → 显示 1 事件

# 追加 SubagentStop handler
cat > .ion/hooks.json <<'EOF'
{
  "version": 1,
  "hooks": {
    "UserPromptSubmit": [{"type":"command","command":"echo hi"}],
    "SubagentStop": [{"type":"command","command":"echo done"}]
  }
}
EOF

# 不重启，直接 list
ion hooks list
# → 立即显示 2 事件
```

**验证点：**
- ✅ 改完存盘后 `list` 立即反映新配置
- ✅ 不用跑 `ion hooks reload`
- ✅ 下次事件触发用新配置

### B3 禁用/启用

```bash
ion hooks disable
ion hooks test UserPromptSubmit --stdin '{"prompt":"x"}'
# → 应显示 "hooks disabled, skipping"

ion hooks enable
ion hooks test UserPromptSubmit --stdin '{"prompt":"x"}'
# → 应正常执行 handler
```

**验证点：**
- ✅ disable 后所有 handler 跳过
- ✅ enable 后恢复

### B4 disableAllHooks 紧急逃生

```bash
cat > ~/.ion/hooks.json <<'EOF'
{"disableAllHooks": true}
EOF

ion hooks list
# → 显示 disabled: true

ion hooks test UserPromptSubmit --stdin '{}'
# → 跳过
```

**验证点：**
- ✅ disableAllHooks=true 时全局禁用
- ✅ 删除该字段或设 false 后恢复

---

## Group C：handler 调试（test / dry-run）

> 验证 `ion hooks test` 模拟触发 + `dry-run` 只看过滤。

### C1 command handler 模拟触发

```bash
# 准备 inject_outline.sh
cat > .ion/scripts/inject_outline.sh <<'EOF'
#!/bin/bash
echo "=== 项目文档大纲 ==="
echo "## 设计文档"
EOF
chmod +x .ion/scripts/inject_outline.sh

ion hooks test UserPromptSubmit --stdin '{"prompt":"改文档"}'
```

**预期输出：**
```
─── 模拟触发: UserPromptSubmit ───
stdin: {"prompt":"改文档",...}

[filter] matcher=* → 1 handler 命中
  └─ handler #1 (command)

[执行] bash .ion/scripts/inject_outline.sh
  ├─ exit code: 0
  └─ stdout:
     === 项目文档大纲 ===
     ## 设计文档

[结果]
  outcome: Continue
  additionalContext: === 项目文档大纲 ===...
```

**验证点：**
- ✅ 显示 stdin → handler → stdout 完整链路
- ✅ stdout 作为 additionalContext

### C2 command exit 2 阻断

```bash
# 准备阻断脚本
cat > .ion/scripts/block.sh <<'EOF'
#!/bin/bash
echo '{"decision":"block","reason":"禁止的操作"}' 
exit 2
EOF

ion hooks test UserPromptSubmit --stdin '{"prompt":"x"}'
```

**验证点：**
- ✅ exit 2 → outcome: block
- ✅ reason 取自 stdout JSON 的 decision.reason

### C3 dry-run 只看过滤

```bash
# 配置 PreToolUse matcher="bash|write|edit"
ion hooks dry-run PreToolUse --stdin '{"tool_name":"read"}'
```

**预期：**
```
[filter] matcher="bash|write|edit" → tool_name="read" 不匹配
  └─ handler: 跳过 ✗
将执行 0 个 handler（dry-run 不真跑）
```

**验证点：**
- ✅ 显示哪些 handler 会跑/会跳过
- ✅ 不真执行

### C4 handler 单独调试

```bash
ion hooks test SubagentStop --stdin '{}' --handler 1
```

**验证点：**
- ✅ 只跑指定 handler（--handler 参数）

---

## Group D：agent handler（真调工具）（✅ 已验证 — hooks_agent_ci.sh Group E）

> 验证 agent handler 真的 spawn 带工具的子 Worker，能改文件。**这是 ION 比 pi 强的核心**。

### D1 SubagentStop 触发大纲同步

```bash
# 配置：SubagentStop → agent handler
cat > .ion/hooks.json <<'EOF'
{
  "version": 1,
  "hooks": {
    "SubagentStop": [{
      "type": "agent",
      "prompt": "读 docs/test.md，把内容写入 .ion/outline.json",
      "model": "faux",
      "max_turns": 10,
      "allowed_tools": ["read","write"]
    }]
  }
}
EOF

# 准备测试文件
echo "# Test Doc" > docs/test.md
rm -f .ion/outline.json

ion hooks test SubagentStop --stdin '{"last_message":"done"}'
```

**验证点：**
- ✅ 子 Worker 真有工具（能 read/write）
- ✅ outline.json 被创建/更新
- ✅ 与 pi 不同：ION 的 agent handler 真能改文件
- ✅ allowed_tools 生效（子 Worker 只有 read/write，没有 bash）

### D2 max_turns 限制

```bash
# 配置 max_turns=2，给一个需要 5 步的任务
ion hooks test SubagentStop --stdin '{}' 
# → 子 Worker 跑 2 步后停止
```

**验证点：**
- ✅ 子 Worker 到 max_turns 停止
- ✅ 未完成的任务不影响主流程（不阻断）

---

## Group E：实时观察（watch / trace）

> 验证实时事件流和链路追踪。

### E1 watch 实时流

```bash
# 终端 1：开 watch
ion hooks watch

# 终端 2：触发一次 prompt
ion rpc --session <sid> --method prompt --params '{"text":"hello"}'
```

**预期（终端 1 实时打印）：**
```
[10:23:15] UserPromptSubmit  sess_xx  handler#1(command)  → exit 0  0.1s
[10:23:16] PreToolUse  sess_xx  read  handler#1(command)  → exit 0  0.05s
[10:23:18] PostToolUse  sess_xx  read  handler#1(command)  → exit 0  0.05s
[10:23:30] Stop  sess_xx  handler#1(command)  → exit 0  0.1s
```

**验证点：**
- ✅ 每次触发实时打印
- ✅ 显示 event/session/handler/exit/耗时

### E2 trace 链路追踪

```bash
ion hooks trace SubagentStop --last
```

**预期输出（完整管道）：**
```
─── Trace: SubagentStop (exec_abc123, 2.3s ago) ───
触发: 2026-07-13 10:23:15  耗时: 2.3s

[上游] 事件源
  stdin: {session_id: "sess_xx", last_message: "我改完了"}

[中游] hook 引擎
  ├─ 读 hooks.json: <project>/.ion/hooks.json
  ├─ matcher: * → 1 handler 命中
  └─ handler #1 (agent)

[中游] handler 执行
  agent handler:
    ├─ create_worker(agent: default, allowed_tools: [read,write,edit], max_turns: 100)
    ├─ 子 Worker wkr_yy 跑了 8 turns:
    │    turn 1: read docs/design/NEW.md
    │    turn 2: read .ion/outline.json
    │    turn 3-6: write .ion/outline.json
    ├─ 子 Worker 退出: "更新了 2 个文件"
    └─ exit 0

[下游] 结果影响
  ├─ block: 否
  ├─ 业务数据变更: .ion/outline.json (+2 entries)
  └─ emit event: outline_synced {files: [NEW.md, OLD.md]}
```

**验证点：**
- ✅ 展开完整管道：上游 stdin → 过滤 → handler → 下游影响
- ✅ agent handler 显示子 Worker 各 turn
- ✅ 显示业务数据变更

---

## Group F：聚合统计（stats / log）

### F1 stats 聚合

```bash
# 跑一批操作后
ion hooks stats --since 1h
```

**验证点：**
- ✅ 各事件触发次数/平均耗时/block 率
- ✅ 识别最慢 handler

### F2 log 排查失败

```bash
ion hooks log --tail 20 --failed
```

**验证点：**
- ✅ 只看失败的（exit ≠ 0）
- ✅ 显示 stderr 摘要

---


> **Group G/H（outline 业务数据 + host 后台同步）不属于 hooks 内核测试**。
> 大纲同步的端到端验证是用户的 hooks 配置 + 脚本的事，见 [HOOKS_GUIDE.md §7 附录](../design/HOOKS_GUIDE.md) 的完整示例。

---
## 测试脚本登记

> Group A-D 已有自动化脚本覆盖；Group E/F（ion hooks watch/trace/stats 内置 CLI）待实现。

| 脚本 | 覆盖 Group | 状态 |
|------|-----------|------|
| `tests/patch1_worker_config.rs` | A（create_worker 增强） | ✅ 已实现（5 passed） |
| `tests/hooks_ci.sh` | B（配置/热重载）+ B.1/B.2/B.3（command handler 教程） | ✅ 已实现（8 passed） |
| `tests/hooks_agent_ci.sh` | D（agent handler 真能 spawn + 死循环防护） | ✅ 已实现（4 passed） |
| `tests/hooks_e2e.rs` | 内核引擎（HooksConfig/handler_runner/matcher） | ✅ 已实现（10 passed，`--test-threads=1`） |
| `scripts/hooks_test.sh` | 用户验证工具（validate/test/list，纯 bash） | ✅ 已实现 |

**待实现**：Group E/F（`ion hooks watch/trace/stats` 内置 CLI 命令，目前用 `scripts/hooks_test.sh` 替代）

登记到 [AGENTS.md 测试统计表](../../AGENTS.md)。
