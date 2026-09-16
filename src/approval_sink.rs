//! ApprovalSink — 统一审批总线的**来源侧窄接口**。
//!
//! ## 背景
//! ION 有三套互不相识的审批来源：
//! 1. **ui_ask**：worker 进程内 `SecuredRuntime::resolve_ask`（CommandGuard 中危/
//!    PermissionEngine Ask 规则触发），pending 在 worker 进程内 `runtime::pending_ui()`，
//!    事件经 worker stdout → host pump → 登记总线。应答经 `ask_respond` 命令回流
//!    worker 放行（J8 缺口闭环）。
//! 2. **remote_verb**：`worker_registry::verb_approvals_global()` 全局表
//!    （verb_pending/verb_review 一套自有 API）；verb_gate ask_on_deny 时直登总线。
//! 3. **file_snapshot**：worker 内核 `file_snapshot::approval::ApprovalManager`
//!    （review_pending/review_approve worker 级 RPC），事件 ApprovalRequest/
//!    ApprovalResolved 经 worker stdout 上报。
//!
//! ## 单一数据源（集成后终态）
//! 生产 sink 是 [`BusBackedSink`]——所有登记/消除转发进
//! `approval_bus::ApprovalBus::global()`（统一审批总线，host 唯一表）。
//! host 侧镜像监听（`bin/ion.rs mirror_ui_event_to_bus`）与 pump 登记路径
//! （[`try_register_from_worker_event`]）双路写同一张表，靠 dedupe key 对齐
//! 幂等去重：pump 登记先于 EventBus 广播 → 带 worker/session 的条目胜出。
//! `NoopSink` 仅在未安装时兜底；`RecordingSink` 保留为测试替身（lib 单测用）。
//!
//! ## 合并对齐契约
//! - `ApprovalSink::register(entry)`：新审批出现（BusBackedSink → bus.register，
//!   dedupe 语义对齐镜像路径：`ask:` / `verb:` / `fs:` 前缀）
//! - `ApprovalSink::resolve(id, decision)`：审批完成（allow/deny/approve/reject/timeout）
//! - `ApprovalSink::resolve_kind_for_worker(worker_id, kind, decision)`：按来源批量
//!   消除（file_snapshot 的 "superseded" 在 BusBackedSink 是 no-op——dedupe 签名兜）
//! - `ApprovalSink::resolve_file_paths(worker, session, paths, decision)`：per-path 精收
//! - `ApprovalSink::pending()`：bus 快照（native id 视图）
//! - `respond_route(entry, decision)`：应答路由决策（纯函数）——worker ui_ask→
//!   ask_respond / remote_verb→verb_review / file_snapshot→review_approve_all|reject_all；
//!   host 级 ui_ask oneshot 分支在 `WorkerRegistry::execute_unified_approval`
//!   （worker_registry.rs，RPC `approval_respond` 与审批泵共用）

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
    /// file_snapshot 的 per-path 精收（ApprovalResolved 带 path 时）。
    /// 缺省实现退化为 worker+kind 粗收（RecordingSink 等测试替身用）。
    fn resolve_file_paths(
        &self,
        worker_id: &str,
        _session_id: &str,
        _paths: &[String],
        decision: &str,
    ) {
        self.resolve_kind_for_worker(worker_id, ApprovalKind::FileSnapshot, decision);
    }
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
/// CI 已改走正式 RPC approvals_pending（BusBackedSink 为生产实现）。
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
// BusBackedSink — 生产实现：转发到 ApprovalBus::global()（单一数据源）
// ---------------------------------------------------------------------------

/// 生产 sink：所有登记/消除转发进统一审批总线（`ApprovalBus::global()`）。
///
/// 与 host 侧镜像监听（`bin/ion.rs mirror_ui_event_to_bus`）双路写同一张表，
/// 靠 **dedupe key 对齐** 幂等去重（pump 登记先于 EventBus 广播 → 带worker/
/// session 信息的条目胜出；镜像路径随后 dedupe 命中不产生第二条）：
///
/// | kind         | dedupe key                    | TTL    |
/// |--------------|-------------------------------|--------|
/// | ui_ask       | `ask:<native request_id>`     | 120s   |
/// | remote_verb  | `verb:<native requestId>`     | 300s   |
/// | file_snapshot| `fs:<path:status 签名>`        | 无     |
///
/// file_snapshot 采用总线规范语义（同文件集幂等；**不做** worker 级快照替换）——
/// per-path resolved / reset 负责收口，见 APPROVAL_BUS.md §1。
pub struct BusBackedSink;

impl BusBackedSink {
    fn to_bus_kind(kind: ApprovalKind) -> crate::approval_bus::ApprovalKind {
        match kind {
            ApprovalKind::UiAsk => crate::approval_bus::ApprovalKind::UiAsk,
            ApprovalKind::RemoteVerb => crate::approval_bus::ApprovalKind::RemoteVerb,
            ApprovalKind::FileSnapshot => crate::approval_bus::ApprovalKind::FileSnapshot,
        }
    }

    fn from_bus_kind(kind: crate::approval_bus::ApprovalKind) -> ApprovalKind {
        match kind {
            crate::approval_bus::ApprovalKind::UiAsk => ApprovalKind::UiAsk,
            crate::approval_bus::ApprovalKind::RemoteVerb => ApprovalKind::RemoteVerb,
            crate::approval_bus::ApprovalKind::FileSnapshot => ApprovalKind::FileSnapshot,
        }
    }

    /// file_snapshot dedupe 签名（与镜像路径同一算法：path:status 串联）
    fn file_sig(payload: &serde_json::Value) -> String {
        let files = payload
            .get("files")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if files.is_empty() {
            // 无文件列表（异常载荷）：退化为 requestId 维度
            return payload
                .get("requestId")
                .or_else(|| payload.get("request_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .to_string();
        }
        files
            .iter()
            .map(|f| {
                format!(
                    "{}:{}",
                    f.get("path").and_then(|v| v.as_str()).unwrap_or("?"),
                    f.get("status").and_then(|v| v.as_str()).unwrap_or("?")
                )
            })
            .collect::<Vec<_>>()
            .join("|")
    }

    /// 摘要（与镜像路径同格式：Ask: title — message / host_call verb: target / N file(s)）
    fn summary(entry: &ApprovalEntry) -> String {
        match entry.kind {
            ApprovalKind::UiAsk => {
                let title = entry
                    .payload
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let message = entry
                    .payload
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if message.is_empty() {
                    format!("Ask: {title}")
                } else {
                    format!("Ask: {title} — {message}")
                }
            }
            ApprovalKind::RemoteVerb => {
                let verb = entry
                    .payload
                    .get("verb")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                match entry
                    .payload
                    .get("args")
                    .and_then(|a| a.get("path").or_else(|| a.get("url")))
                    .and_then(|v| v.as_str())
                {
                    Some(target) => format!("host_call {verb}: {target}"),
                    None => format!("host_call {verb}"),
                }
            }
            ApprovalKind::FileSnapshot => {
                let total = entry
                    .payload
                    .get("total")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let paths: Vec<String> = entry
                    .payload
                    .get("files")
                    .and_then(|v| v.as_array())
                    .map(|fs| {
                        fs.iter()
                            .filter_map(|f| f.get("path").and_then(|v| v.as_str()))
                            .take(3)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                let more = total.saturating_sub(paths.len() as u64);
                let mut s = format!("{total} file(s) pending review: {}", paths.join(", "));
                if more > 0 {
                    s.push_str(&format!(" (+{more} more)"));
                }
                s
            }
        }
    }

    /// 归一 payload：注入 `nativeRequestId`（路由回源所需；镜像路径同款约定）
    fn normalized_payload(entry: &ApprovalEntry) -> serde_json::Value {
        let mut p = entry.payload.clone();
        if let Some(obj) = p.as_object_mut() {
            obj.insert(
                "nativeRequestId".to_string(),
                serde_json::Value::String(entry.id.clone()),
            );
        }
        p
    }

    /// decision 字符串 → (总线决定, 决定者)
    fn classify_decision(decision: &str) -> (crate::approval_bus::ApprovalDecision, crate::approval_bus::ApprovalActor) {
        match decision.trim().to_lowercase().as_str() {
            "timeout" => (
                crate::approval_bus::ApprovalDecision::Reject,
                crate::approval_bus::ApprovalActor::System,
            ),
            "allow" | "approve" | "approved" | "yes" | "true" | "accept" | "ok" => (
                crate::approval_bus::ApprovalDecision::Approve,
                crate::approval_bus::ApprovalActor::User,
            ),
            _ => (
                crate::approval_bus::ApprovalDecision::Reject,
                crate::approval_bus::ApprovalActor::User,
            ),
        }
    }

    /// 按 apr_ id 或 native id 定位总线条目
    fn find_bus_entry(
        bus: &crate::approval_bus::ApprovalBus,
        id: &str,
    ) -> Option<crate::approval_bus::ApprovalEntry> {
        bus.pending()
            .into_iter()
            .find(|e| e.id == id || e.native_request_id() == Some(id))
    }
}

impl ApprovalSink for BusBackedSink {
    fn register(&self, entry: ApprovalEntry) {
        let bus = crate::approval_bus::ApprovalBus::global();
        let (dedupe, ttl) = match entry.kind {
            ApprovalKind::UiAsk => (format!("ask:{}", entry.id), Some(120_000u64)),
            ApprovalKind::RemoteVerb => (format!("verb:{}", entry.id), Some(300_000)),
            ApprovalKind::FileSnapshot => (format!("fs:{}", Self::file_sig(&entry.payload)), None),
        };
        let worker = if entry.worker_id.is_empty() {
            None
        } else {
            Some(entry.worker_id.as_str())
        };
        let kind = Self::to_bus_kind(entry.kind);
        let summary = Self::summary(&entry);
        let payload = Self::normalized_payload(&entry);
        bus.register(kind, &entry.session_id, worker, &summary, payload, &dedupe, ttl);
    }

    fn resolve(&self, id: &str, decision: &str) {
        let bus = crate::approval_bus::ApprovalBus::global();
        let Some(entry) = Self::find_bus_entry(bus, id) else {
            return; // 从未登记/已被收口：静默（幂等）
        };
        let (dec, actor) = Self::classify_decision(decision);
        let _ = bus.resolve_as(&entry.id, dec, actor, Some(decision));
    }

    fn resolve_kind_for_worker(&self, worker_id: &str, kind: ApprovalKind, decision: &str) {
        // file_snapshot + "superseded"（新 ApprovalRequest 顶旧）：总线规范语义
        // 靠 dedupe 签名幂等，不做 worker 级清场（避免同集重发条目抖动）
        if kind == ApprovalKind::FileSnapshot && decision == "superseded" {
            return;
        }
        let bus = crate::approval_bus::ApprovalBus::global();
        let bus_kind = Self::to_bus_kind(kind);
        let (dec, actor) = Self::classify_decision(decision);
        bus.mark_resolved_matching_as(
            |e| e.kind == bus_kind && e.worker_id.as_deref() == Some(worker_id),
            dec,
            actor,
            Some(decision),
        );
    }

    fn resolve_file_paths(
        &self,
        worker_id: &str,
        session_id: &str,
        paths: &[String],
        decision: &str,
    ) {
        let bus = crate::approval_bus::ApprovalBus::global();
        let (dec, actor) = Self::classify_decision(decision);
        let worker = worker_id.to_string();
        if paths.is_empty() {
            // 无 path 的 ApprovalResolved：worker+session 维度粗收
            let session = session_id.to_string();
            bus.mark_resolved_matching_as(
                |e| {
                    e.kind == crate::approval_bus::ApprovalKind::FileSnapshot
                        && (e.worker_id.as_deref() == Some(worker.as_str())
                            || (!session.is_empty() && e.session_id == session))
                },
                dec,
                actor,
                Some(decision),
            );
        } else {
            // per-path 精收：移除 payload.files 含任一 path 的条目（对齐镜像路径）
            let owned: Vec<String> = paths.to_vec();
            bus.mark_resolved_matching_as(
                move |e| {
                    e.kind == crate::approval_bus::ApprovalKind::FileSnapshot
                        && e.worker_id.as_deref() == Some(worker.as_str())
                        && e.payload.get("files").and_then(|v| v.as_array()).is_some_and(
                            |fs| {
                                fs.iter().any(|f| {
                                    f.get("path")
                                        .and_then(|v| v.as_str())
                                        .is_some_and(|p| owned.contains(&p.to_string()))
                                })
                            },
                        )
                },
                dec,
                actor,
                Some(decision),
            );
        }
    }

    fn pending(&self) -> Vec<ApprovalEntry> {
        crate::approval_bus::ApprovalBus::global()
            .pending()
            .into_iter()
            .map(|e| ApprovalEntry {
                // 对外保留 native id（req_/vapp_/appr_）——来源侧与泵按 native 寻址
                id: e.native_request_id().unwrap_or(e.id.as_str()).to_string(),
                kind: Self::from_bus_kind(e.kind),
                worker_id: e.worker_id.unwrap_or_default(),
                session_id: e.session_id,
                payload: e.payload,
                created_at_ms: e.raised_at_ms,
            })
            .collect()
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
            // file-snapshot 的 ApprovalResolved 带 path/decision（不带 requestId）：
            // per-path 精收（BusBackedSink 对齐总线规范语义；RecordingSink 退化为粗收）。
            // 剩余 pending 由下一条 ApprovalRequest 重建（快照语义）
            let decision = data
                .get("decision")
                .and_then(|v| v.as_str())
                .unwrap_or("resolved");
            let paths: Vec<String> = data
                .get("path")
                .and_then(|v| v.as_str())
                .map(|p| vec![p.to_string()])
                .unwrap_or_default();
            s.resolve_file_paths(worker_id, session_id, &paths, decision);
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

/// 测试串行锁（仅测试构建可见）：会替换全局 sink 的测试必须先持有
/// （防并行测试中途换掉 sink）。worker_registry 等其它模块的 sink 相关测试也用它。
#[cfg(test)]
pub(crate) static TEST_SINK_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
        let _g = TEST_SINK_LOCK.lock().unwrap();
        // 未安装时 sink() 返回 no-op：register/resolve 不 panic、pending 为空
        let s = sink();
        s.register(entry(ApprovalKind::UiAsk, "req_x", "w1"));
        s.resolve("req_x", "allow");
        assert!(s.pending().is_empty());
    }

    #[test]
    fn recording_sink_register_idempotent_and_resolve() {
        let _g = TEST_SINK_LOCK.lock().unwrap();
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
        let _g = TEST_SINK_LOCK.lock().unwrap();
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
        let _g = TEST_SINK_LOCK.lock().unwrap();
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
        let _g = TEST_SINK_LOCK.lock().unwrap();
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
        let _g = TEST_SINK_LOCK.lock().unwrap();
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

    // ── BusBackedSink：单一数据源转发 + 双登记路径幂等 ──

    fn bus_sink_setup() -> std::sync::MutexGuard<'static, ()> {
        let g = TEST_SINK_LOCK.lock().unwrap();
        crate::approval_bus::ApprovalBus::global().clear();
        g
    }

    fn bus_sink_teardown() {
        crate::approval_bus::ApprovalBus::global().clear();
        set_sink(Arc::new(NoopSink));
    }

    /// 双登记路径去重：sink 登记（带 worker/session）与镜像登记（无 worker）同
    /// dedupe key → 单条目，且带 worker 信息的先到者胜出
    #[test]
    fn bus_backed_sink_dual_registration_dedupes() {
        let _g = bus_sink_setup();
        let s: Arc<dyn ApprovalSink> = Arc::new(BusBackedSink);
        // 路径 1（pump → sink，先到）：带 worker/session
        s.register(ApprovalEntry {
            id: "req_d1".into(),
            kind: ApprovalKind::UiAsk,
            worker_id: "w1".into(),
            session_id: "sess_w1".into(),
            payload: serde_json::json!({"request_id": "req_d1", "title": "t", "message": "m"}),
            created_at_ms: 1,
        });
        // 路径 2（EventBus 镜像，后到）：同 request_id，无 worker——dedupe 命中
        let bus = crate::approval_bus::ApprovalBus::global();
        let before = bus.pending();
        assert_eq!(before.len(), 1);
        // 直接以镜像 register 形状写 bus（dedupe ask:req_d1）
        bus.register(
            crate::approval_bus::ApprovalKind::UiAsk,
            "",
            None,
            "Ask: t — m",
            serde_json::json!({"nativeRequestId": "req_d1", "title": "t", "message": "m"}),
            "ask:req_d1",
            Some(120_000),
        );
        let after = bus.pending();
        assert_eq!(after.len(), 1, "双路径同 key 去重，不产生第二条");
        assert_eq!(after[0].worker_id.as_deref(), Some("w1"), "先到的 worker 信息保留");
        assert_eq!(after[0].session_id, "sess_w1");
        assert_eq!(after[0].native_request_id(), Some("req_d1"));
        bus_sink_teardown();
    }

    /// 三来源映射：kind/summary/payload(nativeRequestId)/TTL/dedupe 语义
    #[test]
    fn bus_backed_sink_maps_three_kinds() {
        let _g = bus_sink_setup();
        let s: Arc<dyn ApprovalSink> = Arc::new(BusBackedSink);
        s.register(ApprovalEntry {
            id: "vapp_7".into(),
            kind: ApprovalKind::RemoteVerb,
            worker_id: "w2".into(),
            session_id: "sess_w2".into(),
            payload: serde_json::json!({"verb": "http.fetch", "args": {"url": "https://x"}}),
            created_at_ms: 1,
        });
        s.register(ApprovalEntry {
            id: "appr_9".into(),
            kind: ApprovalKind::FileSnapshot,
            worker_id: "w3".into(),
            session_id: "sess_w3".into(),
            payload: serde_json::json!({
                "requestId": "appr_9", "total": 1,
                "files": [{"path": "a.rs", "status": "pending", "diffStat": "+1"}]
            }),
            created_at_ms: 1,
        });
        let bus = crate::approval_bus::ApprovalBus::global();
        let list = bus.pending();
        assert_eq!(list.len(), 2);
        let verb = list.iter().find(|e| e.kind == crate::approval_bus::ApprovalKind::RemoteVerb).unwrap();
        assert_eq!(verb.native_request_id(), Some("vapp_7"));
        assert!(verb.summary.contains("http.fetch"), "summary: {}", verb.summary);
        assert!(verb.expires_at_ms.is_some(), "verb TTL 300s 对齐等待窗");
        let fs = list.iter().find(|e| e.kind == crate::approval_bus::ApprovalKind::FileSnapshot).unwrap();
        assert!(fs.summary.contains("a.rs"));
        assert!(fs.expires_at_ms.is_none(), "file_snapshot 持续型无 TTL");
        // file_snapshot 同文件集重发（requestId 不同）→ dedupe 签名幂等
        s.register(ApprovalEntry {
            id: "appr_10".into(),
            kind: ApprovalKind::FileSnapshot,
            worker_id: "w3".into(),
            session_id: "sess_w3".into(),
            payload: serde_json::json!({
                "requestId": "appr_10", "total": 1,
                "files": [{"path": "a.rs", "status": "pending", "diffStat": "+1"}]
            }),
            created_at_ms: 2,
        });
        assert_eq!(bus.pending().len(), 2, "同集幂等不新增");
        bus_sink_teardown();
    }

    /// resolve（native id 寻址）+ superseded no-op + per-path 精收
    #[test]
    fn bus_backed_sink_resolve_semantics() {
        let _g = bus_sink_setup();
        let s: Arc<dyn ApprovalSink> = Arc::new(BusBackedSink);
        s.register(ApprovalEntry {
            id: "req_r1".into(),
            kind: ApprovalKind::UiAsk,
            worker_id: "w1".into(),
            session_id: "s1".into(),
            payload: serde_json::json!({"request_id": "req_r1", "title": "t", "message": "m"}),
            created_at_ms: 1,
        });
        // native id resolve（verb_gate 超时路径用）
        s.resolve("req_r1", "timeout");
        assert!(crate::approval_bus::ApprovalBus::global().pending().is_empty());

        // file_snapshot：superseded no-op（dedupe 兜）；per-path resolved 收口
        s.register(ApprovalEntry {
            id: "appr_20".into(),
            kind: ApprovalKind::FileSnapshot,
            worker_id: "w9".into(),
            session_id: "s9".into(),
            payload: serde_json::json!({
                "requestId": "appr_20", "total": 2,
                "files": [{"path": "x.rs", "status": "pending"}, {"path": "y.rs", "status": "pending"}]
            }),
            created_at_ms: 1,
        });
        s.resolve_kind_for_worker("w9", ApprovalKind::FileSnapshot, "superseded");
        assert_eq!(crate::approval_bus::ApprovalBus::global().pending().len(), 1, "superseded 不清场");
        s.resolve_file_paths("w9", "s9", &["x.rs".to_string()], "approved");
        assert!(crate::approval_bus::ApprovalBus::global().pending().is_empty(), "per-path 收口整条");
        bus_sink_teardown();
    }

    /// pending() 返回 native id 视图（泵/来源侧按 native 寻址）
    #[test]
    fn bus_backed_sink_pending_is_native_id_view() {
        let _g = bus_sink_setup();
        let s: Arc<dyn ApprovalSink> = Arc::new(BusBackedSink);
        s.register(ApprovalEntry {
            id: "req_p1".into(),
            kind: ApprovalKind::UiAsk,
            worker_id: "w1".into(),
            session_id: "s1".into(),
            payload: serde_json::json!({"request_id": "req_p1", "title": "t", "message": "m"}),
            created_at_ms: 5,
        });
        let p = s.pending();
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].id, "req_p1", "native id（非 apr_ 前缀）");
        assert_eq!(p[0].worker_id, "w1");
        assert!(p[0].created_at_ms > 0, "created_at_ms 取总线 raisedAtMs（注册时刻）");
        bus_sink_teardown();
    }
}
