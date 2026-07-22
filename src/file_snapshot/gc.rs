//! GC — 垃圾回收
//!
//! 启动时跑一次，异步执行不阻塞 agent。
//! 分级策略：超 100MB → 删 7天前 → 删 1天前 → 可达性分析

use super::object_store::ObjectStore;
use std::sync::Arc;

const STORE_LIMIT: u64 = 100 * 1024 * 1024; // 100MB

/// 执行 GC（启动时调用）
/// active_hashes: 当前会话用到的所有 hash（白名单保护）
pub fn enforce_limit(store: &ObjectStore, active_hashes: &[String]) {
    let size = store.store_size();
    if size <= STORE_LIMIT {
        return; // 没超，跳过
    }

    tracing::info!("[file-snapshot] GC triggered: {}MB > {}MB limit",
        size / 1024 / 1024, STORE_LIMIT / 1024 / 1024);

    // 第 1 步：删 7 天前的（保护 active）
    prune_old_objects(store, 7 * 24 * 3600, active_hashes);
    let size1 = store.store_size();
    if size1 <= STORE_LIMIT {
        tracing::info!("[file-snapshot] GC step 1 done: {}MB", size1 / 1024 / 1024);
        return;
    }

    // 第 2 步：删 1 天前的
    prune_old_objects(store, 24 * 3600, active_hashes);
    let size2 = store.store_size();
    if size2 <= STORE_LIMIT {
        tracing::info!("[file-snapshot] GC step 2 done: {}MB", size2 / 1024 / 1024);
        return;
    }

    // 第 3 步：可达性分析（从 active 出发，删不可达的）
    gc_unreachable(store, active_hashes);
    let size3 = store.store_size();
    tracing::info!("[file-snapshot] GC step 3 done: {}MB", size3 / 1024 / 1024);
}

/// 删超过 max_age_secs 的 object（保护 active_hashes）
fn prune_old_objects(store: &ObjectStore, max_age_secs: u64, active_hashes: &[String]) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let cutoff = now.saturating_sub(max_age_secs);

    let all_objects = store.list_objects();
    for hash in &all_objects {
        if active_hashes.contains(hash) {
            continue; // 保护活跃 object
        }
        // 检查 createdAt
        if let Some(created) = get_object_created_at(store, hash) && created < cutoff {
            store.delete_object(hash);
        }
    }
}

/// 可达性分析：从 active_hashes 出发，删除任何不在 active 集合里的 object
fn gc_unreachable(store: &ObjectStore, active_hashes: &[String]) {
    let all_objects = store.list_objects();
    let active_set: std::collections::HashSet<&String> = active_hashes.iter().collect();

    for hash in &all_objects {
        if !active_set.contains(hash) {
            store.delete_object(hash);
        }
    }
}

/// 读取 object 的创建时间（Unix 时间戳），用 object 文件本身的 mtime
fn get_object_created_at(store: &ObjectStore, hash: &str) -> Option<u64> {
    let object_path = store.object_path(hash);
    let metadata = std::fs::metadata(&object_path).ok()?;
    let mtime = metadata.modified().ok()?;
    mtime.duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_secs())
}

/// 异步执行 GC（启动时 void 调用）
pub fn run_gc_async(store: Arc<ObjectStore>, active_hashes: Vec<String>) {
    std::thread::spawn(move || {
        enforce_limit(&store, &active_hashes);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enforce_limit_under_threshold() {
        let tmp = std::env::temp_dir().join(format!("fs_gc_test_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let store = ObjectStore::new_at(tmp.join("store"));

        // 写一些内容（远小于 100MB）
        store.write_object(b"hello");
        store.write_object(b"world");

        let active = store.list_objects();
        enforce_limit(&store, &active);

        // 没超限制，应全部保留
        let remaining = store.list_objects();
        assert!(remaining.len() >= 2, "小数据不应被 GC");

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn gc_removes_unreachable() {
        let tmp = std::env::temp_dir().join(format!("fs_gc_unreach_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let store = ObjectStore::new_at(tmp.join("store"));

        // 写 3 个 object
        let r1 = store.write_object(b"aaa");
        let r2 = store.write_object(b"bbb");
        let _r3 = store.write_object(b"ccc");

        // 只保护 r1 和 r2
        let active = vec![r1.hash.clone(), r2.hash.clone()];
        gc_unreachable(&store, &active);

        let remaining = store.list_objects();
        assert!(remaining.contains(&r1.hash), "r1 应保留");
        assert!(remaining.contains(&r2.hash), "r2 应保留");
        assert!(!remaining.iter().any(|h| h.starts_with(&_r3.hash[..2]) && h == &_r3.hash),
            "r3 不可达应被删");

        std::fs::remove_dir_all(&tmp).ok();
    }
}
