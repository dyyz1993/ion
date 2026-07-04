# ION Team — 单项目自治 Agent 团队

> **状态：开发中** — `ion team --project .` 已实现：
> 1️⃣ 扫描 `.ion/agents/` 加载 Agent 定义
> 2️⃣ 读取 `PRD.md`
> 3️⃣ Spawn Leader Worker（指定 agent=leader）
> 4️⃣ 事件泵（实时打印 text_delta + agent_end）
> 5️⃣ 最终状态报告
>
> **待接续：** 解析 Leader 的 JSON 计划 → spawn Developer 并行开发 → Reviewer → QA → Merge

---

## 一、整体架构

```
用户
 │  ion team --project ./my-app
 ▼
┌──────────────────────────────────────────────┐
│  ion manager start (多项目守护进程)            │
│  ┌──────────────────────────────────────┐    │
│  │  Team A (my-app)                     │    │
│  │  ┌──────────┐                        │    │
│  │  │ Leader   │ ← 拆任务、分配、跟踪     │    │
│  │  └────┬─────┘                        │    │
│  │       │ spawn × N                    │    │
│  │  ┌────┴─────┐  ┌────┴─────┐         │    │
│  │  │ Developer│  │ Developer│  ...     │    │
│  │  │ worktree │  │ worktree │          │    │
│  │  │ dev/feat1│  │ dev/feat2│          │    │
│  │  └──────────┘  └──────────┘         │    │
│  │  ┌──────────┐  ┌──────────┐         │    │
│  │  │ Reviewer │  │ QA       │         │    │
│  │  └──────────┘  └──────────┘         │    │
│  └──────────────────────────────────────┘    │
│  ┌──────────────────────────────────────┐    │
│  │  Team B (web-api)                    │    │
│  │  ...                                 │    │
│  └──────────────────────────────────────┘    │
│  Python UI (可选) ← WebSocket ← 事件推送     │
└──────────────────────────────────────────────┘
```

### 分层职责

| 层 | 职责 | 实现 |
|----|------|------|
| **ion CLI** | 用户入口，解析命令 | `src/bin/ion.rs`, `src/bin/ion_worker.rs` |
| **ion manager** | 多项目守护进程，管理 Worker 生命周期 | `src/bin/ion.rs` — `cmd_manager_start` |
| **ion team** | 单项目自治团队，自动编排工作流 | `src/bin/ion.rs` — `cmd_team` (第 1466 行) |
| **WorkerRegistry** | Worker 创建/销毁/通信/事件 | `src/worker_registry.rs` |
| **Agent 定义 + 解析** | 角色描述、system prompt、frontmatter 解析 | `src/agent_config.rs` — `parse_agent_file` (第 148 行) / `find_agent` (第 171 行) |
| **Python UI** | 纯前端展示，WebSocket 透传 | `server.py`（可选，未实现） |

---

## 二、项目目录结构

```
my-project/
├── .ion/
│   ├── agents/
│   │   ├── leader.md          # 项目经理
│   │   ├── developer.md       # 开发工程师
│   │   ├── reviewer.md        # 代码审查员 (只读)
│   │   ├── qa.md              # 测试工程师
│   │   └── ops.md             # 运维 (可选)
│   ├── workflow.md            # 工作流定义 (可选，有默认值)
│   └── config.yml             # 团队配置 (可选)
├── PRD.md                     # 产品需求文档（核心输入）
├── README.md
└── src/                       # 项目代码
```

### PRD.md — 项目需求文档

唯一必需的人工输入。描述要开发什么功能、规格、技术栈。

### .ion/agents/*.md — Agent 角色定义

YAML frontmatter + Markdown body（system prompt）：

```yaml
---
name: developer
description: 开发工程师，在隔离 worktree 中实现功能
color: green
tier: pro
tools:
  - read
  - write
  - edit
  - bash
disallowed_tools: []
---
# Developer Agent

你是开发工程师，在独立的 git worktree 中工作。
...
```

### .ion/workflow.md — 工作流定义（可选）

定义团队协作的阶段：

```yaml
---
name: standard-dev
stages:
  - analyze    # Leader 分析 PRD
  - split      # Leader 拆任务
  - develop    # Developer 并行开发
  - review     # Reviewer 审查
  - qa         # 测试
  - merge      # 合并发布
---
```

---

## 三、ion team 命令设计

```
ion team [OPTIONS]

选项:
  --project, -p <PATH>      项目目录 (默认: .)
  --max-workers <N>         最大并行 Worker 数 (默认: 4)
  --worktree                启用 worktree 隔离 (默认: true)
  --dry-run                 只打印计划，不实际执行
```

### 执行流程

```
ion team --project ./my-app
  │
  ├─ [1] 读取配置
  │    ├─ .ion/config.yml (可选)
  │    ├─ .ion/agents/*.md
  │    └─ PRD.md
  │
  ├─ [2] 启动 WorkerRegistry
  │    ├─ subscribe_global → 事件泵
  │    └─ process_pending_commands → 后台任务
  │
  ├─ [3] Spawn Leader
  │    ├─ agent=leader, 读 PRD
  │    ├─ Leader 分析需求 → 输出 JSON 计划
  │    └─ 格式: {"need_split":true,"subtasks":[...]}
  │
  ├─ [4] 解析 JSON → Spawn Developer × N
  │    ├─ 每个 Developer 带 worktree (dev/module-name)
  │    ├─ prompt = 模块规格 + 工作说明
  │    └─ 设置 channel 通信
  │
  ├─ [5] 等待开发完成
  │    ├─ pump task 转发 text_delta 到 stdout
  │    ├─ Developer 通过 CHANNEL_SEND 汇报
  │    └─ 全部完成或超时
  │
  ├─ [6] Spawn Reviewer
  │    ├─ agent=reviewer (只读模式)
  │    ├─ 审查 Developer 的代码
  │    └─ Approve / Request Changes
  │
  ├─ [7] Spawn QA
  │    ├─ 编写并运行测试
  │    └─ 报告测试结果
  │
  └─ [8] Leader 总结
       ├─ 合并 PR、更新文档
       └─ 输出完成报告
```

---

## 四、Agent 角色定义

### Leader (项目经理)

```
职责: 分析 PRD → 拆分子任务 → 分配 → 跟踪 → 总结
权限: 读 PRD，创建 Worker，发送消息
工具: read, grep, find, ls, bash
输出: JSON 拆分计划
```

### Developer (开发工程师)

```
职责: 在 worktree 中实现功能 → commit → 汇报
权限: 读写代码，执行命令
工具: read, write, edit, bash, grep, find, ls
隔离: 每个 Developer 独立的 git worktree + 独立分支
```

### Reviewer (代码审查员)

```
职责: 审查 Developer 的代码 → Approve / Request Changes
权限: 只读（无权写/编辑）
工具: read, grep, find, ls, bash
disallowed_tools: write, edit
```

### QA (测试工程师)

```
职责: 编写并运行测试 → 报告覆盖率
权限: 读写测试文件，运行测试
工具: read, write, edit, bash
```

---

## 五、Workflow 流程详解

```
阶段 1: analyze
  Leader 读取 PRD.md + 现有代码
  → 理解需求，输出分析报告
  → 确定是否需要拆分

阶段 2: split
  Leader 输出 JSON 计划
  → 解析 subtasks
  → 为每个 subtask spawn Developer

阶段 3: develop (并行)
  Developer 1 → worktree: dev/auth-module     → 写代码 → commit
  Developer 2 → worktree: dev/api-gateway     → 写代码 → commit
  Developer 3 → worktree: dev/db-layer        → 写代码 → commit
  ↓
  每个 Developer 完成后通过 channel 汇报
  ↓
  Leader 跟踪进度

阶段 4: review (可选)
  Reviewer 审查每个 Developer 的代码
  → Approve 或 Request Changes
  → Developer 修改

阶段 5: qa (可选)
  QA 编写测试用例
  → 运行测试
  → 报告结果

阶段 6: merge
  Leader 合并分支到 main
  → 更新文档 / CHANGELOG
  → 输出完成报告
```

---

## 六、当前实现状态

### 已完成 ✅

| 组件 | 状态 | 位置 |
|------|------|------|
| Agent .md 文件解析 + frontmatter 加载 | ✅ | `src/agent_config.rs:148` `parse_agent_file()` |
| `get_agent_detail` RPC — 返回完整 agent 信息 | ✅ | `src/bin/ion_worker.rs:534` |
| `switch_agent` RPC — 动态切换 Worker agent | ✅ | `src/bin/ion_worker.rs:466` + `src/agent/agent_loop.rs:120` |
| Worktree 隔离基础设施（`git worktree add -b`, 清理, 环境变量） | ✅ | `src/worker_registry.rs:55` `WorktreeConfig`, `create_worktree_advanced()` (行 1022) |
| 非 git 目录自动 `git init` + initial commit | ✅ | `src/worker_registry.rs:140-160` |
| `ion manager start` RPC 模式（stdin/stdout + IO Bridge） | ✅ | `src/bin/ion.rs` — `cmd_manager_start` |
| 事件泵（`drain_events` + subscriber 转发） | ✅ | `src/worker_registry.rs:485` `drain_events()` |
| `channel_send` — 真实路由到频道订阅者 | ✅ | `src/worker_registry.rs:722` `channel_send()` |
| `ion team --project .` 基础框架 | ✅ | `src/bin/ion.rs:1466` `cmd_team()` |
| → 扫描 `.ion/agents/` 加载 agents | ✅ | `cmd_team()` 调用 `agent_config::parse_agent_file()` |
| → 读取 PRD.md | ✅ | `cmd_team()` 直接读取 |
| → Spawn Leader → feed PRD prompt | ✅ | `cmd_team()` 创建 Worker + `send_to_worker` |
| → 事件泵实时打印 text_delta + agent_end | ✅ | `cmd_team()` 中 pump task（tokio::spawn） |
| → 最终状态报告 | ✅ | `cmd_team()` 末尾打印 workers 列表 |
| Worktree 隔离集成到 team 流程（Developer 带 worktree） | ❌ P0 | Developer spawn 阶段未实现 |

### 待实现 🔧

| 组件 | 优先级 | 位置/说明 |
|------|--------|-----------|
| Leader JSON 计划解析 → spawn Developer | **P0** | `cmd_team()` 需要解析 Leader 输出的 `{"need_split":true,"subtasks":[...]}`，对每个 subtask 调用 `create_worker()` |
| Developer worktree 隔离 | **P0** | WorkerRegistry 已有 `WorktreeConfig` 和 `create_worktree_advanced()`，但 `cmd_team()` 未传入。Developer 需要产互独立分支 |
| Developer prompt 组装 + 派发 | **P0** | 把 subtask.spec 作为 prompt，设置 agent=developer，限制工具集 |
| CHANNEL_SEND 汇报检测 | **P0** | 检测 Developer 回复中的状态（complete/progress），Leader 通过 channel 跟踪进度 |
| 超时/自动重试 | **P0** | Worker 长时间无响应（Agent 完成但未汇报）时自动重启或降级 |
| Reviewer 自动 spawn | **P1** | Developer 完成后自动触发 review（agent=reviewer，只读 + disallowed_tools） |
| QA 自动 spawn | **P1** | Review 通过后自动编写并运行测试 |
| Leader 自动总结 + 合并 | **P1** | 合并分支到 main，更新 CHANGELOG，输出完成报告 |
| workflow.md 解析 | **P1** | 从 `.ion/workflow.md` 读取阶段配置（或使用默认 6 阶段） |
| `--dry-run` 支持 | **P1** | 只打印计划，不实际 spawn Worker |
| Python UI | **P2** | WebSocket 透传事件到前端渲染 |
| Agent 热更新 | **P2** | `.ion/agents/*.md` 变化时自动重载 |

### Python UI 层

Python 是可选层，只做 WebSocket 透传和前端渲染。
所有核心逻辑在 Rust Manager/Team 中，Python 不管理 Worker、不维护状态。

---

## 七、当前实现详情

### `cmd_team()` 实际执行流 (`src/bin/ion.rs:1466`)

```rust
// 伪代码
async fn cmd_team(project_path, max_workers) {
    // [1] 扫描 .ion/agents/*.md → parse_agent_file()
    let agents = scan_agents(&proj.join(".ion/agents/"));

    // [2] 创建 WorkerRegistry + 事件泵
    let registry = Arc::new(Mutex::new(WorkerRegistry::new()));
    tokio::spawn(pump_task(registry.clone()));

    // [3] 读取 PRD.md
    let prd_text = read_prd(&proj.join("PRD.md"));

    // [4] Spawn Leader
    let mut cfg = WorkerCreateConfig::default();
    cfg.agent = Some("leader");
    cfg.model = Some("deepseek-v4-flash");
    cfg.provider = Some("opencode");
    cfg.project_path = Some(project_path);
    cfg.channels = Some(vec!["main"]);
    let leader = registry.create_worker(cfg).await?;

    // [5] Feed PRD → Leader 分析
    registry.send_to_worker(&leader.worker_id, "prompt", json!({"text": plan_prompt})).await;

    // [6] 等待一段时间 + 事件泵显示 text_delta
    tokio::time::sleep(Duration::from_secs(15)).await;

    // [7] 打印状态
    for w in registry.list_workers() {
        println!("   ◈ {} [{}] status={:?}", w.worker_id[..12], w.model, w.status);
    }
}
```

### 关键基础设施已可用

| 基础设施 | 函数/结构 | 文件行 |
|----------|----------|--------|
| Agent 定义解析 | `parse_agent_file()`, `find_agent()` | `src/agent_config.rs:148, 171` |
| Agent 信息 RPC | `"get_agent_detail"` handler | `src/bin/ion_worker.rs:534` |
| 动态切换 Agent | `"switch_agent"` handler | `src/bin/ion_worker.rs:466` |
| Worktree 创建 | `create_worktree_advanced()` | `src/worker_registry.rs:1022` |
| Worktree 清理 | `remove_worktree()` | `src/worker_registry.rs:1088` |
| Worker 创建 | `create_worker(config)` | `src/worker_registry.rs:116` |
| Worker 通信 | `send_to_worker()`, `channel_send()` | `src/worker_registry.rs:651, 722` |
| 事件订阅 | `subscribe()`, `subscribe_global()`, `drain_events()` | `src/worker_registry.rs:530, 809, 485` |
| 进程管理 | `kill_worker()`, `reclaim()` | `src/worker_registry.rs:380, 434` |

## 八、快速开始

```bash
# 1. 源码编译
cargo build --bin ion

# 2. 创建项目
mkdir -p my-project/.ion/agents
cd my-project
git init
echo "# My App" > README.md
git add . && git commit -m "init"

# 3. 写需求
cat > PRD.md << 'EOF'
# CLI 待办事项工具

功能:
- todo add "内容"
- todo list
- todo done <id>
- todo delete <id>
- JSON 文件持久化
EOF

# 4. 复制 Agent 定义
# （需要先准备 leader.md / developer.md / reviewer.md / qa.md）
# 参考：https://github.com/ion/ion-demo-workflow
# 或从 examples 目录复制：
# cp -r /path/to/ion/examples/team-workflow/.ion/agents .ion/

# 5. 启动团队
ion team --project .

# 6. 观察输出
# → Agent 加载：📋 Agent: leader (项目经理)
# → PRD 加载：📄 PRD loaded (xxx bytes)
# → Leader 启动：📋 Spawning Leader...
# → 分析过程：📝 [wkr_abc12345] 我将分析这个 PRD...
# → 完成：      ✅ [wkr_abc12345] complete
```

---

## 九、与 pi 的关系

| 概念 | pi (pi-coding-agent) | ION |
|------|---------------------|-----|
| 单 Agent 对话 | ✅ | ✅ `ion "message"` |
| RPC 协议 | ✅ JSONL stdin/stdout | ✅ 完全兼容 |
| Manager 守护进程 | ✅ | ✅ `ion manager start` |
| 会话存储 | ✅ JSONL v3 | ✅ 完全兼容 |
| Agent 自定义 .md | ✅ | ✅ `~/.ion/agent/agents/` |
| Worktree 隔离 | ❌ | ✅ `worktree: {"branch":"dev/xx"}` |
| 多 Agent 团队 | ❌ | ✅ `ion team`（开发中） |
| 自动化工作流 | ❌ | 🔧 设计中 |
