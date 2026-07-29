//! 各事件 stdin JSON 组装
//!
//! 对齐 pi 的 stdin-builder.ts，Claude Code 兼容协议。
//! 所有事件 stdin 都包含通用字段（session_id/cwd/hook_event_name/workspace_roots），
//! 各事件再附加自己的字段。

use serde_json::{json, Value};

/// 通用字段（所有事件都有）
/// 对齐 Claude Code 的 createBaseHookInput：session_id + cwd + transcript_path +
/// permission_mode + hook_event_name。
pub fn common_fields(event: &str) -> Value {
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let session_id = std::env::var("ION_SESSION_ID").unwrap_or_default();
    // transcript_path: session JSONL 路径，让 hook 脚本能读对话历史
    let transcript_path = if !session_id.is_empty() {
        let cwd_for_path = std::env::var("ION_SESSION_CWD").unwrap_or_else(|_| cwd.clone());
        crate::session_jsonl::session_path(&cwd_for_path).to_string_lossy().to_string()
    } else {
        String::new()
    };
    let mut obj = serde_json::Map::new();
    obj.insert("session_id".into(), json!(session_id));
    obj.insert("cwd".into(), json!(cwd.clone()));
    obj.insert("transcript_path".into(), json!(transcript_path));
    obj.insert("hook_event_name".into(), json!(event));
    obj.insert("workspace_roots".into(), json!([cwd]));
    // permission_mode（可选）
    if let Ok(mode) = std::env::var("ION_SECURITY_MODE") {
        obj.insert("permission_mode".into(), json!(mode));
    }
    Value::Object(obj)
}

/// 合并通用字段 + 事件特有字段
fn build(event: &str, extra: Value) -> Value {
    let mut stdin = common_fields(event);
    if let Some(obj) = stdin.as_object_mut() && let Some(extra_obj) = extra.as_object() {
        obj.extend(extra_obj.clone());
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
    build("PreCompact", json!({
        "message_count": message_count,
        "trigger": "auto",
        "custom_instructions": "",
    }))
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

pub fn post_tool_use(tool_name: &str, tool_input: &Value, tool_response: &Value, is_error: bool, tool_call_id: &str) -> Value {
    let event = if is_error { "PostToolUseFailure" } else { "PostToolUse" };
    build(event, json!({
        "tool_name": tool_name,
        "llm_tool_name": tool_name,
        "tool_input": tool_input,
        "tool_response": tool_response,
        "tool_use_id": tool_call_id,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_common_fields_has_transcript_path() {
        let stdin = common_fields("PreToolUse");
        assert!(stdin.get("transcript_path").is_some(), "transcript_path must exist");
        assert!(stdin.get("session_id").is_some(), "session_id must exist");
        assert!(stdin.get("cwd").is_some(), "cwd must exist");
        assert!(stdin.get("hook_event_name").is_some(), "hook_event_name must exist");
        assert_eq!(stdin["hook_event_name"], "PreToolUse");
    }

    #[test]
    fn test_pre_tool_use_has_tool_use_id() {
        let stdin = pre_tool_use("Bash", &json!({"command": "ls"}), "call_abc123");
        assert_eq!(stdin["tool_name"], "Bash");
        assert_eq!(stdin["tool_use_id"], "call_abc123");
        assert!(stdin.get("tool_input").is_some());
    }

    #[test]
    fn test_post_tool_use_has_tool_use_id_and_response() {
        let stdin = post_tool_use(
            "Read",
            &json!({"file_path": "src/lib.rs"}),
            &json!({"output": "file contents"}),
            false,
            "call_xyz789",
        );
        assert_eq!(stdin["hook_event_name"], "PostToolUse");
        assert_eq!(stdin["tool_use_id"], "call_xyz789");
        assert!(stdin.get("tool_response").is_some(), "tool_response must exist");
    }

    #[test]
    fn test_post_tool_use_failure_event_name() {
        let stdin = post_tool_use(
            "Bash",
            &json!({"command": "false"}),
            &json!({"output": "error"}),
            true,
            "call_err",
        );
        assert_eq!(stdin["hook_event_name"], "PostToolUseFailure");
    }

    #[test]
    fn test_stop_has_last_assistant_message() {
        let stdin = stop("task completed", 0);
        assert_eq!(stdin["last_assistant_message"], "task completed");
        assert_eq!(stdin["stop_hook_active"], false);
    }

    #[test]
    fn test_session_start_has_source() {
        let stdin = session_start("SessionStart", "startup");
        assert_eq!(stdin["source"], "startup");
        assert_eq!(stdin["reason"], "startup");
    }

    #[test]
    fn test_user_prompt_submit_has_prompt() {
        let stdin = user_prompt_submit("hello world");
        assert_eq!(stdin["prompt"], "hello world");
    }
}
