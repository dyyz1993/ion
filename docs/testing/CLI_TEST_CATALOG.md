# ION CLI 测试指南总目录

> **状态：已完成** — 覆盖 45 个功能模块，54 个 CI 脚本，~500+ CLI 验证 case。
>
> 每个模块按功能类别分组，列出：设计文档 → CI 脚本 → Group 数 → Case 数 → 覆盖状态。

## 统计总览

| 状态 | 模块数 | 说明 |
|------|--------|------|
| ✅ 已对齐 | 22 | 设计文档有 Group 章节 + 对应 CI 脚本 |
| 🔶 部分对齐 | 7 | CI 存在但设计文档缺正式 Group 章节（或反之） |
| 📋 仅 CI | 11 | CI 脚本存在，无设计文档 Group 章节 |
| ⚠️ 设计有/CI 缺 | 2 | 设计文档有 Group 但无 CI 脚本 |
| ❌ 完全未覆盖 | 3 | 无设计文档 Group、无 CI 脚本 |
| **总计** | **45** | |

**CI 脚本总计**：54 个 `tests/*_ci.sh` + 12 个 `tests/e2e/group_*.sh` + 928 个 Rust 单元/集成测试

---

## 1. 基础执行 / 会话

| 模块 | 设计文档 | CI 脚本 | Group | Case | 状态 |
|------|---------|--------|-------|------|------|
| CLI flag 对齐 | [CLI_ARCHITECTURE](../design/CLI_ARCHITECTURE.md) §A1-1/A1-3 | `cli_alignment_ci.sh` | 14 | 46 | ✅ 已对齐 |
| CLI 落地方案 | [CLI_PLAN](../design/CLI_PLAN.md) §四 | (via cli_alignment_ci) | 16 | — | ✅ 已对齐 |
| 会话持久化 | [SESSION_MESSAGE](../design/SESSION_MESSAGE.md) §七 | `sessions_ci.sh` | 4 | 20 | ✅ 已对齐 |
| 会话条目 | — | `session_entries_ci.sh` | — | 10 | 📋 仅 CI |
| 中断/abort | — | `abort_ci.sh` + `soft_interrupt_ci.sh` + `overflow_recovery_ci.sh` | 3 | 14 | 📋 仅 CI |
| Crash Recovery | [CRASH_RECOVERY](../design/CRASH_RECOVERY.md) §5 | `crash_recovery_ci.sh` | 4 | 6 | ✅ 已对齐 |
| Record/Replay | [RECORD_REPLAY](../design/RECORD_REPLAY.md) §9 | `record_replay_ci.sh` + `streaming_replay_ci.sh` | 3 | 18 | 🔶 部分对齐 |
| Streaming | — | `streaming_throughput_ci.sh` + `realtime_stitch_ci.sh` | 4 | 25 | 📋 仅 CI |

---

## 2. RPC / 工具

| 模块 | 设计文档 | CI 脚本 | Group | Case | 状态 |
|------|---------|--------|-------|------|------|
| RPC 协议 | — | `unit_rpc_test.rs` (20 cases) | — | 20 | ✅ 已对齐 (Rust) |
| 工具系统 | [E2E_TEST_SPEC](E2E_TEST_SPEC.md) Group E | `tests/e2e/group_e_tools.sh` | 1 | 12 | ✅ 已对齐 |
| Skill 工具 | [SKILL_TOOL](../design/SKILL_TOOL.md) §4 | `skill_tool_ci.sh` | 4 | 27 | ✅ 已对齐 |
| Bash 扩展 | [BASH_EXTENSION](../design/BASH_EXTENSION.md) §13 | (via runtime_ci + permission_ci) | 5 | 18 | ✅ 已对齐 |
| Plan 工具 | AGENTS.md §plan | (via extensions_ci) | — | — | 📋 仅 CI |

---

## 3. Memory

| 模块 | 设计文档 | CI 脚本 | Group | Case | 状态 |
|------|---------|--------|-------|------|------|
| Global Memory | — | `global_memory_ci.sh` | 3 | 8 | 📋 仅 CI |
| Memory Agent | [MEMORY_AGENT](../design/MEMORY_AGENT.md) §extension_rpc | `memory_agent_ci.sh` | 3 | 10 | 🔶 部分对齐 |
| Memory Active (V0.2/V0.3) | [MEMORY_ACTIVE](../design/MEMORY_ACTIVE.md) | `memory_active_ci.sh` + `memory_v2_processing_ci.sh` + `memory_injection_ci.sh` | 6 | 44 | ✅ 已对齐 |
| Soft Delete/Compact | [SOFT_DELETE_COMPACT](../design/SOFT_DELETE_COMPACT.md) | `soft_delete_ci.sh` | — | 7 | 🔶 部分对齐 |
| Compaction | [COMPACTION](../design/COMPACTION.md) §9 | `compaction_ci.sh` | 4 | 16 | ✅ 已对齐 |

---

## 4. 消息 / SSE / 事件

| 模块 | 设计文档 | CI 脚本 | Group | Case | 状态 |
|------|---------|--------|-------|------|------|
| 消息拉取 | [MESSAGE_RETRIEVAL_CASES](MESSAGE_RETRIEVAL_CASES.md) | `message_retrieval_ci.sh` | 13 | 55 | ✅ 已对齐 |
| Message Source Tag | [MESSAGE_SOURCE_TAG](../design/MESSAGE_SOURCE_TAG.md) §6 | `message_source_ci.sh` | 5 | 9 | ✅ 已对齐 |
| SSE 事件 | — | `sse_events_ci.sh` | 5 | 13 | 📋 仅 CI |
| 导出 (export) | — | `export_ci.sh` | 4 | 18 | 📋 仅 CI |

---

## 5. File Snapshot / LSP / Hooks

| 模块 | 设计文档 | CI 脚本 | Group | Case | 状态 |
|------|---------|--------|-------|------|------|
| File Snapshot | [FILE_SNAPSHOT](../design/FILE_SNAPSHOT.md) §11 + [FILE_SNAPSHOT_CASES](FILE_SNAPSHOT_CASES.md) | `file_snapshot_ci.sh` (665 行) | 12 | 38 | ✅ 已对齐 |
| Rollback Impact | [ROLLBACK_IMPACT_CASES](ROLLBACK_IMPACT_CASES.md) | `rollback_impact_ci.sh` | 3 | 26 | ✅ 已对齐 |
| LSP 扩展 | [LSP_EXTENSION](../design/LSP_EXTENSION.md) §4 + [LSP_CLI_TEST](HOOKS_CLI_TEST.md) | `lsp_ci.sh` | 5 | 14 | ✅ 已对齐 |
| Hooks 系统 | [HOOKS_AND_OUTLINE_SYNC](../design/HOOKS_AND_OUTLINE_SYNC.md) + [HOOKS_CLI_TEST](HOOKS_CLI_TEST.md) | `hooks_ci.sh` + `hooks_agent_ci.sh` + `hooks_handler_ci.sh` + `session_hook_ci.sh` | 8 | 26 | ✅ 已对齐 |
| Context Index | [CONTEXT_INDEX](../design/CONTEXT_INDEX.md) §9 | `context_index_e2e.rs` (Rust) | 4 | 3 | ⚠️ **设计有/CI 缺** |

---

## 6. 权限 / UI

| 模块 | 设计文档 | CI 脚本 | Group | Case | 状态 |
|------|---------|--------|-------|------|------|
| Permission 系统 | [PERMISSION_SYSTEM](../design/PERMISSION_SYSTEM.md) §十一 | `permission_ci.sh` + `runtime_ci.sh` | 18 | 28 | ✅ 已对齐 |
| Permission Store | [PERMISSION_STORE](../design/PERMISSION_STORE.md) §4 | `permission_store_ci.sh` | 1 | 23 | ✅ 已对齐 |
| Secured Runtime | — | `runtime_ci.sh` | 4 | 16 | 📋 仅 CI |
| UI 集成 | — | `ui_integration_ci.sh` + `p3_ui_ci.sh` | 3 | 18 | 📋 仅 CI |
| Audit 日志 | — | `p3_audit_ci.sh` | — | 7 | 📋 仅 CI |
| Apple Container | [APPLE_CONTAINER_EXTENSION](../design/APPLE_CONTAINER_EXTENSION.md) | `apple_container_ci.sh` | 8 | 26 | ✅ 已对齐 |

---

## 7. 扩展系统

| 模块 | 设计文档 | CI 脚本 | Group | Case | 状态 |
|------|---------|--------|-------|------|------|
| Extension System | [EXTENSION_SYSTEM](../design/EXTENSION_SYSTEM.md) §11.4 | (various) | 1 | — | ✅ 已对齐 |
| Extension Host API (fs) | [EXTENSION_HOST_API](../design/EXTENSION_HOST_API.md) §4 | `extension_fs_ci.sh` | 4 | 23 | ✅ 已对齐 |
| Extension CLI (install/list/remove) | AGENTS.md §ion extension | `extension_cli_ci.sh` | 4 | 11 | ✅ 已对齐 |
| Extension Flags | [EXTENSION_SYSTEM](../design/EXTENSION_SYSTEM.md) §11.4 Group F | `extension_flags_ci.sh` | 1 | 10 | ✅ 已对齐 |
| 内置扩展 (todo/plan) | — | `extensions_ci.sh` + `p4_extension_ci.sh` | — | 30 | 📋 仅 CI |
| Extension Ecosystem | [EXTENSION_ECOSYSTEM](../design/EXTENSION_ECOSYSTEM.md) | `p4_extension_ci.sh` + `p4_events_ci.sh` | — | 16 | 🔶 部分对齐 |
| Monitor 扩展 | [MONITOR_EXTENSION](../design/MONITOR_EXTENSION.md) §5 | `monitor_ci.sh` (745 行) | 10 | 37 | ✅ 已对齐 |
| Events | [EXTENSION_ECOSYSTEM](../design/EXTENSION_ECOSYSTEM.md) §2 | `p4_events_ci.sh` | — | 10 | 📋 仅 CI |
| 热重载 | — | `p2_hotreload_ci.sh` | — | 9 | 📋 仅 CI |

---

## 8. Provider / 模型

| 模块 | 设计文档 | CI 脚本 | Group | Case | 状态 |
|------|---------|--------|-------|------|------|
| Provider 协议 | [PROVIDER_PROTOCOL](../design/PROVIDER_PROTOCOL.md) §7 | `ion-provider/tests/e2e_real_api.rs` (Rust) | 5 | 19+6 e2e | ✅ 已对齐 (Rust) |
| Faux Provider | [FAUX_PROVIDER](../design/FAUX_PROVIDER.md) | `faux_scenarios_ci.sh` | 3 | 4 | 📋 仅 CI |
| Tier Models | — | `tier_models_ci.sh` | 1 | 9 | 📋 仅 CI |

---

## 9. 多智能体编排

| 模块 | 设计文档 | CI 脚本 | Group | Case | 状态 |
|------|---------|--------|-------|------|------|
| Team 编排 | [TEAM_ORCHESTRATION](../design/TEAM_ORCHESTRATION.md) | `team_e2e.sh` + `tests/e2e/group_g_team.sh` | — | 18 | 🔶 部分对齐 |
| Scenario 2 (子任务) | — | `scenario2_ci.sh` | 8 | 27 | ✅ 已对齐 |
| Self-Heal Pipeline | [SELF_HEALING_PIPELINE](../design/SELF_EVOLUTION.md) | `self_heal_ci.sh` | 4 | 12 | ✅ 已对齐 |
| Goal Supervisor | [GOAL_SUPERVISOR](../design/GOAL_SUPERVISOR.md) §4 | `goal_supervisor_ci.sh` | 7 | 26 | ✅ 已对齐 |
| Goal Evolver | (referenced) | `goal_evolver_ci.sh` | 6 | 18 | 🔶 部分对齐 |
| Improver Agent | [IMPROVER_AGENT](../design/SELF_EVOLUTION.md) | — | 0 | 0 | ❌ **未覆盖** |
| Self-Evolution | [SELF_EVOLUTION](../design/SELF_EVOLUTION.md) | — | 0 | 0 | ❌ **未覆盖** |

---

## 10. Workflow / MCP

| 模块 | 设计文档 | CI 脚本 | Group | Case | 状态 |
|------|---------|--------|-------|------|------|
| Workflow Engine | [WORKFLOW_ENGINE](../design/WORKFLOW_ENGINE.md) | `workflow_ci.sh` (555 行) | 11 | 15 | ✅ 已对齐 |
| Workflow Gate | [WORKFLOW_GATE](../design/WORKFLOW_GATE.md) §五 | `scenario2_ci.sh` Group A2-9 | 1 | 6 | ✅ 已对齐 |
| MCP 系统 | [MCP_SYSTEM](../design/MCP_SYSTEM.md) §5 | `mcp_ci.sh` (692 行) | 11 | 37 | ✅ 已对齐 |

---

## 11. 其他

| 模块 | 设计文档 | CI 脚本 | Group | Case | 状态 |
|------|---------|--------|-------|------|------|
| Config Dimensions | [CONFIG_DIMENSIONS](../design/CONFIG_DIMENSIONS.md) | — | 7 | 0 | ⚠️ **设计有/CI 缺** |
| Watchdog | [WATCHDOG_DUAL_VERSION](../design/WATCHDOG_DUAL_VERSION.md) | — | 0 | 0 | ❌ **未覆盖** |
| Session Tree | [SESSION_TREE](../design/SESSION_TREE.md) §4 | `session_tree_ci.sh` + `session_tree_verify.sh` | 10 | 15 | ✅ 已对齐 |
| Hooks Agent (real LLM) | — | `hooks_agent_real.sh` | — | 3 | ✅ 已对齐 |

---

## 缺口清单

### ⚠️ 设计有 / CI 缺（需要补 CI 脚本）

| 模块 | 设计文档 | 缺失的脚本 | Group 数 | 说明 |
|------|---------|-----------|---------|------|
| **Context Index** | CONTEXT_INDEX.md §9 | `tests/context_index_ci.sh` | 4 | Groups A-D，有完整 CLI 测试章节但未写成 .sh |
| **Config Dimensions** | CONFIG_DIMENSIONS.md §8 | `tests/config_dimensions_ci.sh` | 7 | Groups A-G，文档标注"实现后编写" |

### ❌ 完全未覆盖（无设计文档 Group、无 CI 脚本）

| 模块 | 说明 |
|------|------|
| **Improver Agent** | `IMPROVER_AGENT.md` 无 CLI 测试章节，无 CI 脚本 |
| **Self-Evolution** | `SELF_EVOLUTION.md` 无 CLI 测试章节，无 CI 脚本（evolve_*.sh 是手动脚本） |
| **Watchdog** | `WATCHDOG_DUAL_VERSION.md` 无 CLI 测试章节，无独立 CI 脚本 |

---

## Top 5 最大 CI 脚本

| 排名 | 脚本 | 行数 | Case 数 | Group 数 |
|------|------|------|---------|---------|
| 1 | `monitor_ci.sh` | 745 | 37 | A-J (10) |
| 2 | `message_retrieval_ci.sh` | 714 | 58 | A-N+P (14) |
| 3 | `mcp_ci.sh` | 692 | 37 | A-J (10) |
| 4 | `file_snapshot_ci.sh` | 665 | 38 | A-L (12) |
| 5 | `workflow_ci.sh` | 555 | 15 | W1-W7 (11) |

---

## 标准格式

参照 [docs/templates/CLI_TEST_TEMPLATE.md](../templates/CLI_TEST_TEMPLATE.md)：

```
### RPC 接口规格
请求：ion rpc --session <sid> --method <method> --params '{...}'
请求参数表
响应 JSON（成功/失败）
验证点清单（✅）

### Group A: 基础功能
#### A1 查询 xxx
ion rpc ... → 响应 JSON → 验证点
```

每个 Group 按**用户场景**分（不按技术维度），核心链路全覆盖。
