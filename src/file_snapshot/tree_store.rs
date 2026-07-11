//! Tree 对象存储 — 完整状态快照（对标 pi writeTree/readTree）
//!
//! tree 对象 = 扁平的 `path\0hash\npath\0hash...` 映射
//! 存在 ObjectStore 的 objects/ 里（和 file 对象统一存储）
//! 相同文件集 → 相同 hash → 内容寻址去重
//!
//! 每个 turn 有变更时写一个 step-snapshot（baseline_tree_hash + snapshot_tree_hash + diff）
//! 回滚时读 tree 写回，O(1)，不需要回放 delta 链

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use super::object_store::ObjectStore;

/// tree 条目：path → content_hash
pub type TreeEntries = HashMap<String, String>;

/// tree 对象的序列化格式：`path\0hash\npath\0hash...`（排序后 join）
/// 排序是为了让相同文件集产生相同 hash（顺序无关化）
pub fn serialize_tree(entries: &TreeEntries) -> String {
    let mut pairs: Vec<(&String, &String)> = entries.iter().collect();
    pairs.sort_by(|a, b| a.0.cmp(b.0));
    pairs.iter()
        .map(|(path, hash)| format!("{}\0{}", path, hash))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 反序列化 tree 对象
pub fn deserialize_tree(data: &str) -> TreeEntries {
    let mut entries = HashMap::new();
    for line in data.lines() {
        if let Some(idx) = line.find('\0') {
            let path = &line[..idx];
            let hash = &line[idx + 1..];
            entries.insert(path.to_string(), hash.to_string());
        }
    }
    entries
}

/// 把扫描结果写成 tree 对象（每个 file 内容先存 object_store，再拼 tree 数据再存）
/// 返回 tree_hash + entries
pub fn write_tree(store: &ObjectStore, files: &HashMap<String, Vec<u8>>) -> (String, TreeEntries) {
    let mut entries = HashMap::new();
    for (path, content) in files {
        let result = store.write_object(content);
        entries.insert(path.clone(), result.hash);
    }
    let tree_data = serialize_tree(&entries);
    let tree_hash = store.write_object(tree_data.as_bytes()).hash;
    (tree_hash, entries)
}

/// 读 tree 对象，返回 path → hash 映射
pub fn read_tree(store: &ObjectStore, tree_hash: &str) -> Option<TreeEntries> {
    let data = store.read_object_text(tree_hash)?;
    Some(deserialize_tree(&data))
}

/// 变更类型
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TreeChangeStatus {
    Added,
    Modified,
    Deleted,
}

/// tree 之间的 diff 结果
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TreeDiff {
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub deleted: Vec<String>,
}

impl TreeDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.modified.is_empty() && self.deleted.is_empty()
    }

    pub fn total(&self) -> usize {
        self.added.len() + self.modified.len() + self.deleted.len()
    }
}

/// 对比两棵 tree 的 per-file hash → {added, modified, deleted}
pub fn compute_diff(old: &TreeEntries, new: &TreeEntries) -> TreeDiff {
    let mut diff = TreeDiff {
        added: Vec::new(),
        modified: Vec::new(),
        deleted: Vec::new(),
    };

    for (path, new_hash) in new {
        match old.get(path) {
            None => diff.added.push(path.clone()),
            Some(old_hash) if old_hash != new_hash => diff.modified.push(path.clone()),
            _ => {}
        }
    }
    for path in old.keys() {
        if !new.contains_key(path) {
            diff.deleted.push(path.clone());
        }
    }

    diff.added.sort();
    diff.modified.sort();
    diff.deleted.sort();
    diff
}

/// 获取某文件在某 tree 中的 content hash
pub fn get_file_hash<'a>(tree: &'a TreeEntries, path: &str) -> Option<&'a String> {
    tree.get(path)
}

/// step-snapshot entry（每个有变更的 turn 写一条）
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StepSnapshot {
    /// 快照 ID（turn_id）
    pub turn_id: String,
    /// 上一轮的 tree hash（baseline）
    pub baseline_tree_hash: String,
    /// 本轮的 tree hash
    pub snapshot_tree_hash: String,
    /// 本轮 diff（baseline → snapshot）
    pub diff: TreeDiff,
    /// 时间戳
    pub timestamp: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_store() -> (std::path::PathBuf, ObjectStore) {
        let tmp = std::env::temp_dir().join(format!("fs_tree_test_{}_{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos()));
        let store = ObjectStore::new_at(tmp.clone());
        (tmp, store)
    }

    #[test]
    fn tree_serialize_deserialize_roundtrip() {
        let mut entries = HashMap::new();
        entries.insert("src/a.rs".into(), "hash_a".into());
        entries.insert("src/b.rs".into(), "hash_b".into());
        let data = serialize_tree(&entries);
        let back = deserialize_tree(&data);
        assert_eq!(back.len(), 2);
        assert_eq!(back.get("src/a.rs"), Some(&"hash_a".to_string()));
    }

    #[test]
    fn tree_same_content_same_hash() {
        let (tmp, store) = tmp_store();
        let mut files = HashMap::new();
        files.insert("a.txt".into(), b"hello".to_vec());

        let (hash1, _) = write_tree(&store, &files);
        let (hash2, _) = write_tree(&store, &files);  // 相同内容
        assert_eq!(hash1, hash2, "相同文件集应产生相同 tree hash");

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn tree_different_content_different_hash() {
        let (tmp, store) = tmp_store();
        let mut files1 = HashMap::new();
        files1.insert("a.txt".into(), b"v1".to_vec());
        let (hash1, _) = write_tree(&store, &files1);

        let mut files2 = HashMap::new();
        files2.insert("a.txt".into(), b"v2".to_vec());
        let (hash2, _) = write_tree(&store, &files2);

        assert_ne!(hash1, hash2, "不同内容应产生不同 tree hash");

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn compute_diff_added_modified_deleted() {
        let mut old = HashMap::new();
        old.insert("a.rs".into(), "h1".into());   // 不变
        old.insert("b.rs".into(), "h2".into());   // 会修改
        old.insert("c.rs".into(), "h3".into());   // 会删除

        let mut new = HashMap::new();
        new.insert("a.rs".into(), "h1".into());   // 不变
        new.insert("b.rs".into(), "h2_new".into()); // 修改
        new.insert("d.rs".into(), "h4".into());   // 新增

        let diff = compute_diff(&old, &new);
        assert_eq!(diff.added, vec!["d.rs"]);
        assert_eq!(diff.modified, vec!["b.rs"]);
        assert_eq!(diff.deleted, vec!["c.rs"]);
        assert_eq!(diff.total(), 3);
    }

    #[test]
    fn compute_diff_identical_empty() {
        let mut tree = HashMap::new();
        tree.insert("a.rs".into(), "h1".into());

        let diff = compute_diff(&tree, &tree);
        assert!(diff.is_empty());
    }

    #[test]
    fn read_tree_returns_entries() {
        let (tmp, store) = tmp_store();
        let mut files = HashMap::new();
        files.insert("x.rs".into(), b"content x".to_vec());
        files.insert("y.rs".into(), b"content y".to_vec());

        let (tree_hash, _) = write_tree(&store, &files);
        let entries = read_tree(&store, &tree_hash).expect("tree 应能读回");

        assert_eq!(entries.len(), 2);
        assert!(entries.contains_key("x.rs"));
        assert!(entries.contains_key("y.rs"));

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn get_file_hash_lookup() {
        let mut tree = HashMap::new();
        tree.insert("a.rs".into(), "hash_a".into());
        assert_eq!(get_file_hash(&tree, "a.rs"), Some(&"hash_a".to_string()));
        assert_eq!(get_file_hash(&tree, "missing.rs"), None);
    }

    // ── 空间膨胀验证（核心：随时间增长是否线性，不是指数）──

    #[test]
    fn space_growth_linear_not_exponential() {
        // 模拟长会话：100 轮，每轮改 5 个文件（10 个文件池中循环改）
        // 验证 object 数和存储大小是线性增长，不是 N × turn × files
        let (tmp, store) = tmp_store();

        let base_files: Vec<String> = (0..10).map(|i| format!("src/f{}.rs", i)).collect();

        // 先建一个 10 文件的 baseline tree
        let mut files0 = HashMap::new();
        for path in &base_files {
            files0.insert(path.clone(), format!("v0_{}", path).into_bytes());
        }
        let (_baseline_hash, _) = write_tree(&store, &files0);
        let baseline_count = store.list_objects().len();
        let baseline_size = store.store_size();

        // 100 轮，每轮改 5 个文件（改内容）
        for turn in 0..100 {
            let mut files = HashMap::new();
            for (i, path) in base_files.iter().enumerate() {
                // 只改前 5 个，后 5 个不变
                let content = if i < 5 {
                    format!("v{}_{}", turn, path).into_bytes()
                } else {
                    format!("v0_{}", path).into_bytes()
                };
                files.insert(path.clone(), content);
            }
            let (_hash, _) = write_tree(&store, &files);
        }

        let final_count = store.list_objects().len();
        let final_size = store.store_size();

        // 断言：
        // 1. object 数应远小于 100 轮 × 10 文件 = 1000
        //    后 5 个文件每轮不变（内容相同→hash 相同→不产生新 object）
        //    前 5 个文件每轮变 → 100 轮 × 5 = 500 个 file object
        //    tree 对象：100 轮 + 1 baseline = 101 个 tree（每轮 tree 内容不同）
        //    理论上限约 601，但 tree 去重可能更少
        println!("baseline: {} objects, {} bytes", baseline_count, baseline_size);
        println!("after 100 turns: {} objects, {} bytes", final_count, final_size);
        println!("growth: {}x objects, {}x size",
            final_count as f64 / baseline_count.max(1) as f64,
            final_size as f64 / baseline_size.max(1) as f64);

        // 核心断言：不是指数增长
        // 100 轮 × 10 文件如果是全量复制 = 1000 object
        // content-addressable 去重后应远小于此
        assert!(final_count < 1000,
            "object 数 ({}) 应远小于全量复制的 1000（去重生效）", final_count);

        // 线性增长验证：前 5 个文件每轮 1 个新 object = ~500 + 101 tree ≈ 601
        // 允许一定误差，但不应超过 700
        assert!(final_count < 700,
            "object 数 ({}) 应接近线性 ~600，不应指数膨胀", final_count);

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn no_change_turn_zero_overhead() {
        // 核心断言：连续 N 轮不改动文件 → tree hash 相同 → 不产生新 object
        let (tmp, store) = tmp_store();

        let mut files = HashMap::new();
        files.insert("a.rs".into(), b"stable content".to_vec());
        files.insert("b.rs".into(), b"also stable".to_vec());

        let (hash1, _) = write_tree(&store, &files);
        let count1 = store.list_objects().len();

        // 连续 50 轮写相同的 tree
        for _ in 0..50 {
            let (h, _) = write_tree(&store, &files);
            assert_eq!(h, hash1, "无变更 turn 的 tree hash 应相同");
        }
        let count50 = store.list_objects().len();

        // object 数完全不变（零开销）
        assert_eq!(count1, count50,
            "50 轮无变更 turn 后 object 数应不变 ({} → {})：零开销",
            count1, count50);

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn gc_reclaims_after_snapshot_removal() {
        // 验证 GC 能回收不再被引用的 object
        // 注：当前 GC 用 active_hashes 白名单，模拟"只保护当前 tree 引用的 file"
        let (tmp, store) = tmp_store();

        // 写 v1 + v2 两版文件
        let mut files_v1 = HashMap::new();
        files_v1.insert("a.rs".into(), b"v1".to_vec());
        let (_hash_v1, _) = write_tree(&store, &files_v1);

        let mut files_v2 = HashMap::new();
        files_v2.insert("a.rs".into(), b"v2".to_vec());
        let (hash_v2, _) = write_tree(&store, &files_v2);

        let count_before_gc = store.list_objects().len();

        // 收集 v2 tree 引用的 hash 作为 active（模拟"只保留最新快照"）
        let v2_tree = read_tree(&store, &hash_v2).unwrap();
        let mut active: Vec<String> = v2_tree.values().cloned().collect();
        active.push(hash_v2.clone());

        // 手动 GC（删除不在 active 里的 object）
        let all = store.list_objects();
        let active_set: std::collections::HashSet<&String> = active.iter().collect();
        for hash in &all {
            if !active_set.contains(hash) && *hash != hash_v2 {
                store.delete_object(hash);
            }
        }

        let count_after_gc = store.list_objects().len();
        println!("before GC: {} objects, after GC: {} objects", count_before_gc, count_after_gc);
        assert!(count_after_gc < count_before_gc,
            "GC 应回收不再引用的 object（v1 内容 + v1 tree）");

        // v2 tree 仍可读（active 保护的）
        let v2_tree_back = read_tree(&store, &hash_v2);
        assert!(v2_tree_back.is_some(), "GC 后 active tree 应仍可读");

        std::fs::remove_dir_all(&tmp).ok();
    }
}
