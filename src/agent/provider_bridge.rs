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

#[async_trait::async_trait]
impl ApiProvider for BridgeProvider {
    async fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&StreamOptions>,
        cancel: Option<CancellationToken>,
    ) -> ion_provider::error::ProviderResult<EventStream> {
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

        // abort：本地流立即以 Aborted 收尾（result 不悬挂）+ 通知 Manager 关上游流
        //（防已中止的 SSE 继续烧 token）
        if let Some(token) = cancel {
            let cancel_id = id;
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

        // Manager 死亡 → ssh 断 → worker 整体退出，无需本地超时兜底。
        Ok(stream)
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
}
