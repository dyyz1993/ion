//! Snapshot 数据结构 + 路线 1 采集逻辑
//!
//! 路线 1：write/edit 工具级 before/after
//! 路线 2：bash 目录扫描（后续在 scanner.rs 实现）

use super::object_store::{ObjectStore, WriteResult};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 路线 1：write/edit 的 before/after 记录
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolSnapshot {
    pub turn_id: String, // 全局唯一 ID（如 "ts_a3f8b2"），不依赖下标
    pub tool_call_id: String,
    pub tool_name: String,           // "write" | "edit" | "bash"
    pub path: String,                // 文件路径（可能 cwd 外）
    pub before_hash: Option<String>, // 执行前内容 hash（None = 文件不存在 OR 未知）
    pub after_hash: Option<String>,  // 执行后内容 hash（None = 文件被删除）
    pub timestamp: String,
    /// before_hash=None 时，区分两种情况：
    /// - false = 文件原本不存在（write 新建）→ restore 时应删除
    /// - true  = bash/扫描路线，文件原本存在但没存旧内容 → restore 时应跳过（不可误删）
    #[serde(default)]
    pub before_unknown: bool,
    /// 所属 session_id（XL4：跨 session 恢复 + 隔离用）
    /// 旧数据无此字段 → 反序列化为空字符串 → loader 视为"未知 session"（向后兼容）
    #[serde(default)]
    pub session_id: String,
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
    pub turn_id: String,
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
    /// bash 工具：记录目录快照（路线 2）
    DirCapture { scan: super::scanner::DirScanResult },
}

/// 快照管理器：管理 ToolSnapshot + DirSnapshot 的存储和查询
pub struct SnapshotStore {
    /// 存储目录（snapshots/ 子目录）
    snapshots_dir: PathBuf,
    /// object store 引用（用于存内容）
    objects: std::sync::Arc<ObjectStore>,
    /// step 序号基线：首次分配时从既有 step 文件取 max(seq)（跨进程重启续号），
    /// 之后进程内单调递增（fix4/h5-sandbox-failover 任务2）
    step_seq_base: std::sync::OnceLock<u64>,
    /// step 序号进程内偏移（与 step_seq_base 相加得到下一个 seq）
    step_seq_counter: std::sync::atomic::AtomicU64,
}

impl SnapshotStore {
    pub fn new(project_key: &str) -> Self {
        let store_dir = crate::paths::file_store_dir(project_key);
        let snapshots_dir = store_dir.join("snapshots");
        std::fs::create_dir_all(snapshots_dir.join("tool")).ok();
        std::fs::create_dir_all(snapshots_dir.join("restore")).ok();
        Self {
            snapshots_dir,
            objects: std::sync::Arc::new(ObjectStore::for_project(project_key)),
            step_seq_base: std::sync::OnceLock::new(),
            step_seq_counter: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// 测试用：直接指定存储目录
    #[cfg(test)]
    pub fn new_at(store_dir: PathBuf) -> Self {
        let snapshots_dir = store_dir.join("snapshots");
        std::fs::create_dir_all(snapshots_dir.join("tool")).ok();
        std::fs::create_dir_all(snapshots_dir.join("restore")).ok();
        let objects = ObjectStore::new_at(store_dir);
        Self {
            snapshots_dir,
            objects: std::sync::Arc::new(objects),
            step_seq_base: std::sync::OnceLock::new(),
            step_seq_counter: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// 快照目录路径（restore_point 存储用）
    pub fn snapshots_dir(&self) -> &std::path::Path {
        &self.snapshots_dir
    }

    /// 获取 object store（用于读写文件内容）
    pub fn objects(&self) -> &ObjectStore {
        &self.objects
    }

    /// 存储工具级快照（路线 1）
    /// XL4: 路径加 session 维度（tool/<session_id>/<turn_id>.jsonl），避免跨 session turn_id 冲突
    pub fn save_tool_snapshot(&self, snap: &ToolSnapshot) {
        let safe_turn = snap.turn_id.replace('/', "_");
        let safe_sess = if snap.session_id.is_empty() {
            "_legacy".to_string() // 旧数据兜底
        } else {
            snap.session_id.replace('/', "_")
        };
        // 新路径：tool/<session_id>/<turn_id>.jsonl
        let dir = self.snapshots_dir.join("tool").join(&safe_sess);
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("{}.jsonl", safe_turn));
        let line = serde_json::to_string(snap).unwrap_or_default();
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = writeln!(f, "{}", line);
        }
    }

    /// 读取指定 turn 的所有工具级快照
    pub fn load_tool_snapshots(&self, turn_id: &str) -> Vec<ToolSnapshot> {
        let safe_name = turn_id.replace('/', "_");
        // 兼容：先找新路径（tool/*/<turn_id>.jsonl），再找老路径（tool/<turn_id>.jsonl）
        let mut all = Vec::new();
        let tool_dir = self.snapshots_dir.join("tool");
        // 新路径：遍历 session 子目录
        if let Ok(sess_dirs) = std::fs::read_dir(&tool_dir) {
            for sess_entry in sess_dirs.flatten() {
                if sess_entry.path().is_dir() {
                    let p = sess_entry.path().join(format!("{}.jsonl", safe_name));
                    if p.exists() {
                        let snaps: Vec<ToolSnapshot> = read_jsonl(&p);
                        all.extend(snaps);
                    }
                }
            }
        }
        // 老路径（兼容旧数据）
        let legacy_path = tool_dir.join(format!("{}.jsonl", safe_name));
        if legacy_path.exists() {
            all.extend(read_jsonl(&legacy_path));
        }
        all
    }

    /// 读取全部工具级快照（按 timestamp 排序）
    /// XL4: 遍历 tool/ 和 tool/<session_id>/ 两层路径（兼容新旧）
    pub fn load_all_tool_snapshots(&self) -> Vec<ToolSnapshot> {
        let mut all = Vec::new();
        let tool_dir = self.snapshots_dir.join("tool");
        if let Ok(entries) = std::fs::read_dir(&tool_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    // 新路径：session 子目录 → 读里面的 .jsonl
                    if let Ok(sub) = std::fs::read_dir(&path) {
                        for f in sub.flatten() {
                            let snaps: Vec<ToolSnapshot> = read_jsonl(&f.path());
                            all.extend(snaps);
                        }
                    }
                } else {
                    // 老路径：直接的 .jsonl 文件
                    let snaps: Vec<ToolSnapshot> = read_jsonl(&path);
                    all.extend(snaps);
                }
            }
        }
        all.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
        all
    }

    /// 读取指定 session 的全部工具级快照（XL4 跨 session 恢复用）
    /// session_id 为空 → 返回全部（兼容旧调用方）
    pub fn load_tool_snapshots_by_session(&self, session_id: &str) -> Vec<ToolSnapshot> {
        if session_id.is_empty() {
            return self.load_all_tool_snapshots();
        }
        let safe_sess = session_id.replace('/', "_");
        let sess_dir = self.snapshots_dir.join("tool").join(&safe_sess);
        let mut all = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&sess_dir) {
            for entry in entries.flatten() {
                let snaps: Vec<ToolSnapshot> = read_jsonl(&entry.path());
                all.extend(snaps);
            }
        }
        all.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
        all
    }

    /// 读取指定 turn 之后的所有工具级快照（restore 用）
    /// after_turn_id 的快照不包含在内
    /// XL4: 优先按 session_id 过滤（减少跨 session 干扰），无 session_id 时回退全量
    pub fn load_tool_snapshots_after(&self, after_turn_id: &str) -> Vec<ToolSnapshot> {
        let all = self.load_all_tool_snapshots();
        // 找到 after_turn_id 对应的 timestamp，返回之后的所有快照
        let cutoff_ts = all
            .iter()
            .find(|s| s.turn_id == after_turn_id)
            .map(|s| s.timestamp.clone());
        match cutoff_ts {
            Some(cutoff) => all.into_iter().filter(|s| s.timestamp > cutoff).collect(),
            None => all, // 找不到 → 返回全部（安全降级）
        }
    }

    /// XL4: 按 session_id 过滤的版本——只返回该 session 中 after_turn_id 之后的快照
    pub fn load_tool_snapshots_after_by_session(
        &self,
        after_turn_id: &str,
        session_id: &str,
    ) -> Vec<ToolSnapshot> {
        let all = self.load_tool_snapshots_by_session(session_id);
        let cutoff_ts = all
            .iter()
            .find(|s| s.turn_id == after_turn_id)
            .map(|s| s.timestamp.clone());
        match cutoff_ts {
            Some(cutoff) => all.into_iter().filter(|s| s.timestamp > cutoff).collect(),
            None => all,
        }
    }

    /// 读取某文件的全部历史（按 timestamp 排序）
    pub fn load_file_history(&self, file_path: &str) -> Vec<ToolSnapshot> {
        let all = self.load_all_tool_snapshots();
        all.into_iter().filter(|s| s.path == file_path).collect()
    }

    // ── tree 快照方法（步骤 2 新增）──

    /// 存储 step-snapshot（每 turn 有变更时写一条）
    /// seq 在此分配（写入顺序权威）：同毫秒写入时排序不再依赖 turn_id 字典序
    pub fn save_step_snapshot(&self, snap: &super::tree_store::StepSnapshot) {
        let mut snap = snap.clone();
        snap.seq = self.next_step_seq();
        let safe_name = snap.turn_id.replace('/', "_");
        let path = self
            .snapshots_dir
            .join("tree")
            .join(format!("{}.json", safe_name));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let json = serde_json::to_string_pretty(&snap).unwrap_or_default();
        let _ = std::fs::write(&path, json);
    }

    /// 下一个 step seq：基线 = 既有 step 文件的最大 seq（跨进程重启续号），
    /// 进程内单调递增。基线只扫一次（OnceLock），竞态下最多多扫一次目录（幂等）。
    fn next_step_seq(&self) -> u64 {
        let base = *self.step_seq_base.get_or_init(|| {
            self.load_all_step_snapshots()
                .iter()
                .map(|s| s.seq)
                .max()
                .unwrap_or(0)
        });
        base + self.step_seq_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
    }

    /// 读取全部 step-snapshot（按 timestamp → seq → turn_id 排序）。
    /// seq 是写入时分配的单调序号：同毫秒写入按真实写入顺序定序（current_tree_hash
    /// 不再可能取到陈旧快照）；旧格式 step 无 seq（default 0）→ 退化为旧
    /// (timestamp, turn_id) 行为，向后兼容。
    pub fn load_all_step_snapshots(&self) -> Vec<super::tree_store::StepSnapshot> {
        let dir = self.snapshots_dir.join("tree");
        let mut all = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                if let Ok(content) = std::fs::read_to_string(entry.path())
                    && let Ok(snap) =
                        serde_json::from_str::<super::tree_store::StepSnapshot>(&content)
                {
                    all.push(snap);
                }
            }
        }
        all.sort_by(|a, b| {
            a.timestamp
                .cmp(&b.timestamp)
                .then_with(|| a.seq.cmp(&b.seq))
                .then_with(|| a.turn_id.cmp(&b.turn_id))
        });
        all
    }

    /// 获取最新的 step-snapshot（当前 tree 状态）
    pub fn latest_step_snapshot(&self) -> Option<super::tree_store::StepSnapshot> {
        self.load_all_step_snapshots().into_iter().last()
    }

    /// 获取最新的 snapshot_tree_hash（当前完整状态）
    pub fn current_tree_hash(&self) -> Option<String> {
        self.latest_step_snapshot().map(|s| s.snapshot_tree_hash)
    }

    /// 按 turn_id 查找对应的 snapshot_tree_hash（XL3 full restore 用）
    /// 从 step-snapshot 序列里找该 turn 的完整磁盘状态 tree
    pub fn find_tree_hash_by_turn_id(&self, turn_id: &str) -> Option<String> {
        let steps = self.load_all_step_snapshots();
        // 精确匹配 turn_id
        if let Some(s) = steps.iter().find(|s| s.turn_id == turn_id) {
            return Some(s.snapshot_tree_hash.clone());
        }
        // fallback：找 turn_id 最接近但不超过的 step-snapshot（该 turn 之后最近一次建树）
        // 按 timestamp 排序已保证，找最后一个 timestamp <= target 的
        let target_ts = steps
            .iter()
            .find(|s| s.turn_id == turn_id)
            .map(|s| s.timestamp.as_str());
        if let Some(ts) = target_ts {
            return steps
                .iter()
                .filter(|s| s.timestamp.as_str() <= ts)
                .next_back()
                .map(|s| s.snapshot_tree_hash.clone());
        }
        // 都找不到 → 返回最新的
        self.current_tree_hash()
    }
}

/// 读 JSONL 文件反序列化
fn read_jsonl<T: for<'de> Deserialize<'de>>(path: &std::path::Path) -> Vec<T> {
    let mut result = Vec::new();
    if let Ok(content) = std::fs::read_to_string(path) {
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
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
            let path = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
            if path.is_empty() {
                return BeforeState::Skip;
            }
            let before_hash = std::fs::read(path)
                .ok()
                .map(|content| store.write_object(&content).hash);
            BeforeState::FileCapture {
                path: path.to_string(),
                before_hash,
            }
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
    turn_id: &str,
    tool_call_id: &str,
    tool_name: &str,
) -> Option<ToolSnapshot> {
    let (path, before_hash) = match before {
        BeforeState::FileCapture { path, before_hash } => (path.clone(), before_hash.clone()),
        _ => return None,
    };

    let after_hash = std::fs::read(&path).ok().map(|content| {
        let result: WriteResult = store.write_object(&content);
        result.hash
    });

    // 内容没变就不记（write 相同内容）
    if before_hash == after_hash {
        return None;
    }

    Some(ToolSnapshot {
        turn_id: turn_id.to_string(),
        tool_call_id: tool_call_id.to_string(),
        tool_name: tool_name.to_string(),
        path,
        before_hash,
        after_hash,
        timestamp: crate::session_jsonl::timestamp_iso(),
        before_unknown: false, // write/edit 路线：before_hash=None 确实是新建
        session_id: String::new(), // XL4: capture 不设，由 save_snap 补
    })
}

/// 在 bash 工具执行后对比目录扫描，生成多个 ToolSnapshot（路线 2）
/// 返回 Vec 是因为一个 bash 命令可能改多个文件
pub fn capture_after_dir(
    before: &BeforeState,
    store: &ObjectStore,
    cwd: &str,
    turn_id: &str,
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
                // 新文件（bash 扫描到的，但 before_scan 没有 → 要么真新建，要么在 before 时已存在但未被扫到）
                let abs_path = std::path::Path::new(cwd).join(path);
                let after_hash = std::fs::read(&abs_path)
                    .ok()
                    .map(|c| store.write_object(&c).hash);
                if let Some(h) = after_hash {
                    snapshots.push(ToolSnapshot {
                        turn_id: turn_id.to_string(),
                        tool_call_id: tool_call_id.to_string(),
                        tool_name: "bash".into(),
                        path: path.clone(),
                        before_hash: None,
                        after_hash: Some(h),
                        timestamp: crate::session_jsonl::timestamp_iso(),
                        before_unknown: true,      // bash 路线没存 before 内容
                        session_id: String::new(), // XL4: 由 save_snap 补
                    });
                }
            }
            Some((b_mtime, b_size)) if mtime != b_mtime || size != b_size => {
                // 修改了
                let abs_path = std::path::Path::new(cwd).join(path);
                let after_hash = std::fs::read(&abs_path)
                    .ok()
                    .map(|c| store.write_object(&c).hash);
                if let Some(h) = after_hash {
                    snapshots.push(ToolSnapshot {
                        turn_id: turn_id.to_string(),
                        tool_call_id: tool_call_id.to_string(),
                        tool_name: "bash".into(),
                        path: path.clone(),
                        before_hash: None, // bash 路线没存 before 内容
                        after_hash: Some(h),
                        timestamp: crate::session_jsonl::timestamp_iso(),
                        before_unknown: true,      // bash 路线没存 before 内容
                        session_id: String::new(), // XL4: 由 save_snap 补
                    });
                }
            }
            _ => {} // 没变
        }
    }

    // 检查删除
    for path in before_scan.files.keys() {
        if !after_scan.files.contains_key(path) {
            snapshots.push(ToolSnapshot {
                turn_id: turn_id.to_string(),
                tool_call_id: tool_call_id.to_string(),
                tool_name: "bash".into(),
                path: path.clone(),
                before_hash: None,
                after_hash: None,
                timestamp: crate::session_jsonl::timestamp_iso(),
                before_unknown: true, // bash 路线没存删前内容，restore 不可误删
                session_id: String::new(), // XL4: 由 save_snap 补
            });
        }
    }

    snapshots
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── step-snapshot 同毫秒写入序（fix4/h5-sandbox-failover 任务2）──

    /// 同毫秒写两个 step：current_tree_hash 必须取后写的（写入顺序权威），
    /// 不能由 turn_id 字典序决定（旧行为：turn_id 反序时取到陈旧快照）。
    #[test]
    fn same_timestamp_steps_order_by_write_seq() {
        let tmp = std::env::temp_dir().join(format!(
            "fs_step_seq_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let store = SnapshotStore::new_at(tmp.clone());
        let ts = "2026-09-15T10:00:00.123Z"; // 人为同毫秒

        // 先写：turn_id 字典序大（"ts_z9"），tree 状态 = stale
        let first = crate::file_snapshot::tree_store::StepSnapshot {
            session_id: "s".into(),
            turn_id: "ts_z9".into(),
            baseline_tree_hash: "h0".into(),
            snapshot_tree_hash: "stale_hash".into(),
            diff: crate::file_snapshot::tree_store::TreeDiff {
                added: vec![],
                modified: vec![],
                deleted: vec![],
            },
            timestamp: ts.into(),
        seq: 0,
        };
        store.save_step_snapshot(&first);
        // 后写：turn_id 字典序小（"ts_a1"），tree 状态 = latest
        let second = crate::file_snapshot::tree_store::StepSnapshot {
            session_id: "s".into(),
            turn_id: "ts_a1".into(),
            baseline_tree_hash: "stale_hash".into(),
            snapshot_tree_hash: "latest_hash".into(),
            diff: crate::file_snapshot::tree_store::TreeDiff {
                added: vec![],
                modified: vec![],
                deleted: vec![],
            },
            timestamp: ts.into(),
        seq: 0,
        };
        store.save_step_snapshot(&second);

        // 旧排序（ts,turn_id）会把 ts_a1 排前 → current_tree_hash = stale_hash（红）
        assert_eq!(
            store.current_tree_hash(),
            Some("latest_hash".to_string()),
            "同毫秒写入时 current_tree_hash 必须取后写的 step"
        );
        let steps = store.load_all_step_snapshots();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].turn_id, "ts_z9", "先写的排前");
        assert_eq!(steps[1].turn_id, "ts_a1", "后写的排后");

        std::fs::remove_dir_all(&tmp).ok();
    }

    /// 向后兼容：旧 step 文件无 seq 字段 → serde default 回落，
    /// 与带 seq 的新 step 混排不炸，旧 step 之间仍按 (timestamp, turn_id) 定序。
    #[test]
    fn legacy_step_without_seq_mixed_with_new() {
        let tmp = std::env::temp_dir().join(format!(
            "fs_step_legacy_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(tmp.join("snapshots/tree")).unwrap();
        // 手写旧格式（无 seq 字段）
        let legacy = r#"{
            "turn_id": "ts_legacy_b",
            "session_id": "s",
            "baseline_tree_hash": "lb",
            "snapshot_tree_hash": "legacy_hash",
            "diff": {"added": [], "modified": [], "deleted": []},
            "timestamp": "2026-09-15T09:00:00.000Z"
        }"#;
        std::fs::write(tmp.join("snapshots/tree/ts_legacy_b.json"), legacy).unwrap();

        let store = SnapshotStore::new_at(tmp.clone());
        // 新格式写入（带 seq）：晚于 legacy 的时间戳
        let new_step = crate::file_snapshot::tree_store::StepSnapshot {
            session_id: "s".into(),
            turn_id: "ts_new_a".into(),
            baseline_tree_hash: "legacy_hash".into(),
            snapshot_tree_hash: "new_hash".into(),
            diff: crate::file_snapshot::tree_store::TreeDiff {
                added: vec![],
                modified: vec![],
                deleted: vec![],
            },
            timestamp: "2026-09-15T10:00:00.000Z".into(),
            seq: 0,
        };
        store.save_step_snapshot(&new_step);

        let steps = store.load_all_step_snapshots();
        assert_eq!(steps.len(), 2, "旧格式必须能被读出（不炸）");
        assert_eq!(steps[0].turn_id, "ts_legacy_b");
        assert_eq!(steps[0].seq, 0, "旧格式 seq 回落 0");
        assert_eq!(steps[1].turn_id, "ts_new_a");
        assert!(steps[1].seq > 0, "新写入 step 分配到 >0 的 seq");
        assert_eq!(store.current_tree_hash(), Some("new_hash".to_string()));

        std::fs::remove_dir_all(&tmp).ok();
    }

    /// 旧格式 step 之间（seq 全 0）仍按 (timestamp, turn_id) 旧行为定序。
    #[test]
    fn legacy_steps_fall_back_to_turn_id_order() {
        let tmp = std::env::temp_dir().join(format!(
            "fs_step_legacy_order_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(tmp.join("snapshots/tree")).unwrap();
        for (tid, hash) in [("ts_b", "h_b"), ("ts_a", "h_a"), ("ts_c", "h_c")] {
            let json = format!(
                r#"{{"turn_id":"{tid}","baseline_tree_hash":"x","snapshot_tree_hash":"{hash}","diff":{{"added":[],"modified":[],"deleted":[]}},"timestamp":"2026-09-15T09:00:00.000Z"}}"#
            );
            std::fs::write(tmp.join(format!("snapshots/tree/{tid}.json")), json).unwrap();
        }
        let store = SnapshotStore::new_at(tmp.clone());
        let steps = store.load_all_step_snapshots();
        let ids: Vec<&str> = steps.iter().map(|s| s.turn_id.as_str()).collect();
        assert_eq!(ids, vec!["ts_a", "ts_b", "ts_c"], "seq 全 0 时退化为 turn_id 字典序");

        std::fs::remove_dir_all(&tmp).ok();
    }

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
        let snap = capture_after(&before, &store, "ts_test01", "tc_test", "write");
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

    /// XL4: 跨 session 隔离 — 不同 session 的快照存到不同子目录
    #[test]
    fn xl4_snapshots_isolated_by_session() {
        let tmp = std::env::temp_dir().join(format!(
            "fs_xl4_iso_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let store = SnapshotStore::new_at(tmp.clone());

        // session A 的快照
        store.save_tool_snapshot(&ToolSnapshot {
            turn_id: "ts_001".into(),
            tool_call_id: "tc_a".into(),
            tool_name: "write".into(),
            path: "a.rs".into(),
            before_hash: None,
            after_hash: Some("hash_a".into()),
            timestamp: "2026-07-13T10:00:01Z".into(),
            before_unknown: false,
            session_id: "sess_A".into(),
        });

        // session B 的快照（同 turn_id "ts_001"，但不同 session）
        store.save_tool_snapshot(&ToolSnapshot {
            turn_id: "ts_001".into(),
            tool_call_id: "tc_b".into(),
            tool_name: "write".into(),
            path: "b.rs".into(),
            before_hash: None,
            after_hash: Some("hash_b".into()),
            timestamp: "2026-07-13T10:00:02Z".into(),
            before_unknown: false,
            session_id: "sess_B".into(),
        });

        // load_by_session("sess_A") 只返回 A 的快照
        let sess_a = store.load_tool_snapshots_by_session("sess_A");
        assert_eq!(sess_a.len(), 1, "sess_A 应只有 1 条快照");
        assert_eq!(sess_a[0].path, "a.rs", "应该是 a.rs");
        assert_eq!(sess_a[0].session_id, "sess_A");

        // load_by_session("sess_B") 只返回 B 的快照
        let sess_b = store.load_tool_snapshots_by_session("sess_B");
        assert_eq!(sess_b.len(), 1, "sess_B 应只有 1 条快照");
        assert_eq!(sess_b[0].path, "b.rs", "应该是 b.rs");
        assert_eq!(sess_b[0].session_id, "sess_B");

        // load_all 返回全部（2 条，跨 session）
        let all = store.load_all_tool_snapshots();
        assert_eq!(all.len(), 2, "全量应返回 2 条（跨 session）");

        std::fs::remove_dir_all(&tmp).ok();
    }

    /// XL4: turn_id 冲突不混淆（不同 session 同 turn_id 存到不同子目录）
    #[test]
    fn xl4_turn_id_collision_no_corruption() {
        let tmp = std::env::temp_dir().join(format!(
            "fs_xl4_coll_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let store = SnapshotStore::new_at(tmp.clone());

        // 两个 session 都用 turn_id="ts_collision"，但内容不同
        for sess in &["sess_X", "sess_Y"] {
            store.save_tool_snapshot(&ToolSnapshot {
                turn_id: "ts_collision".into(),
                tool_call_id: "tc".into(),
                tool_name: "write".into(),
                path: format!("file_{}.rs", sess),
                before_hash: None,
                after_hash: Some(format!("hash_{}", sess)),
                timestamp: "2026-07-13T10:00:00Z".into(),
                before_unknown: false,
                session_id: sess.to_string(),
            });
        }

        // 验证文件存在在各自子目录（没 append 到同一文件）
        let sess_x_snaps = store.load_tool_snapshots_by_session("sess_X");
        let sess_y_snaps = store.load_tool_snapshots_by_session("sess_Y");
        assert_eq!(sess_x_snaps.len(), 1);
        assert_eq!(sess_y_snaps.len(), 1);
        assert_ne!(
            sess_x_snaps[0].path, sess_y_snaps[0].path,
            "不同 session 同 turn_id 的快照不应混淆"
        );

        std::fs::remove_dir_all(&tmp).ok();
    }

    /// XL4: 旧数据兼容（session_id 为空的快照存在 _legacy 子目录，load_all 仍能读）
    #[test]
    fn xl4_legacy_data_compatible() {
        let tmp = std::env::temp_dir().join(format!(
            "fs_xl4_leg_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let store = SnapshotStore::new_at(tmp.clone());

        // 模拟旧数据：session_id 为空
        store.save_tool_snapshot(&ToolSnapshot {
            turn_id: "ts_old".into(),
            tool_call_id: "tc".into(),
            tool_name: "write".into(),
            path: "old.rs".into(),
            before_hash: None,
            after_hash: Some("old_hash".into()),
            timestamp: "2026-07-13T09:00:00Z".into(),
            before_unknown: false,
            session_id: String::new(), // 旧数据无 session_id
        });

        // load_all 应能读到旧数据
        let all = store.load_all_tool_snapshots();
        assert_eq!(all.len(), 1, "旧数据应被 load_all 读到");
        assert_eq!(all[0].path, "old.rs");

        std::fs::remove_dir_all(&tmp).ok();
    }
}
