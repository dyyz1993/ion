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

/// A conversation turn: one user prompt + assistant/tools until the next user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnGroup {
    pub title: String,
    pub tool_calls: usize,
    pub messages: Vec<MsgLine>,
    pub expanded: bool,
}

/// Group messages into turns. The latest turn defaults expanded; older turns
/// default collapsed to one summary line.
pub fn group_turns(messages: &[MsgLine]) -> Vec<TurnGroup> {
    let mut turns: Vec<TurnGroup> = Vec::new();
    for m in messages {
        if m.kind == MsgKind::User {
            turns.push(TurnGroup {
                title: m.body.clone(),
                tool_calls: 0,
                messages: Vec::new(),
                expanded: false,
            });
        }
        if turns.is_empty() {
            // Pre-turn assistant/tool messages: synthesize an implicit turn.
            turns.push(TurnGroup {
                title: "(before first prompt)".into(),
                tool_calls: 0,
                messages: Vec::new(),
                expanded: false,
            });
        }
        let turn = turns.last_mut().unwrap();
        turn.messages.push(m.clone());
        if m.kind == MsgKind::Tool {
            turn.tool_calls += 1;
        }
    }
    if let Some(last) = turns.last_mut() {
        last.expanded = true;
    }
    turns
}

pub fn render_turns(
    turns: &[TurnGroup],
    expanded_turns: &std::collections::HashSet<usize>,
) -> Vec<String> {
    let mut out = Vec::new();
    if turns.len() > 1 {
        out.push(format!(
            "▸ 历史轮次 ({}) — 已折叠",
            turns.len().saturating_sub(1)
        ));
    }
    for (i, t) in turns.iter().enumerate() {
        let expanded = t.expanded || expanded_turns.contains(&i);
        if !expanded {
            out.push(format!(
                "  ▸ 第{}轮 · {} · {} tool calls",
                i + 1,
                truncate_preview(&t.title, 28),
                t.tool_calls
            ));
            continue;
        }
        if i == turns.len() - 1 {
            out.push("── 当前轮次 ──".into());
        } else {
            out.push(format!(
                "▾ 第{}轮 · {}",
                i + 1,
                truncate_preview(&t.title, 28)
            ));
        }
        for l in crate::conversation::render_lines(&t.messages) {
            out.push(format!("  {l}"));
        }
    }
    out
}

fn truncate_preview(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}

#[cfg(test)]
mod turn_tests {
    use super::*;
    use serde_json::json;

    fn msgs() -> Vec<serde_json::Value> {
        vec![
            json!({"message": {"role": "user", "content": "第一轮：排查登录"}}),
            json!({"message": {"role": "assistant", "content": "我先查日志"}}),
            json!({"message": {"role": "tool", "toolName": "bash", "content": "tail logs"}, "id": "t1"}),
            json!({"message": {"role": "user", "content": "第二轮：检查 Redis"}}),
            json!({"message": {"role": "assistant", "content": "读取 redis.yml"}}),
            json!({"message": {"role": "tool", "toolName": "read", "content": "pool config"}, "id": "t2"}),
        ]
    }

    #[test]
    fn groups_by_user_prompt_and_expands_only_latest() {
        let lines = message_lines(&msgs(), &Default::default());
        let turns = group_turns(&lines);
        assert_eq!(turns.len(), 2);
        assert!(!turns[0].expanded);
        assert!(turns[1].expanded);
        assert_eq!(turns[0].tool_calls, 1);
        assert_eq!(turns[1].tool_calls, 1);
    }

    #[test]
    fn render_turns_collapses_history_and_expands_current() {
        let lines = message_lines(&msgs(), &Default::default());
        let turns = group_turns(&lines);
        let rendered = render_turns(&turns, &Default::default());
        assert!(rendered.iter().any(|l| l.contains("历史轮次 (1)")));
        assert!(
            rendered
                .iter()
                .any(|l| l.contains("第1轮 · 第一轮：排查登录 · 1 tool calls"))
        );
        assert!(rendered.iter().any(|l| l.contains("── 当前轮次 ──")));
        assert!(rendered.iter().any(|l| l.contains("❯ user")));
    }

    #[test]
    fn explicit_expansion_reopens_history_turn() {
        let lines = message_lines(&msgs(), &Default::default());
        let turns = group_turns(&lines);
        let mut expanded = std::collections::HashSet::new();
        expanded.insert(0);
        let rendered = render_turns(&turns, &expanded);
        assert!(
            rendered
                .iter()
                .any(|l| l.contains("▾ 第1轮 · 第一轮：排查登录"))
        );
        // No collapsed line for turn 1.
        assert!(
            !rendered
                .iter()
                .any(|l| l.contains("第1轮 · 第一轮：排查登录 · 1 tool calls"))
        );
    }

    #[test]
    fn empty_and_prefix_only_messages_tolerated() {
        assert!(group_turns(&[]).is_empty());
        let lines = message_lines(
            &[json!({"message": {"role": "assistant", "content": "prefix"}})],
            &Default::default(),
        );
        let turns = group_turns(&lines);
        assert_eq!(turns.len(), 1);
        assert!(turns[0].expanded);
    }
}

/// Chat composer state: draft text, cursor, attachments, and status summary.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Composer {
    pub draft: String,
    pub cursor: usize,
    pub attachments: Vec<String>,
    pub model: String,
    pub ctx_used: u64,
    pub ctx_max: u64,
    pub runtime_ms: u64,
    pub child_count: usize,
    pub peer_count: usize,
}

impl Composer {
    pub fn insert(&mut self, ch: char) {
        let byte = self.cursor.min(self.draft.len());
        if self.draft.is_empty() {
            self.draft.push(ch);
            self.cursor = ch.len_utf8();
        } else {
            self.draft.insert(byte, ch);
            self.cursor += ch.len_utf8();
        }
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let end = self.cursor;
        let start = self.draft[..end]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
        self.draft.replace_range(start..end, "");
        self.cursor = start;
    }

    #[allow(dead_code)]
    pub fn attach(&mut self, name: impl Into<String>) {
        self.attachments.push(name.into());
    }

    pub fn ctx_percent(&self) -> u32 {
        if self.ctx_max == 0 {
            0
        } else {
            u64::div_ceil(self.ctx_used * 100, self.ctx_max) as u32
        }
    }

    pub fn ctx_bar(&self, width: usize) -> String {
        let width = width.max(1) as u32;
        let percent = self.ctx_percent().min(100);
        let filled = (percent * width).div_ceil(100);
        let filled = filled.min(width);
        "█".repeat(filled as usize) + &"░".repeat((width - filled) as usize)
    }

    #[allow(dead_code)]
    pub fn status_line(&self) -> String {
        let p = self.ctx_percent();
        let arrow = if p > 85 {
            "red"
        } else if p > 60 {
            "amber"
        } else {
            "green"
        };
        format!(
            "{} · ctx {} {}% ({}/{}) · {}ms · 子{} · 同{}",
            self.model,
            self.ctx_bar(10),
            p,
            self.ctx_used,
            self.ctx_max,
            self.runtime_ms,
            self.child_count,
            self.peer_count,
        )
        .replace(" · ", &format!(" · <{arrow}> · ")) // simplified marker for test; real color is renderer-side
    }
}

#[cfg(test)]
mod composer_tests {
    use super::*;

    #[test]
    fn composer_edit_utf8_and_backspace() {
        let mut c = Composer::default();
        c.insert('中');
        assert_eq!((c.draft.as_str(), c.cursor), ("中", 3));
        c.insert('文');
        assert_eq!((c.draft.as_str(), c.cursor), ("中文", 6));
        c.insert('a');
        assert_eq!((c.draft.as_str(), c.cursor), ("中文a", 7));
        for ch in ['中', '文'] {
            c.insert(ch);
        }
        assert_eq!(c.draft, "中文a中文");
        c.backspace();
        assert_eq!(c.draft, "中文a中");
        assert_eq!(c.cursor, "中文a中".len());
    }

    #[test]
    fn composer_ctx_percent_ceil_and_bar() {
        let mut c = Composer {
            ctx_used: 1,
            ctx_max: 128,
            ..Default::default()
        };
        assert_eq!(c.ctx_percent(), 1);
        assert_eq!(c.ctx_bar(10).chars().filter(|c| *c == '█').count(), 1);
        c.ctx_used = 126;
        assert_eq!(c.ctx_percent(), 99);
        c.ctx_used = 128;
        assert_eq!(c.ctx_percent(), 100);
        assert_eq!(c.ctx_bar(10).chars().filter(|c| *c == '█').count(), 10);
    }

    #[test]
    fn composer_status_includes_context_and_runtime() {
        let c = Composer {
            model: "glm-5.2".into(),
            ctx_used: 78,
            ctx_max: 128,
            runtime_ms: 872_000,
            child_count: 1,
            peer_count: 2,
            ..Default::default()
        };
        let status = c.status_line();
        assert!(status.contains("glm-5.2"));
        assert!(status.contains("61%"));
        assert!(status.contains("78/128"));
        assert!(status.contains("872000ms"));
        assert!(status.contains("子1"));
        assert!(status.contains("同2"));
    }
}

/// Modal path input for adding attachments.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AttachmentPicker {
    pub open: bool,
    pub path: String,
}

impl AttachmentPicker {
    pub fn open(&mut self) {
        self.open = true;
        self.path.clear();
    }

    pub fn close(&mut self) {
        self.open = false;
        self.path.clear();
    }

    pub fn insert(&mut self, ch: char) {
        if self.open {
            self.path.push(ch);
        }
    }

    pub fn backspace(&mut self) {
        if self.path.is_empty() {
            return;
        }
        let end = self.path.len();
        let start = self.path[..end]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
        self.path.replace_range(start..end, "");
    }

    pub fn confirm(&mut self) -> Option<String> {
        let trimmed = self.path.trim().to_string();
        self.close();
        (!trimmed.is_empty()).then_some(trimmed)
    }
}

/// Streaming tool-call display state (design F skeleton).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveToolCard {
    pub name: String,
    pub command: String,
    pub output_tail: Vec<String>,
    pub elapsed_ms: u64,
}

impl LiveToolCard {
    pub fn spinner(&self) -> &'static str {
        const FRAMES: [&str; 4] = ["⠋", "⠙", "⠹", "⠸"];
        FRAMES[(self.elapsed_ms / 120) as usize % 4]
    }

    #[allow(dead_code)]
    pub fn push_output(&mut self, line: impl Into<String>) {
        self.output_tail.push(line.into());
        let overflow = self.output_tail.len().saturating_sub(10);
        if overflow > 0 {
            self.output_tail.drain(..overflow);
        }
    }

    pub fn render(&self) -> Vec<String> {
        let mut out = vec![format!(
            "{} [tool] {} — {} ({:.1}s)",
            self.spinner(),
            self.name,
            self.command,
            self.elapsed_ms as f64 / 1000.0
        )];
        out.extend(self.output_tail.iter().map(|l| format!("  {l}")));
        out.push("  ▌".into());
        out
    }

    pub fn render_completed(&self) -> Vec<String> {
        let mut out = vec![self.completed_header()];
        out.extend(self.output_tail.iter().map(|l| format!("  {l}")));
        out
    }

    pub fn completed_header(&self) -> String {
        format!(
            "▸ [tool] {} — {} ({:.1}s)",
            self.name,
            self.command,
            self.elapsed_ms as f64 / 1000.0
        )
    }
}

#[cfg(test)]
mod picker_and_live_tests {
    use super::*;

    #[test]
    fn attachment_picker_lifecycle_utf8_and_confirm() {
        let mut picker = AttachmentPicker::default();
        picker.open();
        for ch in ['/', '图', '片'] {
            picker.insert(ch);
        }
        assert_eq!(picker.path, "/图片");
        picker.backspace();
        assert_eq!(picker.path, "/图");
        let confirmed = picker.confirm();
        assert_eq!(confirmed.as_deref(), Some("/图"));
        assert!(!picker.open);
        assert!(picker.path.is_empty());
        picker.insert('x');
        assert!(picker.path.is_empty());
    }

    #[test]
    fn live_tool_card_tail_and_render() {
        let mut card = LiveToolCard {
            name: "bash".into(),
            command: "tail -f logs".into(),
            output_tail: Vec::new(),
            elapsed_ms: 1240,
        };
        for i in 0..12 {
            card.push_output(format!("line-{i}"));
        }
        assert_eq!(card.output_tail.len(), 10);
        assert_eq!(card.output_tail[0], "line-2");
        assert_eq!(card.output_tail[9], "line-11");
        let rendered = card.render();
        assert!(rendered[0].contains("[tool] bash — tail -f logs (1.2s)"));
        assert!(rendered.iter().any(|l| l.contains("line-11")));
        assert_eq!(*rendered.last().unwrap(), "  ▌".to_string());
    }

    #[test]
    fn spinner_cycles_frames() {
        let card = LiveToolCard {
            name: "bash".into(),
            command: "x".into(),
            output_tail: Vec::new(),
            elapsed_ms: 360,
        };
        assert_eq!(card.spinner(), "⠸");
    }
}
