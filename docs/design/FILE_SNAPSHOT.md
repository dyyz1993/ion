# File Snapshot 设计文档

> **状态：开发中** — 双路快照系统（工具级 before/after + 目录扫描兜底），内核模块。

---

## 概览

追踪 agent 在会话中改了哪些文件，提供精确 diff 和变更历史查询。对标 pi 的 `file-snapshot-manager.ts`，但简化存储——不做完整的 mini-git，而用**双路混合**方案。

| 能力 | 入口 | 状态 |
|------|------|------|
| write/edit 精确 diff | `on_tool_execution_start/end` | 🔧 待实现 |
| bash 目录扫描兜底 | `on_tool_execution_start/end` | 🔧 待实现 |
| get_modified_files | RPC | 🔧 待实现 |
| get_file_diff | RPC | 🔧 待实现 |
| get_batch_diffs | RPC | 🔧 待实现 |
| get_file_history | RPC | 🔧 待实现 |
| restore_files（回滚） | RPC | 🔧 后续 |

### 两大核心约束（贯穿全设计）

| # | 约束 | 保证手段 |
|---|------|---------|
| **A** | **不会随项目增长形成爆炸级数据** | content hash 去重 + 100MB 硬上限 + 分级 GC |
| **B** | **不会因执行时间久而越来越慢** | mtime+size 快速过滤 + 增量扫描 + LRU 缓存 |

---

## 1. 双路架构

```
┌─────────────────────────────────────────────────────┐
│ 路线 1：工具级 before/after（write/edit 专用）         │
│                                                      │
│ 触发：on_tool_execution_start(bash/write/edit)       │
│ 覆盖：write/edit 操作的所有文件（含 cwd 外）           │
│ 方式：before 读旧内容 → after 读新内容 → diff         │
│ 精度：100% 精确                                       │
│ 存储量：O(改动文件数 × 内容版本数)，去重后极小          │
│ 索引：按 (turnId, toolCallId, path) 查                │
├─────────────────────────────────────────────────────┤
│ 路线 2：cwd 目录扫描（bash 兜底）                      │
│                                                      │
│ 触发：on_tool_execution_start/end（仅 bash）           │
│ 覆盖：cwd 目录树内的文件                               │
│ 过滤：git ignore 智能过滤（见 §3）                     │
│ 方式：before 扫 mtime+size → after 扫 → 对比           │
│       只对变化的文件读内容算 hash                      │
│ 精度：mtime 启发式（够用）                             │
│ 存储量：O(变化文件数)，不存未变文件                     │
│ 索引：按 turnId 查                                    │
└─────────────────────────────────────────────────────┘
```

### 覆盖矩阵

| 改动来源 | cwd 内 | cwd 外 |
|---------|--------|--------|
| **write/edit 工具** | 路线 1：精确 diff | 路线 1：精确 diff |
| **bash 工具** | 路线 2：目录扫描（mtime 启发式） | ❌ 不追踪（极罕见，可接受） |

---

## 2. 存储设计（约束 A：不爆炸）

### 2.1 路径结构

```
~/.ion/file-store/
├── <project_key>/                    ← git-common-dir hash（worktree 共享）
│   ├── objects/                       ← content-addressed 存储（去重）
│   │   ├── ab/cdef123...             ← 文件内容 blob（前 2 位 hash 做子目录）
│   │   └── ...
│   ├── metadata/                      ← object 元数据
│   │   └── ab/cdef123.json            ← { createdAt, accessedAt, size }
│   └── snapshots/                     ← 快照元数据
│       ├── tool/                      ← 路线 1：工具级快照
│       │   └── <turnId>.jsonl         ← 每行一个 {path, beforeHash, afterHash, toolCallId}
│       └── turn/                      ← 路线 2：目录扫描快照
│           └── <turnId>.json          ← { treeHash, changedFiles: [{path, beforeHash, afterHash}] }
```

### 2.2 project_key 算法（worktree 共享）

```rust
fn project_key(cwd: &str) -> String {
    // git common dir 在主仓库和所有 worktree 里一致
    if let Ok(output) = Command::new("git")
        .args(["rev-parse", "--absolute-git-dir"])
        .current_dir(cwd)
        .output()
    {
        let git_dir = String::from_utf8_lossy(&output.stdout).trim();
        // /ion/.git → 主仓库；/ion/.git/worktrees/xxx → worktree
        // 取 common dir（去掉 worktrees/xxx 后缀）
        let common = git_dir.split("/worktrees/").next().unwrap_or(git_dir);
        return hash(common);
    }
    hash(cwd) // 非 git：fallback 到 cwd
}
```

**主仓库和 worktree 共享同一个存储目录**——不会被识别成新项目。

### 2.3 content-addressed 去重

```
文件内容 "hello world" → sha256 → "b94d27b9..."
├── 已存在？→ 跳过写入（不重复存）
└── 不存在？→ 写入 objects/b9/4d27b9...
```

**同一个文件被改 100 次**：100 个不同内容版本 → 100 个 object（hash 不同）。
**相同内容出现两次**（如改回去）：只存 1 个 object（hash 相同，命中去重）。

### 2.4 存储增长分析

| 场景 | 快照元数据 | object 存储 | 总量 |
|------|-----------|------------|------|
| 100 轮纯对话（无文件改动） | 0 条 | 0 个 | **0** |
| 100 轮，每轮改 3 个文件（各不同内容） | 100 × 3 = 300 条（KB 级） | 300 个 object | **~几 MB** |
| 100 轮，反复改同一个文件（内容不重复） | 100 条 | 100 个 object | **~1 MB** |
| 100 轮，反复改同一个文件（内容重复） | 100 条 | 1 个 object（去重） | **~几 KB** |
| 1000 轮大会话 | 1000 × N 条 | ≤ 100MB（GC 封顶） | **≤ 100MB** |

**关键**：增长是 O(唯一内容版本数)，不是 O(turn × 文件数)。

---

## 3. git ignore 智能过滤

### 3.1 规则（你的方案）

```
扫描 cwd 目录树时，遇到 git ignore 条目：

git ignore 条目 → 是文件夹吗？
  ├─ 是文件夹（target/、node_modules/、.cache/）→ 跳过整个文件夹
  │
  └─ 是文件（.env、*.log、secret.key）
      └─ 检查是否二进制？
          ├─ 二进制 → 跳过
          └─ 文本 → 记录！（.env 等被改了也要追踪）
```

### 3.2 二进制检测

读文件前 8KB，如果满足任一条件判定为二进制：
- 含 null byte（`\0`）
- 不可打印字符占比 > 30%

### 3.3 额外硬限制

| 限制 | 值 | 原因 |
|------|----|------|
| 单文件大小上限 | 1MB | 大文件不进快照 |
| 单次扫描总量上限 | 50MB | 防大仓库卡死 |
| 单次扫描文件数上限 | 5000 | 同上 |
| 超限时 | 标注 `truncated: true`，停止扫描 | 优雅降级 |

---

## 4. 性能设计（约束 B：不变慢）

### 4.1 mtime+size 快速过滤（路线 2 核心）

```
scan_dir(cwd) 遍历文件：
  ├─ 记录 (path, mtime, size)    ← 只 stat，不读内容，极快
  ├─ 对比 before 快照的 (path, mtime, size)
  │
  ├─ mtime + size 没变 → 跳过（99% 的文件）
  └─ mtime 或 size 变了 → 读内容算 hash → 存 diff
```

**5000 个文件扫描 < 100ms**（只 stat，不读内容）。

### 4.2 增量索引

快照元数据按 turnId 索引，查询 `get_modified_files(turnId: 5)` 是 O(1) 索引查找，不扫描。

### 4.3 LRU 缓存

- 目录扫描结果缓存（同 cwd + mtime 未变 → 复用）
- tree hash 缓存（最多 10 棵树 LRU）

### 4.4 异步 GC

GC 只在**启动时**跑一次（跟 pi 一样），不阻塞 agent 执行。分级策略：

```
启动时检查 file-store 总大小：
  ├─ ≤ 100MB → 跳过
  ├─ > 100MB → 分级清理：
  │   ├─ 第 1 步：删 7 天前的 object（保留 active treeHash 引用的）
  │   ├─ 第 2 步：还超 → 删 1 天前的
  │   └─ 第 3 步：还超 → 可达性分析（从 tree 出发，删不可达的）
  └─ 完成
```

**GC 不阻塞 agent**——启动时异步 `void` 执行，不等待。

### 4.5 性能不会随会话变长的保证

| 担心 | 保证 |
|------|------|
| 快照越来越多，查询越来越慢 | 按 turnId 索引，O(1) 查找，不随 turn 数增长 |
| object store 越来越大，读写越来越慢 | content hash 寻址，O(1) 查找 + 100MB GC 封顶 |
| 扫描全目录越来越慢 | mtime+size 过滤，只读变化的文件；不变文件 stat 后跳过 |
| 1000 轮会话内存爆炸 | 快照元数据在磁盘（JSONL），不全部加载到内存；按 turn 按需读 |

---

## 5. 数据结构

### 5.1 工具级快照（路线 1）

```rust
/// 路线 1：write/edit 的 before/after 记录
#[derive(Serialize, Deserialize)]
struct ToolSnapshot {
    turn_id: u32,
    tool_call_id: String,
    tool_name: String,         // "write" | "edit"
    path: String,              // 文件路径（可能 cwd 外）
    before_hash: Option<String>,  // 执行前内容 hash（None = 文件不存在）
    after_hash: Option<String>,   // 执行后内容 hash（None = 文件被删除）
    timestamp: String,
}
```

### 5.2 目录扫描快照（路线 2）

```rust
/// 路线 2：bash 前后的目录扫描对比
#[derive(Serialize, Deserialize)]
struct DirSnapshot {
    turn_id: u32,
    tool_call_id: String,      // 触发扫描的 bash 命令
    changed_files: Vec<DirFileChange>,
    truncated: bool,           // 是否因超限而截断
}

#[derive(Serialize, Deserialize)]
struct DirFileChange {
    path: String,              // 相对 cwd 的路径
    status: ChangeStatus,      // added | modified | deleted
    before_hash: Option<String>,
    after_hash: Option<String>,
}

#[derive(Serialize, Deserialize)]
enum ChangeStatus {
    Added,
    Modified,
    Deleted,
}
```

### 5.3 object store 元数据

```rust
#[derive(Serialize, Deserialize)]
struct ObjectMeta {
    hash: String,
    size: u64,
    created_at: String,        // ISO 8601
    accessed_at: String,       // 最近访问时间
}
```

---

## 6. 采集流程（详细）

### 6.1 路线 1：write/edit before/after

```rust
// on_tool_execution_start（write/edit 工具）
fn before_tool(tool_name: &str, args: &Value, store: &ObjectStore) -> BeforeState {
    if tool_name != "write" && tool_name != "edit" { return BeforeState::Skip; }
    let path = args.get("file_path")?.as_str()?;
    let before_content = fs::read(path).ok();  // 可能不存在（新文件）
    let before_hash = before_content.as_ref().map(|c| store.write_object(c));
    BeforeState::Capture { path, before_hash }
}

// on_tool_execution_end（write/edit 工具）
fn after_tool(before: &BeforeState, store: &ObjectStore, turn_id: u32, tool_call_id: &str) {
    let (path, before_hash) = match before { BeforeState::Capture { .. } => ..., _ => return };
    let after_content = fs::read(&path).ok();  // 可能被删除
    let after_hash = after_content.as_ref().map(|c| store.write_object(c));
    // 存 ToolSnapshot
    save_tool_snapshot(ToolSnapshot { turn_id, tool_call_id, path, before_hash, after_hash, .. });
}
```

### 6.2 路线 2：bash 目录扫描

```rust
// on_tool_execution_start（bash 工具）
fn before_bash(cwd: &str) -> DirScanResult {
    scan_dir_fast(cwd)  // 返回 HashMap<path, (mtime, size)>，不读内容
}

// on_tool_execution_end（bash 工具）
fn after_bash(cwd: &str, before: &DirScanResult, store: &ObjectStore) -> DirSnapshot {
    let after = scan_dir_fast(cwd);
    let mut changes = Vec::new();
    for (path, (mtime, size)) in &after {
        match before.get(path) {
            None => {
                // 新文件
                let hash = store.write_object(&fs::read(path));
                changes.push(DirFileChange { path, status: Added, before_hash: None, after_hash: hash });
            }
            Some((b_mtime, b_size)) if mtime != b_mtime || size != b_size => {
                // 改了
                let before_hash = ...;  // before 时没读内容，只能从上一轮快照取
                let after_hash = store.write_object(&fs::read(path));
                changes.push(DirFileChange { path, status: Modified, before_hash, after_hash });
            }
            _ => {} // 没变，跳过
        }
    }
    // 检查删除的文件
    for (path, _) in &before {
        if !after.contains(path) {
            changes.push(DirFileChange { path, status: Deleted, ... });
        }
    }
    DirSnapshot { changed_files: changes, .. }
}
```

---

## 7. RPC 接口

### 7.1 get_modified_files

```bash
ion rpc --session <sid> --method get_modified_files \
  --params '{"fromTurn": 1, "toTurn": 5}'
```

**响应：**

```json
{
  "files": [
    {
      "path": "src/main.rs",
      "status": "modified",
      "source": "tool",            // 路线 1（精确）
      "turnId": 3,
      "toolCallId": "tc_abc",
      "hasDiff": true
    },
    {
      "path": ".env",              // git ignore 但被改了（文本）
      "status": "modified",
      "source": "turn_scan",       // 路线 2（目录扫描）
      "turnId": 3,
      "toolCallId": "tc_def",
      "hasDiff": true
    },
    {
      "path": "/etc/hosts",        // cwd 外，write 工具改的
      "status": "modified",
      "source": "tool",
      "turnId": 5,
      "toolCallId": "tc_ghi",
      "hasDiff": true
    }
  ],
  "summary": { "added": 1, "modified": 3, "deleted": 0 }
}
```

**参数：**

| 参数 | 类型 | 说明 |
|------|------|------|
| `fromTurn` | number | 起始 turn（可选） |
| `toTurn` | number | 结束 turn（可选） |
| `fromEntryId` | string | 起始 entry（可选，优先于 fromTurn） |
| `toEntryId` | string | 结束 entry（可选） |

### 7.2 get_file_diff

```bash
ion rpc --session <sid> --method get_file_diff \
  --params '{"filePath": "src/main.rs", "fromTurn": 1, "toTurn": 5}'
```

**响应：**

```json
{
  "path": "src/main.rs",
  "diff": "--- before\n+++ after\n@@ -10,3 +10,4 @@\n fn main() {\n-    println!(\"hello\");\n+    println!(\"hello world\");\n+    new_line();\n }\n",
  "beforeHash": "b94d27b9...",
  "afterHash": "c3ab8ff1...",
  "hasContent": true
}
```

### 7.3 get_batch_diffs

```bash
ion rpc --session <sid> --method get_batch_diffs \
  --params '{"fromTurn": 1, "toTurn": 5}'
```

**响应（聚合统计 + 各文件 diff）：**

```json
{
  "files": [
    { "path": "src/main.rs", "diff": "...", "added": 2, "removed": 1 },
    { "path": "Cargo.toml", "diff": "...", "added": 1, "removed": 0 }
  ],
  "summary": { "files": 2, "added": 3, "removed": 1 }
}
```

### 7.4 get_file_history

```bash
ion rpc --session <sid> --method get_file_history \
  --params '{"filePath": "src/main.rs"}'
```

**响应（按 turn 的时间线）：**

```json
{
  "path": "src/main.rs",
  "history": [
    { "turnId": 1, "action": "added", "toolCallId": "tc_001", "hash": "aaa..." },
    { "turnId": 3, "action": "modified", "toolCallId": "tc_005", "hash": "bbb..." },
    { "turnId": 5, "action": "modified", "toolCallId": "tc_010", "hash": "ccc..." }
  ]
}
```

---

## 8. GC 设计（约束 A 兜底）

### 8.1 触发时机

**启动时跑一次**，异步执行不阻塞 agent。

### 8.2 分级策略

```
enforceLimit(100MB):
  ├─ getStoreSize() ≤ 100MB → 跳过
  ├─ pruneOldObjects(maxAge = 7天)
  │   └─ 删 createdAt < now-7天 且不被 active treeHash 引用的 object
  ├─ getStoreSize() 还超？
  │   └─ pruneOldObjects(maxAge = 1天)
  └─ getStoreSize() 还超？
      └─ gc(activeTreeHashes)
          └─ 从所有 tree 出发做可达性分析，删不可达 object
```

### 8.3 active treeHash 保护

GC 前先收集当前会话的所有活跃 treeHash（sessionStart + 所有快照），作为白名单——保证当前会话用到的内容不会被误删。

---

## 9. 内核模块结构

```
src/file_snapshot/
├── mod.rs              ← 模块入口 + FileSnapshotExtension（实现 Extension trait）
├── object_store.rs     ← content-addressed 存储（write_object / read_object / 去重）
├── scanner.rs          ← cwd 目录扫描 + git ignore 智能过滤 + 二进制检测
├── snapshot.rs         ← ToolSnapshot / DirSnapshot 数据结构 + 采集逻辑
├── diff.rs             ← unified diff 生成
└── gc.rs               ← 垃圾回收（enforceLimit / pruneOldObjects / gc）
```

接入方式：`FileSnapshotExtension` 实现 `Extension` trait，注册到 agent 的 ExtensionRegistry，通过 `on_tool_execution_start/end` 钩子触发采集。

---

## 10. 实现计划

| 阶段 | 内容 | 难度 |
|------|------|------|
| **Phase 1** | object_store（content-addressed 去重存储）+ project_key | 中 |
| **Phase 2** | 路线 1：write/edit before/after 采集 + ToolSnapshot 存储 | 小 |
| **Phase 3** | get_modified_files + get_file_diff + get_file_history RPC | 小 |
| **Phase 4** | scanner（目录扫描 + git ignore 智能过滤） | 中 |
| **Phase 5** | 路线 2：bash before/after 目录扫描对比 | 中 |
| **Phase 6** | GC（enforceLimit + pruneOldObjects + 可达性分析） | 中 |
| **Phase 7** | CLI 测试（Group A-F） | 小 |

**先做 Phase 1-3**（路线 1 完整链路），验证 write/edit 精确 diff + RPC 查询。再补 Phase 4-6（路线 2 + GC）。

---

## 11. CLI 测试指南

### Group A: object_store 基础

#### A1 写入 + 去重

```bash
# 写入内容 → 得到 hash
ion rpc --session <sid> --method _snapshot_write_object \
  --params '{"content": "hello world"}'
# → { "hash": "b94d27b9..." }

# 再次写入相同内容 → 去重（返回相同 hash，不重复写）
ion rpc --session <sid> --method _snapshot_write_object \
  --params '{"content": "hello world"}'
# → { "hash": "b94d27b9...", "deduped": true }
```

**验证点：**
- ✅ 相同内容返回相同 hash
- ✅ 去重标记 `deduped: true`
- ✅ object 文件只存了一份

#### A2 读取

```bash
ion rpc --session <sid> --method _snapshot_read_object \
  --params '{"hash": "b94d27b9..."}'
# → { "content": "hello world" }
```

---

### Group B: write/edit 精确 diff（路线 1）

#### B1 write 新文件

```bash
# agent write 一个新文件
ion rpc --session <sid> --method call_tool \
  --params '{"tool": "write", "input": {"file_path": "test_new.txt", "content": "new content"}}'

# 查改动
ion rpc --session <sid> --method get_modified_files
```

**验证点：**
- ✅ `test_new.txt` 出现在结果里，`status: "added"`
- ✅ `source: "tool"`（路线 1）
- ✅ `beforeHash: null`（新文件）

#### B2 edit 已有文件

```bash
# 先 write 一个文件
ion rpc --session <sid> --method call_tool \
  --params '{"tool": "write", "input": {"file_path": "test_edit.txt", "content": "line1\nline2"}}'

# 再 edit 它
ion rpc --session <sid> --method call_tool \
  --params '{"tool": "edit", "input": {"file_path": "test_edit.txt", "old_string": "line2", "new_string": "line2 modified"}}'

# 查 diff
ion rpc --session <sid> --method get_file_diff \
  --params '{"filePath": "test_edit.txt"}'
```

**验证点：**
- ✅ diff 正确显示 `line2` → `line2 modified`
- ✅ `beforeHash` 和 `afterHash` 不同
- ✅ `source: "tool"`

#### B3 write 项目外文件

```bash
# write 到 /tmp
ion rpc --session <sid> --method call_tool \
  --params '{"tool": "write", "input": {"file_path": "/tmp/test_external.txt", "content": "external"}}'

# 查改动
ion rpc --session <sid> --method get_modified_files
```

**验证点：**
- ✅ `/tmp/test_external.txt` 出现在结果里
- ✅ `source: "tool"`（项目外也追踪）

#### B4 文件删除

```bash
# write 然后 bash rm
ion rpc --session <sid> --method call_tool \
  --params '{"tool": "write", "input": {"file_path": "test_del.txt", "content": "x"}}'
ion rpc --session <sid> --method call_tool \
  --params '{"tool": "bash", "input": {"command": "rm test_del.txt"}}'

# 查改动
ion rpc --session <sid> --method get_modified_files
```

**验证点：**
- ✅ `test_del.txt` 出现，`status: "deleted"`

---

### Group C: 按轮次查询

#### C1 get_modified_files 按 turn 范围

```bash
# 造 3 轮改动
# Turn 1: write a.txt
# Turn 2: write b.txt
# Turn 3: edit a.txt

# 查 Turn 2-3 的改动
ion rpc --session <sid> --method get_modified_files \
  --params '{"fromTurn": 2, "toTurn": 3}'
```

**验证点：**
- ✅ 只返回 Turn 2-3 的改动（b.txt + a.txt 的 edit）
- ✅ Turn 1 的 a.txt 初始 write 不在结果里

#### C2 get_file_history

```bash
# a.txt 被 write（Turn 1）+ edit（Turn 3）
ion rpc --session <sid> --method get_file_history \
  --params '{"filePath": "a.txt"}'
```

**验证点：**
- ✅ 返回 2 条历史（Turn 1 added + Turn 3 modified）
- ✅ 每条带 turnId + hash

#### C3 get_batch_diffs

```bash
ion rpc --session <sid> --method get_batch_diffs \
  --params '{"fromTurn": 1, "toTurn": 3}'
```

**验证点：**
- ✅ 返回所有改动文件的 diff
- ✅ summary 统计 added/removed 行数

---

### Group D: 目录扫描（路线 2，bash 兜底）

#### D1 bash 间接改文件

```bash
# bash 用 sed 改文件
ion rpc --session <sid> --method call_tool \
  --params '{"tool": "write", "input": {"file_path": "sed_test.txt", "content": "hello"}}'
ion rpc --session <sid> --method call_tool \
  --params '{"tool": "bash", "input": {"command": "sed -i s/hello/world/ sed_test.txt"}}'

# 查改动
ion rpc --session <sid> --method get_modified_files
```

**验证点：**
- ✅ `sed_test.txt` 出现，`source: "turn_scan"`（路线 2）
- ✅ status: "modified"

#### D2 git ignore 智能过滤

```bash
# 造一个被 git ignore 的 .env 文件（文本）
echo "*.env" > .gitignore
ion rpc --session <sid> --method call_tool \
  --params '{"tool": "bash", "input": {"command": "echo KEY=val > test.env"}}'

# 造一个被 git ignore 的文件夹
ion rpc --session <sid> --method call_tool \
  --params '{"tool": "bash", "input": {"command": "mkdir -p target && echo x > target/out"}}'

# 查改动
ion rpc --session <sid> --method get_modified_files
```

**验证点：**
- ✅ `test.env`（git ignore 文本文件）**出现**在结果里
- ✅ `target/out`（git ignore 文件夹）**不出现**在结果里

#### D3 大文件跳过

```bash
# 写一个 > 1MB 的文件
ion rpc --session <sid> --method call_tool \
  --params '{"tool": "bash", "input": {"command": "dd if=/dev/zero of=big.bin bs=1m count=2"}}'

# 查改动
ion rpc --session <sid> --method get_modified_files
```

**验证点：**
- ✅ `big.bin` 不在结果里（超 1MB 跳过）

---

### Group E: worktree 共享

#### E1 主仓库和 worktree 共享存储

```bash
# 主仓库跑会话 → 改文件
# worktree 里跑另一个会话 → 改不同文件
# 查 get_modified_files 应该各自独立但共享同一 project_key
ion rpc --session <sid_main> --method get_modified_files
ion rpc --session <sid_wt> --method get_modified_files
```

**验证点：**
- ✅ 两个会话的改动各自独立
- ✅ 但 project_key 相同（验证 `git rev-parse --absolute-git-dir`）

---

### Group F: GC

#### F1 存储不超 100MB

```bash
# 跑大量操作后检查 file-store 大小
du -sh ~/.ion/file-store/<project_key>/
# 应 ≤ 100MB
```

#### F2 启动时 GC 触发

```bash
# 手动造 > 100MB 的 object
# 重启 ion → GC 应触发清理
# 检查清理后大小
```

---

## 12. 不在本期范围

- ❌ `restore_files`（文件回滚恢复）— 后续，需设计原子性
- ❌ 全工作目录 hash tree（pi 的 treeHash 机制）— 我们用文件级 hash 更简单
- ❌ 远程/容器内的文件追踪 — 仅追踪本地 cwd
- ❌ 实时 file watcher（inotify/fsevent）— 用 mtime 轮询够用

---

## 13. 关键设计决策索引

| 决策 | 理由 |
|------|------|
| 双路混合（工具级 + 目录扫描） | write/edit 精确 + bash 兜底，互补盲区 |
| project_key 用 git-common-dir | worktree 共享，不被识别成新项目 |
| git ignore 智能过滤（文件夹跳过/文件查二进制） | 保留 .env 等有价值的 ignore 文件 |
| content-addressed 去重 | 相同内容只存一次，避免爆炸 |
| mtime+size 快速过滤 | 5000 文件 < 100ms，不变慢 |
| 100MB 硬上限 + 分级 GC | 硬封顶防爆炸 |
| 按磁盘存（JSONL + object），不全加载内存 | 1000 轮不爆内存 |
| GC 仅启动时跑 | 不阻塞 agent 执行 |

---

## 14. 行为契约（测试前必须固定）

以下契约经过评审确认，测试必须按此断言，禁止自行猜测语义。

### 14.1 Turn 范围语义

- `fromTurn` 和 `toTurn` 均为**包含边界**（inclusive）
- 未传 `fromTurn` 时，从会话第一个 turn 开始
- 未传 `toTurn` 时，查询到当前最新 turn
- `fromTurn > toTurn` 时返回参数错误

### 14.2 路径表示

- **cwd 内文件**：规范化相对路径（去 `..`、去 `.`）
- **cwd 外文件**：规范化绝对路径
- 路径禁止包含未处理的 `..`
- Unicode、空格、括号、emoji 路径必须正常工作

### 14.3 快照来源（source 字段）

每条变更记录的 `source` 字段区分来源：

| source | 来源 |
|--------|------|
| `tool_write` | write 工具的 before/after |
| `tool_edit` | edit 工具的 before/after |
| `turn_scan` | bash 目录扫描对比 |

### 14.4 GC 后的 RPC 行为

当 object 被 GC 删除但元数据还在时：

```json
{
  "diffAvailable": false,
  "error": { "code": "SNAPSHOT_OBJECT_MISSING" }
}
```

- **不崩溃**，不返回伪造的空 diff
- 批量查询返回部分成功 + `partialErrors`

### 14.5 Worktree 隔离

- 主仓库和 worktree **共享** object store（同一个 project_key）
- 但**变更记录按 session 隔离**（各自独立的 ToolSnapshot）
- 用主仓库 session 查询时，**看不到** worktree session 的修改
- 相同内容在主仓库和 worktree 中只存**一个 object**
- worktree 删除后，只要 session 历史仍引用 object，可达性 GC **不得删除**

---

## 15. 已知限制（XFail）

以下场景是当前系统的能力边界，测试应标记为 XFail（预期失败/已知限制）：

| # | 场景 | 当前行为 | 限制原因 | 何时修复 |
|---|------|---------|---------|---------|
| X1 | 编辑器原子保存（tmp + rename） | 路线 2 捕获 tmp 的 added+deleted 噪声 | 无 rename 检测 | 后续加 rename 检测 |
| X2 | bash 修改后恢复原内容 | mtime 变了会误报 modified | 路线 2 只比 mtime+size | 后续加内容 hash 对比 |
| X3 | mtime 碰撞（cp --preserve） | 漏检（mtime+size 不变但内容变了） | 同 X2 | 同 X2 |
| X4 | touch 只改 mtime | 误报 modified | 同 X2 | 同 X2 |
| X5 | chmod 只改权限 | 不记录（正确行为） | 只追踪内容 | 不需修复 |
| X6 | 符号链接逃逸 | 存的是 symlink 路径不是 resolved | 无 resolve 逻辑 | 后续加 resolvedPath |
| X7 | 路线 2 的 before 内容缺失 | bash 路线没存 before content，diff 只有 after | 设计限制 | 后续从上一轮快照取 |

---

## 16. 补充 CLI 测试 case（评审后新增）

### P0 — 真实端到端 + 关键边界

| # | 场景 | 验证点 |
|---|------|--------|
| H1 | write 新文件 → get_modified_files | status=added, source=tool_write |
| H2 | edit 已有文件 → get_file_diff | 精确 diff, source=tool_edit |
| H3 | write 到 /tmp（项目外）→ get_modified_files | 绝对路径, source=tool_write |
| H4 | bash sed 改文件 → get_modified_files | source=turn_scan |
| H5 | 按 turn 范围查（Turn 2-3） | 只返回范围内 |
| H6 | 同文件多轮改动 → get_file_history | 完整时间线 |
| H7 | 编辑器原子保存（tmp+rename） | XFail：tmp 噪声（X1） |
| H8 | bash 修改后恢复原内容 | XFail：误报 modified（X2） |
| H9 | 工具失败但文件已改 | 快照仍捕获 |
| H10 | 同文件 added→modified→deleted | 聚合语义固定 |
| H11 | 符号链接逃逸 | XFail：路径语义（X6） |
| H12 | 文件 rename | old=deleted + new=added，object 去重 |

### P1 — 边界场景

| # | 场景 | 验证点 |
|---|------|--------|
| I1 | 空文件（0 字节）write | 正常记录 |
| I2 | 大文件（> 1MB） | 跳过 + skipReason |
| I3 | write 相同内容 | 不记录（无变化） |
| I4 | 文件被删除（bash rm） | status=deleted |
| I5 | .env（git ignore 文本） | **保留**在结果 |
| I6 | target/ 下文件（git ignore 文件夹） | **不出现** |
| I7 | 中文/Unicode 文件名 | 正常 |
| I8 | touch 只改 mtime | XFail：误报（X4） |
| I9 | chmod 只改权限 | 不记录（正确） |
| I10 | 非法 UTF-8 文本 | 按二进制跳过 |
| I11 | 嵌套 .gitignore + !negation | 正确解析 |
| I12 | 两个不同路径写相同内容 | 两个事件 + 一个 object |
| I13 | 非 Git 项目 | project_key = hash(cwd) |

### P2 — 压力和性能

| # | 场景 | 验证点 |
|---|------|--------|
| J1 | 一轮 bash 改 100 个文件 | 全部捕获，宽松超时 |
| J2 | 超过 5000 文件 | truncated=true |
| J3 | 1000 轮会话查询 | 实际耗时记录（不断言 O(1)） |
| J4 | GC 后查历史 | diffAvailable=false + SNAPSHOT_OBJECT_MISSING |
| J5 | mtime 碰撞 | XFail：漏检（X3） |
