/// Conversation panel model: parse get_session_messages into collapsible lines.
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MsgLine {
    pub kind: MsgKind,
    pub header: String,
    pub body: String,
    pub expanded: bool,
    pub error: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MsgKind {
    User,
    Assistant,
    Tool,
    System,
    Unknown,
}

/// running/无结果 的工具块默认展开；e 切换后按 id 记忆。
pub fn message_lines(
    messages: &[Value],
    expanded: &std::collections::HashSet<String>,
) -> Vec<MsgLine> {
    let mut out = Vec::new();
    for (i, m) in messages.iter().enumerate() {
        let id = m
            .get("id")
            .or_else(|| m.get("entryId"))
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| format!("idx-{i}"));
        let obj = m.get("message").unwrap_or(m);
        let role = obj
            .get("role")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let mtype = m
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();

        // Extract primary text.
        let text = extract_text(obj);
        let kind = match role.as_str() {
            "user" => MsgKind::User,
            "assistant" => MsgKind::Assistant,
            _ => match (mtype.as_str(), role.as_str()) {
                ("message", "tool") | (_, "tool") => MsgKind::Tool,
                ("custom", _) | ("custom_message", _) => MsgKind::System,
                ("message", _) => MsgKind::Unknown,
                _ => {
                    if m.get("toolCallId").is_some() || m.get("tool_call_id").is_some() {
                        MsgKind::Tool
                    } else {
                        MsgKind::Unknown
                    }
                }
            },
        };

        let error = obj
            .get("isError")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
            || text.starts_with("Error");

        match kind {
            MsgKind::User => out.push(MsgLine {
                kind,
                header: "❯ user".into(),
                body: text,
                expanded: true,
                error,
            }),
            MsgKind::Assistant => {
                if text.is_empty() {
                    continue;
                }
                out.push(MsgLine {
                    kind,
                    header: "assistant".into(),
                    body: text,
                    expanded: true,
                    error,
                })
            }
            MsgKind::Tool => {
                let tool_name = obj
                    .get("toolName")
                    .or_else(|| obj.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("tool");
                let running = text.is_empty();
                let key = format!("tool:{id}");
                let expanded = if running || error {
                    !expanded.contains(&format!("-{key}"))
                } else {
                    expanded.contains(&key)
                };
                out.push(MsgLine {
                    kind,
                    header: format!("▸ [tool] {tool_name}"),
                    body: text,
                    expanded,
                    error,
                });
            }
            MsgKind::System => out.push(MsgLine {
                kind,
                header: format!("· {mtype}"),
                body: text,
                expanded: false,
                error,
            }),
            MsgKind::Unknown => {
                if !text.is_empty() {
                    out.push(MsgLine {
                        kind,
                        header: mtype.clone(),
                        body: text,
                        expanded: false,
                        error,
                    });
                }
            }
        }
    }
    out
}

fn extract_text(obj: &Value) -> String {
    // Text may be at content (string) or content[] with {"type":"text","text":..}
    if let Some(s) = obj.get("content").and_then(|c| c.as_str()) {
        return s.to_string();
    }
    if let Some(arr) = obj.get("content").and_then(|c| c.as_array()) {
        let mut parts = Vec::new();
        for block in arr {
            if block.get("type").and_then(|t| t.as_str()) == Some("text")
                && let Some(t) = block.get("text").and_then(|t| t.as_str())
            {
                parts.push(t.to_string());
            }
        }
        return parts.join("\n");
    }
    // Fallbacks for compact shapes.
    for key in ["text", "output", "result"] {
        if let Some(s) = obj.get(key).and_then(|v| v.as_str()) {
            return s.to_string();
        }
    }
    String::new()
}

pub fn render_lines(lines: &[MsgLine]) -> Vec<String> {
    let mut out = Vec::new();
    for l in lines {
        if l.expanded {
            if !l.header.is_empty() {
                out.push(l.header.clone());
            }
            for seg in l.body.lines() {
                out.push(format!("  {seg}"));
            }
        } else {
            let mut header = l.header.clone();
            if !l.body.is_empty() && l.kind == MsgKind::Tool {
                let first = l.body.lines().next().unwrap_or("").to_string();
                header.push_str(&format!(" — {}", truncate(&first, 60)));
            }
            out.push(header);
        }
    }
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}

pub fn parse_messages(response: &Value) -> Vec<Value> {
    response
        .get("data")
        .and_then(|d| d.get("messages"))
        .and_then(|m| m.as_array())
        .cloned()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn user_assistant_tool_error_mapping() {
        let msgs = vec![
            json!({"message": {"role": "user", "content": "hi"}}),
            json!({"message": {"role": "assistant", "content": [{"type": "text", "text": "hello"}]}}),
            json!({"message": {"role": "tool", "toolName": "bash", "content": "Error: boom"}, "id": "t1"}),
            json!({"message": {"role": "assistant", "content": [{"type": "text", "text": "recover"}]}}),
        ];
        let lines = message_lines(&msgs, &Default::default());
        assert_eq!(lines[0].kind, MsgKind::User);
        assert!(lines[0].expanded);
        assert_eq!(lines[0].header, "❯ user");
        assert_eq!(lines[1].kind, MsgKind::Assistant);
        assert_eq!(lines[2].kind, MsgKind::Tool);
        assert!(lines[2].error);
        assert!(lines[2].expanded, "error tool defaults expanded");
        assert_eq!(lines[3].kind, MsgKind::Assistant);
    }

    #[test]
    fn running_tool_expanded_and_toggle_collapses() {
        let msgs = vec![
            json!({"message": {"role": "tool", "toolName": "bash", "content": ""}, "id": "t1"}),
        ];
        let mut expanded: std::collections::HashSet<String> = Default::default();
        let lines = message_lines(&msgs, &expanded);
        assert!(lines[0].expanded, "running tool default open");
        expanded.insert("-tool:t1".into());
        let lines = message_lines(&msgs, &expanded);
        assert!(!lines[0].expanded, "user collapse respected");
    }

    #[test]
    fn system_custom_collapsed_and_unknown_tolerated() {
        let msgs = vec![
            json!({"type": "custom_message", "message": {"role": "system", "content": "note"}}),
            json!({"type": "message", "message": {"role": "weird", "content": "???"}, "id": "u1"}),
        ];
        let lines = message_lines(&msgs, &Default::default());
        assert_eq!(lines[0].kind, MsgKind::System);
        assert!(!lines[0].expanded);
        assert_eq!(lines[1].kind, MsgKind::Unknown);
    }

    #[test]
    fn render_collapsed_shows_tool_preview() {
        let msgs = vec![
            json!({"message": {"role": "tool", "toolName": "read", "content": "line1\nline2"}, "id": "t1"}),
        ];
        let mut expanded: std::collections::HashSet<String> = Default::default();
        expanded.insert("tool:t1".into());
        let lines = message_lines(&msgs, &expanded);
        let rendered = render_lines(&lines);
        assert_eq!(rendered[0], "▸ [tool] read");
        assert_eq!(rendered[1], "  line1");
    }

    #[test]
    fn parse_messages_extracts_data() {
        let resp = json!({"data": {"messages": [{"message": {"role": "user", "content": "x"}}]}});
        assert_eq!(parse_messages(&resp).len(), 1);
        assert!(parse_messages(&json!({"data": {}})).is_empty());
    }
}
