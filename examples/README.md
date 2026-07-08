# Team Agent 示例模板

> 4 个开箱即用的 agent .md 文件。复制到项目的 `.ion/agents/` 目录即可使用。

## 快速开始

```bash
# 在你的项目里
mkdir -p .ion/agents
cp examples/agents/*.md .ion/agents/

# 强制本地 runtime
echo '{"runtime":{"default_mode":"local"}}' > .ion/config.json

# 跑
ion --host --agent coordinator "创建 hello.py 打印 hello world"
```

## 6 个角色

| Agent | 职责 | 工具 | 禁用 |
|-------|------|------|------|
| **orchestrator** | 分阶段 pipeline（DEVELOP→MERGE→PUBLISH + gate） | spawn_worker / bash / read | edit / write |
| **coordinator** | 简单编排（3 种调度策略） | spawn_worker / await_worker / read / ls | edit / write / bash |
| **developer** | 写代码、提交 | write / edit / bash / read / ls | spawn_worker |
| **merger** | 合并分支、清理 worktree | bash / ls / read | edit / write / spawn_worker |
| **reviewer** | 代码审查（只读） | read / grep / bash / git_diff | edit / write / spawn_worker |
| **publisher** | GitHub push / issue / PR | bash / ls / read | edit / write / spawn_worker |

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

## 完整闭环流程

```
coordinator 拆任务
  → spawn developer (worktree=true) × N
  → developer 创建文件 + commit
  → await_worker 逐个等待
  → spawn merger
    → 检查各 worktree 状态
    → 处理未提交文件（git add -A）
    → merge 到 master
    → cleanup worktree + branch
  → coordinator 总结
```

## 自定义

这些 .md 文件是**纯文本提示词**，直接编辑就能改编排逻辑。比如：
- 加一个 `tester` agent 负责跑测试
- 让 coordinator 在 merge 前先 spawn reviewer
- 调整 developer 的 commit 消息格式

不需要改任何 Rust 代码。
