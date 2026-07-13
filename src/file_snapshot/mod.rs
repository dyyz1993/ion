//! File Snapshot — 双路文件快照系统
//!
//! 路线 1：write/edit 工具级 before/after（100% 精确 diff）
//! 路线 2：bash 目录扫描 + turn_end 仓库内兜底（mtime+size 启发式）
//!
//! 存储约束：content hash 去重 + 100MB 上限 + 分级 GC
//! 性能约束：mtime+size 快速过滤 + 按 turnId 索引
//!
//! 扫描分区：
//! - 仓库目录内：每轮 turn_end 扫描（兜底捕获 bash/外部工具/手动改动）
//! - 仓库目录外：只靠 write/edit 工具拦截（不定期扫描仓库外路径）

pub mod object_store;
pub mod snapshot;
pub mod scanner;
pub mod diff;
pub mod gc;
pub mod restore;
pub mod tree_store;
pub mod approval;

pub use object_store::{ObjectStore, project_key, content_hash};
pub use snapshot::{
    ToolSnapshot, DirSnapshot, DirFileChange, ChangeStatus,
    BeforeState, SnapshotStore, capture_before, capture_after, capture_after_dir,
};
pub use scanner::{scan_dir_fast, is_binary, DirScanResult};
pub use tree_store::{
    TreeEntries, TreeDiff, TreeChangeStatus, StepSnapshot,
    serialize_tree, deserialize_tree, write_tree, read_tree, compute_diff, get_file_hash,
};
pub use approval::{ApprovalManager, ApprovalExtension, ApprovalStatus, FileApproval, PendingFile};
pub use diff::{unified_diff, count_diff};

use crate::agent::extension::{Extension, SessionContext, TurnContext};
use crate::agent::error::AgentResult;
use std::collections::HashMap;
use std::sync::Mutex;

/// File Snapshot 扩展 — 通过 on_tool_execution_start/end + on_turn_end 钩子采集
pub struct FileSnapshotExtension {
    /// 统一存储上下文（拿 cwd / config_root / session_id）
    storage: crate::storage_context::StorageContext,
    /// project key（缓存，从 storage.cwd 算）
    project_key: String,
    /// 快照存储（Arc 共享，RPC 层也能访问）
    store: std::sync::Arc<SnapshotStore>,
    /// 当前 turn 的 before 状态（tool_call_id → BeforeState）
    before_states: Mutex<HashMap<String, BeforeState>>,
    /// 当前 turn 的唯一 ID（如 "ts_a3f8b2"，on_turn_start 时生成）
    current_turn_id: Mutex<String>,
    /// 上一轮 turn_end 的目录扫描结果（用于本轮 turn_end 对比）
    last_scan: Mutex<Option<DirScanResult>>,
    /// baseline tree hash（审批/回滚的参考点，session_start 时建立）
    baseline_tree_hash: Mutex<Option<String>>,
}

/// 生成 6 位随机 hex turn ID（如 "ts_a3f8b2"）
fn gen_turn_id() -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    std::time::SystemTime::now().hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    format!("ts_{:06x}", hasher.finish() & 0xFFFFFF)
}

impl FileSnapshotExtension {
    /// 创建并返回 (extension, store_arc)
    pub fn new_pair(storage: crate::storage_context::StorageContext) -> (Self, std::sync::Arc<SnapshotStore>) {
        let pk = project_key(&storage.cwd);
        let store = std::sync::Arc::new(SnapshotStore::new(&pk));
        let ext = Self {
            storage,
            project_key: pk,
            store: store.clone(),
            before_states: Mutex::new(HashMap::new()),
            current_turn_id: Mutex::new(String::new()),
            last_scan: Mutex::new(None),
            baseline_tree_hash: Mutex::new(None),
        };
        (ext, store)
    }

    /// 兼容旧签名（测试用）
    pub fn new_pair_with_cwd(cwd: &str) -> (Self, std::sync::Arc<SnapshotStore>) {
        Self::new_pair(crate::storage_context::StorageContext::new(cwd, "test", cwd))
    }

    /// 获取快照存储（RPC 查询用）
    pub fn store(&self) -> &SnapshotStore {
        &self.store
    }

    /// 收集当前所有 ToolSnapshot 引用到的 hash（GC 白名单）
    fn collect_active_hashes(&self) -> Vec<String> {
        let snaps = self.store.load_all_tool_snapshots();
        let mut hashes: Vec<String> = Vec::new();
        for snap in &snaps {
            if let Some(h) = &snap.before_hash {
                hashes.push(h.clone());
            }
            if let Some(h) = &snap.after_hash {
                hashes.push(h.clone());
            }
        }
        // 也保护 step-snapshot 引用的 tree hash
        for step in self.store.load_all_step_snapshots() {
            hashes.push(step.baseline_tree_hash);
            hashes.push(step.snapshot_tree_hash);
        }
        hashes.sort();
        hashes.dedup();
        hashes
    }

    /// 扫描 cwd → 读文件内容 → path → Vec<u8>（供 write_tree 用）
    fn scan_to_file_contents(&self) -> HashMap<String, Vec<u8>> {
        let scan = scan_dir_fast(&self.storage.cwd);
        let mut files = HashMap::new();
        for (rel_path, _) in &scan.files {
            let abs_path = std::path::Path::new(&self.storage.cwd).join(rel_path);
            if let Ok(content) = std::fs::read(&abs_path) {
                files.insert(rel_path.clone(), content);
            }
        }
        files
    }
}

#[async_trait::async_trait]
impl Extension for FileSnapshotExtension {
    fn name(&self) -> &str { "file_snapshot" }

    async fn on_session_start(&self, _ctx: &SessionContext) -> AgentResult<()> {
        // session start：建立初始扫描 baseline + baseline tree + 异步触发 GC
        let scan = scan_dir_fast(&self.storage.cwd);
        *self.last_scan.lock().unwrap() = Some(scan);

        // 建立 baseline tree（session start 时的完整文件状态）
        let files = self.scan_to_file_contents();
        let (tree_hash, _) = tree_store::write_tree(self.store.objects(), &files);
        *self.baseline_tree_hash.lock().unwrap() = Some(tree_hash.clone());

        // 写 baseline step-snapshot（让 current_tree_hash 从一开始就有值）
        let step = tree_store::StepSnapshot {
            turn_id: "ts_session_start".to_string(),
            baseline_tree_hash: tree_hash.clone(),
            snapshot_tree_hash: tree_hash,
            diff: tree_store::TreeDiff {
                added: vec![],
                modified: vec![],
                deleted: vec![],
            },
            timestamp: crate::session_jsonl::timestamp_iso(),
        };
        self.store.save_step_snapshot(&step);

        // 异步 GC（不阻塞 agent）
        let active_hashes = self.collect_active_hashes();
        let store = std::sync::Arc::new(ObjectStore::for_project(&self.project_key));
        gc::run_gc_async(store, active_hashes);

        Ok(())
    }

    async fn on_turn_start(
        &self,
        _ctx: &mut TurnContext,
    ) -> AgentResult<()> {
        // 每轮 turn 生成唯一 ID（不依赖下标递增）
        *self.current_turn_id.lock().unwrap() = gen_turn_id();
        Ok(())
    }

    async fn on_turn_end(&self, _ctx: &TurnContext) -> AgentResult<()> {
        // turn_end 仓库内扫描兜底 + tree 快照
        let turn_id = self.current_turn_id.lock().unwrap().clone();
        let prev_scan = self.last_scan.lock().unwrap().take();

        let current_scan = scan_dir_fast(&self.storage.cwd);

        if let Some(before) = prev_scan {
            // 路线 2 delta 采集（保持现有兼容）
            let before_state = BeforeState::DirCapture { scan: before };
            let snaps = capture_after_dir(
                &before_state,
                self.store.objects(),
                &self.storage.cwd,
                &turn_id,
                "turn_end_scan",
            );
            for snap in snaps {
                self.store.save_tool_snapshot(&snap);
            }
        }

        // tree 快照：扫描当前文件 → write_tree → compute_diff(baseline) → 有变更才写 step-snapshot
        let files = self.scan_to_file_contents();
        let (current_tree_hash, _) = tree_store::write_tree(self.store.objects(), &files);

        let baseline = self.baseline_tree_hash.lock().unwrap().clone();
        if let Some(ref baseline_hash) = baseline {
            if &current_tree_hash != baseline_hash {
                // 有变更：算 diff + 写 step-snapshot
                let old_tree = tree_store::read_tree(self.store.objects(), baseline_hash)
                    .unwrap_or_default();
                let new_tree = tree_store::read_tree(self.store.objects(), &current_tree_hash)
                    .unwrap_or_default();
                let diff = tree_store::compute_diff(&old_tree, &new_tree);
                if !diff.is_empty() {
                    let step = tree_store::StepSnapshot {
                        turn_id: turn_id.clone(),
                        baseline_tree_hash: baseline_hash.clone(),
                        snapshot_tree_hash: current_tree_hash.clone(),
                        diff,
                        timestamp: crate::session_jsonl::timestamp_iso(),
                    };
                    self.store.save_step_snapshot(&step);
                }
            }
        }

        // 保存本轮扫描结果，供下一轮对比
        *self.last_scan.lock().unwrap() = Some(current_scan);

        Ok(())
    }

    async fn on_tool_execution_start(
        &self,
        ctx: &crate::agent::extension::ToolExecutionContext,
    ) -> AgentResult<()> {
        let before = capture_before(&ctx.tool_name, &ctx.args, self.store.objects(), &self.storage.cwd);
        if !matches!(before, BeforeState::Skip) {
            self.before_states.lock().unwrap().insert(ctx.tool_call_id.clone(), before);
        }
        Ok(())
    }

    async fn on_tool_execution_end(
        &self,
        ctx: &crate::agent::extension::ToolExecutionContext,
    ) -> AgentResult<()> {
        let turn_id = self.current_turn_id.lock().unwrap().clone();
        let before = self.before_states.lock().unwrap().remove(&ctx.tool_call_id);

        if let Some(before_state) = before {
            match &before_state {
                BeforeState::FileCapture { .. } => {
                    if let Some(snap) = capture_after(
                        &before_state, self.store.objects(),
                        &turn_id, &ctx.tool_call_id, &ctx.tool_name,
                    ) {
                        self.store.save_tool_snapshot(&snap);
                    }
                }
                BeforeState::DirCapture { .. } => {
                    let snaps = capture_after_dir(
                        &before_state, self.store.objects(), &self.storage.cwd,
                        &turn_id, &ctx.tool_call_id,
                    );
                    for snap in snaps {
                        self.store.save_tool_snapshot(&snap);
                    }
                }
                BeforeState::Skip => {}
            }
        }
        Ok(())
    }
}
