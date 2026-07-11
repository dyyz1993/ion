# File Snapshot & Review — pi 对齐清单与执行计划

> **状态：开发中** — 基于 pi（`file-snapshot-manager.ts` + `file-review` extension）的全面对标，规划 ION 的快照模型升级 + per-file 审批 + 回滚完善。
>
> **前置文档**：[FILE_SNAPSHOT.md](./FILE_SNAPSHOT.md)（现有实现的设计文档，本文档是其升级路线）

---

## 何时使用这个文档

规划文件快照系统的下一步演进时使用。本文档是**对比清单 + 执行路径**，不是实现细节。每一步实现时参照对应的"该不该对齐"判断。

**核心判断**：快照存储模型是审批/回滚的地基。当前 delta 流模型让 per-file 审批变复杂，需先升级到 tree 快照模型。

---

## 一、总体差距

| 能力 | ION 现状 | pi 状态 | ION 目标 |
|------|---------|--------|---------|
| 文件快照 | ⚠️ delta 流（per-turn delta） | ✅ tree 对象（完整状态） | 升级到 tree |
| 变更检测 | ✅ 双轨（工具拦截 + 扫描） | ✅ 纯扫描 | 保持（ION 更优） |
| 全部审批 | ❌ 无 | ✅ approveAll/rejectAll | 新建 |
| 单文件审批 | ❌ 无 | ✅ review.approve({path}) | 新建 |
| 单文件回滚 | ❌ 无 | ✅ restoreFiles({files}) | 新建 |
| 审批持久化 | ❌ 无 | ✅ session entry | 新建 |
| undo 回滚 | ⚠️ 有零件没接 | ✅ unrevert-point | 接上 |
| diff | ✅ similar（真行级） | ✅ jsdiff | 已对齐 |

---

## 二、详细对比清单（逐项判断该不该对齐）

### 2.1 快照存储模型（地基，必须先升级）

| # | 维度 | ION 现状 | pi 做法 | 对齐? | 理由 |
|---|------|---------|--------|-------|------|
| S1 | **存储格式** | delta 流（`ToolSnapshot` per-turn：before_hash/after_hash） | tree 对象（`path\0hash` 完整状态映射） | ✅ 对齐 | tree 让 O(1) 读任意时刻状态；per-file 审批/回滚直接操作 tree 不用回放 |
| S2 | **变更查询** | `load_all_tool_snapshots` 回放全部历史 O(n) | `computeDiff(old_tree, new_tree)` O(1) | ✅ 对齐 | 长会话 delta 回放越来越慢 |
| S3 | **无变更 turn** | 每 turn 都写 jsonl | 检测 `hasChanges`，无变更不写 entry | ✅ 对齐 | 省会话日志体积 |
| S4 | **内容去重** | ✅ SipHash 64bit 去重 | ✅ fnv1a 32bit 去重 | 已对齐 | — |
| S5 | **hash 强度** | SipHash 64bit | fnv1a 32bit | ION 更强 | pi 的 32bit 有碰撞风险，ION 的 64bit 更稳（保留不换） |
| S6 | **tree 格式** | 无 | `path\0hash\npath\0hash...`（排序后 join） | ✅ 采用 | 扁平结构，简单够用，排序保证内容相同则 hash 相同 |
| S7 | **step-snapshot entry** | 无（用 ToolSnapshot jsonl） | `{baselineTreeHash, snapshotTreeHash, diff, turnIndex}` | ✅ 采用 | 链式快照点，支持回滚和 GC 可达性 |
| S8 | **session_start baseline** | 无 | `sessionStartTreeHash` | ✅ 采用 | 审批/回滚的起始参考点 |

**结论：存储模型从 delta 流升级到 tree 快照。这是 per-file 审批的地基。**

### 2.2 变更检测（ION 已更优，保持现状）

| # | 维度 | ION 现状 | pi 做法 | 对齐? | 理由 |
|---|------|---------|--------|-------|------|
| D1 | **工具拦截** | ✅ write/edit before/after 精确捕获 | ❌ 纯全量扫描 | ION 更优 | 双轨更精确，保留 |
| D2 | **全量扫描兜底** | ✅ turn_end 扫描（刚加） | ✅ turn_end 全量扫描 | 已对齐 | — |
| D3 | **忽略策略** | ✅ 不遵守 .gitignore（独立清单） | ❌ 遵守 .gitignore | ION 更优 | .env 等文件需要追踪，保留 |
| D4 | **扫描限制** | 1MB/50MB/5000 文件 | 1MB/50MB/5000 文件 | 已对齐 | — |
| D5 | **bash 扫描 before** | ⚠️ 没存 before 内容（before_unknown 兜底） | N/A（pi 不做工具拦截） | 已有兜底 | before_unknown 标记 + restore 跳过，不误删 |

**结论：变更检测 ION 已经比 pi 好，保持现状。**

### 2.3 审批（完全空白，需新建对齐 pi）

| # | 维度 | ION 现状 | pi 做法 | 对齐? | 理由 |
|---|------|---------|--------|-------|------|
| A1 | **审批触发** | ❌ 无 | post-hoc，用户 RPC 主动调 | ✅ 做 + 改进 | ION 用 on_gate_check 主动推送到 UI（pi 没有） |
| A2 | **全部审批** | ❌ 无 | ✅ `approveAll` / `rejectAll` | ✅ 对齐 | 批量操作必备 |
| A3 | **单文件审批** | ❌ 无 | ✅ `review.approve({path})` | ✅ 对齐 | per-file 是核心需求 |
| A4 | **审批持久化** | ❌ 无 | ✅ session entry（`file-approval`） | ✅ 对齐 | 重启不丢审批状态 |
| A5 | **approve 锚定 baseline** | ❌ 无 | ✅ 记录 `snapshotEntryId` + `snapshotTreeHash` | ✅ 对齐 | 保证多轮审批语义正确（关键！） |
| A6 | **re-approval 重置** | ❌ 无 | ✅ 已批准文件被改 → 回 pending | ✅ 对齐 | 旧决策作废，需重新审 |
| A7 | **net-zero 过滤** | ❌ 无 | ✅ added→deleted 且未 approved → 不显示 | ✅ 对齐 | 减少幽灵条目噪声 |
| A8 | **no-op 过滤** | ❌ 无 | ✅ 内容相同（git checkout 回滚）→ 跳过 | ✅ 对齐 | 外部操作后不误报 |
| A9 | **everApproved Set** | ❌ 无 | ✅ 只增不减，防 net-zero 误吞已批准文件 | ✅ 对齐 | net-zero 的安全阀 |
| A10 | **deny 后重跑快照** | ❌ 无 | ✅ reject → rollback → 重跑 onTurnEnd | ✅ 对齐 | 不重跑会产生幽灵 pending |
| A11 | **session_start 重建** | ❌ 无 | ✅ 按 entry 顺序回放重建 4 个内存结构 | ✅ 对齐 | 冷启动恢复审批状态 |
| A12 | **无 UI 默认** | — | 用户主动 RPC | ✅ ION 改进 | 无 UI 时标记 pending 不阻塞，有 UI 再审批 |

**结论：审批完全空白，需新建。核心是 pi 的 6 个算法链（A5~A11），缺一不可。**

#### pi 审批的 6 个核心算法（正确性链条）

```
approve 锚定 baseline（A5）
    ↓
re-approval 重置（A6）：改了回 pending，baseline 不动
    ↓
pending 算 diff 时用 approved snapshot 做 oldContent（A5 消费侧）
    ↓
net-zero（A7）+ no-op（A8）过滤幽灵条目
    ↓
reject → 回滚 + 重跑快照对齐 baseline（A10）
    ↓
session_start 按 entry 顺序回放重建（A11）
```

### 2.4 回滚（部分有，需完善）

| # | 维度 | ION 现状 | pi 做法 | 对齐? | 理由 |
|---|------|---------|--------|-------|------|
| R1 | **整体回滚** | ✅ `restore_code_to_turn` | ✅ `restoreFiles` | 已对齐 | tree 模型后改为读 tree 写回 O(1) |
| R2 | **单文件回滚** | ❌ 无 | ✅ `restoreFiles({files:[...]})` | ✅ 对齐 | per-file deny 时需要 |
| R3 | **回滚预览** | ❌ 无 | ✅ `preview:true` 只看不写 | ✅ 对齐 | 审批前让用户看要回滚什么 |
| R4 | **dirty 检测** | ❌ 无 | ✅ 检测用户手动改过，标 `forceRestored` | 🟡 可选 | 防强制覆盖用户改动（V2） |
| R5 | **undo 回滚** | ⚠️ `restore_point` 写了没消费 | ✅ `unrevert-point` 完整实现 | ✅ 对齐 | ION 有零件，接上即可 |
| R6 | **deny 后重跑** | ❌ 无 | ✅ reject 后重跑 onTurnEnd | ✅ 对齐 | 同 A10 |

**结论：单文件回滚（R2）+ undo 消费（R5）+ deny 后重跑（R6）必须做。**

### 2.5 diff（已对齐）

| # | 维度 | ION 现状 | pi 做法 | 对齐? |
|---|------|---------|--------|-------|
| F1 | **行级 diff** | ✅ similar crate（刚换，Myers） | ✅ jsdiff | 已对齐 |
| F2 | **intra-line word diff** | ❌ 无 | ✅ `diffWords`（TUI 渲染） | 🟡 UI 层，后续 |
| F3 | **编辑工具预览 diff** | ❌ 无 | ✅ `edit-diff.ts`（fuzzy match） | 🟡 可选，后续 |

### 2.6 GC / 压缩（需清理死代码）

| # | 维度 | ION 现状 | pi 做法 | 对齐? | 理由 |
|---|------|---------|--------|-------|------|
| G1 | **metadata 存储** | ✅ 写了但全死代码（4 字段无读取者） | ✅ 写了且 GC 用 mtime | ✅ ION 删掉 | 没用不如删 |
| G2 | **分级 GC** | ✅ 刚接线（7天→24h→可达性） | ✅ 同策略 | 已对齐 | — |
| G3 | **per-project 配额** | ✅ 100MB | ✅ 100MB | 已对齐 | — |
| G4 | **可达性 root** | active_hashes 白名单（临时构建） | tree 对象当 root（自然可达性） | ✅ tree 后自动对齐 | tree 引用的 file 自动受保护 |

**结论：删 metadata 死代码，tree 模型后 GC root 自动变成 tree。**

### 2.7 存储目录结构对比

**ION 现状（待简化）：**
```
~/.ion/file-store/<project_key>/
├── objects/<前2位>/<hash>        ← 文件内容
├── metadata/<前2位>/<hash>       ← 死代码（4 字段无读取者）
└── snapshots/
    ├── tool/<turn_id>.jsonl      ← delta 流（ToolSnapshot）
    ├── turn/                     ← 空目录
    └── restore/<rp_id>.json      ← restore_point（没消费）
```

**目标（简化 + tree 模型后）：**
```
~/.ion/file-store/<project_key>/
├── objects/<前2位>/<hash>        ← 文件内容 + tree 对象（统一存储）
└── snapshots/
    ├── tree/<turn_id>.json       ← step-snapshot（baseline_tree + snapshot_tree + diff）
    └── restore/<rp_id>.json      ← restore_point / unrevert-point（接上消费）
```

---

## 三、执行计划（4 步，每步独立可验证）

### 步骤 1：存储简化（清理地基）

**目标**：删死代码，存储结构从 5 目录减到 3。

| 任务 | 文件 | 说明 |
|------|------|------|
| 删 metadata/ | object_store.rs | write_metadata / touch_accessed / metadata_path 全删 |
| 删 cache 字段 | object_store.rs | `#[allow(dead_code)]` 从未读写 |
| 删 turn/ 空目录 | snapshot.rs | 创建了从没写过 |
| GC 改用 object mtime | gc.rs | get_object_created_at 读 object 文件而非 metadata |
| 暴露 object_path | object_store.rs | GC 需要拿到 object 路径 |

**验证**：`cargo test --lib file_snapshot` 全过 + 目录结构只有 objects/ + snapshots/

### 步骤 2：tree 快照模型（升级地基）

**目标**：从 delta 流升级到 tree 快照，O(1) 读任意时刻状态。

| 任务 | 说明 |
|------|------|
| tree 对象格式 | `path\0hash\npath\0hash...`（排序后 join），存进 objects/（和 file 对象统一） |
| write_tree 函数 | 扫描结果 → 写每个 file 对象 → 拼 tree 数据 → 写 tree 对象 → 返回 tree_hash |
| read_tree 函数 | 读 tree 对象 → 解析 path→hash 映射 |
| compute_diff | 对比 old_tree / new_tree 的 per-file hash → {added, modified, deleted} |
| step-snapshot entry | 每 turn 有变更时写：`{baseline_tree_hash, snapshot_tree_hash, diff, turn_id}` |
| on_turn_end 改造 | 扫描 → write_tree → compute_diff(baseline, current) → 有变更才写 step-snapshot |
| session_start | 建 baseline tree（sessionStartTreeHash） |
| SnapshotStore 改造 | 新增 tree 查询方法（get_tree_at_turn / get_file_state_at_turn） |
| GC root 改为 tree | tree 对象当 root，tree 引用的 file 自动受保护 |
| 旧数据兼容 | 保留 ToolSnapshot jsonl 读取（迁移期），或直接弃旧 |

**验证**：tree 对象能写入读出 + compute_diff 正确 + 无变更 turn 不写 entry

### 步骤 3：回滚升级

**目标**：单文件回滚 + undo 消费 + 预览模式。

| 任务 | 说明 |
|------|------|
| `restore_single_file(store, target_tree, path)` | 读 target_tree 拿到 path 的 hash → read_object → write_file |
| `restore_to_tree(store, target_tree)` | 整体回滚：读 tree 所有 path→hash → 逐文件写回（O(1)） |
| 回滚预览 | `preview:true` 只返回 diff 不写盘 |
| restore_point 消费 | 接上 undo 回滚（读 restore_point → 恢复到回滚前状态） |
| dirty 检测（V2） | 回滚前对比磁盘 hash vs 快照 hash，标记 forceRestored |

**验证**：单文件回滚 + 整体回滚 + undo 往返 + 预览不写盘

### 步骤 4：per-file 审批（对齐 pi file-review）

**目标**：post-hoc per-file 审批，全部 + 单文件，6 算法链，主动推送。

| 任务 | 说明 |
|------|------|
| ApprovalExtension | 新扩展，持 Arc<SnapshotStore> + Arc<EventBus>，挂 on_gate_check |
| 审批状态持久化 | session entry：`file-approval`（per-file 状态）+ `file-review-turn`（per-turn 变更） |
| review.pending RPC | 算 pending 列表（聚合 turn + 套 net-zero/no-op 过滤） |
| review.approve({path}) | 单文件批准，锚定 baseline（记录 tree_hash） |
| review.reject({path}) | 单文件拒绝 → restore_single_file → 重跑快照 |
| review.approveAll / rejectAll | 批量操作 |
| review.approvals({status?}) | 查询审批状态 |
| on_gate_check 推送 | agent Stop 时算 diff → 推 Ask 事件到 EventBus → 等 UI 响应 |
| 无 UI 默认 | 标记 pending 不阻塞，返回 Allow |
| **6 算法链** | A5 锚定 / A6 重置 / A7 net-zero / A8 no-op / A10 deny 重跑 / A11 重建 |
| session_start 重建 | 按 entry 顺序回放，重建 approvals + everApproved + approvedSnapshot + turnLog |
| deny 静默插入 | 往 session.jsonl 追加"被回滚"消息，下一轮对 agent 可见 |

**验证**：approve/reject/pending 全流程 + 多轮审批语义 + 重启不丢状态 + 无 UI 不阻塞

---

## 四、CLI 测试指南

> 格式参照 [CLI_TEST_TEMPLATE.md](../templates/CLI_TEST_TEMPLATE.md) + [FILE_SNAPSHOT.md §11](./FILE_SNAPSHOT.md) 的 Group 风格。
> Group 按测试主题分组（不按 RPC），每个 case 给可直接复制运行的 `ion rpc` 命令 + ✅ 验证点 + 审计日志说明。
> 4 个步骤各有独立 Group，每步实现后单独验证，最后有跨步骤主链路 case。

### 前置准备

```bash
# 编译
cargo build --bin ion --bin ion-worker

# 启 host（faux 模式，不调真 LLM）
ION_FAUX_REPLY="snapshot test" ./target/debug/ion serve &
HOST_PID=$!
sleep 2

# 建测试 session
./target/debug/ion rpc --method create_session --params '{"cwd":"/tmp/test-project"}'
# → 拿到 <sid>
```

---

### Group S：步骤 1 — 存储简化验证

> 验证死代码删除后存储结构干净，GC 用 object mtime。

#### S1 存储目录结构（无 metadata/、无 turn/）

```bash
# 跑一轮 write 后检查目录结构
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"write","input":{"file_path":"a.txt","content":"hello"}}'

# 检查存储结构
find ~/.ion/file-store/ -type d | sort
```

**✅ 验证点**：
- 只有 `objects/` 和 `snapshots/` 两个子目录（无 `metadata/`、无 `snapshots/turn/`）
- `snapshots/` 下只有 `tool/`（或步骤2后 `tree/`）+ `restore/`

#### S2 GC 用 object mtime（不读 metadata）

```bash
# 单元测试覆盖（无需 host）
cargo test --lib file_snapshot::gc -- --nocapture
```

**✅ 验证点**：
- `enforce_limit_under_threshold` 通过
- `gc_removes_unreachable` 通过
- GC 不依赖 metadata 文件

#### S3 内容去重仍然生效

```bash
# 写相同内容两次
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"write","input":{"file_path":"dup1.txt","content":"same"}}'
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"write","input":{"file_path":"dup2.txt","content":"same"}}'

# objects/ 里相同内容只有一份 blob
find ~/.ion/file-store/ -path "*/objects/*" -type f | wc -l
```

**✅ 验证点**：相同内容的 object 只存一份（去重）。

---

### Group T：步骤 2 — tree 快照模型验证

> 验证 tree 对象存储 + compute_diff + 跳过无变更 turn。

#### T1 tree 对象写入与读取

```bash
# 跑一轮改动后查 step-snapshot
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"write","input":{"file_path":"tree_test.txt","content":"v1"}}'

# 查最新快照（新 RPC，步骤 2 实现）
ion rpc --session <sid> --method get_current_tree \
  --params '{}'
```

**响应（预期）：**
```json
{
  "type": "response",
  "success": true,
  "data": {
    "tree_hash": "ab3f1c...",
    "baseline_tree_hash": "session_start_hash...",
    "file_count": 1,
    "turn_id": "ts_xxxx"
  }
}
```

**✅ 验证点**：
- tree_hash 非空，指向 objects/ 里的一个 tree 对象
- baseline_tree_hash 是 session start 的 tree
- file_count 包含 tree_test.txt

**审计日志**：step-snapshot entry 写到 `snapshots/tree/<turn_id>.json`

#### T2 compute_diff（tree hash 比对）

```bash
# 改第二个文件
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"write","input":{"file_path":"tree_b.txt","content":"new"}}'

# 查 baseline → 当前的 diff
ion rpc --session <sid> --method get_tree_diff \
  --params '{"from":"<baseline_tree_hash>","to":"<current_tree_hash>"}'
```

**✅ 验证点**：
- 返回 `{added: ["tree_b.txt"], modified: ["tree_test.txt"], deleted: []}`（O(1) tree 比对，非回放）
- 不需要 load_all_tool_snapshots

#### T3 无变更 turn 不写 step-snapshot

```bash
# 记下当前 step-snapshot 数量
ls ~/.ion/file-store/<pk>/snapshots/tree/ | wc -l

# 发一条不涉及文件改动的消息（agent 只回复文本）
ion rpc --session <sid> --method prompt --params '{"text":"你好"}'

# 再数 step-snapshot
ls ~/.ion/file-store/<pk>/snapshots/tree/ | wc -l
```

**✅ 验证点**：无变更 turn 不产生新的 step-snapshot 文件（数量不变）。

#### T4 长会话性能（O(1) vs O(n) 回放）

```bash
# 造 50 轮改动
for i in $(seq 1 50); do
  ion rpc --session <sid> --method call_tool \
    --params "{\"tool\":\"write\",\"input\":{\"file_path\":\"p$i.txt\",\"content\":\"v$i\"}}"
done

# 查改动（tree 模型应 O(1)）
time ion rpc --session <sid> --method get_modified_files --params '{}'
```

**✅ 验证点**：50 轮后查询时间不随轮数线性增长（对比 delta 流的 O(n) 回放）。

---

### Group R：步骤 3 — 回滚升级验证

> 验证单文件回滚 + 整体回滚 + undo + 预览。

#### R1 单文件回滚（per-file restore）

```bash
# 改 3 个文件
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"write","input":{"file_path":"r1.txt","content":"new1"}}'
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"write","input":{"file_path":"r2.txt","content":"new2"}}'
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"write","input":{"file_path":"r3.txt","content":"new3"}}'

# 只回滚 r2.txt（新 RPC，步骤 3 实现）
ion rpc --session <sid> --method restore_single_file \
  --params '{"path":"r2.txt","to_tree":"<baseline_tree_hash>"}'
```

**响应（预期）：**
```json
{
  "type": "response",
  "success": true,
  "data": {
    "path": "r2.txt",
    "action": "restored",
    "from_hash": "h_new2",
    "to_hash": null
  }
}
```

**✅ 验证点**：
- `r2.txt` 被回滚（新文件 → 删除）
- `r1.txt` 和 `r3.txt` 不受影响（仍是 new1/new3）

#### R2 整体回滚到指定 tree

```bash
# 回滚到 baseline（所有改动撤销）
ion rpc --session <sid> --method restore_to_tree \
  --params '{"tree_hash":"<baseline_tree_hash>"}'
```

**✅ 验证点**：
- 所有 r1/r2/r3.txt 被撤销
- restore_point 写入 `snapshots/restore/`

#### R3 回滚预览（不写盘）

```bash
# 预览回滚 r1.txt 的效果
ion rpc --session <sid> --method restore_single_file \
  --params '{"path":"r1.txt","to_tree":"<baseline>","preview":true}'
```

**✅ 验证点**：
- 返回"将要做什么"（diff / action）
- 但文件没变（preview 不写盘）

#### R4 undo 回滚（restore_point 消费）

```bash
# 先做一次整体回滚
ion rpc --session <sid> --method restore_to_tree \
  --params '{"tree_hash":"<baseline>"}'
# → 生成 restore_point rp_xxx

# undo：恢复到回滚前
ion rpc --session <sid> --method undo_restore \
  --params '{"restore_point_id":"rp_xxx"}'
```

**✅ 验证点**：
- 文件恢复到回滚前状态
- restore_point 被正确读取和消费（之前写了没读的 bug 修复）

---

### Group A：步骤 4 — per-file 审批验证

> 验证全部审批 + 单文件审批 + 6 算法链 + 主动推送 + 无 UI 默认。

#### A1 review.pending（列出待审批文件）

```bash
# 改 3 个文件
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"write","input":{"file_path":"a1.txt","content":"x"}}'
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"write","input":{"file_path":"a2.txt","content":"y"}}'

# 查 pending（新 RPC，步骤 4 实现）
ion rpc --session <sid> --method review_pending --params '{}'
```

**响应（预期）：**
```json
{
  "type": "response",
  "success": true,
  "data": {
    "pending": [
      {"path":"a1.txt","status":"added","diff_stat":"a1.txt | 1 +","old_content":null,"new_content":"x"},
      {"path":"a2.txt","status":"added","diff_stat":"a2.txt | 1 +","old_content":null,"new_content":"y"}
    ],
    "summary": {"total": 2, "added": 2, "modified": 0, "deleted": 0}
  }
}
```

**✅ 验证点**：
- 两个文件都在 pending 列表
- 每个文件有 diff_stat + old/new content
- summary 统计正确

#### A2 单文件 approve（锚定 baseline）

```bash
ion rpc --session <sid> --method review_approve \
  --params '{"path":"a1.txt"}'
```

**响应（预期）：**
```json
{
  "type": "response",
  "success": true,
  "data": {
    "path": "a1.txt",
    "status": "approved",
    "approved_tree_hash": "ab3f1c...",
    "snapshot_entry_id": "entry_xxx"
  }
}
```

**✅ 验证点**：
- a1.txt 状态 → approved
- 记录了 approved_tree_hash（锚定 baseline，A5 算法）
- session.jsonl 追加 file-approval entry

#### A3 单文件 reject（回滚 + 重跑快照）

```bash
ion rpc --session <sid> --method review_reject \
  --params '{"path":"a2.txt"}'
```

**响应（预期）：**
```json
{
  "type": "response",
  "success": true,
  "data": {
    "path": "a2.txt",
    "status": "rejected",
    "rolled_back": true,
    "action": "deleted"
  }
}
```

**✅ 验证点**：
- a2.txt 状态 → rejected
- 文件被回滚（新文件 → 删除）
- 重跑快照对齐 baseline（A10 算法，不产生幽灵 pending）

**审计日志**：
- session.jsonl 追加 file-approval entry（status=rejected）
- session.jsonl 追加"静默插入"消息（a2.txt 被回滚，下一轮 agent 可见）
- 新 step-snapshot 写入（回滚后状态）

#### A4 全部审批

```bash
# 改 3 个文件后
ion rpc --session <sid> --method review_approve_all --params '{}'
```

**✅ 验证点**：
- 所有 pending 文件 → approved
- 每个都锚定 baseline

```bash
# reject_all 同理
ion rpc --session <sid> --method review_reject_all --params '{}'
```

**✅ 验证点**：所有 pending 文件 → rejected + 回滚。

#### A5 re-approval 重置（A6 算法）

```bash
# a1.txt 已 approved（A2）
# 再改它
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"write","input":{"file_path":"a1.txt","content":"changed"}}'

# 查 pending
ion rpc --session <sid> --method review_pending --params '{}'
```

**✅ 验证点**：
- a1.txt 重新出现在 pending（状态从 approved 回到 pending）
- diff 从上次 approved 的 baseline 算（不是从 session start），只显示增量

#### A6 net-zero 过滤（A7 算法）

```bash
# 新建文件 then 删除（不经审批）
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"write","input":{"file_path":"nz.txt","content":"temp"}}'
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"bash","input":{"command":"rm nz.txt"}}'

# 查 pending
ion rpc --session <sid> --method review_pending --params '{}'
```

**✅ 验证点**：
- `nz.txt` **不出现**在 pending（added → deleted，net-zero，从未 approved）

#### A7 无 UI 默认（不阻塞）

```bash
# 场景 1：直接执行（无 subscribe --ui）
ion "改一下 a.txt"

# agent Stop 时 on_gate_check 触发审批
# 但没有 UI 订阅 → 标记 pending，返回 Allow（不阻塞）
```

**✅ 验证点**：
- agent 正常完成（不卡住）
- `review_pending` 能查到变更（pending 已记录）
- 后续有 UI 连上时可审批

#### A8 主动推送（有 UI 时）

```bash
# 终端 1：订阅 UI 事件
ion subscribe --ui

# 终端 2：agent 改文件后 Stop
ion "改一下 b.txt"

# 终端 1 收到审批请求事件
# {"type":"ui_event","ui_type":"ApprovalRequest","data":{"request_id":"req_xxx","files":[...]}}
```

**✅ 验证点**：
- subscribe --ui 收到 ApprovalRequest 事件
- 事件 data 含文件列表 + request_id
- 用 `ion rpc --method ui_respond --params '{"request_id":"req_xxx","response":"approve"}'` 回复

---

### Group M：主链路 case（跨步骤端到端）

> 完整的真实使用场景，验证多个步骤协作。

#### M1 审批闭环：agent 改文件 → 用户审批 → 部分拒绝

```bash
# 1. agent 改 5 个文件（模拟一轮开发）
for f in m1.rs m2.rs m3.rs m4.rs m5.rs; do
  ion rpc --session <sid> --method call_tool \
    --params "{\"tool\":\"write\",\"input\":{\"file_path\":\"$f\",\"content\":\"code\"}}"
done

# 2. agent Stop → on_gate_check 触发审批推送（有 UI）
#    → subscribe --ui 收到 ApprovalRequest

# 3. 用户查 pending
ion rpc --session <sid> --method review_pending --params '{}'
# → 5 个文件

# 4. 用户 approve 3 个、reject 2 个
ion rpc --session <sid> --method review_approve --params '{"path":"m1.rs"}'
ion rpc --session <sid> --method review_approve --params '{"path":"m2.rs"}'
ion rpc --session <sid> --method review_approve --params '{"path":"m3.rs"}'
ion rpc --session <sid> --method review_reject  --params '{"path":"m4.rs"}'
ion rpc --session <sid> --method review_reject  --params '{"path":"m5.rs"}'

# 5. 验证结果
ion rpc --session <sid> --method review_approvals --params '{}'
```

**✅ 验证点**：
- m1/m2/m3 → approved（文件保留）
- m4/m5 → rejected（文件回滚删除）
- session.jsonl 有 5 条 file-approval entry
- 回滚后 step-snapshot 反映正确磁盘状态（m4/m5 没了）

#### M2 多轮审批：approve → 再改 → 再审批

```bash
# 第 1 轮：改 a.txt，approve
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"write","input":{"file_path":"multi.txt","content":"v1"}}'
ion rpc --session <sid> --method review_approve --params '{"path":"multi.txt"}'

# 第 2 轮：再改 a.txt
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"write","input":{"file_path":"multi.txt","content":"v2"}}'

# 查 pending
ion rpc --session <sid> --method review_pending --params '{}'
```

**✅ 验证点**（A5 + A6 联合）：
- multi.txt 重新出现在 pending（re-approval 重置）
- diff 只显示 v1→v2 的增量（从上次 approved baseline 算，不是从 session start）
- old_content = "v1"（approved baseline），new_content = "v2"（当前磁盘）

#### M3 回滚闭环：改动 → 回滚 → undo

```bash
# 1. 改 3 个文件
for f in u1.txt u2.txt u3.txt; do
  ion rpc --session <sid> --method call_tool \
    --params "{\"tool\":\"write\",\"input\":{\"file_path\":\"$f\",\"content\":\"new\"}}"
done

# 2. 整体回滚到 baseline
ion rpc --session <sid> --method restore_to_tree \
  --params '{"tree_hash":"<baseline>"}'
# → restore_point rp_001

# 3. 验证文件都回去了
ls u1.txt u2.txt u3.txt 2>&1  # 应不存在

# 4. undo 回滚
ion rpc --session <sid> --method undo_restore \
  --params '{"restore_point_id":"rp_001"}'

# 5. 验证文件恢复了
cat u1.txt  # → "new"
```

**✅ 验证点**（R1 + R2 + R4 联合）：
- 回滚 → 文件消失
- undo → 文件恢复
- restore_point 正确消费

#### M4 重启不丢状态

```bash
# 1. 改文件 + 部分审批
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"write","input":{"file_path":"persist.txt","content":"x"}}'
ion rpc --session <sid> --method review_approve --params '{"path":"persist.txt"}'

# 2. 重启 host
kill $HOST_PID
ION_FAUX_REPLY="snapshot test" ./target/debug/ion serve &
sleep 2

# 3. 恢复 session
ion rpc --session <sid> --method resume_session --params '{"session_id":"<sid>"}'

# 4. 查审批状态
ion rpc --session <sid> --method review_approvals --params '{}'
```

**✅ 验证点**（A11 算法）：
- session_start 重建后，persist.txt 仍是 approved
- pending 列表正确（没有幽灵条目）

---

### Group D：单元测试 + 集成测试

```bash
# D1 步骤1：存储简化
cargo test --lib file_snapshot::object_store -- --nocapture
cargo test --lib file_snapshot::gc -- --nocapture

# D2 步骤2：tree 模型
cargo test --lib file_snapshot::tree_store -- --nocapture

# D3 步骤3：回滚
cargo test --lib file_snapshot::restore -- --nocapture

# D4 步骤4：审批
cargo test --lib file_snapshot::approval -- --nocapture

# D5 全量
cargo test --lib file_snapshot -- --nocapture

# D6 CLI 端到端（file_snapshot_ci.sh 更新后）
bash tests/file_snapshot_ci.sh
```

---

### 测试矩阵汇总

| 步骤 | Group | case 数 | 关键 case |
|------|-------|---------|----------|
| 1 存储简化 | S | 3 | S1 目录结构、S2 GC mtime、S3 去重 |
| 2 tree 模型 | T | 4 | T1 tree 读写、T2 compute_diff、T3 跳过无变更、T4 O(1) 性能 |
| 3 回滚升级 | R | 4 | R1 单文件、R2 整体、R3 预览、R4 undo |
| 4 审批 | A | 8 | A1 pending、A2 approve、A3 reject、A4 全部、A5 重置、A6 net-zero、A7 无UI、A8 推送 |
| 主链路 | M | 4 | M1 审批闭环、M2 多轮、M3 回滚闭环、M4 重启不丢 |
| 单测 | D | 6 | 每步骤单元测试 + 全量 + CI 脚本 |
| **合计** | | **29** | |

---

## 四补、Harness 验证指南

> CLI 测试指南（§四）验证的是 RPC 接口连通性和返回格式。**Harness 验证验证的是真实 agent 行为**——让 agent 真的调 write/edit/bash 工具、真的触发 on_gate_check、真的走审批流程，但**不调真 LLM**。
>
> 参照 [agent-test-harness skill](https://~/.agents/skills/agent-test-harness/SKILL.md) + 项目的 FauxProvider 机制。

### 为什么需要 harness

| 场景 | CLI 测试能验证吗 | Harness 能验证吗 |
|------|-----------------|-----------------|
| RPC 接口返回正确 JSON | ✅ | ✅ |
| agent 调 write 后 SnapshotStore 有记录 | ❌（手动造数据） | ✅（真实工具执行） |
| on_gate_check 在 Stop 时触发审批 | ❌ | ✅（真实 agent loop） |
| 审批 deny 后 agent 下一轮看到"被回滚"消息 | ❌ | ✅（多轮交互） |
| re-approval 重置（approve → 改 → 回 pending） | ❌ | ✅ |

### 分层 harness 策略

```
层 1：单元测试（cargo test --lib）
  └─ 纯逻辑验证，手动构造数据，最快最精确
     覆盖：tree 读写 / compute_diff / net-zero 过滤 / restore 算法

层 2：FauxProvider Factory 集成测试（cargo test --test）
  └─ 真实 agent loop + 工具执行 + 审批 hook，LLM 用 Factory 动态控制
     覆盖：采集链路 / on_gate_check 触发 / 审批多轮交互 / re-approval

层 3：ION_FAUX_SCRIPT shell 脚本（tests/*_ci.sh）
  └─ host 模式冒烟 + RPC 连通性，grep 输出验证
     覆盖：RPC 接口 / CLI flag / 端到端冒烟

层 4：真实 API case（最后补，标 #[ignore]）
  └─ 真实 LLM 驱动 agent 做真实任务
     覆盖：真实场景验证 / 压力测试
```

### 层 2：FauxProvider Factory 审批测试（核心）

> 审批功能的深测必须用 Factory 函数——因为审批本质是"根据当前上下文决定放行/拦截"，Static/JSONL 做不到 context 动态分支。

#### H1 采集链路（write → SnapshotStore 有记录）

**文件**：`tests/file_snapshot_harness.rs`（新建）

```rust
use ion_provider::faux::*;

#[tokio::test]
async fn harness_write_creates_snapshot() {
    // 1. create: 隔离环境
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path();

    // 2. configure: 注册 FauxProvider + file_snapshot 扩展
    let mut registry = ApiRegistry::new();
    let faux = register_faux(&mut registry);

    // Factory: 第 1 次调用 → 返回 write 工具调用
    //          第 2 次调用 → 返回 Stop（触发 on_gate_check）
    faux.set_responses(vec![
        FauxResponseStep::Factory(Box::new(|_ctx, _opts, state, _model| {
            if state.call_count == 0 {
                // 第 1 轮：调 write 工具
                faux_assistant_message(
                    FauxContent::Single(faux_tool_call("write", serde_json::json!({
                        "file_path": cwd.join("test.rs").to_string(),
                        "content": "fn main() {}"
                    }))),
                    FauxMessageOptions::with_stop_reason("ToolUse"),
                )
            } else {
                // 第 2 轮：Stop
                faux_assistant_message(FauxContent::Text("done".into()), Default::default())
            }
        })),
    ]);

    // 3. execute: 跑 agent loop
    let agent = Agent::new(/* ... */);
    agent.run("写一个 hello world").await.unwrap();

    // 4. observe: 验证 SnapshotStore 有记录
    let store = agent.file_snapshot_store();
    let snaps = store.load_all_tool_snapshots();
    assert!(snaps.iter().any(|s| s.path.contains("test.rs")), "write 应被采集");
    assert!(snaps[0].after_hash.is_some(), "应有 after hash");
}
```

**✅ 验证点**：
- write 工具执行后 SnapshotStore 有 ToolSnapshot
- before_hash=None（新文件）、after_hash 有值
- on_gate_check 被触发（Stop 时）

#### H2 on_gate_check 审批拦截

```rust
#[tokio::test]
async fn harness_gate_check_triggers_approval() {
    // Factory: write → Stop
    faux.set_responses(vec![
        FauxResponseStep::Static(faux_tool_call_msg("write", json!({...}))),
        FauxResponseStep::Static(faux_assistant_message(Text("done"), Default::default())),
    ]);

    // 注入 EventBus 捕获审批事件
    let bus = Arc::new(ExtensionEventBus::new());
    let agent = Agent::new().with_event_bus(bus.clone());

    agent.run("改文件").await.unwrap();

    // observe: 检查是否推了 ApprovalRequest 事件
    let ui_events = bus.drain_ui_events();
    assert!(ui_events.iter().any(|e| e.custom_type == "ApprovalRequest"),
        "on_gate_check 应推送审批请求");
}
```

#### H3 审批 deny → 回滚 → agent 下一轮看到消息

```rust
#[tokio::test]
async fn harness_deny_rollback_and_agent_sees_it() {
    // Factory:
    //   call 0 → write 工具调用
    //   call 1 → Stop（触发审批，deny → 回滚）
    //   call 2 → 读 context.messages，验证能看到"被回滚"消息
    faux.set_responses(vec![
        FauxResponseStep::Static(faux_tool_call_msg("write", json!({...}))),
        FauxResponseStep::Static(faux_assistant_message(Text("done"), Default::default())),
        FauxResponseStep::Factory(Box::new(|ctx, _opts, _state, _model| {
            // 验证 context 里有"被回滚"消息
            let has_rollback_msg = ctx.messages.iter()
                .any(|m| m.content_contains("回滚"));
            assert!(has_rollback_msg, "deny 后 agent 应看到回滚消息");
            faux_assistant_message(Text("知道了"), Default::default())
        })),
    ]);

    // 配置审批 Extension：deny 所有文件
    let approval = ApprovalExtension::new(store).with_auto_deny();

    agent.run("改文件").await.unwrap();
    // Factory 闭包内的 assert 已经验证了
}
```

**✅ 验证点**：
- deny 后文件被回滚
- session.jsonl 追加了"被回滚"消息
- agent 下一轮 context.messages 含回滚消息

#### H4 re-approval 重置（approve → 改 → 回 pending）

```rust
#[tokio::test]
async fn harness_re_approval_reset() {
    // Factory:
    //   call 0 → write a.txt "v1"
    //   call 1 → Stop（审批 approve a.txt）
    //   call 2 → write a.txt "v2"（再改）
    //   call 3 → Stop（审批 pending 应该又有 a.txt）
    faux.set_responses(vec![
        FauxResponseStep::Static(faux_tool_call_msg("write", json!({"content":"v1"}))),
        FauxResponseStep::Static(faux_assistant_message(Text("done"), Default::default())),
        FauxResponseStep::Static(faux_tool_call_msg("write", json!({"content":"v2"}))),
        FauxResponseStep::Static(faux_assistant_message(Text("done"), Default::default())),
    ]);

    agent.run("改两次").await.unwrap();

    // 第一次 Stop 后 approve
    approval.approve("a.txt").await;

    // 第二次 Stop 后检查 pending
    let pending = approval.pending().await;
    assert!(pending.iter().any(|p| p.path.contains("a.txt")),
        "approve 后再改应回 pending（re-approval 重置）");

    // diff 应从 approved baseline 算（v1→v2），不是从 session start
    let a_pending = pending.iter().find(|p| p.path.contains("a.txt")).unwrap();
    assert_eq!(a_pending.old_content, Some("v1"));  // approved baseline
    assert_eq!(a_pending.new_content, Some("v2"));  // 当前磁盘
}
```

#### H5 net-zero 过滤（added → deleted 不显示）

```rust
#[tokio::test]
async fn harness_net_zero_filtered() {
    // Factory: write nz.txt → bash rm nz.txt → Stop
    faux.set_responses(vec![
        FauxResponseStep::Static(faux_tool_call_msg("write", json!({"content":"temp"}))),
        FauxResponseStep::Static(faux_tool_call_msg("bash", json!({"command":"rm nz.txt"}))),
        FauxResponseStep::Static(faux_assistant_message(Text("done"), Default::default())),
    ]);

    agent.run("建了又删").await.unwrap();

    let pending = approval.pending().await;
    assert!(!pending.iter().any(|p| p.path.contains("nz.txt")),
        "added→deleted 且未 approved → net-zero 过滤，不显示");
}
```

### 层 3：Shell 脚本 harness（CI 冒烟）

> `ION_FAUX_SCRIPT` JSONL 脚本驱动 host 模式，验证 RPC 连通性。

**文件**：`tests/file_snapshot_ci.sh`（现有，扩展）

```bash
# ── Group J: 审批 RPC harness（新增）──

# 造 faux 脚本：write 工具调用 → Stop
cat > /tmp/faux_approval.jsonl << 'JSONL'
{"tool_call":{"name":"write","input":{"file_path":"j1.txt","content":"harness"}}}
{"text":"done"}
JSONL

# 启 host + faux
ION_FAUX_SCRIPT=/tmp/faux_approval.jsonl ./target/debug/ion serve &
sleep 2

# 建会话 + 跑一轮
ion rpc --method create_session --params '{"cwd":"/tmp/test-j"}'
ion rpc --session <sid> --method prompt --params '{"text":"写文件"}'
sleep 1

# J1: review_pending 有 j1.txt
RESULT=$(ion rpc --session <sid> --method review_pending --params '{}')
echo "$RESULT" | grep -q "j1.txt" && pass "J1: pending 含 j1.txt" || fail "J1"

# J2: approve 后状态变更
ion rpc --session <sid> --method review_approve --params '{"path":"j1.txt"}'
RESULT=$(ion rpc --session <sid> --method review_approvals --params '{}')
echo "$RESULT" | grep -q "approved" && pass "J2: approve 生效" || fail "J2"

# J3: reject 后文件回滚
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"write","input":{"file_path":"j2.txt","content":"will_reject"}}'
ion rpc --session <sid> --method review_reject --params '{"path":"j2.txt"}'
test ! -f /tmp/test-j/j2.txt && pass "J3: reject 后文件被回滚删除" || fail "J3"
```

### 层 4：真实 case（最后补，标 `#[ignore]`）

> 真实 LLM 驱动 agent 做真实任务，验证完整审批闭环。

**文件**：`tests/file_snapshot_e2e.rs`

```rust
#[tokio::test]
#[ignore] // 需 ION_E2E=1 + API key
async fn real_agent_approval_workflow() {
    /* 1. 给 agent 一个真实任务："在 src/ 下加一个 hello.rs" */
    /* 2. agent 真的 write 文件 → Stop → on_gate_check 触发 */
    /* 3. 验证 review_pending 有 hello.rs */
    /* 4. approve → 验证文件保留 */
    /* 5. 再让 agent 改 → 验证 re-approval 重置 */
    /* 6. reject → 验证回滚 */
}

// 运行方式：
// ION_E2E=1 ION_API_KEY="sk-xxx" \
//   cargo test --test file_snapshot_e2e -- --ignored --nocapture
```

### Harness 测试矩阵

| 层 | 机制 | 文件 | case 数 | 覆盖 |
|----|------|------|---------|------|
| 1 单测 | cargo test --lib | src/file_snapshot/*_test | 已有 22 | 纯逻辑 |
| 2 Factory | FauxProvider 集成测试 | tests/file_snapshot_harness.rs（新建） | H1-H5 | 采集 + 审批 hook + 多轮 |
| 3 Shell | ION_FAUX_SCRIPT CI | tests/file_snapshot_ci.sh Group J（新增） | J1-J3 | RPC 冒烟 |
| 4 真实 | #[ignore] e2e | tests/file_snapshot_e2e.rs（新建） | E1 | 真实场景 |
| **合计** | | | **9 harness + 22 单测** | |

---

## 五、ION 相对 pi 的优势（保持不变）

| 优势 | 说明 |
|------|------|
| **双轨变更检测** | ION 工具拦截 + 扫描，pi 纯扫描。ION 更精确 |
| **不遵守 .gitignore** | ION 追踪 .env 等文件，pi 遵守 .gitignore 会漏 |
| **主动推送审批** | ION 用 on_gate_check 推送，pi 纯被动等用户 RPC |
| **更强 hash** | ION 64bit SipHash，pi 32bit fnv1a |
| **bash restore 安全** | ION before_unknown 兜底，pi 不做工具拦截无此问题 |

---

## 六、涉及文件清单（预估）

| 步骤 | 文件 | 改动类型 |
|------|------|---------|
| **1 存储简化** | object_store.rs / gc.rs / snapshot.rs | 删死代码 |
| **2 tree 模型** | 新增 tree_store.rs / 改造 snapshot.rs / 改造 mod.rs | 新功能 |
| **3 回滚升级** | restore.rs / mod.rs | 新功能 |
| **4 审批** | 新增 approval.rs / ion_worker.rs（注册）/ ion.rs（CLI） | 新功能 |
| **测试** | file_snapshot_ci.sh + 单元测试 | 每步配套 |
| **文档** | FILE_SNAPSHOT.md 更新 + 本文持续更新 | 状态同步 |

---

## 七、pi 参考源码索引

| 能力 | pi 文件 |
|------|---------|
| tree 存储 | `packages/coding-agent/src/core/file-store/internal-git.ts` |
| 快照管理 | `packages/coding-agent/src/core/file-store/file-snapshot-manager.ts` |
| 审批 | `extensions/file-review/index.ts` + `contract.ts` |
| 快照扩展 | `extensions/file-snapshot/index.ts` |
| diff | `src/core/tools/edit-diff.ts` + `src/modes/interactive/components/diff.ts` |
| 测试参考 | `test/suite/file-review-workflow.test.ts` + `test/file-snapshot-manager.test.ts` |
