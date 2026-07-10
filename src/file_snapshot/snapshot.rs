//! Snapshot 数据结构 + 路线 1 采集逻辑
//!
//! 路线 1：write/edit 工具级 before/after
//! 路线 2：bash 目录扫描（后续在 scanner.rs 实现）

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use super::object_store::{ObjectStore, WriteResult};

/// 路线 1：write/edit 的 before/after 记录
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolSnapshot {
    pub turn_id: u32,
    pub tool_call_id: String,
    pub tool_name: String,         // "write" | "edit"
    pub path: String,              // 文件路径（可能 cwd 外）
    pub before_hash: Option<String>, // 执行前内容 hash（None = 文件不存在）
    pub after_hash: Option<String>,  // 执行后内容 hash（None = 文件被删除）
    pub timestamp: String,
}

/// 路线 2：目录扫描的文件变更
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DirFileChange {
    pub path: String,
    pub status: ChangeStatus,
    pub before_hash: Option<String>,
    pub after_hash: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ChangeStatus {
    Added,
    Modified,
    Deleted,
}

/// 目录扫描快照（路线 2）
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DirSnapshot {
    pub turn_id: u32,
    pub tool_call_id: String,
    pub changed_files: Vec<DirFileChange>,
    pub truncated: bool,
}

/// before 状态（工具执行前捕获）
pub enum BeforeState {
    /// 不需要采集（非 write/edit/bash 工具）
    Skip,
    /// write/edit 工具：记录单文件 before
    FileCapture {
        path: String,
        before_hash: Option<String>,
    },
    /// bash 工具：记录目录快照（路线 2，后续实现）
    #[allow(dead_code)]
    DirCapture {
        scan: super::scanner::DirScanResult,
    },
}

/// 快照管理器：管理 ToolSnapshot + DirSnapshot 的存储和查询
pub struct SnapshotStore {
    /// 存储目录（snapshots/ 子目录）
    snapshots_dir: PathBuf,
    /// object store 引用（用于存内容）
    #[allow(dead_code)]
    objects: std::sync::Arc<ObjectStore>,
}

impl SnapshotStore {
    pub fn new(project_key: &str) -> Self {
        let store_dir = crate::paths::file_store_dir(project_key);
        let snapshots_dir = store_dir.join("snapshots");
        std::fs::create_dir_all(snapshots_dir.join("tool")).ok();
        std::fs::create_dir_all(snapshots_dir.join("turn")).ok();
        Self {
            snapshots_dir,
            objects: std::sync::Arc::new(ObjectStore::for_project(project_key)),
        }
    }

    /// 获取 object store（用于读写文件内容）
    pub fn objects(&self) -> &ObjectStore {
        &self.objects
    }

    /// 存储工具级快照（路线 1）
    pub fn save_tool_snapshot(&self, snap: &ToolSnapshot) {
        let path = self.snapshots_dir.join("tool").join(format!("{}.jsonl", snap.turn_id));
        let line = serde_json::to_string(snap).unwrap_or_default();
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            let _ = writeln!(f, "{}", line);
        }
    }

    /// 读取指定 turn 的所有工具级快照
    pub fn load_tool_snapshots(&self, turn_id: u32) -> Vec<ToolSnapshot> {
        let path = self.snapshots_dir.join("tool").join(format!("{}.jsonl", turn_id));
        read_jsonl(&path)
    }

    /// 读取 turn 范围内的所有工具级快照
    pub fn load_tool_snapshots_range(&self, from: Option<u32>, to: Option<u32>) -> Vec<ToolSnapshot> {
        let mut all = Vec::new();
        // 扫描 tool/ 目录下所有 .jsonl 文件
        let dir = self.snapshots_dir.join("tool");
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let fname = entry.file_name().to_string_lossy().to_string();
                if let Some(turn_str) = fname.strip_suffix(".jsonl") {
                    if let Ok(turn) = turn_str.parse::<u32>() {
                        let in_range = from.map_or(true, |f| turn >= f)
                            && to.map_or(true, |t| turn <= t);
                        if in_range {
                            all.extend(self.load_tool_snapshots(turn));
                        }
                    }
                }
            }
        }
        all.sort_by_key(|s| s.turn_id);
        all
    }

    /// 读取某文件的全部历史（按 turn 排序）
    pub fn load_file_history(&self, file_path: &str) -> Vec<ToolSnapshot> {
        // 扫描所有 turn 的 tool snapshots，过滤出指定 path
        let dir = self.snapshots_dir.join("tool");
        let mut history = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let snaps: Vec<ToolSnapshot> = read_jsonl(&entry.path());
                for snap in snaps {
                    if snap.path == file_path {
                        history.push(snap);
                    }
                }
            }
        }
        history.sort_by_key(|s| s.turn_id);
        history
    }
}

/// 读 JSONL 文件反序列化
fn read_jsonl<T: for<'de> Deserialize<'de>>(path: &std::path::Path) -> Vec<T> {
    let mut result = Vec::new();
    if let Ok(content) = std::fs::read_to_string(path) {
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() { continue; }
            if let Ok(item) = serde_json::from_str(line) {
                result.push(item);
            }
        }
    }
    result
}

// ─── 路线 1 采集逻辑 ────────────────────────────────────────────

/// 在工具执行前采集 before 状态
pub fn capture_before(
    tool_name: &str,
    args: &serde_json::Value,
    store: &ObjectStore,
    cwd: &str,
) -> BeforeState {
    match tool_name {
        "write" | "edit" => {
            let path = args.get("file_path")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if path.is_empty() {
                return BeforeState::Skip;
            }
            let before_hash = std::fs::read(path).ok()
                .map(|content| store.write_object(&content).hash);
            BeforeState::FileCapture { path: path.to_string(), before_hash }
        }
        "bash" => {
            // 路线 2：bash 前扫目录（只 stat mtime+size，不读内容）
            let scan = super::scanner::scan_dir_fast(cwd);
            BeforeState::DirCapture { scan }
        }
        _ => BeforeState::Skip,
    }
}

/// 在工具执行后采集 after 状态，生成 ToolSnapshot（路线 1：write/edit）
pub fn capture_after(
    before: &BeforeState,
    store: &ObjectStore,
    turn_id: u32,
    tool_call_id: &str,
    tool_name: &str,
) -> Option<ToolSnapshot> {
    let (path, before_hash) = match before {
        BeforeState::FileCapture { path, before_hash } => (path.clone(), before_hash.clone()),
        _ => return None,
    };

    let after_hash = std::fs::read(&path).ok()
        .map(|content| {
            let result: WriteResult = store.write_object(&content);
            result.hash
        });

    // 内容没变就不记（write 相同内容）
    if before_hash == after_hash {
        return None;
    }

    Some(ToolSnapshot {
        turn_id,
        tool_call_id: tool_call_id.to_string(),
        tool_name: tool_name.to_string(),
        path,
        before_hash,
        after_hash,
        timestamp: crate::session_jsonl::timestamp_iso(),
    })
}

/// 在 bash 工具执行后对比目录扫描，生成多个 ToolSnapshot（路线 2）
/// 返回 Vec 是因为一个 bash 命令可能改多个文件
pub fn capture_after_dir(
    before: &BeforeState,
    store: &ObjectStore,
    cwd: &str,
    turn_id: u32,
    tool_call_id: &str,
) -> Vec<ToolSnapshot> {
    let before_scan = match before {
        BeforeState::DirCapture { scan } => scan,
        _ => return vec![],
    };

    let after_scan = super::scanner::scan_dir_fast(cwd);
    let mut snapshots = Vec::new();

    // 检查新增 + 修改
    for (path, (mtime, size)) in &after_scan.files {
        match before_scan.files.get(path) {
            None => {
                // 新文件
                let abs_path = std::path::Path::new(cwd).join(path);
                let after_hash = std::fs::read(&abs_path).ok()
                    .map(|c| store.write_object(&c).hash);
                if let Some(h) = after_hash {
                    snapshots.push(ToolSnapshot {
                        turn_id,
                        tool_call_id: tool_call_id.to_string(),
                        tool_name: "bash".into(),
                        path: path.clone(),
                        before_hash: None,
                        after_hash: Some(h),
                        timestamp: crate::session_jsonl::timestamp_iso(),
                    });
                }
            }
            Some((b_mtime, b_size)) if mtime != b_mtime || size != b_size => {
                // 修改了
                let abs_path = std::path::Path::new(cwd).join(path);
                let after_hash = std::fs::read(&abs_path).ok()
                    .map(|c| store.write_object(&c).hash);
                if let Some(h) = after_hash {
                    snapshots.push(ToolSnapshot {
                        turn_id,
                        tool_call_id: tool_call_id.to_string(),
                        tool_name: "bash".into(),
                        path: path.clone(),
                        before_hash: None, // bash 路线没存 before 内容
                        after_hash: Some(h),
                        timestamp: crate::session_jsonl::timestamp_iso(),
                    });
                }
            }
            _ => {} // 没变
        }
    }

    // 检查删除
    for (path, _) in &before_scan.files {
        if !after_scan.files.contains_key(path) {
            snapshots.push(ToolSnapshot {
                turn_id,
                tool_call_id: tool_call_id.to_string(),
                tool_name: "bash".into(),
                path: path.clone(),
                before_hash: None,
                after_hash: None,
                timestamp: crate::session_jsonl::timestamp_iso(),
            });
        }
    }

    snapshots
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_write_new_file() {
        let tmp = std::env::temp_dir().join(format!("fs_cap_test_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let test_file = tmp.join("new.txt");

        // 用 new_at 创建 store（测试专用，指定临时目录）
        let store = ObjectStore::new_at(tmp.join("store"));

        // before: 文件不存在
        let args = serde_json::json!({"file_path": test_file.to_string_lossy()});
        let before = capture_before("write", &args, &store, tmp.to_string_lossy().as_ref());
        match &before {
            BeforeState::FileCapture { before_hash, .. } => {
                assert!(before_hash.is_none(), "新文件 before_hash 应为 None");
            }
            _ => panic!("应为 FileCapture"),
        }

        // 模拟 write
        std::fs::write(&test_file, "new content").unwrap();

        // after
        let snap = capture_after(&before, &store, 1, "tc_test", "write");
        assert!(snap.is_some(), "应生成 snapshot");
        let snap = snap.unwrap();
        assert_eq!(snap.status_or_default(), "added");
        assert!(snap.before_hash.is_none());
        assert!(snap.after_hash.is_some());

        std::fs::remove_dir_all(&tmp).ok();
    }

    impl ToolSnapshot {
        fn status_or_default(&self) -> &str {
            match (&self.before_hash, &self.after_hash) {
                (None, Some(_)) => "added",
                (Some(_), None) => "deleted",
                (Some(_), Some(_)) => "modified",
                _ => "unchanged",
            }
        }
    }
}
