# Workflow Gate — 内核级交付校验

> **状态：已完成** — `GateDecision` + `on_gate_check` hook + `WorkflowExtension` 全部实现。
> 6 个单元测试通过，全量 217 测试无回归。

---

## 一、设计理念

**让内核强制校验交付质量，不依赖 LLM 自觉。**

之前纯提示词方案的问题：orchestrator 靠 LLM 自觉跑 `bash` 检查 gate——LLM 可能忘记、可能跳过、可能幻觉。现在内核在 LLM 决定 Stop 时**自动**执行 gate 命令，不通过就注入消息强制继续。

```
之前（纯提示词）：
  LLM: "我做完了" → orchestrator 记不记得跑 gate？→ 靠不住 😅

之后（内核强制）：
  LLM: "我做完了" → 内核：等等，我检查下 gate
                   → gate 通过 → 允许 Stop
                   → gate 不通过 → 注入"GATE FAILED" → 继续干活 💪
```

---

## 二、原理

### 2.1 GateDecision 枚举

```rust
pub enum GateDecision {
    /// Gate 通过，允许 agent 停止
    Allow,
    /// Gate 失败，注入消息后强制继续循环
    RetryWith(String),
}
```

定义在 `src/agent/extension.rs`。是第 30 个 Extension hook。

### 2.2 调用时机

在 `inner_loop` 的 `StopReason::Stop` 分支里：

```
inner_loop (每轮 turn)
  │
  ├─ stream_with_retry → LLM 响应
  │
  ├─ stop_reason = Stop?
  │    ├─ 反幻觉重试（没调工具？→ 警告 + 重试）
  │    ├─ check_gates()
  │    │    ├─ GateDecision::Allow → 正常停止
  │    │    └─ GateDecision::RetryWith(msg)
  │    │         → push msg → continue（强制再跑一轮）
  │
  └─ stop_reason = ToolUse?
       → 执行工具 → continue
```

关键代码在 `src/agent/agent_loop.rs:574-594`。

### 2.3 WorkflowExtension

`src/agent/workflow_extension.rs` 实现 `on_gate_check`：

```
on_gate_check(ctx)
  │
  ├─ 已通过标记? → Allow（idempotent）
  │
  ├─ 超过 max_retries? → Allow（防无限循环）
  │
  ├─ 执行 bash gate_command
  │    ├─ 输出包含 gate_expected? → Allow + 标记已通过
  │    └─ 不包含?
  │         → RetryWith("GATE CHECK FAILED (attempt N/M).\n
  │                     Gate command: `...`\n
  │                     Expected: '...'\n
  │                     Actual: '...'\n
  │                     Fix it so the gate passes.")
```

### 2.4 注册时机

`ion_worker.rs` 在 extension 注册阶段：
- 读 agent 配置的 `workflow:` 段
- 有定义 → 注册 `WorkflowExtension`
- 无定义 → 不注册（零开销）

---

## 三、CLI 用法

### 3.1 在 agent .md 里定义 gate

```yaml
---
name: developer
tools:
  - write
  - bash
  - ls
disallowed_tools:
  - spawn_worker
workflow:
  # bash 命令：创建的文件必须存在
  gate_command: "ls calc.py 2>/dev/null && echo EXISTS || echo MISSING"
  # 期望输出包含的字符串
  gate_expected: "EXISTS"
  # 最大重试次数（默认 3）
  max_retries: 3
---
```

把文件放到 `.ion/agents/developer.md`，然后：

```bash
ion --host --agent developer "创建 calc.py"
```

### 3.2 实现举例

**Gate 通过：**

```bash
# developer 说"我做完了"
# 内核自动执行: ls calc.py 2>/dev/null && echo EXISTS || echo MISSING
# → 输出 "EXISTS" → 包含 "EXISTS" → Allow
# → agent 正常停止
```

**Gate 失败（developer 没创建文件）：**

```bash
# developer 说"我做完了"
# 内核自动执行: ls calc.py 2>/dev/null && echo EXISTS || echo MISSING
# → 输出 "MISSING" → 不包含 "EXISTS" → RetryWith
# → 内核注入：
#   "GATE CHECK FAILED (attempt 1/3).
#    Gate command: `ls calc.py 2>/dev/null && echo EXISTS || echo MISSING`
#    Expected output to contain: 'EXISTS'
#    Actual output: MISSING
#    Fix the issue so the gate passes."
# → 强制继续循环 → developer 收到这条消息 → 修复
```

### 3.3 更多 gate 示例

```yaml
# 文件存在检查
workflow:
  gate_command: "ls hello.py 2>/dev/null && echo FOUND || echo LOST"
  gate_expected: "FOUND"

# git commit 检查
workflow:
  gate_command: "git log --oneline -1 | wc -l | grep -q 1 && echo COMMITTED || echo NONE"
  gate_expected: "COMMITTED"

# Python 测试通过
workflow:
  gate_command: "python3 -m pytest test_calc.py -q 2>&1 | grep -q 'passed' && echo TESTS_OK || echo FAILED"
  gate_expected: "TESTS_OK"

# 文件内容检查
workflow:
  gate_command: "grep -q 'def add' calc.py && echo HAS_FUNC || echo NO_FUNC"
  gate_expected: "HAS_FUNC"
```

---

## 四、完整交付闭环（orchestrator + gate）

当前 orchestrator 和 workflow gate 是两层互补的机制：

| 层 | 作用 | 改什么 |
|:---|:-----|:-------|
| **orchestrator.md**（提示词） | 阶段编排：哪个阶段 spawn 哪个 agent | 编辑 .md |
| **WorkflowExtension**（内核） | 每阶段 gate 校验强制执行 | 在 agent .md 加 workflow: 段 |

示例——developer 自动有 gate 校验：

```yaml
# .ion/agents/developer.md
workflow:
  gate_command: "ls calc.py 2>/dev/null && echo EXISTS || echo MISSING"
  gate_expected: "EXISTS"
```

```bash
# orchestrator 跑 Stage 2 DEVELOP
ion --host --agent orchestrator "创建 calc.py"

# → orchestrator: spawn developer with worktree=true
# → developer: 创建 calc.py
# → developer: "我做完了" (Stop)
# → 内核: 执行 ls calc.py → 不通过 → RetryWith
# → developer 收到"GATE CHECK FAILED" → 修复 gate_test.py
# → developer: "我修复了" (Stop)
# → 内核: ls calc.py → 通过 → Allow
# → orchestrator: Stage 2 PASS → 进 Stage 3
```

---

## 五、CI 测试

### 5.1 单元测试

```bash
cargo test --lib workflow
```

6 个用例：

| 测试 | 验证 |
|------|------|
| `test_gate_pass_when_command_succeeds` | gate 命令输出含期望 → Allow |
| `test_gate_fail_returns_retry_with` | gate 命令不含期望 → RetryWith(msg) |
| `test_gate_retries_exhausted_allows_stop` | 超过 max_retries → Allow |
| `test_gate_passes_after_success` | 通过后 idempotent → Allow |
| `test_workflow_gate_config_defaults` | 默认值正确 |
| `test_workflow_gate_config_custom` | 自定义值正确 |

### 5.2 全量回归

```bash
cargo test --lib     # 180 passed
cargo test --bin ion # 37 passed
```

### 5.3 预期加入 CI 的 e2e 测试

后续可以在 `tests/scenario2_ci.sh` 增加 gate 校验的 Group：

```
Group A2-9: Workflow Gate
  A2-9-1: developer 带 gate 创建文件 → gate 通过
  A2-9-2: developer 带 gate 没创建文件 → gate 失败 → 重试
  A2-9-3: gate 重试超限 → Allow（防止无限循环）
```

---

## 六、文件索引

| 文件 | 内容 |
|------|------|
| `src/agent/extension.rs` | `GateDecision` 枚举、`on_gate_check` hook、`check_gates` 聚合 |
| `src/agent/agent_loop.rs:574-594` | inner_loop Stop 分支调用 gate |
| `src/agent/workflow_extension.rs` | `WorkflowExtension` 实现 + 6 个单元测试 |
| `src/agent_config.rs` | `AgentConfig.workflow` 字段 |
| `src/bin/ion_worker.rs` | WorkflowExtension 注册 |
| `examples/agents/orchestrator.md` | orchestrator 提示词（可搭配 gate 使用） |
