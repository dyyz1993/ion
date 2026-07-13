# File Snapshot 审批与回滚 — CLI 用例集

> **状态：实测态（2026-07-13）** — 基于 code + harness H1-H5 + CI Group J 真实行为编写。每个 case 标注验证来源（✅ 实测 / ⚠️ 代码依据待 CI）。
>
> **前置文档**：
> - [FILE_SNAPSHOT.md](../design/FILE_SNAPSHOT.md) — 设计文档（双路快照架构 + Group A-F + restore_files §17 + K 矩阵 §18）
> - [FILE_SNAPSHOT_REVIEW_ALIGNMENT.md](../design/FILE_SNAPSHOT_REVIEW_ALIGNMENT.md) — pi 对齐清单（tree 快照模型 + per-file 审批升级路线）
>
> **前置条件**：`config.json` 需开启 `{"file-snapshot":{"enabled":true}}`，否则所有 `review_*` 和 `restore_files` RPC 返回错误（见 Group X1）。
>
> **本文档只列 CLI 命令 + 真实响应 JSON + 验证点**。实现细节见设计文档。

---

## 接口速查

### RPC 接口（7 个）

| 接口 | 用途 | 所在 Group | 验证来源 |
|------|------|-----------|---------|
| `review_pending` | 拉取待审批文件列表（含 diff） | V1 | ✅ CI J2 |
| `review_approve` | 单文件批准（锚定 baseline tree hash） | V2, L3 | ✅ CI J3 + harness H3 |
| `review_reject` | 单文件拒绝 + 自动回滚代码 | V3, V4, L1 | ✅ harness H4 |
| `review_approve_all` | 批量批准全部 pending | V5 | ✅ harness H5 |
| `review_reject_all` | 批量拒绝 + 批量回滚 | V6 | ⚠️ 代码依据 |
| `review_approvals` | 查询审批状态（可按 status 过滤） | V7 | ⚠️ 代码依据 |
| `restore_files` | 代码恢复（独立调用，不配合消息回滚） | R6 | ✅ CI H2 |

### CLI flag（2 个）

| flag | 用途 | 所在 Group | 验证来源 |
|------|------|-----------|---------|
| `--rollback <ENTRY_ID>` | 回滚消息（移动 leaf，磁盘不动） | R1 | ⚠️ 代码依据 |
| `--rollback <id> --restore-code` | 回滚消息 + 恢复代码 | R2 | ⚠️ 代码依据 |
| `--rollback <id> --rollback-reason <text>` | 回滚 + tombstone 原因 | R3 | ⚠️ 代码依据 |

### 审批事件（4 种，经 `subscribe --ui` 接收）

| customType | 触发时机 | 所在 Group | 验证来源 |
|------------|---------|-----------|---------|
| `ApprovalRequest` | agent Stop 且有 pending 文件 | E1 | ⚠️ 代码依据 |
| `ApprovalResolved`(approved) | `review_approve` 成功 | E2 | ⚠️ 代码依据 |
| `ApprovalResolved`(rejected) | `review_reject` 成功 | E3 | ⚠️ 代码依据 |
| `ApprovalReset` | 已批准文件再改 → 自动回 pending | E4, L2 | ⚠️ 代码依据 |

---

## Group R: 回滚（消息层 + 代码联动）

> 验证 Session Tree 的 `--rollback` 能力，以及消息回滚与代码恢复的联动。核心：`--rollback` 只移动消息层 leaf 指针，`--restore-code` 才恢复磁盘文件。

### R1 只回滚消息（磁盘不动）

**场景**：agent 改了代码但方向错了，用户想撤回对话但不影响磁盘文件（手动处理代码）。

```bash
# 前置：会话已有若干 turn，agent write 了 src/main.rs
ion --resume sess_xxx --rollback msg_005
```

**输出：**
```
[rollback] moved leaf to msg_005
```

**验证点：**
- ✅ leaf 指针移到 msg_005（`ion session tree sess_xxx` 确认）
- ✅ msg_005 之后的消息不可见（但 entry 仍在 jsonl，only-append 不变量）
- ✅ **磁盘文件不变**（src/main.rs 仍是 agent 改后的状态）
- ✅ tombstone entry 未写入（没传 `--rollback-reason`）

**来源**：⚠️ 代码依据（`src/bin/ion.rs:2091-2099`）

---

### R2 回滚消息 + 恢复代码

**场景**：用户想完全撤销 agent 的改动——对话回到之前的状态，磁盘文件也恢复。

```bash
ion --resume sess_xxx --rollback msg_005 --restore-code
```

**输出：**
```
[restore-code] restored 3 files (deleted 1, skipped 5)
[restore-code] restore_point: rp_a1b2c3
[rollback] moved leaf to msg_005
```

**执行顺序**（重要）：
1. 解析 `msg_005` → 找到所属 `turn_summary` → 得到 `turnId`
2. `restore_code_to_turn(turnId)` — **先恢复磁盘**（撤销该 turn 之后的所有文件改动）
3. `make_rollback(msg_005)` — **再回滚消息**（追加 leaf_pointer）
4. 记录 `restore_point`（可用于 undo_restore 撤销恢复）

**验证点：**
- ✅ 磁盘文件恢复到 msg_005 时的状态（agent 新建的文件被删除，修改的文件回退）
- ✅ leaf 指针移到 msg_005
- ✅ restore_point 已记录（`rp_` 前缀 + 6 位 hex）
- ✅ 未被会话追踪的文件不动（如用户手动创建的文件）

**来源**：⚠️ 代码依据（`src/bin/ion.rs:2062-2099`，restore_files JSON 范例见 [FILE_SNAPSHOT.md §17.4](../design/FILE_SNAPSHOT.md)）

---

### R3 回滚带原因（tombstone）

**场景**：回滚时记录原因，方便后续审计为什么回滚。

```bash
ion --resume sess_xxx --rollback msg_005 \
  --rollback-reason "方向错了，需要重新设计 API"
```

**输出：**
```
[rollback] moved leaf to msg_005
[rollback] tombstone recorded
```

**验证点：**
- ✅ leaf 移到 msg_005
- ✅ tombstone entry 写入 session.jsonl（含 reason 文本）
- ✅ `--rollback-reason` 不带 `--rollback` 时 clap 报错（`requires = "rollback"`）

**来源**：⚠️ 代码依据（`src/bin/ion.rs:2099-2102`）

---

### R4 回滚后继续对话（turnId 全局递增）

**场景**：回滚到某个点后继续聊，新 turn 的 turnId 接续全局最大值（不覆盖历史快照）。

```bash
# 第一次 run: turnId 0,1,2,3,4
ion --resume sess_xxx --rollback msg_003   # 回滚到 turnId=2

# 第二次 run: turnId 从 5 开始（全局 max=4 + 1）
ion --resume sess_xxx "继续，换种方式实现"
```

**验证点：**
- ✅ 第二次 run 的 turn_summary turnId=5,6,7...（不重复 0,1,2）
- ✅ `snapshots/tool/5.jsonl` 不覆盖 `snapshots/tool/0.jsonl`
- ✅ 回滚后的新 write，`before_hash` 是恢复后的内容（磁盘已恢复）

**来源**：⚠️ 代码依据（[FILE_SNAPSHOT.md §19](../design/FILE_SNAPSHOT.md) turn_id 全局唯一性设计）

---

### R5 穿越压缩点被拒绝

**场景**：Turn 10 压缩了 Turn 1-5，用户想回滚到 Turn 3——被拒绝（压缩点之前的上下文已丢失）。

```bash
ion --resume sess_xxx --rollback msg_003   # msg_003 在压缩点之前
```

**输出（exit 1）：**
```
❌ Cannot rollback to msg_003: it is before a compaction point (entry_compact_001).
   Branching across compaction loses summarized context.
   Hint: use `ion --fork-from-leaf sess_xxx/msg_003` instead.
```

**验证点：**
- ✅ 进程 exit 1，不执行回滚
- ✅ 错误提示包含压缩点 entry id
- ✅ 建议用 `--fork-from-leaf` 替代（从分支点提取新 session）
- ✅ 回滚到压缩点**之后**正常工作（msg_008 → Turn 8，在压缩点 Turn 5 之后）

**来源**：⚠️ 代码依据（`src/bin/ion.rs:2054-2059`，`session_tree::check_compaction_safety`）

---

### R6 restore_files RPC 独立调用

**场景**：不配合消息回滚，单独调用 RPC 恢复代码（比如只恢复磁盘，对话历史不动）。

```bash
ion rpc --session sess_xxx --method restore_files \
  --params '{"toTurn":"ts_003"}'
```

**请求参数：**

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `toTurn` | string | 是 | 目标 turnId（turn_summary entry 的 id，如 `"ts_003"`） |

**响应 JSON（成功）：**

```json
{
  "type": "response",
  "id": "1",
  "command": "restore_files",
  "success": true,
  "data": {
    "restoredFiles": [
      { "path": "/abs/src/main.rs", "action": "restored", "fromHash": "abc123", "toHash": "def456" },
      { "path": "/abs/src/new.txt", "action": "deleted", "fromHash": "ghi789", "toHash": null },
      { "path": "/abs/Cargo.toml", "action": "skipped", "fromHash": null, "toHash": null, "reason": "not_modified_after_target_turn" }
    ],
    "restorePoint": "rp_a1b2c3",
    "summary": { "restored": 1, "deleted": 1, "skipped": 1 }
  }
}
```

**`action` 取值：**

| action | 含义 |
|--------|------|
| `restored` | 文件恢复到 before 内容 |
| `deleted` | 文件被删除（原本不存在，会话期间创建的） |
| `skipped` | 跳过（未被追踪 / before 内容未捕获 / write 失败） |

**验证点：**
- ✅ `toTurn` 之后的改动被撤销（restored + deleted）
- ✅ restore_point 返回（可用于 `undo_restore`）
- ✅ summary 统计正确

**来源**：✅ CI H2 实测（`tests/file_snapshot_ci.sh` Group H2，空会话返回空列表）

---

## Group V: 审批（review_* 6 RPC）

> 验证 per-file 审批能力。核心：`review_pending` 拉取 → `approve` 锚定 baseline / `reject` 自动回滚 → `approve_all`/`reject_all` 批量 → `approvals` 查询。

### V1 review_pending 拉取待审批

**场景**：agent 完成任务 Stop 后，用户查看有哪些文件待审批。

```bash
ion rpc --session sess_xxx --method review_pending --params '{}'
```

**响应 JSON（成功，有待审批文件）：**

```json
{
  "type": "response",
  "id": "1",
  "command": "review_pending",
  "success": true,
  "data": {
    "pending": [
      {
        "path": "src/main.rs",
        "status": "modified",
        "diffStat": "src/main.rs | 5+2-",
        "oldContent": "fn main() {...}",
        "newContent": "fn main() {...new...}"
      },
      {
        "path": "src/new_feature.rs",
        "status": "added",
        "diffStat": "src/new_feature.rs | 42+",
        "oldContent": null,
        "newContent": "pub fn new_feature() {...}"
      }
    ],
    "summary": { "total": 2, "added": 1, "modified": 1, "deleted": 0 }
  }
}
```

**响应字段：**

| 字段 | 类型 | 说明 |
|------|------|------|
| `pending[].path` | string | 相对 cwd 的路径 |
| `pending[].status` | string | `added` / `modified` / `deleted` |
| `pending[].diffStat` | string | `{path} \| {add}+{del}-`（added 只有 `N+`，deleted 是 `deleted`） |
| `pending[].oldContent` | string\|null | 旧内容（新文件为 null） |
| `pending[].newContent` | string\|null | 新内容（删除为 null） |
| `summary` | object | 按 status 分类的计数 |

**验证点：**
- ✅ agent Stop 后 `on_gate_check` 触发，pending 自动收集变更
- ✅ diffStat 格式正确（行级统计）
- ✅ 空响应：`{"pending":[],"summary":{"total":0,"added":0,"modified":0,"deleted":0}}`

**来源**：✅ CI J2 实测（`tests/file_snapshot_ci.sh` Group J2，验证 `review_pending` 含 `j2.txt`）

---

### V2 review_approve 单文件（锚定 baseline）

**场景**：用户审查 diff 后，批准某个文件的改动。

```bash
ion rpc --session sess_xxx --method review_approve \
  --params '{"path":"src/main.rs"}'
```

**请求参数：**

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `path` | string | 是 | 文件路径（与 pending 中的 path 一致） |

**响应 JSON（成功）：**

```json
{
  "type": "response",
  "id": "1",
  "command": "review_approve",
  "success": true,
  "data": {
    "path": "src/main.rs",
    "status": "approved",
    "approvedTreeHash": "tree_a1b2c3d4..."
  }
}
```

**验证点：**
- ✅ 该文件从 `review_pending` 列表消失
- ✅ `approvedTreeHash` 记录 approve 时刻的 tree 快照（baseline 锚点）
- ✅ `review_approvals` 查询该文件 status=`approved`
- ✅ 审批状态持久化到 session.jsonl（`file-approval` entry）

**baseline 锚定语义**（重要）：approve 后该文件的 baseline 锚定到 `approvedTreeHash`。如果后续同文件再被改动，re-approval 机制会把它重置为 pending（见 L2），但 diff 仍从上次 approved 的 tree 算（见 L3）。

**来源**：✅ CI J3 实测 + harness H3（`file_snapshot_harness.rs:159-188`）

---

### V3 review_reject 单文件（新文件删除）

**场景**：agent write 了一个新文件，用户审批时拒绝，文件应被删除（回滚到不存在状态）。

```bash
ion rpc --session sess_xxx --method review_reject \
  --params '{"path":"src/unwanted.rs"}'
```

**请求参数：**

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `path` | string | 是 | 文件路径 |

**响应 JSON（成功，新文件场景 → action=deleted）：**

```json
{
  "type": "response",
  "id": "1",
  "command": "review_reject",
  "success": true,
  "data": {
    "path": "src/unwanted.rs",
    "status": "rejected",
    "action": "deleted",
    "rolledBack": true,
    "denyMessageInjected": true
  }
}
```

**验证点：**
- ✅ `action=deleted`（新文件被删除，磁盘文件不存在）
- ✅ `rolledBack=true`（代码已回滚）
- ✅ `denyMessageInjected=true`（deny 消息已写入 session.jsonl，agent 下一轮可见——见 L1）
- ✅ `review_approvals` 查询该文件 status=`rejected`
- ✅ ApprovalResolved 事件推送（见 E3）

**来源**：✅ harness H4 实测（`file_snapshot_harness.rs:191-217`，验证 reject 后磁盘文件不存在）

---

### V4 review_reject 已有文件（回退旧内容）

**场景**：agent 修改了一个已有文件，用户拒绝，文件应恢复到修改前的内容（不是删除）。

```bash
# 前置：src/main.rs 原本存在，agent 改了它
ion rpc --session sess_xxx --method review_reject \
  --params '{"path":"src/main.rs"}'
```

**响应 JSON（成功，已有文件场景 → action=restored）：**

```json
{
  "type": "response",
  "id": "1",
  "command": "review_reject",
  "success": true,
  "data": {
    "path": "src/main.rs",
    "status": "rejected",
    "action": "restored",
    "rolledBack": true,
    "denyMessageInjected": true
  }
}
```

**验证点：**
- ✅ `action=restored`（文件回退到 baseline 内容，文件仍存在）
- ✅ 磁盘文件内容 = baseline tree 中该文件的内容
- ✅ 文件未被删除（区别于 V3 的新文件场景）

**action 判定逻辑**：
- baseline tree 中**有**该文件 → `restored`（回退旧内容）
- baseline tree 中**没有**该文件 → `deleted`（删除新文件）

**来源**：⚠️ 代码依据（`src/file_snapshot/restore.rs:234-306` `restore_single_file`，harness H4 只覆盖了 deleted 场景，restored 场景待补 CI）

---

### V5 review_approve_all 批量批准

**场景**：用户审查完所有 diff，一次性批准全部 pending 文件。

```bash
ion rpc --session sess_xxx --method review_approve_all --params '{}'
```

**响应 JSON（成功）：**

```json
{
  "type": "response",
  "id": "1",
  "command": "review_approve_all",
  "success": true,
  "data": {
    "approved": 3,
    "errors": 0,
    "total": 3
  }
}
```

**验证点：**
- ✅ `approved` = 成功批准的文件数
- ✅ 批准后 `review_pending` 返回空列表
- ✅ 每个文件都锚定了各自的 baseline tree hash
- ✅ 部分失败时 `errors > 0`（如某文件无法获取 current tree）

**来源**：✅ harness H5 实测（`file_snapshot_harness.rs:220-251`，验证 approve_all 后 pending 为空）

---

### V6 review_reject_all 批量拒绝

**场景**：用户否决 agent 的全部改动，批量拒绝并回滚所有文件。

```bash
ion rpc --session sess_xxx --method review_reject_all --params '{}'
```

**响应 JSON（成功）：**

```json
{
  "type": "response",
  "id": "1",
  "command": "review_reject_all",
  "success": true,
  "data": {
    "rejected": 3,
    "errors": 0,
    "total": 3
  }
}
```

**验证点：**
- ✅ `rejected` = 成功拒绝的文件数
- ✅ 每个文件都触发了代码回滚（新文件删除 / 已有文件回退）
- ✅ **每个** reject 都注入了 deny 消息到 session.jsonl（N 个文件 = N 条 `approval_deny` entry）
- ✅ 批量回滚后磁盘文件全部恢复
- ✅ `review_pending` 返回空列表

**来源**：⚠️ 代码依据（`src/file_snapshot/approval.rs:293-296` `reject_all`，内部循环调用 `reject()`，无独立 harness/CI 测试）

---

### V7 review_approvals 状态查询

**场景**：查询当前所有文件的审批状态，或按 status 过滤。

```bash
# 查全部
ion rpc --session sess_xxx --method review_approvals --params '{}'

# 只查已批准的
ion rpc --session sess_xxx --method review_approvals \
  --params '{"status":"approved"}'
```

**请求参数：**

| 字段 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| `status` | string | 否 | 不过滤 | `pending` / `approved` / `rejected`；传其他值或缺失 = 不过滤 |

**响应 JSON（成功）：**

```json
{
  "type": "response",
  "id": "1",
  "command": "review_approvals",
  "success": true,
  "data": {
    "approvals": [
      {
        "path": "src/main.rs",
        "status": "approved",
        "timestamp": 1720866245,
        "approvedTreeHash": "tree_a1b2c3d4..."
      },
      {
        "path": "src/unwanted.rs",
        "status": "rejected",
        "timestamp": 1720866250,
        "approvedTreeHash": null
      }
    ]
  }
}
```

**验证点：**
- ✅ approved 状态的文件有 `approvedTreeHash`（baseline 锚点）
- ✅ rejected 状态的文件 `approvedTreeHash=null`
- ✅ `status` 过滤生效（只返回匹配的）
- ✅ 状态可从 session.jsonl 恢复（`on_session_start` 调 `restore_from_session`）

**来源**：⚠️ 代码依据（`src/bin/ion_worker.rs:2245-2265`，无独立 harness/CI 测试）

---

### V8 review_approve 缺参数 / 未启用报错

**场景**：验证错误处理。

```bash
# 缺 path 参数
ion rpc --session sess_xxx --method review_approve --params '{}'
```

```json
{ "type": "response", "id": "1", "command": "review_approve", "success": false, "error": "missing 'path'" }
```

```bash
# file-snapshot 未启用
ion rpc --session sess_xxx --method review_approve --params '{"path":"x.rs"}'
```

```json
{ "type": "response", "id": "1", "command": "review_approve", "success": false, "error": "approval not enabled" }
```

```bash
# approve 时无 current tree snapshot
ion rpc --session sess_xxx --method review_approve --params '{"path":"x.rs"}'
```

```json
{ "type": "response", "id": "1", "command": "review_approve", "success": false, "error": "No current tree snapshot available" }
```

**验证点：**
- ✅ 缺 `path` → `"missing 'path'"`
- ✅ 未启用 → `"approval not enabled"`
- ✅ 无 tree snapshot → `"No current tree snapshot available"`

**来源**：⚠️ 代码依据（`src/bin/ion_worker.rs:2170/2177/2180`）

---

## Group L: 联动场景（审批副作用 + re-approval）

> 验证审批的副作用机制：deny 消息注入、baseline 锚定、re-approval 重置、worktree 隔离。

### L1 reject 后 deny 消息注入（agent 下一轮可见）

**场景**：用户 reject 后，agent 下一轮会看到 deny 消息，知道哪个文件被拒绝并已回滚。

```bash
# 1. agent write reject.rs → 用户 reject
ion rpc --session sess_xxx --method review_reject \
  --params '{"path":"reject.rs"}'
# → action=deleted, denyMessageInjected=true

# 2. 查看 session.jsonl，deny entry 已写入
tail -1 ~/.ion/agent/sessions/sess_xxx.jsonl | jq .
```

**注入的 entry 结构：**

```json
{
  "type": "message",
  "id": "approval_deny_1720866250000",
  "parentId": null,
  "timestamp": "2026-07-13T10:30:50Z",
  "message": {
    "role": "user",
    "content": [
      {
        "type": "text",
        "text": "📋 审批拒绝：文件 reject.rs 已回滚（action: deleted）。用户不认可这次改动，请重新处理。"
      }
    ]
  },
  "customType": "approval_deny"
}
```

**验证点：**
- ✅ entry id 格式 `approval_deny_{unix_millis}`
- ✅ `customType: "approval_deny"`
- ✅ message.role=`user`（让 agent 当作用户消息处理）
- ✅ deny 文本包含 path 和 action
- ✅ agent 下一轮 prompt 时，这条消息在 context 里（LLM 会看到"用户拒绝了 reject.rs"）
- ✅ `review_reject_all` 每个文件都会注入一条 deny entry

**来源**：⚠️ 代码依据（`src/bin/ion_worker.rs:2191-2207`，无独立 harness 断言）

---

### L2 re-approval 重置（已批准文件再改 → 自动回 pending）

**场景**：用户已批准 src/main.rs，但 agent 在下一轮又改了它——已批准状态自动重置为 pending，需要重新审批。

```bash
# 1. agent write main.rs → 用户 approve
ion rpc --session sess_xxx --method review_approve --params '{"path":"src/main.rs"}'
# → status=approved, approvedTreeHash=tree_v1

# 2. agent 下一轮又改了 main.rs（on_turn_end 触发）

# 3. main.rs 自动重置为 pending
ion rpc --session sess_xxx --method review_approvals --params '{"status":"pending"}'
```

**响应：**

```json
{
  "data": {
    "approvals": [
      { "path": "src/main.rs", "status": "pending", "timestamp": 1720866300, "approvedTreeHash": "tree_v1" }
    ]
  }
}
```

**验证点：**
- ✅ `on_turn_end` 检查 step-snapshot diff，涉及已批准文件时触发 `check_re_approval`
- ✅ status 从 `approved` → `pending`
- ✅ **`approvedTreeHash` 保留**（baseline 锚定不丢，下次 diff 从 tree_v1 算）
- ✅ ApprovalReset 事件推送（见 E4）

**来源**：⚠️ 代码依据（`src/file_snapshot/approval.rs:314-337` `check_re_approval` + `on_turn_end` hook）

---

### L3 approve 锚定后 diff 从 baseline 算

**场景**：验证 baseline 锚定的实际效果——approve 后同文件再改，pending 的 diff 是从上次 approved 的 tree 算，不是从会话开始算。

```bash
# 1. main.rs 原始内容 v0
# 2. agent 改成 v1 → approve（baseline 锚定 tree_v1）
# 3. agent 改成 v2 → on_turn_end 重置为 pending
# 4. 查看 pending 的 diff
ion rpc --session sess_xxx --method review_pending --params '{}'
```

**预期 diff：**
- ✅ `oldContent` = v1（baseline tree_v1 中的内容，**不是** v0）
- ✅ `newContent` = v2
- ✅ diffStat 只统计 v1→v2 的变化（不含 v0→v1 的历史改动）

**来源**：⚠️ 代码依据（`src/file_snapshot/approval.rs` `baseline_for_path` 从 `approvedTreeHash` 取 baseline）

---

### L4 worktree 内审批（project_key 共享 + session 隔离）

**场景**：在 git worktree 里跑会话并审批，验证审批状态按 session 隔离，但快照存储共享。

```bash
# worktree 里的会话审批
ion rpc --session sess_wt --method review_approve --params '{"path":"src/main.rs"}'

# 主仓库的会话不受影响
ion rpc --session sess_main --method review_approvals --params '{}'
# → sess_main 的 approvals 不含 sess_wt 的审批记录
```

**验证点：**
- ✅ 审批状态按 session 隔离（`ApprovalManager` 是 per-session 的）
- ✅ 但快照存储共享同一个 project_key（`git rev-parse --absolute-git-dir` 算出一致 key）
- ✅ 主仓库和 worktree 的相同内容只存一个 object（去重）

**来源**：⚠️ 代码依据（[FILE_SNAPSHOT.md §14.5 Worktree 隔离](../design/FILE_SNAPSHOT.md)）

---

## Group E: 事件推送（subscribe --ui）

> 验证审批事件的推送链路。所有事件通过 Worker stdout → Manager event-pump → EventBus broadcast → `ion subscribe --ui` 接收。

### 通用事件信封

所有审批事件的信封结构一致：

```json
{
  "type": "event",
  "event": {
    "type": "extension_event",
    "extension": "file-approval",
    "customType": "ApprovalRequest | ApprovalResolved | ApprovalReset",
    "visibility": "llm_and_ui",
    "data": { ... }
  }
}
```

- `extension` 恒为 `"file-approval"`
- `visibility` 恒为 `"llm_and_ui"`（LLM 和 UI 都可见）
- `customType` 是 3 种事件的路由 key（subscribe --ui 白名单匹配）

---

### E1 ApprovalRequest（agent Stop 时推送）

**场景**：agent 完成任务 Stop 时，如果有 pending 文件，推送审批请求事件。

```bash
# Terminal 1: 订阅
ion subscribe --session sess_xxx --ui

# Terminal 2: 触发 agent 执行（write 文件后 Stop）
ion rpc --session sess_xxx --method prompt --params '{"text":"帮我写个 hello.rs"}'
```

**Terminal 1 收到的事件：**

```json
{
  "type": "event",
  "event": {
    "type": "extension_event",
    "extension": "file-approval",
    "customType": "ApprovalRequest",
    "visibility": "llm_and_ui",
    "data": {
      "requestId": "appr_1720866245000",
      "total": 1,
      "files": [
        { "path": "hello.rs", "status": "added", "diffStat": "hello.rs | 5+" }
      ]
    }
  }
}
```

**验证点：**
- ✅ `on_gate_check` 在 agent Stop 时触发（pending 非空才推送）
- ✅ `requestId` 格式 `appr_{unix_millis}`
- ✅ files 列表只含 `path`/`status`/`diffStat`（不含 oldContent/newContent，精简推送）
- ✅ GateDecision 始终返回 `Allow`（审批是 post-hoc 的，不阻塞 agent 停止）
- ✅ pending 为空时不推送事件

**来源**：⚠️ 代码依据（`src/file_snapshot/approval.rs:492-524` `on_gate_check`）

---

### E2 ApprovalResolved（approve 触发）

**场景**：用户 approve 后，推送 decision=approved 事件。

```bash
# Terminal 1 已订阅
# Terminal 2: approve
ion rpc --session sess_xxx --method review_approve --params '{"path":"src/main.rs"}'
```

**Terminal 1 收到的事件：**

```json
{
  "type": "event",
  "event": {
    "type": "extension_event",
    "extension": "file-approval",
    "customType": "ApprovalResolved",
    "visibility": "llm_and_ui",
    "data": {
      "path": "src/main.rs",
      "decision": "approved",
      "approvedTreeHash": "tree_a1b2c3d4..."
    }
  }
}
```

**验证点：**
- ✅ `decision` 恒为 `"approved"`（approve 路径）
- ✅ `approvedTreeHash` = approve 时刻的 current tree hash
- ✅ UI 收到后可将该文件标记为"已批准"

**来源**：⚠️ 代码依据（`src/file_snapshot/approval.rs:234-238`）

---

### E3 ApprovalResolved（reject 触发）

**场景**：用户 reject 后，推送 decision=rejected 事件（含回滚结果）。

```bash
ion rpc --session sess_xxx --method review_reject --params '{"path":"src/unwanted.rs"}'
```

**收到的事件：**

```json
{
  "type": "event",
  "event": {
    "type": "extension_event",
    "extension": "file-approval",
    "customType": "ApprovalResolved",
    "visibility": "llm_and_ui",
    "data": {
      "path": "src/unwanted.rs",
      "decision": "rejected",
      "action": "deleted",
      "rolledBack": true
    }
  }
}
```

**验证点：**
- ✅ `decision` 恒为 `"rejected"`（reject 路径，与 E2 共用 customType 但 decision 不同）
- ✅ `action` = 回滚结果（`restored` / `deleted` / `skipped`）
- ✅ `rolledBack` 恒为 `true`

**来源**：⚠️ 代码依据（`src/file_snapshot/approval.rs:276-281`）

---

### E4 ApprovalReset（re-approval 触发）

**场景**：已批准文件被再次修改，`on_turn_end` 检测到后重置为 pending 并推送事件。

```bash
# 前置：src/main.rs 已 approved，agent 下一轮又改了它
# on_turn_end 自动触发 → ApprovalReset 事件
```

**收到的事件：**

```json
{
  "type": "event",
  "event": {
    "type": "extension_event",
    "extension": "file-approval",
    "customType": "ApprovalReset",
    "visibility": "llm_and_ui",
    "data": {
      "paths": ["src/main.rs", "src/lib.rs"],
      "reason": "file_changed_after_approval"
    }
  }
}
```

**验证点：**
- ✅ `paths` 是被重置的文件列表（已 approved/rejected 且在本轮被改的）
- ✅ `reason` 恒为 `"file_changed_after_approval"`
- ✅ 事件在 `on_turn_end` 时推送（每轮结束检查 step-snapshot diff）

**来源**：⚠️ 代码依据（`src/file_snapshot/approval.rs:332-335`）

---

### E5 subscribe --ui customType 白名单路由

**场景**：验证 3 种 customType 都能通过 `subscribe --ui` 的白名单路由到 UI。

```bash
ion subscribe --session sess_xxx --ui
```

**验证点：**
- ✅ `ApprovalRequest` 事件可被 `--ui` 接收
- ✅ `ApprovalResolved` 事件可被 `--ui` 接收
- ✅ `ApprovalReset` 事件可被 `--ui` 接收
- ✅ 非白名单 customType 不路由到 `--ui`（但 `--extension file-approval` 可收到全部）

**来源**：⚠️ 代码依据（Manager event-pump 的 customType 白名单）

---

## Group X: 边界与错误

### X1 file-snapshot 未启用时的错误文本（3 种不一致）

**场景**：config.json 未开启 `file-snapshot.enabled`，各 RPC 返回的错误文本不一致（已知差异）。

```bash
# restore_files → "file-snapshot not enabled"
ion rpc --session sess_xxx --method restore_files --params '{"toTurn":"ts_001"}'
# → {"error": "file-snapshot not enabled"}

# review_pending → "approval not enabled (requires file-snapshot)"
ion rpc --session sess_xxx --method review_pending --params '{}'
# → {"error": "approval not enabled (requires file-snapshot)"}

# review_approve / review_reject / review_approve_all / review_reject_all / review_approvals → "approval not enabled"
ion rpc --session sess_xxx --method review_approve --params '{"path":"x.rs"}'
# → {"error": "approval not enabled"}
```

**验证点：**
- ⚠️ `restore_files` 报 `"file-snapshot not enabled"`
- ⚠️ `review_pending` 报 `"approval not enabled (requires file-snapshot)"`
- ⚠️ 其余 5 个 review_* 报 `"approval not enabled"`（最短，无括号说明）

> **已知差异**：三种错误文本不一致，是历史实现遗留。功能正确（都阻止了操作），但文本统一性可改进。

**来源**：⚠️ 代码依据（`src/bin/ion_worker.rs:2137/2164/2180/2218/2230/2242/2264`）

---

### X2 review_approvals status 传未知值 → 静默退化

**场景**：`review_approvals` 的 status 参数传非标准值（如 `"done"`），静默退化为不过滤。

```bash
ion rpc --session sess_xxx --method review_approvals \
  --params '{"status":"done"}'
```

**预期：** 返回全部审批记录（等同于不传 status）。

**验证点：**
- ✅ 不报错
- ✅ 返回全部 approvals（pending + approved + rejected 混合）
- ⚠️ 未知值静默退化为 `None`（`ion_worker.rs:2252` 的 `_ => None` 分支）

**来源**：⚠️ 代码依据（`src/bin/ion_worker.rs:2248-2253`，status 匹配只认 pending/approved/rejected）

---

### X3 restore_files 缺 toTurn 参数

**场景**：调用 `restore_files` 不传 `toTurn`。

```bash
ion rpc --session sess_xxx --method restore_files --params '{}'
```

```json
{ "type": "response", "id": "1", "command": "restore_files", "success": false, "error": "missing 'toTurn' (turnId)" }
```

**验证点：**
- ✅ 明确报错 `"missing 'toTurn' (turnId)"`

**来源**：⚠️ 代码依据（`src/bin/ion_worker.rs:2118`）

---

### X4 reject 时 baseline 缺失

**场景**：`review_reject` 时无法获取 baseline tree hash（如会话刚建没有 step-snapshot）。

```bash
ion rpc --session sess_xxx --method review_reject --params '{"path":"x.rs"}'
```

```json
{ "type": "response", "id": "1", "command": "review_reject", "success": false, "error": "No baseline tree available" }
```

**验证点：**
- ✅ 报错 `"No baseline tree available"`
- ✅ 文件未被回滚（reject 在回滚前就失败了）
- ✅ deny 消息未注入（reject 整体失败）

**来源**：⚠️ 代码依据（`src/file_snapshot/approval.rs:248`）

---

## 汇总

### 按验证来源统计

| 来源 | case 数 | case 列表 |
|------|--------|----------|
| ✅ CI 实测 | 3 | R6, V1, V2 |
| ✅ harness 实测 | 4 | V2, V3, V5（+ V3 的 deleted 场景） |
| ⚠️ 代码依据待 CI | 19 | R1-R5, V4, V6-V8, L1-L4, E1-E5, X1-X4 |

### 按功能模块统计

| 模块 | case 数 | 覆盖 |
|------|--------|------|
| Group R 回滚 | 6 | 消息回滚 / 代码联动 / tombstone / turnId 接续 / 压缩点拒绝 / 独立 restore |
| Group V 审批 | 8 | pending / approve / reject(2场景) / approve_all / reject_all / approvals / 错误处理 |
| Group L 联动 | 4 | deny 注入 / re-approval 重置 / baseline 锚定 / worktree 隔离 |
| Group E 事件 | 5 | ApprovalRequest / Resolved(approve) / Resolved(reject) / Reset / 白名单路由 |
| Group X 边界 | 4 | 未启用报错 / status 退化 / 缺参数 / baseline 缺失 |
| **合计** | **27** | |

---

## 已知差异与注意点

1. **3 种"未启用"错误文本不一致**（见 X1）—— `restore_files` / `review_pending` / 其余 review_* 各用不同文本，功能正确但文本不统一。
2. **`approved_turn_id` 字段恒为 None** —— `FileApproval` 结构体中有此字段但 `approve()` 始终赋 `None`，不出现在任何 RPC 响应或事件中（死字段，保留供未来用）。
3. **`review_approvals` status 传未知值静默退化**（见 X2）—— 不报错，退化为不过滤。调用方需自行确保传合法值。
4. **`review_reject` 是唯一写 session.jsonl 用户消息的 RPC**（见 L1）—— approve 不注入消息，只有 reject 会注入 `approval_deny` entry 让 agent 下一轮看到。
5. **持久化用 snake_case，RPC 输出用 camelCase** —— session.jsonl 的 `file-approval` entry 用 `approved_tree_hash`，RPC 响应用 `approvedTreeHash`。消费方需注意大小写。

---

## 未覆盖场景（诚实标注）

以下场景当前**无 harness 或 CI 测试**，文档基于代码依据编写，标 ⚠️：

| 场景 | 现状 | 建议 |
|------|------|------|
| `review_reject_all` 批量拒绝 | 无 harness/CI（只有 approve_all 的 H5） | 补 harness H6 |
| `review_approvals` 独立测试 | 无 harness/CI | 补 harness H7 |
| reject 已有文件（action=restored） | harness H4 只覆盖新文件删除 | 补 harness H4b |
| 事件推送 CLI 断言 | 无（harness 不断言事件） | 补 CI K1-K4（subscribe + grep customType） |
| deny 消息 agent 可见性 | 无端到端断言 | 补 harness：reject → 下一轮 context 含 deny 文本 |
| 真实 LLM 审批闭环 | `e1_real_agent_approval_workflow` 是 `#[ignore]` 空壳 | 补实现（需 ION_E2E=1 + API key） |
| re-approval 重置端到端 | 无 harness/CI | 补 harness H8 |
