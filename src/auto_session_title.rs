//! Auto Session Title — automatically generate a title for each session.
//!
//! After the first turn, generates a short title and writes it to:
//! 1. session.jsonl as a custom entry (type=session_name) — export 读这个
//! 2. session index — ion sessions 列表显示
//! 3. session-titles.json — 兼容旧格式

use crate::agent::error::AgentResult;
use crate::agent::extension::{Extension, TurnContext};
use std::sync::atomic::{AtomicBool, Ordering};

pub struct AutoSessionTitle {
    done: AtomicBool,
    name: String,
}

impl Default for AutoSessionTitle {
    fn default() -> Self {
        Self::new()
    }
}

impl AutoSessionTitle {
    pub fn new() -> Self {
        Self {
            done: AtomicBool::new(false),
            name: "auto-session-title".into(),
        }
    }

    fn generate_title_heuristic(first_message: &str) -> String {
        let trimmed = first_message.trim();

        if trimmed.starts_with('/') || trimmed.starts_with('!') {
            return trimmed.chars().take(80).collect();
        }

        let first_line = trimmed.lines().next().unwrap_or(trimmed);
        let first_sentence = first_line
            .split(['.', '。', '!', '?'])
            .next()
            .unwrap_or(first_line);

        let title = first_sentence.trim();

        if title.chars().count() > 80 {
            let truncated: String = title.chars().take(77).collect();
            format!("{truncated}...")
        } else if title.is_empty() {
            "Untitled".to_string()
        } else {
            title.to_string()
        }
    }
}

#[async_trait::async_trait]
impl Extension for AutoSessionTitle {
    fn name(&self) -> &str {
        &self.name
    }

    async fn on_turn_end(&self, ctx: &TurnContext) -> AgentResult<()> {
        if self.done.load(Ordering::SeqCst) {
            return Ok(());
        }

        if ctx.turn_index > 0 {
            return Ok(());
        }

        let first_user_msg = ctx.messages.iter().find_map(|msg| match msg {
            crate::agent::messages::Message::User(u) => {
                let text = u
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        crate::agent::messages::ContentBlock::Text(t) => Some(t.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                if text.is_empty() { None } else { Some(text) }
            }
            _ => None,
        });

        if let Some(text) = first_user_msg {
            let title = Self::generate_title_heuristic(&text);
            tracing::info!("[auto-session-title] generated: \"{}\"", title);

            // ★ 写入 session.jsonl 作为 custom entry（type=session_name）
            // 这样 export 时能从 entries 里读到最后一条 session_name。
            if let (Some(cwd), Some(sid)) = (ctx.session_cwd.as_ref(), ctx.session_id.as_ref()) {
                let entry = serde_json::json!({
                    "type": "session_name",
                    "name": title,
                    "session_id": sid,
                });
                crate::session_jsonl::append_raw_entry(cwd, &entry);
                tracing::info!("[auto-session-title] wrote session_name entry to jsonl");

                // 同时更新 session index（让 ion sessions 显示标题）
                crate::session_index::SessionIndex::set_name(sid, &title);
            }

            // 兼容：也写 session-titles.json（用 session_id 做 key，不再用 turn_N）
            let titles_path = crate::paths::root()
                .join("agent")
                .join("session-titles.json");
            let mut titles: std::collections::HashMap<String, String> =
                std::fs::read_to_string(&titles_path)
                    .ok()
                    .and_then(|s| serde_json::from_str(&s).ok())
                    .unwrap_or_default();
            let session_key = ctx.session_id.clone().unwrap_or_else(|| format!("turn_{}", ctx.turn_index));
            titles.insert(session_key, title.clone());
            if let Ok(json) = serde_json::to_string_pretty(&titles) {
                let _ = std::fs::write(&titles_path, json);
            }

            self.done.store(true, Ordering::SeqCst);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_short_message() {
        let title = AutoSessionTitle::generate_title_heuristic("Fix the bug in parser");
        assert_eq!(title, "Fix the bug in parser");
    }

    #[test]
    fn test_long_message_truncated() {
        let long = "A".repeat(100);
        let title = AutoSessionTitle::generate_title_heuristic(&long);
        assert!(title.ends_with("..."));
        assert!(title.len() <= 80);
    }

    #[test]
    fn test_multiline() {
        let title = AutoSessionTitle::generate_title_heuristic("First line\nSecond line");
        assert_eq!(title, "First line");
    }

    #[test]
    fn test_first_sentence() {
        let title = AutoSessionTitle::generate_title_heuristic("Fix the bug. Also update docs.");
        assert_eq!(title, "Fix the bug");
    }

    #[test]
    fn test_command_prefix() {
        let title = AutoSessionTitle::generate_title_heuristic("!ls -la");
        assert_eq!(title, "!ls -la");
    }

    #[test]
    fn test_empty() {
        let title = AutoSessionTitle::generate_title_heuristic("   ");
        assert_eq!(title, "Untitled");
    }

    #[test]
    fn test_chinese() {
        let title = AutoSessionTitle::generate_title_heuristic("修复解析器的 bug。然后更新文档。");
        assert_eq!(title, "修复解析器的 bug");
    }
}
