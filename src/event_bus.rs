//! Extension EventBus — 扩展事件总线
//!
//! 插件的所有事件通过这里广播给订阅者（socket subscriber / CLI / Gateway）。
//! 插件只管 `emit_plugin_event()`，不碰传输层。
//!
//! ## 订阅方式
//! - 按 `plugin` 过滤：`subscribe("memory")` → 只收 memory 事件
//! - 按 `session` 过滤：`subscribe_with_session("memory", "sess_xxx")` → 只收某 session
//!
//! ## 背压处理
//! 每个 subscriber 一个 bounded queue（1000 条）。慢客户端队列满后自动断开。

use tokio::sync::mpsc;

/// 插件事件的优先级/可见性
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EventVisibility {
    /// 发给 LLM 也发给订阅者
    LlmAndUi,
    /// 仅订阅者可见，不发给 LLM
    UiOnly,
}

/// 一条插件事件
#[derive(Clone, Debug)]
pub struct ExtensionEvent {
    /// 来源插件名（"memory", "todo" 等）
    pub extension: String,
    /// 自定义类型（"memory_saved", "memory_injected" 等）
    pub custom_type: String,
    /// 可选的 session 作用域
    pub session: Option<String>,
    /// 数据载荷
    pub data: serde_json::Value,
    /// 是否已持久化到 session.jsonl
    pub persisted: bool,
    /// 可见性
    pub visibility: EventVisibility,
    /// 关联 ID（追踪用）
    pub correlation_id: String,
}

impl ExtensionEvent {
    pub fn new(extension: &str, custom_type: &str) -> Self {
        Self {
            extension: extension.to_string(),
            custom_type: custom_type.to_string(),
            session: None,
            data: serde_json::Value::Null,
            persisted: false,
            visibility: EventVisibility::UiOnly,
            correlation_id: String::new(),
        }
    }

    pub fn with_session(mut self, sid: &str) -> Self { self.session = Some(sid.to_string()); self }
    pub fn with_data(mut self, data: serde_json::Value) -> Self { self.data = data; self }
    pub fn with_persisted(mut self, p: bool) -> Self { self.persisted = p; self }
    pub fn with_visibility(mut self, v: EventVisibility) -> Self { self.visibility = v; self }
    pub fn with_correlation(mut self, cid: &str) -> Self { self.correlation_id = cid.to_string(); self }
}

/// Subscriber 过滤器
#[derive(Clone, Debug)]
struct SubFilter {
    extension: Option<String>,
    session: Option<String>,
}

/// 单个订阅者
struct Subscriber {
    filter: SubFilter,
    tx: mpsc::Sender<ExtensionEvent>,
}

/// 插件事件总线
#[derive(Default)]
pub struct ExtensionEventBus {
    subscribers: Vec<Subscriber>,
}

impl ExtensionEventBus {
    pub fn new() -> Self { Self { subscribers: Vec::new() } }

    /// 订阅指定插件的所有事件（不限制 session）
    pub fn subscribe(&mut self, extension: &str) -> mpsc::Receiver<ExtensionEvent> {
        self.subscribe_with_filter(SubFilter {
            extension: Some(extension.to_string()),
            session: None,
        })
    }

    /// 订阅指定插件 + session 的事件
    pub fn subscribe_with_session(&mut self, extension: &str, session: &str) -> mpsc::Receiver<ExtensionEvent> {
        self.subscribe_with_filter(SubFilter {
            extension: Some(extension.to_string()),
            session: Some(session.to_string()),
        })
    }

    /// 订阅全部事件（插件 + session 都不限制）
    pub fn subscribe_all(&mut self) -> mpsc::Receiver<ExtensionEvent> {
        self.subscribe_with_filter(SubFilter { extension: None, session: None })
    }

    fn subscribe_with_filter(&mut self, filter: SubFilter) -> mpsc::Receiver<ExtensionEvent> {
        // bounded queue: 1000 条，慢客户端自动断开
        let (tx, rx) = mpsc::channel::<ExtensionEvent>(1000);
        self.subscribers.push(Subscriber { filter, tx });
        rx
    }

    /// 广播事件给所有匹配的 subscriber
    pub fn broadcast(&mut self, event: &ExtensionEvent) {
        self.subscribers.retain(|sub| {
            // 过滤
            if let Some(ref extension) = sub.filter.extension {
                if extension != &event.extension { return true; }
            }
            if let Some(ref session) = sub.filter.session {
                if let Some(ref ev_sess) = event.session {
                    if session != ev_sess { return true; }
                } else {
                    return true;
                }
            }
            // 发送（bounded queue，失败 = 客户端太慢，断开）
            match sub.tx.try_send(event.clone()) {
                Ok(()) => true,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    tracing::warn!("[eventbus] subscriber too slow, disconnecting (extension={:?}, session={:?})",
                        sub.filter.extension, sub.filter.session);
                    false
                }
                Err(mpsc::error::TrySendError::Closed(_)) => false,
            }
        });
    }

    /// 广播事件 JSON 值（构造 ExtensionEvent）
    pub fn broadcast_raw(&mut self, extension: &str, custom_type: &str, data: serde_json::Value) {
        let event = ExtensionEvent::new(extension, custom_type).with_data(data);
        self.broadcast(&event);
    }

    /// 清理已关闭的订阅者
    pub fn cleanup(&mut self) {
        self.subscribers.retain(|sub| !sub.tx.is_closed());
    }
}
