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
                // before_hash = None，两种情况：
                if snap.before_unknown {
                    // bash/扫描路线：文件原本存在但没存旧内容 → 无法精确回滚，跳过（不可误删！）
                    restored_files.push(RestoredFile {
                        path: path.clone(),
                        action: "skipped".into(),
                        from_hash: current_hash,
                        to_hash: None,
                        reason: Some("before_content_not_captured".into()),
                    });
                    summary.skipped += 1;
                } else {
                    // write/edit 路线：文件原本不存在（新建的）→ 删除以回滚
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

/// 读取 restore_point（undo 回滚用）
fn load_restore_point(store: &SnapshotStore, rp_id: &str) -> Option<RestorePoint> {
    let path = store.snapshots_dir().join("restore").join(format!("{}.json", rp_id));
    let content = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

/// 单文件回滚到指定 tree 的状态（per-file restore，审批 deny 用）
///
/// - target_tree 中有该文件 → 读内容写回（modified）
/// - target_tree 中没有该文件 → 删除（added → undo）
/// - preview=true → 只返回动作不写盘
///
/// 注意：file_path 是绝对路径，cwd 用于相对化后查 tree（tree key 是相对 cwd 的）
pub fn restore_single_file(
    store: &SnapshotStore,
    target_tree_hash: &str,
    file_path: &str,
    cwd: &str,
    preview: bool,
) -> RestoredFile {
    let objects = store.objects();

    // 把绝对路径转成相对 cwd 的 key（tree 里存的是相对路径）
    let rel_key = std::path::Path::new(file_path)
        .strip_prefix(cwd)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| file_path.to_string());

    // 记录恢复前磁盘状态（restore_point 用）
    let current_hash = std::fs::read(file_path).ok()
        .map(|content| objects.write_object(&content).hash);

    // 读 target tree 拿该文件的状态
    let target_tree = super::tree_store::read_tree(objects, target_tree_hash)
        .unwrap_or_default();

    match super::tree_store::get_file_hash(&target_tree, &rel_key) {
        Some(target_hash) => {
            // target tree 中有该文件 → 恢复成 target 内容
            match objects.read_object(target_hash) {
                Some(content) => {
                    if preview {
                        // 预览：只返回意图不写盘
                        RestoredFile {
                            path: file_path.to_string(),
                            action: "would_restore".into(),
                            from_hash: current_hash,
                            to_hash: Some(target_hash.clone()),
                            reason: None,
                        }
                    } else if std::fs::write(file_path, &content).is_ok() {
                        RestoredFile {
                            path: file_path.to_string(),
                            action: "restored".into(),
                            from_hash: current_hash,
                            to_hash: Some(target_hash.clone()),
                            reason: None,
                        }
                    } else {
                        RestoredFile {
                            path: file_path.to_string(),
                            action: "skipped".into(),
                            from_hash: current_hash,
                            to_hash: None,
                            reason: Some("write_failed".into()),
                        }
                    }
                }
                None => {
                    // object 被 GC 了
                    RestoredFile {
                        path: file_path.to_string(),
                        action: "skipped".into(),
                        from_hash: current_hash,
                        to_hash: None,
                        reason: Some("SNAPSHOT_OBJECT_MISSING".into()),
                    }
                }
            }
        }
        None => {
            // target tree 中没有该文件 → 删除（之前 added 的文件回滚到不存在）
            if preview {
                RestoredFile {
                    path: file_path.to_string(),
                    action: "would_delete".into(),
                    from_hash: current_hash,
                    to_hash: None,
                    reason: None,
                }
            } else if std::fs::remove_file(file_path).is_ok() {
                RestoredFile {
                    path: file_path.to_string(),
                    action: "deleted".into(),
                    from_hash: current_hash,
                    to_hash: None,
                    reason: None,
                }
            } else {
                RestoredFile {
                    path: file_path.to_string(),
                    action: "skipped".into(),
                    from_hash: current_hash,
                    to_hash: None,
                    reason: Some("delete_failed".into()),
                }
            }
        }
    }
}

/// 整体回滚到指定 tree（读 tree 所有 path→hash → 逐文件写回，O(文件数)）
///
/// - target_tree 中有的文件 → 写回 target 内容
/// - 当前磁盘有但 target_tree 没有的文件 → 删除（target 之后新增的）
/// - preview=true → 只返回动作不写盘
pub fn restore_to_tree(
    store: &SnapshotStore,
    target_tree_hash: &str,
    cwd: &str,
    preview: bool,
) -> RestoreResult {
    let objects = store.objects();
    let target_tree = super::tree_store::read_tree(objects, target_tree_hash)
        .unwrap_or_default();

    // 扫描当前磁盘状态（拿到当前文件列表）
    let current_scan = super::scanner::scan_dir_fast(cwd);
    let current_paths: std::collections::HashSet<&String> = current_scan.files.keys().collect();

    let mut restored_files = Vec::new();
    let mut restore_point_files = Vec::new();
    let mut summary = RestoreSummary::default();

    // 1. target tree 中有的文件 → 写回 target 内容
    for (path, target_hash) in &target_tree {
        let abs_path = std::path::Path::new(cwd).join(path);
        let abs_str = abs_path.to_string_lossy().to_string();

        // 记录 restore_point（非预览时）
        if !preview {
            let cur_hash = std::fs::read(&abs_path).ok()
                .map(|c| objects.write_object(&c).hash);
            restore_point_files.push(RestorePointFile {
                path: abs_str.clone(),
                hash: cur_hash,
            });
        }

        match objects.read_object(target_hash) {
            Some(content) => {
                if preview {
                    restored_files.push(RestoredFile {
                        path: abs_str,
                        action: "would_restore".into(),
                        from_hash: None,
                        to_hash: Some(target_hash.clone()),
                        reason: None,
                    });
                } else if std::fs::write(&abs_path, &content).is_ok() {
                    restored_files.push(RestoredFile {
                        path: abs_str,
                        action: "restored".into(),
                        from_hash: None,
                        to_hash: Some(target_hash.clone()),
                        reason: None,
                    });
                    summary.restored += 1;
                } else {
                    restored_files.push(RestoredFile {
                        path: abs_str,
                        action: "skipped".into(),
                        from_hash: None,
                        to_hash: None,
                        reason: Some("write_failed".into()),
                    });
                    summary.skipped += 1;
                }
            }
            None => {
                restored_files.push(RestoredFile {
                    path: abs_str,
                    action: "skipped".into(),
                    from_hash: None,
                    to_hash: None,
                    reason: Some("SNAPSHOT_OBJECT_MISSING".into()),
                });
                summary.skipped += 1;
            }
        }
    }

    // 2. 当前磁盘有但 target_tree 没有的文件 → 删除（target 之后新增的）
    //    XL3 安全检查：如果 scan 截断了（超 5000 文件 / 50MB / 深度 10），
    //    current_paths 不完整，删除会误删漏扫的文件 → 跳过整个删除阶段
    if current_scan.truncated {
        // 扫描被截断，无法安全判断哪些文件该删 → 只恢复不删除
        restored_files.push(RestoredFile {
            path: cwd.to_string(),
            action: "skipped".into(),
            from_hash: None,
            to_hash: None,
            reason: Some("scan_truncated_skip_delete".into()),
        });
        // 不进删除循环，直接到 restore_point 生成
    } else {
        for current_path in current_paths {
            if !target_tree.contains_key(current_path) {
                let abs_path = std::path::Path::new(cwd).join(current_path);
                let abs_str = abs_path.to_string_lossy().to_string();

                if !preview {
                    let cur_hash = std::fs::read(&abs_path).ok()
                        .map(|c| objects.write_object(&c).hash);
                    restore_point_files.push(RestorePointFile {
                        path: abs_str.clone(),
                        hash: cur_hash,
                    });
                }

                if preview {
                    restored_files.push(RestoredFile {
                        path: abs_str,
                        action: "would_delete".into(),
                        from_hash: None,
                        to_hash: None,
                        reason: None,
                    });
                } else if std::fs::remove_file(&abs_path).is_ok() {
                    restored_files.push(RestoredFile {
                        path: abs_str,
                        action: "deleted".into(),
                        from_hash: None,
                        to_hash: None,
                        reason: None,
                    });
                    summary.deleted += 1;
                } else {
                    restored_files.push(RestoredFile {
                        path: abs_str,
                        action: "skipped".into(),
                        from_hash: None,
                        to_hash: None,
                        reason: Some("delete_failed".into()),
                    });
                    summary.skipped += 1;
                }
            }
        }
    }

    // 生成 restore_point（非预览时）
    let restore_point_id = if preview {
        String::new()
    } else {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::time::SystemTime::now().hash(&mut hasher);
        let rp_id = format!("rp_{:06x}", hasher.finish() & 0xFFFFFF);

        let rp = RestorePoint {
            id: rp_id.clone(),
            turn_id: target_tree_hash.to_string(),
            timestamp: crate::session_jsonl::timestamp_iso(),
            files: restore_point_files,
        };
        save_restore_point(store, &rp);
        rp_id
    };

    RestoreResult {
        restored_files,
        restore_point_id,
        summary,
    }
}

/// undo 回滚（用 restore_point 恢复到回滚前的磁盘状态）
///
/// restore_point 记录了回滚前每个文件的 hash，undo 把它们写回去。
/// 返回恢复结果（每个文件写回的状态）。
pub fn undo_restore(store: &SnapshotStore, restore_point_id: &str) -> RestoreResult {
    let rp = match load_restore_point(store, restore_point_id) {
        Some(rp) => rp,
        None => {
            return RestoreResult {
                restored_files: vec![RestoredFile {
                    path: String::new(),
                    action: "skipped".into(),
                    from_hash: None,
                    to_hash: None,
                    reason: Some("RESTORE_POINT_NOT_FOUND".into()),
                }],
                restore_point_id: restore_point_id.to_string(),
                summary: RestoreSummary { restored: 0, deleted: 0, skipped: 1 },
            };
        }
    };

    let objects = store.objects();
    let mut restored_files = Vec::new();
    let mut summary = RestoreSummary::default();

    for rp_file in &rp.files {
        match &rp_file.hash {
            Some(hash) => {
                // 回滚前文件存在 → 写回
                match objects.read_object(hash) {
                    Some(content) => {
                        if std::fs::write(&rp_file.path, &content).is_ok() {
                            restored_files.push(RestoredFile {
                                path: rp_file.path.clone(),
                                action: "restored".into(),
                                from_hash: None,
                                to_hash: Some(hash.clone()),
                                reason: None,
                            });
                            summary.restored += 1;
                        } else {
                            restored_files.push(RestoredFile {
                                path: rp_file.path.clone(),
                                action: "skipped".into(),
                                from_hash: None,
                                to_hash: None,
                                reason: Some("write_failed".into()),
                            });
                            summary.skipped += 1;
                        }
                    }
                    None => {
                        restored_files.push(RestoredFile {
                            path: rp_file.path.clone(),
                            action: "skipped".into(),
                            from_hash: None,
                            to_hash: None,
                            reason: Some("SNAPSHOT_OBJECT_MISSING".into()),
                        });
                        summary.skipped += 1;
                    }
                }
            }
            None => {
                // 回滚前文件不存在 → 删除（回滚时新建的文件，undo 时删掉）
                if std::fs::remove_file(&rp_file.path).is_ok() {
                    restored_files.push(RestoredFile {
                        path: rp_file.path.clone(),
                        action: "deleted".into(),
                        from_hash: None,
                        to_hash: None,
                        reason: None,
                    });
                    summary.deleted += 1;
                } else {
                    // 文件已经不存在了，可能已经被删了
                    restored_files.push(RestoredFile {
                        path: rp_file.path.clone(),
                        action: "skipped".into(),
                        from_hash: None,
                        to_hash: None,
                        reason: Some("already_absent".into()),
                    });
                    summary.skipped += 1;
                }
            }
        }
    }

    RestoreResult {
        restored_files,
        restore_point_id: restore_point_id.to_string(),
        summary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// 全局原子计数器，保证并发测试的临时目录唯一（避免 cargo test 多线程下 process::id 相同导致冲突）
    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn unique_id(label: &str) -> String {
        let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        format!("{}_{}_{}_{n}", label, std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos())
    }

    #[test]
    fn restore_deletes_new_file() {
        let tmp = std::env::temp_dir().join(unique_id("fs_restore_del"));
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
            before_unknown: false, // write 路线：before_hash=None = 新建，restore 应删除
            session_id: String::new(),
        });

        // 模拟一个更早的 turn（空，作为回滚目标）
        // restore 到 ts_000（不存在 → 返回全部 later）
        let result = restore_code_to_turn(&store, "ts_000");

        // new.txt 应被删除（before_hash=None 且 before_unknown=false）
        assert!(!test_file.exists(), "文件应被删除");
        assert_eq!(result.summary.deleted, 1);
        assert!(!result.restore_point_id.is_empty());

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn restore_reverts_modified_file() {
        let tmp = std::env::temp_dir().join(unique_id("fs_restore_mod"));
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
            before_unknown: false,
            session_id: String::new(),
        });

        // restore 到 ts_000（不存在 → 恢复全部）
        let result = restore_code_to_turn(&store, "ts_000");

        // 文件应恢复成 "original"
        let content = std::fs::read_to_string(&test_file).unwrap();
        assert_eq!(content, "original");
        assert_eq!(result.summary.restored, 1);

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn restore_bash_modified_file_not_deleted() {
        // 回归测试：bash 路线 before_unknown=true 的文件，restore 时不应被删除
        let tmp = std::env::temp_dir().join(unique_id("fs_restore_bash"));
        std::fs::create_dir_all(&tmp).unwrap();
        let store_dir = tmp.join("store");
        let store = SnapshotStore::new_at(store_dir);

        let test_file = tmp.join("config.rs");
        std::fs::write(&test_file, "modified by bash").unwrap();

        // 模拟 bash 扫描路线：before_hash=None（没存旧内容），before_unknown=true
        let after_hash = store.objects().write_object(b"modified by bash").hash;
        store.save_tool_snapshot(&ToolSnapshot {
            turn_id: "ts_001".into(),
            tool_call_id: "tc_1".into(),
            tool_name: "bash".into(),
            path: test_file.to_string_lossy().to_string(),
            before_hash: None,
            after_hash: Some(after_hash),
            timestamp: "2026-07-11T10:00:03Z".into(),
            before_unknown: true, // bash 路线：没存旧内容
            session_id: String::new(),
        });

        let result = restore_code_to_turn(&store, "ts_000");

        // 文件不应被删除（before_unknown=true → 跳过，不误删）
        assert!(test_file.exists(), "bash 改过的文件 restore 不应被删除");
        assert_eq!(result.summary.skipped, 1, "应 skipped 而非 deleted");
        assert_eq!(result.summary.deleted, 0, "绝不能 deleted");
        let skipped_file = result.restored_files.iter().find(|f| f.path.contains("config.rs"));
        assert!(skipped_file.is_some());
        assert_eq!(skipped_file.unwrap().reason.as_deref(), Some("before_content_not_captured"));

        std::fs::remove_dir_all(&tmp).ok();
    }

    // ── tree-based restore 测试（步骤 3）──

    fn setup_tree_test() -> (std::path::PathBuf, SnapshotStore, String) {
        // 返回 (work_dir, store, baseline_tree_hash)
        // work_dir 和 store 分开存放，避免 scan_dir_fast 扫到 store 文件
        let id = unique_id("fs_tree_restore");
        let base = std::env::temp_dir().join(&id);
        let work_dir = base.join("work");
        std::fs::create_dir_all(&work_dir).unwrap();
        let store = SnapshotStore::new_at(base.join("store"));

        // 建 baseline tree：2 个文件
        let mut files = std::collections::HashMap::new();
        files.insert("a.rs".into(), b"original_a".to_vec());
        files.insert("b.rs".into(), b"original_b".to_vec());
        let (baseline_hash, _) = super::super::tree_store::write_tree(store.objects(), &files);

        // 写到磁盘
        std::fs::write(work_dir.join("a.rs"), "original_a").unwrap();
        std::fs::write(work_dir.join("b.rs"), "original_b").unwrap();

        (work_dir, store, baseline_hash)
    }

    #[test]
    fn restore_single_file_modifies() {
        let (work_dir, store, baseline_hash) = setup_tree_test();

        // 改 a.rs
        std::fs::write(work_dir.join("a.rs"), "modified_a").unwrap();

        // 单文件回滚 a.rs 到 baseline
        let result = restore_single_file(
            &store,
            &baseline_hash,
            work_dir.join("a.rs").to_string_lossy().as_ref(),
            work_dir.to_string_lossy().as_ref(),
            false,
        );
        assert_eq!(result.action, "restored");
        assert_eq!(std::fs::read_to_string(work_dir.join("a.rs")).unwrap(), "original_a");

        // b.rs 不受影响
        assert_eq!(std::fs::read_to_string(work_dir.join("b.rs")).unwrap(), "original_b");

        std::fs::remove_dir_all(work_dir.parent().unwrap()).ok();
    }

    #[test]
    fn restore_single_file_deletes_new_file() {
        let (work_dir, store, baseline_hash) = setup_tree_test();

        // 新建 c.rs（baseline 里没有）
        std::fs::write(work_dir.join("c.rs"), "new_file").unwrap();

        // 回滚 c.rs → baseline 里没有 → 删除
        let result = restore_single_file(
            &store,
            &baseline_hash,
            work_dir.join("c.rs").to_string_lossy().as_ref(),
            work_dir.to_string_lossy().as_ref(),
            false,
        );
        assert_eq!(result.action, "deleted");
        assert!(!work_dir.join("c.rs").exists());

        std::fs::remove_dir_all(work_dir.parent().unwrap()).ok();
    }

    #[test]
    fn restore_single_file_preview_no_write() {
        let (work_dir, store, baseline_hash) = setup_tree_test();

        std::fs::write(work_dir.join("a.rs"), "modified_a").unwrap();

        // 预览模式
        let result = restore_single_file(
            &store,
            &baseline_hash,
            work_dir.join("a.rs").to_string_lossy().as_ref(),
            work_dir.to_string_lossy().as_ref(),
            true, // preview
        );
        assert_eq!(result.action, "would_restore");
        // 文件没变（preview 不写盘）
        assert_eq!(std::fs::read_to_string(work_dir.join("a.rs")).unwrap(), "modified_a");

        std::fs::remove_dir_all(work_dir.parent().unwrap()).ok();
    }

    #[test]
    fn restore_to_tree_overall() {
        let (work_dir, store, baseline_hash) = setup_tree_test();

        // 改 a.rs + 新建 c.rs
        std::fs::write(work_dir.join("a.rs"), "changed").unwrap();
        std::fs::write(work_dir.join("c.rs"), "new").unwrap();

        // 整体回滚到 baseline
        let result = restore_to_tree(&store, &baseline_hash, work_dir.to_string_lossy().as_ref(), false);
        assert!(result.summary.restored >= 2, "a.rs 和 b.rs 应被恢复");
        assert_eq!(result.summary.deleted, 1, "c.rs 应被删除");
        assert!(!result.restore_point_id.is_empty());

        // 验证磁盘
        assert_eq!(std::fs::read_to_string(work_dir.join("a.rs")).unwrap(), "original_a");
        assert_eq!(std::fs::read_to_string(work_dir.join("b.rs")).unwrap(), "original_b");
        assert!(!work_dir.join("c.rs").exists(), "c.rs 应被删除");

        std::fs::remove_dir_all(work_dir.parent().unwrap()).ok();
    }

    #[test]
    fn restore_to_tree_preview_no_write() {
        let (work_dir, store, baseline_hash) = setup_tree_test();

        std::fs::write(work_dir.join("a.rs"), "changed").unwrap();

        let result = restore_to_tree(&store, &baseline_hash, work_dir.to_string_lossy().as_ref(), true);
        // 预览：文件没变
        assert_eq!(std::fs::read_to_string(work_dir.join("a.rs")).unwrap(), "changed");
        assert!(result.restore_point_id.is_empty(), "预览不生成 restore_point");

        std::fs::remove_dir_all(work_dir.parent().unwrap()).ok();
    }

    #[test]
    fn undo_restore_roundtrip() {
        let (work_dir, store, baseline_hash) = setup_tree_test();

        // 改 a.rs + 新建 c.rs
        std::fs::write(work_dir.join("a.rs"), "changed").unwrap();
        std::fs::write(work_dir.join("c.rs"), "new").unwrap();

        // 整体回滚（c.rs 被删，a.rs 恢复成 original_a）
        let result = restore_to_tree(&store, &baseline_hash, work_dir.to_string_lossy().as_ref(), false);
        let rp_id = result.restore_point_id.clone();

        // 此时 a.rs=original_a, b.rs=original_b, c.rs 不存在
        assert!(!work_dir.join("c.rs").exists());

        // undo：restore_point 记录的是回滚前状态（a.rs=changed, c.rs=new）
        // undo 把它们写回去
        let undo_result = undo_restore(&store, &rp_id);
        assert!(undo_result.summary.restored >= 1, "a.rs/c.rs 应被恢复到回滚前内容");

        // 验证磁盘恢复
        assert_eq!(std::fs::read_to_string(work_dir.join("a.rs")).unwrap(), "changed");
        assert!(work_dir.join("c.rs").exists(), "c.rs 应恢复");

        std::fs::remove_dir_all(work_dir.parent().unwrap()).ok();
    }

    /// XL3: full restore 删除手动创建的文件（target_tree 没有但磁盘有的文件）
    #[test]
    fn xl3_full_restore_deletes_manual_files() {
        let (work_dir, store, baseline_hash) = setup_tree_test();

        // 用户手动创建 manual.txt（不经任何工具，target_tree 里没有）
        std::fs::write(work_dir.join("manual.txt"), "user created").unwrap();
        assert!(work_dir.join("manual.txt").exists());

        // full restore 到 baseline（只有 a.rs + b.rs）
        let result = restore_to_tree(&store, &baseline_hash, work_dir.to_string_lossy().as_ref(), false);

        // manual.txt 应被删除（target_tree 没有它）
        assert!(!work_dir.join("manual.txt").exists(),
            "XL3: full restore 应删除 target_tree 没有的手动文件");
        assert!(result.summary.deleted >= 1, "应至少删除 1 个文件（manual.txt）");

        // a.rs / b.rs 恢复成 baseline 内容
        assert_eq!(std::fs::read_to_string(work_dir.join("a.rs")).unwrap(), "original_a");
        assert_eq!(std::fs::read_to_string(work_dir.join("b.rs")).unwrap(), "original_b");

        std::fs::remove_dir_all(work_dir.parent().unwrap()).ok();
    }

    /// XL3: scan 截断时跳过删除阶段（防误删漏扫文件）
    /// 这个测试验证 reason="scan_truncated_skip_delete" 的逻辑路径
    /// （不真的造 5000 文件触发截断，而是验证非截断时删除正常工作）
    #[test]
    fn xl3_restore_to_tree_preserves_target_files() {
        let (work_dir, store, baseline_hash) = setup_tree_test();

        // 改 a.rs + 新建 c.rs（c.rs 不在 baseline）
        std::fs::write(work_dir.join("a.rs"), "changed").unwrap();
        std::fs::write(work_dir.join("c.rs"), "extra").unwrap();

        // restore 到 baseline
        let result = restore_to_tree(&store, &baseline_hash, work_dir.to_string_lossy().as_ref(), false);

        // c.rs 应被删除（不在 baseline）
        assert!(!work_dir.join("c.rs").exists(), "c.rs 不在 baseline 应被删除");
        // a.rs 恢复成 original_a
        assert_eq!(std::fs::read_to_string(work_dir.join("a.rs")).unwrap(), "original_a");
        // 不应有截断跳过标记（文件少，不会截断）
        assert!(!result.restored_files.iter().any(|f| f.reason.as_deref() == Some("scan_truncated_skip_delete")),
            "小文件量不应触发截断跳过");

        std::fs::remove_dir_all(work_dir.parent().unwrap()).ok();
    }
}
