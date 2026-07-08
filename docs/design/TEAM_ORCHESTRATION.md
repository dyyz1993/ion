# Team 编排 — Agent.md 驱动方案

> **状态：已验证** — 5 任务串行 converge 全部通过（a/b/c/d/e.py 真实可用）。
> - `tests/scenario2_ci.sh` — 27 用例（场景 2 全覆盖）
> - `tests/team_e2e.sh` — 8 用例（Team 编排）
> - 修了 3 个真实 bug：disallowed_tools 不生效 / worktree config 丢失 / 反幻觉重试

---

## 一、设计理念

**内核只提供原语，编排策略全交给 `.md` 提示词。**

ION 没有 `ion team` 这种硬编码命令。团队编排完全通过：
- `ion --host --agent coordinator "做这个"` 启动
- `coordinator.md` 提示词让 LLM 自己决定怎么拆任务
- 内核的 `spawn_worker` 工具让 LLM 派生子 worker
- 递归 idle 检测让进程自动退出

**用户想改编排逻辑，编辑 `.md` 文件就行，不用碰 Rust。**

> 历史背景：旧版有 `ion team --project .` 命令，硬编码了 Leader/Developer/Reviewer 工作流。
> 已删除，旧设计稿见 [archive/TEAM_ARCH.md](../archive/TEAM_ARCH.md)。

---

## 二、架构图

```
┌─────────────────────────────────────────────────────────────────┐
│   👤 用户                                                       │
│    │                                                            │
│    │  ion --host --agent coordinator "创建 hello.py"            │
│    ▼                                                            │
│ ┌──────────────────────────────────────────────────────────┐   │
│ │  CLI 入口 (src/bin/ion.rs::cmd_host)                      │   │
│ │  • 启动 WorkerRegistry + 事件泵（按行打印 text_delta）    │   │
│ │  • spawn entry worker                                     │   │
│ │  • 递归 idle 检测 → 自动退出                              │   │
│ └──────────────────────────────────────────────────────────┘   │
│    │                                                            │
│    │ spawn worker (--agent coordinator)                         │
│    ▼                                                            │
│ ┌──────────────────────────────────────────────────────────┐   │
│ │  内核层（Rust）                                           │   │
│ │  • spawn_worker / send_to_worker / await_worker 工具      │   │
│ │  • Worktree 隔离基础设施                                  │   │
│ │  • 递归 idle 检测 (entry_worker_id + DFS)                 │   │
│ │  • Agent .md 加载（find_agent: 项目 > 全局 > 内置）       │   │
│ └──────────────────────────────────────────────────────────┘   │
│    │                                                            │
│    │ 加载哪个 agent.md？                                         │
│    ▼                                                            │
│ ┌──────────────────────────────────────────────────────────┐   │
│ │  策略层（.md 文件，纯文本，用户可编辑）                    │   │
│ │                                                          │   │
│ │  查找顺序：                                              │   │
│ │  1. <cwd>/.ion/agents/<name>.md   ← 项目级（最高优先）   │   │
│ │  2. ~/.ion/agent/agents/<name>.md ← 全局                 │   │
│ │  3. Rust 内置 (build/explore/plan) ← 兜底                │   │
│ │                                                          │   │
│ │  coordinator.md  → 拆任务，调 spawn_worker               │   │
│ │  developer.md    → 执行任务，写代码                      │   │
│ │  reviewer.md     → (待补) 只读工具集，审查代码           │   │
│ └──────────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────────┘
```

---

## 三、运行时数据流

```
ion --host --agent coordinator "创建 hello.py"
   │
   ├─① cmd_host() 启动
   │   ├─ 创建 WorkerRegistry
   │   ├─ 启动事件泵 task（按行打印 text_delta）
   │   └─ 启动 command 处理 task
   │
   ├─② spawn entry worker (agent=coordinator)
   │   ├─ WorkerRegistry 启动 ion-worker 子进程，传 --agent coordinator
   │   ├─ ion_worker::main 收到 --agent → find_agent("coordinator")
   │   ├─ 加载 .ion/agents/coordinator.md
   │   ├─ 应用 system_prompt + restrict_tools（按 frontmatter 配置）
   │   └─ 注入 initial_prompt（用户消息）
   │
   ├─③ coordinator worker 运行
   │   └─ LLM 根据 coordinator.md 的提示词，决定调 spawn_worker 工具
   │
   ├─④ coordinator 调 spawn_worker(agent="developer", task="...")
   │   ├─ WorkerRegistry 再启动一个 ion-worker 子进程，传 --agent developer
   │   ├─ find_agent("developer") → 加载 developer.md
   │   ├─ 应用 developer 的 system_prompt + 工具白名单
   │   └─ developer LLM 用 write 工具创建文件
   │
   ├─⑤ developer 完成 → coordinator 收到 first_turn_output
   │   └─ coordinator 输出总结
   │
   └─⑥ 递归 idle 检测
       ├─ coordinator idle? ✓
       ├─ developer idle? ✓
       └─ 全部 idle → 清理退出
```

---

## 四、Agent .md 文件格式

```markdown
---
name: coordinator
description: Team coordinator — breaks down tasks and dispatches
tools:
  - read
  - grep
  - find
  - ls
  - spawn_worker
  - send_to_worker
  - resume_worker
  - await_worker
disallowed_tools:
  - edit
  - write
  - bash
thinking_level: high
color: cyan
---

You are the **Coordinator**. You DON'T write code yourself.

Your job:
1. Read the user's request and the project layout.
2. Break the work into 1-3 concrete subtasks.
3. For each subtask, call spawn_worker(relation='child', agent='developer', task='<spec>').
...
```

### frontmatter 字段

| 字段 | 类型 | 说明 |
|------|------|------|
| `name` | string | Agent 名称（必需） |
| `description` | string | 一句话描述 |
| `tools` | string[] | 工具白名单（不写则用全部） |
| `disallowed_tools` | string[] | 工具黑名单 |
| `model` | string | 指定模型 ID |
| `max_turns` | number | 最大轮次 |
| `thinking_level` | string | off/minimal/low/medium/high/xhigh |
| `tier` | string | fast/pro/max（模型分级） |
| `color` | string | 显示颜色 |

### body 部分

直接作为 system prompt 注入。**这是策略层的核心**——所有"怎么拆任务、怎么分配、怎么汇报"的逻辑都写在这里。

---

## 五、关键内核能力（已有，未改动）

| 能力 | 工具/RPC | 位置 |
|------|---------|------|
| 派生 worker | `spawn_worker` 工具 | `src/agent/tool.rs::SpawnWorkerTool` |
| 同步等待子 worker | `spawn_worker(wait=true)` | 同上 |
| 异步派生 + 后续等待 | `spawn_worker(wait=false)` + `await_worker` | 同上 |
| 给任意 worker 发消息 | `send_to_worker` 工具 | `src/agent/tool.rs::SendToWorkerTool` |
| 继续子 worker 对话 | `resume_worker` 工具 | 同上 |
| 频道广播 | `channel_send` 工具 | 同上 |
| 杀掉 worker | `kill_worker` 工具 | 同上 |
| Worktree 隔离 | `WorktreeConfig` | `src/worker_registry.rs` |

### spawn_worker 语义

```javascript
spawn_worker(relation="child", agent="developer", task="...", wait=true)
// relation:
//   "child" = 父子关系，默认同步（wait=true 阻塞到首轮 agent_end）
//   "peer"  = 独立，异步，靠 follow_up 汇报
// wait (child only):
//   true  = 阻塞，返回 first_turn_output
//   false = 立即返回 worker_id，后续用 await_worker 收结果
```

---

## 六、本地 vs 远程 Runtime

⚠️ **重要**：默认 runtime 由 `~/.ion/config.json` 的 `runtime.default_mode` 决定。
如果全局配置成 `remote`，所有工具调用会走 SSH。

**项目级覆盖**：在 `<cwd>/.ion/config.json` 写：

```json
{
  "runtime": {
    "default_mode": "local"
  }
}
```

这会让该项目下的所有 worker 在本地执行。

---

## 七、快速开始

```bash
# 1. 创建项目
mkdir my-project && cd my-project
git init && echo "# test" > README.md && git add . && git commit -m "init"

# 2. 强制本地 runtime
mkdir -p .ion
echo '{"runtime":{"default_mode":"local"}}' > .ion/config.json

# 3. 创建 agent 定义
mkdir -p .ion/agents
# （把 coordinator.md 和 developer.md 放进去，见下面"示例文件"）

# 4. 跑
ion --host --agent coordinator "创建 hello.py 打印 hello world"
```

---

## 八、示例 Agent 文件

完整可用的示例见 `tests/team_e2e.sh` 中的内嵌文件（每次 CI 跑都会重新生成）。

最小化 coordinator.md：

```markdown
---
name: coordinator
tools: [read, ls, spawn_worker, send_to_worker]
disallowed_tools: [edit, write, bash]
---

You are the Coordinator. Use spawn_worker to delegate coding tasks to developer.
Never edit/write/bash yourself.
```

最小化 developer.md：

```markdown
---
name: developer
tools: [read, edit, write, bash]
disallowed_tools: [spawn_worker]
---

You are a Developer. Execute the task spec. Verify with bash when relevant.
```

---

## 九、验证

```bash
# 完整 e2e 测试（含真实 LLM）
bash tests/team_e2e.sh
# → 8 passed, 0 failed
```

测试覆盖：
- A1: developer 单 agent 直接执行
- B1: coordinator spawn 至少 2 个 worker
- B2: 文件实际创建
- B3: 文件内容正确
- B4: 递归 idle 退出
- C1: 不存在的 agent 错误处理

---

## 十、调度策略（三种模式灵活混用）

### 策略 A：串行（最稳定，推荐）
任务有依赖或不需要并行时，用同步 child：
```
spawn_worker(relation='child', agent='developer', task='...', worktree=true, wait=true)
```
一次只跑 1 个 developer，干完再 spawn 下一个。**5 个任务串行全部成功。**

### 策略 B：小批量并行（2-3 个独立任务）
任务互相独立、文件不重叠时，批量异步派发：
```
spawn_worker(relation='child', wait=false) × 2-3
await_worker(worker_id) × N
```
**最多 3 个并行**。超过 3 个时分批跑。

### 策略 C：后台同级（长任务/监控）
```
spawn_worker(relation='peer', report_channel='main')
```
peer 自动通过 follow_up 汇报完成。

---

## 十一、Bug 修复记录

### Bug 1: disallowed_tools 不生效
- **现象**: coordinator 的 `disallowed_tools: [edit, write, bash]` 被忽略，coordinator 能直接写文件
- **根因**: `ion_worker.rs` 只应用了 `tools`（白名单），完全忽略 `disallowed_tools`（黑名单）
- **修复**: 加载 agent 和 switch_agent 时，遍历 disallowed_tools 逐个 `remove_tool`
- **commit**: `a66d0e9`

### Bug 2: worktree worker 找不到项目 config
- **现象**: worktree worker 在远程 `/root` 跑而不是本地 worktree 目录
- **根因**: worktree 目录没有 `.ion/config.json`，worker 子进程回退到全局 remote 配置
- **修复**: spawn worker 时传 `ION_PROJECT_ROOT` 环境变量；config.rs 的 runtime default 改为 local（不从全局继承）
- **commit**: `ad3b02b`

### Bug 3: developer 幻觉（说创建了但没调 write）
- **现象**: developer 说"文件已创建"但实际没调 write 工具
- **修复**: 反幻觉重试机制 — 检测到 StopReason::Stop 且无 ToolCallEnd 事件时自动重试并注入 WARNING
- **commit**: `5d83aef`

### Bug 4: developer 写了文件但不 commit
- **现象**: worktree 里有文件但 git status 显示 untracked，merger 找不到可合并的 commit
- **修复**: merger.md 增加处理未提交文件的能力（`git add -A && git commit` 再 merge）
- **说明**: 这是提示词层修复，不需要改内核

---

## 十二、Agent 模板

开箱即用的 6 个 agent .md 文件在 `examples/agents/`：

| 文件 | 角色 | 工具 |
|------|------|------|
| `orchestrator.md` | 分阶段 pipeline 引擎（DEVELOP→MERGE→PUBLISH + gate 校验） | spawn_worker / bash |
| `coordinator.md` | 简单编排（无 gate，3 种调度策略） | spawn_worker / await_worker |
| `developer.md` | 写代码 + 强制 commit + 自验证 | write / edit / bash |
| `merger.md` | 合并分支 + 处理未提交文件 + cleanup worktree | bash |
| `reviewer.md` | 代码审查（只读） | read / grep / bash |
| `publisher.md` | GitHub push / issue / PR（调 gh CLI） | bash |

使用方法见 [examples/README.md](../../examples/README.md)。

---

## 十三、Orchestrator 分阶段 Pipeline

### 概念

Orchestrator 是一个**分阶段工作流引擎**。它不是内核功能——纯 `.md` 提示词驱动。
每个 stage 做 3 件事：**SPAWN agent → CHECK GATE → REPORT**。

### 流程

```
orchestrator
├── Stage 1: DEVELOP
│   ├── spawn developer (worktree=true, wait=true)
│   └── gate: ls <file>  → PASS / FAIL (重试 max 2)
│
├── Stage 2: MERGE
│   ├── spawn merger (wait=true)
│   └── gate: git log | grep merge  → PASS / FAIL
│
└── Stage 3: PUBLISH
    ├── spawn publisher (wait=true)
    └── gate: git remote -v | grep origin  → PASS / FAIL
```

### Gate 校验

Gate 用 `bash` 执行检查命令，输出包含预期字符串则 PASS。
失败自动重试（max 2 次），超限报 `PIPELINE ABORTED`。

### 真实验证

3 阶段 pipeline 全部跑通：
- DEVELOP: developer 创建 square.py（worktree 隔离）✅
- MERGE: merger 合并到 master + cleanup ✅
- PUBLISH: publisher 创建 GitHub repo + push ✅
- GitHub 验证: square.py 内容正确 ✅
