# Workflow 模板

> 写新 workflow 时的参考。完整设计见 [WORKFLOW_ENGINE.md](../design/WORKFLOW_ENGINE.md)。

---

## 最小 workflow

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

## 带 gate + loop_back

```yaml
name: with-review
stages:
  - id: develop
    agent: developer
    task: "实现功能"
    gate:
      command: "ls main.py && echo OK"
      expected: OK
    on_fail:
      loop_back: develop
      max_loops: 3

  - id: review
    agent: reviewer
    task: "审查 main.py"
    gate:
      command: "grep -q APPROVE && echo PASS"
      expected: PASS
    on_fail:
      loop_back: develop
      max_loops: 2
```

## 带 if 条件 + context

```yaml
name: conditional
context:
  needs_deploy: true
stages:
  - id: build
    agent: developer
    task: "构建项目"
    outputs:
      build_result: "stage_output"
    gate:
      command: "echo '{{context.build_result}}' | grep -q success && echo BUILT || echo FAIL"
      expected: BUILT

  - id: deploy
    agent: publisher
    task: "部署到生产"
    if: "stages.build.status == 'done' && context.needs_deploy == true"

  - id: cleanup
    if: "always"
    commands:
      - "rm -rf /tmp/build"
```

## 带 worktree + cleanup

```yaml
name: isolated-dev
stages:
  - id: develop
    agent: developer
    task: "开发功能"
    worktree: true
    gate:
      command: "ls feature.py && echo EXISTS"
      expected: EXISTS
      max_retries: 3
    on_fail:
      loop_back: develop
      max_loops: 3
    cleanup:
      on_success: true      # gate 过了 → 自动删 worktree
      on_failure: false     # gate 没过 → 留现场排查

  - id: merge
    agent: merger
    task: "合并到 master"
    if: "stages.develop.status == 'done'"
```

---

## 字段速查表

### Stage 必填

| 字段 | 说明 |
|------|------|
| `id` | 唯一标识（被 if/loop_back/outputs 引用） |
| `agent` | agent 名称（或用 `commands` 代替） |
| `task` | 任务描述（支持 `{{context.xxx}}`） |

### Gate

| 字段 | 默认 | 说明 |
|------|:----:|------|
| `gate.command` | — | bash 命令 |
| `gate.expected` | `PASS` | 期望输出包含的字符串 |
| `gate.max_retries` | 3 | 重试次数 |

### 失败处理

| 字段 | 默认 | 说明 |
|------|:----:|------|
| `on_fail.loop_back` | — | 回退到哪个 stage |
| `on_fail.max_loops` | 3 | 回退最大次数 |

### 条件 + 上下文

| 字段 | 说明 |
|------|------|
| `if` | 条件表达式（`stages.X.status == 'done'`、`context.x == true`、`always`） |
| `context.xxx` | 全局变量（在 task/gate 里用 `{{context.xxx}}` 引用） |
| `outputs.key` | stage 输出写入 context |

### Cleanup

| 字段 | 默认 | 说明 |
|------|:----:|------|
| `cleanup.on_success` | true | 成功后清理 worktree |
| `cleanup.on_failure` | false | 失败后清理 worktree |

---

## 常用 gate 命令

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

# 目录非空
"ls -la src/ | grep -q '.py' && echo HAS_FILES"
```

---

## 常用 if 条件

```yaml
# 上游完成后才跑
if: "stages.develop.status == 'done'"

# 上游失败时跑（错误处理）
if: "stages.develop.status == 'failed'"

# 上下文布尔值
if: "context.needs_review == true"

# 复合条件
if: "stages.merge.status == 'done' && context.needs_publish == true"

# 无条件（cleanup 用）
if: "always"
```
