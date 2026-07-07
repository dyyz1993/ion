# Session Tree（会话分支）设计文档

> **状态：设计稿** — 文件内分支（B）+ 从分支点 fork。数据模型已就绪（`parent_session` 字段已存在、entry 均带 `parentId`、扩展钩子已 stub），待实现。

---

## 何时使用这个文档

启动 Session Tree 功能开发时使用。覆盖：能力清单 → 数据模型 → 主流程 → CLI 接口 → 完整 CLI 测试指南（Group A/B/C/D/E）。

**参考样本**：[BASH_EXTENSION.md](./BASH_EXTENSION.md)、[COMPACTION.md](./COMPACTION.md)（同样的"设计 + Group 测试"结构）。

**对齐 pi**：`packages/coding-agent/src/core/session-manager.ts` 的 `getTree` / `branch` / `createBranchedSession` / `forkFrom` / leaf 恢复算法。

---

## 概览

让一个 session 文件内部能形成**树**——用户/Agent 可以回退到任意一条消息，从那里分叉出一条新路径，原路径完整保留。支持给分支命名、切换分支、从某个分支点提取出独立的新 session 文件。

| 能力 | 入口 | 状态 |
|------|------|------|
| leaf 指针持久化 | `LeafPointerEntry`（新 entry 类型） | 🔧 设计稿 |
| 树构建 | `SessionFile::get_tree()` | 🔧 |
| 分支（移动 leaf） | `ion --branch <entry-id>` / `branch_session` 工具 | 🔧 |
| 分支命名 | `ion --branch <id> --name <name>`（复用 `LabelEntry`） | 🔧 |
| 切换分支 | `ion --checkout <name>` | 🔧 |
| 回滚（移 leaf 回退，路径保留） | `ion --rollback <entry-id>` | 🔧 |
| 查看树 | `ion session tree <sid>` / `ion session branches <sid>` | 🔧 |
| 从分支点 fork | `ion --fork-from-leaf <sid>/<entry-id>`（新 session 文件，记 `parentSession`） | 🔧 |
| Agent 自主分叉 | `branch_session` 工具 | 🔧 |
| 扩展钩子激活 | `on_session_before_fork` / `on_session_tree`（已 stub） | 🔧 |

### 实现状态核查清单

| # | 功能 | 状态 | 验证 |
|---|------|------|------|
| 1.1 | `LeafPointerEntry` 结构 + 序列化 | 🔧 | `cargo test --lib session_jsonl` |
| 1.2 | `get_tree()` 构建器 | 🔧 | 同上 |
| 1.3 | leaf 恢复算法（load 时） | 🔧 | 同上 |
| 2.1 | `--branch` CLI flag | 🔧 | CLI Group A |
| 2.2 | `--branch --name` + `--checkout` + `--rollback` | 🔧 | CLI Group B |
| 2.3 | `session tree` / `session branches` 子命令 | 🔧 | CLI Group A |
| 3.1 | `--fork-from-leaf` 提取新 session | 🔧 | CLI Group C |
| 4.1 | `branch_session` Agent 工具（含 rollback） | 🔧 | CLI Group D |
| 5.1 | 扩展钩子激活 | 🔧 | CLI Group D |
| 6.1 | compaction 安全检查 | 🔧 | CLI Group E（XFail） |
| 7.1 | only-append 不变量验证 | 🔧 | CLI Group F（审计） |

---

## 0. 核心不变量：Only-Append（强制）

> **本功能所有操作禁止修改、删除、重排 session 文件中的任何已有 entry。所有状态变更必须通过追加新 entry 实现。**

这条不变量覆盖 branch / checkout / rollback / fork 等所有操作。理由：
1. **可追溯**——历史完整保留，任何时刻能重建任意分支状态
2. **可回滚的回滚**——回滚本身也只追加，因此回滚可逆
3. **并发安全**——多进程/多 Worker 追加互不破坏（靠文件 append 原子性）
4. **崩溃安全**——写一半崩溃只丢最后一条 entry，不损坏历史

### 0.1 操作 → append 映射表

| 操作 | 追加的 entry | 是否改旧行？ |
|------|-------------|------------|
| 分叉（branch） | `LeafPointerEntry{leafId:X}` | ❌ 否 |
| 命名（name） | `LabelEntry{targetId, label}` | ❌ 否 |
| 切换（checkout） | `LeafPointerEntry{leafId:Y}` | ❌ 否 |
| **回滚（rollback）** | `LeafPointerEntry{leafId:X}` + 可选 `BranchSummaryEntry{summary:"rollback..."}` | ❌ 否 |
| 追加消息 | `MessageEntry{parentId: leaf}` | ❌ 否 |
| 从 leaf fork | 写**新文件**，源文件 0 字节修改 | ❌ 否 |

### 0.2 回滚的语义（与 branch 同构）

回滚在 only-append 模型里**就是 branch**——都是移动 leaf 指针。区别只在意图和 tombstone 标记：

```
假设当前 leaf = msg_010，用户说"回滚到 msg_005"

→ 追加：LeafPointerEntry{leafId: msg_005}
→ 追加：BranchSummaryEntry{parentId: msg_010, summary: "rollback: msg_005..msg_010",
                          fromId: msg_010, fromHook: false}
→ current_leaf 重解析为 msg_005（或其后最深后代）

msg_005..msg_010 这段路径完整保留在文件里——getTree 仍可见，可随时 checkout 回去。
```

关键属性：
- **路径不丢**——被回滚的消息仍在 JSONL 里
- **可逆**——回滚本身也是 append，再 checkout 回 msg_010 即恢复
- **可标记**——BranchSummaryEntry 的 `summary` 字段记录"为什么放弃"，纯文本无需 LLM
- **上下文恢复**——load 时 LLM 收到的 messages 是 root→current_leaf 的路径，被回滚段天然不在其中

### 0.3 禁止的操作（审计红线）

| 操作 | 禁止理由 |
|------|---------|
| 删除 entry | 破坏 parentId 链，getTree 断裂 |
| 修改 entry 内容 | 历史不可变，改了无法追溯 |
| 重排 entry 顺序 | append 顺序 = 时间线，重排破坏时序语义 |
| 重生 entry id | 破坏 parent 引用，跨文件 fork 失效 |
| 截断文件 | 同"删除"，破坏 only-append |

这些在 §4 Group F 有自动化审计测试。

---

## 1. 配置

### 1.1 新增 CLI 参数

**文件**：[src/bin/ion.rs](file:///Users/xuyingzhou/Project/study-rust/ion/src/bin/ion.rs)（在 `Cli` struct 的 global args 区域，约 L173-195 附近）

```rust
/// 在当前 session 内从某条 entry 分叉（移动 leaf 指针）
#[arg(long, global = true)]
branch: Option<String>,

/// 给 --branch 的分支命名（复用 LabelEntry）
#[arg(long, global = true, requires = "branch")]
branch_name: Option<String>,

/// 切换到指定名称的分支
#[arg(long, global = true)]
checkout: Option<String>,

/// 回滚到某条 entry（移动 leaf，被回滚的路径保留）
#[arg(long, global = true)]
rollback: Option<String>,

/// --rollback 时给被废弃路径打 tombstone 标记（纯文本，不调 LLM）
#[arg(long, global = true, requires = "rollback")]
rollback_reason: Option<String>,

/// 从某个 session 的某个 leaf 提取成新 session 文件（记 parentSession）
#[arg(long, global = true, value_name = "SID/ENTRY_ID")]
fork_from_leaf: Option<String>,
```

### 1.2 新增 `session` 子命令组

**文件**：[src/bin/ion.rs](file:///Users/xuyingzhou/Project/study-rust/ion/src/bin/ion.rs) `Commands` enum（约 L242）

当前只有扁平的 `Sessions`（列出全部）。新增 `Session` 子命令组：

```rust
enum Commands {
    // ... 现有 ...
    Sessions,                          // 保留：扁平列表（向后兼容）
    Session(SessionAction),            // 新增：树/分支操作
}

enum SessionAction {
    /// 显示某个 session 的消息树
    Tree { session: String },
    /// 列出某个 session 的所有命名分支
    Branches { session: String },
    /// 显示从 root 到指定 leaf 的路径
    Path { session: String, leaf: String },
}
```

### 1.3 `SessionMeta` 新增字段

**文件**：[src/session_index.rs](file:///Users/xuyingzhou/Project/study-rust/ion/src/session_index.rs#L7-L53)

```rust
pub struct SessionMeta {
    // ... 现有字段 ...
    /// 父 session ID（fork_from_leaf 时设置）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
    /// 当前 leaf entry ID（冗余缓存，避免每次读 JSONL）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_leaf: Option<String>,
    /// 命名分支数（冗余统计）
    #[serde(default)]
    pub branch_count: u32,
}
```

`#[serde(default)]` 保证旧 index 文件向后兼容。

---

## 2. 主流程 / 数据结构

### 2.1 `LeafPointerEntry`（新 entry 类型）

**文件**：[src/session_jsonl.rs](file:///Users/xuyingzhou/Project/study-rust/ion/src/session_jsonl.rs)（在 `BranchSummaryEntry` 附近，约 L151）

对齐 pi 的 `LeafPointerEntry`（`parentId: null`，脱离消息树）：

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LeafPointerEntry {
    #[serde(rename = "type")]
    pub entry_type: String,                    // "leaf_pointer"
    pub id: String,
    pub parent_id: Option<String>,             // 永远 None（脱离树）
    pub timestamp: String,
    pub leaf_id: Option<String>,               // 指向的 entry id；None = 重置到 root
}
```

JSONL 行示例：

```json
{"type":"leaf_pointer","id":"a1b2c3d4","parentId":null,"timestamp":"2026-07-07T10:00:00Z","leafId":"e5f6a7b8"}
```

### 2.2 树节点 + `get_tree()`

**文件**：[src/session_jsonl.rs](file:///Users/xuyingzhou/Project/study-rust/ion/src/session_jsonl.rs)（`SessionFile` impl）

```rust
#[derive(Clone, Debug)]
pub struct SessionTreeNode {
    pub entry: serde_json::Value,
    pub children: Vec<SessionTreeNode>,
    pub label: Option<String>,         // 来自 LabelEntry
}

impl SessionFile {
    /// 从 entries 构建 parentId 树。对齐 pi getTree()。
    pub fn get_tree(&self) -> Vec<SessionTreeNode> {
        // Step 1: 过滤掉 header + leaf_pointer（它们不是树节点）
        let entries: Vec<&Value> = self.entries.iter()
            .filter(|e| {
                e["type"].as_str() != Some("session")
                    && e["type"].as_str() != Some("leaf_pointer")
            })
            .collect();

        // Step 2: 建 label 映射（LabelEntry.targetId -> label）
        let labels: HashMap<String, String> = self.entries.iter()
            .filter_map(|e| {
                if e["type"].as_str() == Some("label") {
                    Some((e["targetId"].as_str()?.to_string(),
                          e["label"].as_str()?.to_string()))
                } else { None })
            })
            .collect();

        // Step 3: 建 id -> node 映射
        let mut node_map: HashMap<String, SessionTreeNode> = HashMap::new();
        for e in &entries {
            let id = e["id"].as_str().unwrap_or("").to_string();
            node_map.insert(id.clone(), SessionTreeNode {
                entry: e.clone(), children: vec![], label: labels.get(&id).cloned(),
            });
        }

        // Step 4: 按 parentId 挂载；找不到 parent 的当 root
        let mut roots = vec![];
        for e in &entries {
            let id = e["id"].as_str().unwrap_or("");
            let parent_id = e["parentId"].and_then(|p| p.as_str());
            match parent_id {
                None | Some(p) if p == id => roots.push(id.to_string()),
                Some(p) => {
                    if let Some(parent) = node_map.get_mut(p) {
                        let child = node_map.get(id).cloned();
                        if let Some(c) = child { parent.children.push(c); }
                    } else {
                        roots.push(id.to_string());  // orphan 当 root
                    }
                }
            }
        }

        // Step 5: 按 timestamp 升序排每个节点的 children
        roots.into_iter()
            .filter_map(|id| node_map.remove(&id))
            .map(|mut n| { sort_children(&mut n); n })
            .collect()
    }
}
```

### 2.3 `branch()` —— 移动 leaf

**文件**：[src/session_jsonl.rs](file:///Users/xuyingzhou/Project/study-rust/ion/src/session_jsonl.rs)

```rust
impl SessionFile {
    /// 移动 leaf 指针到指定 entry。对齐 pi branch()。
    /// 副作用：追加一条 leaf_pointer entry；不修改任何已有 entry。
    pub fn branch(&mut self, branch_from_id: &str) -> Result<()> {
        if !self.entries.iter().any(|e| e["id"].as_str() == Some(branch_from_id)) {
            bail!("Entry {} not found", branch_from_id);
        }
        let lp = LeafPointerEntry {
            entry_type: "leaf_pointer".into(),
            id: generate_id(),
            parent_id: None,
            timestamp: timestamp_iso(),
            leaf_id: Some(branch_from_id.to_string()),
        };
        self.entries.push(serde_json::to_value(&lp)?);
        self.current_leaf = Some(branch_from_id.to_string());
        Ok(())
    }
}
```

### 2.4 `rollback()` —— 回滚（与 branch 同构）

**文件**：[src/session_jsonl.rs](file:///Users/xuyingzhou/Project/study-rust/ion/src/session_jsonl.rs)

回滚 = branch + 可选 tombstone。复用 `BranchSummaryEntry`（ION 已有，`session_jsonl.rs:151`），但 `summary` 是纯文本，**不调 LLM**：

```rust
impl SessionFile {
    /// 回滚：把 leaf 移回 rollback_to，被跳过的路径保留 + 打 tombstone。
    /// 对齐 pi branchWithSummary，但 summary 是用户给定文本而非 LLM 生成。
    pub fn rollback(
        &mut self,
        rollback_to: &str,           // 回滚到这条 entry
        reason: Option<&str>,        // 可选：为什么回滚（纯文本）
    ) -> Result<()> {
        if !self.entries.iter().any(|e| e["id"].as_str() == Some(rollback_to)) {
            bail!("Entry {} not found", rollback_to);
        }

        // 1. 追加 leaf_pointer（和 branch 完全一样）
        let lp = LeafPointerEntry {
            entry_type: "leaf_pointer".into(),
            id: generate_id(),
            parent_id: None,
            timestamp: timestamp_iso(),
            leaf_id: Some(rollback_to.to_string()),
        };
        self.entries.push(serde_json::to_value(&lp)?);

        // 2. 可选：追加 BranchSummaryEntry 作为 tombstone
        //    parentId = 当前 leaf（被回滚的起点），标记"这段被废弃了"
        if let Some(reason_text) = reason {
            let old_leaf = self.current_leaf.clone();
            let bs = serde_json::json!({
                "type": "branch_summary",
                "id": generate_id(),
                "parentId": old_leaf,                    // 指向被废弃路径的起点
                "timestamp": timestamp_iso(),
                "fromId": old_leaf.as_deref().unwrap_or("root"),
                "summary": format!("rollback: {} → {} | {}",
                                   old_leaf.as_deref().unwrap_or("root"),
                                   rollback_to, reason_text),
                "fromHook": false,
            });
            self.entries.push(bs);
        }

        // 3. 重解析 current_leaf（走 §2.5 的统一算法）
        self.current_leaf = Self::resolve_current_leaf(&self.entries);
        Ok(())
    }
}
```

**与 branch 的区别：**

| 维度 | branch | rollback |
|------|--------|----------|
| leaf_pointer | ✅ 追加 | ✅ 追加（相同） |
| BranchSummaryEntry | ❌ 不追加 | ✅ 可选追加（tombstone） |
| 废弃路径 | 隐式（leaf 移走即废弃） | 显式（summary 标记） |
| 语义 | "从 X 试试别的" | "X 之后走错了，回 X" |
| 用户可逆 | ✅ checkout 回去 | ✅ checkout 回去（完全相同） |

---

### 2.5 leaf 恢复算法（load 时）—— 对齐 pi `_buildIndex`

**文件**：[src/session_jsonl.rs](file:///Users/xuyingzhou/Project/study-rust/ion/src/session_jsonl.rs) `SessionFile::load`

> **完全对齐 pi**（`session-manager.ts` L1028-1104）。处理"leaf_pointer 之后又 append 了消息"的复杂场景：找到最后一个 leaf_pointer 后，**重解析**它的 leafId 到"depth 最深且非 parent 的后代"。

**算法（三阶段）：**

```rust
impl SessionFile {
    pub fn load(path: &Path) -> Result<Self> {
        // ... 现有读取逻辑 ...
        let header = /* 第一行 */;
        let entries = /* 其余行 */;

        let current_leaf = Self::resolve_current_leaf(&entries);

        Ok(Self { header, entries, current_leaf })
    }

    /// 对齐 pi _buildIndex Phase B/C
    fn resolve_current_leaf(entries: &[Value]) -> Option<String> {
        // 收集所有 parent_id（谁被别人指向）
        let mut parent_ids: HashSet<&str> = HashSet::new();
        for e in entries {
            if e["type"].as_str() != Some("session")
                && e["type"].as_str() != Some("leaf_pointer") {
                if let Some(p) = e["parentId"].and_then(|p| p.as_str()) {
                    parent_ids.insert(p);
                }
            }
        }
        // id -> entry 索引
        let by_id: HashMap<&str, &Value> = entries.iter()
            .filter_map(|e| e["id"].as_str().map(|id| (id, e))).collect();

        // Phase B: 从后往前找最后一个 leaf_pointer
        let lp_pos = entries.iter().rposition(
            |e| e["type"].as_str() == Some("leaf_pointer")
        );

        let base_id: Option<&str> = match lp_pos {
            // 找到 leaf_pointer
            Some(i) => {
                let lp = &entries[i];
                match lp["leafId"].as_str() {
                    Some(target_id) => {
                        // 在 leaf_pointer 之后的所有非-parent 后代中，找 depth 最深的
                        Self::deepest_descendant_after(
                            entries, target_id, &parent_ids, &by_id, i, false,
                        )
                    }
                    None => {
                        // leafId == null（reset）：在 i 之后找 depth 最深的非-parent entry
                        Self::deepest_descendant_after(
                            entries, "", &parent_ids, &by_id, i, true,
                        )
                    }
                }
            }
            // Phase C: 无 leaf_pointer —— 全局找 depth 最深的非-parent entry
            None => {
                Self::deepest_non_parent(entries, &parent_ids, &by_id)
            }
        };
        base_id.map(|s| s.to_string())
    }

    /// 在 pos 之后找目标的后代（或任意 entry）中 depth 最深且非 parent 的
    fn deepest_descendant_after(
        entries: &[Value], target: &str, parent_ids: &HashSet<&str>,
        by_id: &HashMap<&str, &Value>, pos: usize, allow_any: bool,
    ) -> Option<&str> {
        let mut best: Option<(&str, usize)> = None;  // (id, depth)
        for e in entries.iter().skip(pos + 1) {
            let id = e["id"].as_str()?;
            if e["type"].as_str() == Some("session") { continue; }
            if e["type"].as_str() == Some("leaf_pointer") { continue; }
            if parent_ids.contains(id) { continue; }  // 是别人的 parent，跳过
            let is_descendant = allow_any
                || Self::is_descendant_of(id, target, by_id);
            if is_descendant {
                let depth = Self::entry_depth(id, by_id);
                match best {
                    None => best = Some((id, depth)),
                    Some((_, d)) if depth > d => best = Some((id, depth)),
                    _ => {}
                }
            }
        }
        // 若 pos 之后没找到，回退到 target 本身（若 target 非 parent）
        best.map(|(id, _)| id).or_else(|| {
            if !allow_any && !parent_ids.contains(target) { Some(target) }
            else { None }
        })
    }

    /// 无 leaf_pointer 时：全局找 depth 最深的非-parent entry
    fn deepest_non_parent(
        entries: &[Value], parent_ids: &HashSet<&str>,
        by_id: &HashMap<&str, &Value>,
    ) -> Option<&str> {
        entries.iter()
            .filter(|e| {
                e["type"].as_str() != Some("session")
                    && e["type"].as_str() != Some("leaf_pointer")
            })
            .filter_map(|e| e["id"].as_str())
            .filter(|id| !parent_ids.contains(id))
            .max_by_key(|id| Self::entry_depth(id, by_id))
    }

    /// 沿 parentId 回溯，判断 id 是否是 target 的后代
    fn is_descendant_of(id: &str, target: &str, by_id: &HashMap<&str, &Value>) -> bool {
        let mut cur = Some(id);
        let mut visited = HashSet::new();
        while let Some(cid) = cur {
            if !visited.insert(cid) { return false; }  // 环保护
            if cid == target { return true; }
            cur = by_id.get(cid)
                .and_then(|e| e["parentId"].and_then(|p| p.as_str()));
        }
        false
    }

    /// 沿 parentId 回溯数 depth（带环保护）
    fn entry_depth(id: &str, by_id: &HashMap<&str, &Value>) -> usize {
        let mut depth = 0;
        let mut cur = Some(id);
        let mut visited = HashSet::new();
        while let Some(cid) = cur {
            if !visited.insert(cid) { break; }
            depth += 1;
            cur = by_id.get(cid)
                .and_then(|e| e["parentId"].and_then(|p| p.as_str()));
        }
        depth
    }
}
```

**场景验证：**

| 文件状态 | 算法结果 |
|---------|---------|
| 无 leaf_pointer | 全局最深非-parent entry |
| 末尾 leaf_pointer 指向 msg_004，其后无 append | msg_004（若非 parent）/ 否则回退 |
| leaf_pointer 指向 msg_004，其后 append msg_007/msg_008 | msg_008（最深后代） |
| 多个 leaf_pointer | 只认最后一个（从后往前找） |

### 2.6 append 逻辑改造（核心）

**文件**：[src/bin/ion.rs](file:///Users/xuyingzhou/Project/study-rust/ion/src/bin/ion.rs) `save_session`（L2676-2704）+ 所有 append 调用点

当前 append 用"上一条 entry 的 id"作为 parentId。改造为**用 `current_leaf` 作为 parentId**：

```rust
// 修复前（L2691）：
let mut parent_id = id.to_string();   // 用 session id 当首个 parent

// 修复后：
let mut parent_id = session_file.current_leaf.clone()
    .unwrap_or_else(|| id.to_string());   // 有 leaf 用 leaf，否则用 session id
```

所有追加 message/compaction/agent_change 的位置（约 6 处调用点）都要从 `session_file.current_leaf` 取 parent，而不是"上一条"。

### 2.7 `create_branched_session()` —— 从 leaf 提取新文件

**文件**：[src/session_jsonl.rs](file:///Users/xuyingzhou/Project/study-rust/ion/src/session_jsonl.rs)

对齐 pi `createBranchedSession(leafId, {compact})`：

```rust
impl SessionFile {
    /// 从 root 到 leaf 提取路径，写入新 session 文件。
    /// 新文件 header.parentSession 指向源文件。
    pub fn create_branched_session(
        &self,
        leaf_id: &str,
        new_id: &str,
        compact: bool,
    ) -> Result<PathBuf> {
        // Step 1: 提取 root→leaf 路径（沿 parentId 回溯 + 反转）
        let path = self.get_branch_path(leaf_id)?;

        // Step 2: compaction 截断（可选）—— 从后往前找第一个 compaction，slice
        let path = if compact {
            self.truncate_at_compaction(path)
        } else { path };

        // Step 3: 新 header，parentSession 指向源文件路径
        let header = SessionHeader {
            entry_type: "session".into(), version: 3, id: new_id.into(),
            timestamp: timestamp_iso(), cwd: self.header.cwd.clone(),
            parent_session: Some(self.source_path.clone()),  // ← 关键
        };

        // Step 4: 写新文件（header + path entries）
        let new_path = session_jsonl_path(&header.cwd, new_id);
        SessionFile::save(&header.cwd, &header, &path);
        Ok(new_path)
    }

    /// 沿 parentId 回溯到 root，返回 root→leaf 顺序的 entries
    fn get_branch_path(&self, leaf_id: &str) -> Result<Vec<Value>> {
        let by_id: HashMap<&str, &Value> = self.entries.iter()
            .filter_map(|e| e["id"].as_str().map(|id| (id, e))).collect();
        let mut path = vec![];
        let mut cur = Some(leaf_id);
        while let Some(id) = cur {
            if let Some(e) = by_id.get(id) {
                path.push(e.clone());
                cur = e["parentId"].and_then(|p| p.as_str());
            } else { break; }
        }
        path.reverse();
        Ok(path)
    }
}
```

### 2.8 关键决策点

| 场景 | 处理 | 理由 |
|------|------|------|
| `--branch` 指向不存在的 entry | 报错退出，不创建 leaf_pointer | 防止脏数据 |
| `--branch` 点在 `CompactionEntry` 之前 | 拒绝 + 提示用 `--fork-from-leaf --compact` | 分支穿越压缩点会丢上下文 |
| `--checkout` 找不到分支名 | 报错列出可用分支名 | 引导用户 |
| ID 冲突 | `generate_id()` 加 collision 检查（重试 100 次） | 现有 `generate_id` 用纳秒时间戳，高并发可能撞 |
| 同一 entry 被多次 branch | 允许多个 leaf_pointer 指向同一 entry | 这是树的自然形态 |
| `--rollback` 不带 reason | 只追加 leaf_pointer，不打 tombstone | 等同于 branch，幂等 |
| `--rollback` 带 reason | leaf_pointer + BranchSummaryEntry（纯文本） | tombstone 便于 UI 区分废弃路径 |
| `--continue` 恢复 | 恢复 `last_session` 的 `current_leaf`（不是文件末尾） | 用户上次显式选的分支要保留 |
| fork 复制时重生 ID | **不重生**——保留原 entry 的 id 和 parentId | 重生会破坏 parent 链；新文件用新 session id 区分即可 |

---

## 3. 接口规格

### 3.1 `--branch` CLI

**请求：**

```bash
ion --resume <sid> --branch <entry-id> "从这条消息换个思路"
# 或带命名：
ion --resume <sid> --branch <entry-id> --name try-async "用 async 重写"
```

**行为：**
1. 加载 session，验证 entry-id 存在
2. 调用 `on_session_before_fork(entry_id)` 钩子（可取消）
3. 追加 `LeafPointerEntry{ leafId: entry-id }`
4. 若有 `--name`，追加 `LabelEntry{ targetId: entry-id, label: name }`
5. 后续 prompt 的 parentId 接 leaf（即 entry-id），形成新枝
6. 调用 `on_session_tree(new_leaf_id)` 钩子

**输出（成功）：**

```
[branch] moved leaf to e5f6a7b8
[branch] labeled: try-async → e5f6a7b8
```

### 3.2 `--checkout` CLI

**请求：**

```bash
ion --resume <sid> --checkout try-async "继续这条分支"
```

**行为：**
1. 找到 `label == "try-async"` 的 LabelEntry，取其 targetId
2. 追加 `LeafPointerEntry{ leafId: targetId }`
3. 后续消息接在 targetId 之后

### 3.3 `--rollback` CLI

**请求：**

```bash
# 基础回滚（只移 leaf，路径保留）
ion --resume <sid> --rollback <entry-id> "回滚后继续"

# 带 tombstone（记录为什么放弃当前路径）
ion --resume <sid> --rollback <entry-id> \
    --rollback-reason "方案走错了，回到分叉点" "回滚后继续"
```

**行为（对齐 §0.2）：**
1. 验证 entry-id 存在
2. 追加 `LeafPointerEntry{ leafId: entry-id }`
3. 若有 `--rollback-reason`：追加 `BranchSummaryEntry{ parentId: <旧 leaf>, summary: "rollback: ... | <reason>" }`
4. 重解析 current_leaf
5. 后续 prompt 接在新 leaf

**输出（成功）：**

```
[rollback] moved leaf to msg_005
[rollback] tombstone: msg_010 → msg_005 | 方案走错了，回到分叉点
[rollback] abandoned path (msg_005..msg_010) preserved in file
```

**关键属性：**
- ✅ 被回滚的路径（msg_005..msg_010）仍在文件里，`session tree` 可见
- ✅ 可用 `--checkout <name>` 或 `--rollback <msg_010>` 恢复（回滚本身可逆）
- ✅ tombstone 是纯文本，无 LLM 调用

### 3.4 `session tree` 子命令

**请求：**

```bash
ion session tree <sid>
```

**输出（ASCII 树）：**

```
Session: a3f4b5c6
cwd: /Users/xuyingzhou/Project/study-rust/ion

msg_001 [user] "实现 calc 函数"
└─ msg_002 [assistant] "我来用同步方式..."
   ├─ msg_003 [user] "加上错误处理"
   │  └─ msg_004 [assistant] "已加 try/catch..."
   │     └─ msg_005 [user] "换个思路"  ← try-async (e5f6a7b8) [当前 leaf]
   │        └─ msg_006 [assistant] "用 async 重写..."
   └─ msg_007 [user] "测试覆盖率"  ← test-branch (c8d9e0f1)

命名分支:
  try-async   → e5f6a7b8 [当前]
  test-branch → c8d9e0f1
```

### 3.5 `session branches` 子命令

**请求：**

```bash
ion session branches <sid>
```

**输出：**

```
NAME          TARGET        CREATED                   CURRENT
try-async     e5f6a7b8      2026-07-07 10:00:00       *
test-branch   c8d9e0f1      2026-07-07 10:05:00
```

### 3.6 `--fork-from-leaf` CLI

**请求：**

```bash
ion --fork-from-leaf <sid>/<entry-id> "在新 session 里继续这条分支"
# 带 compaction 截断：
ion --fork-from-leaf <sid>/<entry-id> --compact "..."
```

**行为：**
1. 加载源 session，提取 root→entry-id 路径
2. 生成新 session UUID
3. 写新 `.jsonl` 文件：header.parentSession = 源文件路径
4. 复制 path entries（**保留原 id 和 parentId**，不重生）
5. 更新 `SessionMeta.parent_session`

**输出：**

```
[fork-from-leaf] new session: b7c8d9e0
[fork-from-leaf] parent: a3f4b5c6
[fork-from-leaf] path: 4 entries (compact: no)
```

### 3.7 `branch_session` Agent 工具（含 rollback）

**文件**：[src/agent/tool.rs](file:///Users/xuyingzhou/Project/study-rust/ion/src/agent/tool.rs)

```json
{
  "name": "branch_session",
  "description": "在当前会话内从某条消息分叉/回滚，探索不同方案。原路径完整保留（only-append）。",
  "parameters": {
    "type": "object",
    "properties": {
      "from_entry": { "type": "string", "description": "从哪条 entry 分叉或回滚到（默认当前 leaf）" },
      "name": { "type": "string", "description": "分支名（可选）" },
      "is_rollback": { "type": "boolean", "description": "true=回滚语义（打 tombstone），false=分叉语义。默认 false" },
      "reason": { "type": "string", "description": "回滚原因（仅 is_rollback=true 时生效，纯文本）" },
      "prompt": { "type": "string", "description": "新分支的初始指令" }
    },
    "required": ["prompt"]
  }
}
```

**返回：**

```json
{"new_leaf": "e5f6a7b8", "branch_name": "try-async", "abandoned_path": ["msg_003", "msg_004"]}
```

### 3.8 扩展钩子激活

**文件**：[src/agent/extension.rs](file:///Users/xuyingzhou/Project/study-rust/ion/src/agent/extension.rs#L134-L142)

现有 stub 签名（不变）：

```rust
async fn on_session_before_fork(&self, _entry_id: &str) -> AgentResult<()> { Ok(()) }
async fn on_session_tree(&self, _leaf_id: &str) -> AgentResult<()> { Ok(()) }
```

在 `--branch` / `branch_session` 执行路径中调用（默认 no-op，扩展可覆盖）。

---

## 4. CLI 测试指南

> **所有测试通过 `SessionTreeHarness` 执行**（统一入口、不调真实 LLM、环境隔离）。
> Harness 的设计、接口、P0/P1/XFail 验收用例见独立的验收规格：[docs/testing/SESSION_TREE_SPEC.md](../testing/SESSION_TREE_SPEC.md)。
>
> 本节的 Group A/B/C/... 是**开发者视角**的测试组织（按模块分组），对应验收规格里的 P0/P1 用例。两者覆盖同一批能力，只是视角不同：
> - 验收规格（给 QA）：harness 接口 + P0/P1 分级 + 红线判定
> - 本节（给开发）：Group 分组 + 实现细节 + Rust 测试函数名

### Harness 实现要点

**文件**：`tests/session_tree_harness.rs`（新建）

```rust
/// Session Tree 统一测试 harness
/// 对齐 agent-test-harness skill 的 create→configure→execute→observe→cleanup 模式
pub struct SessionTreeHarness {
    pub session_id: String,
    pub file_path: PathBuf,
    pub session_file: SessionFile,       // 内存中的会话对象
    event_rx: mpsc::Receiver<Value>,     // 扩展事件收集
    _tmp_dir: TempDir,                   // 析构时自动清理
}

impl SessionTreeHarness {
    /// 1. create：临时目录 + 空会话
    pub fn create() -> Self { /* tempfile + SessionFile::new */ }

    /// 从已有文件加载（模拟重启）
    pub fn load_from(path: &Path) -> Self { /* SessionFile::load */ }

    /// 2. configure/seed：塞预设消息（不调 LLM）
    pub fn seed(&mut self, msgs: &[(Role, &str)]) { /* message_to_entry + append */ }

    /// 3. execute：分支操作
    pub fn branch(&mut self, entry_id: &str, name: Option<&str>) -> Result<()>
    pub fn checkout(&mut self, name: &str) -> Result<()>
    pub fn rollback(&mut self, to: &str, reason: Option<&str>) -> Result<()>
    pub fn fork_from_leaf(&self, leaf_id: &str) -> Result<SessionTreeHarness>

    /// 4. observe：查询
    pub fn tree(&self) -> Vec<SessionTreeNode>      // get_tree()
    pub fn branches(&self) -> Vec<BranchInfo>
    pub fn current_leaf(&self) -> Option<String>
    pub fn messages_on_path(&self) -> Vec<Value>     // root→leaf 路径
    pub fn snapshot(&self) -> FileSnapshot           // {sha256, line_count, msg_count}
    pub fn snapshot_prefix(&self, n: usize) -> String // 前 n 行的 sha256
    pub fn entries_by_type(&self, t: &str) -> Vec<Value>
    pub fn events(&self) -> Vec<Value>

    /// 5. cleanup：自动（TempDir 析构）
}
```

**Harness 设计原则：**
- **绕过 LLM**：`seed()` 直接构造 entry 写文件，不经过 agent loop（对齐 `compaction_e2e.rs` / `memory_e2e.rs` 的惯例）
- **环境隔离**：`tempfile::TempDir`（ION 现有测试用 `std::env::temp_dir()` 手动管理，harness 用 `tempfile` crate 更安全）
- **不走 CLI**：直接调 `SessionFile` 的方法，不 spawn `ion` 进程（CLI 层在 Group H bash 脚本里单独验证）

### Group A：基础分支（leaf 移动 + 树查看）

> 验证：`--branch` 移动 leaf、`session tree` 正确展示树结构、append 接 leaf 而非末尾。

#### A1 单次 branch + 树查看

**前置：** 已有一个 3 轮对话的 session。

```bash
# 1. 准备：创建一个 session 并跑 3 轮
cd /tmp/ion-tree-test
ion "实现一个加法函数 add(a,b)" 
# → 记下输出的 session id，假设 a3f4b5c6
ion --resume a3f4b5c6 "加上日志"
ion --resume a3f4b5c6 "改成乘法"

# 2. 查看当前树（线性）
ion session tree a3f4b5c6
```

**预期（branch 前，线性）：**

```
Session: a3f4b5c6
cwd: /tmp/ion-tree-test

msg_001 [user] "实现一个加法函数 add(a,b)"
└─ msg_002 [assistant] "fn add(a, b) { a + b }"
   └─ msg_003 [user] "加上日志"
      └─ msg_004 [assistant] "已加 println!..."
         └─ msg_005 [user] "改成乘法"
            └─ msg_006 [assistant] "fn mul(a, b) { a * b }"
```

```bash
# 3. 从 msg_004 分叉（绕过"改成乘法"那轮）
ion --resume a3f4b5c6 --branch msg_004 "加上除法"

# 4. 再看树
ion session tree a3f4b5c6
```

**预期（branch 后，分叉）：**

```
Session: a3f4b5c6
cwd: /tmp/ion-tree-test

msg_001 [user] "实现一个加法函数 add(a,b)"
└─ msg_002 [assistant] "fn add(a, b) { a + b }"
   └─ msg_003 [user] "加上日志"
      └─ msg_004 [assistant] "已加 println!..."
         ├─ msg_005 [user] "改成乘法"        ← 旧路径（保留）
         │  └─ msg_006 [assistant] "fn mul..."
         └─ msg_007 [user] "加上除法"        ← 新分支 [当前 leaf]
            └─ msg_008 [assistant] "fn div..."
```

**验证点：**
- ✅ msg_005/006（旧路径）仍在文件里（没被删）
- ✅ msg_007 的 parentId == msg_004（不是 msg_006）
- ✅ 当前 leaf 指向 msg_008

**文件验证（直接看 JSONL）：**

```bash
cat ~/.ion/agent/sessions/*/a3f4b5c6.jsonl | grep leaf_pointer
```

**预期：**

```json
{"type":"leaf_pointer","id":"x1y2z3w4","parentId":null,"timestamp":"...","leafId":"msg_004"}
```

#### A2 多次 branch 形成多分支

```bash
# 在 A1 基础上，再从 msg_002 分叉
ion --resume a3f4b5c6 --branch msg_002 "用泛型重写"

ion session tree a3f4b5c6
```

**预期：**

```
msg_001
└─ msg_002
   ├─ msg_003 → msg_004 → ├─ msg_005 → msg_006 (旧)
   │                       └─ msg_007 → msg_008 (div)
   └─ msg_009 "用泛型重写" → msg_010 [当前 leaf]
```

**验证点：**
- ✅ 树有 3 个叶子（msg_006 / msg_008 / msg_010）
- ✅ 当前 leaf == msg_010
- ✅ 文件里有 2 条 leaf_pointer

---

### Group B：分支命名 + 切换

> 验证：`--name` 给分支打标签、`--checkout` 切换、`session branches` 列表。

#### B1 命名分支

```bash
# 前置：A2 的多分支 session
# 给 msg_007 那条分支命名
ion --resume a3f4b5c6 --branch msg_007 --name div-branch "继续除法优化"

ion session branches a3f4b5c6
```

**预期：**

```
NAME          TARGET        CREATED                   CURRENT
div-branch    msg_007       2026-07-07 10:30:00       *
```

**验证点：**
- ✅ 文件里有对应 LabelEntry：`{"type":"label","targetId":"msg_007","label":"div-branch"}`
- ✅ `branches` 列表显示该分支

#### B2 checkout 切换

```bash
# 切到另一条分支（假设 A2 里 msg_009 那条没命名，先命名）
ion --resume a3f4b5c6 --branch msg_009 --name generic-branch "命名泛型分支"
ion --resume a3f4b5c6 --checkout div-branch "回到除法分支"

ion session branches a3f4b5c6
```

**预期：**

```
NAME            TARGET        CREATED                   CURRENT
div-branch      msg_007       2026-07-07 10:30:00       *
generic-branch  msg_009       2026-07-07 10:35:00
```

**验证点：**
- ✅ checkout 后追加了新 leaf_pointer 指向 msg_007
- ✅ 后续 prompt 的 parentId == msg_007
- ✅ `branches` 的 CURRENT 列正确标记 div-branch

#### B3 checkout 不存在的分支名

```bash
ion --resume a3f4b5c6 --checkout nonexist "..."
```

**预期（失败）：**

```
[error] branch 'nonexist' not found. Available: div-branch, generic-branch
```

**验证点：**
- ✅ 不写入任何 entry
- ✅ 错误信息列出可用分支

#### B4 回滚（基础）—— 验证 only-append

```bash
# 前置：A1 的多分支 session a3f4b5c6
# 当前 leaf 假设在 msg_008（div 分支末尾）
# 先记录回滚前的文件行数
wc -l ~/.ion/agent/sessions/*/a3f4b5c6.jsonl   # 假设 8 行

# 回滚到 msg_004
ion --resume a3f4b5c6 --rollback msg_004 "回到加日志之后重新来"

# 回滚后再看行数
wc -l ~/.ion/agent/sessions/*/a3f4b5c6.jsonl   # 应为 9 或 10 行（多了 leaf_pointer）
```

**预期（文件只增不减）：**

```bash
# 文件行数：8 → 9（+1 leaf_pointer）
# 关键：msg_005..msg_008 仍在文件里，未被删除
cat ~/.ion/agent/sessions/*/a3f4b5c6.jsonl | grep -c '"type":"message"'
# → 仍为 6（msg_001-006 + msg_007-008），一条没少
```

**验证点：**
- ✅ 文件行数只增不减（only-append 不变量）
- ✅ `leaf_pointer` 指向 msg_004
- ✅ msg_005/006/007/008（被回滚路径）仍在文件里
- ✅ current_leaf 重解析为 msg_004
- ✅ 后续 prompt 接在 msg_004 后

#### B5 回滚带 tombstone

```bash
# 前置：B4 之后，session 又跑了几轮，当前 leaf 在 msg_012
ion --resume a3f4b5c6 --rollback msg_008 \
    --rollback-reason "异步方案性能不达标，回同步" "继续优化同步版"
```

**预期（文件追加 2 条 entry）：**

```bash
# 1. leaf_pointer 指向 msg_008
# 2. branch_summary 作为 tombstone
tail -2 ~/.ion/agent/sessions/*/a3f4b5c6.jsonl | python3 -m json.tool
```

```json
{"type":"leaf_pointer","id":"...","parentId":null,"timestamp":"...","leafId":"msg_008"}
{"type":"branch_summary","id":"...","parentId":"msg_012","timestamp":"...","fromId":"msg_012","summary":"rollback: msg_012 → msg_008 | 异步方案性能不达标，回同步","fromHook":false}
```

**验证点：**
- ✅ tombstone 的 `parentId` 指向被废弃路径起点（msg_012）
- ✅ `summary` 是纯文本，无 LLM 调用痕迹
- ✅ `fromHook: false`（用户/Agent 触发，非扩展）
- ✅ msg_009..msg_012 仍在文件里

#### B6 回滚的可逆性（回滚的回滚）

```bash
# 前置：B5 回滚到 msg_008
# 现在"回滚这次回滚"——checkout 回 msg_012
ion --resume a3f4b5c6 --branch msg_012 "恢复异步方案继续"

ion session tree a3f4b5c6
```

**预期：** current_leaf 重解析为 msg_012，异步分支恢复"活跃"

**验证点：**
- ✅ 回滚是 append，回滚的回滚也是 append
- ✅ 文件里现在有 3 条 leaf_pointer（B4 + B5 + 本次的 branch）
- ✅ 没有任何 entry 被删除

---

### Group C：从分支点 fork（跨文件）

> 验证：`--fork-from-leaf` 提取路径成新 session、`parentSession` 正确记录、原 entry id 保留。

#### C1 基础 fork-from-leaf

```bash
# 前置：A2 的多分支 session（a3f4b5c6）
# 从 msg_004 那个分叉点提取成新文件
ion --fork-from-leaf a3f4b5c6/msg_004 "在新 session 继续"
# → 输出新 session id，假设 b7c8d9e0

# 验证新 session 的 header
head -1 ~/.ion/agent/sessions/*/b7c8d9e0.jsonl | python3 -m json.tool
```

**预期（新 session header）：**

```json
{
  "type": "session",
  "version": 3,
  "id": "b7c8d9e0",
  "timestamp": "2026-07-07T11:00:00Z",
  "cwd": "/tmp/ion-tree-test",
  "parentSession": "/Users/xuyingzhou/.ion/agent/sessions/--hash--/a3f4b5c6.jsonl"
}
```

**验证点：**
- ✅ `parentSession` 指向源文件绝对路径
- ✅ 新文件只含 root→msg_004 的 entries（4 条：msg_001/002/003/004）
- ✅ entry 的 id 和 parentId 与源文件一致（未重生）

```bash
# 验证 SessionMeta
ion sessions | grep b7c8d9e0
```

**预期：** index 里 b7c8d9e0 的 parent_session 字段 == a3f4b5c6

#### C2 fork-from-leaf 带 compaction 截断

```bash
# 前置：一个已经触发过 compaction 的长 session（>50 轮）
# 假设 session c1d2e3f4，msg_030 是 CompactionEntry
ion --fork-from-leaf c1d2e3f4/msg_045 --compact "..."
```

**预期：**
- ✅ 新文件只含 msg_030（compaction）到 msg_045 的 entries
- ✅ msg_001-029 被截断（因为在最近 compaction 之前）

---

### Group D：Agent 工具 + 扩展钩子

> 验证：Agent 自主调用 `branch_session`、扩展钩子被触发。

#### D1 Agent 调用 branch_session

**前置：** host 模式（场景 2），`ion --host`。

```bash
# Terminal 1：启动 host + 订阅
ion --host --agent developer "实现一个排序算法，如果快排不行就换归并"

# Terminal 2：订阅事件
ion subscribe --session <host-sid>
```

**预期事件流（Terminal 2）：**

```json
{"type":"event","event":{"type":"tool_call","tool":"branch_session","params":{"from_entry":"msg_004","name":"merge-sort","prompt":"尝试归并排序"}}}
{"type":"event","event":{"type":"tool_result","tool":"branch_session","result":{"new_leaf":"msg_006","branch_name":"merge-sort","abandoned_path":["msg_005"]}}}
```

**验证点：**
- ✅ Agent 自主决定分叉（无需用户干预）
- ✅ session tree 显示两条分支
- ✅ Agent 在新分支上继续工作

#### D2 扩展钩子触发

**前置：** 安装一个测试用 WASM 扩展，覆盖 `on_session_before_fork` 和 `on_session_tree`。

```bash
# 触发 branch
ion --resume <sid> --branch msg_004 "..."
```

**预期（扩展收到钩子）：**

```bash
ion subscribe --extension <ext-name>
```

```json
{"type":"event","event":{"type":"extension_event","extension":"test-hook","data":{"hook":"on_session_before_fork","entry_id":"msg_004"}}}
{"type":"event","event":{"type":"extension_event","extension":"test-hook","data":{"hook":"on_session_tree","leaf_id":"msg_006"}}}
```

**验证点：**
- ✅ `on_session_before_fork` 在 branch 前触发，参数是 entry_id
- ✅ `on_session_tree` 在 branch 后触发，参数是新 leaf_id
- ✅ 若扩展返回 Err，branch 被取消

---

### Group E：边界 + 错误用例（XFail）

> 验证：错误输入被正确拒绝，不产生脏数据。

#### E1 branch 不存在的 entry

```bash
ion --resume <sid> --branch nonexist_entry "..."
```

**预期（失败）：**

```
[error] Entry nonexist_entry not found in session <sid>
```

**验证点：**
- ✅ 无 leaf_pointer 写入
- ✅ session 文件无变化

#### E2 branch 点在 compaction 之前

```bash
# 前置：session 有 CompactionEntry at msg_030
ion --resume <sid> --branch msg_010 "..."
```

**预期（失败）：**

```
[error] Cannot branch at msg_010: it is before a compaction point (msg_030).
        Branching across compaction loses summarized context.
        Hint: use `ion --fork-from-leaf <sid>/msg_010 --compact` instead.
```

**验证点：**
- ✅ 拒绝执行
- ✅ 提示用户用 fork-from-leaf --compact

#### E3 fork-from-leaf 路径不存在

```bash
ion --fork-from-leaf <sid>/nonexist_leaf "..."
```

**预期（失败）：**

```
[error] Leaf nonexist_leaf not found in session <sid>
```

#### E4 checkout 时 session 无任何分支

```bash
# 前置：全新 session，无 LabelEntry
ion --resume <sid> --checkout anyname "..."
```

**预期（失败）：**

```
[error] No named branches in session <sid>. Use `ion --branch <id> --name <name>` to create one.
```

---

### Group F：单元测试 + 集成测试

#### F1 单元测试

```bash
cargo test --lib session_jsonl
cargo test --lib session_index
```

**预期覆盖：**

| 测试 | 验证点 |
|------|--------|
| `test_leaf_pointer_serialize` | LeafPointerEntry JSON 格式正确 |
| `test_get_tree_linear` | 线性链返回单根单链 |
| `test_get_tree_branch` | 分叉返回多叶子 |
| `test_get_tree_orphan_as_root` | parentId 指向不存在 entry 时当 root |
| `test_branch_moves_leaf` | branch 后 current_leaf 更新 |
| `test_branch_nonexistent_fails` | branch 不存在 entry 返回 Err |
| `test_rollback_appends_only` | rollback 后文件行数只增不减 |
| `test_rollback_with_reason_adds_tombstone` | rollback+reason 追加 branch_summary |
| `test_rollback_reversible` | rollback 后再 branch 回去，无数据丢失 |
| `test_load_restores_leaf` | load 后 current_leaf 走 pi 重解析算法 |
| `test_load_no_leaf_pointer` | 无 leaf_pointer 时 current_leaf == 最后一条 entry |
| `test_get_branch_path` | root→leaf 路径正确 |
| `test_truncate_at_compaction` | compaction 截断正确 |

#### F2 集成测试

```bash
cargo test --test session_tree -- --nocapture
```

**预期覆盖：**

| 测试 | 验证点 |
|------|--------|
| `branch_then_append_parents_off_leaf` | branch 后新消息 parentId == branch 点 |
| `old_path_preserved_after_branch` | branch 后旧 entries 仍在文件 |
| `named_branch_visible_in_branches_cmd` | --name 后 branches 列表可见 |
| `checkout_switches_leaf` | checkout 后 current_leaf 正确 |
| `fork_from_leaf_records_parent` | 新 session header.parentSession 正确 |
| `fork_from_leaf_preserves_entry_ids` | 新文件 entry id 与源一致 |
| `compaction_safety_check_rejects` | branch 跨 compaction 被拒 |

---

### Group G：only-append 不变量审计

> **这是本功能最重要的 Group。** 验证任何操作都不破坏 only-append 不变量——文件只增不减、entry 不改不删。

#### G1 全操作序列后文件完整性

```bash
# 前置：全新 session
cd /tmp/ion-tree-test
ion "step 1"   # → sid = test_only_append
ion --resume test_only_append "step 2"
ion --resume test_only_append "step 3"

# 记录初始状态
INITIAL_HASH=$(sha256sum ~/.ion/agent/sessions/*/test_only_append.jsonl | awk '{print $1}')
INITIAL_LINES=$(wc -l < ~/.ion/agent/sessions/*/test_only_append.jsonl)
INITIAL_MSG_COUNT=$(grep -c '"type":"message"' ~/.ion/agent/sessions/*/test_only_append.jsonl)

# 执行一系列操作
ENTRY_2=$(grep '"type":"message"' ~/.ion/agent/sessions/*/test_only_append.jsonl | sed -n '2p' | python3 -c "import json,sys;print(json.load(sys.stdin)['id'])")
ion --resume test_only_append --branch $ENTRY_2 --name branch-A "branch A step"
ion --resume test_only_append --checkout branch-A "继续 A"
ion --resume test_only_append --rollback $ENTRY_2 --rollback-reason "A 走错了" "回滚重来"
ion --resume test_only_append --branch $ENTRY_2 --name branch-B "branch B"

# 审计
FINAL_LINES=$(wc -l < ~/.ion/agent/sessions/*/test_only_append.jsonl)
FINAL_MSG_COUNT=$(grep -c '"type":"message"' ~/.ion/agent/sessions/*/test_only_append.jsonl)
```

**预期（不变量）：**

| 不变量 | 检查 | 预期 |
|--------|------|------|
| 文件只增不减 | `FINAL_LINES > INITIAL_LINES` | ✅ |
| 原始消息零丢失 | `FINAL_MSG_COUNT >= INITIAL_MSG_COUNT` | ✅（新增的 ≥ 原 3 条） |
| 原始 entry 内容未改 | 对比前 N 条 entry 的 sha256 | ✅ 与 INITIAL 一致 |

**验证命令（原始 entry 完整性）：**

```bash
# 取前 INITIAL_LINES 行（原始内容），计算 hash，应与操作前一致
head -n $INITIAL_LINES ~/.ion/agent/sessions/*/test_only_append.jsonl | sha256sum
# → 必须等于 $INITIAL_HASH
```

**验证点：**
- ✅ 前 `INITIAL_LINES` 行的 sha256 完全不变（证明旧 entry 一字未改）
- ✅ 新增的全是 append（leaf_pointer / label / branch_summary / message）
- ✅ 没有任何 entry 被删除或修改

#### G2 fork-from-leaf 不修改源文件

```bash
# 前置：G1 的 session（已有多个分支）
SOURCE_HASH=$(sha256sum ~/.ion/agent/sessions/*/test_only_append.jsonl | awk '{print $1}')

# 从某个 leaf fork 出新 session
ion --fork-from-leaf test_only_append/$ENTRY_2 "fork 出来继续"

# 源文件 hash 应完全不变
sha256sum ~/.ion/agent/sessions/*/test_only_append.jsonl | awk '{print $1}'
```

**预期：** 与 `SOURCE_HASH` 完全相等

**验证点：**
- ✅ fork 只写新文件，源文件 0 字节修改
- ✅ 源 session 的所有分支、tombstone 完好

#### G3 回滚不丢消息（数据完整性）

```bash
# 前置：一个 5 轮对话的 session（msg_001..msg_010）
BEFORE=$(grep -c '"type":"message"' ~/.ion/agent/sessions/*/<sid>.jsonl)

# 回滚到 msg_002（丢弃 msg_003..msg_010）
ion --resume <sid> --rollback msg_002 --rollback-reason "全部重来"

AFTER=$(grep -c '"type":"message"' ~/.ion/agent/sessions/*/<sid>.jsonl)
```

**预期：** `AFTER == BEFORE`（一条消息都没少）

**验证点：**
- ✅ 回滚后消息总数不变
- ✅ msg_003..msg_010 仍在文件里（grep 能找到）
- ✅ current_leaf 重解析为 msg_002，但物理数据零丢失

---

### Group H：e2e 真实 API（可选）

> 标记为 `#[ignore]`，需显式启用。

```bash
ION_E2E_REAL=1 ION_API_KEY="sk-xxx" \
cargo test --test session_tree_e2e -- --ignored --nocapture
```

**预期覆盖：**

| 测试 | 验证点 |
|------|--------|
| `real_branch_explores_alternative` | 真实 LLM 在分支上产出不同方案 |
| `real_agent_branch_session_tool` | Agent 自主调用 branch_session |

---

## 5. 实现顺序（建议）

| Phase | 内容 | 预估 |
|-------|------|------|
| 0 | **SessionTreeHarness 搭建**（tempfile + seed + 查询方法 + TempDir 清理） | 0.5 天 |
| 1 | `LeafPointerEntry` + `get_tree()` + leaf 恢复(pi 重解析) + 单元测试（F1） | 1.5 天 |
| 2 | `--branch` CLI + append 改造 + Group A | 1 天 |
| 3 | `--name` + `--checkout` + `--rollback` + `session branches` + Group B | 1 天 |
| 4 | `session tree` 子命令 + Group A 树展示 | 0.5 天 |
| 5 | `create_branched_session` + `--fork-from-leaf` + Group C | 1 天 |
| 6 | `branch_session` 工具(含 rollback) + 扩展钩子激活 + Group D | 1 天 |
| 7 | compaction 安全检查 + Group E + 集成测试 F2 | 0.5 天 |
| 8 | only-append 不变量审计测试 + Group G | 0.5 天 |
| **合计** | | **~7.5 天** |

---

## 6. 后续工作

| # | 待办 | 优先级 |
|---|------|--------|
| 1 | branch_summary 的 LLM 生成版（当前仅纯文本 tombstone） | P2 |
| 2 | 树展示的 UI 优化（颜色、折叠、被回滚路径灰显） | P3 |
| 3 | 与 worktree 联动（branch/rollback 时可选创建 worktree） | P3 |
| 4 | 跨文件 session 血统（任意 fork 都记 parentSession，不止 fork-from-leaf） | P3 |
| 5 | session tree 导出（dot/mermaid 格式） | P4 |
| 6 | GC 策略（被多轮回滚的深度路径何时归档） | P4 |
