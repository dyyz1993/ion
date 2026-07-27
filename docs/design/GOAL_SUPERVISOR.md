# Goal Supervisor — 目标驱动的自主闭环扩展

> **状态：设计定稿** — 证据驱动的 goal 闭环 + 后台静默进化系统。基于现有 ExtensionApi，零内核改动。

---

## 0. 设计起源与定位

### 0.1 为什么做这个

pi 有 `session-supervisor` 扩展，但它是 **LLM 软判断**（小模型 reassessment + 置信度门槛），会幻觉——agent 说"完成了"但其实没完成。

ION 的 Goal Supervisor 采用**证据驱动**：完成 = 全部检测项 PASS + 有证据，不是"LLM 觉得完成了"。

### 0.2 跟 Plan / Monitor 的关系（三者独立，互不替代）

| | **Plan（计划）** | **Goal（目标）** | **Monitor（监控）** |
|---|---|---|---|
| **是什么** | 一份 Markdown 文档 | 一段目标描述 | 一个外部信号源 |
| **触发时机** | 不明确时（agent 主动进 plan mode） | 给目标时（用户/agent 设 goal） | 外部信号来时（定时器/脚本） |
| **谁拆解** | agent 拆 + 用户确认每步 | agent 自己拆，不要求确认 | 不拆解，只触发 |
| **停止条件** | 用户审批放行 | 目标 100% 完成（证据证明） | 信号持续/手动停 |
| **运转模式** | 一次性（确认完就 exit） | 持续闭环（执行→校验→继续） | 持续循环（interval） |
| **核心价值** | 人机对齐方案 | 自主闭环直到完成 | 外部驱动触发 |

**结论**：Plan 是"协作确认工具"，Goal 是"自主闭环引擎"，Monitor 是"外部触发器"。三者完全不同，互不替代。

### 0.3 归属：用户扩展，不进内核

- **场景 1（`ion "xxx"`）用不上**：进程跑完即退，闭环没机会转
- **场景 2/3 能用**：有 host，agent_end 后能注入 continue
- **策略性强**：完成判定、检测项生成、进化策略全是策略层
- **内核底座已具备**：`on_agent_end` + `ApiRegistry::complete` + resume + emit + fs

→ 做成**用户扩展**（跟 learning/monitor 同级），零内核改动。

---

## 1. 核心流程（证据驱动）

```
用户/agent 设目标（goal_set tool，对话式）
    │
    ① Focus：锁定上下文（当前会话 / 工作区 / 文件集）
    │
    ② 加载技能：
       ├─ GOAL skill（通用：怎么拆目标、怎么生成检测项）
       └─ 领域 skill（rust-ci / ts-ci：CI 清单 + 常见意外）
    │
    ③ 生成检测清单（LLM 调 ApiRegistry，拿 skill 当 prompt）：
       ├─ CI 检测（兜底）：cargo build / test / clippy / grep U+FFFD
       └─ 意外检测（目标相关）：改 auth→验证登录不破；改中文→查 U+FFFD
    │
    ④ 闭环执行（on_agent_end 自动触发）：
       agent 干活 → agent_end → run_all_checks（确定性执行）
         ├─ 全 PASS + 有证据 → complete ✅
         └─ 有 FAIL → 注入"检测X失败，证据Y" → continue → 回到执行
```

**核心原则：完成不是 LLM 说的，是证据证明的。**

---

## 2. 对 LLM 暴露的工具：只有 1 个

| Tool | 作用 |
|------|------|
| **`goal_set`** | 设/覆盖目标。参数：`objective`（必填）、`checks`（可选，不传则内部用 skill 生成） |

### 2.1 为什么只有 1 个

- **不要 `goal_clear`**：直接 `goal_set` 覆盖即可。给 clear 会给 agent 偷懒的入口（"目标难就清掉"）
- **不要 `goal_propose` / `goal_confirm`**：agent 要确认就在对话里说"我建议目标X，行吗"，用户说行，agent 调 `goal_set`
- **不要 `goal_regenerate`**：就是再调一次 `goal_set`
- **不要 `goal_add_check`**：检测项在 `goal_set` 时一次性带，或让内部 skill 生成
- **不要 `goal_run_checks`**：这是 `on_agent_end` 内部自动行为，不暴露

### 2.2 `goal_set` 工具签名

```json
{
  "name": "goal_set",
  "description": "设置或覆盖当前会话的目标。设置后，每次 agent 结束会自动跑检测项，没全 PASS 就继续执行，直到目标完成或触发防线。",
  "parameters": {
    "objective": { "type": "string", "description": "目标描述，一段话" },
    "checks": {
      "type": "array",
      "description": "可选。检测项清单。不传则用 skill 自动生成。",
      "items": {
        "type": "object",
        "properties": {
          "name": { "type": "string" },
          "type": { "enum": ["ci", "contingency"] },
          "command": { "type": "string", "description": "执行的命令" },
          "pass_criteria": { "type": "string", "description": "通过条件，如 exit_code==0" }
        }
      }
    }
  }
}
```

---

## 3. 检测项数据结构（核心）

每个检测项**必须能产出证据**，否则不算检测：

```rust
struct Check {
    name: String,                      // 唯一标识，如 "cargo_test"
    check_type: CheckType,             // Ci | Contingency
    rationale: String,                 // 为什么有这个检测（改了 auth→验证登录不破）
    command: String,                   // cargo test / grep U+FFFD / 自定义脚本
    pass_criteria: PassCriteria,       // ExitCode(i32) | GrepEmpty | FileExists | ...
    must_pass: bool,                   // 是否阻断完成
    generated_by: CheckSource,         // Skill | User | Internal
}

struct CheckResult {
    name: String,
    status: CheckStatus,               // Pass | Fail | Error | Skipped
    evidence: Evidence,                // 必须有，否则视为 Fail
    duration_ms: u64,
}

struct Evidence {
    exit_code: Option<i32>,
    stdout_excerpt: Option<String>,    // 截断后的输出
    artifact_path: String,             // 完整日志文件路径（可复查）
    matches: Option<Vec<String>>,      // grep 匹配行（用于 U+FFFD 等）
}
```

### 3.1 两类检测

| 类型 | 谁定 | 例子 | 特点 |
|------|------|------|------|
| **CI 检测**（兜底） | 领域 skill 写死 | cargo build/test/clippy、tsc --noEmit | 不管目标是什么都要过 |
| **意外检测**（目标相关） | GOAL skill 根据目标动态生成 | 改中文→查 U+FFFD；改 API→查兼容；改 SQL→查注入 | 覆盖"这个目标可能引入的意外" |

**关键**：意外检测的生成质量，是进化 agent 要持续改进的核心。

---

## 4. Skill 设计（两层）

### 4.1 GOAL skill（通用，`skills/goal/SKILL.md`）

```markdown
# GOAL Skill

## 职责：把目标拆成可验证的检测清单

### 生成检测项的规则
1. 分析目标 → 找出"这个改动可能引入的意外"
2. 每个意外 → 生成一个检测项（必须有 command + evidence）
3. 兜底加 CI 检测（调领域 skill 的 CI 清单）

### 意外检测的生成启发式
- 目标涉及"改代码" → 检测：现有测试不破
- 目标涉及"中文/文案" → 检测：grep U+FFFD == 0
- 目标涉及"API/接口" → 检测：公开 API diff 为空
- 目标涉及"SQL/数据库" → 检测：注入测试 + migration 可回滚
- 目标涉及"权限/安全" → 检测：权限测试全过

### 证据要求（硬性）
- 每个 PASS 必须有 artifact（日志文件/diff）
- 无 evidence 的 verdict 一律视为 FAIL
```

### 4.2 领域 skill（`skills/rust-ci/SKILL.md`）

```markdown
# Rust CI Skill

## CI 检测清单（兜底，不管目标是什么都跑）
- cargo build --lib
- cargo test --lib
- cargo clippy -- -D warnings
- grep U+FFFD src/ == 0  （ION 特色）

## 常见意外（Rust 特有，供 GOAL skill 参考）
- 编译 warning 增加
- 公开 API 破坏
- U+FFFD 乱码（UTF-8 处理）
```

**ION 自带 rust-ci skill**（开箱即用，因为 ION 是 rust 项目）。其他语言（ts/python）用户自写或调 skill。

---

## 5. 状态机

```
                    goal_set(objective)
                          │
                          ▼
                     ┌─ running ─────────┐
                     │   (agent 执行)     │
                     └───────┬───────────┘
                              │ on_agent_end
                              ▼
                     ┌─ checking ─────────┐
                     │ run_all_checks     │
                     │ (确定性执行)        │
                     └───┬──────┬─────┬───┘
                         │      │     │
              all_pass ←─┘      │     └──→ blocked
              (有证据)           │          (卡住,等人)
                               │
                  has_fail / repetitive
                               │
                               ▼
                     ┌─ continuing ───────┐
                     │ 注入 continue msg   │
                     │ (带失败证据)        │
                     │ 回到 running        │
                     └────────────────────┘

  任意状态 → exhausted   （硬上限/时长/成本到顶）
  任意状态 → cancelled   （用户 goal_set 覆盖）
```

---

## 6. 死循环防护（6 道防线）

参考 pi 的 5 道 + ION 加的 2 道（时长/成本上限）：

| # | 防线 | 实现 | 默认值 |
|---|------|------|--------|
| ① | **硬上限** | `max_iterations`，到顶标记 exhausted | 20 |
| ② | **置信度门槛** | 生成检测项时 LLM 置信度（用于判断检测质量） | 0.6 |
| ③ | **重复检测** | actionPlan 文本相似度 > 阈值 → 标记 repetitive | 0.8 |
| ④ | **重复换策略** | repetitive 时注入"换角度"prompt | — |
| ⑤ | **总时长上限**（ION 新增） | `max_total_duration_min` | 30 分钟 |
| ⑥ | **成本上限**（ION 新增） | `max_total_cost_usd` | $5 |

### 6.1 配置

```json
{
  "goal_supervisor": {
    "enabled": false,
    "check_on_agent_end": true,
    "generate_checks_model": "fast",
    "execute_model": "max",
    "max_iterations": 20,
    "max_total_duration_min": 30,
    "max_total_cost_usd": 5.0,
    "repetition_threshold": 0.8,
    "delay_ms": 5000,
    "pause_threshold_ms": 300000
  }
}
```

---

## 7. 日志 Schema（进化系统的输入，核心）

### 7.1 日志位置

```
~/.ion/agent/goal-runs/<session_id>/
  ├── goal.json              # 目标元信息
  ├── iterations.jsonl       # 每次迭代完整记录（核心）
  └── final-report.json      # 最终结果（含 outcome 回填）
```

### 7.2 `iterations.jsonl` 每行 schema

```json
{
  "iter": 3,
  "timestamp": "2026-07-27T10:30:00Z",
  "session_id": "sess_xxx",
  "goal_id": "goal_xxx",
  "objective": "修复登录bug并加测试",

  "guards_hit": {
    "repetitive": false,
    "max_iter": false,
    "max_duration": false,
    "max_cost": false,
    "low_confidence": true
  },
  "similarity_to_prev": 0.3,

  "llm_calls": [
    {
      "purpose": "generate_checks",
      "model": "zai/glm-5.2",
      "checks_generated": 5
    },
    {
      "purpose": "analyze_failure",
      "model": "opencode/deepseek-v4-flash",
      "analysis_used": true
    }
  ],

  "context_snapshot": {
    "recent_messages": 15,
    "file_changes": ["src/auth.rs"],
    "git_diff_lines": 142,
    "test_results_included": false,
    "skill_loaded": "rust-ci"
  },

  "checks_run": [
    {
      "name": "cargo_test",
      "type": "ci",
      "status": "pass",
      "evidence": {
        "exit_code": 0,
        "artifact": "iter3/cargo_test.log"
      }
    },
    {
      "name": "no_ufffd",
      "type": "contingency",
      "status": "fail",
      "evidence": {
        "matches": ["src/auth.rs:42"],
        "artifact": "iter3/ufffd.txt"
      }
    }
  ],
  "all_passed": false,
  "failed_checks": ["no_ufffd"],

  "total_elapsed_ms": 135000,
  "total_cost_usd": 0.15,

  "outcome": null
}
```

### 7.3 `final-report.json`（含 outcome 回填）

```json
{
  "goal_id": "goal_xxx",
  "final_status": "complete",
  "total_iterations": 7,
  "total_duration_ms": 320000,
  "total_cost_usd": 0.42,
  "stopped_reason": "goal_completed",
  "guards_hit_summary": {
    "low_confidence": 2,
    "repetitive": 1
  },

  "outcome": "fixed",
  "outcome_detail": {
    "fixed_at_iter": 7,
    "final_failed_checks": [],
    "key_breakthrough": "iter5 换角度后用 grep 定位到 U+FFFD"
  }
}
```

### 7.4 outcome 回填机制（关键设计）

- **即时字段**（guards_hit / checks_run）：迭代当下写
- **outcome 字段**（led_to_fix / 最终 status）：goal 结束后回填

让进化 agent 能关联"当时这么干 → 后来结果如何"，不是凭感觉调参。

---

## 8. 进化系统（重点）

### 8.1 架构

```
┌─────────────────────────────────────────────────────┐
│  Goal Evolver Agent（后台静默运转，用户无感）         │
│                                                       │
│  触发：积累一定时间（每 24h 或每 10 个 goal）跑一次    │
│  输入：~/.ion/agent/goal-runs/ 全部日志               │
│                                                       │
│  分析维度（3 个问题）：                                │
│   1. 死循环风险：guards_hit 频率 + 重复模式            │
│   2. 模型选择：check.model vs outcome 相关性           │
│   3. 上下文充分性：context_snapshot 缺失项             │
│                                                       │
│  输出（唯一出口）：给主仓库提 GitHub Issue             │
└─────────────────────────────────────────────────────┘
```

### 8.2 触发方式（双模式）

**模式 A：生产模式（积累一定时间跑一次）**

不是每天固定 cron，按积累触发：
- 默认每 24h 或每积累 10 个 goal（哪个先到跑哪个）
- 基于 MonitorExtension 的 interval 能力
- 跑完默默提 Issue，不打扰用户（不推通知）

**模式 B：快速进化验证（手动触发，秒级出结果）**

生产模式的问题：要等积累 + 24h 才看到进化效果，开发期不可接受。因此提供手动触发入口，用**造好的数据集**秒级验证进化逻辑：

```bash
# 用造好的日志数据集，秒级验证 evolver 的分析逻辑 + Issue 提交
ion rpc --method goal_evolver_run_once \
  --params '{"data_dir": "tests/fixtures/goal-runs/", "dry_run": true}'
```

- `data_dir`：指向测试 fixture（手工构造的 iterations.jsonl，覆盖各种场景）
- `dry_run: true`：只输出"会提什么 Issue"，不真提（CI 用）
- `dry_run: false`：真提 Issue 到测试仓库（端到端验证用）

**为什么需要快速进化验证**：
- 进化逻辑（3 个分析维度）本身是复杂代码，必须有单测覆盖各种日志形态
- 不能等 24h 定时器才知道分析逻辑对不对
- fixture 数据集可手工构造极端场景（死循环/模型错/上下文缺），确定性验证

### 8.3 Goal Evolver Agent 定义（`examples/agents/goal-evolver.md`）

```markdown
# Goal Evolver Agent

你是 Goal Supervisor 的进化引擎。后台分析 goal 运行日志，发现问题并提 Issue。

## 分析任务

读取最近的 goal 运行日志（~/.ion/agent/goal-runs/），针对 3 个维度分析：

### Q1 死循环风险
- 找 repetitive=true 且最终 exhausted 的 goal
- 分析：是哪个检测项反复 FAIL 修不好？
- 判断：检测项太严？skill 没给对方向？还是 agent 能力不够？
- **日志是否够诊断**：如果某个场景无法从日志判断原因 → 提 Issue 标注
  "日志缺失 X 字段，无法诊断 Y 场景"，要求 goal-supervisor 补充日志

### Q2 模型选择是否错误
- 按 model 分组统计 outcome
  - generate_checks 用 model A，后续 PASS 率多少？
  - analyze_failure 用 model B，led_to_fix 率多少？
- 找出：用了太弱的模型导致检测质量差的案例
- **改进**：调整 generate_checks_model 建议，或提 Issue 要求换模型

### Q3 上下文充分性
- 找 confidence 低 / outcome=abandoned 的迭代
- 看 context_snapshot 缺了什么（test_results_included=false 等）
- **严重缺失 → 提 Issue**：标注"context 缺少 Y，导致检测失败"

## 输出动作（唯一出口）

只做一件事：**给主仓库提 GitHub Issue**。

Issue 格式：
- 标题：`[goal-evolver] <问题摘要>`
- 正文含：日志证据（goal_id + iter 号）+ 分析结论 + 改进建议
- 标签：`goal-evolver`、`bug`/`enhancement`

不直接改代码、不改 config、不推通知——只提 Issue。
```

### 8.4 进化闭环

```
Goal 跑（产生日志）
     ↓
goal-evolver 积累一定时间跑一次
     ↓
默默分析全部日志
     ↓
给主仓库提 Issue（带日志证据）
     ↓
开发者 / A→B 自进化看 Issue 改代码
     ↓
新版本 Goal 跑 → 产生新日志 → 回到顶部
```

**关键**：每一轮进化基于真实 outcome 数据，Issue 是唯一出口。

### 8.5 进化系统能改什么、不能改什么

| 改动 | 谁改 | 机制 |
|------|------|------|
| 阈值（max_iter/confidence/delay） | 开发者看 Issue 后改 config | 人工 |
| check prompt 模板 | 开发者看 Issue 后改 skill 文件 | 人工 |
| model tier 选择 | 开发者看 Issue 后改 config | 人工 |
| 检测项生成启发式（skill 逻辑） | 开发者看 Issue 后改 skill | 人工 / A→B 自进化 |
| 闭环本身（状态机/防线/日志字段） | 开发者看 Issue 后改 Rust 代码 | 人工 / A→B 自进化 |

**进化 agent 本身只提 Issue，不直接改任何东西**——所有改动都走 Issue → 人工/A→B 评审 → 落地。符合"底层干净"原则。

---

## 9. ExtensionApi 依赖检查（零内核改动）

| 能力 | ION 现状 | 用途 |
|------|---------|------|
| `on_agent_end` 钩子 | ✅ `src/agent/extension.rs:113` | 触发检测 |
| `ApiRegistry::complete` | ✅ `ion_provider::registry::complete` | 调 LLM 生成检测项/分析失败 |
| resume / 注入 message | ✅ resume_worker 工具 + steer 机制 | continue 闭环 |
| `emit_extension_event` | ✅ `src/worker_api.rs:256` | 推送状态变化 |
| 文件读写 | ✅ ExtensionApi.fs / ctx.fs | 日志落盘 |
| 定时触发（evolver 用） | ✅ MonitorExtension interval | 进化 agent 定时跑 |

**结论：零内核改动**。Goal Supervisor + Goal Evolver 全部基于现有 ExtensionApi。

---

## 10. 落地计划

### Phase 1：Goal Supervisor 扩展核心

- `goal_set` tool
- 状态机（running → checking → continuing → complete/exhausted/blocked）
- 6 道防线
- run_all_checks（确定性执行检测项 + 收证据）
- 基础日志（iterations.jsonl）

### Phase 2：Skill + 完整日志

- GOAL skill（通用检测项生成）
- rust-ci skill（ION 自带，开箱即用）
- outcome 回填机制
- final-report.json

### Phase 3：Goal Evolver Agent

- `examples/agents/goal-evolver.md`
- 基于 MonitorExtension 定时触发
- 默默扫日志 → 提主仓库 Issue
- 进化报告存档（~/.ion/agent/goal-evolver-reports/）

---

## 11. 验证体系（两层，缺一不可）

> 遵守 AGENTS.md 测试规范：Harness 验证（FauxProvider，零 API 成本，确定性）+ 快速进化验证（fixture 数据集，秒级）。真实 LLM case 最后补。

### 11.1 第一层：Harness 验证（场景 2，FauxProvider 驱动）

**目标**：不调真 LLM，用 FauxProvider Factory 驱动 `ion --host`，验证 goal 闭环真的转起来。

**为什么用场景 2**：
- 场景 1 进程跑完即退，闭环没机会转（goal 在场景 1 本就不工作，是设计预期）
- 场景 2 有 host，agent_end 后能注入 continue，闭环能完整跑完
- 场景 2 不需要 socket，测试脚本简单（起 host → RPC → 断言）

**核心 harness 场景**：

| Group | 场景 | FauxProvider 行为 | 验证点 |
|-------|------|-------------------|--------|
| **A：基础闭环** | A1 set→检测全 PASS→complete | Factory: 第 1 轮 agent 干活调 bash 写文件，检测项是 `test -f` | ✅ goal 状态 running→checking→complete；✅ final-report outcome=fixed |
| | A2 set→有 FAIL→continue→PASS→complete | Factory: 第 1 轮写一半，第 2 轮（收到 continue）补完 | ✅ 跑了 2 轮；✅ iter1 FAIL iter2 PASS；✅ 最终 complete |
| **B：防线触发** | B1 max_iter 到顶→exhausted | Factory: 每轮都写错，永远 FAIL | ✅ 跑满 max_iter 后停止；✅ final_status=exhausted |
| | B2 repetitive 检测→换策略 | Factory: 连续 3 轮 actionPlan 相似 | ✅ 第 4 轮注入"换角度"prompt；✅ guards_hit.repetitive=true |
| | B3 时长上限触发 | 构造慢检测命令 + 调小 max_duration | ✅ 超时停止；✅ stopped_reason=max_duration |
| | B4 成本上限触发 | 调小 max_cost + 构造高 token 消耗 | ✅ 超成本停止 |
| **C：证据落盘** | C1 检测项 PASS 有 artifact | 检测项 `cargo test`（mock 成功） | ✅ artifact 文件存在；✅ iterations.jsonl 有 evidence |
| | C2 无 evidence 视为 FAIL | 检测项 command 故意不产出 artifact | ✅ status=fail；✅ reason="no evidence" |
| **D：日志完整性** | D1 iterations.jsonl schema 全字段 | 跑一轮后读日志 | ✅ 含 guards_hit/llm_calls/context_snapshot/checks_run/outcome |
| | D2 outcome 回填 | goal 结束后读 final-report | ✅ outcome 非 null；✅ 含 fixed_at_iter |
| **E：tool 行为** | E1 goal_set 覆盖语义 | 连续调 2 次 goal_set | ✅ 第 2 次覆盖第 1 次；✅ 旧 goal 状态 cancelled |
| | E2 goal_set 不传 checks→skill 生成 | 只传 objective | ✅ 内部调 skill 生成检测项；✅ 日志记录 generated_by=skill |

**Harness 脚本**：`tests/goal_supervisor_ci.sh`（参照 `tests/file_snapshot_ci.sh` 格式），起 host + FauxProvider + 断言。

**FauxProvider Factory 关键**（用 Factory 不用 Static，因为要根据上下文分支）：

```rust
// 伪代码：A2 场景的 Factory
let faux = FauxProvider::new();
faux.push_factory(move |context, _opts, _state, _model| {
    let messages = &context.messages;
    // 第 1 轮：写一半（触发 FAIL）
    if !messages.iter().any(|m| m.contains("continue: no_ufffd failed")) {
        return faux_tool_call("bash", r#"write_file("src/auth.rs", "半成品")"#);
    }
    // 第 2 轮（收到 continue 注入）：补完
    faux_tool_call("bash", r#"write_file("src/auth.rs", "完整内容")"#)
});
```

### 11.2 第二层：快速进化验证（fixture 数据集，秒级）

**目标**：验证 goal-evolver 的分析逻辑（3 个维度）+ Issue 提交，不依赖真实 goal 运行。

**为什么单独一层**：
- 进化逻辑是复杂代码，各种日志形态都要覆盖
- 不能等 24h 定时器才知道分析对不对
- 手工构造 fixture 可覆盖极端场景（死循环/模型错/上下文缺），确定性

**fixture 数据集**（`tests/fixtures/goal-runs/`）：

```
tests/fixtures/goal-runs/
  ├── case_deadloop/          # Q1：死循环案例
  │   └── iterations.jsonl    # 构造 repetitive=true + exhausted
  ├── case_wrong_model/       # Q2：模型选错
  │   └── iterations.jsonl    # 构造弱模型 + 低 PASS 率
  ├── case_missing_context/   # Q3：上下文缺失
  │   └── iterations.jsonl    # 构造 test_results_included=false
  └── case_healthy/           # 健康 case（不应提 Issue）
      └── iterations.jsonl
```

**验证 RPC**：

```bash
# dry_run：只输出分析结论 + 拟提的 Issue，不真提（CI 用）
ion rpc --method goal_evolver_run_once \
  --params '{"data_dir": "tests/fixtures/goal-runs/case_deadloop/", "dry_run": true}'

# 期望输出：
# {
#   "analyzed_goals": 1,
#   "issues_planned": [
#     {
#       "title": "[goal-evolver] 检测项 no_ufffd 反复 FAIL 修不好",
#       "dimension": "deadloop",
#       "evidence": {"goal_id": "...", "iters": [3,4,5]},
#       "suggestion": "检测项太严或 skill 缺失修复方向"
#     }
#   ]
# }
```

**进化验证场景**：

| Group | 场景 | fixture | 验证点 |
|-------|------|---------|--------|
| **F：分析逻辑** | F1 死循环识别 | case_deadloop | ✅ issues_planned 含 deadloop 维度；✅ 证据指向反复 FAIL 的检测项 |
| | F2 模型选错识别 | case_wrong_model | ✅ issues_planned 含 model 维度；✅ 指出弱模型 + 低 PASS 率 |
| | F3 上下文缺失识别 | case_missing_context | ✅ issues_planned 含 context 维度；✅ 指出缺 test_results |
| | F4 健康 case 不误报 | case_healthy | ✅ issues_planned 为空 |
| **G：Issue 提交** | G1 dry_run 不真提 | 任意 fixture + dry_run=true | ✅ 只输出计划，不调 gh |
| | G2 真提到测试仓库 | 任意 fixture + dry_run=false + test repo | ✅ gh issue create 被调用；✅ Issue 含日志证据 |

**进化验证脚本**：`tests/goal_evolver_ci.sh`，纯 RPC + 断言，秒级跑完。

### 11.3 第三层：真实 LLM case（最后补，`ION_E2E=1`）

> Phase 3 完成后补。用真实模型（glm-5.2 执行 + deepseek-v4-flash 检测）跑一个真实 goal，验证：
> - 检测项生成质量（LLM 真能生成合理的 CI + 意外检测）
> - 闭环在真实 LLM 下的表现（不是 mock 的确定性）
> - 标 `#[ignore]` + `ION_E2E=1` 触发

### 11.4 验证脚本登记（实现后填入 AGENTS.md 测试统计表）

| 脚本 | 数量 | 覆盖 |
|------|------|------|
| `goal_supervisor_ci` | ~15 | Group A-E：基础闭环/防线/证据/日志/tool |
| `goal_evolver_ci` | ~6 | Group F-G：分析逻辑/Issue 提交 |

---

## 12. 对比 pi

| | pi session-supervisor | ION Goal Supervisor |
|---|---|---|
| 完成判定 | LLM reassessment（软） | 证据驱动（硬） |
| 暴露给 LLM 的 tool | 多个（setGoal/clearGoal/refineGoal/...） | 1 个（goal_set） |
| 死循环防护 | 5 道（次数/置信度/重复/换策略/delay） | 6 道（+时长/成本上限） |
| 进化系统 | 无 | goal-evolver agent 提 Issue |
| 会幻觉吗 | 会 | 不会（证据驱动） |
| 归属 | 扩展 | 扩展（同） |

---

## 13. 深化：进展分析 + 目标校正 + 诊断 agent

> **状态：已完成** — 4 项深化全部实现，928 lib tests + 真实 LLM 验证通过。

B1-B3 实现了"完成判定"（硬判定）。深化增加了**"进展好不好"**（软分析）+ **"不好怎么办"**（校正 + 诊断）。

### 13.1 进展分析（ProgressReport）

每次 `on_gate_check` 触发 RetryWith 前，自动运行 `analyze_progress()`：

| Trend | 触发条件 | 给 agent 的建议 |
|-------|---------|---------------|
| **Converging** | failed 集合在缩小 | "继续，进展良好" |
| **Oscillating** | failed 数量不变但元素变化 | "考虑调 goal_refine 调整检测项" |
| **Stagnant** | failed 完全相同连续多轮 | "换策略或放宽检测" |
| **Drifting** | action plan 跟 objective 相似度 < 0.15 | "重新聚焦目标" |

**Drifting 有两个检测维度**：
1. **语义偏离**：action plan 文本跟 objective 关键词重叠低（Jaccard < 0.15）
2. **工具行为偏离**（Task 4）：连续 5+ 次只 bash/read 不 write/edit → "你在探索但不实现"

分析结果附加到 RetryWith 消息：
```
Goal not complete. Failed: [cargo_test, no_ufffd]
📊 Progress: Oscillating. Different checks keep failing each iteration.
   Consider calling goal_refine to adjust or split the checks.
Fix the failing checks.
```

### 13.2 目标校正（goal_refine tool）

`goal_refine` — 增量调整运行中的目标，不清零进展：

| 参数 | 作用 |
|------|------|
| `objective_patch` | 更新目标描述 |
| `checks_add` | 添加检测项 |
| `checks_remove` | 按 name 删除检测项 |

**跟 goal_set 区别**：
- `goal_set` = 覆盖整个 goal（清零 iteration_count/cost）
- `goal_refine` = 增量改（保留 iteration_count/cost/started_at）

典型流程：agent 看到 Oscillating 建议 → 调 goal_refine 删掉太严的检测项 → 继续执行。

### 13.3 诊断 agent（goal_diagnose tool + goal-diagnostician）

`goal_diagnose` — 当目标严重卡住时，spawn 一个专家 agent 深度诊断：

```
goal_diagnose → spawn_worker(agent="goal-diagnostician", task=打包上下文)
  → diagnostician 读日志 → 分析 3 维度：
    1. 检测项质量：太严？测错东西？
    2. agent 能力：用对工具？重复？
    3. 目标可行性：太大？太模糊？
  → 返回 ROOT CAUSE + RECOMMENDATION
```

诊断 agent 定义在 `examples/agents/goal-diagnostician.md`（read-only，不 edit/write/spawn）。

### 13.4 偏离监控（on_tool_execution_end）

在 GoalState 增加 `recent_tools` 字段（滑动窗口 K=10），记录每次工具调用的 `(tool_name, target_summary)`。

`on_tool_execution_end` 钩子自动记录。`analyze_progress` 检查 recent_tools：连续 5+ 次只 bash/read 无 write/edit → 标记 Drifting。

### 13.5 暴露给 LLM 的 3 个 tool

| Tool | 作用 | 何时用 |
|------|------|--------|
| `goal_set` | 设目标（覆盖 + B2 自动生成检测项） | 开始新目标 |
| `goal_refine` | 增量调整（加/删检测项，改 objective） | 进展分析建议调整时 |
| `goal_diagnose` | spawn 诊断 agent 深度分析 | 严重卡住时 |

### 13.6 验证

- **单元测试**：ProgressTrend 各分支（Converging/Oscillating/Stagnant/Drifting）+ GoalRefineTool（add/remove/patch/error）= 9 新测试
- **真实 LLM**：GLM-5.2 跑 goal_set（无 checks → B2 自动生成 → 全 PASS → complete）
- **全量回归**：928 lib tests，0 回归

