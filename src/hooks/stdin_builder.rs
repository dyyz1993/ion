//! 各事件 stdin JSON 组装
//!
//! 对齐 pi 的 stdin-builder.ts，Claude Code 兼容协议。
//! 所有事件 stdin 都包含通用字段（session_id/cwd/hook_event_name/workspace_roots），
//! 各事件再附加自己的字段。

use serde_json::{json, Value};

/// 通用字段（所有事件都有）
fn common_fields(event: &str) -> Value {
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    json!({
        "session_id": std::env::var("ION_SESSION_ID").unwrap_or_default(),
        "cwd": cwd.clone(),
        "hook_event_name": event,
        "workspace_roots": [cwd],
    })
}

/// 合并通用字段 + 事件特有字段
fn build(event: &str, extra: Value) -> Value {
    let mut stdin = common_fields(event);
    if let Some(obj) = stdin.as_object_mut() {
        if let Some(extra_obj) = extra.as_object() {
            obj.extend(extra_obj.clone());
        }
    }
    stdin
}

pub fn session_start(event: &str, reason: &str) -> Value {
    build(event, json!({"reason": reason, "source": reason}))
}

pub fn session_end() -> Value {
    build("SessionEnd", json!({}))
}

pub fn pre_compact(message_count: usize) -> Value {
    build("PreCompact", json!({"message_count": message_count}))
}

pub fn user_prompt_submit(prompt: &str) -> Value {
    build("UserPromptSubmit", json!({"prompt": prompt}))
}

pub fn pre_tool_use(tool_name: &str, tool_input: &Value, tool_call_id: &str) -> Value {
    build("PreToolUse", json!({
        "tool_name": tool_name,
        "llm_tool_name": tool_name,
        "tool_input": tool_input,
        "tool_use_id": tool_call_id,
    }))
}

pub fn post_tool_use(tool_name: &str, tool_input: &Value, tool_response: &Value, is_error: bool) -> Value {
    let event = if is_error { "PostToolUseFailure" } else { "PostToolUse" };
    build(event, json!({
        "tool_name": tool_name,
        "llm_tool_name": tool_name,
        "tool_input": tool_input,
        "tool_response": tool_response,
    }))
}

pub fn subagent_start() -> Value {
    build("SubagentStart", json!({}))
}

pub fn subagent_stop(last_message: &str, loop_count: u32) -> Value {
    build("SubagentStop", json!({
        "last_assistant_message": last_message,
        "loop_count": loop_count,
        "stop_hook_active": loop_count > 0,
    }))
}

pub fn stop(last_message: &str, loop_count: u32) -> Value {
    build("Stop", json!({
        "last_assistant_message": last_message,
        "loop_count": loop_count,
        "stop_hook_active": loop_count > 0,
    }))
}

pub fn notification(notification_type: &str, message: &str) -> Value {
    build("Notification", json!({
        "notification_type": notification_type,
        "message": message,
    }))
}

pub fn permission_request(tool: &str, args: &Value) -> Value {
    build("PermissionRequest", json!({
        "tool": tool,
        "args": args,
    }))
}
