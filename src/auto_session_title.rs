//! Auto Session Title — automatically generate a title for each session.
//!
//! After the first turn, calls a fast model to generate a short title.
//! Falls back to heuristic if LLM call fails (errors silently swallowed).
//! Writes the title to session.jsonl as a custom entry (type=session_name).

use crate::agent::error::AgentResult;
use crate::agent::extension::{Extension, TurnContext};
use ion_provider::registry;
use ion_provider::types::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub struct AutoSessionTitle {
    done: AtomicBool,
    name: String,
    /// ApiRegistry（让扩展能调 fast model 生成标题）
    registry: Option<Arc<registry::ApiRegistry>>,
    /// Fast model（用于标题生成，比主模型便宜/快）
    title_model: Option<Model>,
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
            registry: None,
            title_model: None,
        }
    }

    pub fn with_registry(registry: Arc<registry::ApiRegistry>, title_model: Model) -> Self {
        Self {
            done: AtomicBool::new(false),
            name: "auto-session-title".into(),
            registry: Some(registry),
            title_model: Some(title_model),
        }
    }

    /// LLM 生成标题：用 fast model 调一次，返回 ≤50 字符的标题。
    /// 任何错误都静默吃掉，返回 None。
    async fn generate_title_llm(
        registry: &registry::ApiRegistry,
        model: &Model,
        first_user_msg: &str,
    ) -> Option<String> {
        let system_prompt = "You are a title generator. Generate a concise title (max 50 chars, no quotes, no period at end) summarizing the user's request. Reply with ONLY the title, nothing else.";
        let context = Context::new(
            Some(system_prompt.into()),
            vec![Message::User(UserMessage {
                role: "user".into(),
                content: vec![ContentBlock::Text(TextContent {
                    text: first_user_msg.chars().take(500).collect(),
                    text_signature: None,
                })],
                timestamp: 0,
                source: MessageSource::Prompt,
            })],
        );

        match registry::complete(registry, model, &context, None).await {
            Ok(assistant) => {
                let text = assistant
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        AssistantContentBlock::Text(t) => Some(t.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                let title = text.trim().trim_matches('"').trim();
                if title.is_empty() || title.len() > 100 {
                    None
                } else {
                    Some(title.to_string())
                }
            }
            Err(e) => {
                tracing::warn!("[auto-session-title] LLM call failed (silently swallowed): {e}");
                None
            }
        }
    }

    /// 启发式 fallback（不调 LLM）
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

    /// 写入 session.jsonl custom entry + 更新索引
    fn write_title(
        &self,
        title: &str,
        session_id: &str,
        session_cwd: &str,
    ) {
        let entry = serde_json::json!({
            "type": "session_name",
            "name": title,
            "session_id": session_id,
            "timestamp": crate::session_jsonl::timestamp_iso(),
        });
        crate::session_jsonl::append_raw_entry(session_cwd, &entry);
        tracing::info!("[auto-session-title] wrote session_name entry: \"{title}\"");

        crate::session_index::SessionIndex::set_name(session_id, title);

        // 兼容旧格式
        let titles_path = crate::paths::root().join("agent").join("session-titles.json");
        let mut titles: std::collections::HashMap<String, String> =
            std::fs::read_to_string(&titles_path)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default();
        titles.insert(session_id.to_string(), title.to_string());
        if let Ok(json) = serde_json::to_string_pretty(&titles) {
            let _ = std::fs::write(&titles_path, json);
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

        // 找第一条 user message
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
            // ★ 用 fast model 生成标题（错误静默吃掉）
            let title = if let (Some(reg), Some(model)) = (&self.registry, &self.title_model) {
                match Self::generate_title_llm(reg, model, &text).await {
                    Some(t) => t,
                    None => Self::generate_title_heuristic(&text),
                }
            } else {
                Self::generate_title_heuristic(&text)
            };

            tracing::info!("[auto-session-title] generated: \"{title}\"");

            // 写入 session.jsonl + 索引
            if let (Some(cwd), Some(sid)) = (ctx.session_cwd.as_ref(), ctx.session_id.as_ref()) {
                self.write_title(&title, sid, cwd);
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
