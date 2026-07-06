// ─────────────────────────────────────────────────────────────────────────────
// UI 事件通道测试 — ExtensionEventBus 的 UI 路由
// ─────────────────────────────────────────────────────────────────────────────

use ion::event_bus::{ExtensionEvent, ExtensionEventBus};

// ── UI 事件构造 ────────────────────────────────────────────────────────────

#[test]
fn ui_event_new_ui_creates_ask_event() {
    let event = ExtensionEvent::new_ui("Ask", "权限请求", "是否允许读取 /tmp/secret?");
    assert_eq!(event.route, "ui");
    assert_eq!(event.custom_type, "Ask");
    assert_eq!(event.extension, "ui");
    let title = event.data.get("title").and_then(|v| v.as_str()).unwrap_or("");
    let message = event.data.get("message").and_then(|v| v.as_str()).unwrap_or("");
    assert_eq!(title, "权限请求");
    assert_eq!(message, "是否允许读取 /tmp/secret?");
}

#[test]
fn ui_event_has_correct_route() {
    let ext_event = ExtensionEvent::new("memory", "memory_saved");
    assert_eq!(ext_event.route, "extension", "plugin events should have route=extension");

    let ui_event = ExtensionEvent::new_ui("Notif", "标题", "消息");
    assert_eq!(ui_event.route, "ui", "UI events should have route=ui");
}

// ── subscribe_ui 只收 UI 事件 ──────────────────────────────────────────────

#[test]
fn subscribe_ui_only_receives_ui_events() {
    let mut bus = ExtensionEventBus::new();
    let mut rx = bus.subscribe_ui();

    // 发一个插件事件（应该收不到）
    let plugin_event = ExtensionEvent::new("memory", "memory_saved")
        .with_data(serde_json::json!({"key": "val"}));
    bus.broadcast(&plugin_event);

    // 发一个 UI 事件（应该能收到）
    let ui_event = ExtensionEvent::new_ui("Ask", "测试", "测试消息");
    bus.broadcast(&ui_event);

    // UI 订阅者应该只收到 UI 事件
    let received = rx.try_recv();
    match received {
        Ok(ev) => {
            assert_eq!(ev.route, "ui", "should receive ui event");
            assert_eq!(ev.custom_type, "Ask");
        }
        Err(_) => panic!("subscribe_ui should have received the UI event"),
    }

    // 不应再有消息（plugin_event 被过滤了）
    let extra = rx.try_recv();
    assert!(extra.is_err(), "should not receive plugin event through subscribe_ui");
}

// ── 普通 subscribe 不受 UI 事件影响 ────────────────────────────────────────

#[test]
fn plugin_subscribe_does_not_receive_ui_events() {
    let mut bus = ExtensionEventBus::new();
    let mut rx = bus.subscribe("memory");

    // UI 事件
    let ui_event = ExtensionEvent::new_ui("Ask", "测试", "消息");
    bus.broadcast(&ui_event);

    // 插件事件
    let plugin_event = ExtensionEvent::new("memory", "memory_saved");
    bus.broadcast(&plugin_event);

    // memory 订阅者应该只收到插件事件
    let received = rx.try_recv();
    match received {
        Ok(ev) => {
            assert_eq!(ev.route, "extension", "should only receive extension events");
            assert_eq!(ev.custom_type, "memory_saved");
        }
        Err(_) => panic!("subscribe memory should have received the plugin event"),
    }
}

// ── subscribe_all 收全部 ───────────────────────────────────────────────────

#[test]
fn subscribe_all_receives_both_routes() {
    let mut bus = ExtensionEventBus::new();
    let mut rx = bus.subscribe_all();

    let ui_event = ExtensionEvent::new_ui("Alert", "告警", "资源不足");
    bus.broadcast(&ui_event);

    let plugin_event = ExtensionEvent::new("todo", "task_done");
    bus.broadcast(&plugin_event);

    // 应该收到两条
    let first = rx.try_recv().expect("should receive first event");
    let second = rx.try_recv().expect("should receive second event");
    assert_ne!(first.route, second.route, "should receive both routes");
}

// ── AskResolved 事件 ───────────────────────────────────────────────────────

#[test]
fn ui_ask_resolved_event() {
    let event = ExtensionEvent::new_ui("AskResolved", "req_abc123", "allow")
        .with_data(serde_json::json!({"response": "allow", "resolved_by": "cli"}));
    assert_eq!(event.custom_type, "AskResolved");
    let response = event.data.get("response").and_then(|v| v.as_str()).unwrap_or("");
    assert_eq!(response, "allow");
}
