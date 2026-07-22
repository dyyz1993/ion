# ION 文档导航

> 本目录收纳 ION 项目的所有设计文档、指南、模板和归档。
> 根目录只保留 `AGENTS.md`（开发者约定）和 `README.md`（项目说明）。

---

## 目录结构

```
docs/
├── README.md                          ← 本文件（总导航）
│
├── templates/                          ← 5 个文档模板（写新文档时套用）
│   ├── DESIGN_TEMPLATE.md             ← 功能设计文档模板
│   ├── CLI_TEST_TEMPLATE.md           ← CLI 测试指南模板（Group A/B/C/D）
│   ├── TEST_SPEC_TEMPLATE.md          ← 测试规格模板（P0/P1/XFail）
│   ├── PI_ALIGNMENT_TEMPLATE.md       ← pi 对齐报告模板
│   └── EXTENSION_MANUAL_TEMPLATE.md  ← 扩展手册模板
│
├── guides/                             ← 用户/开发者使用指南
│   ├── CLI_USAGE.md                   ← 命令速查
│   ├── DEPLOY_ARCH.md                 ← 部署场景
│   └── EXTENSION_WORKFLOW.md          ← 扩展开发工作流
│
├── design/                             ← 功能设计文档（每个子系统一份）
│   ├── EXTENSION_SYSTEM.md            ← WASM 扩展系统
│   ├── BASH_EXTENSION.md              ← Bash 扩展（合并自 3 个文档）
│   ├── MEMORY_EXTENSION.md            ← Memory 扩展（合并自 2 个文档）
│   ├── COMPACTION.md                  ← Compaction 会话压缩
│   ├── PROVIDER_PROTOCOL.md           ← 多 Provider 协议
│   ├── PERMISSION_SYSTEM.md           ← 权限系统（合并自 4 个文档）
│   ├── SESSION_MESSAGE.md             ← Session 消息系统（合并自 2 个文档）
│   ├── HOOK_SYSTEM.md                 ← Shell Hook 系统（暂不开发）
│   ├── TEAM_ORCHESTRATION.md          ← Team 编排（agent.md 驱动）
│   ├── WORKFLOW_GATE.md               ← Workflow Gate（内核交付校验）
│   └── PI_RPC_ALIGNMENT.md            ← pi RPC 对齐
│
├── testing/                            ← 测试相关
│   └── TEST_CASES.md                  ← 测试用例总表
│
└── archive/                            ← 已归档（被合并/被替代，保留历史）
    ├── BASH_EXTENSION.md              ← 已合并到 design/BASH_EXTENSION.md
    ├── BASH_API_CHECKLIST.md          ← 已合并
    ├── BASH_EXTENSION_TUTORIAL.md     ← 已合并
    ├── PERMISSION_SYSTEM.md           ← 已合并到 design/PERMISSION_SYSTEM.md
    ├── PERMISSION_CLI.md              ← 已合并
    ├── PERMISSION_TESTS.md            ← 已合并
    ├── SECURITY_CLI_GUIDE.md          ← 已合并
    ├── SESSION_MESSAGE.md             ← 已合并到 design/SESSION_MESSAGE.md
    ├── SESSION_MESSAGE_TYPES.md       ← 已合并
    ├── MEMORY_EXTENSION.md            ← 已合并到 design/MEMORY_EXTENSION.md
    ├── MEMORY_SPEC.md                 ← 已合并
    ├── RPC_DIFF_REPORT.md             ← 被 PI_RPC_ALIGNMENT.md 替代
    └── WORKER_COMM_TODO.md            ← 任务已完成
```

---

## 按场景查文档

### 我想了解某个功能怎么设计/实现的

| 功能 | 文档 |
|------|------|
| WASM 扩展系统 | [design/EXTENSION_SYSTEM.md](design/EXTENSION_SYSTEM.md) |
| Bash 进程管理 | [design/BASH_EXTENSION.md](design/BASH_EXTENSION.md) |
| Memory 记忆扩展 | [design/MEMORY_EXTENSION.md](design/MEMORY_EXTENSION.md) |
| Memory Agent (SQLite) | [design/MEMORY_AGENT.md](design/MEMORY_AGENT.md) |
| Compaction 会话压缩 | [design/COMPACTION.md](design/COMPACTION.md) |
| 多 Provider 协议 | [design/PROVIDER_PROTOCOL.md](design/PROVIDER_PROTOCOL.md) |
| 权限系统 | [design/PERMISSION_SYSTEM.md](design/PERMISSION_SYSTEM.md) |
| Session 消息系统 | [design/SESSION_MESSAGE.md](design/SESSION_MESSAGE.md) |
| Session Tree（会话分支） | [design/SESSION_TREE.md](design/SESSION_TREE.md) |
| Shell Hook 系统 | [design/HOOK_SYSTEM.md](design/HOOK_SYSTEM.md) |
| Apple Container | [design/APPLE_CONTAINER_EXTENSION.md](design/APPLE_CONTAINER_EXTENSION.md) |
| Extension 生态 | [design/EXTENSION_ECOSYSTEM.md](design/EXTENSION_ECOSYSTEM.md) |
| FauxProvider (LLM Mock) | [design/FAUX_PROVIDER.md](design/FAUX_PROVIDER.md) |
| Record/Replay | [design/RECORD_REPLAY.md](design/RECORD_REPLAY.md) |
| Crash Recovery | [design/CRASH_RECOVERY.md](design/CRASH_RECOVERY.md) |
| Team 编排（agent.md 驱动） | [design/TEAM_ORCHESTRATION.md](design/TEAM_ORCHESTRATION.md) |
| Workflow Gate（内核交付校验） | [design/WORKFLOW_GATE.md](design/WORKFLOW_GATE.md) |
| Workflow Engine（结构化交付流水线） | [design/WORKFLOW_ENGINE.md](design/WORKFLOW_ENGINE.md) |
| CLI 完整方案 | [design/CLI_PLAN.md](design/CLI_PLAN.md) |
| CLI 架构设计 | [design/CLI_ARCHITECTURE.md](design/CLI_ARCHITECTURE.md) |
| CLI 路线图 | [design/CLI_ROADMAP.md](design/CLI_ROADMAP.md) |
| pi RPC 对齐 | [design/PI_RPC_ALIGNMENT.md](design/PI_RPC_ALIGNMENT.md) |
| MCP 系统（Model Context Protocol） | [design/MCP_SYSTEM.md](design/MCP_SYSTEM.md) |
| MCP 方案 C 共享池 | [design/MCP_PLAN_C.md](design/MCP_PLAN_C.md) |
| 配置维度分析 | [design/CONFIG_DIMENSIONS.md](design/CONFIG_DIMENSIONS.md) |
| File Snapshot 双路快照 | [design/FILE_SNAPSHOT.md](design/FILE_SNAPSHOT.md) |
| File Snapshot 对齐 | [design/FILE_SNAPSHOT_REVIEW_ALIGNMENT.md](design/FILE_SNAPSHOT_REVIEW_ALIGNMENT.md) |
| Context Index（上下文索引） | [design/CONTEXT_INDEX.md](design/CONTEXT_INDEX.md) |
| 软删除/软压缩 | [design/SOFT_DELETE_COMPACT.md](design/SOFT_DELETE_COMPACT.md) |
| 消息拉取设计 | [design/MESSAGE_RETRIEVAL_DESIGN.md](design/MESSAGE_RETRIEVAL_DESIGN.md) |

### 我想查测试用例

| 测试 | 文档 |
|------|------|
| 完整测试用例 | [testing/TEST_CASES.md](testing/TEST_CASES.md) |
| Session Tree 验收规格 | [testing/SESSION_TREE_SPEC.md](testing/SESSION_TREE_SPEC.md) |
| 消息拉取 CLI 用例 | [testing/MESSAGE_RETRIEVAL_CASES.md](testing/MESSAGE_RETRIEVAL_CASES.md) |

### 我想查 CLI 命令怎么用

- [guides/CLI_USAGE.md](guides/CLI_USAGE.md) — 命令速查
- [guides/DEPLOY_ARCH.md](guides/DEPLOY_ARCH.md) — 部署场景
- 各 design 文档的"CLI 测试指南"章节（Group A/B/C/D 格式）

### 我想开发 WASM 扩展

- [guides/EXTENSION_WORKFLOW.md](guides/EXTENSION_WORKFLOW.md) — 开发测试闭环
- [templates/EXTENSION_MANUAL_TEMPLATE.md](templates/EXTENSION_MANUAL_TEMPLATE.md) — 手册模板
- [design/EXTENSION_SYSTEM.md](design/EXTENSION_SYSTEM.md) — 系统设计

### 我想写新功能的设计文档

→ 用 [templates/DESIGN_TEMPLATE.md](templates/DESIGN_TEMPLATE.md)

### 我想给功能写 CLI 测试

→ 用 [templates/CLI_TEST_TEMPLATE.md](templates/CLI_TEST_TEMPLATE.md)

### 我想给功能写测试规格（给评审方）

→ 用 [templates/TEST_SPEC_TEMPLATE.md](templates/TEST_SPEC_TEMPLATE.md)

### 我想调研 pi 某项能力并规划对齐

→ 用 [templates/PI_ALIGNMENT_TEMPLATE.md](templates/PI_ALIGNMENT_TEMPLATE.md)

### 我想查测试用例

→ [testing/TEST_CASES.md](testing/TEST_CASES.md)

---

## 文档规范

详见 [AGENTS.md §文档规范](../AGENTS.md)。要点：

1. **根目录只保留** `AGENTS.md` + `README.md` + 标准配置文件（Cargo.toml / Makefile / .gitignore）
2. **新文档必须放到对应子目录**：design / guides / templates / testing
3. **每个文档开头标注状态**：已完成 / 已验证 / 开发中 / 暂不开发 / 待定
4. **术语规范**：统一用 "extension"，禁止 "plugin" / "插件"
5. **写新文档前先查模板**：[templates/](templates/)

---

## 归档说明

`archive/` 目录保留被合并或被替代的旧文档，仅供历史查阅。**不要在 archive/ 里维护内容**——如果旧文档有未迁移的内容，应该合并到 design/ 对应文档。

| 归档文档 | 去向 | 原因 |
|---------|------|------|
| BASH_EXTENSION.md / BASH_API_CHECKLIST.md / BASH_EXTENSION_TUTORIAL.md | design/BASH_EXTENSION.md | 3 文档合并 |
| PERMISSION_SYSTEM.md / PERMISSION_CLI.md / PERMISSION_TESTS.md / SECURITY_CLI_GUIDE.md | design/PERMISSION_SYSTEM.md | 4 文档合并 |
| SESSION_MESSAGE.md / SESSION_MESSAGE_TYPES.md | design/SESSION_MESSAGE.md | 2 文档合并 |
| MEMORY_EXTENSION.md / MEMORY_SPEC.md | design/MEMORY_EXTENSION.md | 2 文档合并 |
| RPC_DIFF_REPORT.md | design/PI_RPC_ALIGNMENT.md | 被替代 |
| WORKER_COMM_TODO.md | — | 任务已完成 |

---

## Self-Evolution (A→B Architecture)

| Document | Content |
|----------|---------|
| [design/SELF_EVOLUTION.md](./design/SELF_EVOLUTION.md) | A→B architecture overview |
| [design/EVOLVER_LESSONS_LEARNED.md](./design/EVOLVER_LESSONS_LEARNED.md) | 18 real problems + solutions |
| [design/WATCHDOG_DUAL_VERSION.md](./design/WATCHDOG_DUAL_VERSION.md) | Watchdog dual-version design (pending) |

## Agent Definitions (examples/agents/)

| Agent | Role |
|-------|------|
| coordinator.md | Orchestrate via spawn_worker (sync + async) |
| developer.md | Write code in container (with safety rules) |
| reviewer.md | Review changes (structured checklist) |
| evolver_agent.md | Self-evolution orchestrator guide |
