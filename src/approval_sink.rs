//! ApprovalSink — 统一审批总线的**来源侧窄接口**（M2 定义，供 M1 合并对齐）。
//!
//! ## 背景
//! ION 有三套互不相识的审批来源：
//! 1. **ui_ask**：worker 进程内 `SecuredRuntime::resolve_ask`（CommandGuard 中危/
//!    PermissionEngine Ask 规则触发），pending 在 worker 进程内 `runtime::pending_ui()`，
//!    事件经 worker stdout → host pump → EventBus 广播。J8 缺口：host 不登记 →
//!    `ui_respond` 报 "request not found or already expired"。
//! 2. **remote_verb**：`worker_registry::verb_approvals_global()` 全局表
//!    （verb_pending/verb_review 一套自有 API）。
//! 3. **file_snapshot**：worker 内核 `file_snapshot::approval::ApprovalManager`
//!    （review_pending/review_approve worker 级 RPC），事件 ApprovalRequest/
//!    ApprovalResolved 经 worker stdout 上报。
//!
//! M1 正在并行实现统一总线（`src/approval_bus.rs` + host RPC approvals_pending/
//! approval_respond）。本文件是 M2 来源侧接入的**隔离层**：三来源只调这里的
//! `register/resolve/resolve_kind_for_worker/pending`，默认 NoopSink（master
//! 现状零行为变化）。合并时协调者把全局 sink 换成 M1 的真实现（或让 M1 的
//! ApprovalBus implement 本 trait），来源侧逻辑与测试不用动。
//!
//! ## 合并对齐契约（给协调者）
//! - `ApprovalSink::register(entry)`：新审批出现（幂等：同 id 重复注册刷新条目）
//! - `ApprovalSink::resolve(id, decision)`：审批完成（allow/deny/approve/reject/timeout）
//! - `ApprovalSink::resolve_kind_for_worker(worker_id, kind, decision)`：按来源批量
//!   消除（file_snapshot 的 ApprovalRequest 是"当前 pending 全集快照"，新请求应
//!   替换同 worker 同 kind 的旧条目）
//! - `ApprovalSink::pending()`：host RPC approvals_pending 的数据源
//! - `respond_route(entry, decision)`：approval_respond 的路由决策——决定 host
//!   把决定转发到哪（worker 命令 / verb_review），M1 的 RPC 直接调用

use std::sync::Arc;
use std::sync::RwLock;

/// 审批来源种类
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ApprovalKind {
    /// worker 进程内权限引擎 Ask（runtime::resolve_ask）
    UiAsk,
    /// 远程 worker verb 调用审批（worker_registry::verb_approvals）
    RemoteVerb,
    /// file-snapshot 文件审批（file_snapshot::approval）
    FileSnapshot,
}

impl ApprovalKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ApprovalKind::UiAsk => "ui_ask",
            ApprovalKind::RemoteVerb => "remote_verb",
            ApprovalKind::FileSnapshot => "file_snapshot",
        }
    }
}

/// 统一审批表的一条条目（对齐 M1 的 ApprovalEntry 形状）
#[derive(Clone, Debug)]
pub struct ApprovalEntry {
    /// 原始请求 ID（ui_ask: req_xxx / remote_verb: vapp_xxx / file_snapshot: appr_xxx）
    pub id: String,
    /// 来源种类
    pub kind: ApprovalKind,
    /// 产生该审批的 worker id（host 路由 respond 用）
    pub worker_id: String,
    /// 会话 ID
    pub session_id: String,
    /// 原始载荷（Ask: request_id/title/message；verb: verb/args；file: files/total）
    pub payload: serde_json::Value,
    /// 创建时间（ms since epoch）
    pub created_at_ms: u64,
}

impl ApprovalEntry {
    /// 序列化成 host RPC / 事件里的 JSON 形状
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "kind": self.kind.as_str(),
            "workerId": self.worker_id,
            "sessionId": self.session_id,
            "payload": self.payload,
            "createdAtMs": self.created_at_ms,
        })
    }
}

/// 统一审批总线的来源侧窄接口（详见模块文档"合并对齐契约"）。
pub trait ApprovalSink: Send + Sync {
    /// 新审批出现（幂等：同 id 重复注册刷新条目）
    fn register(&self, entry: ApprovalEntry);
    /// 审批完成：移除条目并记录 decision
    fn resolve(&self, id: &str, decision: &str);
    /// 按 worker + kind 批量消除（file_snapshot 快照替换语义用）
    fn resolve_kind_for_worker(&self, worker_id: &str, kind: ApprovalKind, decision: &str);
    /// 当前 pending 条目（approvals_pending 数据源）
    fn pending(&self) -> Vec<ApprovalEntry>;
}

/// master 现状的 no-op 实现（默认安装——不接总线时行为零变化）。
pub struct NoopSink;

impl ApprovalSink for NoopSink {
    fn register(&self, _entry: ApprovalEntry) {}
    fn resolve(&self, _id: &str, _decision: &str) {}
    fn resolve_kind_for_worker(&self, _worker_id: &str, _kind: ApprovalKind, _decision: &str) {}
    fn pending(&self) -> Vec<ApprovalEntry> {
        Vec::new()
    }
}

/// 测试/CI 观察用实现：登记与消除全部留痕，pending 可查询。
/// host 侧 CI（approval_bus_ci.sh）通过 `_m2_approvals_pending` 临时 RPC 读它。
pub struct RecordingSink {
    entries: RwLock<Vec<ApprovalEntry>>,
    /// (id, decision) 审计轨迹
    pub resolutions: RwLock<Vec<(String, String)>>,
}

impl Default for RecordingSink {
    fn default() -> Self {
        Self::new()
    }
}

impl RecordingSink {
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(Vec::new()),
            resolutions: RwLock::new(Vec::new()),
        }
    }
}

impl ApprovalSink for RecordingSink {
    fn register(&self, entry: ApprovalEntry) {
        let mut es = self.entries.write().unwrap();
        // 幂等：同 id 先删后插（刷新）
        es.retain(|e| e.id != entry.id);
        es.push(entry);
    }

    fn resolve(&self, id: &str, decision: &str) {
        let mut es = self.entries.write().unwrap();
        es.retain(|e| e.id != id);
        drop(es);
        self.resolutions.write().unwrap().push((id.to_string(), decision.to_string()));
    }

    fn resolve_kind_for_worker(&self, worker_id: &str, kind: ApprovalKind, decision: &str) {
        let mut es = self.entries.write().unwrap();
        let removed: Vec<String> = es
            .iter()
            .filter(|e| e.worker_id == worker_id && e.kind == kind)
            .map(|e| e.id.clone())
            .collect();
        es.retain(|e| !(e.worker_id == worker_id && e.kind == kind));
        drop(es);
        let mut res = self.resolutions.write().unwrap();
        for id in removed {
            res.push((id, decision.to_string()));
        }
    }

    fn pending(&self) -> Vec<ApprovalEntry> {
        self.entries.read().unwrap().clone()
    }
}

// ---------------------------------------------------------------------------
// 全局安装点（进程级单例；host 在启动时换真实现，测试换 RecordingSink）
// ---------------------------------------------------------------------------

static SINK: RwLock<Option<Arc<dyn ApprovalSink>>> = RwLock::new(None);

/// 安装全局 sink（重复调用替换——host 启动 / 测试 setup 各自调用）。
pub fn set_sink(s: Arc<dyn ApprovalSink>) {
    *SINK.write().unwrap() = Some(s);
}

/// 取全局 sink（未安装时返回 NoopSink 的共享实例——master 现状零行为）。
pub fn sink() -> Arc<dyn ApprovalSink> {
    let g = SINK.read().unwrap();
    match g.as_ref() {
        Some(s) => s.clone(),
        None => Arc::new(NoopSink),
    }
}

// ---------------------------------------------------------------------------
// 事件 → 登记的收敛入口（host pump 调；"哪些事件算审批来源"的知识只住这里）
// ---------------------------------------------------------------------------

/// host event-pump 在转发 worker extension_event 时顺手调用：
/// - customType "Ask" → 登记 ui_ask（payload 带 request_id）
/// - "AskResolved" / "AskTimedOut" → 消除对应 ui_ask
/// - "ApprovalRequest"（file-snapshot）→ 替换登记该 worker 的 file_snapshot 条目
/// - "ApprovalResolved" → 消除该 worker 的 file_snapshot 条目（file-snapshot 事件
///   不带 requestId，按 worker+kind 消除；单个文件 approve 后仍有 pending 时下一条
///   ApprovalRequest 会重新登记——快照语义）
/// - 其它 customType → 忽略（快速返回）
pub fn try_register_from_worker_event(
    worker_id: &str,
    session_id: &str,
    custom_type: &str,
    data: &serde_json::Value,
) {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let s = sink();
    match custom_type {
        "Ask" => {
            let request_id = data
                .get("request_id")
                .or_else(|| data.get("requestId"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if request_id.is_empty() {
                return;
            }
            s.register(ApprovalEntry {
                id: request_id,
                kind: ApprovalKind::UiAsk,
                worker_id: worker_id.to_string(),
                session_id: session_id.to_string(),
                payload: data.clone(),
                created_at_ms: now_ms,
            });
        }
        "AskResolved" | "AskTimedOut" => {
            let request_id = data
                .get("request_id")
                .or_else(|| data.get("requestId"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if !request_id.is_empty() {
                let decision = data
                    .get("response")
                    .and_then(|v| v.as_str())
                    .unwrap_or(if custom_type == "AskTimedOut" { "timeout" } else { "resolved" });
                s.resolve(request_id, decision);
            }
        }
        "ApprovalRequest" => {
            let request_id = data
                .get("requestId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if request_id.is_empty() {
                return;
            }
            // 快照替换语义：新 ApprovalRequest 代表该 worker 当前 pending 全集，
            // 旧的 appr_* 条目已过时
            s.resolve_kind_for_worker(worker_id, ApprovalKind::FileSnapshot, "superseded");
            s.register(ApprovalEntry {
                id: request_id,
                kind: ApprovalKind::FileSnapshot,
                worker_id: worker_id.to_string(),
                session_id: session_id.to_string(),
                payload: data.clone(),
                created_at_ms: now_ms,
            });
        }
        "ApprovalResolved" => {
            // file-snapshot 的 ApprovalResolved 不带 requestId（带 path/decision）——
            // 按 worker+kind 消除；剩余 pending 会由下一条 ApprovalRequest 重建
            s.resolve_kind_for_worker(worker_id, ApprovalKind::FileSnapshot, "resolved");
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// respond 路由（approval_respond 的决策核心；host 执行，lib 可单测）
// ---------------------------------------------------------------------------

/// decision 归一化：批准类词 → true，拒绝类词 → false，其它 → 错误
pub fn normalize_decision(decision: &str) -> Result<bool, String> {
    match decision.trim().to_lowercase().as_str() {
        "allow" | "approve" | "approved" | "yes" | "true" | "accept" | "ok" => Ok(true),
        "deny" | "denied" | "reject" | "rejected" | "no" | "false" | "decline" => Ok(false),
        other => Err(format!(
            "invalid decision '{other}' (expected allow/approve or deny/reject)"
        )),
    }
}

/// approval_respond 的路由决策：根据条目来源决定 host 把决定发到哪。
///
/// - UiAsk → 经 send_to_worker 发 `ask_respond` 给产生该 Ask 的 worker
///   （worker 侧从 runtime::pending_ui 取 oneshot 放行工具执行）
/// - RemoteVerb → host 内 `worker_registry::verb_review(request_id, approve)`
/// - FileSnapshot → 经 send_to_worker 发 `review_approve_all` / `review_reject_all`
#[derive(Debug)]
pub enum RespondRoute {
    /// host 经 registry.send_to_worker(worker_id, method, params) 转发
    WorkerCommand {
        worker_id: String,
        method: &'static str,
        params: serde_json::Value,
    },
    /// host 内直接调 worker_registry::verb_review
    VerbReview { request_id: String, approve: bool },
}

/// 由条目 + decision 计算路由（纯函数，host 侧执行见 bin/ion.rs execute 路径）。
pub fn respond_route(entry: &ApprovalEntry, decision: &str) -> Result<RespondRoute, String> {
    let approve = normalize_decision(decision)?;
    Ok(match entry.kind {
        ApprovalKind::UiAsk => {
            let request_id = entry
                .payload
                .get("request_id")
                .or_else(|| entry.payload.get("requestId"))
                .and_then(|v| v.as_str())
                .unwrap_or(entry.id.as_str())
                .to_string();
            RespondRoute::WorkerCommand {
                worker_id: entry.worker_id.clone(),
                method: "ask_respond",
                params: serde_json::json!({
                    "request_id": request_id,
                    "response": if approve { "allow" } else { "deny" },
                }),
            }
        }
        ApprovalKind::RemoteVerb => RespondRoute::VerbReview {
            request_id: entry.id.clone(),
            approve,
        },
        ApprovalKind::FileSnapshot => RespondRoute::WorkerCommand {
            worker_id: entry.worker_id.clone(),
            method: if approve {
                "review_approve_all"
            } else {
                "review_reject_all"
            },
            params: serde_json::Value::Null,
        },
    })
}

/// remote_verb 来源的条目构造（worker_registry 接线处调用；抽出便于单测形状）。
pub fn verb_entry(
    request_id: &str,
    worker_session: &str,
    worker_id: &str,
    verb: &str,
    args: &serde_json::Value,
) -> ApprovalEntry {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    ApprovalEntry {
        id: request_id.to_string(),
        kind: ApprovalKind::RemoteVerb,
        worker_id: worker_id.to_string(),
        session_id: worker_session.to_string(),
        payload: serde_json::json!({
            "verb": verb,
            "args": args,
        }),
        created_at_ms: now_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: ApprovalKind, id: &str, worker: &str) -> ApprovalEntry {
        ApprovalEntry {
            id: id.to_string(),
            kind,
            worker_id: worker.to_string(),
            session_id: format!("sess_{worker}"),
            payload: serde_json::json!({"request_id": id, "title": "t", "message": "m"}),
            created_at_ms: 1,
        }
    }

    #[test]
    fn noop_sink_is_default_and_silent() {
        // 未安装时 sink() 返回 no-op：register/resolve 不 panic、pending 为空
        let s = sink();
        s.register(entry(ApprovalKind::UiAsk, "req_x", "w1"));
        s.resolve("req_x", "allow");
        assert!(s.pending().is_empty());
    }

    #[test]
    fn recording_sink_register_idempotent_and_resolve() {
        set_sink(Arc::new(RecordingSink::new()));
        let s = sink();
        s.register(entry(ApprovalKind::UiAsk, "req_1", "w1"));
        s.register(entry(ApprovalKind::UiAsk, "req_1", "w1"));
        assert_eq!(s.pending().len(), 1, "同 id 重复注册应刷新不叠加");
        s.resolve("req_1", "allow");
        assert!(s.pending().is_empty());
        // 恢复 no-op，避免污染其它测试
        set_sink(Arc::new(NoopSink));
    }

    #[test]
    fn recording_sink_resolve_kind_for_worker_scopes() {
        set_sink(Arc::new(RecordingSink::new()));
        let s = sink();
        s.register(entry(ApprovalKind::FileSnapshot, "appr_1", "w1"));
        s.register(entry(ApprovalKind::FileSnapshot, "appr_2", "w1"));
        s.register(entry(ApprovalKind::FileSnapshot, "appr_3", "w2"));
        s.register(entry(ApprovalKind::UiAsk, "req_9", "w1"));
        s.resolve_kind_for_worker("w1", ApprovalKind::FileSnapshot, "superseded");
        let left = s.pending();
        assert_eq!(left.len(), 2, "w2 的 file 条目与 w1 的 ui_ask 应保留");
        assert!(left.iter().any(|e| e.id == "appr_3"));
        assert!(left.iter().any(|e| e.id == "req_9"));
        set_sink(Arc::new(NoopSink));
    }

    #[test]
    fn try_register_ask_and_resolved() {
        set_sink(Arc::new(RecordingSink::new()));
        let s = sink();
        try_register_from_worker_event(
            "w1",
            "s1",
            "Ask",
            &serde_json::json!({"request_id": "req_a", "title": "高危命令", "message": "..."}),
        );
        let p = s.pending();
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].kind, ApprovalKind::UiAsk);
        assert_eq!(p[0].worker_id, "w1");
        assert_eq!(p[0].session_id, "s1");
        try_register_from_worker_event(
            "w1",
            "s1",
            "AskResolved",
            &serde_json::json!({"request_id": "req_a", "response": "allow"}),
        );
        assert!(s.pending().is_empty());
        set_sink(Arc::new(NoopSink));
    }

    #[test]
    fn try_register_file_snapshot_snapshot_replacement() {
        set_sink(Arc::new(RecordingSink::new()));
        let s = sink();
        try_register_from_worker_event(
            "w1",
            "s1",
            "ApprovalRequest",
            &serde_json::json!({"requestId": "appr_1", "total": 2, "files": []}),
        );
        try_register_from_worker_event(
            "w1",
            "s1",
            "ApprovalRequest",
            &serde_json::json!({"requestId": "appr_2", "total": 3, "files": []}),
        );
        let p = s.pending();
        assert_eq!(p.len(), 1, "新 ApprovalRequest 应替换旧 file_snapshot 条目");
        assert_eq!(p[0].id, "appr_2");
        // ApprovalResolved 不带 requestId → 按 worker+kind 消除
        try_register_from_worker_event(
            "w1",
            "s1",
            "ApprovalResolved",
            &serde_json::json!({"path": "a.rs", "decision": "approved"}),
        );
        assert!(s.pending().is_empty());
        set_sink(Arc::new(NoopSink));
    }

    #[test]
    fn try_register_ignores_unrelated_events() {
        set_sink(Arc::new(RecordingSink::new()));
        let s = sink();
        try_register_from_worker_event("w1", "s1", "memory_saved", &serde_json::json!({}));
        try_register_from_worker_event("w1", "s1", "SettingsChanged", &serde_json::json!({}));
        // Ask 但缺 request_id → 忽略
        try_register_from_worker_event("w1", "s1", "Ask", &serde_json::json!({"title": "x"}));
        assert!(s.pending().is_empty());
        set_sink(Arc::new(NoopSink));
    }

    #[test]
    fn respond_route_ui_ask_maps_to_worker_command() {
        let e = entry(ApprovalKind::UiAsk, "req_a", "w1");
        match respond_route(&e, "allow").unwrap() {
            RespondRoute::WorkerCommand {
                worker_id,
                method,
                params,
            } => {
                assert_eq!(worker_id, "w1");
                assert_eq!(method, "ask_respond");
                assert_eq!(params["request_id"], "req_a");
                assert_eq!(params["response"], "allow");
            }
            other => panic!("expected WorkerCommand, got {other:?}"),
        }
        match respond_route(&e, "deny").unwrap() {
            RespondRoute::WorkerCommand { params, .. } => assert_eq!(params["response"], "deny"),
            other => panic!("expected WorkerCommand, got {other:?}"),
        }
    }

    #[test]
    fn respond_route_verb_maps_to_verb_review() {
        let e = entry(ApprovalKind::RemoteVerb, "vapp_1", "w2");
        match respond_route(&e, "approve").unwrap() {
            RespondRoute::VerbReview { request_id, approve } => {
                assert_eq!(request_id, "vapp_1");
                assert!(approve);
            }
            other => panic!("expected VerbReview, got {other:?}"),
        }
    }

    #[test]
    fn respond_route_file_snapshot_maps_to_review_all() {
        let e = entry(ApprovalKind::FileSnapshot, "appr_1", "w3");
        match respond_route(&e, "allow").unwrap() {
            RespondRoute::WorkerCommand { method, .. } => assert_eq!(method, "review_approve_all"),
            other => panic!("expected WorkerCommand, got {other:?}"),
        }
        match respond_route(&e, "reject").unwrap() {
            RespondRoute::WorkerCommand { method, .. } => assert_eq!(method, "review_reject_all"),
            other => panic!("expected WorkerCommand, got {other:?}"),
        }
    }

    #[test]
    fn respond_route_rejects_invalid_decision() {
        let e = entry(ApprovalKind::UiAsk, "req_a", "w1");
        assert!(respond_route(&e, "maybe").is_err());
    }

    #[test]
    fn normalize_decision_variants() {
        assert!(normalize_decision("ALLOW").unwrap());
        assert!(normalize_decision(" approve ").unwrap());
        assert!(!normalize_decision("Deny").unwrap());
        assert!(!normalize_decision("rejected").unwrap());
        assert!(normalize_decision("banana").is_err());
    }

    #[test]
    fn verb_entry_shape() {
        let e = verb_entry("vapp_9", "sess_v", "w_v", "http.fetch", &serde_json::json!({"url":"https://x"}));
        assert_eq!(e.kind, ApprovalKind::RemoteVerb);
        assert_eq!(e.id, "vapp_9");
        assert_eq!(e.session_id, "sess_v");
        assert_eq!(e.worker_id, "w_v");
        assert_eq!(e.payload["verb"], "http.fetch");
        assert_eq!(e.payload["args"]["url"], "https://x");
        assert!(e.created_at_ms > 0);
    }

    #[test]
    fn entry_to_json_shape() {
        let e = entry(ApprovalKind::UiAsk, "req_j", "wJ");
        let j = e.to_json();
        assert_eq!(j["id"], "req_j");
        assert_eq!(j["kind"], "ui_ask");
        assert_eq!(j["workerId"], "wJ");
        assert_eq!(j["sessionId"], "sess_wJ");
        assert!(j.get("payload").is_some());
        assert!(j.get("createdAtMs").is_some());
    }
}
