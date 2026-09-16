//! 统一审批总线（Approval Bus）— host 维护的三来源统一审批表
//!
//! 设计文档：[docs/design/APPROVAL_BUS.md]
//!
//! ION 原有三套互不相通的审批家族：
//! 1. host 级 UI Ask（permission 询问：`runtime::pending_ui` + `ui_respond`）
//! 2. worker 级 file-snapshot 审批（`file_snapshot::approval::ApprovalManager` + `review_*` RPC）
//! 3. host 级远程动词审批（`worker_registry::verb_approvals_global` + `verb_review` RPC）
//!
//! 本模块提供内核统一审批表：三类来源注册进同一张表，条目形状统一
//! （`ApprovalEntry`），id 统一 `apr_<hex>` 前缀。host 侧出口（RPC / 事件 /
//! snapshot）长在既有协议上，外部桥接方（webui / IM / 手机推送）用一个协议接完。
//!
//! 存储落位（对齐 AGENTS.md 存储原则）：**内存态不落盘**。审批是瞬态，
//! 宁丢也不建 sidecar 文件；file-snapshot 的持久恢复仍归它自己的 custom
//! 条目机制（session JSONL），本总线只做运行期统一视图。
//!
//! 幂等：注册携带 dedupe_key（来源侧的稳定标识），同 key 且未过期的重复
//! 注册不产生新条目（返回既有 id）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

/// 当前 Unix 毫秒
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// 条目与枚举
// ---------------------------------------------------------------------------

/// 审批来源家族（三来源映射，serde 值对齐 RPC 惯例：`ui_ask` | `file_snapshot` | `remote_verb`）
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalKind {
    /// host 级 UI Ask（permission 询问 / wasm ui_ask，pending_ui 表）
    UiAsk,
    /// worker 级 file-snapshot 审批（ApprovalManager，review_* RPC）
    FileSnapshot,
    /// host 级远程动词审批（verb_approvals 表，verb_review RPC）
    RemoteVerb,
}

impl ApprovalKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ApprovalKind::UiAsk => "ui_ask",
            ApprovalKind::FileSnapshot => "file_snapshot",
            ApprovalKind::RemoteVerb => "remote_verb",
        }
    }
}

/// 审批决定（`approval_respond` 的 decision 参数）
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Approve,
    Reject,
}

impl ApprovalDecision {
    /// 解析 RPC 参数（fail-closed：未知值一律 Reject 由调用方校验）
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "approve" => Some(ApprovalDecision::Approve),
            "reject" => Some(ApprovalDecision::Reject),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            ApprovalDecision::Approve => "approve",
            ApprovalDecision::Reject => "reject",
        }
    }
}

/// 统一审批条目。字段序列化为 camelCase（对齐 RPC 响应惯例）。
#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalEntry {
    /// 统一 id：`apr_<8hex>`（区别于 file-snapshot 原生 `appr_<ts>`）
    pub id: String,
    /// 来源家族
    pub kind: ApprovalKind,
    /// 关联会话（可为空串：来源未提供时）
    pub session_id: String,
    /// 关联 worker（可空）
    pub worker_id: Option<String>,
    /// 人读摘要（UI 列表直接渲染）
    pub summary: String,
    /// 来源原始载荷（含 nativeRequestId 等路由回源所需字段）
    pub payload: serde_json::Value,
    /// 注册时间（Unix ms）
    pub raised_at_ms: u64,
    /// 过期时间（Unix ms；None = 不过期）
    pub expires_at_ms: Option<u64>,
    /// 幂等键（来源侧稳定标识；不序列化）
    #[serde(skip)]
    pub dedupe_key: String,
}

impl ApprovalEntry {
    /// payload 里的来源原生 id（各来源注册时约定写入 `nativeRequestId`）
    pub fn native_request_id(&self) -> Option<&str> {
        self.payload
            .get("nativeRequestId")
            .and_then(|v| v.as_str())
    }
}

/// 注册结果：`newly_registered=false` 表示幂等命中既有条目
#[derive(Clone, Debug)]
pub struct RegisterOutcome {
    pub id: String,
    pub newly_registered: bool,
    pub entry: ApprovalEntry,
}

/// 非决定性移除的原因（决定性移除走 `Resolved`）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoveCause {
    /// 来源侧重置（file-snapshot re-approval reset 等）
    Reset,
    /// 来源侧已不存在 / 被替换（新请求顶掉旧请求）
    SourceGone,
    /// 惰性过期清理
    Expired,
}

impl RemoveCause {
    pub fn as_str(&self) -> &'static str {
        match self {
            RemoveCause::Reset => "reset",
            RemoveCause::SourceGone => "source_gone",
            RemoveCause::Expired => "expired",
        }
    }
}

/// 总线变化通知（host 侧据此广播 EventBus 事件）
#[derive(Clone, Debug)]
pub enum ApprovalChange {
    /// 新条目注册（幂等命中不触发）
    Registered(ApprovalEntry),
    /// 审批决定落地（approve/reject，含经旧 API 完成的镜像同步）
    Resolved {
        entry: ApprovalEntry,
        decision: ApprovalDecision,
        note: Option<String>,
    },
    /// 非决定性移除（重置 / 来源消失 / 过期）
    Removed {
        entry: ApprovalEntry,
        cause: RemoveCause,
    },
}

// ---------------------------------------------------------------------------
// 总线
// ---------------------------------------------------------------------------

#[derive(Default)]
struct BusInner {
    entries: HashMap<String, ApprovalEntry>,
    /// dedupe_key → entry id
    by_dedupe: HashMap<String, String>,
}

/// 统一审批总线。进程内单例（`ApprovalBus::global()`），内存态不落盘。
pub struct ApprovalBus {
    inner: Mutex<BusInner>,
    callbacks: Mutex<Vec<Arc<dyn Fn(ApprovalChange) + Send + Sync>>>,
}

impl Default for ApprovalBus {
    fn default() -> Self {
        Self::new()
    }
}

/// `approval_respond` 路由失败的分类（错误信息可直接透给 RPC 客户端）
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolveError {
    NotFound(String),
    Expired(String),
}

impl ResolveError {
    pub fn message(&self) -> String {
        match self {
            ResolveError::NotFound(id) => {
                format!("approval not found: {id} (unknown, already resolved, or source removed)")
            }
            ResolveError::Expired(id) => format!("approval expired: {id}"),
        }
    }
}

impl ApprovalBus {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(BusInner::default()),
            callbacks: Mutex::new(Vec::new()),
        }
    }

    /// host 进程级全局总线
    pub fn global() -> &'static ApprovalBus {
        static BUS: OnceLock<ApprovalBus> = OnceLock::new();
        BUS.get_or_init(ApprovalBus::new)
    }

    /// 注册变化回调（host 启动时接线一次；回调内禁止再进总线方法——回调在锁外执行）
    pub fn on_change(&self, cb: Arc<dyn Fn(ApprovalChange) + Send + Sync>) {
        self.callbacks.lock().unwrap().push(cb);
    }

    fn fire(&self, change: ApprovalChange) {
        let cbs: Vec<Arc<dyn Fn(ApprovalChange) + Send + Sync>> =
            self.callbacks.lock().unwrap().clone();
        for cb in cbs {
            cb(change.clone());
        }
    }

    fn new_id() -> String {
        format!("apr_{}", &uuid::Uuid::new_v4().simple().to_string()[..8])
    }

    /// 惰性清理已过期条目（返回被清理的条目，供调用方在锁外 fire）
    fn purge_expired_locked(inner: &mut BusInner, now: u64) -> Vec<ApprovalEntry> {
        let expired: Vec<String> = inner
            .entries
            .values()
            .filter(|e| e.expires_at_ms.is_some_and(|t| t <= now))
            .map(|e| e.id.clone())
            .collect();
        expired
            .into_iter()
            .filter_map(|id| {
                inner.by_dedupe.retain(|_, v| v != &id);
                inner.entries.remove(&id)
            })
            .collect()
    }

    /// 注册一条审批。
    ///
    /// 幂等：同 `dedupe_key` 的存活条目直接返回（`newly_registered=false`）。
    /// `ttl_ms`：注册即起算的存活窗口；None = 不过期（file-snapshot 等持续型）。
    pub fn register(
        &self,
        kind: ApprovalKind,
        session_id: &str,
        worker_id: Option<&str>,
        summary: &str,
        payload: serde_json::Value,
        dedupe_key: &str,
        ttl_ms: Option<u64>,
    ) -> RegisterOutcome {
        let now = now_ms();
        let mut to_fire: Vec<ApprovalChange> = Vec::new();
        let outcome = {
            let mut inner = self.inner.lock().unwrap();
            to_fire.extend(
                Self::purge_expired_locked(&mut inner, now)
                    .into_iter()
                    .map(|e| ApprovalChange::Removed {
                        entry: e,
                        cause: RemoveCause::Expired,
                    }),
            );
            if let Some(id) = inner.by_dedupe.get(dedupe_key)
                && let Some(existing) = inner.entries.get(id)
            {
                RegisterOutcome {
                    id: existing.id.clone(),
                    newly_registered: false,
                    entry: existing.clone(),
                }
            } else {
                let entry = ApprovalEntry {
                    id: Self::new_id(),
                    kind,
                    session_id: session_id.to_string(),
                    worker_id: worker_id.map(str::to_string),
                    summary: summary.to_string(),
                    payload,
                    raised_at_ms: now,
                    expires_at_ms: ttl_ms.map(|t| now + t),
                    dedupe_key: dedupe_key.to_string(),
                };
                let id = entry.id.clone();
                inner
                    .by_dedupe
                    .insert(dedupe_key.to_string(), id.clone());
                inner.entries.insert(id.clone(), entry.clone());
                to_fire.push(ApprovalChange::Registered(entry.clone()));
                RegisterOutcome {
                    id,
                    newly_registered: true,
                    entry,
                }
            }
        };
        for ch in to_fire {
            self.fire(ch);
        }
        outcome
    }

    /// 查询单条（过期视为不存在，惰性清理）
    pub fn get(&self, id: &str) -> Option<ApprovalEntry> {
        let now = now_ms();
        let mut inner = self.inner.lock().unwrap();
        let expired = Self::purge_expired_locked(&mut inner, now);
        drop(inner);
        for e in expired {
            self.fire(ApprovalChange::Removed {
                entry: e,
                cause: RemoveCause::Expired,
            });
        }
        let inner = self.inner.lock().unwrap();
        inner.entries.get(id).cloned()
    }

    /// 全量待审批列表（按 raisedAtMs 升序，同毫秒按 id 稳定排序；惰性清过期）
    pub fn pending(&self) -> Vec<ApprovalEntry> {
        self.pending_at(now_ms())
    }

    /// `pending` 的可注入时钟版（单测用）
    pub fn pending_at(&self, now: u64) -> Vec<ApprovalEntry> {
        let mut inner = self.inner.lock().unwrap();
        let expired = Self::purge_expired_locked(&mut inner, now);
        drop(inner);
        for e in expired {
            self.fire(ApprovalChange::Removed {
                entry: e,
                cause: RemoveCause::Expired,
            });
        }
        let inner = self.inner.lock().unwrap();
        let mut list: Vec<ApprovalEntry> = inner.entries.values().cloned().collect();
        drop(inner);
        list.sort_by(|a, b| {
            a.raised_at_ms
                .cmp(&b.raised_at_ms)
                .then_with(|| a.id.cmp(&b.id))
        });
        list
    }

    /// 落地审批决定：移除条目并广播 `Resolved`。
    /// 语义上"决定路由回来源执行"由调用方（host 出口）在调本方法之前完成；
    /// 本方法只负责统一表的收口。
    pub fn resolve(
        &self,
        id: &str,
        decision: ApprovalDecision,
        note: Option<&str>,
    ) -> Result<ApprovalEntry, ResolveError> {
        let now = now_ms();
        let mut inner = self.inner.lock().unwrap();
        let expired = Self::purge_expired_locked(&mut inner, now);
        let removed = inner.entries.remove(id);
        if removed.is_some() {
            inner.by_dedupe.retain(|_, v| v != id);
        }
        drop(inner);
        for e in expired {
            self.fire(ApprovalChange::Removed {
                entry: e,
                cause: RemoveCause::Expired,
            });
        }
        match removed {
            Some(entry) => {
                self.fire(ApprovalChange::Resolved {
                    entry: entry.clone(),
                    decision,
                    note: note.map(str::to_string),
                });
                Ok(entry)
            }
            None => {
                // 区分"从未存在"与"刚过期"对客户端没有额外价值，统一 NotFound 语义
                Err(ResolveError::NotFound(id.to_string()))
            }
        }
    }

    /// 按谓词移除（监听器同步来源侧消失/重置用）。返回被移除的条目。
    pub fn remove_matching(
        &self,
        pred: impl Fn(&ApprovalEntry) -> bool,
        cause: RemoveCause,
    ) -> Vec<ApprovalEntry> {
        let mut inner = self.inner.lock().unwrap();
        let hits: Vec<String> = inner
            .entries
            .values()
            .filter(|e| pred(e))
            .map(|e| e.id.clone())
            .collect();
        let removed: Vec<ApprovalEntry> = hits
            .into_iter()
            .filter_map(|id| {
                inner.by_dedupe.retain(|_, v| v != &id);
                inner.entries.remove(&id)
            })
            .collect();
        drop(inner);
        for e in &removed {
            self.fire(ApprovalChange::Removed {
                entry: e.clone(),
                cause,
            });
        }
        removed
    }

    /// 把一条已注册条目标记为已决定（监听器同步旧 API 完成的决定用：
    /// `verb_review` / `ui_respond` 走旧通路时，镜像条目同步收口）。
    /// 找不到（从未注册/已被收口）时静默返回 None。
    pub fn mark_resolved_matching(
        &self,
        pred: impl Fn(&ApprovalEntry) -> bool,
        decision: ApprovalDecision,
        note: Option<&str>,
    ) -> Option<ApprovalEntry> {
        let mut inner = self.inner.lock().unwrap();
        let hit: Option<String> = inner
            .entries
            .values()
            .find(|e| pred(e))
            .map(|e| e.id.clone());
        let removed = hit.and_then(|id| {
            inner.by_dedupe.retain(|_, v| v != &id);
            inner.entries.remove(&id)
        });
        drop(inner);
        if let Some(entry) = removed {
            self.fire(ApprovalChange::Resolved {
                entry: entry.clone(),
                decision,
                note: note.map(str::to_string),
            });
            Some(entry)
        } else {
            None
        }
    }

    /// 清空（测试用 / host 进程内重置）
    pub fn clear(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.entries.clear();
        inner.by_dedupe.clear();
    }
}

// ---------------------------------------------------------------------------
// 单元测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn bus() -> ApprovalBus {
        ApprovalBus::new()
    }

    #[test]
    fn register_returns_apr_prefixed_id() {
        let b = bus();
        let out = b.register(
            ApprovalKind::UiAsk,
            "sess_1",
            None,
            "allow bash?",
            serde_json::json!({"nativeRequestId": "req_ab12"}),
            "ask:req_ab12",
            None,
        );
        assert!(out.id.starts_with("apr_"), "id 统一前缀: {}", out.id);
        assert!(out.newly_registered);
        assert_eq!(out.entry.kind, ApprovalKind::UiAsk);
        assert_eq!(out.entry.session_id, "sess_1");
    }

    #[test]
    fn register_is_idempotent_on_dedupe_key() {
        let b = bus();
        let a = b.register(
            ApprovalKind::RemoteVerb,
            "s",
            None,
            "fs.read",
            serde_json::json!({"nativeRequestId": "vapp_1"}),
            "verb:vapp_1",
            None,
        );
        let again = b.register(
            ApprovalKind::RemoteVerb,
            "s",
            None,
            "fs.read",
            serde_json::json!({"nativeRequestId": "vapp_1"}),
            "verb:vapp_1",
            None,
        );
        assert!(!again.newly_registered);
        assert_eq!(a.id, again.id, "同来源同 key 重复注册返回既有 id");
        assert_eq!(b.pending().len(), 1, "不产生新条目");
    }

    #[test]
    fn pending_sorted_by_raised_at() {
        let b = bus();
        // 同一毫秒内注册两条，靠 id 稳定排序；再造一条更早的
        let e1 = b.register(
            ApprovalKind::UiAsk,
            "s",
            None,
            "a",
            serde_json::json!({}),
            "k1",
            None,
        );
        let e2 = b.register(
            ApprovalKind::RemoteVerb,
            "s",
            None,
            "b",
            serde_json::json!({}),
            "k2",
            None,
        );
        let list = b.pending();
        assert_eq!(list.len(), 2);
        let ids: Vec<&str> = list.iter().map(|e| e.id.as_str()).collect();
        assert!(ids.contains(&e1.id.as_str()));
        assert!(ids.contains(&e2.id.as_str()));
        // 同毫秒：按 id 字典序
        let sorted_correctly = list.windows(2).all(|w| {
            w[0].raised_at_ms < w[1].raised_at_ms
                || (w[0].raised_at_ms == w[1].raised_at_ms && w[0].id <= w[1].id)
        });
        assert!(sorted_correctly);
    }

    #[test]
    fn expiry_is_lazy_and_fires_removed() {
        let b = bus();
        let fired = Arc::new(Mutex::new(Vec::new()));
        let sink = fired.clone();
        b.on_change(Arc::new(move |ch| {
            if let ApprovalChange::Removed { cause, .. } = ch {
                sink.lock().unwrap().push(cause);
            }
        }));
        let t0 = now_ms();
        b.register(
            ApprovalKind::UiAsk,
            "s",
            None,
            "x",
            serde_json::json!({}),
            "k",
            Some(100),
        );
        assert_eq!(b.pending_at(t0 + 50).len(), 1, "未过期可见");
        assert_eq!(b.pending_at(t0 + 150).len(), 0, "过期后惰性清理");
        assert_eq!(
            fired.lock().unwrap().as_slice(),
            &[RemoveCause::Expired],
            "过期清理触发 Removed{{Expired}} 回调"
        );
        // 清理后 dedupe 释放：同 key 可重新注册
        let re = b.register(
            ApprovalKind::UiAsk,
            "s",
            None,
            "x",
            serde_json::json!({}),
            "k",
            None,
        );
        assert!(re.newly_registered);
    }

    #[test]
    fn expired_entry_can_reregister_after_purge() {
        let b = bus();
        b.register(
            ApprovalKind::RemoteVerb,
            "s",
            None,
            "x",
            serde_json::json!({}),
            "verb:1",
            Some(10),
        );
        // 旧的已过期但未触发清理 → register 先惰性清理再查 dedupe → 新条目
        let _ = b.pending_at(now_ms() + 100); // 用未来时钟强制惰性清理
        let out = b.register(
            ApprovalKind::RemoteVerb,
            "s",
            None,
            "x",
            serde_json::json!({}),
            "verb:1",
            None,
        );
        assert!(out.newly_registered, "过期条目不阻塞同 key 重注册");
    }

    #[test]
    fn resolve_removes_and_fires() {
        let b = bus();
        let fired = Arc::new(Mutex::new(Vec::new()));
        let sink = fired.clone();
        b.on_change(Arc::new(move |ch| {
            if let ApprovalChange::Resolved { decision, .. } = ch {
                sink.lock().unwrap().push(decision);
            }
        }));
        let out = b.register(
            ApprovalKind::UiAsk,
            "s",
            None,
            "x",
            serde_json::json!({"nativeRequestId": "req_1"}),
            "ask:req_1",
            None,
        );
        let entry = b
            .resolve(&out.id, ApprovalDecision::Approve, Some("by ci"))
            .expect("resolve ok");
        assert_eq!(entry.id, out.id);
        assert_eq!(
            fired.lock().unwrap().as_slice(),
            &[ApprovalDecision::Approve]
        );
        // 再 resolve 同 id → NotFound
        let err = b.resolve(&out.id, ApprovalDecision::Reject, None).unwrap_err();
        assert!(matches!(err, ResolveError::NotFound(_)));
        assert!(err.message().contains(out.id.as_str()));
    }

    #[test]
    fn mark_resolved_matching_syncs_old_api_path() {
        let b = bus();
        b.register(
            ApprovalKind::RemoteVerb,
            "s",
            None,
            "fs.read",
            serde_json::json!({"nativeRequestId": "vapp_9"}),
            "verb:vapp_9",
            None,
        );
        let hit = b.mark_resolved_matching(
            |e| e.kind == ApprovalKind::RemoteVerb && e.native_request_id() == Some("vapp_9"),
            ApprovalDecision::Reject,
            Some("verb_review"),
        );
        assert!(hit.is_some());
        assert!(b.pending().is_empty());
        // 二次同步静默
        let again = b.mark_resolved_matching(
            |e| e.native_request_id() == Some("vapp_9"),
            ApprovalDecision::Reject,
            None,
        );
        assert!(again.is_none());
    }

    #[test]
    fn remove_matching_supports_source_gone() {
        let b = bus();
        b.register(
            ApprovalKind::FileSnapshot,
            "s1",
            Some("w1"),
            "2 files",
            serde_json::json!({"files": []}),
            "fs:s1:a",
            None,
        );
        b.register(
            ApprovalKind::FileSnapshot,
            "s2",
            None,
            "1 files",
            serde_json::json!({"files": []}),
            "fs:s2:b",
            None,
        );
        let removed = b.remove_matching(|e| e.session_id == "s1", RemoveCause::SourceGone);
        assert_eq!(removed.len(), 1);
        assert_eq!(b.pending().len(), 1);
        assert_eq!(b.pending()[0].session_id, "s2");
    }

    #[test]
    fn entry_serializes_camel_case() {
        let b = bus();
        let out = b.register(
            ApprovalKind::FileSnapshot,
            "sess_x",
            Some("wrk_y"),
            "sum",
            serde_json::json!({"nativeRequestId": "appr_1"}),
            "fs:1",
            Some(1000),
        );
        let v = serde_json::to_value(&out.entry).unwrap();
        assert_eq!(v["id"], out.id);
        assert_eq!(v["kind"], "file_snapshot");
        assert_eq!(v["sessionId"], "sess_x");
        assert_eq!(v["workerId"], "wrk_y");
        assert_eq!(v["summary"], "sum");
        assert_eq!(v["payload"]["nativeRequestId"], "appr_1");
        assert!(v["raisedAtMs"].is_u64());
        assert!(v["expiresAtMs"].is_u64());
        // dedupe_key 不外泄
        assert!(v.get("dedupe_key").is_none());
        assert!(v.get("dedupeKey").is_none());
    }

    #[test]
    fn decision_parse_and_str() {
        assert_eq!(
            ApprovalDecision::parse("approve"),
            Some(ApprovalDecision::Approve)
        );
        assert_eq!(
            ApprovalDecision::parse("reject"),
            Some(ApprovalDecision::Reject)
        );
        assert!(ApprovalDecision::parse("yes").is_none());
        assert!(ApprovalDecision::parse("Allow").is_none());
        assert_eq!(ApprovalDecision::Approve.as_str(), "approve");
    }

    #[test]
    fn global_bus_roundtrip() {
        // 全局总线基本可用（不与其它测试共享状态：clear 后再注册）
        let g = ApprovalBus::global();
        g.clear();
        let out = g.register(
            ApprovalKind::UiAsk,
            "",
            None,
            "g",
            serde_json::json!({}),
            "test:global",
            None,
        );
        assert!(g.get(&out.id).is_some());
        g.clear();
        assert!(g.pending().is_empty());
    }

    #[test]
    fn kind_str_roundtrip() {
        assert_eq!(ApprovalKind::UiAsk.as_str(), "ui_ask");
        assert_eq!(ApprovalKind::FileSnapshot.as_str(), "file_snapshot");
        assert_eq!(ApprovalKind::RemoteVerb.as_str(), "remote_verb");
        let k: ApprovalKind = serde_json::from_str("\"remote_verb\"").unwrap();
        assert_eq!(k, ApprovalKind::RemoteVerb);
    }
}
