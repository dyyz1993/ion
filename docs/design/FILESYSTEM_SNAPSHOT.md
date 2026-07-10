# Filesystem Snapshot — 文件系统快照与回滚

> **状态：设计稿** — 自建 content-addressable 文件变更追踪系统,与项目 git 无关。Phase 1(快照生命周期)待实现。
>
> **术语澄清**:本文档的"快照"指**文件系统级快照**——把工作区文件状态冻结成可恢复的 checkpoint。与 [CONTEXT_INDEX.md](./CONTEXT_INDEX.md) 的"上下文快照折叠"(运行时把过期 read 内容替换成占位符)完全不同,两者无依赖关系。

---

## 何时使用这个文档

启动文件系统快照功能开发时使用。本文档覆盖 **Phase 1(快照生命周期 + 双轨变更检测)的全设计**。Phase 2(基于快照的文件回滚 restore)和 Phase 3(快照操作审批)在 §9 后续工作中记录方向。

**为什么需要文件系统快照**:当前 ION 的回滚只有**会话级**(session_tree,回退对话历史)和**代码隔离级**(worktree,用完即弃),缺一个中间层——**把工作区文件状态打一个 checkpoint,后续 agent 改坏了能一键 restore 回去**。这是 agent 自主开发时的安全网。

**为什么不用项目自己的 git**:见 §1.2 旧方案否决理由。

---

## 概览

### 核心思路

建立一个**全局独立的内容存储**,与任何项目的 `.git` 无关。变更检测走**双轨**:

| 轨道 | 触发时机 | 原理 | 代价 |
|------|---------|------|------|
| **① 工具拦截** | `write`/`edit` 工具执行时 | 工具知道改了哪个文件、改前改后内容 | ~0(顺手记录) |
| **② 文件扫描** | session start + 每轮 turn 后 | 扫描项目目录,mtime+hash 对比找变化 | 只 hash mtime 变了的文件 |

两轨互补:工具拦截精确但只覆盖 ION 工具改的文件;文件扫描兜底捕获所有外部改动(bash `sed`、其他工具、用户手动改)。

### 三 Phase 路线

| Phase | 能力 | 状态 | 依赖 |
|-------|------|------|------|
| **1** | 快照生命周期(create/list/inspect/delete)+ 双轨变更检测 | 🔧 设计稿(本文档) | 无 |
| **2** | 基于快照的文件回滚(restore) | 🔧 设计稿 | Phase 1 |
| **3** | 快照操作审批(create/restore 前拦截) | 🔧 方向 | Phase 1+2 |

```
文件系统快照 + 变更检测   ← 地基
    │
    ├─→ 文件回滚 restore  ← 读 manifest + blob 写回
    │
    └─→ 快照操作审批      ← 叠加(复用 PermissionEngine,不新建)
```

### 能力清单(Phase 1)

| 能力 | 入口 | 状态 |
|------|------|------|
| 创建快照(冻结文件状态) | `fs_snapshot_create` RPC + LLM 工具 | 🔧 |
| 列出快照 | `fs_snapshot_list` RPC | 🔧 |
| 查看快照详情(文件清单+diff) | `fs_snapshot_inspect` RPC | 🔧 |
| 删除快照 | `fs_snapshot_delete` RPC | 🔧 |
| 双轨变更检测(工具拦截 + 文件扫描) | `after_tool_call` 钩子 + turn 轮末扫描 | 🔧 |
| 扫描忽略规则(跳过常见目录) | 内置 ignore 清单 | 🔧 |
| 跨项目文件追踪(展示改到其他项目) | manifest 含完整路径 | 🔧 |
| 快照元数据持久化 | `~/.ion/fs-snapshots/` | 🔧 |

### 实现状态核查清单

| # | 功能 | 状态 | 验证 |
|---|------|------|------|
| 1.1 | content-addressable blob 存储(去重) | 🔧 | `cargo test --lib fs_snap_blob_dedup` |
| 1.2 | 工具拦截记录(write/edit 改前改后) | 🔧 | `cargo test --lib fs_snap_tool_track` |
| 1.3 | 文件扫描(mtime+hash 对比) | 🔧 | `cargo test --lib fs_snap_scan` |
| 1.4 | 扫描忽略规则(skip node_modules/target/.git 等) | 🔧 | `cargo test --lib fs_snap_ignore` |
| 1.5 | `create` 快照(生成 manifest) | 🔧 | `ion rpc ... fs_snapshot_create` |
| 1.6 | `list` 快照 | 🔧 | `ion rpc ... fs_snapshot_list` |
| 1.7 | `inspect` 快照(diff 展示) | 🔧 | `ion rpc ... fs_snapshot_inspect` |
| 1.8 | `delete` 快照(blob 引用计数回收) | 🔧 | `ion rpc ... fs_snapshot_delete` |
| 1.9 | 跨项目文件追踪(展示改到项目外的文件) | 🔧 | `ion rpc ... fs_snapshot_inspect` 含外部路径 |
| 2.1 | `restore` 快照(Phase 2) | 🔧 | 待 Phase 2 |
| 3.1 | create/restore 审批拦截(Phase 3) | 🔧 | 待 Phase 3 |

---

## 1. 存储架构与变更检测

### 1.1 全局独立存储(content-addressable)

**存储位置**:`~/.ion/fs-snapshots/`(全局,不碰任何项目的 `.git`)

```
~/.ion/fs-snapshots/
├── {项目hash}--{项目名}/                    ← 按项目维度隔离
│   ├── blobs/                              ← 内容存储(content-addressable,全局去重)
│   │   ├── a1b2c3                          ← 一份文件内容只存一次(hash 命名)
│   │   ├── d4e5f6
│   │   └── ...
│   ├── snapshots/
│   │   ├── snap-0001.json                  ← manifest:路径 → hash 的完整映射
│   │   ├── snap-0002.json
│   │   └── snap-0003.json
│   └── meta.json                           ← 项目级元数据(下一个 snap 序号等)
```

**为什么用 content-addressable 而不是显式 delta:**

| 方案 | 存的是什么 | 回滚 | 删快照 |
|------|-----------|------|--------|
| 显式 delta 链 | 每次只存 diff,链式回放 | 要从基线回放整条 delta 链 | 删中间一个 delta,后面的全断 |
| **content-addressable(采用)** | **每个文件内容存一份,manifest 存完整路径→hash 映射** | **直接读目标 manifest 写回,无需回放** | **blob 引用计数,删快照只减 ref,无依赖** |

你说的"每次只存增量"是对的——content-addressable **自动实现增量**:文件内容没变 → hash 相同 → manifest 里指向同一个 blob → 零额外空间。**存储是增量的,但 manifest 是完整状态**——这是关键,回滚时不需要重建 delta 链。

**Blob 引用计数与空间回收:**

```
snap-0001: { "src/a.rs": hash_a1, "src/b.rs": hash_b2 }
snap-0002: { "src/a.rs": hash_a1, "src/b.rs": hash_b3 }  ← a.rs 没变,共享 blob

删 snap-0001:
  hash_a1 refcount: 2 → 1  (snap-0002 还在用,不删)
  hash_b2 refcount: 1 → 0  (没人用了,删掉)
  hash_b3 refcount: 1      (不动)
```

### 1.2 为什么不用项目自己的 git(旧方案否决)

| 方案 | 问题 |
|------|------|
| A. 直接 commit 到项目 .git | 污染用户 git 历史,不可接受 |
| B. 复用项目 .git + 独立 ref | ① 非 git 目录用不了;② 把 ION 快照塞进用户 `.git` 是不干净的耦合;③ 依赖项目 git 状态,用户 `git gc`/`git reset` 可能误伤 |
| C. 另起独立 git 仓库 | 文件要复制进新仓库,扫描+复制开销;且还是依赖 git 生态 |
| **D. 自建 content-addressable(采用)** | **与项目 git 完全无关,非 git 目录也能用,存储去重,删除安全** |

### 1.3 双轨变更检测

**为什么需要两轨:**

| 来源 | 工具拦截能捕获吗 | 文件扫描能捕获吗 |
|------|-----------------|-----------------|
| `write`/`edit` 工具改文件 | ✅ 精确 | ✅(下一轮扫描) |
| `bash` 执行 `sed -i`/`echo >` | ❌ 不确定 bash 改了什么 | ✅ 兜底 |
| 其他工具/用户手动改文件 | ❌ | ✅ 兜底 |
| 其他项目被改动(跨项目) | ❌ | ✅(扫描到项目外路径时) |

#### 轨道 ①:工具拦截(精确,低成本)

```
after_tool_call(call, result):
    if call.name in ["write", "edit"]:
        path = call.arguments["file_path"]
        old_content = read_current(path)      // 改前内容
        // ...工具执行...
        new_content = result.output            // 改后内容
        record_change(path, old_hash, new_hash)
```

工具拦截**零额外扫描成本**,但只覆盖 ION 工具。

#### 轨道 ②:文件扫描(兜底,捕获所有变化)

触发时机:
- **session start**:扫描一遍,建立 baseline(首次为完整快照,后续与上次对比)
- **每轮 turn 结束后**:扫描变化

扫描算法(mtime 优化,不全量 hash):

```
scan_changes(cwd, last_scan_state):
    changed = []
    for path in walk(cwd, skip=IGNORE_RULES):
        mtime = stat(path).mtime
        if path not in last_scan_state:
            // 新文件
            changed.append(path)
        elif last_scan_state[path].mtime != mtime:
            // mtime 变了,进一步 hash 确认
            if hash(path) != last_scan_state[path].hash:
                changed.append(path)
        // mtime 没变的直接跳过(零 IO)
    // 删除的文件:在 last_scan_state 但不在扫描结果里
    deleted = last_scan_state.keys() - scanned_paths
    return changed, deleted
```

**mtime 优化**:大部分文件没改(mtime 没变),直接跳过不做 hash,扫描成本只跟"变化文件数"成正比,不是"总文件数"。

#### 扫描忽略规则

```
内置忽略清单(不扫描的目录/文件):
├── .git/                              ← 所有 VCS 目录
├── node_modules/                      ← JS/TS
├── target/                            ← Rust
├── __pycache__/, *.pyc, .venv/        ← Python
├── build/, dist/, out/                ← 通用构建产物
├── .next/, .nuxt/                     ← 前端框架
├── .gradle/, .m2/                     ← JVM 构建缓存
├── vendor/                            ← Go(部分)
├── Pods/                              ← iOS
├── .ion/                              ← ION 自身数据
├── ~/.ion/tmp/                        ← 全局临时目录
└── *.log, *.lock, *.swp              ← 日志/锁/交换文件
```

**关键原则:忽略 `.gitignore`。** 不管文件是否在 gitignore 里,**只要不在上述内置忽略清单中,都扫描**。因为 agent 可能改了 gitignore 的文件(如 `.env`、临时配置),这些改动同样需要被追踪和审批。

### 1.4 跨项目文件追踪

agent 可能改到项目外的文件(bash `cd ../other && edit`、绝对路径写入)。处理:

| 场景 | 处理 | 展示 |
|------|------|------|
| 改到同项目内文件 | 正常追踪 | inspect 正常显示 |
| 改到其他项目的文件 | **记录到 manifest**(完整绝对路径),但不污染其他项目存储 | inspect **单独列出**,标注 `[跨项目]` |
| 改到全局临时目录(~/.ion/tmp) | 不追踪(在忽略清单) | — |

**审批价值**:Phase 3 审批时,能看到"这次操作改了项目内 3 个文件 + 项目外 1 个文件",跨项目改动在审批界面高亮,用户更容易发现误操作。

---

## 2. 时空代价分析

> 核心结论:content-addressable 存储,空间自动去重;mtime 优化扫描,时间只跟变化量成正比。

### 扫描时间代价

| 步骤 | 操作 | 耗时 |
|------|------|------|
| session start 全量扫描 | 遍历目录树 + hash 所有文件 | O(文件总数),首次较慢 |
| 每轮 turn 后增量扫描 | 遍历目录树(stat mtime) + 只 hash mtime 变化的文件 | O(目录数) + O(变化文件数) |

**mtime 优化的效果**:1 万文件的项目,如果一轮只改了 5 个文件:
- 目录树遍历:stat 1 万次(~50ms,纯 metadata)
- hash 计算:只对 5 个文件(~5ms)
- **总计 ~55ms**,而非全量 hash 的 ~10 秒

session start 的全量扫描可以异步,不阻塞用户对话。

### 存储空间代价

| 维度 | 代价 | 说明 |
|------|------|------|
| **单文件** | 存一份内容 blob | hash 命名,相同内容自动去重 |
| **同一文件多次快照** | 只在内容变化时产生新 blob | hash 没变 → manifest 指向旧 blob → 零额外空间 |
| **不同文件相同内容** | 共享一个 blob | content-addressable 天然去重 |
| **删除快照** | blob 引用计数,refcount=0 才删 | 删快照安全,不影响其他快照 |

**增量效果示例:**

```
snap-0001 (baseline):  src/a.rs (v1), src/b.rs (v1), 100 个文件
  存储: 100 个 blob

snap-0002 (改了 a.rs):  src/a.rs (v2), src/b.rs (v1), 100 个文件
  新增存储: 1 个 blob(a.rs v2)
  其余 99 个 blob 与 snap-0001 共享

snap-0003 (改了 b.rs):  src/a.rs (v2), src/b.rs (v2), 100 个文件
  新增存储: 1 个 blob(b.rs v2)
  其余与 snap-0001/0002 共享

3 个快照,100 文件项目,实际存储: 102 个 blob(不是 300 个)
```

### 与其他方案对比

| 方案 | 10 次快照(每次改 5 文件,1 万文件项目) | 空间 |
|------|--------------------------------------|------|
| **content-addressable(本方案)** | ~50 个 blob × 平均文件大小 | **最小** |
| 全量复制 | 10 × 1 万 = 10 万文件副本 | ~10000x |
| git ref 方案 | 类似 content-addressable | 接近,但依赖 git |

---

## 3. 数据结构

### 3.1 Snapshot Manifest(每个快照一个)

```rust
pub struct SnapshotManifest {
    /// 快照 ID
    pub id: String,                    // "snap-0001"

    /// 项目路径(此快照所属项目)
    pub project_path: String,

    /// 创建时间(Unix 毫秒)
    pub created_at: u64,

    /// 用户/agent 标签
    pub label: Option<String>,         // "before-refactor"

    /// 触发类型
    pub trigger: SnapshotTrigger,      // Baseline | ToolTrack | TurnScan

    /// 文件状态:路径 → content hash
    /// 这是"完整状态"(路径→hash 映射),不是 delta
    pub files: HashMap<String, FileHash>,

    /// 跨项目文件(单独标注,审批用)
    pub external_files: Vec<String>,

    /// 相对上一快照的变化摘要
    pub changes: ChangeSummary,
}

pub enum SnapshotTrigger {
    Baseline,        // session start 全量扫描
    ToolTrack,       // 工具拦截(write/edit)
    TurnScan,        // 每轮 turn 后扫描
}

pub struct ChangeSummary {
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub deleted: Vec<String>,
    pub external: Vec<String>,  // 跨项目改动
}
```

### 3.2 项目级元数据

**文件**:`~/.ion/fs-snapshots/{项目hash}--{项目名}/meta.json`

```rust
pub struct ProjectMeta {
    pub project_path: String,
    pub next_snap_seq: u32,                // 下一个快照序号
    pub last_scan_state: HashMap<String, FileScanEntry>,  // 上次扫描状态(mtime+hash)
    pub max_snapshots: usize,              // 默认 50
}

pub struct FileScanEntry {
    pub mtime: u64,
    pub hash: FileHash,
}
```

### 3.3 Blob 存储约定

```
~/.ion/fs-snapshots/{项目}/blobs/{hash前2位}/{hash}
```

按 hash 前两位分桶,避免单目录文件数过多。blob 内容是原始文件内容,无压缩(V1;V2 可加 zstd)。

---

## 4. 配置

通过 `config.json`:

```json
{
  "extensions": {
    "filesystem-snapshot": {
      "enabled": true,
      "max_snapshots": 50,
      "scan_on_turn": true,
      "scan_ignore_extra": [],
      "track_external": true
    }
  }
}
```

| 字段 | 默认 | 说明 |
|------|------|------|
| `enabled` | `true` | 是否启用快照能力 |
| `max_snapshots` | `50` | 超过自动删最旧 |
| `scan_on_turn` | `true` | 每轮 turn 后扫描(关闭则只靠工具拦截) |
| `scan_ignore_extra` | `[]` | 用户自定义额外忽略路径(叠加到内置清单) |
| `track_external` | `true` | 是否追踪跨项目文件改动 |

---

## 5. 主流程

### 5.1 session start 初始化

```
on_session_start(cwd):
    1. 确定 project_dir = ~/.ion/fs-snapshots/{hash(cwd)}--{basename(cwd)}/
    2. 若不存在 → 创建目录结构(blobs/ + snapshots/ + meta.json)
    3. 全量扫描 cwd → 建立 last_scan_state
    4. 生成 baseline 快照 snap-0001(完整文件列表)
       trigger = Baseline
    5. 异步执行(不阻塞对话)
```

### 5.2 双轨变更采集

```
# 轨道①:工具拦截(实时)
after_tool_call(call, result):
    if call.name in ["write", "edit"]:
        path = abs(call.arguments["file_path"])
        old = blob_store.get_or_read(path)
        new = result.output
        pending_changes.add(path, old_hash, new_hash)

# 轨道②:每轮 turn 后扫描
on_turn_end(cwd):
    changed, deleted = scan_changes(cwd, last_scan_state)
    for path in changed + deleted:
        pending_changes.add(path)
    update last_scan_state
```

### 5.3 创建快照(create)

```
fs_snapshot_create(label?, cwd):
    1. 合并 pending_changes(工具拦截 + 扫描)
    2. 若 pending_changes 为空 → 返回"无变更,无需快照"
    3. 对每个变化文件:内容存入 blob_store(去重)
    4. 构建 manifest:
       - files = 上一个快照的 files + 本次变化(完整路径→hash 映射)
       - changes = {added, modified, deleted, external}
    5. 写 snapshots/snap-{seq}.json
    6. 清空 pending_changes
    7. 若超 max_snapshots → 删最旧的(引用计数回收 blob)
    8. 返回 {id, label, changes_summary, created_at}
```

### 5.4 列出 / 查看 / 删除

```
list:   读 snapshots/ 目录 → 返回所有 manifest 摘要
inspect:读 manifest → 与上一个快照 diff → 返回变更文件列表
        full_diff=true 时返回完整 diff 文本
        跨项目文件单独列出,标注 [跨项目]
delete: 删 manifest → 对其 files 里的每个 hash 减引用计数 → refcount=0 的 blob 删掉
```

---

## 6. 接口规格

### 6.1 `fs_snapshot_create`

**请求:**

```bash
ion rpc --session <sid> --method fs_snapshot_create \
  --params '{"label":"before-refactor"}'
```

**请求参数:**

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `label` | string | 否 | 快照标签 |

**响应 JSON(成功):**

```json
{
  "type": "response",
  "id": "1",
  "command": "fs_snapshot_create",
  "success": true,
  "data": {
    "id": "snap-0002",
    "label": "before-refactor",
    "created_at": 1752192000000,
    "trigger": "turn_scan",
    "changes": {
      "added": ["src/new.rs"],
      "modified": ["src/main.rs"],
      "deleted": [],
      "external": ["/etc/hosts"]
    }
  }
}
```

**响应 JSON(无变更):**

```json
{
  "type": "response",
  "id": "1",
  "command": "fs_snapshot_create",
  "success": true,
  "data": {
    "id": null,
    "message": "无文件变更,无需快照"
  }
}
```

### 6.2 `fs_snapshot_list`

```bash
ion rpc --session <sid> --method fs_snapshot_list --params '{}'
```

**响应 JSON(成功):**

```json
{
  "type": "response",
  "id": "1",
  "command": "fs_snapshot_list",
  "success": true,
  "data": {
    "snapshots": [
      {
        "id": "snap-0002",
        "label": "before-refactor",
        "created_at": 1752192000000,
        "trigger": "turn_scan",
        "file_count": 101,
        "changes_count": 2
      },
      {
        "id": "snap-0001",
        "label": "baseline",
        "created_at": 1752191000000,
        "trigger": "baseline",
        "file_count": 100,
        "changes_count": 100
      }
    ]
  }
}
```

### 6.3 `fs_snapshot_inspect`

```bash
ion rpc --session <sid> --method fs_snapshot_inspect \
  --params '{"id":"snap-0002","full_diff":false}'
```

**响应 JSON(成功,含跨项目标注):**

```json
{
  "type": "response",
  "id": "1",
  "command": "fs_snapshot_inspect",
  "success": true,
  "data": {
    "id": "snap-0002",
    "label": "before-refactor",
    "created_at": 1752192000000,
    "diff_stat": " src/main.rs | 12 +++---\n src/new.rs  | 45 ++++++++++++++++++\n 2 files changed, 51 insertions(+), 6 deletions(-)",
    "files": [
      {"path": "src/main.rs", "status": "modified", "insertions": 6, "deletions": 6},
      {"path": "src/new.rs", "status": "added", "insertions": 45, "deletions": 0}
    ],
    "external_files": [
      {"path": "/etc/hosts", "status": "modified", "note": "[跨项目] 审批请注意"}
    ]
  }
}
```

### 6.4 `fs_snapshot_delete`

```bash
ion rpc --session <sid> --method fs_snapshot_delete \
  --params '{"id":"snap-0001"}'
```

**响应 JSON(成功):**

```json
{
  "type": "response",
  "id": "1",
  "command": "fs_snapshot_delete",
  "success": true,
  "data": {
    "deleted": "snap-0001",
    "blobs_reclaimed": 3,
    "space_reclaimed": "~12KB"
  }
}
```

---

## 7. CLI 测试指南

> 格式参照 [CLI_TEST_TEMPLATE.md](../templates/CLI_TEST_TEMPLATE.md),Group 按测试主题分组。每个 case 给可直接复制运行的 `ion rpc` 命令。

### Group A:快照生命周期(核心功能)

> 验证 create/list/inspect/delete 四个核心 RPC 的主路径。

```bash
# A1 session start 自动创建 baseline 快照
ion rpc --session <sid> --method fs_snapshot_list --params '{}'
```

**预期**:返回 `snap-0001`,`trigger: "baseline"`,`file_count` 等于项目文件数(扣除忽略)。

**✅ 验证点**:
- `~/.ion/fs-snapshots/{项目}/` 目录结构存在
- blob 存储存在,内容与项目文件一致
- 不碰项目 `.git`(`git status` 无变化)

```bash
# A2 改文件后创建增量快照
echo "// changed" >> src/main.rs
ion rpc --session <sid> --method fs_snapshot_create \
  --params '{"label":"after-change"}'
```

**预期**:返回 `id: snap-0002`,`changes.modified: ["src/main.rs"]`。

**✅ 验证点**:
- blob 存储只新增 `src/main.rs` 的新版本(旧版本 blob 仍在)
- `snap-0002` 的 manifest 含完整文件列表(含未变文件)

```bash
# A3 列出快照(降序)
ion rpc --session <sid> --method fs_snapshot_list --params '{}'
```

**预期**:snap-0002 在前,snap-0001 在后。

```bash
# A4 查看快照详情
ion rpc --session <sid> --method fs_snapshot_inspect \
  --params '{"id":"snap-0002","full_diff":false}'
```

**预期**:返回 `diff_stat` + `files` 数组,显示 src/main.rs modified。

```bash
# A5 删除快照(引用计数回收)
ion rpc --session <sid> --method fs_snapshot_delete \
  --params '{"id":"snap-0001"}'
```

**预期**:`blobs_reclaimed` 只回收 snap-0001 独有的 blob(snap-0002 共享的不回收)。list 后 snap-0001 消失。

### Group B:双轨变更检测

> 验证工具拦截 + 文件扫描两轨互补。

```bash
# B1 工具拦截:通过 write 工具改文件 → 即时记录
# (via agent: 让 agent 调 write 工具)
ion rpc --session <sid> --method fs_snapshot_inspect \
  --params '{"id":"snap-0002"}'
```

**预期**:write 工具改的文件出现在 changes 里(工具拦截轨道捕获)。

```bash
# B2 文件扫描:bash 改文件(sed -i)→ 下一轮扫描捕获
ion rpc --session <sid> --method call_tool \
  --params '{"name":"bash","arguments":{"command":"sed -i \"s/foo/bar/g\" src/lib.rs"}}'
ion rpc --session <sid> --method fs_snapshot_create --params '{}'
```

**预期**:sed 改的文件出现在 changes 里(文件扫描轨道捕获,工具拦截捕获不到)。

```bash
# B3 无变更时创建快照 → 提示无需快照
ion rpc --session <sid> --method fs_snapshot_create --params '{}'
```

**预期**:`data.id: null`,`message: "无文件变更"`。

### Group C:扫描忽略规则

> 验证内置忽略清单生效。

```bash
# C1 node_modules 下的改动不被追踪
mkdir -p node_modules/fake && echo x > node_modules/fake/index.js
ion rpc --session <sid> --method fs_snapshot_create --params '{}'
```

**预期**:无变更或 changes 不含 node_modules 文件。

```bash
# C2 gitignore 的文件仍被追踪(关键:不遵守 gitignore)
echo "secret" > .env
ion rpc --session <sid> --method fs_snapshot_create --params '{}'
```

**预期**:`.env` 出现在 changes.added(即使 .gitignore 忽略它)。

### Group D:跨项目文件追踪

> 验证改到项目外的文件能被检测和展示。

```bash
# D1 bash 改项目外文件 → 快照标注 [跨项目]
ion rpc --session <sid> --method call_tool \
  --params '{"name":"bash","arguments":{"command":"echo x > /tmp/ion-test-external.txt"}}'
ion rpc --session <sid> --method fs_snapshot_create --params '{}'
```

**预期**:inspect 返回的 `external_files` 含该路径,标注 `[跨项目]`。

**✅ 验证点**:跨项目改动单独列出,审批界面可高亮。

### Group E:单元测试 + 集成测试

```bash
# E1 单元测试:blob 去重
cargo test --lib fs_snap_blob_dedup

# E2 单元测试:mtime 优化扫描(只 hash 变化文件)
cargo test --lib fs_snap_scan_mtime

# E3 单元测试:引用计数回收
cargo test --lib fs_snap_refcount

# E4 集成测试:session start → turn scan → create → delete 全链路
cargo test --test fs_snapshot_integration
```

---

## 8. 与 Bash 扩展的协作

> 参照 [BASH_EXTENSION.md](./BASH_EXTENSION.md) 的测试用例组织方式。

Bash 扩展是文件变更的**重要来源**(`sed -i`/`echo >`/`cp`/`rm` 等)。快照系统与 Bash 的协作:

| Bash 操作 | 快照系统行为 | 测试场景 |
|-----------|-------------|---------|
| `sed -i` 改文件 | 文件扫描轨道捕获(下一轮) | Group B2 |
| `echo > file` | 文件扫描轨道捕获 | Group B2 |
| `rm file` | 文件扫描捕获(deleted) | Group B(扩展) |
| `mkdir` | 不追踪(目录无内容) | — |
| `git clone` 到项目内 | 新文件被扫描捕获 | — |

**关键**:bash 改的文件靠**文件扫描轨道**(轨道②)捕获,因为工具拦截无法解析 bash 命令改了什么。这正体现了双轨设计的必要性。

---

## 9. 后续工作

| # | 待办 | Phase | 优先级 |
|---|------|-------|--------|
| 1 | **实现 Phase 1**(content-addressable 存储 + 双轨检测 + 4 RPC) | 1 | P1(当前) |
| 2 | **文件回滚 restore**(读 manifest + blob 写回) | 2 | P2 |
| 3 | **restore 审批拦截**(覆盖工作区前确认 + 跨项目高亮) | 3 | P3 |
| 4 | blob 压缩(zstd) | — | 低 |
| 5 | 快照导出/导入 | — | 低 |

### Phase 2:文件回滚(restore)方向

```
fs_snapshot_restore(id):
    1. 读目标 manifest → 得到路径→hash 完整映射
    2. 【审批点】对比当前工作区与目标快照的差异:
       - 哪些文件会被覆盖
       - 是否有未快照的变更(危险)
       - 跨项目文件高亮
    3. 从 blob_store 读出每个 hash 的内容 → 写回工作区
    4. 对于目标 manifest 不含、但当前有的文件(快照后新增的):
       默认保留(不删),或按选项清理
    5. 返回 {restored: id, overwritten: N, skipped: M}
```

**关键**:content-addressable 设计让 restore 极其简单——manifest 是完整状态,读 blob 写回即可,**不需要回放 delta 链**。

### Phase 3:审批拦截方向

复用现有 `PermissionEngine` + `UiSystem`,不新建审批系统:

| 操作 | 审批策略 |
|------|---------|
| `create` | 无需审批(只读当前状态) |
| `restore` | **必须审批**:展示将被覆盖的文件 + 跨项目改动高亮 |
| `delete` | 无需审批(删快照不动工作区) |

审批界面要点(来自你的需求):
- 展示改动文件列表
- 跨项目改动**单独列出高亮**
- 可选择性回滚(只回滚部分文件,V2)

---

## 10. 对标分析

| 能力 | ION 现状 | 本设计 | pi 状态 |
|------|---------|--------|---------|
| 文件系统快照 | ❌ 无 | ✅ Phase 1 | ❌ 无 |
| 文件回滚 | ❌ 无 | ✅ Phase 2 | ❌ 无 |
| 快照操作审批 | ⚠️ 部分 | ✅ Phase 3 | ❌ 无 |
| 跨项目文件追踪 | ❌ 无 | ✅ manifest 标注 | ❌ 无 |
| content-addressable 存储 | ❌ 无 | ✅ blob 去重 | ❌ 无 |

> 文件系统快照是 ION 原创增量,pi 无此能力。
