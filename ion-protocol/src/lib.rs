//! # ion-protocol — ION RPC 线协议信封层
//!
//! ION 的线协议是 **JSON 行协议**（JSONL over stdin/stdout 或 Unix socket），
//! 对齐 pi 的 `--mode rpc`。本 crate 只抽**信封层**：请求/响应信封、事件外壳、
//! 订阅协议帧（snapshot / stale_route / subscribed ack 等）的**类型与常量**，
//! 由三处消费方共享：
//!
//! - **worker**（`src/worker_rpc.rs`）：stdout 上输出响应信封与事件外壳
//! - **host**（`src/bin/ion.rs`）：socket 上输出响应帧、转发事件帧
//! - **CLI 客户端**（`ion rpc` / `ion subscribe`）：构造请求、解析响应
//!
//! ## 职责边界（对标 pi protocol 包，但更窄）
//!
//! 信封与帧归本 crate；**命令 payload 保持 opaque**——`data` / `params` / `event`
//! 内部结构由调用方自由构造，本 crate 不定义、不校验（120+ 命令的 payload 契约
//! 由 JSON schema 体系另行管理）。epoch 表、订阅路由、快照数据组装等**逻辑留在
//! 消费方**，这里只有纯函数的形状构造。
//!
//! ## 宽容解析（与 pi 的差异，有意为之）
//!
//! pi 新版 protocol 包对 unknown field 采取拒绝策略；ION **不学**：行解析只要求
//! 整行是合法 JSON，未知字段一律透传忽略。理由：
//!
//! 1. ION 的消费方（webui 网关、CI 脚本、第三方 UI）已经依赖宽容行为多年；
//! 2. 信封层字段（`id`/`type`/`method`/`session`）是**加法演进**的（epoch、
//!    `replayed`、`snapshot` 都是后加的），拒绝未知字段会让旧 host 读不了新
//!    worker 的帧，破坏"先升级一端"的滚动兼容；
//! 3. 命令 payload 本来就是 opaque 的，信封层没有立场替 payload 校验。
//!
//! ## 已知的不统一（记录，不修复）
//!
//! - `create_session` 的字段命名按调用形态分裂（host 级 camelCase、worker 级
//!   snake_case）。**保持现状**——统一属于 payload 层变更，超出信封层职责。
//! - host 响应统一**紧凑序列化**（[`serialize_line`]，`serde_json::to_string`）。
//!   历史上 CLI 显示层曾对响应 pretty-print，紧凑化是本 crate 落地时唯一允许的
//!   行为变更（对 JSON 解析型消费者无感，仅影响字符串匹配型脚本）。

use serde::Deserialize;
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/// 协议版本（`hello` 握手返回值）。
pub const PROTOCOL_VERSION: u64 = 1;

/// 响应信封的 `type` 字段值。
pub const TYPE_RESPONSE: &str = "response";

/// 事件外壳的 `type` 字段值（worker→host stdout 的事件包裹）。
pub const TYPE_EVENT: &str = "event";

/// 旧 epoch 订阅者收到的一次性作废通知帧 `type`。
pub const TYPE_STALE_ROUTE: &str = "stale_route";

// ---------------------------------------------------------------------------
// 请求信封（C→S / host→worker stdin）
// ---------------------------------------------------------------------------

/// 宽容请求视图：从一帧 JSON 里提取请求信封字段。
///
/// 兼容历史形态：
/// - CLI `ion rpc`：`{"id","method","params","session"?}`
/// - CLI `ion subscribe`：`{"method":"subscribe","session"?,"extension"?,"ui"?,"replay"?}`（可无 id）
/// - worker 命令回落：无 `method` 时读 `type`（旧版兼容，见 worker 主循环）
#[derive(Debug, Clone, Deserialize, Default)]
pub struct Request {
    /// 请求 id（响应原样回带；subscribe 等流式请求可省略）
    #[serde(default)]
    pub id: Option<Value>,
    /// 方法名
    #[serde(default)]
    pub method: String,
    /// 参数（opaque，payload 契约不在信封层）
    #[serde(default)]
    pub params: Option<Value>,
    /// 目标会话（可选；host 用它路由到 worker）
    #[serde(default)]
    pub session: Option<String>,
    /// 订阅的扩展名（仅 subscribe）
    #[serde(default)]
    pub extension: Option<String>,
    /// 是否 UI 订阅（仅 subscribe；ui_respond 同源绑定的前提）
    #[serde(default)]
    pub ui: Option<bool>,
    /// 回放条数（仅 subscribe）
    #[serde(default)]
    pub replay: Option<u64>,
}

impl Request {
    /// 从宽容解析中提取方法名：优先 `method`，缺失回落 `type`，再缺失为空串。
    /// （与 worker 主循环 / host 命令循环的历史行为一致。）
    pub fn method_or_type(v: &Value) -> &str {
        v.get("method")
            .and_then(|v| v.as_str())
            .or_else(|| v.get("type").and_then(|v| v.as_str()))
            .unwrap_or("")
    }

    /// 帧是否带请求 id（CLI rpc 客户端用它区分响应帧与事件帧）。
    pub fn has_request_id(v: &Value) -> bool {
        v.get("id").is_some()
    }

    /// 构造 CLI `ion rpc` 的请求信封：`{"id","method","params"}`。
    pub fn rpc(id: &str, method: &str, params: Value) -> Value {
        json!({"id": id, "method": method, "params": params})
    }

    /// 构造带 session 的请求信封：`{"id","method","params","session"}`。
    pub fn rpc_with_session(id: &str, method: &str, params: Value, session: &str) -> Value {
        json!({"id": id, "method": method, "params": params, "session": session})
    }

    /// 构造 `ion subscribe` 请求：`{"method":"subscribe", ...可选过滤字段}`。
    pub fn subscribe(
        session: Option<&str>,
        extension: Option<&str>,
        ui: bool,
        replay: Option<usize>,
    ) -> Value {
        let mut req = json!({"method": "subscribe"});
        if let Some(sid) = session {
            req["session"] = json!(sid);
        }
        if let Some(ext) = extension {
            req["extension"] = json!(ext);
        }
        if ui {
            req["ui"] = json!(true);
        }
        if let Some(n) = replay {
            req["replay"] = json!(n);
        }
        req
    }
}

// ---------------------------------------------------------------------------
// 响应信封
// ---------------------------------------------------------------------------

/// worker 响应信封：`{"id","type":"response","command","success","data"|"error"}`。
///
/// worker 侧响应**必须带 `command`**（回显方法名）；成功带 `data`，失败带
/// `error`（字符串），两者互斥。
pub mod worker_response {
    use super::{TYPE_RESPONSE, json};

    pub fn success(id: &str, command: &str, data: &serde_json::Value) -> serde_json::Value {
        json!({
            "id": id,
            "type": TYPE_RESPONSE,
            "command": command,
            "success": true,
            "data": data,
        })
    }

    pub fn error(id: &str, command: &str, error: &str) -> serde_json::Value {
        json!({
            "id": id,
            "type": TYPE_RESPONSE,
            "command": command,
            "success": false,
            "error": error,
        })
    }
}

/// host 响应信封：`{"type":"response","id","success","data"|"error"}`。
///
/// host 响应**不带 `command`**（与 worker 信封的历史差异，保持不变）；`id`
/// 透传请求的 id（解析失败时为 `null`）。
pub mod host_response {
    use super::{TYPE_RESPONSE, Value, json};

    pub fn success(id: Value, data: Value) -> Value {
        json!({"type": TYPE_RESPONSE, "id": id, "success": true, "data": data})
    }

    pub fn error(id: Value, error: impl Into<String>) -> Value {
        json!({"type": TYPE_RESPONSE, "id": id, "success": false, "error": error.into()})
    }

    /// 解析失败兜底：id 为 null 的失败响应。
    pub fn invalid_json(error: impl std::fmt::Display) -> Value {
        json!({
            "type": TYPE_RESPONSE,
            "id": Value::Null,
            "success": false,
            "error": format!("invalid JSON: {error}"),
        })
    }
}

// ---------------------------------------------------------------------------
// 事件外壳（worker→host stdout）
// ---------------------------------------------------------------------------

/// 事件外壳：`{"type":"event","event":{...}}`。
///
/// Manager 的 stdout-reader 靠外层 `type=="event"` 识别并转发；**扩展事件必须
/// 包这层外壳**，否则 host 不路由。
pub fn event_shell(event: Value) -> Value {
    json!({ "type": TYPE_EVENT, "event": event })
}

/// worker 就绪公告帧（**首帧**，无事件外壳）：
/// `{"type":"ready","session","model","provider","channels","version"}`。
pub fn ready_frame(
    session: &str,
    model: &str,
    provider: &str,
    channels: &[String],
    version: &str,
) -> Value {
    json!({
        "type": "ready",
        "session": session,
        "model": model,
        "provider": provider,
        "channels": channels,
        "version": version,
    })
}

/// worker 就绪信号：host 收到后把创建时预设的 Busy 解除为 Idle。
pub fn worker_ready_event() -> Value {
    event_shell(json!({ "type": "worker_ready" }))
}

/// 每条用户 RPC 完成后广播的摘要事件（多终端实时同步）。
///
/// 只带摘要（id/method/success/error 截断 200 字符/sessionId/timestamp），
/// **不带响应体**——大响应进事件流会挤爆慢订阅者（EventBus bounded 1000 条）。
pub fn rpc_response_event(
    session_id: &str,
    id: &str,
    command: &str,
    success: bool,
    error: Option<&str>,
) -> Value {
    let mut event = json!({
        "type": "rpc_response",
        "id": id,
        "method": command,
        "success": success,
        "sessionId": session_id,
        "timestamp": now_ms(),
    });
    if let Some(err) = error {
        event["error"] = serde_json::Value::String(err.chars().take(200).collect());
    }
    event_shell(event)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Subscribe 协议帧（W2 语义：epoch 栅栏 + 快照先行 + 版本握手）
//
// 详见 docs/design/SUBSCRIBE_PROTOCOL.md。epoch 表与路由逻辑留在 host，
// 这里只有帧的形状。
// ---------------------------------------------------------------------------

/// `hello` 握手响应：`{"type":"response","id","success":true,"data":{"protocolVersion":1}}`。
///
/// 不消费连接——客户端可继续 subscribe / rpc；不发 hello 的旧客户端完全兼容。
pub fn hello_reply(id: Option<&Value>) -> Value {
    json!({
        "type": TYPE_RESPONSE,
        "id": id,
        "success": true,
        "data": {"protocolVersion": PROTOCOL_VERSION},
    })
}

/// session 订阅 ack：携带 epoch 与 replay 条数。
pub fn subscribed_ack_session(sid: &str, epoch: u64, replayed: usize) -> Value {
    json!({
        "type": "subscribed",
        "session": sid,
        "stream": "instance",
        "epoch": epoch,
        "replayed": replayed,
    })
}

/// UI 订阅 ack。
pub fn subscribed_ack_ui() -> Value {
    json!({"type": "subscribed", "stream": "ui"})
}

/// 扩展订阅 ack（session 缺省序列化为 null，保持历史形状）。
pub fn subscribed_ack_extension(extension: &str, session: Option<&str>) -> Value {
    json!({
        "type": "subscribed",
        "extension": extension,
        "session": session,
    })
}

/// snapshot 帧：instance_event 外壳 + `customType:"snapshot"`。
///
/// 水合纪律：subscribe 建立后**严格先于** replay / 实时增量推送；客户端凭
/// `.snapshot == true`（或 `.event.customType == "snapshot"`）识别快照帧。
pub fn snapshot_event(sid: &str, epoch: u64, data: Value) -> Value {
    json!({
        "type": "instance_event",
        "session": sid,
        "epoch": epoch,
        "snapshot": true,
        "event": {
            "type": "extension_event",
            "extension": "host",
            "customType": "snapshot",
            "visibility": "ui_only",
            "session": sid,
            "data": data,
        },
    })
}

/// epoch 盖章的转发信封（与旧 instance_event 形状兼容，仅多 epoch 字段）；
/// 无 `event` 键的裸消息整体放进 `event`。
pub fn stamp_instance_event(sid: &str, epoch: u64, msg: Value) -> Value {
    json!({
        "type": "instance_event",
        "session": sid,
        "epoch": epoch,
        "event": msg.get("event").cloned().unwrap_or(msg),
    })
}

/// 旧 epoch 订阅者收到的一次性作废通知（收到即停止转发，连接关闭）。
pub fn stale_route_event(sid: &str, epoch: u64, current_epoch: u64) -> Value {
    json!({
        "type": TYPE_STALE_ROUTE,
        "customType": TYPE_STALE_ROUTE,
        "session": sid,
        "epoch": epoch,
        "currentEpoch": current_epoch,
    })
}

// ---------------------------------------------------------------------------
// host 转发帧（accept 循环 / 事件泵）
// ---------------------------------------------------------------------------

/// UI 事件帧（subscribe ui 流）。
pub fn ui_event_frame(
    custom_type: &str,
    extension: &str,
    session: Option<&str>,
    data: &Value,
    route: &str,
) -> Value {
    json!({
        "type": "ui_event",
        "ui_type": custom_type,
        "extension": extension,
        "session": session,
        "data": data,
        "route": route,
    })
}

/// 扩展事件帧（subscribe extension 流；visibility 取 "llm_and_ui" / "ui_only"）。
#[allow(clippy::too_many_arguments)]
pub fn extension_event_frame(
    extension: &str,
    custom_type: &str,
    session: Option<&str>,
    persisted: bool,
    visibility: &str,
    correlation_id: &str,
    data: &Value,
) -> Value {
    json!({
        "type": "extension_event",
        "extension": extension,
        "customType": custom_type,
        "session": session,
        "persisted": persisted,
        "visibility": visibility,
        "correlation_id": correlation_id,
        "data": data,
    })
}

/// worker 响应包装帧（事件泵 → serve stdout）。
pub fn worker_response_frame(worker_id: &str, session_id: &str, response: &Value) -> Value {
    json!({
        "type": "worker_response",
        "worker_id": worker_id,
        "session_id": session_id,
        "response": response,
    })
}

/// worker 事件包装帧（事件泵 → serve stdout）。
pub fn pump_event_frame(worker_id: &str, session_id: &str, inner: Value) -> Value {
    json!({
        "type": TYPE_EVENT,
        "worker_id": worker_id,
        "session_id": session_id,
        "event": inner,
    })
}

/// overview 快照推送帧（subscribe_overview 流）。
pub fn overview_snapshot_frame(snapshot: Value) -> Value {
    json!({
        "type": "overview_snapshot",
        "data": snapshot,
    })
}

/// 订阅流错误帧：`{"type":"error","error":...}`（无 id，非 RPC 响应）。
pub fn stream_error_frame(error: impl Into<String>) -> Value {
    json!({"type": "error", "error": error.into()})
}

// ---------------------------------------------------------------------------
// 行解析与序列化
// ---------------------------------------------------------------------------

/// 把一行文本解析成 JSON 帧（宽容解析：只要求整行合法 JSON，unknown field 透传；
/// 理由见 crate 顶部文档）。
pub fn parse_line(line: &str) -> Result<Value, serde_json::Error> {
    serde_json::from_str(line.trim())
}

/// 信封的规范序列化：**紧凑**（`serde_json::to_string`），一行一帧。
///
/// host/worker/CLI 的所有线上的 JSON 都应经此函数；失败兜底为空串（与
/// worker `output()` 的历史行为一致——写不了就丢，不 panic 杀 worker）。
pub fn serialize_line(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── 请求信封 ──

    #[test]
    fn rpc_request_shape() {
        let req = Request::rpc("rpc-client", "health", json!({}));
        assert_eq!(req["id"], "rpc-client");
        assert_eq!(req["method"], "health");
        assert!(req["params"].is_object());
        assert!(req.get("session").is_none());
    }

    #[test]
    fn subscribe_request_optional_fields() {
        let req = Request::subscribe(Some("s1"), None, true, Some(3));
        assert_eq!(req["method"], "subscribe");
        assert_eq!(req["session"], "s1");
        assert_eq!(req["ui"], true);
        assert_eq!(req["replay"], 3);
        assert!(req.get("extension").is_none());
        // 无 id（流式请求）
        assert!(req.get("id").is_none());
    }

    #[test]
    fn method_or_type_fallback() {
        assert_eq!(Request::method_or_type(&json!({"method": "a"})), "a");
        assert_eq!(Request::method_or_type(&json!({"type": "b"})), "b");
        assert_eq!(Request::method_or_type(&json!({})), "");
    }

    #[test]
    fn has_request_id_distinguishes_response_from_event() {
        assert!(Request::has_request_id(&json!({"id": "1", "type": "response"})));
        assert!(!Request::has_request_id(&json!({"type": "event", "event": {}})));
    }

    // ── 响应信封 ──

    #[test]
    fn worker_response_success_shape() {
        let v = worker_response::success("1", "health", &json!({"status": "ok"}));
        assert_eq!(v["id"], "1");
        assert_eq!(v["type"], "response");
        assert_eq!(v["command"], "health");
        assert_eq!(v["success"], true);
        assert_eq!(v["data"]["status"], "ok");
        assert!(v.get("error").is_none());
    }

    #[test]
    fn worker_response_error_shape() {
        let v = worker_response::error("2", "x", "Unknown command: x");
        assert_eq!(v["success"], false);
        assert_eq!(v["error"], "Unknown command: x");
        assert!(v.get("data").is_none());
    }

    #[test]
    fn host_response_shape_has_no_command() {
        let v = host_response::success(json!("h1"), json!({"k": 1}));
        assert_eq!(v["type"], "response");
        assert_eq!(v["id"], "h1");
        assert_eq!(v["success"], true);
        assert!(v.get("command").is_none(), "host 信封不带 command");
        let e = host_response::error(Value::Null, "boom");
        assert!(e["id"].is_null());
        assert_eq!(e["error"], "boom");
    }

    #[test]
    fn invalid_json_response_mentions_parse_failure() {
        let v = host_response::invalid_json("expected value");
        assert!(v["id"].is_null());
        assert_eq!(v["success"], false);
        assert_eq!(v["error"], "invalid JSON: expected value");
    }

    // ── 事件外壳 ──

    #[test]
    fn event_shell_wraps_inner_event() {
        let v = event_shell(json!({"type": "text_delta", "delta": "hi"}));
        assert_eq!(v["type"], "event");
        assert_eq!(v["event"]["type"], "text_delta");
    }

    #[test]
    fn ready_frame_is_first_line_bare_shape() {
        let v = ready_frame("s", "m", "p", &["c1".into()], "0.1.0");
        assert_eq!(v["type"], "ready");
        assert_eq!(v["session"], "s");
        assert_eq!(v["model"], "m");
        assert_eq!(v["provider"], "p");
        assert_eq!(v["channels"][0], "c1");
        assert_eq!(v["version"], "0.1.0");
        // 裸帧：不带事件外壳
        assert!(v.get("event").is_none());
    }

    #[test]
    fn worker_ready_is_event_shell() {
        let v = worker_ready_event();
        assert_eq!(v["type"], "event");
        assert_eq!(v["event"]["type"], "worker_ready");
    }

    #[test]
    fn rpc_response_event_truncates_long_error() {
        let long_err = "x".repeat(500);
        let v = rpc_response_event("sess", "9", "m", false, Some(&long_err));
        assert_eq!(v["type"], "event");
        assert_eq!(v["event"]["type"], "rpc_response");
        assert_eq!(v["event"]["id"], "9");
        assert_eq!(v["event"]["method"], "m");
        assert_eq!(v["event"]["success"], false);
        assert_eq!(v["event"]["sessionId"], "sess");
        assert!(v["event"]["timestamp"].is_u64());
        // 截断到 200 字符
        assert_eq!(v["event"]["error"].as_str().unwrap().len(), 200);
    }

    #[test]
    fn rpc_response_event_success_has_no_error_key() {
        let v = rpc_response_event("sess", "9", "m", true, None);
        assert!(v["event"].get("error").is_none());
    }

    // ── Subscribe 协议帧 ──

    #[test]
    fn hello_reply_shape() {
        let v = hello_reply(Some(&json!("h1")));
        assert_eq!(v["type"], "response");
        assert_eq!(v["id"], "h1");
        assert_eq!(v["success"], true);
        assert_eq!(v["data"]["protocolVersion"], PROTOCOL_VERSION);
        let v2 = hello_reply(None);
        assert!(v2["id"].is_null());
    }

    #[test]
    fn subscribed_ack_session_shape() {
        let v = subscribed_ack_session("s", 3, 2);
        assert_eq!(v["type"], "subscribed");
        assert_eq!(v["session"], "s");
        assert_eq!(v["stream"], "instance");
        assert_eq!(v["epoch"], 3);
        assert_eq!(v["replayed"], 2);
    }

    #[test]
    fn subscribed_ack_ui_shape() {
        let v = subscribed_ack_ui();
        assert_eq!(v["type"], "subscribed");
        assert_eq!(v["stream"], "ui");
    }

    #[test]
    fn subscribed_ack_extension_null_session() {
        let v = subscribed_ack_extension("memory", None);
        assert_eq!(v["type"], "subscribed");
        assert_eq!(v["extension"], "memory");
        assert!(v["session"].is_null());
    }

    #[test]
    fn snapshot_frame_shape() {
        let v = snapshot_event("s", 1, json!({"worker": null}));
        assert_eq!(v["type"], "instance_event");
        assert_eq!(v["session"], "s");
        assert_eq!(v["epoch"], 1);
        assert_eq!(v["snapshot"], true);
        assert_eq!(v["event"]["type"], "extension_event");
        assert_eq!(v["event"]["extension"], "host");
        assert_eq!(v["event"]["customType"], "snapshot");
        assert_eq!(v["event"]["visibility"], "ui_only");
    }

    #[test]
    fn stamp_instance_event_wraps_bare_messages() {
        let out = stamp_instance_event("s", 7, json!({"type":"event","event":{"type":"text_delta"}}));
        assert_eq!(out["type"], "instance_event");
        assert_eq!(out["epoch"], 7);
        assert_eq!(out["event"]["type"], "text_delta");
        let bare = stamp_instance_event("s", 7, json!({"type": "agent_start"}));
        assert_eq!(bare["event"]["type"], "agent_start");
    }

    #[test]
    fn stale_route_shape() {
        let v = stale_route_event("s", 1, 2);
        assert_eq!(v["type"], "stale_route");
        assert_eq!(v["customType"], "stale_route");
        assert_eq!(v["epoch"], 1);
        assert_eq!(v["currentEpoch"], 2);
    }

    // ── 转发帧 ──

    #[test]
    fn ui_event_frame_shape() {
        let v = ui_event_frame("Ask", "file", Some("s1"), &json!({"q": 1}), "ui");
        assert_eq!(v["type"], "ui_event");
        assert_eq!(v["ui_type"], "Ask");
        assert_eq!(v["extension"], "file");
        assert_eq!(v["session"], "s1");
        assert_eq!(v["route"], "ui");
    }

    #[test]
    fn extension_event_frame_shape() {
        let v = extension_event_frame("memory", "memory_saved", None, true, "llm_and_ui", "c1", &json!({}));
        assert_eq!(v["type"], "extension_event");
        assert_eq!(v["customType"], "memory_saved");
        assert_eq!(v["persisted"], true);
        assert_eq!(v["visibility"], "llm_and_ui");
        assert_eq!(v["correlation_id"], "c1");
    }

    #[test]
    fn worker_response_and_pump_event_frames() {
        let w = worker_response_frame("w-1", "s1", &json!({"success": true}));
        assert_eq!(w["type"], "worker_response");
        assert_eq!(w["worker_id"], "w-1");
        let p = pump_event_frame("w-1", "s1", json!({"type": "text_delta"}));
        assert_eq!(p["type"], "event");
        assert_eq!(p["event"]["type"], "text_delta");
    }

    #[test]
    fn overview_and_stream_error_frames() {
        let o = overview_snapshot_frame(json!({"workers": []}));
        assert_eq!(o["type"], "overview_snapshot");
        let e = stream_error_frame("no worker");
        assert_eq!(e["type"], "error");
        assert_eq!(e["error"], "no worker");
        assert!(e.get("id").is_none());
    }

    // ── 解析与序列化 ──

    #[test]
    fn parse_line_tolerant_of_unknown_fields() {
        let v = parse_line(r#"{"id":"1","method":"m","future_field":42}"#).unwrap();
        assert_eq!(v["id"], "1");
        assert_eq!(v["future_field"], 42);
        assert!(parse_line("not json").is_err());
    }

    #[test]
    fn serialize_line_is_compact_one_frame_per_line() {
        let v = worker_response::success("1", "health", &json!({"a": 1}));
        let line = serialize_line(&v);
        assert!(!line.contains('\n'));
        assert!(!line.contains(": "), "紧凑序列化不该有 pretty 分隔: {line}");
        // 往返一致
        assert_eq!(parse_line(&line).unwrap(), v);
    }
}
