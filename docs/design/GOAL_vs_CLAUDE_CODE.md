# Goal Supervisor vs Claude Code `/goal` — 架构对比

> **状态：定稿**（基于 ion `master` + Claude Code 2026-08 文档）
> **目的**：搞清楚 ion 自己的 `GoalSupervisorExtension` 跟 Claude Code 内置的 `/goal`、`/plan`、todos、subagents、hooks 到底是同一个东西的不同实现，还是完全不同的物种。

---

## TL;DR

**Claude Code `/goal` = 灵活但靠 LLM 自觉；ion `GoalSupervisor` = 死板但 kernel 强制 + 证据驱动 + 反死循环 + 跨 session 进化。**

ion 这套明显是为「**无人值守的自主编程 agent**」设计的——你看重「agent 说我完成了但是不是真完成」，CC 那条路天然有幻觉风险，ion 直接用 shell exit code 把这个口堵死了。

---

## 1. 对比表

| 维度 | **ion `GoalSupervisor`** | **Claude Code `/goal`** | 谁更重 |
|---|---|---|---|
| **触发** | agent 调 `goal_set` 工具（自声明） | 用户敲 `/goal <条件>` | 平 |
| **停止判定** | **确定性**：跑 shell checks，全 PASS 才放行 | **LLM 软判断**：fast model 每轮检查条件 | **ion**（无幻觉） |
| **闭环机制** | **kernel 钩子 `on_gate_check`**：agent 想停 → 跑 checks → 失败则 `RetryWith(msg)` 强制注入继续 | LLM 自愿，每轮检查后自动起新 turn | **ion**（agent 无法绕过） |
| **检查来源** | 三源：LLM 生成 / 用户提供 / CI 默认（cargo build/test/no-ufffd） | 单源：用户给的条件 | **ion** |
| **检查类型** | `ExitCode` / `GrepEmpty` / `FileExists` | 自然语言条件 | **ion**（机器可验） |
| **反死循环** | 4 重 guard：`max_iter`(10) / `max_duration`(60min) / `max_cost`($1) / **`similarity≥0.8`×3** | ❌ 几乎没有，有 runaway 风险 | **ion** |
| **失败分析** | `goal_diagnose` 工具 + LLM 分析失败原因 | 无 | **ion** |
| **进化层** | 写 `iterations.jsonl` → `goal_evolver` 跨 session 分析（deadloop/model/context 三维度）→ 生成 Issue | 无 | **ion** |
| **集成位置** | cmd_run + worker_rpc 都注册（src/bin/ion.rs:2131、worker_rpc.rs:1013） | 任意模式 built-in | 平 |
| **kernel 改动** | 零改动（用 ExtensionApi） | N/A | 平 |

---

## 2. 三个本质差异

### 2.1 强制力：kernel 钩子 vs LLM 自愿

ion 在 `agent_loop` 里挂了 `on_gate_check`（[`src/agent/agent_loop.rs:1428`](../../src/agent/agent_loop.rs)）：

```rust
// LLM 决定 Stop → 内核主动调 extensions.check_gates
match self.extensions.check_gates(&gate_ctx).await? {
    GateDecision::RetryWith(msg) => {
        // 失败 → 注入失败证据作为新 user message
        self.messages.push(Message::User(... msg ...));
        continue;  // 强制再循环
    }
    GateDecision::Allow => return Ok(stop_reason),
}
```

**agent 想停下时 kernel 主动拦截**——跑 checks，失败就把失败证据 + 剩余步骤作为新的 user message 注入，agent 必须继续。

Claude Code 的 `/goal` 是「LLM 自己每轮检查」，理论上 LLM 可以宣告「我觉得完成了」然后停（即便条件没真满足）。

### 2.2 验证方式：确定性 shell vs 自然语言

ion 的 check 是结构化定义：

```json
{
  "name": "unit_tests",
  "command": "cargo test",
  "pass_criteria": {"kind": "exit_code", "expected": 0},
  "must_pass": true
}
```

三种 `pass_criteria`：`ExitCode` / `GrepEmpty` / `FileExists`，全部**机器可验**。

Claude Code 的 `/goal` 条件是自然语言，靠 fast model 理解——好处是灵活，坏处是**会幻觉**（这正是 [`GOAL_SUPERVISOR.md` §0.1](GOAL_SUPERVISOR.md) 明确反对 pi 的 `session-supervisor` 路线的原因）。

### 2.3 反 runaway：ion 有 4 重护栏，CC 几乎没有

```rust
max_iter         = 10       // 单 goal 最多迭代 10 次
max_duration_min = 60       // 总耗时上限
max_cost_usd     = 1.0      // 总花费上限
similarity ≥ 0.8 × 3        // 连续 3 次输出相似度 ≥0.8 → 判定死循环
```

Claude Code 的 `/goal` 文档明确**没有 max-turn 限制**，runaway 风险靠自己写 Stop hook 兜底。

---

## 3. ion 独有：进化闭环（goal_evolver）

这是 ion 设计里 Claude Code 完全没有的一层（[设计文档 §8](GOAL_SUPERVISOR.md)）：

```
goal_supervisor 执行
    ↓ 写 iterations.jsonl
goal_evolver 分析（deadloop / model 选错 / context 不足）
    ↓ 生成 GitHub Issue / 改进计划
人类 / A→B self-evolution 实施
```

跨 session 累积学习「什么样的目标容易死循环、什么模型容易分析失败」——这是产品级的进化系统。Claude Code 那边对应的能力（如果有）得用户自己拼 hooks + 自定义脚本。

---

## 4. Claude Code 这边对应的功能

| ion 对应物 | Claude Code 功能 | 触发 | 性质 |
|---|---|---|---|
| `goal_supervisor` | `/goal` | 用户敲命令 | 跨 turn 持续，LLM 软判断 |
| — | `/plan`（plan mode） | Shift+Tab / 命令 | 一次性研究计划，等用户审批 |
| — | TaskCreate / TodoWrite | LLM 主动 | 视觉跟踪，不驱动迭代 |
| — | Agent (subagents) | LLM 主动 | 隔离 context，不能自驱循环 |
| `on_gate_check` | Stop hooks | 内核钩子 | 阻断/放行，不会"继续试" |
| — | `/loop` | 用户敲命令 | 按时间间隔重跑 |

**Claude Code 文档引用**：
- `/goal`: https://code.claude.com/docs/en/goal.md
- Plan mode: https://code.claude.com/docs/en/permission-modes.md
- Subagents: https://code.claude.com/docs/en/sub-agents.md
- Hooks: https://code.claude.com/docs/en/hooks.md
- `/loop` & scheduled tasks: https://code.claude.com/docs/en/scheduled-tasks.md
- Auto mode: https://code.claude.com/docs/en/auto-mode-config.md

---

## 5. Claude Code 真正缺的

按"自主闭环直到完成"这个标准，Claude Code **没有**直接对等物：

- ❌ LLM 无法被 kernel 强制重试（最多阻断/放行）
- ❌ 没有 deadloop 检测（`/goal` 无 max-turn 限制）
- ❌ 没有相似度阈值（不会自动判定"agent 卡了"）
- ❌ 没有跨 session 的失败学习层
- ❌ 自然语言 stop 条件依赖 fast model 不幻觉

要用 Claude Code 拼出 ion 的体验，得自己组：
`/goal` + 自定义 Stop hook（实现 max-iter + 相似度检测）+ 自定义日志聚合（跨 session 学习）

---

## 6. ion 短板

- **CLI 单次模式用不上闭环**：`ion "xxx"` 进程跑完即退，agent 还没来得及第二次 goal_set 就退出了。所以 on_gate_check 在 cmd_run 路径下虽然能触发（最多一次 retry），但真正的多轮闭环需要 host 模式（`ion --host` / worker_rpc / WebUI）。
- **检查定义偏程序员**：`cargo_build / cargo_test / GrepEmpty` 都是 dev 视角，不像 CC 的自然语言那样跨领域友好。
- **进化层未完整实现**：`goal_evolver.rs` 是分析层，但「自动开 GitHub Issue」这部分还没接到生产。

---

## 7. 结论：什么时候用哪个

| 场景 | 推荐 | 原因 |
|---|---|---|
| 探索式产品需求 / 用户在环 | Claude Code `/goal` | 自然语言灵活，用户随时打断 |
| **无人值守自主编程**（CI、self-evolution） | **ion GoalSupervisor** | 证据驱动，agent 装完成没用，必须真过 checks |
| 长跑训练（跨 session 学习） | **ion** | goal_evolver 进化层 CC 没有 |
| 一次性研究 / 文档撰写 | 都不用，用 plan mode | 闭环是浪费 |

---

## 附录：相关代码索引

| 组件 | 路径 | 行号 |
|---|---|---|
| `GoalSupervisorExtension` | `src/goal_supervisor_extension.rs` | 263 |
| `GoalSetTool` | 同上 | 1376 |
| `GoalRefineTool` | 同上 | 1539 |
| `GoalDiagnoseTool` | 同上 | 1640 |
| `on_gate_check` 钩子 | 同上 | 1254 |
| 4 重 guard | 同上 | 465 |
| 进化层 `goal_evolver` | `src/goal_evolver.rs` | — |
| cmd_run 注册 | `src/bin/ion.rs` | 1592（state）/ 1631（tool）/ 2131（ext） |
| worker_rpc 注册 | `src/worker_rpc.rs` | 326（state+tool）/ 1013（ext） |
| kernel `check_gates` 调用 | `src/agent/agent_loop.rs` | 1428 |
| 设计文档 | `docs/design/GOAL_SUPERVISOR.md` | — |
| 本文档 | `docs/design/GOAL_vs_CLAUDE_CODE.md` | — |
