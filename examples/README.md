# Agent 示例模板

> 7 个开箱即用的 agent .md 文件 + 1 个 workflow YAML 示例。复制到项目的 `.ion/agents/` 目录即可使用。

## 快速开始

```bash
# 在你的项目里
mkdir -p .ion/agents
cp examples/agents/*.md .ion/agents/

# 默认本地 runtime（不需要额外配置）
ion --host --agent coordinator "创建 hello.py 打印 hello world"
```

## 7 个角色

| Agent | 职责 | 工具 | 禁用 |
|-------|------|------|------|
| **wf** | Workflow 执行引擎（读 yaml → spawn → gate → 写回状态） | read / write / edit / bash / spawn_worker / await_worker | — |
| **orchestrator** | 分阶段 pipeline（DEVELOP→MERGE→PUBLISH + gate） | spawn_worker / bash / read | edit / write |
| **coordinator** | 简单编排（3 种调度策略） | spawn_worker / await_worker / read / ls | edit / write / bash |
| **developer** | 写代码、提交 | write / edit / bash / read / ls | spawn_worker |
| **merger** | 合并分支、清理 worktree | bash / ls / read | edit / write / spawn_worker |
| **reviewer** | 代码审查（只读） | read / grep / bash / git_diff | edit / write / spawn_worker |
| **publisher** | GitHub push / issue / PR | bash / ls / read | edit / write / spawn_worker |

## Workflow 模式

除了用 `--agent coordinator` 做简单编排，还可以用结构化 workflow：

```bash
# 复制 workflow 示例
cp examples/workflows/delivery.wf.yaml .ion/workflow.yaml

# 校验
ion workflow validate .ion/workflow.yaml

# 执行（wf agent 读 yaml → 逐 stage 执行 → gate 校验 → 写回状态）
ion workflow run .ion/workflow.yaml

# 查看状态
ion workflow status .ion/workflow.yaml
```

Workflow YAML 定义了 stage 列表，每个 stage 有 agent / task / gate / if / loop_back / cleanup。详见 [docs/design/WORKFLOW_ENGINE.md](../docs/design/WORKFLOW_ENGINE.md)。

## 三种调度策略

### 策略 A：串行（最稳定，推荐）
```bash
ion --host --agent coordinator "创建 a.py 和 b.py，一个一个来"
# coordinator 会: spawn(wait=true) → 等 → spawn(wait=true) → 等 → merger
```

### 策略 B：小批量并行（2-3 个独立任务）
```bash
ion --host --agent coordinator "并行创建 3 个独立模块"
# coordinator 会: spawn(wait=false) × 3 → await × 3 → merger
```

### 策略 C：后台同级（长任务）
```bash
ion --host --agent coordinator "后台跑个监控任务"
# coordinator 会: spawn(relation='peer') → 自动 follow_up 汇报
```

## 自定义

这些 .md 文件是**纯文本提示词**，直接编辑就能改编排逻辑。比如：
- 加一个 `tester` agent 负责跑测试
- 让 coordinator 在 merge 前先 spawn reviewer
- 调整 developer 的 commit 消息格式

不需要改任何 Rust 代码。
