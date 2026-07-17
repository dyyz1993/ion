# 实现计划:turn 耗时 + 关键事件时间戳

## 目标

1. **turn 耗时**:让 `list_turns` / `get_turn_detail` RPC 返回每轮 turn 的 `durationMs`
2. **关键事件时间戳**:给 `agent_start` / `agent_end` / `error` / `tool_execution_end` + 扩展事件统一加 `timestamp` 字段

## ⚠️ 一个重要的实现约束(必读)

**timestamp 必须放在 event 对象内部,不能放顶层。**

原因:Manager 的 event-pump(`ion.rs:3273`)重建事件时只搬运 `event` 内部字段:
```rust
"event": msg.get("event").cloned().unwrap_or(msg.clone()),
```
worker 端顶层加的字段会被丢弃。现有的 `tool_execution_start` 事件(`ion_worker.rs:3234`)就是放在 event 内部,本次保持一致。

---

## Part 1:turn 耗时(持久化层 + RPC 出口)

好消息:`session_jsonl.rs` 的 `append_turn_summary` 函数**已经接收 `duration_ms` 参数并写入 jsonl**(session_jsonl.rs:750,765),测试 fixture 也早就写好了 durationMs(`message_retrieval_ci.sh:95/98/102`)。**这一层完全不用改。** 缺的只是:① 采集真实耗时 ② 内存结构透传 ③ RPC 序列化。

### 改动 1.1 — 采集 turn 开始时刻
**文件**:`src/agent/agent_loop.rs:610`

在 `self.turn_index = turn;` **之前**加一行(在 `for turn in 0..max_turns {` 循环体顶部):
```rust
let turn_start = std::time::Instant::now();
```

用全路径 `std::time::Instant`(跟现有 864 行的 `let start = std::time::Instant::now()` 保持一致,不动 import)。变量名用 `turn_start` 避免和 864 行的工具级 `start` 重名。

### 改动 1.2 — persist_turn_summary 签名加参数
**文件**:`src/agent/agent_loop.rs:1328-1333`

```rust
fn persist_turn_summary(
    &self,
    turn: u64,
    events: &[ion_provider::StreamEvent],
    stop_reason: &ion_provider::StopReason,
    duration_ms: u64,           // ← 新增
) {
```

### 改动 1.3 — persist_turn_summary 内部用真值替换 0
**文件**:`src/agent/agent_loop.rs:1424`

```rust
// 原:0, // durationMs 暂不测(需 turn 开始时间戳)
duration_ms,
```

### 改动 1.4 — 4 个调用点传真实 duration
**文件**:`src/agent/agent_loop.rs`,4 处调用:

- **:745**(Stop/Length 正常结束)
- **:999**(ToolUse 路径)
- **:1005**(Error 路径)
- **:1053**(Aborted 路径)

每处都加第 4 个参数 `turn_start.elapsed().as_millis() as u64`。因为 4 处都在 `inner_loop` 循环体内,`turn_start` 直接可见,无需跨函数传参。

示例(745 行):
```rust
self.persist_turn_summary(
    turn as u64,
    &events,
    &stop_reason,
    turn_start.elapsed().as_millis() as u64,
);
```

### 改动 1.5 — TurnOverview 加字段
**文件**:`src/message_retrieval.rs:161`(在 `summary` 字段后)

```rust
pub summary: String,
pub duration_ms: u64,    // ← 新增
```

### 改动 1.6 — extract_from_turn_summary 读字段
**文件**:`src/message_retrieval.rs:839` 之后(`..Default::default()` 之前)

```rust
duration_ms: ts
    .get("durationMs")
    .and_then(|v| v.as_u64())
    .unwrap_or(0),
```

### 改动 1.7 — list_turns / get_turn_detail RPC 加字段(4 处)

| 文件:行 | 改动 |
|---------|------|
| `src/rpc.rs:788`(list_turns)| `"summary": t.summary,` 后加 `"durationMs": t.duration_ms,` |
| `src/rpc.rs:856`(get_turn_detail)| `"status": detail.overview.status,` 后加 `"durationMs": detail.overview.duration_ms,` |
| `src/bin/ion_worker.rs:868`(list_turns)| `"summary": t.summary,` 后加 `"durationMs": t.duration_ms,` |
| `src/bin/ion_worker.rs:913`(get_turn_detail)| `"status": detail.overview.status,` 后加 `"durationMs": detail.overview.duration_ms,` |

---

## Part 2:关键 stdout 事件加 timestamp(event 内部)

按现有 `tool_execution_start` 的样式,`timestamp: now_ms()` 塞在 event 对象内部。

### 改动 2.1 — agent_start / agent_end / error(ion_worker.rs,4 处)

| 行号 | 事件 | 分支 |
|------|------|------|
| `:1027` | agent_start | bash 分支 |
| `:1029` | agent_end | bash 分支 |
| `:1066` | agent_start | 主分支 |
| `:1080` | agent_end | 主分支 |
| `:1085` | error | 主分支 |

示例(1066):
```rust
output(&serde_json::json!({"type":"event","event":{"type":"agent_start","sessionId":sid,"timestamp":now_ms()}}));
```

> `now_ms()` 在 `ion_worker.rs:3692` 已定义,直接复用。

### 改动 2.2 — tool_execution_end 补 timestamp
**文件**:`src/bin/ion_worker.rs:3261-3271`

在 event 内部 `durationMs` 旁边加 `"timestamp": now_ms()`(与 start 对称,能独立定位结束时刻)。

### 改动 2.3 — 扩展事件 helper 统一注入(3 个文件)

| 文件:行 | helper | 覆盖范围 |
|--------|--------|---------|
| `src/agent/bash.rs:673` | `emit_extension_event` | process_started/completed/output(5 个发射点全覆盖) |
| `src/file_snapshot/approval.rs:447` | `emit_approval_event` | ApprovalRequest/Resolved(3 个发射点全覆盖) |
| `src/agent/memory.rs:476` | `emit()` | memory_saved 等(注意是顶层格式,timestamp 加在顶层 ev) |

每个 helper 在构造 event JSON 时加 `"timestamp": now_ms()`。

> bash.rs 在 :669 已有 `now_ms()`;approval.rs 需要确认用 `now_ms()` 还是 `now_ts()`(可能要加一个毫秒版 helper);memory.rs 需要加 `now_ms()`(目前没有)。

### 改动 2.4 — memory.rs:388 内联 println
**文件**:`src/agent/memory.rs:388-395`(MemoryTool::execute 里)

这个不走 helper,是内联 println,需要单独加 `"timestamp": now_ms()` 到顶层 ev。

---

## 不改的地方(明确边界)

- ❌ **text_delta** — 高频流式,不加
- ❌ **tool_execution_update** — 高频流式,不加
- ❌ **ion.rs event-pump** — 不改,timestamp 在 worker 端注入后自动透传
- ❌ **worker_registry.rs child_event 重建** — 不改,`msg["event"]` 浅 clone 自动带 timestamp
- ❌ **session_jsonl.rs** — 落盘早就支持 durationMs,不用动
- ❌ **Event 枚举 / WorkerEvent 枚举** — 僵尸类型,不在生产路径,不碰
- ❌ **memory.rs 格式不一致问题** — 顶层 extension_event vs event 嵌套,这是独立 bug,本次只加 timestamp 不动格式

---

## 测试

### 必须更新

- **`tests/message_retrieval_ci.sh`** — 补一个 case(或加到现有 Group)验证 `list_turns` RPC 响应里含 `durationMs`。目前这个脚本完全没测 RPC 响应字段。
- **手工冒烟** — 跑一个真实会话,确认 `list_turns` 返回的 durationMs 是非零真实值(不是 0)。

### 可选(不强制)

- `tests/unit_rpc_test.rs` — 可补 `u05_list_turns` 测 RPC 响应字段(目前完全没测 list_turns)。
- 现有 `tests/message_retrieval_ci.sh:416-421`(K3)已验证 jsonl 落盘含 durationMs,不用改。

### 验证命令

```bash
# 编译
cargo build --bin ion --bin ion-worker

# 单元测试不破
cargo test --lib

# 手工冒烟 turn 耗时(用 faux 驱动,不调真 LLM)
# 起 host → prompt → list_turns 检查 durationMs 非零
```

---

## 改动量汇总

| Part | 文件数 | 改动点数 | 复杂度 |
|------|--------|---------|--------|
| Part 1 (turn 耗时) | 3 个文件(agent_loop / message_retrieval / rpc×2) | 11 处 | 低,每处 1-3 行 |
| Part 2 (事件 timestamp) | 4 个文件(ion_worker / bash / approval / memory) | ~11 处 | 低,每处 1 行 |
| 测试 | 1 个脚本 | 1 个新 case | 中 |
| **合计** | **6 个文件** | **~23 处** | **低** |

所有改动都是纯加字段,不破坏现有消费者(它们按 `event.type` 分发,多出来的 timestamp 字段会被忽略)。