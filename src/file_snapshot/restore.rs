//! restore_files — 代码恢复（消息+代码联动回滚）
//!
//! 恢复算法：
//! 1. 收集 target_turn 之后所有 ToolSnapshot
//! 2. 按文件路径分组，每个文件取 target_turn 时的最后状态
//! 3. 有 before_hash → 恢复成 before 内容（回到改之前）
//! 4. before_hash = None（文件原本不存在）→ 删除文件
//! 5. 写入 restore_point 快照（记录恢复前磁盘状态，方便撤销）

use super::snapshot::{ToolSnapshot, SnapshotStore};
use serde::{Deserialize, Serialize};

/// 恢复结果
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RestoreResult {
    pub restored_files: Vec<RestoredFile>,
    pub restore_point_id: String,
    pub summary: RestoreSummary,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RestoredFile {
    pub path: String,
    pub action: String,    // "restored" | "deleted" | "skipped"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RestoreSummary {
    pub restored: usize,
    pub deleted: usize,
    pub skipped: usize,
}

/// restore_point 快照（记录恢复前的磁盘状态，用于撤销恢复）
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RestorePoint {
    pub id: String,
    pub turn_id: String,          // 回滚到哪个 turn
    pub timestamp: String,
    pub files: Vec<RestorePointFile>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RestorePointFile {
    pub path: String,
    pub hash: Option<String>,     // 恢复前的磁盘内容 hash（None = 文件不存在）
}

/// 恢复代码到指定 turn 之后（撤销该 turn 之后的所有改动）
///
/// 参数：
/// - store: 快照存储
/// - target_turn_id: 回滚到这个 turn（该 turn 的改动保留，之后的撤销）
pub fn restore_code_to_turn(
    store: &SnapshotStore,
    target_turn_id: &str,
) -> RestoreResult {
    let objects = store.objects();
    let later_snaps = store.load_tool_snapshots_after(target_turn_id);

    // 按文件路径分组（取每个文件第一次出现的状态作为恢复目标）
    use std::collections::HashMap;
    let mut file_targets: HashMap<String, &ToolSnapshot> = HashMap::new();
    for snap in &later_snaps {
        // 取每个文件在 later_snaps 里第一次出现的那条
        // 第一次出现的 before_hash 就是 target_turn 时的状态
        file_targets.entry(snap.path.clone()).or_insert(snap);
    }

    let mut restored_files = Vec::new();
    let mut restore_point_files = Vec::new();
    let mut summary = RestoreSummary::default();

    for (path, snap) in &file_targets {
        // 记录恢复前磁盘状态（restore_point）
        let current_hash = std::fs::read(path).ok()
            .map(|content| objects.write_object(&content).hash);
        restore_point_files.push(RestorePointFile {
            path: path.clone(),
            hash: current_hash.clone(),
        });

        // 恢复
        match &snap.before_hash {
            Some(before_hash) => {
                // 有 before 内容 → 恢复成 before
                match objects.read_object(before_hash) {
                    Some(content) => {
                        if std::fs::write(path, &content).is_ok() {
                            restored_files.push(RestoredFile {
                                path: path.clone(),
                                action: "restored".into(),
                                from_hash: current_hash,
                                to_hash: Some(before_hash.clone()),
                                reason: None,
                            });
                            summary.restored += 1;
                        } else {
                            restored_files.push(RestoredFile {
                                path: path.clone(),
                                action: "skipped".into(),
                                from_hash: current_hash,
                                to_hash: None,
                                reason: Some("write_failed".into()),
                            });
                            summary.skipped += 1;
                        }
                    }
                    None => {
                        // object 被 GC 了
                        restored_files.push(RestoredFile {
                            path: path.clone(),
                            action: "skipped".into(),
                            from_hash: current_hash,
                            to_hash: None,
                            reason: Some("SNAPSHOT_OBJECT_MISSING".into()),
                        });
                        summary.skipped += 1;
                    }
                }
            }
            None => {
                // before_hash = None → 文件原本不存在 → 删除
                if std::fs::remove_file(path).is_ok() {
                    restored_files.push(RestoredFile {
                        path: path.clone(),
                        action: "deleted".into(),
                        from_hash: current_hash,
                        to_hash: None,
                        reason: None,
                    });
                    summary.deleted += 1;
                } else {
                    restored_files.push(RestoredFile {
                        path: path.clone(),
                        action: "skipped".into(),
                        from_hash: current_hash,
                        to_hash: None,
                        reason: Some("delete_failed".into()),
                    });
                    summary.skipped += 1;
                }
            }
        }
    }

    // 生成 restore_point ID
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::time::SystemTime::now().hash(&mut hasher);
    let restore_point_id = format!("rp_{:06x}", hasher.finish() & 0xFFFFFF);

    // 存 restore_point
    let rp = RestorePoint {
        id: restore_point_id.clone(),
        turn_id: target_turn_id.to_string(),
        timestamp: crate::session_jsonl::timestamp_iso(),
        files: restore_point_files,
    };
    save_restore_point(store, &rp);

    RestoreResult {
        restored_files,
        restore_point_id,
        summary,
    }
}

/// 保存 restore_point
fn save_restore_point(store: &SnapshotStore, rp: &RestorePoint) {
    let path = store.snapshots_dir().join("restore").join(format!("{}.json", rp.id));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&path, serde_json::to_string_pretty(rp).unwrap_or_default()).ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_deletes_new_file() {
        let tmp = std::env::temp_dir().join(format!("fs_restore_del_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let store_dir = tmp.join("store");
        let store = SnapshotStore::new_at(store_dir);

        // 模拟 Turn 1 创建了 new.txt（before=None, after=hash("hello")）
        let test_file = tmp.join("new.txt");
        std::fs::write(&test_file, "hello").unwrap();
        let after_hash = store.objects().write_object(b"hello").hash;
        store.save_tool_snapshot(&ToolSnapshot {
            turn_id: "ts_001".into(),
            tool_call_id: "tc_1".into(),
            tool_name: "write".into(),
            path: test_file.to_string_lossy().to_string(),
            before_hash: None,
            after_hash: Some(after_hash),
            timestamp: "2026-07-11T10:00:01Z".into(),
        });

        // 模拟一个更早的 turn（空，作为回滚目标）
        // restore 到 ts_000（不存在 → 返回全部 later）
        let result = restore_code_to_turn(&store, "ts_000");

        // new.txt 应被删除（before_hash=None）
        assert!(!test_file.exists(), "文件应被删除");
        assert_eq!(result.summary.deleted, 1);
        assert!(!result.restore_point_id.is_empty());

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn restore_reverts_modified_file() {
        let tmp = std::env::temp_dir().join(format!("fs_restore_mod_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let store_dir = tmp.join("store");
        let store = SnapshotStore::new_at(store_dir);

        let test_file = tmp.join("main.rs");

        // Turn 0: 文件原始内容 "original"
        // Turn 1: edit 改成 "modified"（before=hash("original"), after=hash("modified")）
        std::fs::write(&test_file, "modified").unwrap();
        let before_hash = store.objects().write_object(b"original").hash;
        let after_hash = store.objects().write_object(b"modified").hash;
        store.save_tool_snapshot(&ToolSnapshot {
            turn_id: "ts_001".into(),
            tool_call_id: "tc_1".into(),
            tool_name: "edit".into(),
            path: test_file.to_string_lossy().to_string(),
            before_hash: Some(before_hash),
            after_hash: Some(after_hash),
            timestamp: "2026-07-11T10:00:02Z".into(),
        });

        // restore 到 ts_000（不存在 → 恢复全部）
        let result = restore_code_to_turn(&store, "ts_000");

        // 文件应恢复成 "original"
        let content = std::fs::read_to_string(&test_file).unwrap();
        assert_eq!(content, "original");
        assert_eq!(result.summary.restored, 1);

        std::fs::remove_dir_all(&tmp).ok();
    }
}
