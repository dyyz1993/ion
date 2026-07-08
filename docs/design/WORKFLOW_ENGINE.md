# Workflow Engine — 结构化交付流水线

> **状态：设计稿** — DSL 语法 + 字段表 + 执行流程 + CLI Group。
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
10. [CLI 测试 Group](#十cli-测试-group)
11. [模板与速查](#十一模板与速查)
12. [业界对比](#十二业界对比)

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

# ── 默认值（各 stage 可覆盖）──
defaults:
  max_retries: 3
  max_loops: 3
  cleanup_on_success: true
  cleanup_on_failure: false

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

  - id: cleanup
    if: "always"
    commands:
      - "git worktree prune"
      - "git branch --list 'ion-worker-*' | xargs -r git branch -D 2>/dev/null || true"
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
| `defaults` | object | ❌ | — | 全局默认值 |

### Stage 字段

| 字段 | 类型 | 必填 | 默认 | 说明 |
|------|------|:----:|:----:|------|
| `id` | string | ✅ | — | 唯一标识（被 if/loop_back 引用） |
| `agent` | string | ⚠️ | — | agent 名称（与 commands 二选一） |
| `task` | string | ⚠️ | — | 任务描述（支持 `{{context.xxx}}`） |
| `commands` | list | ⚠️ | — | bash 命令列表（与 agent 二选一） |
| `worktree` | bool | ❌ | false | git worktree 隔离 |
| `if` | string | ❌ | — | 条件表达式（第四节） |
| `outputs` | map | ❌ | — | 输出到 context（第五节） |
| `gate` | object | ❌ | — | gate 校验 |
| `on_fail` | object | ❌ | — | 失败处理 |
| `cleanup` | object | ❌ | — | worktree 清理 |
| `status` | string | ❌ | `pending` | **运行时**（引擎写入） |

### gate 子字段

| 字段 | 类型 | 默认 | 说明 |
|------|------|:----:|------|
| `command` | string | — | bash 命令（支持模板变量） |
| `expected` | string | `PASS` | 期望输出包含的字符串 |
| `max_retries` | int | 3 | gate 失败重试次数 |

### on_fail 子字段

| 字段 | 类型 | 默认 | 说明 |
|------|------|:----:|------|
| `loop_back` | string | — | 回退到哪个 stage |
| `max_loops` | int | 3 | 回退最大次数 |

### cleanup 子字段

| 字段 | 类型 | 默认 | 说明 |
|------|------|:----:|------|
| `on_success` | bool | true | 成功后清理 worktree |
| `on_failure` | bool | false | 失败后清理 worktree |

### outputs 子字段

| 写法 | 含义 |
|------|------|
| `key: "stage_output"` | agent 输出存到 `context.key` |

### status 运行时值

| 值 | 含义 |
|----|------|
| `pending` | 未执行 |
| `running` | 正在执行 |
| `done` | gate 通过 |
| `failed` | gate 失败且重试耗尽 |
| `skipped` | if 条件不满足 |

---

## 四、条件表达式 (`if`)

| 表达式 | 含义 |
|--------|------|
| `stages.X.status == 'done'` | 上游 stage 已完成 |
| `stages.X.status == 'failed'` | 上游 stage 失败 |
| `context.xxx == 'value'` | 上下文匹配 |
| `context.xxx == true` | 布尔判断 |
| `always` | 无条件（用于 cleanup） |
| `A && B` | 逻辑与 |
| `A \|\| B` | 逻辑或 |

---

## 五、上下文传递 (`context`)

```
context（全局键值对）
  ↑                    ↓
  │ stage outputs 写入  │ task/gate 引用 {{context.xxx}}
  └────────────────────┘
```

- **初始化**：从 YAML `context:` 段读入
- **写入**：stage 的 `outputs:` 把 agent 输出存到 context
- **读取**：`{{context.xxx}}` 在 task/gate/command 里替换
- **跨 stage**：`stages.X.status` 引用其他 stage 状态

---

## 六、执行流程

```
wf agent 启动
  │
  ├─ 1. read workflow.yaml
  │
  ├─ 2. for each stage:
  │      ├─ a. 求值 if → false → skip
  │      ├─ b. status=running → write yaml
  │      ├─ c. spawn agent / run commands
  │      ├─ d. gate check → pass → done / fail → retry
  │      ├─ e. outputs 写入 context
  │      └─ f. cleanup worktree（按配置）
  │
  └─ 3. 全部 done → PIPELINE COMPLETE
```

### loop_back 语义

```
review gate 失败 → on_fail.loop_back: develop
  → 回到 develop（status 重置 pending）
  → 完成后再到 review
  → 达到 max_loops → PIPELINE ABORTED
```

---

## 七、持久化与恢复

workflow.yaml 既是定义又是状态。每次 stage 状态变化，引擎**写回** yaml：

```yaml
# 执行到一半
stages:
  - id: spec
    status: done
  - id: develop
    status: done
  - id: review
    status: failed
  - id: merge
    status: pending    # ← 下次从这里继续
```

Agent 自己也能 write/edit 这个文件——创建、修改、调整 gate。

---

## 八、CLI 接口

### `ion workflow validate <path>`

```bash
$ ion workflow validate .ion/workflow.yaml
✅ Valid: 7 stages, 5 gates, 3 loop_backs
```

### `ion workflow run <path>`

```bash
$ ion workflow run .ion/workflow.yaml
[workflow] Stage 1/7: spec... done ✅
[workflow] Stage 2/7: develop... done ✅
[workflow] Stage 3/7: review... CHANGES → loop_back develop (1/3)
[workflow] Stage 2/7: develop (retry)... done ✅
[workflow] Stage 3/7: review... APPROVE → done ✅
[workflow] Stage 4/7: merge... done ✅
[workflow] Stage 5/7: publish... done ✅
[workflow] Stage 6/7: cleanup... done ✅
[workflow] PIPELINE COMPLETE
```

### `ion workflow status <path>`

```bash
$ ion workflow status .ion/workflow.yaml
Workflow: delivery
  spec:     done ✅
  develop:  done ✅
  review:   failed ❌ (2/3 loops)
  merge:    pending ⏳
  publish:  pending ⏳
  cleanup:  pending ⏳
Next: retry review → loop_back to develop
```

---

## 九、与现有架构的关系

```
用户层:    ion workflow validate/run/status
策略层:    workflow.yaml + wf.md (agent)
内核层:    GateDecision + spawn_worker + worktree（已有，不改）
```

| 现有组件 | 关系 |
|:---------|:-----|
| orchestrator.md | workflow.yaml 是结构化替代，orchestrator 保留为 fallback |
| GateCheckExtension | stage 级 gate（内核强制）。workflow 的 gate 是跨 stage 级 |
| spawn_worker | wf agent 用它 spawn specialist agent |
| merger.md | cleanup stage 复用 merger 的 worktree 清理 |

**不改内核。**

---

## 十、CLI 测试 Group

> 对齐 BASH_EXTENSION.md 的 Group 格式：每条用例含完整 CLI 命令 + 预期 JSON。

### Group W1: DSL 校验

#### W1-1 合法 workflow 通过校验

```bash
ion workflow validate examples/workflows/delivery.wf.yaml
```

**预期：**
```json
{"valid": true, "stages": 7, "gates": 5, "loop_backs": 3}
```

**验证点：**
- ✅ 语法正确
- ✅ 所有必填字段存在
- ✅ loop_back 指向的 stage id 存在

#### W1-2 缺少必填字段

```bash
echo 'name: bad
stages:
  - id: x' > /tmp/bad.wf.yaml
ion workflow validate /tmp/bad.wf.yaml
```

**预期：**
```json
{"valid": false, "error": "stage 'x' missing required field: agent or commands"}
```

#### W1-3 loop_back 指向不存在的 stage

```bash
echo 'name: bad
stages:
  - id: x
    agent: developer
    task: test
    on_fail:
      loop_back: nonexistent' > /tmp/bad2.wf.yaml
ion workflow validate /tmp/bad2.wf.yaml
```

**预期：**
```json
{"valid": false, "error": "stage 'x' on_fail.loop_back='nonexistent' but no stage with id 'nonexistent' exists"}
```

#### W1-4 agent 和 commands 互斥

```bash
echo 'name: bad
stages:
  - id: x
    agent: developer
    task: test
    commands: ["echo hi"]' > /tmp/bad3.wf.yaml
ion workflow validate /tmp/bad3.wf.yaml
```

**预期：**
```json
{"valid": false, "error": "stage 'x' has both agent and commands (mutually exclusive)"}
```

---

### Group W2: 单 stage 执行

#### W2-1 gate 通过

```bash
# 准备项目
mkdir -p /tmp/wf-test && cd /tmp/wf-test && git init -q
echo '# t' > README.md && git add . && git commit -q -m init
mkdir -p .ion/agents && echo '{"runtime":{"default_mode":"local"}}' > .ion/config.json
cp examples/agents/developer.md .ion/agents/

# workflow: 单 stage，gate 检查 hello.py 是否存在
cat > .ion/workflow.yaml << 'WF'
name: simple
stages:
  - id: develop
    agent: developer
    task: "创建 hello.py with print('hello')"
    gate:
      command: "ls hello.py && echo EXISTS"
      expected: EXISTS
WF

ion workflow run .ion/workflow.yaml
```

**预期：**
```json
{"stage": "develop", "status": "done", "gate": "PASS"}
```

**验证点：**
- ✅ hello.py 实际创建
- ✅ workflow.yaml 的 develop status 变为 done

#### W2-2 gate 失败 → loop_back → 重试通过

```bash
# developer 第一次不写文件（gate 失败），第二次才写
# （需 mock 或多次 LLM 调用）
ion workflow run .ion/workflow.yaml
```

**预期输出包含：**
```
[workflow] develop: gate FAIL (1/3) → loop_back develop
[workflow] develop (retry): gate PASS → done
```

#### W2-3 gate 超限 → ABORTED

```bash
# gate 检查一个永远不存在的文件
cat > .ion/workflow.yaml << 'WF'
name: abort
stages:
  - id: x
    agent: developer
    task: "do nothing"
    gate:
      command: "ls IMPOSSIBLE.xyz && echo EXISTS"
      expected: EXISTS
      max_retries: 2
    on_fail:
      loop_back: x
      max_loops: 2
WF

ion workflow run .ion/workflow.yaml
```

**预期：**
```json
{"status": "PIPELINE ABORTED", "stage": "x", "reason": "max_loops (2) exceeded"}
```

---

### Group W3: 条件分支

#### W3-1 if 为 true → 执行

```bash
cat > .ion/workflow.yaml << 'WF'
name: cond
context:
  run_it: true
stages:
  - id: step1
    agent: developer
    task: "create a.py"
  - id: step2
    agent: developer
    task: "create b.py"
    if: "context.run_it == true"
WF

ion workflow run .ion/workflow.yaml
```

**预期：** step2 执行，a.py 和 b.py 都创建。

#### W3-2 if 为 false → 跳过

```bash
# context.run_it = false
cat > .ion/workflow.yaml << 'WF'
name: cond
context:
  run_it: false
stages:
  - id: step1
    agent: developer
    task: "create a.py"
  - id: step2
    agent: developer
    task: "create b.py"
    if: "context.run_it == true"
WF

ion workflow run .ion/workflow.yaml
```

**预期：**
```json
{"step1": "done", "step2": "skipped"}
```

**验证点：** b.py 不存在（step2 被跳过）

#### W3-3 if: always → 无论上游都跑

```bash
cat > .ion/workflow.yaml << 'WF'
name: always
stages:
  - id: fail_step
    agent: developer
    task: "do nothing useful"
    gate:
      command: "false"
      expected: "WILL_NEVER_PASS"
  - id: cleanup
    if: "always"
    commands: ["echo CLEANED"]
WF
```

**预期：** fail_step 失败，但 cleanup 仍然执行。

---

### Group W4: 上下文传递

#### W4-1 spec outputs → develop 引用

```bash
cat > .ion/workflow.yaml << 'WF'
name: ctx
stages:
  - id: spec
    agent: coordinator
    task: "输出文件名 calc.py"
    outputs:
      target_file: "stage_output"
  - id: develop
    agent: developer
    task: "创建文件：{{context.target_file}}"
    if: "stages.spec.status == 'done'"
WF

ion workflow run .ion/workflow.yaml
```

**验证点：** develop 的 task 里 `{{context.target_file}}` 被替换为 `calc.py`

#### W4-2 context 初始值

```bash
cat > .ion/workflow.yaml << 'WF'
name: init
context:
  project_name: "my-calc"
stages:
  - id: develop
    agent: developer
    task: "创建 {{context.project_name}}.py"
WF
```

**验证点：** developer 收到的 task 是"创建 my-calc.py"

---

### Group W5: 多 stage 串联

#### W5-1 spec → develop → merge 全部 done

```bash
# 使用 examples/workflows/delivery.wf.yaml（精简版）
ion workflow run .ion/workflow.yaml
```

**预期：**
```json
{"spec": "done", "develop": "done", "merge": "done", "result": "PIPELINE COMPLETE"}
```

#### W5-2 review 失败 → loop_back develop → 修复后通过

**预期输出包含：**
```
[workflow] review: gate FAIL → loop_back develop (1/3)
[workflow] develop (retry): done
[workflow] review (retry): APPROVE → done
```

---

### Group W6: cleanup

#### W6-1 成功后清理 worktree

```bash
cat > .ion/workflow.yaml << 'WF'
name: cleanup-test
stages:
  - id: develop
    agent: developer
    task: "create x.py"
    worktree: true
    cleanup:
      on_success: true
WF

ion workflow run .ion/workflow.yaml
git worktree list  # 应该只剩 master
```

**验证点：** worktree 被清理

#### W6-2 失败后保留 worktree

```yaml
cleanup:
  on_failure: false
```

**验证点：** gate 失败后 worktree 仍在

#### W6-3 cleanup stage (if: always)

```bash
# 无论上游成败，cleanup stage 都执行
git worktree list  # 检查所有 worktree 被清理
```

---

### Group W7: 持久化

#### W7-1 中断后恢复

```bash
# 执行到 stage 3 后 Ctrl+C
ion workflow run .ion/workflow.yaml  # 中断
cat .ion/workflow.yaml  # 检查 status 字段
# spec: done, develop: done, review: pending

# 重新执行
ion workflow run .ion/workflow.yaml  # 从 review 继续
```

**验证点：** 不重新执行已 done 的 stage

#### W7-2 全部完成后状态持久化

```bash
ion workflow run .ion/workflow.yaml
cat .ion/workflow.yaml  # 所有 status=done
```

---

## 十一、模板与速查

### 最小 workflow

```yaml
name: simple
stages:
  - id: develop
    agent: developer
    task: "创建 hello.py"
    gate:
      command: "ls hello.py && echo EXISTS"
      expected: EXISTS
```

### 字段速查

| 字段 | 必填 | 默认 | 说明 |
|------|:----:|:----:|------|
| `id` | ✅ | — | 唯一标识 |
| `agent` | ⚠️ | — | agent 名（或 commands） |
| `task` | ⚠️ | — | 任务（支持 `{{context.xxx}}`） |
| `worktree` | ❌ | false | git 隔离 |
| `if` | ❌ | — | 条件 |
| `gate.command` | ❌ | — | bash 检查命令 |
| `gate.expected` | ❌ | PASS | 期望输出 |
| `gate.max_retries` | ❌ | 3 | 重试次数 |
| `on_fail.loop_back` | ❌ | — | 回退 stage |
| `on_fail.max_loops` | ❌ | 3 | 回退上限 |
| `cleanup.on_success` | ❌ | true | 成功清理 |
| `cleanup.on_failure` | ❌ | false | 失败清理 |

### 常用 gate 命令

```bash
# 文件存在
"ls hello.py && echo EXISTS"

# 文件含特定函数
"grep -q 'def add' calc.py && echo HAS_FUNC"

# Python 测试通过
"python3 -m pytest -q 2>&1 | grep -q passed && echo TESTS_OK"

# Git 有新 commit
"git log --oneline -1 | grep -qi 'merge\\|add' && echo HAS_COMMIT"

# GitHub repo 已创建
"git remote -v | grep -q origin && echo HAS_REMOTE"
```

### 常用 if 条件

```yaml
if: "stages.develop.status == 'done'"
if: "stages.develop.status == 'failed'"
if: "context.needs_review == true"
if: "stages.merge.status == 'done' && context.needs_publish == true"
if: "always"
```

---

## 十二、业界对比

| 能力 | GitHub Actions | GitLab CI | pi | Claude Code | **ION** |
|------|:---:|:---:|:---:|:---:|:---:|
| stages 分组 | ❌ | ✅ | ❌ | ❌ | ✅ |
| 条件分支 | ✅ | ✅ | ❌ | ❌ | ✅ |
| 上下文传递 | ✅ | ❌ | ❌ | ❌ | ✅ |
| gate 校验 | ❌ | ❌ | ✅(工具级) | ❌ | ✅(stage 级) |
| 失败回退 | ❌ | ❌ | ❌ | ❌ | ✅ |
| retry | ❌ | ✅ | ❌ | ❌ | ✅ |
| cleanup (always) | ✅ | ✅ | ❌ | ❌ | ✅ |
| 持久化恢复 | ✅ | ✅ | ❌ | ❌ | ✅ |
| Agent 自写 | ❌ | ❌ | ❌ | ❌ | ✅ |
| DSL 校验 | ✅ | ✅ | ✅ | ❌ | ✅ |
