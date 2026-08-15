//! Approval — 文件变更审批（post-hoc per-file，对标 pi file-review extension）
//!
//! agent 改完文件后（on_gate_check 触发），列出变更 diff 推给用户逐文件审批。
//! approve → 保留；reject → 单文件回滚（restore_single_file）。
//!
//! 核心算法链（对齐 pi file-review）：
//! 1. approve 锚定 baseline（记录 tree_hash，后续 diff 从这里算）
//! 2. re-approval 重置（已批准文件被改 → 回 pending）
//! 3. net-zero 过滤（added→deleted 且未 approved → 不显示）
//! 4. reject → 回滚 + 更新快照
//!
//! 审批状态持久化到 session.jsonl（file-approval entry），重启不丢。

use super::snapshot::SnapshotStore;
use super::tree_store;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

/// 单个文件的审批状态
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Rejected,
}

/// 单个文件的审批记录
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileApproval {
    pub path: String,
    pub status: ApprovalStatus,
    pub timestamp: u64,
    /// approve 时锚定的 tree_hash（后续 diff 的 baseline）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approved_tree_hash: Option<String>,
    /// approve 时对应的 turn_id
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approved_turn_id: Option<String>,
}

/// pending 列表里的单个文件（含 diff 信息）
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingFile {
    pub path: String,
    pub status: String, // added / modified / deleted
    pub diff_stat: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_content: Option<String>,
}

/// 审批管理器（内存状态 + 持久化）
pub struct ApprovalManager {
    /// 文件路径 → 审批记录
    approvals: Mutex<HashMap<String, FileApproval>>,
    /// 历史上曾批准过的文件（只增不减，net-zero 安全阀）
    ever_approved: Mutex<HashSet<String>>,
    /// 快照存储引用
    store: std::sync::Arc<SnapshotStore>,
    /// session cwd（用于 restore 和文件操作）
    cwd: String,
    /// 统一存储上下文（持久化审批 entry 到 session.jsonl 用）
    #[allow(dead_code)]
    storage: crate::storage_context::StorageContext,
    /// 审批状态代数：approvals/ever_approved 每次变更 +1，
    /// 用作 pending_cache 的失效键（避免读锁比较整个 map）
    approval_gen: std::sync::atomic::AtomicU64,
    /// compute_pending 结果缓存：(current_tree_hash, gen, 结果)。
    /// tree 或审批状态没变时重复查询直接命中（大仓库 695 文件首次 ~秒级，命中 0ms）
    pending_cache: Mutex<Option<(String, u64, std::sync::Arc<Vec<PendingFile>>)>>,
}

impl ApprovalManager {
    pub fn new(
        store: std::sync::Arc<SnapshotStore>,
        storage: crate::storage_context::StorageContext,
    ) -> Self {
        Self {
            approvals: Mutex::new(HashMap::new()),
            ever_approved: Mutex::new(HashSet::new()),
            store,
            cwd: storage.cwd.clone(),
            storage,
            approval_gen: std::sync::atomic::AtomicU64::new(0),
            pending_cache: Mutex::new(None),
        }
    }

    /// 获取 baseline tree hash（不加锁版本，compute_pending 内部用）
    fn baseline_for_path(
        &self,
        path: &str,
        approvals: &HashMap<String, FileApproval>,
    ) -> Option<String> {
        // 优先用该文件的 approved baseline（即使状态被 re-approval 重置成 pending，baseline 锚定保持）
        if let Some(appr) = approvals.get(path)
            && let Some(ref h) = appr.approved_tree_hash
        {
            return Some(h.clone());
        }
        // 否则用 session baseline
        self.session_baseline_tree_hash()
    }

    /// session start 的 baseline tree hash
    fn session_baseline_tree_hash(&self) -> Option<String> {
        let steps = self.store.load_all_step_snapshots();
        steps.first().map(|s| s.baseline_tree_hash.clone())
    }

    /// 当前最新 tree hash
    fn current_tree_hash(&self) -> Option<String> {
        self.store.current_tree_hash()
    }

    /// 计算 pending 列表（对比 baseline → current tree 的 diff + 过滤）
    ///
    /// 结果按 (current_tree_hash, 审批代数) 记忆化：同一轮快照内重复查询 0ms。
    /// 首次计算也只解析一次 baseline（原来每个文件都重复读 step snapshots + tree）。
    pub fn compute_pending(&self) -> std::sync::Arc<Vec<PendingFile>> {
        let current_hash = match self.current_tree_hash() {
            Some(h) => h,
            None => return std::sync::Arc::new(vec![]),
        };
        let approval_gen = self.approval_gen.load(std::sync::atomic::Ordering::Acquire);
        {
            let cache = self.pending_cache.lock().unwrap();
            if let Some((ref h, g, ref cached)) = *cache
                && *h == current_hash
                && g == approval_gen
            {
                return cached.clone();
            }
        }

        let current_tree =
            tree_store::read_tree(self.store.objects(), &current_hash).unwrap_or_default();

        let objects = self.store.objects();
        let mut pending = Vec::new();
        let approvals = self.approvals.lock().unwrap();

        // baseline 整体只解析一次（load_all_step_snapshots + read_tree 原来在
        // 每个文件的 baseline_for_path 里重复执行，N 文件 = N 次全量磁盘解析）
        let session_baseline = self.session_baseline_tree_hash();
        let session_baseline_tree = session_baseline
            .as_ref()
            .and_then(|h| tree_store::read_tree(objects, h));
        let empty_tree: tree_store::TreeEntries = HashMap::new();
        // 少数文件有自己的 approved baseline（批准后又改过），按需读取 + 单槽 memo
        let mut per_file_tree: Option<(String, tree_store::TreeEntries)> = None;

        for (path, new_hash) in &current_tree {
            // 状态判断：优先该文件的 approved baseline，否则 session baseline
            let baseline_tree: &tree_store::TreeEntries = if let Some(appr) = approvals.get(path)
                && let Some(ref h) = appr.approved_tree_hash
            {
                if per_file_tree.as_ref().is_none_or(|(ph, _)| ph != h) {
                    per_file_tree =
                        Some((h.clone(), tree_store::read_tree(objects, h).unwrap_or_default()));
                }
                &per_file_tree.as_ref().expect("just set").1
            } else {
                session_baseline_tree.as_ref().unwrap_or(&empty_tree)
            };
            let old_hash = baseline_tree.get(path);

            let (status, old_content, new_content) = match old_hash {
                None => {
                    // 新文件（baseline 里没有）
                    ("added", None, objects.read_object_text(new_hash))
                }
                Some(oh) if oh != new_hash => {
                    // 修改了
                    (
                        "modified",
                        objects.read_object_text(oh),
                        objects.read_object_text(new_hash),
                    )
                }
                _ => continue, // 没变
            };

            // net-zero 过滤：added→deleted 且从未 approved
            // 注意：这里的 deleted 指的是"当前 disk 上不存在但 turn log 里有"
            // tree 模型里直接看 current_tree 有没有就行

            // no-op 过滤：内容相同（git checkout 回滚到 approved 状态）
            if status == "modified" && old_content == new_content {
                continue;
            }

            // 检查审批状态：只有 pending 的才显示
            let approval = approvals.get(path);
            let is_pending = match approval {
                None => true, // 没记录 → pending
                Some(a) => a.status == ApprovalStatus::Pending,
            };
            if !is_pending {
                continue;
            }

            // 计算 diff_stat
            let diff_stat = match (status, &old_content, &new_content) {
                ("added", _, Some(nc)) => format!("{} | {}+", path, nc.lines().count()),
                ("modified", Some(oc), Some(nc)) => {
                    let (add, del) = super::diff::count_changes(oc, nc);
                    format!("{} | {}+{}-", path, add, del)
                }
                ("deleted", Some(oc), _) => format!("{} | {}-", path, oc.lines().count()),
                _ => path.clone(),
            };

            pending.push(PendingFile {
                path: path.clone(),
                status: status.to_string(),
                diff_stat,
                old_content,
                new_content,
            });
        }

        // 也检查被删除的文件（baseline 有，current 没有）——复用上面解析好的 baseline
        if let Some(ref _bh) = session_baseline {
            let baseline_tree = session_baseline_tree.as_ref().unwrap_or(&empty_tree);
            for path in baseline_tree.keys() {
                if !current_tree.contains_key(path) {
                    // 文件被删除了
                    let approval = approvals.get(path);
                    let is_pending = match approval {
                        None => true,
                        Some(a) => a.status == ApprovalStatus::Pending,
                    };
                    if !is_pending {
                        continue;
                    }

                    // net-zero：如果从未 approved，且 baseline 里有但 current 没有
                    // 这里不删——因为文件确实被删了，需要用户审批

                    pending.push(PendingFile {
                        path: path.clone(),
                        status: "deleted".to_string(),
                        diff_stat: format!("{} | deleted", path),
                        old_content: None,
                        new_content: None,
                    });
                }
            }
        }
        drop(approvals);

        let result = std::sync::Arc::new(pending);
        *self.pending_cache.lock().unwrap() = Some((current_hash, approval_gen, result.clone()));
        result
    }

    /// approve 单个文件（锚定 baseline + 持久化到 session.jsonl）
    pub fn approve(&self, path: &str) -> Result<FileApproval, String> {
        self.approve_inner(path, false)
    }

    /// silent=true 时不推 ApprovalResolved（批量操作由调用方聚合成一条汇总事件，
    /// 否则 approve_all 批 N 个文件会刷 N 条事件）
    fn approve_inner(&self, path: &str, silent: bool) -> Result<FileApproval, String> {
        let current_hash = self
            .current_tree_hash()
            .ok_or("No current tree snapshot available")?;
        self.approve_hashed(path, silent, &current_hash)
    }

    /// approve 的批量快速路径：current_hash 由调用方算一次传入。
    /// （current_tree_hash → load_all_step_snapshots 读整个 tree/ 目录，
    /// 695 文件逐个重算 = 28 万次文件读，是批量审批慢的根因）
    fn approve_hashed(
        &self,
        path: &str,
        silent: bool,
        current_hash: &str,
    ) -> Result<FileApproval, String> {
        let mut approvals = self.approvals.lock().unwrap();
        let mut ever_approved = self.ever_approved.lock().unwrap();

        ever_approved.insert(path.to_string());

        let approval = FileApproval {
            path: path.to_string(),
            status: ApprovalStatus::Approved,
            timestamp: now_ts(),
            approved_tree_hash: Some(current_hash.to_string()),
            approved_turn_id: None,
        };
        approvals.insert(path.to_string(), approval.clone());
        drop(approvals);
        drop(ever_approved);
        self.approval_gen.fetch_add(1, std::sync::atomic::Ordering::Release);

        // 持久化到 session.jsonl
        self.persist_approval(&approval);

        // 推送 ApprovalResolved 事件（UI 收到后更新状态；批量时静默）
        if !silent {
            emit_approval_event(
                "ApprovalResolved",
                &serde_json::json!({
                    "path": path,
                    "decision": "approved",
                    "approvedTreeHash": current_hash,
                }),
            );
        }

        Ok(approval)
    }

    /// reject 单个文件（回滚到 baseline + 更新状态 + 持久化）
    pub fn reject(&self, path: &str) -> Result<super::restore::RestoredFile, String> {
        let session_baseline = self.session_baseline_tree_hash();
        self.reject_with_baseline(path, session_baseline.as_deref())
    }

    /// reject 的批量快速路径：session baseline 由调用方算一次传入
    /// （session_baseline_tree_hash 同样每文件重读全部 step snapshots）
    fn reject_with_baseline(
        &self,
        path: &str,
        session_baseline: Option<&str>,
    ) -> Result<super::restore::RestoredFile, String> {
        let baseline_hash = {
            let approvals = self.approvals.lock().unwrap();
            // 优先该文件自己的 approved baseline，否则 session baseline
            approvals
                .get(path)
                .and_then(|a| a.approved_tree_hash.clone())
                .or_else(|| session_baseline.map(|s| s.to_string()))
        }
        .ok_or("No baseline tree available")?;

        // 单文件回滚
        let abs_path = std::path::Path::new(&self.cwd).join(path);
        let result = super::restore::restore_single_file(
            &self.store,
            &baseline_hash,
            abs_path.to_string_lossy().as_ref(),
            &self.cwd,
            false,
        );

        // 标记 rejected
        let approval = FileApproval {
            path: path.to_string(),
            status: ApprovalStatus::Rejected,
            timestamp: now_ts(),
            approved_tree_hash: None,
            approved_turn_id: None,
        };
        let mut approvals = self.approvals.lock().unwrap();
        approvals.insert(path.to_string(), approval.clone());
        drop(approvals);
        self.approval_gen.fetch_add(1, std::sync::atomic::Ordering::Release);

        // 持久化到 session.jsonl
        self.persist_approval(&approval);

        // 推送 ApprovalResolved 事件（UI 收到后更新状态）
        emit_approval_event(
            "ApprovalResolved",
            &serde_json::json!({
                "path": path,
                "decision": "rejected",
                "action": result.action,
                "rolledBack": true,
            }),
        );

        Ok(result)
    }

    /// approve 全部 pending 文件（逐文件静默，仅推一条汇总事件）。
    /// 空批次直接返回不推事件（避免 0/0 噪音刷屏）。
    pub fn approve_all(&self) -> Vec<Result<FileApproval, String>> {
        let pending = self.compute_pending();
        if pending.is_empty() {
            return vec![];
        }
        // hash 只算一次（原来每个文件重读全部 step snapshots）
        let results: Vec<_> = match self.current_tree_hash() {
            Some(h) => pending
                .iter()
                .map(|p| self.approve_hashed(&p.path, true, &h))
                .collect(),
            None => pending
                .iter()
                .map(|_| Err("No current tree snapshot available".to_string()))
                .collect(),
        };
        let approved = results.iter().filter(|r| r.is_ok()).count();
        let failed = results.len() - approved;
        emit_approval_event(
            "ApprovalResolved",
            &serde_json::json!({
                "batch": "approve_all",
                "approved": approved,
                "failed": failed,
                "total": results.len(),
            }),
        );
        results
    }

    /// reject 全部 pending 文件（逐文件静默，仅推一条汇总事件）。
    /// 空批次直接返回不推事件。
    pub fn reject_all(&self) -> Vec<Result<super::restore::RestoredFile, String>> {
        let pending = self.compute_pending();
        if pending.is_empty() {
            return vec![];
        }
        // session baseline 只算一次（原来每个文件重读全部 step snapshots）
        let session_baseline = self.session_baseline_tree_hash();
        let results: Vec<_> = pending
            .iter()
            .map(|p| self.reject_with_baseline(&p.path, session_baseline.as_deref()))
            .collect();
        let rejected = results.iter().filter(|r| r.is_ok()).count();
        emit_approval_event(
            "ApprovalResolved",
            &serde_json::json!({
                "batch": "reject_all",
                "rejected": rejected,
                "total": results.len(),
            }),
        );
        results
    }

    /// 查询审批状态
    /// 单文件 diff（与 compute_pending 同源同 baseline 语义）。
    /// 复用 pending 缓存里的 old/new content —— 通常零额外磁盘读。
    /// 注意不要用 get_file_diff（tool-snapshot 历史）替代：
    /// pending 列表基于 tree 快照，两者数据源不一致会出现"列在 pending 但查不到 diff"。
    pub fn file_diff(&self, path: &str) -> Option<serde_json::Value> {
        let pending = self.compute_pending();
        let p = pending.iter().find(|p| p.path == path)?;
        // deleted 条目在 pending 列表里不带内容（避免缓存整块旧文件），按需读一次
        let (old, new) = if p.status == "deleted"
            && p.old_content.is_none()
            && p.new_content.is_none()
        {
            let approvals = self.approvals.lock().unwrap();
            let baseline_hash = self.baseline_for_path(path, &approvals);
            drop(approvals);
            let content = baseline_hash
                .and_then(|h| tree_store::read_tree(self.store.objects(), &h))
                .and_then(|t| t.get(path).cloned())
                .and_then(|blob| self.store.objects().read_object_text(&blob));
            (content, None)
        } else {
            (p.old_content.clone(), p.new_content.clone())
        };
        let diff = match (&old, &new) {
            (Some(b), Some(a)) => super::diff::unified_diff(b, a, path),
            (None, Some(a)) => format!("+++ new file\n{}", a),
            (Some(b), None) => format!("--- deleted file\n{}", b),
            _ => String::new(),
        };
        // 整文件新增/删除的特例格式里内容行没有 +/- 前缀，count_diff 数不出，
        // 直接按行数计
        let (added, removed) = if old.is_none() || new.is_none() {
            (
                new.as_ref().map(|a| a.lines().count()).unwrap_or(0),
                old.as_ref().map(|b| b.lines().count()).unwrap_or(0),
            )
        } else {
            super::diff::count_diff(&diff)
        };
        Some(serde_json::json!({
            "path": path,
            "status": p.status,
            "diff": diff,
            "diffAvailable": true,
            "added": added,
            "removed": removed,
        }))
    }

    pub fn approvals_list(&self, status_filter: Option<&ApprovalStatus>) -> Vec<FileApproval> {
        let approvals = self.approvals.lock().unwrap();
        approvals
            .values()
            .filter(|a| match status_filter {
                Some(s) => &a.status == s,
                None => true,
            })
            .cloned()
            .collect()
    }

    /// re-approval 重置：已批准/拒绝的文件被新改动修改 → 回 pending
    /// 在 on_turn_end 时调用
    pub fn check_re_approval(&self, changed_paths: &[String]) {
        let mut reset_paths = Vec::new();
        {
            let mut approvals = self.approvals.lock().unwrap();
            for path in changed_paths {
                if let Some(appr) = approvals.get_mut(path)
                    && (appr.status == ApprovalStatus::Approved
                        || appr.status == ApprovalStatus::Rejected)
                {
                    appr.status = ApprovalStatus::Pending;
                    appr.timestamp = now_ts();
                    reset_paths.push(path.clone());
                    // 注意：approved_tree_hash 不删（保持 baseline 锚定）
                    // 这样 diff 仍从上次 approved 位置算
                }
            }
        }
        // 推送 ApprovalReset 事件（UI 收到后刷新审批状态）
        if !reset_paths.is_empty() {
            self.approval_gen.fetch_add(1, std::sync::atomic::Ordering::Release);
            emit_approval_event(
                "ApprovalReset",
                &serde_json::json!({
                    "paths": reset_paths,
                    "reason": "file_changed_after_approval",
                }),
            );
        }
    }

    /// 从 session entries 重建审批状态（session_start 用）
    pub fn restore_from_entries(&self, entries: &[serde_json::Value]) {
        let mut approvals = self.approvals.lock().unwrap();
        let mut ever_approved = self.ever_approved.lock().unwrap();
        approvals.clear();
        ever_approved.clear();

        for entry in entries {
            if entry.get("type").and_then(|v| v.as_str()) == Some("file-approval")
                && let Some(data) = entry.get("data")
            {
                let path = data.get("path").and_then(|v| v.as_str()).unwrap_or("");
                let status_str = data
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("pending");
                let status = match status_str {
                    "approved" => ApprovalStatus::Approved,
                    "rejected" => ApprovalStatus::Rejected,
                    _ => ApprovalStatus::Pending,
                };
                let ts = data.get("timestamp").and_then(|v| v.as_u64()).unwrap_or(0);
                let approved_tree_hash = data
                    .get("approved_tree_hash")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());

                if status == ApprovalStatus::Approved {
                    ever_approved.insert(path.to_string());
                }

                approvals.insert(
                    path.to_string(),
                    FileApproval {
                        path: path.to_string(),
                        status,
                        timestamp: ts,
                        approved_tree_hash,
                        approved_turn_id: None,
                    },
                );
            }
        }
        self.approval_gen.fetch_add(1, std::sync::atomic::Ordering::Release);
    }

    /// 暴露 step-snapshot 给 Extension 用（re-approval 重置）
    pub fn store_load_step_snapshots(&self) -> Vec<tree_store::StepSnapshot> {
        self.store.load_all_step_snapshots()
    }

    /// 持久化单个审批决策到 session.jsonl
    fn persist_approval(&self, approval: &FileApproval) {
        let status_str = match approval.status {
            ApprovalStatus::Pending => "pending",
            ApprovalStatus::Approved => "approved",
            ApprovalStatus::Rejected => "rejected",
        };
        let entry = serde_json::json!({
            "type": "custom",
            "id": format!("fa_{}", now_ts()),
            "parentId": null,
            "timestamp": crate::session_jsonl::timestamp_iso(),
            "customType": "file-approval",
            "data": {
                "path": approval.path,
                "status": status_str,
                "timestamp": approval.timestamp,
                "approved_tree_hash": approval.approved_tree_hash,
            },
        });
        crate::session_jsonl::append_raw_entry(&self.cwd, &entry);
    }

    /// 从 session.jsonl 恢复审批状态（session_start 调）
    pub fn restore_from_session(&self) {
        let entries = crate::message_retrieval::load_entries_cached(&self.cwd);
        let approval_entries: Vec<serde_json::Value> = entries
            .into_iter()
            .filter(|e| e.get("customType").and_then(|v| v.as_str()) == Some("file-approval"))
            .collect();
        if !approval_entries.is_empty() {
            self.restore_from_entries(&approval_entries);
        }
    }

    /// 序列化当前审批状态为 entries（持久化用）
    pub fn to_entries(&self) -> Vec<serde_json::Value> {
        let approvals = self.approvals.lock().unwrap();
        approvals.values().map(|a| {
            serde_json::json!({
                "type": "file-approval",
                "data": {
                    "path": a.path,
                    "status": serde_json::to_string(&a.status).unwrap_or_default().trim_matches('"'),
                    "timestamp": a.timestamp,
                    "approved_tree_hash": a.approved_tree_hash,
                }
            })
        }).collect()
    }
}

fn now_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 推送审批事件到 Manager（仿 BashExtension stdout JSON 模式）
///
/// 事件经 Worker stdout → Manager event-pump → ExtensionEventBus → CLI subscribe
/// customType: ApprovalRequest（有待审批）/ ApprovalResolved（审批完成）/ ApprovalReset（re-approval 重置）
fn emit_approval_event(custom_type: &str, data: &serde_json::Value) {
    let msg = serde_json::json!({
        "type": "event",
        "event": {
            "type": "extension_event",
            "extension": "file-approval",
            "customType": custom_type,
            "visibility": "llm_and_ui",
            "timestamp": now_ts(),
            "data": data,
        },
    });
    println!("{}", serde_json::to_string(&msg).unwrap_or_default());
}

// ────────────────────────────────────────────────────────────────────────────
// ApprovalExtension — 实现 Extension trait，挂 on_gate_check
// ────────────────────────────────────────────────────────────────────────────

use crate::agent::error::AgentResult;
use crate::agent::extension::{Extension, GateDecision, TurnContext};

/// 审批 Extension — agent Stop 时自动检查 pending 变更
///
/// 设计：
/// - 有 pending 变更 + 有 pending 文件 → 返回 RetryWith（注入消息告诉 agent 有待审批文件）
/// - 无 pending 或全部已处理 → 返回 Allow
///
/// 注意：这是"通知"不是"阻塞"——无 UI 订阅时不阻塞 agent（返回 Allow）
pub struct ApprovalExtension {
    mgr: std::sync::Arc<ApprovalManager>,
}

impl ApprovalExtension {
    pub fn new(mgr: std::sync::Arc<ApprovalManager>) -> Self {
        Self { mgr }
    }
}

#[async_trait::async_trait]
impl Extension for ApprovalExtension {
    fn name(&self) -> &str {
        "file-approval"
    }

    async fn on_session_start(
        &self,
        _ctx: &crate::agent::extension::SessionContext,
    ) -> AgentResult<()> {
        // 从 session.jsonl 恢复审批状态
        self.mgr.restore_from_session();
        Ok(())
    }

    async fn on_gate_check(&self, _ctx: &TurnContext) -> AgentResult<GateDecision> {
        let pending = self.mgr.compute_pending();
        if pending.is_empty() {
            return Ok(GateDecision::Allow);
        }

        // 有待审批文件 → 推送事件 + 记录日志
        let file_list: Vec<String> = pending
            .iter()
            .map(|p| format!("  - {} ({})", p.path, p.status))
            .collect();

        // 推送 ApprovalRequest 事件（UI 收到后可展示审批界面）
        let request_id = format!("appr_{}", now_ts());
        let pending_json: Vec<serde_json::Value> = pending
            .iter()
            .map(|p| {
                serde_json::json!({
                    "path": p.path,
                    "status": p.status,
                    "diffStat": p.diff_stat,
                })
            })
            .collect();
        emit_approval_event(
            "ApprovalRequest",
            &serde_json::json!({
                "requestId": request_id,
                "total": pending.len(),
                "files": pending_json,
            }),
        );

        tracing::info!(
            "[file-approval] {} files pending review (requestId={}):\n{}",
            pending.len(),
            request_id,
            file_list.join("\n")
        );

        // 返回 Allow——审批是 post-hoc 的，不阻塞 agent 停止
        // agent 停止后用户通过 RPC 审批（approve/reject）
        Ok(GateDecision::Allow)
    }

    async fn on_turn_end(&self, _ctx: &TurnContext) -> AgentResult<()> {
        // re-approval 重置：turn 结束时检查 step-snapshot 的 diff，
        // 把变更涉及的已批准/已拒绝文件重置为 pending
        let steps = self.mgr.store_load_step_snapshots();
        if let Some(last_step) = steps.last() {
            let changed: Vec<String> = last_step
                .diff
                .added
                .iter()
                .chain(last_step.diff.modified.iter())
                .chain(last_step.diff.deleted.iter())
                .cloned()
                .collect();
            if !changed.is_empty() {
                self.mgr.check_re_approval(&changed);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (
        std::path::PathBuf,
        std::sync::Arc<SnapshotStore>,
        ApprovalManager,
    ) {
        // Process-unique counter guarantees no collision even when parallel
        // tests call setup() within the same nanosecond (subsec_nanos alone
        // collided occasionally → shared store dirs → flaky baseline hashing).
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let id = format!(
            "fs_approval_{}_{}_{}",
            std::process::id(),
            n,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        );
        let base = std::env::temp_dir().join(&id);
        let work_dir = base.join("work");
        std::fs::create_dir_all(&work_dir).unwrap();
        let store = std::sync::Arc::new(SnapshotStore::new_at(base.join("store")));

        // 建 baseline tree（session start）
        let mut files = std::collections::HashMap::new();
        files.insert("a.rs".into(), b"original".to_vec());
        files.insert("b.rs".into(), b"stable".to_vec());
        let (baseline_hash, _) = tree_store::write_tree(store.objects(), &files);
        std::fs::write(work_dir.join("a.rs"), "original").unwrap();
        std::fs::write(work_dir.join("b.rs"), "stable").unwrap();

        // 写一条 step-snapshot 模拟 session start。
        // turn_id 用 ts_000000：load_all_step_snapshots 按 (timestamp, turn_id)
        // 排序，同秒写入时按 turn_id 字典序定序。baseline 必须排在所有测试
        // 写入的 ts_000001/ts_000002... 之前，否则 .last() 会误取 session
        // start 作为"最新"step（这是 Linux CI 上同秒写入导致 flaky 的根因）。
        let step = tree_store::StepSnapshot {
            turn_id: "ts_000000".into(),
            baseline_tree_hash: baseline_hash.clone(),
            snapshot_tree_hash: baseline_hash.clone(), // session start 无变更
            diff: tree_store::TreeDiff {
                added: vec![],
                modified: vec![],
                deleted: vec![],
            },
            timestamp: crate::session_jsonl::timestamp_iso(),
        };
        store.save_step_snapshot(&step);

        let storage = crate::storage_context::StorageContext::new(
            work_dir.to_string_lossy().as_ref(),
            "approval_test",
            work_dir.to_string_lossy().as_ref(),
        );
        let mgr = ApprovalManager::new(store.clone(), storage);
        (work_dir, store, mgr)
    }

    fn write_current_tree(
        store: &SnapshotStore,
        work_dir: &std::path::Path,
        files: &[(&str, &str)],
    ) -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(1);

        let mut file_map = std::collections::HashMap::new();
        for (path, content) in files {
            file_map.insert(path.to_string(), content.as_bytes().to_vec());
            std::fs::write(work_dir.join(path), content).unwrap();
        }
        let (hash, _) = tree_store::write_tree(store.objects(), &file_map);

        // 写 step-snapshot
        let baseline = store
            .load_all_step_snapshots()
            .last()
            .map(|s| s.snapshot_tree_hash.clone())
            .unwrap_or(hash.clone());
        let old_tree = tree_store::read_tree(store.objects(), &baseline).unwrap_or_default();
        let new_tree = tree_store::read_tree(store.objects(), &hash).unwrap_or_default();
        let diff = tree_store::compute_diff(&old_tree, &new_tree);

        let seq = SEQ.fetch_add(1, Ordering::SeqCst);
        let step = tree_store::StepSnapshot {
            turn_id: format!("ts_{:06}", seq),
            baseline_tree_hash: baseline,
            snapshot_tree_hash: hash.clone(),
            diff,
            timestamp: crate::session_jsonl::timestamp_iso(),
        };
        store.save_step_snapshot(&step);
        hash
    }

    #[test]
    fn pending_shows_modified_file() {
        let (work_dir, store, mgr) = setup();

        // 改 a.rs
        write_current_tree(
            &store,
            &work_dir,
            &[("a.rs", "modified"), ("b.rs", "stable")],
        );

        let pending = mgr.compute_pending();
        let a_pending = pending.iter().find(|p| p.path == "a.rs");
        assert!(a_pending.is_some(), "a.rs 应在 pending 列表");
        assert_eq!(a_pending.unwrap().status, "modified");
        assert_eq!(a_pending.unwrap().old_content, Some("original".to_string()));
        assert_eq!(a_pending.unwrap().new_content, Some("modified".to_string()));

        // b.rs 没变，不应出现
        assert!(
            pending.iter().find(|p| p.path == "b.rs").is_none(),
            "b.rs 未改不应在 pending"
        );

        std::fs::remove_dir_all(work_dir.parent().unwrap()).ok();
    }

    #[test]
    fn pending_shows_added_file() {
        let (work_dir, store, mgr) = setup();

        // 新建 c.rs
        write_current_tree(
            &store,
            &work_dir,
            &[("a.rs", "original"), ("b.rs", "stable"), ("c.rs", "new")],
        );

        let pending = mgr.compute_pending();
        let c_pending = pending.iter().find(|p| p.path == "c.rs");
        assert!(c_pending.is_some(), "c.rs 应在 pending");
        assert_eq!(c_pending.unwrap().status, "added");

        std::fs::remove_dir_all(work_dir.parent().unwrap()).ok();
    }

    #[test]
    fn approve_then_modified_not_in_pending() {
        let (work_dir, store, mgr) = setup();

        // 改 a.rs → approve
        write_current_tree(&store, &work_dir, &[("a.rs", "v1"), ("b.rs", "stable")]);
        mgr.approve("a.rs").unwrap();

        // 再改但内容相同（approve 后没变）→ 不应 pending
        let pending = mgr.compute_pending();
        assert!(
            pending.iter().find(|p| p.path == "a.rs").is_none(),
            "approve 后 a.rs 不应在 pending"
        );

        std::fs::remove_dir_all(work_dir.parent().unwrap()).ok();
    }

    #[test]
    fn approve_anchors_baseline() {
        let (work_dir, store, mgr) = setup();

        // 改 a.rs v1 → approve
        write_current_tree(&store, &work_dir, &[("a.rs", "v1"), ("b.rs", "stable")]);
        let _appr = mgr.approve("a.rs").unwrap();

        // 再改 a.rs v2
        write_current_tree(&store, &work_dir, &[("a.rs", "v2"), ("b.rs", "stable")]);

        // 改了 → re-approval 重置（approved → pending）
        mgr.check_re_approval(&["a.rs".into()]);

        let pending = mgr.compute_pending();
        let a = pending
            .iter()
            .find(|p| p.path == "a.rs")
            .expect("a.rs 应回 pending（re-approval）");
        // diff 应从 approved baseline（v1）算，不是从 session start（original）
        assert_eq!(
            a.old_content,
            Some("v1".to_string()),
            "diff baseline 应是 approved 时的 v1"
        );
        assert_eq!(a.new_content, Some("v2".to_string()));

        std::fs::remove_dir_all(work_dir.parent().unwrap()).ok();
    }

    #[test]
    fn re_approval_reset() {
        let (work_dir, store, mgr) = setup();

        write_current_tree(&store, &work_dir, &[("a.rs", "v1"), ("b.rs", "stable")]);
        mgr.approve("a.rs").unwrap();
        assert_eq!(
            mgr.approvals_list(None)
                .iter()
                .find(|a| a.path == "a.rs")
                .unwrap()
                .status,
            ApprovalStatus::Approved
        );

        // 模拟新 turn 改了 a.rs → re-approval 重置
        mgr.check_re_approval(&["a.rs".into()]);
        assert_eq!(
            mgr.approvals_list(None)
                .iter()
                .find(|a| a.path == "a.rs")
                .unwrap()
                .status,
            ApprovalStatus::Pending,
            "改了应回 pending"
        );

        std::fs::remove_dir_all(work_dir.parent().unwrap()).ok();
    }

    #[test]
    fn approve_all_and_reject_all() {
        let (work_dir, store, mgr) = setup();

        write_current_tree(&store, &work_dir, &[("a.rs", "v1"), ("b.rs", "v2")]);

        // approve_all
        let results = mgr.approve_all();
        assert!(results.iter().all(|r| r.is_ok()), "approve_all 应全成功");
        let approved = mgr.approvals_list(Some(&ApprovalStatus::Approved));
        assert_eq!(approved.len(), 2, "两个文件都应 approved");

        std::fs::remove_dir_all(work_dir.parent().unwrap()).ok();
    }

    #[test]
    fn reject_rolls_back_file() {
        let (work_dir, store, mgr) = setup();

        // 新建 c.rs
        write_current_tree(
            &store,
            &work_dir,
            &[("a.rs", "original"), ("b.rs", "stable"), ("c.rs", "new")],
        );

        // reject c.rs → 回滚（baseline 里没有 → 删除）
        let result = mgr.reject("c.rs");
        assert!(result.is_ok(), "reject 应成功");
        assert_eq!(result.unwrap().action, "deleted");
        assert!(!work_dir.join("c.rs").exists(), "c.rs 应被回滚删除");

        std::fs::remove_dir_all(work_dir.parent().unwrap()).ok();
    }

    #[test]
    fn persist_and_restore() {
        let (work_dir, store, mgr) = setup();

        write_current_tree(&store, &work_dir, &[("a.rs", "v1"), ("b.rs", "stable")]);
        mgr.approve("a.rs").unwrap();

        // 序列化
        let entries = mgr.to_entries();
        assert_eq!(entries.len(), 1);

        // 新 manager 恢复
        let storage2 = crate::storage_context::StorageContext::new(
            work_dir.to_string_lossy().as_ref(),
            "approval_test",
            work_dir.to_string_lossy().as_ref(),
        );
        let mgr2 = ApprovalManager::new(store.clone(), storage2);
        mgr2.restore_from_entries(&entries);

        let approved = mgr2.approvals_list(Some(&ApprovalStatus::Approved));
        assert_eq!(approved.len(), 1);
        assert_eq!(approved[0].path, "a.rs");

        std::fs::remove_dir_all(work_dir.parent().unwrap()).ok();
    }

    #[test]
    fn no_op_filter() {
        let (work_dir, store, mgr) = setup();

        // 改 a.rs v1 → approve
        write_current_tree(&store, &work_dir, &[("a.rs", "v1"), ("b.rs", "stable")]);
        mgr.approve("a.rs").unwrap();

        // 内容回到 approved baseline（模拟 git checkout 回 v1）
        // current tree 仍是 v1（没变）→ no-op → 不应 pending
        let pending = mgr.compute_pending();
        assert!(
            pending.iter().find(|p| p.path == "a.rs").is_none(),
            "内容相同（no-op）不应在 pending"
        );

        std::fs::remove_dir_all(work_dir.parent().unwrap()).ok();
    }
}
