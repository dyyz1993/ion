# 回滚影响验证 — Context / Message / Compaction

> **状态：实测态（2026-07-16）** — Rust harness 18 case 全过（含 F1/F2/F3 bug 暴露 + Group TC token/compaction/context 验证）。Bash CI 因 serve session 持久化 bug 暂不可用（见 §已知限制）。

---

## 目标

验证回滚（`--rollback` / `make_rollback`）对三大层面的影响：
1. **Context（LLM 上下文）** — 回滚后被回滚的消息是否还在 context 里
2. **Message 检索** — `get_messages(live/full/branch)` 是否正确过滤
3. **Compaction** — token 计数和压缩判定是否受回滚影响

覆盖用户关心的场景：
- 修改后回滚→继续聊
- 先闲聊→再改代码→回滚那个闲聊
- 多次交替（回滚→聊→回滚→聊）

---

## 已发现的差异（Bug 清单）

> **这三个差异在测试中被成功暴露，是文档与实现不符的根因。修复后需更新对应测试的断言。**

### F1：`SessionFile::load` 不过滤 leaf_pointer（🔴 Context 污染）

| 维度 | 内容 |
|------|------|
| **文档声称** | 回滚后被回滚的消息不在 context（`SESSION_TREE.md §0.2`：load 时 LLM 收到的 messages 是 root→current_leaf 路径） |
| **代码实际** | `SessionFile::load`（`session_jsonl.rs:422-430`）加载全部 `type=="message"` entry，**不读 leaf_pointer** |
| **影响** | 回滚后继续聊，LLM 仍看到被回滚的消息 → context 污染 + token 虚高 + compaction 误触发 |
| **暴露测试** | `c2_load_does_not_filter_leaf_pointer`（assert messages.len()==4 而非期望的 2）、`k1_messages_count_includes_rolled_back` |
| **修复方向** | `SessionFile::load` 加 leaf 过滤，或 Agent 维护 `deleted_entry_ids` 并在 load 后过滤 |

### F2：`turn_summary.entryRange` 恒为空（🟡 --restore-code 失效）

| 维度 | 内容 |
|------|------|
| **文档声称** | `--restore-code` 靠 `entryRange` 匹配 entry → 找到 turnId → 恢复代码（`FILE_SNAPSHOT.md §19.3`） |
| **代码实际** | `agent_loop.rs:1390`: `&[], // entryRange 暂空（内存 Message 无 entryId）` |
| **影响** | `--restore-code` 对真实 turn 永远找不到 turnId → 跳过代码恢复（`ion.rs:2095-2096` 的匹配失败） |
| **暴露测试** | `t2_entry_range_empty` |
| **修复方向** | 给内存 Message 打 entryId，或 persist_turn_summary 时从 session file 读 entry range |

### F3：turnId 每次 run 从 0 重置（🟡 turnId 重复）

| 维度 | 内容 |
|------|------|
| **文档声称** | turnId 全局递增，回滚后继续聊不覆盖历史快照（`FILE_SNAPSHOT.md §19`） |
| **代码实际** | `agent_loop.rs:494-495`: `self.turn_index = 0`（每次 run 开头重置） |
| **影响** | 回滚后继续聊，turn_summary 的 turnId 重复（如 `[0,1,0,1]`）。快照层用了独立的 `gen_turn_id`（不受影响），但 turn_summary 查询会混淆 |
| **暴露测试** | `t1_turnid_should_not_repeat`（检测到 `[0,1,0,1]` 重复） |
| **修复方向** | turn_index 从 session file 读最大 turnId + 1，不重置 |

### 附：serve session 持久化 bug（Bash CI 不可用的原因）

| 维度 | 内容 |
|------|------|
| **现象** | `ion serve` 的 `create_session` 创建的 session，文件 header 的 `cwd` 字段为空，且不写 `SessionIndex` |
| **影响** | `ion --resume <SID> --rollback` 通过 index 查 cwd 失败 → `entry not found` → rollback 无法执行 |
| **当前状态** | Rust harness 绕过此问题（直接调 API），Bash CI 暂跳过 rollback CLI 验证 |

---

## 测试架构

### 第 1 层：Rust Harness（`tests/rollback_harness.rs`）— ✅ 18 case 全过

直接调 `session_tree::make_rollback` + `SessionFile` + `message_retrieval` + `compact` API，不走 CLI。
用 `ion_provider::types::Message` 构造格式正确的消息 entry。

**断言策略**：
- 正向行为：assert 期望结果（retrieval 过滤、磁盘不动、only-append）
- Bug 暴露：assert bug 存在（F1: messages.len()==4、F3: turnId 有重复）

| Group | Case | 验证 | 结果 |
|-------|------|------|------|
| C | c1_live_excludes_rolled_back | retrieval `View::Live` 过滤被回滚消息 | ✅ live=2 < full=4 |
| C | c2_load_does_not_filter_leaf_pointer | **F1 暴露**：SessionFile::load 不过滤 | ✅ messages=4（含被回滚） |
| C | c3_rollback_chain_no_leak | 多次回滚废弃分支不泄漏 | ✅ live=2 |
| M | m1_full_includes_all | `View::Full` 含全部历史 | ✅ full=4 |
| M | m2_branch_view_finds_abandoned | `View::Branch` 查废弃分支 | ✅ 4 条 |
| M | m3_resolve_leaf_after_rollback | resolve_current_leaf 指向回滚目标 | ✅ |
| K | k1_messages_count_includes_rolled_back | **F1 暴露**：messages 含被回滚 | ✅ messages=4 |
| K | k2_rollback_across_compaction_rejected | 穿越压缩点被拒绝 | ✅ safety=Some(...) |
| T | t1_turnid_should_not_repeat | **F3 暴露**：turnId 重复 | ✅ [0,1,0,1] 有重复 |
| T | t2_entry_range_empty | **F2 根因**：entryRange 空 | ✅ |
| S | s1_pure_rollback_no_disk_change | 纯消息回滚磁盘不变 | ✅ V2 保留 |
| S | s2_rollback_is_append_only | only-append（消息不丢） | ✅ 4 条全在 |
| TC | tc1_tokens_include_rolled_back | **F1 暴露**：total_tokens 含被回滚消息 | ✅ tokens > 150（含 4 条） |
| TC | tc2_needs_compact_false_positive | **F1 暴露**：needs_compact 误触发 | ✅ live 2 条小消息但误判需压缩 |
| TC | tc3_context_length_inflated | **F1 暴露**：context 消息数虚高 | ✅ live=2, context=6（差 4 条） |
| TC | tc4_fixed_token_would_be_correct | 修复后预期：live path token 正确 | ✅ live 2 条 token < 100，不压缩 |
| TC | tc5_compaction_safety_independent_of_f1 | compaction 安全检查独立于 F1 | ✅ 穿越拒绝，之后允许 |
| TC | tc6_token_accumulates_across_rollbacks | **F1 暴露**：多次回滚 token 累积 | ✅ context=6 条，live=2 条 |

**运行：**
```bash
cargo test --test rollback_harness -- --nocapture
```

### 第 2 层：Bash CI（`tests/rollback_impact_ci.sh`）— ⚠️ serve bug 待修

仿 `file_snapshot_ci.sh` 的 serve + FauxProvider + rpc 模式。测真实 CLI 全链路。

**当前状态**：因 serve session 持久化 bug（header cwd 为空 + 不写 index），`--rollback` CLI 找不到 session。脚本已写好框架，待 serve bug 修复后启用。

---

## 用户场景验证结果

### 场景 1：修改后回滚→继续聊

**文档支持**：✅ 设计正确（`SESSION_TREE.md §0.2` + `FILE_SNAPSHOT.md §19/20`）

**实际验证**：
- ✅ retrieval 层正确过滤（live < full）
- ✅ 磁盘不受纯消息回滚影响
- ✅ only-append（历史不丢）
- 🔴 **context 仍含被回滚消息**（F1）——LLM 看到废弃分支
- 🔴 **turnId 重复**（F3）——turn_summary 混淆

### 场景 2：先闲聊→改代码→回滚那个闲聊

**文档支持**：✅ 纯消息回滚（`--rollback` 不带 `--restore-code`）不动磁盘

**实际验证**：
- ✅ 纯消息回滚后磁盘仍 = V2（代码不动）
- ✅ `View::Full` 能查到全部历史（含 V1→V2 diff 的快照层独立）
- 🔴 **`--restore-code` 失效**（F2）——entryRange 空，找不到 turnId

### 场景 3：多次交替（回滚→聊→回滚→聊）

**文档支持**：✅ `FILE_SNAPSHOT.md §20` 的 M1-M9 case 矩阵

**实际验证**：
- ✅ only-append 完整（leaf_pointer 只增）
- ✅ 废弃分支不泄漏到 live path
- 🔴 **turnId 重复**（F3）——每次 run 从 0 重置

---

## 修复优先级建议

| Bug | 优先级 | 理由 |
|-----|--------|------|
| **F1** | P0 | context 污染直接影响 LLM 行为（看到废弃消息会产生幻觉）；也导致 token 虚高 + compaction 误判（见 Group TC） |
| **F2** | P1 | `--restore-code` / `restore_files` RPC 对真实 turn 完全失效；阻塞"回滚代码"UI 按钮 |
| **F3** | P2 | turn_summary 查询受影响，但不影响 LLM 行为 |
| **G1** | P1 | `restore_code_to_turn` 缺 preview 参数，UI 无法预览代码回滚影响 |
| serve bug | P1 | 阻塞所有 CLI 级 rollback 测试 |

---

## UI 回滚交互的接口缺口（G1-G3）

> 基于产品设计讨论：用户点某条消息 → 预览影响 → 两个按钮（回滚消息 / 回滚代码）。
> 以下是当前接口对这个交互的支持情况和缺口。

### UI 交互流程（期望）

```
用户点某条消息（entry）
  ↓
拉取预览：这轮改了哪些文件 + diff（get_modified_files + get_batch_diffs）
  ↓
┌─────────────────┐  ┌─────────────────┐  ┌─────────────────────┐
│ [回滚消息]       │  │ [回滚代码]       │  │ [回滚消息+代码]      │
│ 代码不动         │  │ 消息不动         │  │ 两者都回滚           │
│ = --rollback     │  │ = restore_files  │  │ = --rollback         │
│   (无 restore)   │  │   RPC            │  │   --restore-code     │
└─────────────────┘  └─────────────────┘  └─────────────────────┘
```

### 接口支持矩阵

| # | UI 需求 | 当前接口 | 状态 | 缺口 |
|---|---------|---------|:----:|------|
| 1 | 预览回滚影响哪些文件 | `get_modified_files(fromTurn)` | ✅ | — |
| 2 | 预览每个文件的 diff | `get_file_diff` / `get_batch_diffs` | ✅ | — |
| 3 | [回滚消息] 按钮（代码不动） | `--rollback`（无 restore-code） | ✅ | 语义正确，但 F1 导致后续 context 污染 |
| 4 | [回滚代码] 按钮（消息不动） | `restore_files` RPC | ⚠️ | **F2**：entryRange 空 → 找不到 turnId → 跳过 |
| 5 | [回滚代码] 的 preview（不写盘） | `restore_to_tree` 有，`restore_code_to_turn` 没有 | ❌ | **G1**：缺 preview 参数 |
| 6 | 回滚后 context 不含被回滚消息 | `SessionFile::load` | ❌ | **F1**：不过滤 leaf_pointer |

### G1：`restore_code_to_turn` 缺 preview 参数

| 维度 | 内容 |
|------|------|
| **现状** | `restore_single_file` 和 `restore_to_tree` 都有 `preview: bool` 参数；但 `restore_code_to_turn`（`--restore-code` delta 模式调的函数）**没有 preview** |
| **影响** | UI 想预览"回滚代码后会发生什么"（哪些文件恢复/删除），一调就真写了磁盘，无法做 dry-run 预览 |
| **修复** | 给 `restore_code_to_turn` 加 `preview: bool` 参数（参照 `restore_to_tree` 的 preview 实现），preview=true 时返回 `RestoredFile` 列表但不写盘 |
| **代码位置** | `src/file_snapshot/restore.rs:60` — `restore_code_to_turn(store, target_turn_id)` → 加 `preview` 参数 |

### G2：`restore_files` RPC 缺 preview 参数

| 维度 | 内容 |
|------|------|
| **现状** | `restore_files` RPC（`ion_worker.rs`）直接调 `restore_code_to_turn(store, turn_id)`，无 preview 选项 |
| **影响** | 即使 G1 修了（函数层加 preview），RPC 层也需要暴露 preview 参数让 UI 调用 |
| **修复** | `restore_files` RPC 的 params 加 `"preview": true`，传给 `restore_code_to_turn` |

### G3：`--restore-code` 与 F2 的交互

| 维度 | 内容 |
|------|------|
| **现状** | `--rollback <id> --restore-code` 的代码路径（`ion.rs:2125-2134`）靠 `entryRange` 匹配 entry → 找 turnId → 调 `restore_code_to_turn` |
| **影响** | F2（entryRange 恒空）导致这条路径永远找不到 turnId → 跳过代码恢复。用户点"回滚消息+代码"按钮，实际只有消息回滚了 |
| **修复** | 修 F2（persist_turn_summary 时填 entryRange），或改为用 entry id 直接查 turnId（不依赖 entryRange） |

---

## 参考

- [SESSION_TREE.md](../design/SESSION_TREE.md) — 回滚设计（§0.2 回滚语义）
- [FILE_SNAPSHOT.md](../design/FILE_SNAPSHOT.md) — 代码恢复（§17 restore_files、§19 turnId 全局唯一性、§20 交替操作 case）
- [SOFT_DELETE_COMPACT.md](../design/SOFT_DELETE_COMPACT.md) — 软删除/软压缩（context 层过滤）
- [COMPACTION.md](../design/COMPACTION.md) — 压缩设计（threshold / needs_compact / emergency fallback）
- `src/agent/compact.rs` — `total_tokens` / `needs_compact` / `CompactConfig`（Group TC 验证对象）
- `tests/rollback_harness.rs` — Rust harness 源码（18 case）
- `tests/rollback_impact_ci.sh` — Bash CI 框架（待 serve bug 修复后启用）
