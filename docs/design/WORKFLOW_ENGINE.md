# Workflow Engine — 结构化交付流水线

> **状态：设计稿** — DSL 语法 + 字段表 + 执行流程 + CI Group 定义。
> 实现待启动。本文档是唯一权威定义。

---

## 目录

1. [设计理念](#一设计理念)
2. [DSL 语法](#二dsl-语法)
3. [字段定义](#三字段定义)
4. [条件表达式](#四条件表达式-if)
5. [上下文传递](#五上下文传递-context)
6. [执行流程](#六执行流程)
7. [持久化与恢复](#七持久化与恢复)
8. [CLI 接口](#八cli-接口)
9. [与现有架构的关系](#九与现有架构的关系)
10. [CI 测试 Group](#十ci-测试-group)
11. [业界对比](#十一业界对比)

---

## 一、设计理念

### 问题

现在的 orchestrator.md 把"6 阶段交付流程"写在提示词散文里。问题：

- **上下文丢失**：LLM 长对话后可能忘记自己该干什么
- **不可校验**：提示词写错了没有语法检查
- **不可恢复**：session 断了无法知道执行到哪一步
- **不可复用**：换项目要重新写提示词

### 方案

把流程定义从提示词变成**结构化 YAML 文件**（workflow.yaml）：

- **定义即状态**：YAML 文件既是流程定义又是运行时状态（每个 stage 有 `status` 字段）
- **持久化**：文件在磁盘上，上下文丢一万次也不怕
- **可校验**：`ion workflow validate` 检查语法
- **可复用**：复制 YAML 到别的项目就能用
- **Agent 可自写**：LLM 用 write/edit 工具创建和修改 workflow.yaml

### 一句话

> workflow.yaml 是流程的"源代码"——定义、执行、校验、恢复全靠这一个文件。

---

## 二、DSL 语法

### 完整示例

```yaml
name: delivery

# ── 全局上下文（所有 stage 可读可写）──
context:
  project_name: "my-app"
  files: ""
  needs_publish: true

# ── Stages（按顺序执行）──
stages:
  - id: spec
    agent: coordinator
    task: "分析需求，输出文件清单"
    outputs:
      files: "stage_output"
    gate:
      command: "echo '{{context.files}}' | grep -q '.' && echo HAS_SPEC || echo EMPTY"
      expected: HAS_SPEC

  - id: develop
    agent: developer
    task: "创建文件：{{context.files}}"
    worktree: true
    if: "stages.spec.status == 'done'"
    gate:
      command: "ls {{context.files}} && echo EXISTS"
      expected: EXISTS
      max_retries: 3
    on_fail:
      loop_back: develop
      max_loops: 3
    cleanup:
      on_success: true
      on_failure: false

  - id: review
    agent: reviewer
    task: "审查：{{context.files}}"
    if: "stages.develop.status == 'done'"
    outputs:
      verdict: "stage_output"
    gate:
      command: "echo '{{context.verdict}}' | grep -q APPROVE && echo OK || echo CHANGES"
      expected: OK
    on_fail:
      loop_back: develop
      max_loops: 3

  - id: merge
    agent: merger
    task: "合并到 master"
    if: "stages.review.status == 'done'"

  - id: publish
    agent: publisher
    task: "推送到 GitHub"
    if: "stages.merge.status == 'done' && context.needs_publish == true"

  - id: verify
    agent: coordinator
    task: "验证所有验收标准"
    gate:
      command: "python3 -m pytest 2>&1 | grep -q passed && echo PASS || echo FAIL"
      expected: PASS

  - id: cleanup
    if: "always"
    commands:
      - "git worktree prune"
      - "git branch -d $(git branch --list 'ion-worker-*') 2>/dev/null || true"
```

### 语法规则

1. **YAML 格式**：标准 YAML，支持注释、多行字符串、引号
2. **模板变量**：`{{context.xxx}}` 和 `{{stages.X.status}}` 在执行前替换
3. **stage 顺序**：`stages` 数组顺序 = 默认执行顺序（除非 `if` 跳过）
4. **`commands` vs `agent`**：二选一。`commands` 直接跑 bash 不 spawn agent

---

## 三、字段定义

### 顶层字段

| 字段 | 类型 | 必填 | 默认 | 说明 |
|------|------|:----:|:----:|------|
| `name` | string | ✅ | — | workflow 名称 |
| `context` | map | ❌ | `{}` | 全局上下文（键值对） |
| `stages` | list | ✅ | — | stage 列表 |
| `defaults` | object | ❌ | — | 全局默认值（见下） |

### `defaults` 字段（可选，覆盖各 stage 的默认值）

```yaml
defaults:
  max_retries: 3
  max_loops: 3
  cleanup_on_success: true
  cleanup_on_failure: false
```

### Stage 字段

| 字段 | 类型 | 必填 | 默认 | 说明 |
|------|------|:----:|:----:|------|
| `id` | string | ✅ | — | stage 唯一标识（用于 `if`/`loop_back`/`outputs` 引用） |
| `agent` | string | ⚠️ | — | spawn 的 agent 名称（与 `commands` 二选一） |
| `task` | string | ⚠️ | — | 传给 agent 的任务描述（支持 `{{context.xxx}}`） |
| `commands` | list | ⚠️ | — | 直接执行的 bash 命令列表（与 `agent` 二选一） |
| `worktree` | bool | ❌ | false | 是否在 git worktree 隔离执行 |
| `if` | string | ❌ | — | 条件表达式（见第四节） |
| `outputs` | map | ❌ | — | 输出映射（见第五节） |
| `gate` | object | ❌ | — | gate 校验（见下） |
| `on_fail` | object | ❌ | — | 失败处理（见下） |
| `cleanup` | object | ❌ | — | worktree 清理策略（见下） |
| `status` | string | ❌ | `pending` | **运行时状态**（不由用户填写，由引擎写入） |

> ⚠️ `agent` + `task` 和 `commands` 必须二选一。

### `gate` 子字段

| 字段 | 类型 | 必填 | 默认 | 说明 |
|------|------|:----:|:----:|------|
| `command` | string | ✅ | — | bash 命令（支持 `{{context.xxx}}` 模板变量） |
| `expected` | string | ❌ | `PASS` | 期望命令输出包含的字符串 |
| `max_retries` | int | ❌ | 3 | gate 失败后重试次数 |

### `on_fail` 子字段

| 字段 | 类型 | 必填 | 默认 | 说明 |
|------|------|:----:|:----:|------|
| `loop_back` | string | ✅ | — | gate 失败后回到哪个 stage（stage id） |
| `max_loops` | int | ❌ | 3 | loop_back 最大次数 |

### `cleanup` 子字段

| 字段 | 类型 | 必填 | 默认 | 说明 |
|------|------|:----:|:----:|------|
| `on_success` | bool | ❌ | true | stage 成功后清理 worktree |
| `on_failure` | bool | ❌ | false | stage 失败后清理 worktree |

### `outputs` 子字段

| 写法 | 含义 |
|------|------|
| `files: "stage_output"` | 把 agent 的完整输出存到 `context.files` |
| `verdict: "stage_output"` | 把 agent 的完整输出存到 `context.verdict` |

> 后续可扩展：`files: { from: "stage_output", extract: "regex pattern" }` 提取特定内容。

### `status` 运行时值

| 值 | 含义 |
|----|------|
| `pending` | 还没执行 |
| `running` | 正在执行 |
| `done` | gate 通过 |
| `failed` | gate 失败且重试耗尽 |
| `skipped` | `if` 条件不满足，跳过 |

---

## 四、条件表达式 (`if`)

### 语法

```yaml
if: "<表达式>"
```

表达式在 stage 执行前求值。结果为 `true` → 执行；`false` → 跳过（status=skipped）。

### 支持的表达式

| 表达式 | 含义 | 示例 |
|--------|------|------|
| `stages.X.status == 'done'` | 上游 stage X 已完成 | `stages.spec.status == 'done'` |
| `stages.X.status == 'failed'` | 上游 stage X 失败 | `stages.review.status == 'failed'` |
| `context.xxx == 'value'` | 上下文字段等于某值 | `context.needs_publish == 'true'` |
| `context.xxx == true` | 布尔上下文为 true | `context.needs_review == true` |
| `always` | 无条件执行 | 用于 cleanup stage |
| `A && B` | 逻辑与 | `stages.merge.status == 'done' && context.needs_publish == true` |
| `A \|\| B` | 逻辑或 | `stages.review.status == 'done' \|\| context.skip_review == true` |

### 求值规则

1. 先替换所有 `{{...}}` 模板变量
2. 再求值布尔表达式
3. `always` 是保留字，直接返回 true
4. 未定义的 `if` = 无条件执行（等同 `always`）

---

## 五、上下文传递 (`context`)

### 机制

```
context（全局键值对）
  ↑                    ↓
  │ stage outputs 写入  │ stage task/gate 读取
  │                    │
  └────────────────────┘
```

### 初始化

```yaml
context:
  project_name: "my-app"
  files: ""
  needs_publish: true
```

workflow 启动时从 YAML 读入。

### 写入（stage outputs）

```yaml
- id: spec
  outputs:
    files: "stage_output"     # agent 的输出文本 → context.files
```

stage 执行完成后，引擎把 agent 的完整输出存到 `context.files`。

### 读取（模板变量）

```yaml
- id: develop
  task: "创建文件：{{context.files}}"
  gate:
    command: "ls {{context.files}} && echo EXISTS"
```

执行前，`{{context.files}}` 被替换为实际值。

### 引用其他 stage 的状态

```yaml
- id: merge
  if: "stages.develop.status == 'done'"
```

`stages.X.status` 引用 stage X 的运行时状态。

### 引用其他 stage 的输出

```yaml
- id: review
  if: "stages.spec.outputs.files != ''"
```

> 注意：`outputs` 写入 context 后，`context.files` 和 `stages.spec.outputs.files` 都能引用。

---

## 六、执行流程

### 引擎循环（wf agent 的核心逻辑）

```
wf agent 启动
  │
  ├─ 1. read workflow.yaml
  │
  ├─ 2. for each stage in stages:
  │      │
  │      ├─ a. 求值 if 条件
  │      │    ├─ false → status=skipped → 下一个
  │      │    └─ true/无 if → 继续
  │      │
  │      ├─ b. status=running → write yaml
  │      │
  │      ├─ c. 执行 stage:
  │      │    ├─ 有 agent → spawn_worker(agent, task, worktree, wait=true)
  │      │    └─ 有 commands → 逐条 bash -c
  │      │
  │      ├─ d. gate 检查:
  │      │    ├─ 有 gate → bash -c gate.command → 检查 expected
  │      │    │    ├─ 通过 → status=done → write yaml → 继续
  │      │    │    └─ 不通过 → retry（max_retries 次）
  │      │    │         ├─ 重试中 → 回到 c
  │      │    │         └─ 重试耗尽 → status=failed
  │      │    │              ├─ 有 on_fail → loop_back → 回到 loop_back stage
  │      │    │              └─ 无 on_fail → PIPELINE ABORTED
  │      │    └─ 无 gate → status=done → 继续
  │      │
  │      ├─ e. outputs 写入 context
  │      │
  │      └─ f. cleanup:
  │           ├─ status=done + cleanup.on_success → spawn merger remove worktree
  │           └─ status=failed + cleanup.on_failure → spawn merger remove worktree
  │
  └─ 3. 全部 done → PIPELINE COMPLETE → write yaml (all status=done)
```

### loop_back 语义

```
Stage 3 (review) gate 失败 → on_fail.loop_back: develop
  → 回到 Stage 2 (develop)
  → develop 重新执行（status 重置为 pending）
  → 完成后再到 review
  → 如果 review 又失败，再 loop_back
  → 达到 max_loops (3) → PIPELINE ABORTED
```

### 并行（未来扩展）

当前版本只支持串行。未来可加：

```yaml
- id: develop
  parallel:
    - task: "创建 a.py"
    - task: "创建 b.py"
  max_concurrency: 3
```

---

## 七、持久化与恢复

### workflow.yaml 既是定义又是状态

每次 stage 状态变化，引擎**写回** workflow.yaml：

```yaml
# 执行到一半的 workflow.yaml
stages:
  - id: spec
    status: done          # ← 已完成
  - id: develop
    status: done          # ← 已完成
  - id: review
    status: failed        # ← 刚失败
  - id: merge
    status: pending       # ← 还没执行
```

### 恢复流程

```bash
# session 中断后重启
ion --host --agent wf "继续执行 workflow"
# wf agent 读 workflow.yaml
# → 发现 spec=done, develop=done, review=failed
# → 从 review 开始重试（或 loop_back 到 develop）
```

### Agent 自写 workflow

```
用户: "创建一个 calculator 项目推送到 GitHub"
     ↓
wf agent:
  1. 分析需求
  2. write workflow.yaml（自己创建流程定义）
  3. read workflow.yaml → 执行
  4. 如果 gate 一直失败 → edit workflow.yaml（调整 gate 或加 stage）
  5. 再读 → 再执行
```

---

## 八、CLI 接口

### `ion workflow validate <path>`

校验 workflow.yaml 语法。

```bash
$ ion workflow validate .ion/workflow.yaml
✅ Valid workflow: 7 stages, 5 gates
```

```bash
$ ion workflow validate .ion/workflow.yaml
❌ Error: stage 'review' has on_fail.loop_back='develop' but stage 'develop' has on_fail.max_loops=0 (must be > 0)
```

### `ion workflow run <path>`

启动 workflow 执行。

```bash
$ ion workflow run .ion/workflow.yaml
[workflow] Starting: delivery (7 stages)
[workflow] Stage 1/7: spec... done ✅
[workflow] Stage 2/7: develop... done ✅
[workflow] Stage 3/7: review... CHANGES_REQUESTED → loop_back develop (1/3)
[workflow] Stage 2/7: develop (retry)... done ✅
[workflow] Stage 3/7: review... APPROVE → done ✅
[workflow] Stage 4/7: merge... done ✅
[workflow] Stage 5/7: publish... done ✅
[workflow] Stage 6/7: verify... done ✅
[workflow] Stage 7/7: cleanup... done ✅
[workflow] PIPELINE COMPLETE
```

### `ion workflow status <path>`

查看当前执行状态。

```bash
$ ion workflow status .ion/workflow.yaml
Workflow: delivery
  spec:     done ✅
  develop:  done ✅
  review:   failed ❌ (2/3 loops)
  merge:    pending ⏳
  publish:  pending ⏳
  verify:   pending ⏳
  cleanup:  pending ⏳
Next action: retry review (loop_back to develop)
```

---

## 九、与现有架构的关系

### 分层

```
用户层:
  ion workflow validate/run/status   ← CLI 命令

策略层:
  workflow.yaml                      ← 流程定义 + 运行时状态
  wf.md (agent)                      ← 执行引擎（读 yaml → spawn → gate → write）

内核层（已有，不改）:
  GateDecision + on_gate_check       ← gate 校验原语
  spawn_worker / await_worker        ← worker 编排原语
  WorktreeConfig + remove_worktree   ← worktree 隔离原语
```

### 与现有组件的关系

| 现有组件 | 关系 |
|:---------|:-----|
| `orchestrator.md` | workflow.yaml 是它的结构化替代。orchestrator.md 保留为"无 yaml 时的 fallback" |
| `GateCheckExtension`（原 WorkflowExtension） | stage gate 的**单 stage 级**校验。workflow.yaml 的 gate 是**跨 stage 级**的。两者互补 |
| `spawn_worker` 工具 | wf agent 用它 spawn 各个 specialist agent |
| `merger.md` | cleanup stage 复用 merger 的 worktree 清理逻辑 |
| `follow_up_queue` | wf agent 的续跑机制（stage 间传递） |

### 不改的东西

- **不改内核**：GateDecision / spawn_worker / worktree 全部保留
- **不改 agent .md 格式**：frontmatter 不变
- **不强制使用**：不想用 workflow 的用户直接用 `--agent coordinator` 就行

---

## 十、CI 测试 Group

### Group W1: DSL 校验

| # | 测试 | 验证点 |
|---|------|--------|
| W1-1 | 合法 workflow.yaml 通过校验 | 语法正确 → ✅ |
| W1-2 | 缺少必填字段（id/agent/task） | 报错 + 指出缺失字段 |
| W1-3 | loop_back 指向不存在的 stage id | 报错 + 指出不存在的 id |
| W1-4 | if 表达式语法错误 | 报错 + 指出错误位置 |
| W1-5 | agent 和 commands 同时存在 | 报错（互斥） |

### Group W2: 单 stage 执行

| # | 测试 | 验证点 |
|---|------|--------|
| W2-1 | develop → gate 通过 | status=done ✅ |
| W2-2 | develop → gate 失败 → loop_back → 重试通过 | 最终 status=done |
| W2-3 | develop → gate 超限 | status=failed → PIPELINE ABORTED |

### Group W3: 条件分支

| # | 测试 | 验证点 |
|---|------|--------|
| W3-1 | if 条件为 true | stage 执行 |
| W3-2 | if 条件为 false | stage 跳过（status=skipped） |
| W3-3 | if: always | 无论上游成败都执行 |
| W3-4 | if 复合条件 (A && B) | 两个条件都满足才执行 |

### Group W4: 上下文传递

| # | 测试 | 验证点 |
|---|------|--------|
| W4-1 | spec outputs → develop 引用 {{context.files}} | 值正确传递 |
| W4-2 | context 初始值 → stage 里引用 | 初始值可用 |
| W4-3 | stage outputs 写入 → 下游 stage 读取 | 写入后可读 |

### Group W5: 多 stage 串联

| # | 测试 | 验证点 |
|---|------|--------|
| W5-1 | spec → develop → merge → 全部 done | 串行执行完成 |
| W5-2 | review 失败 → loop_back develop → 修复后 review 通过 | 回退循环成功 |

### Group W6: cleanup

| # | 测试 | 验证点 |
|---|------|--------|
| W6-1 | stage 成功 + cleanup.on_success=true | worktree 清理 |
| W6-2 | stage 失败 + cleanup.on_failure=false | worktree 保留 |
| W6-3 | cleanup stage (if: always) | 无论成败都执行 |

### Group W7: 持久化

| # | 测试 | 验证点 |
|---|------|--------|
| W7-1 | 执行到 stage 3 中断 → 重读 yaml → 从 stage 3 继续 | 状态恢复正确 |
| W7-2 | 全部 done → workflow.yaml 所有 status=done | 最终状态持久化 |

---

## 十一、业界对比

| 能力 | GitHub Actions | GitLab CI | pi | Claude Code | **ION workflow.yaml** |
|------|:---:|:---:|:---:|:---:|:---:|
| stages 分组 | ❌ (用 needs) | ✅ | ❌ | ❌ | ✅ |
| 条件分支 (`if`) | ✅ (`${{ }}`) | ✅ (`when:`) | ❌ | ❌ | ✅ |
| 上下文传递 | ✅ (outputs) | ❌ | ❌ | ❌ | ✅ (`context:` + `outputs:`) |
| gate 校验 | ❌ | ❌ | ✅ (工具级) | ❌ | ✅ (stage 级 + 内核强制) |
| 失败回退 (loop_back) | ❌ | ❌ | ❌ | ❌ | ✅ (**ION 原创**) |
| retry | ❌ (step 级) | ✅ | ❌ | ❌ | ✅ |
| cleanup (always) | ✅ (`if: always()`) | ✅ (`when: always`) | ❌ | ❌ | ✅ (`if: always`) |
| 持久化恢复 | ✅ (re-run) | ✅ (retry) | ❌ | ❌ | ✅ (**yaml 即状态**) |
| Agent 可自写 | ❌ | ❌ | ❌ | ❌ | ✅ (**ION 原创**) |
| DSL 校验 | ✅ | ✅ | ✅ (`hooks validate`) | ❌ | ✅ (`workflow validate`) |
