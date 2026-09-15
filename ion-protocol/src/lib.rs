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

/// JSONL 单行字节上限（16 MiB）——对齐 pi protocol `framing.ts` 的行长上限。
///
/// 防滥用：恶意/异常客户端发超大行不允许被读端无限缓冲（OOM 风险）。host socket
/// 与 worker stdin 的读循环在**行累计字节超限**时立即拒绝：发
/// [`line_too_large_frame`] 错误帧后断开连接/退出，绝不把整行缓冲进内存再做解析。
pub const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

// ---------------------------------------------------------------------------
// 错误脱敏 + 有界（P0.2，对标 pi server.ts:512-521 + codec.ts:34-37）
// ---------------------------------------------------------------------------

/// 错误消息的最大字符数（对标 pi `boundedErrorMessage` 的 500 字符上限）。
pub const MAX_ERROR_CHARS: usize = 500;

/// 含 panic/backtrace 细节的错误收敛后的固定文案（对标 pi 的
/// "Internal server error"——内部细节不过协议边界）。
pub const REDACTED_INTERNAL_ERROR: &str = "internal error (details redacted)";

/// 错误消息脱敏 + 截断：线协议错误通道的统一出口（本 crate 内所有 `error`
/// 字段构造点都经此处理）。
///
/// 规则（按序）：
/// 1. 含 panic/backtrace 细节（"panicked at" / "stack backtrace" /
///    "RUST_BACKTRACE"）→ 整条收敛为 [`REDACTED_INTERNAL_ERROR`]；
/// 2. 绝对 home 路径（`$HOME` 展开）→ 替换为 `~`
///    （`/Users/me/.ion/...` → `~/.ion/...`，可诊断但不泄漏 home）；
/// 3. 超过 [`MAX_ERROR_CHARS`] 字符 → 按 char 截断（防 UTF-8 撕裂）。
///
/// **公共错误语义不受影响**：不含 home 路径、无 panic 细节且长度合规的错误
/// （"Unknown command"、"session not found on disk: <sid>"、verb/审批类文案）
/// 原样通过；脱敏幂等，重复调用无副作用。
pub fn sanitize_error(msg: &str) -> String {
    sanitize_error_with(msg, std::env::var("HOME").ok().as_deref())
}

/// [`sanitize_error`] 的依赖注入变体（home 由调用方给定；测试用，避免改动
/// 进程级环境变量）。
pub fn sanitize_error_with(msg: &str, home: Option<&str>) -> String {
    // 1. panic/backtrace 细节 → 整条收敛为固定文案（原始细节只进日志，不过线协议）
    if msg.contains("panicked at")
        || msg.contains("stack backtrace")
        || msg.contains("RUST_BACKTRACE")
    {
        return REDACTED_INTERNAL_ERROR.to_string();
    }
    // 2. 绝对 home 路径 → ~（尾斜杠归一，防双斜杠；空 home / 根目录跳过）
    let replaced = match home {
        Some(h) => {
            let h = h.trim_end_matches('/');
            if h.is_empty() || h == "/" {
                msg.to_string()
            } else {
                msg.replace(h, "~")
            }
        }
        None => msg.to_string(),
    };
    // 3. 有界：按 char 截断（多字节安全）
    if replaced.chars().count() > MAX_ERROR_CHARS {
        replaced.chars().take(MAX_ERROR_CHARS).collect()
    } else {
        replaced
    }
}

// ---------------------------------------------------------------------------
// host 逻辑实例身份（P0.3，对标 pi protocol.ts:65-69 ServerHello.serverId）
// ---------------------------------------------------------------------------

/// 取当前 host 进程的逻辑实例身份（进程内 `OnceLock`，首次访问生成，
/// **不落盘**——按存储落位原则"宁可丢也不建新文件"，重启即换新身份）。
pub fn host_id() -> &'static str {
    static HOST_ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    HOST_ID.get_or_init(generate_host_id)
}

/// 生成规范小写 UUIDv4（对标 pi `ServerId` 的规范形式）。
///
/// 随机源：`/dev/urandom`（macOS/Linux）；取不到时以时间纳秒 + pid + 进程内
/// 计数器混合的 LCG 兜底——身份只要求"进程间不同 + 重启即变 + 进程内多次
/// 生成互不相同"，不要求密码学强度。
pub fn generate_host_id() -> String {
    static FALLBACK_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut bytes = [0u8; 16];
    if !fill_random(&mut bytes) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let pid = u64::from(std::process::id());
        let seq = FALLBACK_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut state = nanos
            ^ (pid << 32)
            ^ seq.wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ (&bytes as *const _ as u64);
        for b in bytes.iter_mut() {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            *b = (state >> 33) as u8;
        }
    }
    // 规范 UUIDv4 位域：version 4 + variant 10xx
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let h: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}{}{}{}-{}{}-{}{}-{}{}-{}{}{}{}{}{}",
        h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7], h[8], h[9], h[10], h[11], h[12], h[13],
        h[14], h[15]
    )
}

/// 从 `/dev/urandom` 填充随机字节；失败（无该设备/读不满）返回 false。
fn fill_random(buf: &mut [u8]) -> bool {
    use std::io::Read;
    match std::fs::File::open("/dev/urandom") {
        Ok(mut f) => f.read_exact(buf).is_ok(),
        Err(_) => false,
    }
}

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
/// `error`（字符串），两者互斥。错误经 [`sanitize_error`] 脱敏 + 有界（P0.2）。
pub mod worker_response {
    use super::{TYPE_RESPONSE, json, sanitize_error};

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
            "error": sanitize_error(error),
        })
    }
}

/// host 响应信封：`{"type":"response","id","success","data"|"error"}`。
///
/// host 响应**不带 `command`**（与 worker 信封的历史差异，保持不变）；`id`
/// 透传请求的 id（解析失败时为 `null`）。错误经 [`sanitize_error`] 脱敏 + 有界。
pub mod host_response {
    use super::{TYPE_RESPONSE, Value, json, sanitize_error};

    pub fn success(id: Value, data: Value) -> Value {
        json!({"type": TYPE_RESPONSE, "id": id, "success": true, "data": data})
    }

    pub fn error(id: Value, error: impl Into<String>) -> Value {
        json!({"type": TYPE_RESPONSE, "id": id, "success": false, "error": sanitize_error(&error.into())})
    }

    /// 解析失败兜底：id 为 null 的失败响应。
    pub fn invalid_json(error: impl std::fmt::Display) -> Value {
        json!({
            "type": TYPE_RESPONSE,
            "id": Value::Null,
            "success": false,
            "error": sanitize_error(&format!("invalid JSON: {error}")),
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
        // 先脱敏再截 200——事件摘要同样不泄漏 home/panic 细节
        let sanitized = sanitize_error(err);
        event["error"] = serde_json::Value::String(sanitized.chars().take(200).collect());
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

/// `hello` 握手响应：
/// `{"type":"response","id","success":true,"data":{"protocolVersion":1,"hostId":"<uuid-v4>"}}`。
///
/// `hostId` 是 host 进程的逻辑实例身份（[`host_id`] 生成，内存态不落盘，重启即变）
/// ——对标 pi `ServerHello.serverId`（protocol.ts:65-69）：客户端可 pin 校验，
/// 防止连到陈旧 socket 背后的"错误 host"。
///
/// 不消费连接——客户端可继续 subscribe / rpc；不发 hello 的旧客户端完全兼容。
pub fn hello_reply(id: Option<&Value>, host_id: &str) -> Value {
    json!({
        "type": TYPE_RESPONSE,
        "id": id,
        "success": true,
        "data": {"protocolVersion": PROTOCOL_VERSION, "hostId": host_id},
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
/// 错误同样经 [`sanitize_error`] 脱敏 + 有界（订阅端与 RPC 端同一脱敏纪律）。
pub fn stream_error_frame(error: impl Into<String>) -> Value {
    json!({"type": "error", "error": sanitize_error(&error.into())})
}

/// 行长超限错误帧（[`MAX_LINE_BYTES`] 防滥用的拒绝回执）。
///
/// 形状归 `type:"error"` 家族（与 [`stream_error_frame`] 同族，现有错误帧消费者
/// 无需改造即可识别），额外携带 `limitBytes` / `actualBytes` 便于诊断。
/// **无 `id`**——超长行不做 JSON 解析，无法安全提取请求 id。
pub fn line_too_large_frame(limit: usize, actual: usize) -> Value {
    json!({
        "type": "error",
        "error": format!("line too large: {actual} bytes exceeds limit {limit}"),
        "limitBytes": limit,
        "actualBytes": actual,
    })
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
        let v = hello_reply(Some(&json!("h1")), host_id());
        assert_eq!(v["type"], "response");
        assert_eq!(v["id"], "h1");
        assert_eq!(v["success"], true);
        assert_eq!(v["data"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(v["data"]["hostId"], host_id(), "hello 响应带 hostId");
        let v2 = hello_reply(None, host_id());
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

    // ── 行长上限（P0.1 对标 pi framing）──

    #[test]
    fn max_line_bytes_is_16mib() {
        assert_eq!(MAX_LINE_BYTES, 16 * 1024 * 1024);
        assert_eq!(MAX_LINE_BYTES, 16_777_216);
    }

    #[test]
    fn line_too_large_frame_shape() {
        let v = line_too_large_frame(MAX_LINE_BYTES, MAX_LINE_BYTES + 1);
        assert_eq!(v["type"], "error", "归 error 帧家族");
        assert_eq!(
            v["error"],
            format!(
                "line too large: {} bytes exceeds limit {}",
                MAX_LINE_BYTES + 1,
                MAX_LINE_BYTES
            )
        );
        assert_eq!(v["limitBytes"], MAX_LINE_BYTES);
        assert_eq!(v["actualBytes"], MAX_LINE_BYTES + 1);
        // 无 id：超长行不解析，无法安全提取请求 id
        assert!(v.get("id").is_none(), "超限错误帧不应带 id: {v}");
        // 一行一帧：序列化后不含换行
        let line = serialize_line(&v);
        assert!(!line.contains('\n'));
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

    // ── 错误脱敏 + 有界（P0.2）──

    #[test]
    fn sanitize_truncates_to_max_chars_on_char_boundary() {
        let home = "/Users/someone";
        // 替换 home（14 chars → 1 char）后仍远超 500：1 + 11 + 600 = 612 chars
        let msg = format!("{home}/workspace/{}", "错".repeat(600));
        let out = sanitize_error_with(&msg, Some(home));
        assert_eq!(out.chars().count(), MAX_ERROR_CHARS);
        // 不含 NUL/替换符——按 char 截断不产生 U+FFFD
        assert!(!out.contains('\u{FFFD}'), "char 截断不撕裂 UTF-8: {out}");
        // ASCII 同样截断
        let ascii = "z".repeat(600);
        assert_eq!(
            sanitize_error_with(&ascii, Some(home)).chars().count(),
            MAX_ERROR_CHARS
        );
    }

    #[test]
    fn sanitize_replaces_home_path_with_tilde() {
        let out = sanitize_error_with(
            "failed to read /Users/someone/.ion/config.json: not found",
            Some("/Users/someone"),
        );
        assert_eq!(out, "failed to read ~/.ion/config.json: not found");
    }

    #[test]
    fn sanitize_handles_home_without_leading_content_and_trailing_slash() {
        // home 带尾斜杠时不产生双斜杠
        let out = sanitize_error_with("bad path /Users/someone/x", Some("/Users/someone/"));
        assert_eq!(out, "bad path ~/x");
        // home 为空串 / 根：不替换
        assert_eq!(sanitize_error_with("/etc/hosts", Some("")), "/etc/hosts");
        assert_eq!(sanitize_error_with("/etc/hosts", Some("/")), "/etc/hosts");
        // 无 HOME 环境变量：原样
        assert_eq!(sanitize_error_with("/Users/x/y", None), "/Users/x/y");
    }

    #[test]
    fn sanitize_collapses_panic_and_backtrace_details() {
        for raw in [
            "panicked at ion-protocol/src/lib.rs:99:9:\nexplicit panic",
            "thread 'main' panicked at src/x.rs:1:1:\nboom\nstack backtrace:\n  0: ...",
            "note: run with RUST_BACKTRACE=1 for a backtrace",
        ] {
            let out = sanitize_error_with(raw, Some("/Users/someone"));
            assert_eq!(out, REDACTED_INTERNAL_ERROR, "panic 细节要收敛: {raw}");
            // 收敛后的文案本身不含原始路径/堆栈
            assert!(!out.contains("src/"));
        }
    }

    #[test]
    fn sanitize_preserves_public_error_semantics() {
        // 公共错误语义不许破坏：Unknown command / session not found / verb / 审批
        let public_errors = [
            "Unknown command: definitely_not_a_real_method".to_string(),
            "session not found on disk: sess_abc123".to_string(),
            "verb approval not found: vr-1".to_string(),
            "missing params.requestId".to_string(),
            "ui_respond rejected: requires a prior subscribe {ui:true} on the same connection"
                .to_string(),
            "request not found or already expired".to_string(),
            "invalid JSON: expected value at line 1 column 3".to_string(),
        ];
        for msg in public_errors {
            assert_eq!(
                sanitize_error_with(&msg, Some("/Users/someone")),
                msg,
                "公共错误文案必须原样通过: {msg}"
            );
        }
    }

    #[test]
    fn sanitize_is_idempotent() {
        let raw = format!("error at /Users/someone/x: {}", "长".repeat(600));
        let once = sanitize_error_with(&raw, Some("/Users/someone"));
        let twice = sanitize_error_with(&once, Some("/Users/someone"));
        assert_eq!(once, twice, "脱敏必须幂等");
    }

    #[test]
    fn error_constructors_sanitize_error_field() {
        // panic 细节经 worker/host 响应构造器都收敛
        let w = worker_response::error("1", "m", "panicked at src/x.rs:1:1:\nboom");
        assert_eq!(w["error"], REDACTED_INTERNAL_ERROR);
        let h = host_response::error(json!("1"), "panicked at src/x.rs:1:1:\nboom");
        assert_eq!(h["error"], REDACTED_INTERNAL_ERROR);
        let s = stream_error_frame("stack backtrace:\n  0: internal");
        assert_eq!(s["error"], REDACTED_INTERNAL_ERROR);
        let ij = host_response::invalid_json("panicked at x");
        assert_eq!(ij["error"], REDACTED_INTERNAL_ERROR);
        // 超长经构造器截断到 500
        let long = "y".repeat(600);
        let w2 = worker_response::error("1", "m", &long);
        assert_eq!(w2["error"].as_str().unwrap().chars().count(), MAX_ERROR_CHARS);
        // home 路径经构造器脱敏（用真实 HOME 断言"不含"，环境无关）
        if let Ok(home) = std::env::var("HOME") {
            let home = home.trim_end_matches('/');
            if !home.is_empty() {
                let h2 = host_response::error(json!("1"), format!("{home}/.ion/secret file"));
                assert!(
                    !h2["error"].as_str().unwrap().contains(home),
                    "构造器不得泄漏 home 路径: {}",
                    h2["error"]
                );
            }
        }
        // rpc_response 事件的 error 摘要同样脱敏
        let ev = rpc_response_event("s", "1", "m", false, Some("panicked at src/x"));
        assert_eq!(ev["event"]["error"], REDACTED_INTERNAL_ERROR);
    }

    // ── host 逻辑实例身份（P0.3）──

    #[test]
    fn generate_host_id_is_canonical_lowercase_uuid_v4() {
        for _ in 0..32 {
            let id = generate_host_id();
            assert_eq!(id.len(), 36, "UUID 长度 36: {id}");
            let bytes = id.as_bytes();
            for (i, b) in bytes.iter().enumerate() {
                match i {
                    8 | 13 | 18 | 23 => assert_eq!(*b, b'-', "连字符位置 {i}: {id}"),
                    _ => assert!(b.is_ascii_hexdigit() && !b.is_ascii_uppercase(), "小写 hex @ {i}: {id}"),
                }
            }
            // version 位 = 4，variant 位 ∈ {8,9,a,b}（规范 UUIDv4，对齐 pi ServerId）
            assert_eq!(&id[14..15], "4", "version 位必须是 4: {id}");
            assert!(
                matches!(&id[19..20], "8" | "9" | "a" | "b"),
                "variant 位必须是 8/9/a/b: {id}"
            );
        }
    }

    #[test]
    fn generate_host_id_is_unique_across_calls() {
        // 兜底随机源也必须保证"进程内多次生成互不相同"
        let a = generate_host_id();
        let b = generate_host_id();
        assert_ne!(a, b, "两次生成不应相同: {a} vs {b}");
    }

    #[test]
    fn host_id_is_stable_within_process() {
        assert_eq!(host_id(), host_id(), "进程内 hostId 稳定");
        let canonical = generate_host_id();
        assert_eq!(host_id().len(), canonical.len());
    }

    #[test]
    fn hello_reply_carries_host_id() {
        let v = hello_reply(Some(&json!("h1")), "3f2b8c6a-1d4e-4f50-9a1b-2c3d4e5f6078");
        assert_eq!(v["type"], "response");
        assert_eq!(v["id"], "h1");
        assert_eq!(v["success"], true);
        assert_eq!(v["data"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(
            v["data"]["hostId"],
            "3f2b8c6a-1d4e-4f50-9a1b-2c3d4e5f6078",
            "hello 响应必须携带 hostId（逻辑实例身份）"
        );
        let v2 = hello_reply(None, "x");
        assert!(v2["id"].is_null());
        assert_eq!(v2["data"]["hostId"], "x");
    }
}
