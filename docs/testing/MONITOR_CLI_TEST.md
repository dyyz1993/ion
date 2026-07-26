# Monitor Extension CLI 测试指南

> **状态：🔧 设计稿** — case 已设计完成，待 v2 代码实现后跑通。
>
> 本文档是**纯 CLI 验证用例**（给 QA/写 CI 脚本的人看），含完整命令 + 请求/响应 JSON + 验证点。
>
> - 想看"是什么/怎么用" → [MONITOR_EXTENSION.md](../design/MONITOR_EXTENSION.md)
> - 想看实现规格 → [MONITOR_EXTENSION.md §2](../design/MONITOR_EXTENSION.md#2-主流程--数据结构)

---

## 测试组设计方法论

### 第一步：核心链路分析

用户拿 Monitor 干什么？

1. **配置能加载、能触发**（最核心）—— v1 已验证
2. **不该触发的不触发**（正确性）—— v1 已验证
3. **并发策略对**（上一个没完怎么办）—— v2 核心
4. **消费方接入对**（spawn / channel / event）—— v2 核心
5. **Agent 写出来的配置合法**（配置正确性）—— v2 核心
6. **事件可观测**（subscribe 看得到）—— v2 核心
7. **真实业务场景跑通**（GitHub issue / 日志 / 进程）—— v2 验证
8. **边界安全**（路径穿越 / 越界 / 注入）—— v2 必查

### 第二步：Group 设计（每条链路一个 Group）

| Group | 链路 | case 数 | 状态 |
|-------|------|--------|------|
| A | 配置能加载+触发（能用） | 5 | ✅ v1 已实现 3/5 |
| B | RPC 管理（能用） | 6 | ✅ v1 已实现 3/6 |
| C | 空输出+错误处理（不会出问题） | 4 | ✅ v1 已实现 3/4 |
| D | 多 monitor 并行（能用） | 3 | ✅ v1 已实现 2/3 |
| **E** | **并发策略**（serial_skip/queue/concurrent） | 9 | 🔧 v2 待实现 |
| **F** | **消费方接入**（auto_spawn/channel/event） | 6 | 🔧 v2 待实现 |
| **G** | **Scheduler Agent** 生成+校验 | 5 | 🔧 v2 待实现 |
| **H** | **事件订阅** | 5 | 🔧 v2 待实现 |
| **I** | **真实业务场景** | 4 | 🔧 v2 待实现 |
| **J** | **边界+安全** | 6 | 🔧 v2 待实现 |
| **合计** | | **53** | v1 已 11/53 |

### 第三步：测试数据原则

- ✅ 用真实场景数据：`gh issue list` / `grep ERROR /var/log` / `pgrep -f critical-service`
- ❌ 不用 `echo trigger` / `echo test`（v1 CI 里踩过）
- 每个 case 的 `script` 都模拟真实业务

---

## RPC 接口规格

### `extension_rpc monitor list`

**请求**：

```bash
ion rpc --method extension_rpc \
  --params '{"extension":"monitor","method":"list"}'
```

**请求参数**：无

**响应 JSON（成功）**：

```json
{
  "type": "response",
  "id": "rpc-client",
  "success": true,
  "data": {
    "monitors": [
      {
        "name": "github-issues",
        "interval_secs": 300,
        "agent": "developer",
        "enabled": true,
        "mode": "serial_skip",
        "trigger_mode": "auto_spawn",
        "max_concurrent": 3,
        "trigger_count": 12,
        "last_run": "2026-07-26T10:30:00Z",
        "last_result": "triggered",
        "active_workers": 0,
        "queue_length": 0
      }
    ]
  }
}
```

**响应字段**：

| 字段 | 类型 | 说明 |
|------|------|------|
| `name` | string | monitor 名 |
| `mode` | string | serial_skip / serial_queue / concurrent |
| `trigger_mode` | string | auto_spawn / channel_notify / event_only |
| `trigger_count` | u64 | 总触发次数 |
| `active_workers` | u32 | 当前活跃 worker 数（concurrent 模式用） |
| `queue_length` | u32 | 当前队列长度（serial_queue 模式用） |
| `last_result` | string | triggered / skipped / queued / throttled / cooldown / failed |

**响应 JSON（失败）**：

```json
{"success": false, "error": "monitor extension not registered (only in serve mode)"}
```

---

### `extension_rpc monitor add`

**请求**：

```bash
ion rpc --method extension_rpc \
  --params '{
    "extension": "monitor",
    "method": "add",
    "params": {
      "name": "github-issues",
      "interval_secs": 300,
      "script": "gh issue list --repo dyyz1993/ion --state open --label bug 2>/dev/null | jq '. | length | tostring'",
      "agent": "developer",
      "prompt_template": "GitHub bug issue 数量：{output}",
      "mode": "serial_skip",
      "trigger_mode": "auto_spawn"
    }
  }'
```

**请求参数**：

| 字段 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| `name` | string | ✅ | — | 唯一标识，正则 `^[a-zA-Z0-9_-]{1,32}$` |
| `interval_secs` | u64 | ✅ | — | 1-86400 |
| `script` | string | ✅ | — | bash 脚本，exit=0 + stdout 非空 = 触发 |
| `agent` | string | ❌ | "developer" | 已注册 agent |
| `prompt_template` | string | ❌ | "Monitor triggered:\n{output}" | 必须含 `{output}` |
| `mode` | string | ❌ | "serial_skip" | serial_skip / serial_queue / concurrent |
| `trigger_mode` | string | ❌ | "auto_spawn" | auto_spawn / channel_notify / event_only |
| `max_concurrent` | u32 | ❌ | 3 | concurrent 上限 |
| `cooldown_secs` | u64 | ❌ | 60 | 冷却秒数 |

**响应 JSON（成功）**：

```json
{
  "success": true,
  "data": {
    "added": "github-issues",
    "validated": true,
    "file": ".ion/monitors/github-issues.json"
  }
}
```

**响应 JSON（校验失败，v2 新增）**：

```json
{
  "success": false,
  "error": "monitor validation failed",
  "data": {
    "errors": [
      "interval_secs=0 不合法，必须 1-86400",
      "script 语法错误：bash -n 报错",
      "agent 'foobar' 不存在"
    ]
  }
}
```

---

### `extension_rpc monitor validate`（v2 新增）

**请求**：

```bash
ion rpc --method extension_rpc \
  --params '{
    "extension": "monitor",
    "method": "validate",
    "params": { ... 同 add 的 def ... }
  }'
```

**响应 JSON（通过）**：

```json
{
  "success": true,
  "data": {
    "valid": true,
    "warnings": [
      "interval_secs=86400 可能太长，建议 300-3600"
    ]
  }
}
```

**响应 JSON（不通过）**：

```json
{
  "success": true,
  "data": {
    "valid": false,
    "errors": [
      "name 含非法字符：'github issues'（只允许 [a-zA-Z0-9_-]）",
      "prompt_template 缺少 {output} 占位符"
    ]
  }
}
```

---

### `extension_rpc monitor test`（v2 新增 dry-run）

**请求**：

```bash
ion rpc --method extension_rpc \
  --params '{
    "extension": "monitor",
    "method": "test",
    "params": {
      "script": "echo hello",
      "prompt_template": "Output: {output}"
    }
  }'
```

**响应 JSON（成功）**：

```json
{
  "success": true,
  "data": {
    "valid": true,
    "script_exit_ok": true,
    "script_stdout": "hello",
    "script_stderr": "",
    "script_duration_ms": 12,
    "would_trigger": true,
    "rendered_prompt": "Output: hello"
  }
}
```

**响应 JSON（脚本失败）**：

```json
{
  "success": true,
  "data": {
    "valid": true,
    "script_exit_ok": false,
    "script_exit_code": 127,
    "script_stderr": "gh: command not found",
    "would_trigger": false
  }
}
```

---

### `extension_rpc monitor status`

**请求**：

```bash
ion rpc --method extension_rpc \
  --params '{"extension":"monitor","method":"status"}'
```

**响应 JSON**：

```json
{
  "success": true,
  "data": {
    "statuses": [
      {
        "name": "github-issues",
        "trigger_count": 12,
        "skip_count": 3,
        "queue_length": 0,
        "active_workers": 0,
        "last_run": "2026-07-26T10:30:00Z",
        "last_result": "triggered",
        "last_error": null
      }
    ]
  }
}
```

---

### 其他 RPC（v1 已实现）

| 方法 | 请求参数 | 行为 |
|------|---------|------|
| `remove` | `{"name": "X"}` | 删除 monitor + 删 .json |
| `enable` | `{"name": "X"}` | 激活 |
| `disable` | `{"name": "X"}` | 停用（保留配置） |

---

## Group A：配置能加载+触发（基础）

> 验证 monitor 能从 .ion/monitors/ 加载、脚本能跑、非空输出能触发 worker。v1 已验证 A1-A3。

### A1 monitor 配置加载

```bash
# 准备
mkdir -p .ion/monitors
cat > .ion/monitors/a1.json <<'EOF'
{"name":"a1-load","interval_secs":3,"script":"echo a1-loaded","agent":"build","prompt_template":"A1: {output}","enabled":true}
EOF

# 启动 serve
ion serve &
SERVE_PID=$!
sleep 8

# 验证：日志里能看到 loaded
grep "loaded.*a1-load" /tmp/serve.log
```

**验证点：**
- ✅ 日志含 `[monitor] loaded: a1-load`
- ✅ `.ion/monitors/a1.json` 被识别为 monitor 定义
- ✅ serve 启动 5s 内完成加载（不阻塞）

### A2 脚本触发 worker

```bash
# A1 的 monitor interval=3s，等 5s 必触发
sleep 5

# 查 list_workers
ion rpc --method list_workers --params '{}'
```

**预期响应（部分）：**

```json
{"data":{"workers":[{"agent":"build","status":"Idle","workerId":"wkr_xxx"}]}}
```

**验证点：**
- ✅ worker 数 >= 1
- ✅ 有一个 worker 的 agent == "build"（A1 配置的）
- ✅ 该 worker 有 sessionId（被 spawn 了）

### A3 trigger 日志

```bash
grep "triggered" /tmp/serve.log
```

**验证点：**
- ✅ 日志含 `[monitor] 'a1-load' triggered!`
- ✅ 日志含 `output=9 bytes`（"a1-loaded" 是 9 字节）
- ✅ 日志含 `triggering agent=build`

### A4 全局 monitor 加载（~/.ion/monitors/）

```bash
# 准备全局 monitor
mkdir -p ~/.ion/monitors
cat > ~/.ion/monitors/a4-global.json <<'EOF'
{"name":"a4-global","interval_secs":60,"script":"echo global","agent":"build","prompt_template":"A4: {output}","enabled":true}
EOF

# 重启 serve
kill $SERVE_PID; sleep 2; ion serve &
sleep 8

# 验证：全局 monitor 也加载了
grep "loaded.*a4-global" /tmp/serve.log
```

**验证点：**
- ✅ 项目级 + 全局级 monitor 都加载
- ✅ 日志显示来源路径（`from .ion/monitors/` vs `from ~/.ion/monitors/`）
- ✅ 同名时项目级优先（不重复加载）

### A5 disabled 字段生效

```bash
cat > .ion/monitors/a5-disabled.json <<'EOF'
{"name":"a5-disabled","interval_secs":3,"script":"echo should-not-trigger","agent":"build","prompt_template":"A5","enabled":false}
EOF

kill $SERVE_PID; sleep 2; ion serve &
sleep 10

# 验证：disabled 的 monitor 加载了但不触发
grep "a5-disabled" /tmp/serve.log
```

**验证点：**
- ✅ 日志含 `[monitor] loaded: a5-disabled (disabled)`
- ✅ 日志**不含** `a5-disabled triggered`
- ✅ `list` RPC 显示 `"enabled": false`

---

## Group B：RPC 管理（list/add/remove/enable/disable）

> 验证通过 RPC 动态管理 monitor。v1 已验证 B1-B3。

### B1 create_session

```bash
ion rpc --method create_session --params '{"agent":"build"}'
# → {"data":{"session_id":"sess_xxx"}}
```

### B2 extension_rpc list（host-level singleton 不可达，预期失败）

```bash
ion rpc --session sess_xxx --method extension_rpc \
  --params '{"extension":"monitor","method":"list"}'
```

**预期响应：**

```json
{"success": false, "error": "extension 'monitor' not found in worker"}
```

**验证点：**
- ✅ Worker RPC 拒绝（monitor 是 host-level singleton，不在 worker 里）
- ✅ Manager-level RPC 可达：`ion rpc --method extension_rpc ...`（不带 session）

### B3 starting 日志

```bash
grep "starting.*a1-load" /tmp/serve.log
```

**验证点：**
- ✅ 日志含 `[monitor] starting 'a1-load' (interval=3s, agent=build)`

### B4 add 新 monitor（v2：强制 validate）

```bash
ion rpc --method extension_rpc \
  --params '{
    "extension": "monitor",
    "method": "add",
    "params": {
      "name": "b4-new",
      "interval_secs": 60,
      "script": "echo b4",
      "agent": "build",
      "prompt_template": "B4: {output}"
    }
  }'
```

**预期响应：**

```json
{"success": true, "data": {"added": "b4-new", "validated": true, "file": ".ion/monitors/b4-new.json"}}
```

**验证点：**
- ✅ 返回 `validated: true`
- ✅ `.ion/monitors/b4-new.json` 文件存在
- ✅ `list` RPC 能看到 b4-new
- ✅ 日志含 `[monitor] added: b4-new`

### B5 remove monitor

```bash
ion rpc --method extension_rpc \
  --params '{"extension":"monitor","method":"remove","params":{"name":"b4-new"}}'
```

**预期响应：**

```json
{"success": true, "data": {"removed": true, "name": "b4-new"}}
```

**验证点：**
- ✅ `.ion/monitors/b4-new.json` 文件被删
- ✅ `list` RPC 不再有 b4-new
- ✅ remove 不存在的 monitor：`{"removed": false}` 不报错

### B6 enable/disable

```bash
# disable
ion rpc --method extension_rpc \
  --params '{"extension":"monitor","method":"disable","params":{"name":"a1-load"}}'

# 验证
ion rpc --method extension_rpc \
  --params '{"extension":"monitor","method":"list"}' | jq '.data.monitors[] | select(.name=="a1-load") | .enabled'
# → false

# enable
ion rpc --method extension_rpc \
  --params '{"extension":"monitor","method":"enable","params":{"name":"a1-load"}}'
```

**验证点：**
- ✅ disable 后该 monitor 不再触发（sleep 10s 看日志）
- ✅ enable 后恢复触发
- ✅ disable 不存在的 monitor：返回 error

---

## Group C：空输出+错误处理（不会出问题）

> 验证不该触发的不触发、错误不崩溃。v1 已验证 C1-C3。

### C1 空输出不触发

```bash
cat > .ion/monitors/c1.json <<'EOF'
{"name":"c1-idle","interval_secs":3,"script":"true","agent":"build","prompt_template":"C1: {output}","enabled":true}
EOF

kill $SERVE_PID; sleep 2; ion serve &
sleep 10

# 验证：日志不含 trigger
grep -c "c1-idle triggered" /tmp/serve.log  # → 0
```

**验证点：**
- ✅ 日志不含 `c1-idle triggered`
- ✅ `status` 显示 `last_result: "no_trigger"`（v2 新增）

### C2 错误脚本不崩溃

```bash
cat > .ion/monitors/c2.json <<'EOF'
{"name":"c2-error","interval_secs":3,"script":"exit 1","agent":"build","prompt_template":"C2: {output}","enabled":true}
EOF

kill $SERVE_PID; sleep 2; ion serve &
sleep 10

# 验证：serve 没崩
ion rpc --method health --params '{}'
# → {"data":{"status":"ok"}}
```

**验证点：**
- ✅ serve 存活（health 返回 ok）
- ✅ 日志含 `c2-error script failed`（v2 新增结构化日志）
- ✅ `status` 显示 `last_error: "exit code 1"`

### C3 多次错误后自动 disable（v2 新增）

```bash
# 连续失败 5 次后自动 disable
cat > .ion/monitors/c3.json <<'EOF'
{"name":"c3-auto-disable","interval_secs":2,"script":"exit 1","agent":"build","prompt_template":"C3: {output}","enabled":true}
EOF

sleep 15  # 等 7-8 次失败

ion rpc --method extension_rpc \
  --params '{"extension":"monitor","method":"list"}' \
  | jq '.data.monitors[] | select(.name=="c3-auto-disable") | .enabled'
# → false（v2 应自动 disable）
```

**验证点：**
- ✅ 连续失败 5 次后 `enabled` 变 false
- ✅ 日志含 `c3-auto-disable auto-disabled after 5 consecutive failures`
- ✅ `status` 显示 `last_error` 含失败次数

### C4 脚本超时不挂死（v2 新增）

```bash
cat > .ion/monitors/c4.json <<'EOF'
{"name":"c4-hang","interval_secs":3,"script":"sleep 9999","agent":"build","prompt_template":"C4: {output}","enabled":true}
EOF

sleep 35  # 等超时

# 验证：serve 还活着，c4 被标记为超时
ion rpc --method health --params '{}'
```

**验证点：**
- ✅ 脚本 30s 超时被 kill（v2 新增 timeout）
- ✅ serve 存活
- ✅ 日志含 `c4-hang script timeout after 30s`

---

## Group D：多 monitor 并行

> 验证多个 monitor 同时跑不互相干扰。v1 已验证 D1。

### D1 两个 monitor 同时加载+触发

```bash
echo '{"name":"d1a","interval_secs":3,"script":"echo first","agent":"build","prompt_template":"D1a: {output}","enabled":true}' > .ion/monitors/d1a.json
echo '{"name":"d1b","interval_secs":3,"script":"echo second","agent":"build","prompt_template":"D1b: {output}","enabled":true}' > .ion/monitors/d1b.json

kill $SERVE_PID; sleep 2; ion serve &
sleep 10

# 验证：两个都触发
awk '/loaded:/{c++} END{print c+0}' /tmp/serve.log  # → >= 2
awk '/triggered/{c++} END{print c+0}' /tmp/serve.log  # → >= 2
```

### D2 不同 agent 各自触发

```bash
echo '{"name":"d2-build","interval_secs":3,"script":"echo a","agent":"build","prompt_template":"{output}","enabled":true}' > .ion/monitors/d2a.json
echo '{"name":"d2-explore","interval_secs":3,"script":"echo b","agent":"explore","prompt_template":"{output}","enabled":true}' > .ion/monitors/d2b.json

sleep 8

ion rpc --method list_workers --params '{}'
```

**验证点：**
- ✅ 两个 worker 都创建：一个 agent=build，一个 agent=explore
- ✅ 各自的 prompt 不串台

### D3 同名冲突检测（v2 新增）

```bash
# 项目级和全局级同名
echo '{"name":"conflict","interval_secs":60,"script":"echo project","agent":"build","prompt_template":"{output}","enabled":true}' > .ion/monitors/conflict.json
echo '{"name":"conflict","interval_secs":60,"script":"echo global","agent":"build","prompt_template":"{output}","enabled":true}' > ~/.ion/monitors/conflict.json

kill $SERVE_PID; sleep 2; ion serve &
sleep 5

# 验证：项目级优先，日志有 warning
grep "conflict.*duplicate" /tmp/serve.log
```

**验证点：**
- ✅ 只加载一个（项目级优先）
- ✅ 日志含 `[monitor] duplicate name 'conflict', project-level wins`

---

## Group E：并发策略（v2 核心）

> 验证 mode 字段的三种行为。这是 v2 最重要的功能。

### E1 serial_skip：busy 时跳过

```bash
# 准备：script 永远触发 + agent 任务长（让 worker 长时间 busy）
cat > .ion/monitors/e1.json <<'EOF'
{
  "name": "e1-skip",
  "interval_secs": 3,
  "script": "echo tick",
  "agent": "build",
  "prompt_template": "E1: {output}。请 sleep 30 秒。",
  "mode": "serial_skip",
  "trigger_mode": "auto_spawn"
}
EOF

kill $SERVE_PID; sleep 2; ion serve &
sleep 12  # 等 3-4 次触发

# 验证：第一次触发，后续 skip
awk '/e1-skip triggered/{c++} END{print c+0}' /tmp/serve.log  # → 1
awk '/e1-skip skipped/{c++} END{print c+0}' /tmp/serve.log    # → >= 2
```

**验证点：**
- ✅ `triggered` 次数 = 1（只第一次）
- ✅ `skipped` 次数 >= 2
- ✅ `status` 显示 `skip_count >= 2`
- ✅ subscribe 能收到 `monitor_skipped` 事件

### E2 serial_skip：idle 时正常触发

```bash
# 让上一个 worker 完成任务
sleep 35

# 再次触发应该正常
awk '/triggered/{c++} END{print c}' /tmp/serve.log  # → 2
```

**验证点：**
- ✅ worker idle 后，下一次 trigger 正常 spawn

### E3 serial_queue：busy 时排队

```bash
cat > .ion/monitors/e3.json <<'EOF'
{
  "name": "e3-queue",
  "interval_secs": 3,
  "script": "echo queue-test",
  "agent": "build",
  "prompt_template": "E3: {output}。请 sleep 20 秒。",
  "mode": "serial_queue",
  "trigger_mode": "auto_spawn"
}
EOF

kill $SERVE_PID; sleep 2; ion serve &
sleep 15  # 等 4-5 次触发

# 验证：第一次触发，后续入队
awk '/e3-queue triggered/{c++} END{print c+0}' /tmp/serve.log  # → 1
awk '/e3-queue queued/{c++} END{print c+0}' /tmp/serve.log     # → >= 3

# 查队列长度
ion rpc --method extension_rpc \
  --params '{"extension":"monitor","method":"status"}' \
  | jq '.data.statuses[] | select(.name=="e3-queue") | .queue_length'
# → >= 3
```

**验证点：**
- ✅ `triggered` = 1
- ✅ `queued` >= 3
- ✅ `status.queue_length` >= 3
- ✅ subscribe 收到 `monitor_queued` 事件

### E4 serial_queue：worker 空闲后处理队列

```bash
# 等 worker 完成当前任务
sleep 25

# 验证：队列被消费
ion rpc --method extension_rpc \
  --params '{"extension":"monitor","method":"status"}' \
  | jq '.data.statuses[] | select(.name=="e3-queue") | .queue_length'
# → 0（或大幅减少）

awk '/e3-queue dequeued/{c++} END{print c+0}' /tmp/serve.log  # → >= 3
```

**验证点：**
- ✅ `queue_length` 降为 0
- ✅ `dequeued` 事件 >= 3
- ✅ 每个 queued 的 prompt 都被处理（不丢任务）

### E5 serial_queue：队列溢出保护（v2 新增）

```bash
# 故意让队列积压超过 10 条
cat > .ion/monitors/e5.json <<'EOF'
{
  "name": "e5-overflow",
  "interval_secs": 1,
  "script": "echo x",
  "agent": "build",
  "prompt_template": "sleep 300",
  "mode": "serial_queue",
  "trigger_mode": "auto_spawn"
}
EOF

kill $SERVE_PID; sleep 2; ion serve &
sleep 15  # 15 秒应该堆积 14 条

ion rpc ... | jq '.data.statuses[] | select(.name=="e5-overflow") | .queue_length'
# → <= 10（被截断）

grep "e5-overflow.*queue_overflow" /tmp/serve.log
```

**验证点：**
- ✅ `queue_length` <= 10（默认上限）
- ✅ 日志含 `queue_overflow` warning
- ✅ 丢弃的是最旧的（FIFO 截断）

### E6 concurrent：每次新建 worker

```bash
cat > .ion/monitors/e6.json <<'EOF'
{
  "name": "e6-concurrent",
  "interval_secs": 2,
  "script": "echo c",
  "agent": "build",
  "prompt_template": "sleep 10",
  "mode": "concurrent",
  "max_concurrent": 3,
  "trigger_mode": "auto_spawn"
}
EOF

kill $SERVE_PID; sleep 2; ion serve &
sleep 10  # 5 次触发

# 验证：创建了 3 个 worker（达上限）
ion rpc --method list_workers --params '{}' \
  | jq '.data.workers | length'
# → 3

awk '/e6-concurrent spawned/{c++} END{print c+0}' /tmp/serve.log  # → 3
awk '/e6-concurrent throttled/{c++} END{print c+0}' /tmp/serve.log  # → >= 2
```

**验证点：**
- ✅ worker 数 = 3（max_concurrent）
- ✅ `spawned` = 3
- ✅ `throttled` >= 2（超过上限的）
- ✅ subscribe 收到 `monitor_throttled` 事件

### E7 concurrent：worker 完成后计数减一

```bash
# 等部分 worker 完成
sleep 15

# 触发新的应该能创建
awk '/e6-concurrent spawned/{c++} END{print c+0}' /tmp/serve.log  # → >= 4
```

**验证点：**
- ✅ worker 完成后 `active_count` 自动减一
- ✅ 后续 trigger 能继续 spawn（不超过 max_concurrent）

### E8 concurrent：max_concurrent=1 等价于 serial_skip

```bash
cat > .ion/monitors/e8.json <<'EOF'
{
  "name": "e8-single",
  "interval_secs": 2,
  "script": "echo s",
  "agent": "build",
  "prompt_template": "sleep 20",
  "mode": "concurrent",
  "max_concurrent": 1,
  "trigger_mode": "auto_spawn"
}
EOF

sleep 10

awk '/e8-single spawned/{c++} END{print c+0}' /tmp/serve.log  # → 1
awk '/e8-single throttled/{c++} END{print c+0}' /tmp/serve.log  # → >= 3
```

**验证点：**
- ✅ 行为等同 serial_skip
- ✅ spawned = 1，throttled >= 3

### E9 mode 字段默认值（serial_skip）

```bash
# 不指定 mode
cat > .ion/monitors/e9.json <<'EOF'
{"name":"e9-default","interval_secs":3,"script":"echo d","agent":"build","prompt_template":"sleep 30","enabled":true}
EOF

sleep 10

# 验证：行为等同 serial_skip
awk '/e9-default triggered/{c++} END{print c+0}' /tmp/serve.log  # → 1
awk '/e9-default skipped/{c++} END{print c+0}' /tmp/serve.log    # → >= 2
```

**验证点：**
- ✅ 不写 mode 时默认 `serial_skip`
- ✅ list RPC 显示 `"mode": "serial_skip"`

---

## Group F：消费方接入（trigger_mode）

### F1 auto_spawn：直接 spawn worker（默认）

```bash
cat > .ion/monitors/f1.json <<'EOF'
{
  "name": "f1-spawn",
  "interval_secs": 3,
  "script": "echo auto",
  "agent": "build",
  "prompt_template": "F1: {output}",
  "trigger_mode": "auto_spawn"
}
EOF

sleep 5

# 验证：worker 被创建
ion rpc --method list_workers --params '{}' | jq '.data.workers | length'
# → >= 1
```

**验证点：**
- ✅ worker 被直接 spawn
- ✅ worker 的 initial_prompt 含 "F1: auto"
- ✅ subscribe 收到 `monitor_spawned` 事件

### F2 channel_notify：推到 main channel

```bash
# 先启动一个订阅 main channel 的 coordinator
ion rpc --method create_session --params '{"agent":"coordinator"}'

# 配置 monitor 推 channel
cat > .ion/monitors/f2.json <<'EOF'
{
  "name": "f2-channel",
  "interval_secs": 3,
  "script": "echo channel-msg",
  "agent": "coordinator",
  "prompt_template": "F2 channel: {output}",
  "trigger_mode": "channel_notify"
}
EOF

sleep 5

# 验证：没有新 worker 被创建，但 coordinator 收到消息
ion rpc --method list_workers --params '{}' | jq '.data.workers | length'
# → 1（只有 coordinator，没新增）
```

**验证点：**
- ✅ 不新增 worker
- ✅ coordinator worker 收到 channel 消息（其 message queue 有新条目）
- ✅ subscribe 收到 `monitor_notified_channel` 事件

### F3 channel_notify：无订阅者退化 event_only

```bash
# 没有 coordinator 在跑，只有 monitor
cat > .ion/monitors/f3.json <<'EOF'
{
  "name": "f3-no-sub",
  "interval_secs": 3,
  "script": "echo x",
  "agent": "coordinator",
  "prompt_template": "F3: {output}",
  "trigger_mode": "channel_notify"
}
EOF

# 关闭所有 coordinator
kill $SERVE_PID; sleep 2; ion serve &
sleep 8

grep "f3-no-sub.*no_subscriber" /tmp/serve.log
```

**验证点：**
- ✅ 日志含 `no_subscriber, fallback to event_only`
- ✅ 不报错，不崩溃
- ✅ subscribe 收到 `monitor_triggered` 事件（退化后）

### F4 event_only：只 emit 事件

```bash
cat > .ion/monitors/f4.json <<'EOF'
{
  "name": "f4-event",
  "interval_secs": 3,
  "script": "echo event-only",
  "agent": "build",
  "prompt_template": "F4: {output}",
  "trigger_mode": "event_only"
}
EOF

sleep 8

# 验证：没有 worker 被创建
ion rpc --method list_workers --params '{}' | jq '.data.workers | length'
# → 0
```

**验证点：**
- ✅ 0 个 worker（不 spawn）
- ✅ subscribe 收到 `monitor_triggered` 事件，含 `output` 字段
- ✅ `status.trigger_count` 递增

### F5 trigger_mode 默认值（auto_spawn）

```bash
cat > .ion/monitors/f5.json <<'EOF'
{"name":"f5-default","interval_secs":3,"script":"echo d","agent":"build","prompt_template":"{output}","enabled":true}
EOF

sleep 5

# 验证：行为等同 auto_spawn
ion rpc --method list_workers --params '{}' | jq '.data.workers | length'
# → >= 1
```

### F6 trigger_mode + mode 组合矩阵

| trigger_mode \ mode | serial_skip | serial_queue | concurrent |
|---------------------|-------------|--------------|------------|
| `auto_spawn` | E1 ✅ | E3 ✅ | E6 ✅ |
| `channel_notify` | skip 推 channel | queue 推 channel | 多次推 channel |
| `event_only` | skip 只 emit | queue 只 emit | 多次 emit |

**F6 验证点：** channel_notify + concurrent 组合下，每次都推 channel（不检查订阅者状态）。

---

## Group G：Scheduler Agent（生成+校验配置）

> 验证 `--agent scheduler` 能生成合法 monitor.json。

### G1 scheduler 理解自然语言需求

```bash
ion --agent scheduler "我要监控 https://github.com/dyyz1993/ion 的新 bug issue，每 5 分钟检查"
```

**预期 agent 行为：**
1. read 现有 monitor 配置（不重复）
2. 询问或推断 mode/trigger_mode
3. write `.ion/monitors/github-issues.json`

**验证点：**
- ✅ agent 第一个动作是 `read .ion/monitors/`
- ✅ agent 生成的 .json 字段完整
- ✅ agent 调 `extension_rpc monitor validate` 自检

### G2 scheduler 自动调 validate

```bash
# 看 scheduler trace（subscribe）
ion subscribe &

ion --agent scheduler "监控磁盘使用率"
```

**验证点：**
- ✅ subscribe 看到 `tool_execution_start extension_rpc monitor validate`
- ✅ validate 返回 `valid: true` 后才调 `add`
- ✅ 若 validate 失败，scheduler 会自动修正后重试

### G3 scheduler 处理校验失败

```bash
# 故意给 scheduler 一个有歧义的需求
ion --agent scheduler "监控 xxx"
```

**验证点：**
- ✅ scheduler 主动澄清（"监控什么？多久一次？"）
- ✅ 不擅自生成空 monitor

### G4 scheduler 推荐 mode/trigger_mode

```bash
ion --agent scheduler "监控 GitHub issue，有新 issue 自动让 developer 处理"
```

**验证点：**
- ✅ 生成的 .json 含 `"mode": "serial_skip"`（issue 处理可能慢，不堆积）
- ✅ 含 `"trigger_mode": "auto_spawn"`
- ✅ prompt_template 含 `{output}`

### G5 scheduler dry-run

```bash
ion --agent scheduler "监控 /var/log/error.log 的 ERROR"
```

**验证点：**
- ✅ scheduler 先调 `monitor test` dry-run 一次
- ✅ 看到 would_trigger=false（log 可能没 ERROR）后调整 script 或保留监控
- ✅ 最后才调 `add`

---

## Group H：事件订阅

> 验证 monitor_* 事件能通过 subscribe 看到。

### H1 monitor_triggered 事件

```bash
# Terminal 1: 订阅
ion subscribe > /tmp/sub.log &

# Terminal 2: 触发（auto_spawn）
cat > .ion/monitors/h1.json <<'EOF'
{"name":"h1-event","interval_secs":3,"script":"echo h1","agent":"build","prompt_template":"H1: {output}","trigger_mode":"event_only","enabled":true}
EOF

sleep 5

# 验证 subscribe 收到事件
grep "monitor_triggered" /tmp/sub.log
```

**预期事件 JSON：**

```json
{
  "type": "event",
  "event": {
    "type": "extension_event",
    "extension": "monitor",
    "customType": "monitor_triggered",
    "visibility": "llm_and_ui",
    "data": {
      "name": "h1-event",
      "output": "h1",
      "mode": "serial_skip",
      "trigger_mode": "event_only"
    }
  }
}
```

**验证点：**
- ✅ customType == "monitor_triggered"
- ✅ data.output == "h1"
- ✅ data.trigger_mode == "event_only"

### H2 monitor_skipped 事件（serial_skip）

```bash
grep "monitor_skipped" /tmp/sub.log
```

**预期 data：**

```json
{"name": "e1-skip", "reason": "all_workers_busy", "active_workers": 1}
```

### H3 monitor_queued 事件（serial_queue）

```bash
grep "monitor_queued" /tmp/sub.log
```

**预期 data：**

```json
{"name": "e3-queue", "queue_length": 3, "queue_capacity": 10}
```

### H4 monitor_throttled 事件（concurrent）

```bash
grep "monitor_throttled" /tmp/sub.log
```

**预期 data：**

```json
{"name": "e6-concurrent", "active_workers": 3, "max_concurrent": 3}
```

### H5 monitor_spawned 事件

```bash
grep "monitor_spawned" /tmp/sub.log
```

**预期 data：**

```json
{"name": "f1-spawn", "worker_id": "wkr_xxx", "agent": "build"}
```

---

## Group I：真实业务场景

### I1 GitHub issue 定时拉取（完整闭环）

```bash
# 1. scheduler 生成
ion --agent scheduler "监控 https://github.com/dyyz1993/ion 的新 bug issue，每 5 分钟"

# 2. 启动 serve
ion serve &

# 3. 5 分钟后（CI 里改成 10 秒 + mock gh）
# 验证：有 issue → spawn developer
ion rpc --method list_workers --params '{}' | jq '.data.workers[].agent'
# → ["developer"]

# 4. developer worker 处理 issue（subscribe 看过程）
grep "agent_start\|text_delta\|agent_end" /tmp/sub.log
```

**验证点：**
- ✅ monitor.json 生成且通过 validate
- ✅ 触发后 developer worker 被创建
- ✅ subscribe 完整事件流：triggered → spawned → agent_start → ... → agent_end

### I2 日志异常监控

```bash
# 准备模拟日志
echo "ERROR: database connection failed" > /tmp/test.log

cat > .ion/monitors/i2.json <<'EOF'
{
  "name": "i2-log",
  "interval_secs": 5,
  "script": "grep ERROR /tmp/test.log 2>/dev/null | tail -3",
  "agent": "build",
  "prompt_template": "日志异常：\n{output}\n请分析原因",
  "mode": "serial_skip",
  "trigger_mode": "channel_notify"
}
EOF

sleep 8
```

**验证点：**
- ✅ 触发（grep 有输出）
- ✅ 推到 main channel（不直接 spawn）

### I3 进程存活检查

```bash
cat > .ion/monitors/i3.json <<'EOF'
{
  "name": "i3-proc",
  "interval_secs": 3,
  "script": "pgrep -f 'nonexistent-process-xxx' > /dev/null || echo DOWN",
  "agent": "user",
  "prompt_template": "进程挂了：{output}",
  "mode": "concurrent",
  "max_concurrent": 1,
  "trigger_mode": "event_only"
}
EOF

sleep 8
```

**验证点：**
- ✅ 触发（进程不存在）
- ✅ event_only 模式不 spawn worker
- ✅ subscribe 收到 monitor_triggered，output="DOWN"

### I4 磁盘使用率周期巡检

```bash
cat > .ion/monitors/i4.json <<'EOF'
{
  "name": "i4-disk",
  "interval_secs": 5,
  "script": "df -h / 2>/dev/null | awk 'NR==2 && $5+0 > 0 {print $5}'",
  "agent": "maintainer",
  "mode": "concurrent",
  "max_concurrent": 1,
  "trigger_mode": "auto_spawn",
  "prompt_template": "磁盘使用率：{output}，请检查"
}
EOF

sleep 8
```

**验证点：**
- ✅ 触发（df 总有输出）
- ✅ spawn maintainer worker

---

## Group J：边界+安全

### J1 路径穿越攻击

```bash
ion rpc --method extension_rpc \
  --params '{
    "extension": "monitor",
    "method": "add",
    "params": {
      "name": "../../../etc/cron.d/evil",
      "interval_secs": 60,
      "script": "echo pwned",
      "agent": "build",
      "prompt_template": "{output}"
    }
  }'
```

**预期响应（v2 必须拒绝）：**

```json
{
  "success": false,
  "error": "monitor validation failed",
  "data": {
    "errors": ["name 含非法字符（只允许 [a-zA-Z0-9_-]）"]
  }
}
```

**验证点：**
- ✅ add 返回校验失败
- ✅ `.ion/monitors/` 目录下没有 `evil` 文件
- ✅ `/etc/cron.d/evil` 没有被创建

### J2 interval_secs=0 死循环

```bash
ion rpc --method extension_rpc \
  --params '{
    "extension":"monitor","method":"add",
    "params":{"name":"j2","interval_secs":0,"script":"echo x","agent":"build","prompt_template":"{output}"}
  }'
```

**预期：**

```json
{"success": false, "error": "interval_secs 必须 >= 1"}
```

**验证点：**
- ✅ 拒绝
- ✅ serve CPU 正常（没死循环）

### J3 interval_secs 超上限

```bash
ion rpc ... --params '{"name":"j3","interval_secs":99999999,...}'
```

**预期：**

```json
{"success": false, "error": "interval_secs 必须 <= 86400（最大 1 天）"}
```

### J4 script 为空

```bash
ion rpc ... --params '{"name":"j4","interval_secs":60,"script":"","agent":"build","prompt_template":"{output}"}'
```

**预期：**

```json
{"success": false, "data": {"errors": ["script 不能为空"]}}
```

### J5 script 注入尝试

```bash
ion rpc ... --params '{
  "name":"j5",
  "interval_secs":60,
  "script":"echo x; rm -rf /",
  "agent":"build",
  "prompt_template":"{output}"
}'
```

**预期：** v2 不阻止添加（脚本是用户自己写的），但：
- ✅ `monitor test` dry-run 时显示 warning
- ✅ 命令守卫（CommandGuard）在 worker 执行时拦截 `rm -rf /`
- ✅ 日志记录"high-risk pattern detected"

### J6 agent 名不存在

```bash
ion rpc ... --params '{
  "name":"j6","interval_secs":60,"script":"echo x","agent":"nonexistent-xxx","prompt_template":"{output}"
}'
```

**预期（v2）：**

```json
{
  "success": true,
  "data": {"added": "j6", "validated": true, "warnings": ["agent 'nonexistent-xxx' 不在已知列表，触发时会失败"]}
}
```

**验证点：**
- ✅ add 成功（带 warning）
- ✅ 第一次触发时 emit `monitor_agent_not_found` + 自动 disable
- ✅ `status.last_error` 记录原因

---

## CI 脚本结构建议

`tests/monitor_ci.sh` 应该按 Group 组织：

```bash
# Group A-D：v1 已有，保留
# Group E：并发策略（serial_skip/queue/concurrent）— v2 核心
# Group F：消费方接入（auto_spawn/channel/event）
# Group G：Scheduler Agent（用 faux 或真实 LLM）
# Group H：事件订阅（subscribe 抓 monitor_* 事件）
# Group I：真实业务（mock gh / 写假 log / pgrep）
# Group J：边界安全（拒绝路径穿越/0 interval/空 script）

# 真实 LLM case 标记 ION_E2E=1
```

---

## 写作规范自查清单

- [x] 每个 case 给完整 `ion rpc` 命令
- [x] 每个 RPC 给请求/响应 JSON 规格 + 字段表
- [x] 每个 case 给验证点清单（✅ 标记）
- [x] Group 按用户场景分（加载/管理/边界/并发/接入/Agent/事件/业务/安全）
- [x] 每个 Group 有"能用"happy path case
- [x] 有专门的边界/安全 Group（J）
- [x] 测试数据是模拟真实场景（gh issue / grep ERROR / pgrep / df）
- [x] case 输入用用户自然语言（G 组）
- [x] Group 命名是用户场景
- [x] 有性能/成本可测量指标（trigger_count / skip_count / queue_length / active_workers）
