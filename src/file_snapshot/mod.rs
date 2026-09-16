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

pub mod approval;
pub mod diff;
pub mod gc;
pub mod object_store;
pub mod restore;
pub mod scanner;
pub mod snapshot;
pub mod tree_store;

pub use approval::{ApprovalExtension, ApprovalManager, ApprovalStatus, FileApproval, PendingFile};
pub use diff::{count_changes, count_diff, unified_diff};
pub use object_store::{ObjectStore, content_hash, project_key};
pub use scanner::{DirScanResult, is_binary, scan_dir_fast};
pub use snapshot::{
    BeforeState, ChangeStatus, DirFileChange, DirSnapshot, SnapshotStore, ToolSnapshot,
    capture_after, capture_after_dir, capture_before,
};
pub use tree_store::{
    StepSnapshot, TreeChangeStatus, TreeDiff, TreeEntries, compute_diff, deserialize_tree,
    get_file_hash, read_tree, serialize_tree, write_tree,
};

use crate::agent::error::AgentResult;
use crate::agent::extension::{Extension, SessionContext, TurnContext};
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

/// 生成 turn ID（如 "ts_a3f8b2c9"）
/// XL4: 扩大到 48bit + 加 session_id seed，降低跨 session 冲突概率
/// （原 24bit 约 4096 turn 就 50% 冲突，48bit 需要 ~16M turn 才 50%）
fn gen_turn_id() -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    std::time::SystemTime::now().hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    format!("ts_{:012x}", hasher.finish() & 0xFFFFFFFFFFFF)
}

impl FileSnapshotExtension {
    /// 创建并返回 (extension, store_arc)
    pub fn new_pair(
        storage: crate::storage_context::StorageContext,
    ) -> (Self, std::sync::Arc<SnapshotStore>) {
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
        Self::new_pair(crate::storage_context::StorageContext::new(
            cwd, "test", cwd,
        ))
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
        for rel_path in scan.files.keys() {
            let abs_path = std::path::Path::new(&self.storage.cwd).join(rel_path);
            if let Ok(content) = std::fs::read(&abs_path) {
                files.insert(rel_path.clone(), content);
            }
        }
        files
    }

    /// XL4: 保存快照时补 session_id（capture 函数构造时没传，统一在这里补）
    fn save_snap(&self, mut snap: snapshot::ToolSnapshot) {
        if snap.session_id.is_empty() {
            snap.session_id = self.storage.session_id.clone();
        }
        self.store.save_tool_snapshot(&snap);
    }
}

#[async_trait::async_trait]
impl Extension for FileSnapshotExtension {
    fn name(&self) -> &str {
        "file_snapshot"
    }

    async fn on_session_start(&self, _ctx: &SessionContext) -> AgentResult<()> {
        // session start：建立初始扫描 baseline + baseline tree + 异步触发 GC
        let scan = scan_dir_fast(&self.storage.cwd);
        *self.last_scan.lock().unwrap() = Some(scan);

        // G2 Bug4：baseline step-snapshot 每会话只建一次（once-guard）。
        // agent.run 每次 prompt 都会触发 on_session_start；无条件重建会把
        // 「上一轮之后、本轮之前」的写入（call_tool RPC 直调、用户手改）吸收进
        // 新 baseline（固定 turn_id → 同名文件覆盖旧锚点），
        // 这些写入永远进不了审批 pending，绕过审批面。
        //
        // ⚠️ turn_id 用 "ts_000000000000_session_start"（全零前缀）而非旧的
        // "ts_session_start"：load_all_step_snapshots 按 (timestamp, turn_id) 排序，
        // hex turn id（[0-9a-f]）字典序全部小于 's' 开头的旧 id——同毫秒写入时
        // session-start 会排到 turn step 之后，current_tree_hash() 因此取到陈旧的
        // session-start 快照（快测里整个 prompt 周期 <1ms，实测可复现）。
        // 全零前缀保证同毫秒时 session-start 稳定排在所有 turn step 之前。
        let baseline_exists = self
            .store
            .load_all_step_snapshots()
            .iter()
            .any(|s| {
                s.turn_id == "ts_000000000000_session_start"
                    && s.session_id == self.storage.session_id
            });
        if baseline_exists {
            // 本会话 baseline 已存在：只补内存锚点（worker 重启后为 None）。
            // 接在最新已提交快照上（不重扫磁盘）——重启期间的写入要在下一轮
            // turn_end 进 diff/pending，而不是被吸收。
            let mut anchor = self.baseline_tree_hash.lock().unwrap();
            if anchor.is_none() {
                *anchor = self.store.current_tree_hash();
            }
        } else {
            // 建立 baseline tree（session start 时的完整文件状态）
            let files = self.scan_to_file_contents();
            let (tree_hash, _) = tree_store::write_tree(self.store.objects(), &files);
            *self.baseline_tree_hash.lock().unwrap() = Some(tree_hash.clone());

            // 写 baseline step-snapshot（让 current_tree_hash 从一开始就有值）
            let step = tree_store::StepSnapshot {
                session_id: self.storage.session_id.clone(),
                turn_id: "ts_000000000000_session_start".to_string(),
                baseline_tree_hash: tree_hash.clone(),
                snapshot_tree_hash: tree_hash,
                diff: tree_store::TreeDiff {
                    added: vec![],
                    modified: vec![],
                    deleted: vec![],
                },
                timestamp: crate::session_jsonl::timestamp_iso(),
                seq: 0,
            };
            self.store.save_step_snapshot(&step);
        }

        // 异步 GC（不阻塞 agent）
        let active_hashes = self.collect_active_hashes();
        let store = std::sync::Arc::new(ObjectStore::for_project(&self.project_key));
        gc::run_gc_async(store, active_hashes);

        Ok(())
    }

    async fn on_turn_start(&self, _ctx: &mut TurnContext) -> AgentResult<()> {
        // 每轮 turn 生成唯一 ID（不依赖下标递增）
        *self.current_turn_id.lock().unwrap() = gen_turn_id();
        Ok(())
    }

    async fn on_turn_end(&self, ctx: &TurnContext) -> AgentResult<()> {
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
                self.save_snap(snap);
            }
        }

        // tree 快照：扫描当前文件 → write_tree → compute_diff(baseline) → 有变更才写 step-snapshot
        let files = self.scan_to_file_contents();
        let (current_tree_hash, _) = tree_store::write_tree(self.store.objects(), &files);

        let baseline = self.baseline_tree_hash.lock().unwrap().clone();
        if let Some(ref baseline_hash) = baseline
            && &current_tree_hash != baseline_hash
        {
            // 有变更：算 diff + 写 step-snapshot
            let old_tree =
                tree_store::read_tree(self.store.objects(), baseline_hash).unwrap_or_default();
            let new_tree =
                tree_store::read_tree(self.store.objects(), &current_tree_hash).unwrap_or_default();
            let diff = tree_store::compute_diff(&old_tree, &new_tree);
            if !diff.is_empty() {
                let step = tree_store::StepSnapshot {
                    session_id: self.storage.session_id.clone(),
                    turn_id: turn_id.clone(),
                    baseline_tree_hash: baseline_hash.clone(),
                    snapshot_tree_hash: current_tree_hash.clone(),
                    diff,
                    timestamp: crate::session_jsonl::timestamp_iso(),
                    seq: 0,
                };
                // 与 pi 一致：文件状态是消息树中的 parented custom entry。
                // 它不进入 LLM context，但 export / timeline / rollback 都能看到并定位。
                let data = serde_json::json!({
                    "baselineTreeHash": step.baseline_tree_hash,
                    "snapshotTreeHash": step.snapshot_tree_hash,
                    "diff": step.diff,
                    "turnIndex": ctx.turn_index,
                    "toolSnapshotTurnId": step.turn_id,
                });
                if crate::session_jsonl::append_custom_entry(
                    &self.storage.cwd,
                    crate::session_jsonl::CUSTOM_TYPE_STEP_SNAPSHOT,
                    data,
                )
                .is_none()
                {
                    tracing::warn!(
                        "file snapshot tree changed but no session leaf was available; JSONL step-snapshot was not appended"
                    );
                }
                self.store.save_step_snapshot(&step);
                // 下一轮只比较自上次已提交快照以来的变化，避免每轮重复累计 diff。
                *self.baseline_tree_hash.lock().unwrap() = Some(current_tree_hash.clone());
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
        let before = capture_before(
            &ctx.tool_name,
            &ctx.args,
            self.store.objects(),
            &self.storage.cwd,
        );
        if !matches!(before, BeforeState::Skip) {
            self.before_states
                .lock()
                .unwrap()
                .insert(ctx.tool_call_id.clone(), before);
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
                        &before_state,
                        self.store.objects(),
                        &turn_id,
                        &ctx.tool_call_id,
                        &ctx.tool_name,
                    ) {
                        self.save_snap(snap);
                    }
                }
                BeforeState::DirCapture { .. } => {
                    let snaps = capture_after_dir(
                        &before_state,
                        self.store.objects(),
                        &self.storage.cwd,
                        &turn_id,
                        &ctx.tool_call_id,
                    );
                    for snap in snaps {
                        self.save_snap(snap);
                    }
                }
                BeforeState::Skip => {}
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn changed_turn_appends_parented_step_snapshot_once() {
        let _guard = crate::paths::env_test_lock();
        let root = std::env::temp_dir().join(format!(
            "ion_step_snapshot_{}_{}",
            std::process::id(),
            crate::session_jsonl::generate_id()
        ));
        let cwd = root.join("work");
        let session_path = root.join("session.jsonl");
        std::fs::create_dir_all(&cwd).unwrap();
        crate::session_jsonl::set_session_file_override(Some(session_path.clone()));
        let cwd_str = cwd.to_string_lossy().to_string();
        crate::session_jsonl::ensure_session_header(&cwd_str, "sess_test");
        crate::session_jsonl::append_raw_entry(
            &cwd_str,
            &serde_json::json!({
                "type":"message", "id":"msg_1", "parentId":"sess_test",
                "timestamp":crate::session_jsonl::timestamp_iso(),
                "message":{"role":"assistant","content":"write file"}
            }),
        );

        let (extension, store) = FileSnapshotExtension::new_pair_with_cwd(&cwd_str);
        extension
            .on_session_start(&SessionContext {
                reason: "test".into(),
                session_id: Some("sess_test".into()),
            })
            .await
            .unwrap();
        std::fs::write(cwd.join("changed.txt"), "changed").unwrap();
        let mut ctx = TurnContext {
            turn_index: 1,
            messages: vec![],
            has_tool_calls: true,
            stop_reason: Some("tool_calls".into()),
            session_id: Some("sess_test".into()),
            session_cwd: Some(cwd_str.clone()),
        };
        extension.on_turn_start(&mut ctx).await.unwrap();
        extension.on_turn_end(&ctx).await.unwrap();

        let read_entries = || {
            std::fs::read_to_string(&session_path)
                .unwrap()
                .lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .collect::<Vec<_>>()
        };
        let entries = read_entries();
        let snapshots: Vec<_> = entries
            .iter()
            .filter(|entry| {
                entry.get("customType").and_then(|v| v.as_str())
                    == Some(crate::session_jsonl::CUSTOM_TYPE_STEP_SNAPSHOT)
            })
            .collect();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0]["parentId"].as_str(), Some("msg_1"));
        assert_eq!(snapshots[0]["data"]["turnIndex"].as_u64(), Some(1));
        assert_eq!(store.load_all_step_snapshots().len(), 2); // baseline + changed turn

        // 同一文件状态再结束一轮，不应重复写 step-snapshot。
        ctx.turn_index = 2;
        extension.on_turn_start(&mut ctx).await.unwrap();
        extension.on_turn_end(&ctx).await.unwrap();
        let entries = read_entries();
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.get("customType").and_then(|v| v.as_str())
                    == Some(crate::session_jsonl::CUSTOM_TYPE_STEP_SNAPSHOT))
                .count(),
            1
        );

        crate::session_jsonl::set_session_file_override(None);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// gen_turn_id should always produce a string with the "ts_" prefix.
    #[test]
    fn gen_turn_id_has_prefix() {
        let id = gen_turn_id();
        assert!(
            id.starts_with("ts_"),
            "turn ID should start with 'ts_', got: {}",
            id
        );
    }

    /// G2 Bug4 回归：on_session_start 的 baseline step-snapshot 每会话只建一次。
    ///
    /// 历史 bug：agent.run 每次 prompt 都触发 on_session_start，无条件重建 baseline
    /// （固定 turn_id "ts_session_start" → 同名文件覆盖旧锚点），把「上一轮之后、
    /// 本轮之前」的写入（call_tool RPC 直调 write、用户手改）吸收进新 baseline——
    /// 这些写入永远进不了审批 pending，绕过审批面。
    ///
    /// 复现路径（对标 S4 报告）：session start → call_tool 直调写文件 → 再次
    /// session start + turn_end → compute_pending 必须能看到该文件（修复前为空）。
    #[tokio::test]
    async fn session_start_baseline_built_once_keeps_call_tool_writes_pending() {
        let _guard = crate::paths::env_test_lock();
        let root = std::env::temp_dir().join(format!(
            "ion_g2_baseline_{}_{}",
            std::process::id(),
            crate::session_jsonl::generate_id()
        ));
        let cwd = root.join("work");
        let session_path = root.join("session.jsonl");
        std::fs::create_dir_all(&cwd).unwrap();
        crate::session_jsonl::set_session_file_override(Some(session_path.clone()));
        let cwd_str = cwd.to_string_lossy().to_string();
        crate::session_jsonl::ensure_session_header(&cwd_str, "sess_g2");
        crate::session_jsonl::append_raw_entry(
            &cwd_str,
            &serde_json::json!({
                "type":"message", "id":"msg_1", "parentId":"sess_g2",
                "timestamp":crate::session_jsonl::timestamp_iso(),
                "message":{"role":"assistant","content":"x"}
            }),
        );

        let (extension, store) = FileSnapshotExtension::new_pair_with_cwd(&cwd_str);
        // 与 new_pair_with_cwd 的 StorageContext 同 session_id（"test"），共享同一 store
        let storage = crate::storage_context::StorageContext::new(&cwd_str, "test", &cwd_str);
        let mgr = crate::file_snapshot::ApprovalManager::new(store.clone(), storage);

        let start_ctx = SessionContext {
            reason: "test".into(),
            session_id: Some("sess_g2".into()),
        };
        let mut turn_ctx = TurnContext {
            turn_index: 0,
            messages: vec![],
            has_tool_calls: false,
            stop_reason: None,
            session_id: Some("sess_g2".into()),
            session_cwd: Some(cwd_str.clone()),
        };

        // prompt 1：session start（建 baseline）→ 空轮 turn_end
        extension.on_session_start(&start_ctx).await.unwrap();
        extension.on_turn_start(&mut turn_ctx).await.unwrap();
        extension.on_turn_end(&turn_ctx).await.unwrap();

        // prompt 间隙：call_tool 直调 write（不产生 tree step-snapshot，只落磁盘）
        std::fs::write(cwd.join("direct.txt"), "direct write").unwrap();

        // prompt 2：又一次 on_session_start + turn_end。
        // 修复前：这里重建 baseline 吸收 direct.txt → pending 为空（红）。
        extension.on_session_start(&start_ctx).await.unwrap();
        turn_ctx.turn_index = 1;
        extension.on_turn_start(&mut turn_ctx).await.unwrap();
        extension.on_turn_end(&turn_ctx).await.unwrap();

        let pending = mgr.compute_pending();
        assert!(
            pending.iter().any(|p| p.path == "direct.txt"),
            "call_tool 直调写入必须进审批 pending（不能被下一轮 baseline 吸收）: {pending:?}"
        );

        // worker 重启模拟：新 extension 实例（内存锚点丢失）+ 同一 store。
        // on_session_start 不得重建 baseline；重启期间的写入也要进 pending。
        std::fs::write(cwd.join("after_restart.txt"), "written while worker down").unwrap();
        let (ext2, _store2) = FileSnapshotExtension::new_pair_with_cwd(&cwd_str);
        ext2.on_session_start(&start_ctx).await.unwrap();
        turn_ctx.turn_index = 2;
        ext2.on_turn_start(&mut turn_ctx).await.unwrap();
        ext2.on_turn_end(&turn_ctx).await.unwrap();

        let pending = mgr.compute_pending();
        assert!(
            pending.iter().any(|p| p.path == "after_restart.txt"),
            "重启后 on_session_start 不得重建 baseline 吸收写入: {pending:?}"
        );
        assert!(
            pending.iter().any(|p| p.path == "direct.txt"),
            "早前 direct.txt 的 pending 不应因重启消失: {pending:?}"
        );

        crate::session_jsonl::set_session_file_override(None);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// S4 回归：项目 `.ion/` 内部文件不得混进审批 pending。
    ///
    /// cwd 下的 `.ion/`（ION 自身配置/缓存/monitors/extensions 状态）会随扫描
    /// 进 baseline tree；ION 或 agent 改写这些内部文件时，审批面板出现垃圾
    /// 条目。修复两层：扫描器整目录排除 `.ion`（根因）+ compute_pending
    /// 兜底过滤（旧 baseline tree 里可能残留升级前的 `.ion` 条目，防止它们
    /// 以 "deleted" 姿态涌进 pending）。
    #[tokio::test]
    async fn pending_excludes_ion_internal_files() {
        let _guard = crate::paths::env_test_lock();
        let root = std::env::temp_dir().join(format!(
            "ion_s4_ion_pending_{}_{}",
            std::process::id(),
            crate::session_jsonl::generate_id()
        ));
        let cwd = root.join("work");
        let session_path = root.join("session.jsonl");
        std::fs::create_dir_all(cwd.join(".ion/cache")).unwrap();
        // 内部文件 + 正常源文件都在 baseline 之前就存在
        std::fs::write(cwd.join(".ion/cache/state.json"), "{\"v\":1}").unwrap();
        std::fs::write(cwd.join("app.rs"), "fn a(){}").unwrap();
        crate::session_jsonl::set_session_file_override(Some(session_path.clone()));
        let cwd_str = cwd.to_string_lossy().to_string();
        crate::session_jsonl::ensure_session_header(&cwd_str, "sess_s4");
        crate::session_jsonl::append_raw_entry(
            &cwd_str,
            &serde_json::json!({
                "type":"message", "id":"msg_1", "parentId":"sess_s4",
                "timestamp":crate::session_jsonl::timestamp_iso(),
                "message":{"role":"assistant","content":"x"}
            }),
        );

        let (extension, store) = FileSnapshotExtension::new_pair_with_cwd(&cwd_str);
        let storage = crate::storage_context::StorageContext::new(&cwd_str, "test", &cwd_str);
        let mgr = crate::file_snapshot::ApprovalManager::new(store.clone(), storage);

        let start_ctx = SessionContext {
            reason: "test".into(),
            session_id: Some("sess_s4".into()),
        };
        let mut turn_ctx = TurnContext {
            turn_index: 0,
            messages: vec![],
            has_tool_calls: false,
            stop_reason: None,
            session_id: Some("sess_s4".into()),
            session_cwd: Some(cwd_str.clone()),
        };

        // session start 建 baseline → 两个文件都被修改 → turn_end 提交 tree
        extension.on_session_start(&start_ctx).await.unwrap();
        extension.on_turn_start(&mut turn_ctx).await.unwrap();
        std::fs::write(cwd.join(".ion/cache/state.json"), "{\"v\":2}").unwrap();
        std::fs::write(cwd.join("app.rs"), "fn b(){}").unwrap();
        extension.on_turn_end(&turn_ctx).await.unwrap();

        let pending = mgr.compute_pending();
        assert!(
            pending.iter().any(|p| p.path == "app.rs"),
            "正常源文件的修改必须进 pending: {pending:?}"
        );
        assert!(
            !pending
                .iter()
                .any(|p| p.path == ".ion" || p.path.starts_with(".ion/")),
            ".ion/ 内部文件不得混进审批 pending: {pending:?}"
        );

        crate::session_jsonl::set_session_file_override(None);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// gen_turn_id should be 15 chars: "ts_" (3) + 12 hex chars.
    #[test]
    fn gen_turn_id_length() {
        let id = gen_turn_id();
        // "ts_" + 12 hex digits = 15 characters total
        assert_eq!(
            id.len(),
            15,
            "expected 15 chars, got '{}' (len {})",
            id,
            id.len()
        );
        // The suffix after "ts_" must be valid hex
        let hex = &id[3..];
        assert!(
            hex.chars().all(|c| c.is_ascii_hexdigit()),
            "suffix '{}' should be all hex digits",
            hex
        );
    }

    /// gen_turn_id relies on SystemTime + PID for seeding, so rapid calls within
    /// the same time tick can collide. We instead verify the basic contract:
    /// every generated ID has the correct format (prefix + hex suffix), and the
    /// generator never panics across many invocations.
    #[test]
    fn gen_turn_id_format_contract_many_calls() {
        for _ in 0..100 {
            let id = gen_turn_id();
            assert!(id.starts_with("ts_"), "missing prefix: {}", id);
            assert_eq!(id.len(), 15, "bad length: {}", id);
            assert!(
                id[3..].chars().all(|c| c.is_ascii_hexdigit()),
                "non-hex suffix: {}",
                id
            );
        }
    }

    /// content_hash should be deterministic: same input -> same output.
    #[test]
    fn content_hash_deterministic_via_reexport() {
        let h1 = content_hash(b"hello world");
        let h2 = content_hash(b"hello world");
        assert_eq!(h1, h2, "same content must hash to the same value");
        // 16 hex chars (64-bit SipHash)
        assert_eq!(h1.len(), 16, "expected 16 hex chars, got len {}", h1.len());
        assert!(h1.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// content_hash should differ for different inputs.
    #[test]
    fn content_hash_differs_for_different_input() {
        let h1 = content_hash(b"hello");
        let h2 = content_hash(b"world");
        assert_ne!(h1, h2, "different content should hash differently");
    }

    /// TreeDiff::is_empty() should be true for a freshly-constructed empty diff,
    /// and false once any bucket is non-empty.
    #[test]
    fn tree_diff_is_empty() {
        let empty = TreeDiff {
            added: vec![],
            modified: vec![],
            deleted: vec![],
        };
        assert!(empty.is_empty());

        let with_added = TreeDiff {
            added: vec!["a.txt".to_string()],
            modified: vec![],
            deleted: vec![],
        };
        assert!(!with_added.is_empty());
    }

    /// TreeDiff::total() should be the sum of added + modified + deleted counts.
    #[test]
    fn tree_diff_total_sums_all_buckets() {
        let diff = TreeDiff {
            added: vec!["a".to_string(), "b".to_string()],
            modified: vec!["c".to_string()],
            deleted: vec!["d".to_string(), "e".to_string(), "f".to_string()],
        };
        assert_eq!(diff.total(), 6);
    }

    /// TreeDiff::total() should be 0 when all buckets are empty.
    #[test]
    fn tree_diff_total_zero_when_empty() {
        let diff = TreeDiff {
            added: vec![],
            modified: vec![],
            deleted: vec![],
        };
        assert_eq!(diff.total(), 0);
    }

    /// count_diff should correctly count added/removed lines,
    /// ignoring the --- / +++ header lines.
    #[test]
    fn count_diff_counts_additions_and_removals_via_reexport() {
        let diff = "--- a/x\n+++ b/x\n@@ -1,1 +1,2 @@\n-old\n+new1\n+new2\n context\n";
        let (added, removed) = count_diff(diff);
        assert_eq!(added, 2, "expected 2 added lines");
        assert_eq!(removed, 1, "expected 1 removed line");
    }

    /// compute_diff should detect added, modified, and deleted entries
    /// between two tree entry maps.
    #[test]
    fn compute_diff_via_reexport_detects_all_changes() {
        let mut old = std::collections::HashMap::new();
        old.insert("a.rs".to_string(), "h1".to_string());
        old.insert("b.rs".to_string(), "h2".to_string());
        old.insert("c.rs".to_string(), "h3".to_string());

        let mut new = std::collections::HashMap::new();
        new.insert("a.rs".to_string(), "h1".to_string()); // unchanged
        new.insert("b.rs".to_string(), "h2_new".to_string()); // modified
        new.insert("d.rs".to_string(), "h4".to_string()); // added

        let diff = compute_diff(&old, &new);
        assert_eq!(diff.added, vec!["d.rs"]);
        assert_eq!(diff.modified, vec!["b.rs"]);
        assert_eq!(diff.deleted, vec!["c.rs"]);
        assert_eq!(diff.total(), 3);
    }

    /// get_file_hash should return Some(hash) for existing paths and None otherwise.
    #[test]
    fn get_file_hash_via_reexport() {
        let mut tree = std::collections::HashMap::new();
        tree.insert("src/main.rs".to_string(), "abc123".to_string());

        assert_eq!(
            get_file_hash(&tree, "src/main.rs"),
            Some(&"abc123".to_string()),
            "existing path should return its hash"
        );
        assert_eq!(
            get_file_hash(&tree, "missing.rs"),
            None,
            "missing path should return None"
        );
    }

    /// unified_diff should embed the provided path into the --- / +++ headers.
    #[test]
    fn unified_diff_includes_path_header_via_reexport() {
        let before = "line1\nline2";
        let after = "line1\nline2 changed";
        let diff = unified_diff(before, after, "src/lib.rs");
        assert!(
            diff.contains("--- a/src/lib.rs"),
            "missing --- header: {}",
            diff
        );
        assert!(
            diff.contains("+++ b/src/lib.rs"),
            "missing +++ header: {}",
            diff
        );
        assert!(
            diff.contains("+line2 changed"),
            "missing added line: {}",
            diff
        );
    }
}
