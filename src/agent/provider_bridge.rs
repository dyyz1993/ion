//! Provider Bridge — REMOTE_WORKER M2 客户端模式的零 key LLM 桥接（worker 侧）。
//!
//! 远程 worker 的所有 LLM 调用不发 HTTP，而是把请求经 stdout JSONL 转发给
//! Manager（Mac 主控端，真 key 所在地）代发，流式响应经 stdin 路由回来。
//! 设计文档：docs/design/REMOTE_WORKER.md §2.3 / §3.2-3.3。
//!
//! 线协议（复用现有 worker stdio 通道）：
//! - W→M `{"type":"manager_command","command":"llm_request","params":{
//!     "id","model_id","provider","context":{...},"options":{...}|null,"_from_worker":sid}}`
//! - W→M `{"type":"manager_command","command":"llm_cancel","params":{"id",...}}`
//! - M→W `{"method":"llm_chunk","params":{"id","ev":<StreamEvent serde>}}`
//!   （Done/Error 事件本身携带最终 AssistantMessage/usage，无需独立 done/error 消息）
//!
//! D4 管辖权：worker 只上报 model_id+provider；Model（含 base_url/key 注入）
//! 由 Manager 从自己的配置解析——被攻陷的 worker 无法指定任意端点。

use ion_provider::event_stream::{EventSender, EventStream};
use ion_provider::registry::{ApiProvider, ApiRegistry};
use ion_provider::types::{
    AssistantMessage, Context, Model, StopReason, StreamEvent, StreamOptions, Usage,
};
use std::collections::HashMap;
use std::io::Write as _;
use std::sync::{Mutex, OnceLock};
use tokio_util::sync::CancellationToken;

/// 桥接开关（Manager spawn 时注入 ION_PROVIDER_BRIDGE=1）。
pub fn is_enabled() -> bool {
    std::env::var("ION_PROVIDER_BRIDGE")
        .map(|v| v == "1")
        .unwrap_or(false)
}

// ── pending 流注册表：request_id → sender（EventSender 非 Clone，终帧消费）──

fn pending() -> &'static Mutex<HashMap<String, EventSender>> {
    static P: OnceLock<Mutex<HashMap<String, EventSender>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(HashMap::new()))
}

fn bridge_output(msg: &serde_json::Value) {
    // 与 worker_rpc::output 同款行原子写（io::stdout().lock() 每行独立持锁）
    let line = serde_json::to_string(msg).unwrap_or_default();
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{line}");
    let _ = stdout.flush();
}

fn aborted_message() -> AssistantMessage {
    AssistantMessage {
        role: "assistant".into(),
        content: vec![],
        api: String::new(),
        provider: String::new(),
        model: String::new(),
        response_model: None,
        response_id: None,
        usage: Usage::default(),
        stop_reason: StopReason::Aborted,
        error_message: Some("aborted via bridge cancel".into()),
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64,
    }
}

// ── BridgeProvider：拦截 worker 的一切 LLM 调用 ───────────────────────────

pub struct BridgeProvider;

// ── 分层超时（P0-2）───────────────────────────────────────────────────────
// 旧行为"Manager 死亡 → ssh 断 → worker 整体退出，无需本地超时兜底"有缺口：
// manager 挂死（如 host 卡死）但 stdio 未断时 llm_request 永久挂，worker 卡 Busy。
// 修复分两层，均不误杀正常长生成：
// ① 首块等待：llm_request 发出到第一个 chunk 到达（ION_BRIDGE_LLM_TIMEOUT_MS，
//    默认 120s）。manager 不发 ack，首帧=上游首个事件；推理模型首 token 可能超
//    120s 的远程部署应与管理端 ION_LLM_IDLE_TIMEOUT_MS 注入值同步调大。
// ② chunk 间空闲：收到任意 chunk 即复位，超限判失败。复用/对齐 agent_loop 的
//    ION_LLM_IDLE_TIMEOUT_MS（同 env 同默认 120s，REMOTE_WORKER 注入 600s）。
// 超时 → pending 表摘除 + 发 llm_cancel 停上游（防烧 token）+ 合成 Error 终帧
// （走既有错误路径：agent_loop 判 Error → 上层重试 / AUTO-RECOVERY）。

fn bridge_first_chunk_timeout_ms() -> u64 {
    std::env::var("ION_BRIDGE_LLM_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(120_000)
}

fn bridge_idle_chunk_timeout_ms() -> u64 {
    std::env::var("ION_LLM_IDLE_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(120_000)
}

fn timeout_error_message(text: &str) -> AssistantMessage {
    AssistantMessage {
        role: "assistant".into(),
        content: vec![],
        api: String::new(),
        provider: String::new(),
        model: String::new(),
        response_model: None,
        response_id: None,
        usage: Usage::default(),
        stop_reason: StopReason::Error,
        error_message: Some(text.to_string()),
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64,
    }
}

/// 从 stream() 抽出的注册+上报（返回 id 让单测能经 route_chunk 回流事件）。
fn register_llm_request(
    model: &Model,
    context: &Context,
    options: Option<&StreamOptions>,
) -> (String, EventStream) {
    let id = format!("llmreq_{}", &uuid::Uuid::new_v4().to_string()[..12]);
    let (stream, sender) = EventStream::new();
    pending().lock().unwrap().insert(id.clone(), sender);
    eprintln!(
        "[bridge] llm_request {id} registered (pending={})",
        pending().lock().unwrap().len()
    );

    // 只上报 model_id + provider（D4：不传 base_url/headers，Manager 自行解析）
    bridge_output(&serde_json::json!({
        "type": "manager_command",
        "command": "llm_request",
        "params": {
            "id": id,
            "model_id": model.id,
            "provider": model.provider,
            "context": context,
            "options": options,
            "_from_worker": crate::worker_rpc::current_session_id(),
        },
    }));
    (id, stream)
}

/// 桥接流看门狗：inner（pending 表注册的原始流）→ outer（消费方拿到的流），
/// 全量转发事件并施加分层超时（见上方"分层超时（P0-2）"注释）。
fn watchdog_bridge_stream(
    id: String,
    mut inner: EventStream,
    first_chunk_ms: u64,
    idle_chunk_ms: u64,
) -> EventStream {
    let (outer, sender) = EventStream::new();
    tokio::spawn(async move {
        let started = std::time::Instant::now();
        let mut got_first = false;
        loop {
            // ① 阶段是绝对 deadline（从 llm_request 发出时刻起算）；
            // ② 阶段每个事件复位计时。
            let budget_ms = if got_first {
                idle_chunk_ms
            } else {
                first_chunk_ms.saturating_sub(started.elapsed().as_millis() as u64)
            };
            let event = tokio::select! {
                ev = inner.recv() => ev,
                _ = tokio::time::sleep(std::time::Duration::from_millis(budget_ms)) => {
                    let (phase, limit) = if got_first {
                        ("idle", idle_chunk_ms)
                    } else {
                        ("first-chunk", first_chunk_ms)
                    };
                    eprintln!(
                        "[bridge] {id} {phase} timeout after {limit}ms — 合成 Error 终帧"
                    );
                    // 迟到的 chunk 不再路由（route_chunk 查不到 id → 静默丢弃）
                    pending().lock().unwrap().remove(&id);
                    sender.error(
                        StopReason::Error,
                        timeout_error_message(&format!(
                            "bridge llm_request {id}: {phase} timeout ({limit}ms) — manager hung or stdio stalled"
                        )),
                    );
                    // 通知 manager 关上游流，防已废请求继续烧 token（对齐 abort 路径）
                    bridge_output(&serde_json::json!({
                        "type": "manager_command",
                        "command": "llm_cancel",
                        "params": {"id": id},
                    }));
                    return;
                }
            };
            match event {
                Some(ev) => {
                    got_first = true;
                    match ev {
                        StreamEvent::Done { message, .. } => {
                            sender.end(message);
                            return;
                        }
                        StreamEvent::Error { reason, message } => {
                            sender.error(reason, message);
                            return;
                        }
                        other => sender.push(other),
                    }
                }
                None => {
                    // inner 无终帧收尾（异常路径）——丢弃 sender，result() 报
                    // "stream ended without result"，对齐 forward_with_done_tap 语义
                    return;
                }
            }
        }
    });
    outer
}

impl BridgeProvider {
    /// stream() 的显式超时参数版（单测直接传小值，避免 env 全进程竞态）。
    async fn stream_with_timeouts(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&StreamOptions>,
        cancel: Option<CancellationToken>,
        first_chunk_ms: u64,
        idle_chunk_ms: u64,
    ) -> ion_provider::error::ProviderResult<EventStream> {
        let (id, inner) = register_llm_request(model, context, options);

        // abort：本地流立即以 Aborted 收尾（result 不悬挂）+ 通知 Manager 关上游流
        //（防已中止的 SSE 继续烧 token）
        if let Some(token) = cancel {
            let cancel_id = id.clone();
            tokio::spawn(async move {
                token.cancelled().await;
                if let Some(s) = pending().lock().unwrap().remove(&cancel_id) {
                    s.error(StopReason::Aborted, aborted_message());
                }
                bridge_output(&serde_json::json!({
                    "type": "manager_command",
                    "command": "llm_cancel",
                    "params": {"id": cancel_id},
                }));
            });
        }

        Ok(watchdog_bridge_stream(
            id, inner, first_chunk_ms, idle_chunk_ms,
        ))
    }
}

#[async_trait::async_trait]
impl ApiProvider for BridgeProvider {
    async fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&StreamOptions>,
        cancel: Option<CancellationToken>,
    ) -> ion_provider::error::ProviderResult<EventStream> {
        self.stream_with_timeouts(
            model,
            context,
            options,
            cancel,
            bridge_first_chunk_timeout_ms(),
            bridge_idle_chunk_timeout_ms(),
        )
        .await
    }
}

/// 在 ApiRegistry 上启用桥接（worker_rpc 在 register_builtins 且非 faux 之后调用）。
pub fn install(registry: &mut ApiRegistry) {
    registry.override_all_with(|| Box::new(BridgeProvider));
}

// ── M→W 路由：worker 主循环收到 llm_chunk 时调用 ──────────────────────────

/// 处理 Manager 回流的 llm_chunk。false = 无法路由（id 未知——已取消/迟到的
/// 终帧，静默丢弃）。
pub fn route_chunk(params: &serde_json::Value) -> bool {
    let Some(id) = params.get("id").and_then(|v| v.as_str()) else {
        return false;
    };
    let Some(ev) = params.get("ev") else {
        return false;
    };
    let Ok(event) = serde_json::from_value::<StreamEvent>(ev.clone()) else {
        tracing::warn!("[bridge] bad llm_chunk payload for {id}");
        return false;
    };

    if !matches!(event, StreamEvent::Done { .. } | StreamEvent::Error { .. }) {
        // 普通分片：借 map 里的 sender push（&self），不取走
        let map = pending().lock().unwrap();
        match map.get(id) {
            Some(sender) => {
                sender.push(event);
                true
            }
            None => {
                let keys: Vec<String> = map.keys().cloned().collect();
                eprintln!("[bridge] chunk id={id} UNKNOWN (pending keys={keys:?})");
                false
            }
        }
    } else {
        // 终帧：取出 owned sender 消费（end/error 完成 result oneshot）
        let Some(sender) = pending().lock().unwrap().remove(id) else {
            return false;
        };
        match event {
            StreamEvent::Done { message, .. } => {
                sender.end(message);
                true
            }
            StreamEvent::Error { reason, message } => {
                sender.error(reason, message);
                true
            }
            _ => unreachable!(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ion_provider::types::{AssistantContentBlock, TextContent};

    fn text_msg(s: &str) -> AssistantMessage {
        AssistantMessage {
            role: "assistant".into(),
            content: vec![AssistantContentBlock::Text(TextContent {
                text: s.into(),
                text_signature: None,
            })],
            api: "openai-completions".into(),
            provider: "zai".into(),
            model: "glm-5.2".into(),
            response_model: None,
            response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 0,
        }
    }

    #[test]
    fn test_stream_event_serde_roundtrip() {
        // 桥接线格式的根基：StreamEvent 无损往返（含中文/终帧/usage）
        let ev = StreamEvent::Done {
            reason: StopReason::Stop,
            message: text_msg("你好 bridge"),
        };
        let j = serde_json::to_value(&ev).unwrap();
        let back: StreamEvent = serde_json::from_value(j).unwrap();
        match back {
            StreamEvent::Done { message, reason } => {
                assert_eq!(reason, StopReason::Stop);
                assert_eq!(message.model, "glm-5.2");
                assert!(matches!(
                    message.content[0],
                    AssistantContentBlock::Text(ref t) if t.text == "你好 bridge"
                ));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[tokio::test]
    async fn test_route_chunk_delivers_and_completes() {
        // 端到端单测（不经 stdio）：pending 流收分片 + 终帧完成 result
        let id = "llmreq_test1";
        let (mut stream, sender) = EventStream::new();
        pending().lock().unwrap().insert(id.into(), sender);

        // 分片 1
        let delta = serde_json::json!({
            "id": id,
            "ev": {"TextDelta": {"content_index": 0, "delta": "你好", "partial": null}}
        });
        // partial 字段是 AssistantMessage——serde 需要合法值；给一个最小形状
        let partial = serde_json::to_value(text_msg("")).unwrap();
        let delta = {
            let mut d = delta;
            d["ev"]["TextDelta"]["partial"] = partial;
            d
        };
        assert!(route_chunk(&delta));

        // 终帧 Done
        let done = serde_json::json!({
            "id": id,
            "ev": {"Done": {"reason": "Stop", "message": text_msg("你好 bridge")}}
        });
        assert!(route_chunk(&done));

        // 收到的第一个事件应是 TextDelta，且 result 返回终帧消息
        let first = stream.recv().await;
        assert!(matches!(first, Some(StreamEvent::TextDelta { delta, .. }) if delta == "你好"));
        let result = stream.result().await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().model, "glm-5.2");
    }

    #[tokio::test]
    async fn test_route_chunk_unknown_id_false() {
        assert!(!route_chunk(
            &serde_json::json!({"id": "ghost", "ev": {"Start": {"partial": null}}})
        ));
    }

    fn bridge_model() -> Model {
        Model {
            id: "glm-5.2".into(),
            name: "GLM-5.2".into(),
            api: "openai-completions".into(),
            provider: "zai".into(),
            base_url: "https://bridge.invalid/v4".into(),
            reasoning: false,
            input: vec!["text".into()],
            cost: Default::default(),
            context_window: 128_000,
            max_tokens: 8_192,
            compat: None,
            headers: None,
        }
    }

    /// P0-2 复现：manager 永不应答（host 卡死但 stdio 未断）时 llm_request 永久挂。
    /// 修复前 stream() 无任何超时（注释称"靠 ssh 断连兜底"）——recv() 永不返回，
    /// 外层 2s 兜底超时显红。修复后首块等待超时（ION_BRIDGE_LLM_TIMEOUT_MS，
    /// 默认 120s，此处调小）到点合成 Error 终帧，交上层重试 / AUTO-RECOVERY。
    #[tokio::test]
    async fn test_bridge_first_chunk_timeout_fires() {
        unsafe { std::env::set_var("ION_BRIDGE_LLM_TIMEOUT_MS", "300") };
        let bridge = BridgeProvider;
        let model = bridge_model();
        let context = Context::new(None, vec![]);
        let mut stream = bridge
            .stream(&model, &context, None, None)
            .await
            .expect("stream registered");
        let ev = tokio::time::timeout(std::time::Duration::from_secs(2), stream.recv())
            .await
            .expect("首块超时应合成 Error 而不是永久挂死（P0-2）");
        assert!(
            matches!(ev, Some(StreamEvent::Error { .. })),
            "期望 Error 终帧，实际 {ev:?}"
        );
    }

    fn text_delta_chunk(id: &str, text: &str) -> serde_json::Value {
        let mut d = serde_json::json!({
            "id": id,
            "ev": {"TextDelta": {"content_index": 0, "delta": text, "partial": null}}
        });
        // partial 是必填 AssistantMessage（serde 需要合法值）
        d["ev"]["TextDelta"]["partial"] = serde_json::to_value(text_msg("")).unwrap();
        d
    }

    /// ② chunk 间空闲超时：首块及时到达后流中断（manager 中途挂死）→
    /// idle 上限（对齐 ION_LLM_IDLE_TIMEOUT_MS 语义）判失败 + pending 清理。
    #[tokio::test]
    async fn test_bridge_idle_chunk_timeout_fires() {
        let model = bridge_model();
        let context = Context::new(None, vec![]);
        let (id, inner) = register_llm_request(&model, &context, None);
        let mut stream = watchdog_bridge_stream(id.clone(), inner, 60_000, 200);

        // 首块及时到达（解除 ① 阶段计时，进入 ② idle 计时），并经看门狗转发
        assert!(route_chunk(&text_delta_chunk(&id, "首块")));
        let first = tokio::time::timeout(std::time::Duration::from_secs(5), stream.recv())
            .await
            .expect("首块应经看门狗转发");
        assert!(
            matches!(first, Some(StreamEvent::TextDelta { ref delta, .. }) if delta == "首块"),
            "实际 {first:?}"
        );

        // 随后静默（manager 中途挂死）→ idle 超时合成 Error
        let ev = tokio::time::timeout(std::time::Duration::from_secs(5), stream.recv())
            .await
            .expect("chunk 间空闲超时应合成 Error（P0-2 ②）");
        assert!(
            matches!(ev, Some(StreamEvent::Error { .. })),
            "期望 Error 终帧，实际 {ev:?}"
        );
        // pending 表已摘除：迟到的 chunk 不再路由
        assert!(
            !pending().lock().unwrap().contains_key(&id),
            "超时后应清理 pending 表"
        );
    }

    /// 正常流不被误杀：chunk 持续到达 + 终帧 Done → 事件/结果原样透传，
    /// 超时看门狗只在真静默时触发。
    #[tokio::test]
    async fn test_bridge_normal_stream_passes_through() {
        let model = bridge_model();
        let context = Context::new(None, vec![]);
        let (id, inner) = register_llm_request(&model, &context, None);
        let mut stream = watchdog_bridge_stream(id.clone(), inner, 10_000, 10_000);

        assert!(route_chunk(&text_delta_chunk(&id, "你好")));
        let done = serde_json::json!({
            "id": id,
            "ev": {"Done": {"reason": "Stop", "message": text_msg("你好 bridge")}}
        });
        assert!(route_chunk(&done));

        let first = stream.recv().await;
        assert!(matches!(first, Some(StreamEvent::TextDelta { delta, .. }) if delta == "你好"));
        let result = stream.result().await;
        assert!(result.is_ok(), "正常流 result 应 OK: {result:?}");
        assert_eq!(result.unwrap().model, "glm-5.2");
        assert!(
            !pending().lock().unwrap().contains_key(&id),
            "终帧消费后应清理 pending 表"
        );
    }
}
