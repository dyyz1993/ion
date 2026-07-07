# Team 编排 — Agent.md 驱动方案

> **状态：已验证** — coordinator + developer 链路在本地 runtime 端到端跑通。
> 8 个 CI 测试全部通过（`tests/team_e2e.sh`）。

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
