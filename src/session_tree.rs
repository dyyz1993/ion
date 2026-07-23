//! Session Tree — 会话内分支树的核心数据层。
//!
//! 纯函数操作 `&[serde_json::Value]`（session entries），不涉及 IO。
//! 所有"分支/回滚/切换"操作返回要追加的新 entries（调用方负责写盘），
//! 严格遵守 only-append 不变量：永不修改/删除已有 entry。
//!
//! 对齐 pi `session-manager.ts` 的 getTree / branch / leaf 恢复算法。
//! 设计文档：docs/design/SESSION_TREE.md

use serde_json::Value;
use std::collections::{HashMap, HashSet};

/// 树节点：一个 entry + 它的子节点。
#[derive(Clone, Debug, serde::Serialize)]
pub struct TreeNode {
    pub entry: Value,
    pub children: Vec<TreeNode>,
    /// 命名分支（来自 LabelEntry），若有。
    pub label: Option<String>,
}

/// 判断一个 entry 是否是 header（session 行）。
fn is_header(e: &Value) -> bool {
    e.get("type").and_then(|v| v.as_str()) == Some("session")
}

/// 判断一个 entry 是否是 leaf_pointer。
fn is_leaf_pointer(e: &Value) -> bool {
    e.get("type").and_then(|v| v.as_str()) == Some("leaf_pointer")
}

/// 取 entry 的 id。
fn entry_id(e: &Value) -> Option<&str> {
    e.get("id").and_then(|v| v.as_str())
}

/// 取 entry 的 parentId。
fn entry_parent_id(e: &Value) -> Option<&str> {
    e.get("parentId").and_then(|v| v.as_str())
}

/// 取 leaf_pointer 的 leafId（指向的真实 entry）。
fn leaf_pointer_target(e: &Value) -> Option<&str> {
    e.get("leafId").and_then(|v| v.as_str())
}

/// 沿 parentId 链回溯，判断 `id` 是否是 `target` 的后代（含自身）。
/// 带环保护。
fn is_descendant_of(id: &str, target: &str, by_id: &HashMap<&str, &Value>) -> bool {
    let mut cur = Some(id);
    let mut visited = HashSet::new();
    while let Some(cid) = cur {
        if !visited.insert(cid) {
            return false; // 环保护
        }
        if cid == target {
            return true;
        }
        cur = by_id.get(cid).and_then(|e| entry_parent_id(e));
    }
    false
}

/// 批量预计算所有 entry 的 depth（单趟 DP，O(n) 总体）。
/// 返回 id → depth 的 HashMap，后续查询 O(1)。
/// 替代逐个调 entry_depth（最坏 O(n²)）。
fn compute_depths<'a>(by_id: &HashMap<&'a str, &'a Value>) -> HashMap<&'a str, usize> {
    let mut depths: HashMap<&str, usize> = HashMap::with_capacity(by_id.len());
    for &id in by_id.keys() {
        if depths.contains_key(id) {
            continue; // 已算过
        }
        // 沿 parentId 链走，记录路径
        let mut path: Vec<&str> = Vec::new();
        let mut cur = Some(id);
        let mut visited = HashSet::new();
        while let Some(cid) = cur {
            if !visited.insert(cid) {
                break; // 环保护
            }
            if let Some(&d) = depths.get(cid) {
                // 链上某点已算过，回填路径
                for (i, p) in path.iter().rev().enumerate() {
                    depths.insert(p, d + i + 1);
                }
                path.clear();
                break;
            }
            path.push(cid);
            cur = by_id.get(cid).and_then(|e| entry_parent_id(e));
        }
        // 如果没遇到已算的点，整条链从 root(depth=0) 开始回填
        if !path.is_empty() {
            for (i, p) in path.iter().rev().enumerate() {
                depths.insert(p, i + 1);
            }
        }
    }
    depths
}

/// 解析当前 leaf（光标位置）。
///
/// 对齐 pi `_buildIndex` Phase B/C：
/// 1. 从后往前找最后一个 leaf_pointer
/// 2. 若找到且 leafId 非空：在其后的后代中找 depth 最深的非-parent entry
/// 3. 若 leafId 为空（reset）：在其后找 depth 最深的非-parent entry
/// 4. 无 leaf_pointer：全局找 depth 最深的非-parent entry
pub fn resolve_current_leaf(entries: &[Value]) -> Option<String> {
    // 收集所有"作为别人 parent"的 id（这些不是 leaf 候选）
    let mut parent_ids: HashSet<String> = HashSet::new();
    for e in entries {
        if !is_header(e) && !is_leaf_pointer(e) && let Some(p) = entry_parent_id(e) {
            parent_ids.insert(p.to_string());
        }
    }
    // id -> entry 索引
    let by_id: HashMap<&str, &Value> = entries
        .iter()
        .filter_map(|e| entry_id(e).map(|id| (id, e)))
        .collect();

    // 预计算所有 depth（O(n) 单趟 DP），替代逐个 entry_depth（最坏 O(n²)）
    let depths = compute_depths(&by_id);

    // Phase B：从后往前找最后一个 leaf_pointer
    let lp_pos = entries.iter().rposition(is_leaf_pointer);

    match lp_pos {
        Some(i) => {
            let lp = &entries[i];
            match leaf_pointer_target(lp) {
                Some(target_id) if !target_id.is_empty() => {
                    // leafId 非空：先在 i 之后找 target 的后代中 depth 最深的非-parent entry。
                    // 若无后代，target 本身就是 leaf（用户显式指向它，不管它是不是别人的 parent）。
                    deepest_descendant_after(entries, target_id, &parent_ids, &by_id, &depths, i, false)
                        .or_else(|| Some(target_id.to_string()))
                }
                _ => {
                    // leafId 为空（reset）：在 i 之后找 depth 最深的非-parent entry
                    deepest_descendant_after(entries, "", &parent_ids, &by_id, &depths, i, true)
                }
            }
        }
        // Phase C：无 leaf_pointer —— 全局找 depth 最深的非-parent entry
        None => deepest_non_parent(entries, &parent_ids, &depths),
    }
}

/// 在 pos 之后找目标的后代（或任意 entry）中 depth 最深且非-parent 的。
fn deepest_descendant_after(
    entries: &[Value],
    target: &str,
    parent_ids: &HashSet<String>,
    by_id: &HashMap<&str, &Value>,
    depths: &HashMap<&str, usize>,
    pos: usize,
    allow_any: bool,
) -> Option<String> {
    let mut best: Option<(String, usize)> = None;
    for e in entries.iter().skip(pos + 1) {
        let id = match entry_id(e) {
            Some(id) => id,
            None => continue,
        };
        if is_header(e) || is_leaf_pointer(e) {
            continue;
        }
        if parent_ids.contains(id) {
            continue; // 是别人的 parent，不是 leaf 候选
        }
        let ok = if allow_any {
            true
        } else {
            is_descendant_of(id, target, by_id)
        };
        if ok {
            // O(1) 查预计算 depth，替代 O(depth) 的 entry_depth
            let depth = *depths.get(id).unwrap_or(&0);
            match &best {
                None => best = Some((id.to_string(), depth)),
                Some((_, d)) if depth > *d => best = Some((id.to_string(), depth)),
                _ => {}
            }
        }
    }
    best.map(|(id, _)| id)
}

/// 无 leaf_pointer 时：全局找 depth 最深的非-parent entry。
fn deepest_non_parent(
    entries: &[Value],
    parent_ids: &HashSet<String>,
    depths: &HashMap<&str, usize>,
) -> Option<String> {
    let mut best: Option<(String, usize)> = None;
    for e in entries {
        let id = match entry_id(e) {
            Some(id) => id,
            None => continue,
        };
        if is_header(e) || is_leaf_pointer(e) {
            continue;
        }
        if parent_ids.contains(id) {
            continue;
        }
        let depth = *depths.get(id).unwrap_or(&0);
        match &best {
            None => best = Some((id.to_string(), depth)),
            Some((_, d)) if depth > *d => best = Some((id.to_string(), depth)),
            _ => {}
        }
    }
    best.map(|(id, _)| id)
}

/// 找最后一个 leaf_pointer 指向的 target（用于判断当前分支点）。
pub fn find_leaf_pointer_target(entries: &[Value]) -> Option<String> {
    entries
        .iter()
        .rev()
        .find(|e| is_leaf_pointer(e))
        .and_then(|lp| leaf_pointer_target(lp).map(|s| s.to_string()))
}

/// 按 parentId 建树。过滤 header 和 leaf_pointer（它们不是树节点）。
/// 对齐 pi `getTree()`。
pub fn get_tree(entries: &[Value]) -> Vec<TreeNode> {
    // 建 label 映射（LabelEntry.targetId -> label）
    let labels: HashMap<String, String> = entries
        .iter()
        .filter_map(|e| {
            if e.get("type").and_then(|v| v.as_str()) == Some("label") {
                let target = e.get("targetId").and_then(|v| v.as_str())?;
                let label = e.get("label").and_then(|v| v.as_str())?;
                Some((target.to_string(), label.to_string()))
            } else {
                None
            }
        })
        .collect();

    // 只保留树节点（非 header、非 leaf_pointer）
    let tree_entries: Vec<&Value> = entries
        .iter()
        .filter(|e| !is_header(e) && !is_leaf_pointer(e))
        .collect();

    // id -> node（先建空 children）
    let mut node_map: HashMap<String, TreeNode> = HashMap::new();
    for e in &tree_entries {
        if let Some(id) = entry_id(e) {
            node_map.insert(
                id.to_string(),
                TreeNode {
                    entry: (*e).clone(),
                    children: vec![],
                    label: labels.get(id).cloned(),
                },
            );
        }
    }

    // 按 parentId 挂载；找不到 parent 的当 root
    let mut roots = vec![];
    for e in &tree_entries {
        let id = match entry_id(e) {
            Some(id) => id,
            None => continue,
        };
        let parent_id = entry_parent_id(e);
        match parent_id {
            None => {
                roots.push(id.to_string());
            }
            Some(p) if p == id => {
                roots.push(id.to_string());
            }
            Some(p) => {
                if node_map.contains_key(p) {
                    if let Some(child) = node_map.get(id).cloned() && let Some(parent) = node_map.get_mut(p) {
                        parent.children.push(child);
                    }
                } else {
                    // parent 不在 node_map（可能是 session id）→ 当 root
                    roots.push(id.to_string());
                }
            }
        }
    }

    // 按 timestamp 升序排每个节点的 children（oldest first）
    let result: Vec<TreeNode> = roots
        .into_iter()
        .filter_map(|id| node_map.remove(&id))
        .map(|mut n| {
            sort_children(&mut n);
            n
        })
        .collect();
    result
}

/// 递归按 timestamp 升序排 children。
fn sort_children(node: &mut TreeNode) {
    node.children.sort_by(|a, b| {
        let ta = a
            .entry
            .get("timestamp")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let tb = b
            .entry
            .get("timestamp")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        ta.cmp(tb)
    });
    for c in &mut node.children {
        sort_children(c);
    }
}

/// 沿 parentId 回溯，返回 root→leaf 顺序的 entries。
/// 跳过 header（session 行）——它不是对话内容。
pub fn get_branch_path(entries: &[Value], leaf_id: &str) -> Vec<Value> {
    let by_id: HashMap<&str, &Value> = entries
        .iter()
        .filter_map(|e| entry_id(e).map(|id| (id, e)))
        .collect();
    let mut path = vec![];
    let mut cur = Some(leaf_id);
    let mut visited = HashSet::new();
    while let Some(id) = cur {
        if !visited.insert(id) {
            break; // 环保护
        }
        if let Some(e) = by_id.get(id) {
            // 跳过 header（session 行）和 leaf_pointer
            if !is_header(e) && !is_leaf_pointer(e) {
                path.push((*e).clone());
            }
            cur = entry_parent_id(e);
        } else {
            break;
        }
    }
    path.reverse();
    path
}

/// compaction 截断：从路径末尾往前找第一个 compaction entry，slice 到那里。
/// 用于 fork-from-leaf 时丢弃被压缩的前缀。
pub fn truncate_at_compaction(path: Vec<Value>) -> Vec<Value> {
    for i in (0..path.len()).rev() {
        if path[i]
            .get("type")
            .and_then(|v| v.as_str())
            == Some("compaction")
        {
            return path[i..].to_vec();
        }
    }
    path
}

/// 找所有命名分支（LabelEntry）。
/// 返回 (label, targetId) 列表。
pub fn named_branches(entries: &[Value]) -> Vec<(String, String)> {
    entries
        .iter()
        .filter_map(|e| {
            if e.get("type").and_then(|v| v.as_str()) == Some("label") {
                let target = e.get("targetId").and_then(|v| v.as_str())?;
                let label = e.get("label").and_then(|v| v.as_str())?;
                Some((label.to_string(), target.to_string()))
            } else {
                None
            }
        })
        .collect()
}

/// 分叉：从 from_id 开新枝。
/// 返回要追加的新 entries（leaf_pointer + 可选 label）。
/// 遵守 only-append：不改已有 entries。
///
/// 错误：from_id 不存在 → Err
pub fn make_branch(from_id: &str, name: Option<&str>) -> Result<Vec<Value>, String> {
    // leaf_pointer（移动光标到 from_id）
    let lp = serde_json::json!({
        "type": "leaf_pointer",
        "id": gen_id(),
        "parentId": null,
        "timestamp": ts(),
        "leafId": from_id,
    });
    let mut new_entries = vec![lp];
    // 可选 label（命名分支）
    if let Some(label) = name {
        let lbl = serde_json::json!({
            "type": "label",
            "id": gen_id(),
            "parentId": null,
            "timestamp": ts(),
            "targetId": from_id,
            "label": label,
        });
        new_entries.push(lbl);
    }
    Ok(new_entries)
}

/// 回滚：光标移回 rollback_to，被跳过的路径保留 + 可选 tombstone。
/// 返回要追加的新 entries（leaf_pointer + 可选 branch_summary）。
///
/// 错误：rollback_to 不存在 → Err
pub fn make_rollback(
    rollback_to: &str,
    old_leaf: Option<&str>,
    reason: Option<&str>,
) -> Result<Vec<Value>, String> {
    let lp = serde_json::json!({
        "type": "leaf_pointer",
        "id": gen_id(),
        "parentId": null,
        "timestamp": ts(),
        "leafId": rollback_to,
    });
    let mut new_entries = vec![lp];
    // 可选 tombstone（branch_summary，纯文本）
    if let Some(reason_text) = reason {
        let from = old_leaf.unwrap_or("root");
        let bs = serde_json::json!({
            "type": "branch_summary",
            "id": gen_id(),
            "parentId": from,
            "timestamp": ts(),
            "fromId": from,
            "summary": format!("rollback: {} → {} | {}", from, rollback_to, reason_text),
            "fromHook": false,
        });
        new_entries.push(bs);
    }
    Ok(new_entries)
}

/// 切换：找 name 对应的 label，光标移到它的 targetId。
/// 返回要追加的新 entries（单个 leaf_pointer）。
///
/// 错误：name 不存在 → Err（列出可用分支）
pub fn make_checkout(entries: &[Value], name: &str) -> Result<Vec<Value>, String> {
    let branches = named_branches(entries);
    let target = branches
        .iter()
        .find(|(label, _)| label == name)
        .map(|(_, t)| t.clone())
        .ok_or_else(|| {
            let avail: Vec<_> = branches.iter().map(|(l, _)| l.as_str()).collect();
            format!("branch '{}' not found. Available: {}", name, avail.join(", "))
        })?;
    let lp = serde_json::json!({
        "type": "leaf_pointer",
        "id": gen_id(),
        "parentId": null,
        "timestamp": ts(),
        "leafId": target,
    });
    Ok(vec![lp])
}

// 内部 helper：生成 id / timestamp（不依赖 session_jsonl，保持模块独立可测）
fn gen_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let ts_nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:x}{:x}", ts_nanos, n)
}

fn ts() -> String {
    // 简易 ISO 时间戳（不依赖 chrono）
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("2026-01-01T00:00:{}Z", secs % 60)
}

/// 校验 entry id 是否存在于 entries 中。
pub fn entry_exists(entries: &[Value], id: &str) -> bool {
    entries.iter().any(|e| entry_id(e) == Some(id))
}

/// compaction 安全检查：判断 target_id 是否在某个 compaction entry 之前。
/// 如果是，分支/回滚到该点会穿越已压缩的上下文（丢失摘要），应拒绝。
///
/// 返回 Some(compaction_id) 表示不安全（target 在该 compaction 之前）；
/// 返回 None 表示安全。
///
/// 算法：找所有 compaction entry，若 target 在任一 compaction 的"祖先链"里
/// （即 compaction 是 target 的后代），则 target 在该 compaction 之前。
pub fn check_compaction_safety(entries: &[Value], target_id: &str) -> Option<String> {
    let by_id: HashMap<&str, &Value> = entries
        .iter()
        .filter_map(|e| entry_id(e).map(|id| (id, e)))
        .collect();

    for e in entries {
        if e.get("type").and_then(|v| v.as_str()) != Some("compaction") {
            continue;
        }
        let compaction_id = match entry_id(e) {
            Some(id) => id,
            None => continue,
        };
        // target 自身就是这个 compaction → 安全（分支到 compaction 点本身 OK）
        if compaction_id == target_id {
            continue;
        }
        // 若 compaction 是 target 的后代（target 在 compaction 之前），不安全
        if is_descendant_of(compaction_id, target_id, &by_id) {
            return Some(compaction_id.to_string());
        }
    }
    None
}

/// Count the number of branches in a session tree.
///
/// A branch is identified by entries with different parentId chains.
/// Counts unique leaf nodes (entries that are not the parent of any other entry).
/// Leaf pointers and headers are excluded from counting.
pub fn count_branches(entries: &[serde_json::Value]) -> usize {
    // Collect all ids that appear as parentId (i.e., have children)
    let parent_ids: HashSet<&str> = entries
        .iter()
        .filter_map(|e| {
            if is_header(e) || is_leaf_pointer(e) {
                return None;
            }
            e.get("parentId").and_then(|v| v.as_str())
        })
        .collect();

    // Count leaf nodes: entries that are not a parent of any other entry,
    // excluding headers and leaf pointers
    entries
        .iter()
        .filter(|e| {
            // Skip headers and leaf pointers
            if is_header(e) || is_leaf_pointer(e) {
                return false;
            }
            let id = match entry_id(e) {
                Some(id) => id,
                None => return false,
            };
            // An entry is a leaf if its id never appears as parentId
            !parent_ids.contains(id)
        })
        .count()
}

// ═══════════════════════════════════════════════════════════════════════════
// 单元测试
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 辅助：构造一条 message entry。
    fn msg(id: &str, parent: &str, text: &str) -> Value {
        json!({
            "type": "message",
            "id": id,
            "parentId": parent,
            "timestamp": "2026-07-08T10:00:00Z",
            "message": {"role": "user", "content": text}
        })
    }

    /// 辅助：构造一条 leaf_pointer entry。
    fn leaf_ptr(id: &str, target: Option<&str>) -> Value {
        json!({
            "type": "leaf_pointer",
            "id": id,
            "parentId": null,
            "timestamp": "2026-07-08T10:00:00Z",
            "leafId": target
        })
    }

    /// 辅助：构造 session header。
    fn header(id: &str) -> Value {
        json!({"type": "session", "version": 3, "id": id, "timestamp": "x", "cwd": "/x"})
    }

    #[test]
    fn resolve_leaf_no_pointer_returns_deepest() {
        // 线性链：h → m1 → m2 → m3
        let entries = vec![
            header("h"),
            msg("m1", "h", "a"),
            msg("m2", "m1", "b"),
            msg("m3", "m2", "c"),
        ];
        assert_eq!(resolve_current_leaf(&entries), Some("m3".into()));
    }

    #[test]
    fn resolve_leaf_with_pointer_returns_target() {
        // 链：h → m1 → m2 → m3，末尾 leaf_pointer 指向 m1（m1 非 parent? m2 是 m1 的 child，m1 是 parent）
        // 所以 m1 是 parent，应回退到 target 本身？不，m1 被引用为 parent → 在 parent_ids 里。
        // 此时 deepest_descendant_after 找 m1 的后代在 pointer 之后 → 无（pointer 是最后）。
        // 回退：target(m1) 是 parent → 返回 None... 这不对。
        // 实际语义：leaf_pointer 指向 m1 表示"光标回到 m1，下条新消息接 m1"。
        // 此时 current_leaf 应该是 m1（即使它是别人的 parent）。
        let entries = vec![
            header("h"),
            msg("m1", "h", "a"),
            msg("m2", "m1", "b"),
            msg("m3", "m2", "c"),
            leaf_ptr("lp1", Some("m1")),
        ];
        // m1 是 parent（m2 的 parent），但 leaf_pointer 显式指向它 → current_leaf = m1
        assert_eq!(resolve_current_leaf(&entries), Some("m1".into()));
    }

    #[test]
    fn resolve_leaf_pointer_then_append_descendants() {
        // 链：h → m1 → m2，leaf_pointer 指向 m1，之后又 append 了 m3（parent=m1）
        // 此时 m3 是 m1 的后代，在 pointer 之后，depth=2（h→m1→m3），应返回 m3
        let entries = vec![
            header("h"),
            msg("m1", "h", "a"),
            msg("m2", "m1", "b"),
            leaf_ptr("lp1", Some("m1")),
            msg("m3", "m1", "c"), // 新分支接 m1
        ];
        assert_eq!(resolve_current_leaf(&entries), Some("m3".into()));
    }

    #[test]
    fn resolve_leaf_empty_entries() {
        assert_eq!(resolve_current_leaf(&[]), None);
    }

    #[test]
    fn resolve_leaf_only_header() {
        let entries = vec![header("h")];
        assert_eq!(resolve_current_leaf(&entries), None);
    }

    #[test]
    fn resolve_leaf_multiple_pointers_last_wins() {
        // 两个 leaf_pointer，最后一个（指向 m2）生效
        let entries = vec![
            header("h"),
            msg("m1", "h", "a"),
            msg("m2", "m1", "b"),
            leaf_ptr("lp1", Some("m1")),
            leaf_ptr("lp2", Some("m2")),
        ];
        assert_eq!(resolve_current_leaf(&entries), Some("m2".into()));
    }

    #[test]
    fn get_tree_linear_chain() {
        let entries = vec![
            header("h"),
            msg("m1", "h", "a"),
            msg("m2", "m1", "b"),
        ];
        let tree = get_tree(&entries);
        assert_eq!(tree.len(), 1, "single root");
        assert_eq!(entry_id(&tree[0].entry), Some("m1"));
        assert_eq!(tree[0].children.len(), 1);
        assert_eq!(entry_id(&tree[0].children[0].entry), Some("m2"));
    }

    #[test]
    fn get_tree_branch_two_children() {
        // m2 和 m3 都以 m1 为 parent → 分叉
        let entries = vec![
            header("h"),
            msg("m1", "h", "a"),
            msg("m2", "m1", "b"),
            msg("m3", "m1", "c"),
        ];
        let tree = get_tree(&entries);
        assert_eq!(tree.len(), 1);
        let root = &tree[0];
        assert_eq!(entry_id(&root.entry), Some("m1"));
        assert_eq!(root.children.len(), 2, "m1 has two children (branch)");
    }

    #[test]
    fn get_tree_filters_leaf_pointer() {
        let entries = vec![
            header("h"),
            msg("m1", "h", "a"),
            leaf_ptr("lp1", Some("m1")),
            msg("m2", "m1", "b"),
        ];
        let tree = get_tree(&entries);
        // leaf_pointer 不应出现在树里
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].children.len(), 1); // m2 是 m1 的子节点，lp 不是
    }

    #[test]
    fn get_tree_orphan_as_root() {
        // parentId 指向不存在的 entry → 当 root
        let entries = vec![
            header("h"),
            msg("m1", "nonexistent", "a"),
        ];
        let tree = get_tree(&entries);
        assert_eq!(tree.len(), 1, "orphan treated as root");
    }

    #[test]
    fn get_branch_path_linear() {
        let entries = vec![
            header("h"),
            msg("m1", "h", "a"),
            msg("m2", "m1", "b"),
            msg("m3", "m2", "c"),
        ];
        let path = get_branch_path(&entries, "m3");
        assert_eq!(path.len(), 3);
        assert_eq!(entry_id(&path[0]), Some("m1"));
        assert_eq!(entry_id(&path[1]), Some("m2"));
        assert_eq!(entry_id(&path[2]), Some("m3"));
    }

    #[test]
    fn get_branch_path_partial() {
        // 从分支中间取路径（m2 那条分支）
        let entries = vec![
            header("h"),
            msg("m1", "h", "a"),
            msg("m2", "m1", "b"),
            msg("m3", "m1", "c"),
            msg("m4", "m3", "d"),
        ];
        let path = get_branch_path(&entries, "m4");
        // m4 → m3 → m1（不含 m2）
        assert_eq!(path.len(), 3);
        let ids: Vec<_> = path.iter().filter_map(|e| entry_id(e)).collect();
        assert_eq!(ids, vec!["m1", "m3", "m4"]);
    }

    #[test]
    fn truncate_at_compaction_no_compaction() {
        let path = vec![msg("m1", "h", "a"), msg("m2", "m1", "b")];
        let result = truncate_at_compaction(path);
        assert_eq!(result.len(), 2, "no compaction → unchanged");
    }

    #[test]
    fn truncate_at_compaction_with_compaction() {
        let compaction = json!({"type": "compaction", "id": "c1", "parentId": "h", "summary": "..."});
        let path = vec![
            msg("m1", "h", "a"),
            compaction,
            msg("m2", "c1", "b"),
        ];
        let result = truncate_at_compaction(path);
        assert_eq!(result.len(), 2, "truncated from compaction onward");
        assert_eq!(entry_id(&result[0]), Some("c1"));
        assert_eq!(entry_id(&result[1]), Some("m2"));
    }

    #[test]
    fn named_branches_finds_labels() {
        let entries = vec![
            header("h"),
            msg("m1", "h", "a"),
            json!({"type": "label", "id": "l1", "parentId": "h", "targetId": "m1", "label": "try-a", "timestamp": "x"}),
            json!({"type": "label", "id": "l2", "parentId": "h", "targetId": "m2", "label": "try-b", "timestamp": "x"}),
        ];
        let branches = named_branches(&entries);
        assert_eq!(branches.len(), 2);
        assert!(branches.contains(&("try-a".into(), "m1".into())));
        assert!(branches.contains(&("try-b".into(), "m2".into())));
    }

    #[test]
    fn find_leaf_pointer_target_returns_last() {
        let entries = vec![
            header("h"),
            msg("m1", "h", "a"),
            leaf_ptr("lp1", Some("m1")),
            leaf_ptr("lp2", Some("m1")),
        ];
        // 两个 leaf_pointer，find 返回最后一个的 target（都是 m1）
        assert_eq!(find_leaf_pointer_target(&entries), Some("m1".into()));
    }

    #[test]
    fn find_leaf_pointer_target_none_when_absent() {
        let entries = vec![header("h"), msg("m1", "h", "a")];
        assert_eq!(find_leaf_pointer_target(&entries), None);
    }

    // ── Phase 3: branch / rollback / checkout ──

    #[test]
    fn make_branch_returns_leaf_pointer() {
        let new = make_branch("m2", None).unwrap();
        assert_eq!(new.len(), 1, "branch without name = just leaf_pointer");
        assert_eq!(new[0]["type"].as_str(), Some("leaf_pointer"));
        assert_eq!(new[0]["leafId"].as_str(), Some("m2"));
    }

    #[test]
    fn make_branch_with_name_adds_label() {
        let new = make_branch("m2", Some("try-div")).unwrap();
        assert_eq!(new.len(), 2);
        assert_eq!(new[0]["type"].as_str(), Some("leaf_pointer"));
        assert_eq!(new[1]["type"].as_str(), Some("label"));
        assert_eq!(new[1]["targetId"].as_str(), Some("m2"));
        assert_eq!(new[1]["label"].as_str(), Some("try-div"));
    }

    #[test]
    fn make_rollback_without_reason() {
        let new = make_rollback("m2", Some("m5"), None).unwrap();
        assert_eq!(new.len(), 1, "rollback without reason = just leaf_pointer");
        assert_eq!(new[0]["leafId"].as_str(), Some("m2"));
    }

    #[test]
    fn make_rollback_with_reason_adds_tombstone() {
        let new = make_rollback("m2", Some("m5"), Some("走错了")).unwrap();
        assert_eq!(new.len(), 2);
        assert_eq!(new[0]["type"].as_str(), Some("leaf_pointer"));
        assert_eq!(new[1]["type"].as_str(), Some("branch_summary"));
        // tombstone 的 parentId 指向被废弃的旧 leaf
        assert_eq!(new[1]["parentId"].as_str(), Some("m5"));
        assert!(new[1]["summary"].as_str().unwrap().contains("走错了"));
        assert!(new[1]["summary"].as_str().unwrap().contains("m5"));
        assert!(new[1]["summary"].as_str().unwrap().contains("m2"));
    }

    #[test]
    fn make_checkout_finds_label() {
        let entries = vec![
            header("h"),
            msg("m1", "h", "a"),
            msg("m2", "m1", "b"),
            json!({"type": "label", "id": "l1", "parentId": null, "targetId": "m1", "label": "try-a", "timestamp": "x"}),
            json!({"type": "label", "id": "l2", "parentId": null, "targetId": "m2", "label": "try-b", "timestamp": "x"}),
        ];
        let new = make_checkout(&entries, "try-b").unwrap();
        assert_eq!(new.len(), 1);
        assert_eq!(new[0]["leafId"].as_str(), Some("m2"));
    }

    #[test]
    fn make_checkout_unknown_name_errors_with_available() {
        let entries = vec![
            header("h"),
            msg("m1", "h", "a"),
            json!({"type": "label", "id": "l1", "parentId": null, "targetId": "m1", "label": "try-a", "timestamp": "x"}),
        ];
        let result = make_checkout(&entries, "nonexist");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("try-a"), "error lists available branches");
    }

    #[test]
    fn entry_exists_finds_id() {
        let entries = vec![header("h"), msg("m1", "h", "a")];
        assert!(entry_exists(&entries, "m1"));
        assert!(!entry_exists(&entries, "nonexist"));
    }

    #[test]
    fn branch_operation_preserves_only_append() {
        // 验证 branch 操作不修改已有 entries（only-append 不变量）
        let entries = vec![
            header("h"),
            msg("m1", "h", "a"),
            msg("m2", "m1", "b"),
        ];
        let original = entries.clone();
        let _new = make_branch("m1", Some("x")).unwrap();
        // 原有 entries 不变
        assert_eq!(entries, original);
    }

    // ── Phase 8: compaction 安全检查 ──

    /// 辅助：构造 compaction entry
    fn compaction(id: &str, parent: &str) -> Value {
        json!({
            "type": "compaction", "id": id, "parentId": parent,
            "timestamp": "x", "summary": "...", "tokensBefore": 1000
        })
    }

    #[test]
    fn compaction_safe_when_no_compaction() {
        let entries = vec![header("h"), msg("m1", "h", "a"), msg("m2", "m1", "b")];
        assert!(check_compaction_safety(&entries, "m1").is_none());
        assert!(check_compaction_safety(&entries, "m2").is_none());
    }

    #[test]
    fn compaction_unsafe_when_target_before_compaction() {
        // h → m1 → m2 → compaction(c1, parent=m2) → m3
        // branch 到 m1：m1 在 c1 之前（c1 是 m1 的后代）→ 不安全
        let entries = vec![
            header("h"),
            msg("m1", "h", "a"),
            msg("m2", "m1", "b"),
            compaction("c1", "m2"),
            msg("m3", "c1", "c"),
        ];
        let result = check_compaction_safety(&entries, "m1");
        assert!(result.is_some(), "branch to m1 (before compaction) must be flagged");
        assert_eq!(result.unwrap(), "c1");
    }

    #[test]
    fn compaction_safe_when_target_after_compaction() {
        // branch 到 m3（compaction 之后）→ 安全
        let entries = vec![
            header("h"),
            msg("m1", "h", "a"),
            compaction("c1", "m1"),
            msg("m2", "c1", "b"),
        ];
        assert!(check_compaction_safety(&entries, "m2").is_none(), "branch to m2 (after compaction) is safe");
        assert!(check_compaction_safety(&entries, "c1").is_none(), "branch to compaction itself is safe");
    }

    #[test]
    fn test_count_branches() {
        // Linear chain: h → m1 → m2 → m3 → m4
        // Only m4 is a leaf (no children) → 1 branch
        let entries = vec![
            header("h"),
            msg("m1", "h", "a"),
            msg("m2", "m1", "b"),
            msg("m3", "m2", "c"),
            msg("m4", "m3", "d"),
        ];
        assert_eq!(count_branches(&entries), 1, "linear chain has 1 branch");

        // Fork: h → m1 → m2, h → m1 → m3
        // Leaf nodes: m2, m3 → 2 branches
        let entries = vec![
            header("h"),
            msg("m1", "h", "a"),
            msg("m2", "m1", "b"),
            msg("m3", "m1", "c"),
        ];
        assert_eq!(count_branches(&entries), 2, "fork with 2 leaves has 2 branches");

        // Multiple levels: h → m1 → m2, h → m1 → m3 → m4
        // Leaf nodes: m2, m4 → 2 branches
        let entries = vec![
            header("h"),
            msg("m1", "h", "a"),
            msg("m2", "m1", "b"),
            msg("m3", "m1", "c"),
            msg("m4", "m3", "d"),
        ];
        assert_eq!(count_branches(&entries), 2, "nested fork has 2 branches");

        // Empty entries → 0
        assert_eq!(count_branches(&[]), 0, "empty entries has 0 branches");

        // Only header → 0
        let entries = vec![header("h")];
        assert_eq!(count_branches(&entries), 0, "only header has 0 branches");

        // Single message → 1 (the message is a leaf)
        let entries = vec![
            header("h"),
            msg("m1", "h", "a"),
        ];
        assert_eq!(count_branches(&entries), 1, "single message has 1 branch");

        // With leaf pointers (should be ignored)
        let entries = vec![
            header("h"),
            msg("m1", "h", "a"),
            msg("m2", "m1", "b"),
            msg("m3", "m1", "c"),
            leaf_ptr("lp1", Some("m2")),
            leaf_ptr("lp2", Some("m3")),
        ];
        assert_eq!(count_branches(&entries), 2, "leaf pointers are not counted as tree nodes");
    }

    // ── Additional coverage tests ──

    // 1. LeafPointerEntry construction + serialization

    /// Verify leaf_pointer entry has the expected JSON shape when constructed via the
    /// helper, and that it round-trips through serde serialization.
    #[test]
    fn leaf_pointer_construction_and_serialization() {
        let lp = leaf_ptr("lp1", Some("m2"));
        // Required fields are present and typed correctly
        assert_eq!(lp["type"].as_str(), Some("leaf_pointer"));
        assert_eq!(lp["id"].as_str(), Some("lp1"));
        assert_eq!(lp["leafId"].as_str(), Some("m2"));
        assert!(lp["parentId"].is_null(), "leaf_pointer parentId is null");

        // Round-trip: serialize to JSON string and back, fields preserved
        let json_str = serde_json::to_string(&lp).expect("must serialize");
        let parsed: Value = serde_json::from_str(&json_str).expect("must deserialize");
        assert_eq!(parsed["type"].as_str(), Some("leaf_pointer"));
        assert_eq!(parsed["leafId"].as_str(), Some("m2"));

        // is_leaf_pointer predicate must accept it
        assert!(is_leaf_pointer(&lp));

        // leaf_pointer_target helper extracts the target id
        assert_eq!(leaf_pointer_target(&lp), Some("m2"));
    }

    /// A leaf_pointer with an empty leafId string represents a "reset".
    /// is_leaf_pointer still matches, and leaf_pointer_target returns Some("").
    #[test]
    fn leaf_pointer_empty_target_is_still_leaf_pointer() {
        let lp = leaf_ptr("lp2", None);
        assert!(is_leaf_pointer(&lp), "type field is still leaf_pointer");
        // None maps to null in JSON; leaf_pointer_target returns None
        assert_eq!(leaf_pointer_target(&lp), None);
    }

    // 2. resolve_current_leaf with various tree shapes

    /// A forked tree with two leaves but no leaf_pointer:
    /// the deeper (or first-seen deepest) non-parent entry wins.
    /// h → m1 → m2 → m3  (depth 3)
    ///      └ → m4         (depth 2)
    /// m3 is the deepest leaf → wins.
    #[test]
    fn resolve_leaf_forked_picks_deeper_branch() {
        let entries = vec![
            header("h"),
            msg("m1", "h", "a"),
            msg("m2", "m1", "b"),
            msg("m4", "m1", "d"), // sibling branch, depth 2
            msg("m3", "m2", "c"), // depth 3
        ];
        assert_eq!(resolve_current_leaf(&entries), Some("m3".into()));
    }

    /// A reset leaf_pointer (empty leafId) in the middle of the log:
    /// entries after it become candidates; the deepest non-parent after it wins.
    #[test]
    fn resolve_leaf_reset_pointer_finds_deepest_after() {
        // h → m1 → m2, then reset pointer, then m3 → m4 (new chain after reset)
        let entries = vec![
            header("h"),
            msg("m1", "h", "a"),
            msg("m2", "m1", "b"),
            leaf_ptr("lp1", None), // reset
            msg("m3", "h", "c"),   // new branch from h
            msg("m4", "m3", "d"),  // deepest non-parent after reset
        ];
        assert_eq!(resolve_current_leaf(&entries), Some("m4".into()));
    }

    /// Two sibling branches both at depth 1 (same depth), neither is a parent.
    /// resolve_current_leaf returns one of them (deterministic: first-seen with
    /// that max depth since `>` is strict).
    #[test]
    fn resolve_leaf_equal_depth_siblings() {
        // h → m1, h → m2  (both depth 1, both leaves)
        let entries = vec![
            header("h"),
            msg("m1", "h", "a"),
            msg("m2", "h", "b"),
        ];
        let leaf = resolve_current_leaf(&entries);
        assert!(leaf == Some("m1".into()) || leaf == Some("m2".into()),
            "leaf must be one of the two siblings");
    }

    // 3. get_tree structure vs full mode (labels attached)

    /// get_tree attaches labels from label entries onto the matching tree node.
    /// Note: label entries themselves are NOT filtered by get_tree (only headers
    /// and leaf_pointers are), so a label with no parentId appears as an extra
    /// root. The label text is attached to the node whose id matches targetId.
    #[test]
    fn get_tree_attaches_label_to_node() {
        let entries = vec![
            header("h"),
            msg("m1", "h", "a"),
            msg("m2", "m1", "b"),
            json!({"type": "label", "id": "l1", "parentId": null, "targetId": "m1", "label": "checkpoint", "timestamp": "x"}),
        ];
        let tree = get_tree(&entries);
        // The label entry (parentId null) becomes a second root, so 2 roots.
        // Find the node for m1 and verify it carries the label.
        let m1_node = tree.iter().find(|n| entry_id(&n.entry) == Some("m1"));
        assert!(m1_node.is_some(), "m1 node must be present");
        assert_eq!(
            m1_node.unwrap().label.as_deref(),
            Some("checkpoint"),
            "m1 node should carry the label"
        );
        // m2 has no label
        let m2_node = tree
            .iter()
            .find_map(|n| n.children.iter().find(|c| entry_id(&c.entry) == Some("m2")));
        assert!(m2_node.is_some());
        assert!(m2_node.unwrap().label.is_none(), "m2 has no label");
    }

    /// get_tree produces a multi-root forest when several entries share no parent
    /// (or have a parentId pointing outside the tree).
    #[test]
    fn get_tree_multiple_roots() {
        let entries = vec![
            header("h"),
            msg("m1", "ghost", "a"), // parent not in tree → root
            msg("m2", "ghost", "b"), // parent not in tree → root
        ];
        let tree = get_tree(&entries);
        assert_eq!(tree.len(), 2, "two disconnected roots form a forest");
        // Each root is childless
        assert_eq!(tree[0].children.len(), 0);
        assert_eq!(tree[1].children.len(), 0);
    }

    /// get_tree sorts children by timestamp ascending. Insert children out of
    /// timestamp order and verify they come back sorted.
    #[test]
    fn get_tree_sorts_children_by_timestamp() {
        let older = json!({"type": "message", "id": "a", "parentId": "root", "timestamp": "2026-01-01T00:00:00Z", "message": {"role":"user","content":"old"}});
        let newer = json!({"type": "message", "id": "b", "parentId": "root", "timestamp": "2026-01-01T00:00:05Z", "message": {"role":"user","content":"new"}});
        let root = json!({"type": "message", "id": "root", "parentId": "h", "timestamp": "2026-01-01T00:00:00Z", "message": {"role":"user","content":"root"}});
        let entries = vec![
            header("h"),
            root,
            newer.clone(), // appended first, later timestamp
            older.clone(), // appended second, earlier timestamp
        ];
        let tree = get_tree(&entries);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].children.len(), 2);
        // Older child must come first after sorting
        assert_eq!(entry_id(&tree[0].children[0].entry), Some("a"));
        assert_eq!(entry_id(&tree[0].children[1].entry), Some("b"));
    }

    // 4. Branch naming + checkout logic helpers

    /// make_branch with a name produces a label whose targetId equals the from_id.
    /// Combined with named_branches round-trip: building a branch then scanning
    /// for labels recovers the same (label, target) pair.
    #[test]
    fn make_branch_label_round_trips_through_named_branches() {
        let base_entries = vec![header("h"), msg("m1", "h", "a")];
        let new = make_branch("m1", Some("feature-x")).unwrap();
        let combined: Vec<Value> = base_entries.iter().cloned().chain(new).collect();

        let branches = named_branches(&combined);
        assert!(branches.iter().any(|(l, t)| l == "feature-x" && t == "m1"),
            "named_branches must find the label created by make_branch");
    }

    /// make_checkout produces a leaf_pointer whose leafId equals the label's
    /// targetId. Appending it to the session and re-running make_checkout for a
    /// different existing label yields a different target.
    #[test]
    fn make_checkout_targets_correct_label() {
        let entries = vec![
            header("h"),
            msg("m1", "h", "a"),
            msg("m2", "m1", "b"),
            json!({"type": "label", "id": "l1", "parentId": null, "targetId": "m1", "label": "root", "timestamp": "x"}),
            json!({"type": "label", "id": "l2", "parentId": null, "targetId": "m2", "label": "tip", "timestamp": "x"}),
        ];
        let co_root = make_checkout(&entries, "root").unwrap();
        assert_eq!(co_root[0]["leafId"].as_str(), Some("m1"));

        let co_tip = make_checkout(&entries, "tip").unwrap();
        assert_eq!(co_tip[0]["leafId"].as_str(), Some("m2"));
    }

    /// named_branches ignores non-label entries (messages, headers, leaf_pointers)
    /// and only returns entries of type "label".
    #[test]
    fn named_branches_ignores_non_label_entries() {
        let entries = vec![
            header("h"),
            msg("m1", "h", "a"),
            leaf_ptr("lp1", Some("m1")),
            json!({"type": "branch_summary", "id": "bs1", "parentId": null, "summary": "...", "timestamp": "x"}),
        ];
        assert!(named_branches(&entries).is_empty(),
            "no label entries → empty result");
    }

    /// find_leaf_pointer_target returns the target of the *last* leaf_pointer in
    /// the log, even when earlier pointers point elsewhere.
    #[test]
    fn find_leaf_pointer_target_returns_last_even_with_different_targets() {
        let entries = vec![
            header("h"),
            msg("m1", "h", "a"),
            msg("m2", "m1", "b"),
            msg("m3", "m2", "c"),
            leaf_ptr("lp1", Some("m1")),
            leaf_ptr("lp2", Some("m3")),
        ];
        // Last pointer wins → m3
        assert_eq!(find_leaf_pointer_target(&entries), Some("m3".into()));
    }

    /// make_branch and make_rollback both generate unique ids (no collision)
    /// across multiple invocations, since gen_id uses an atomic counter.
    #[test]
    fn make_branch_generates_unique_ids() {
        let a = make_branch("m1", None).unwrap();
        let b = make_branch("m1", None).unwrap();
        assert_ne!(a[0]["id"].as_str(), b[0]["id"].as_str(),
            "consecutive make_branch calls must produce distinct ids");
    }
}
