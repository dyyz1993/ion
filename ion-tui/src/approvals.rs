use serde::Deserialize;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct ApprovalCard {
    pub id: String,
    #[serde(default)]
    pub kind: String,
    #[serde(rename = "sessionId", alias = "session_id", default)]
    pub session_id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub payload: serde_json::Value,
}

pub fn parse_approvals(response: &serde_json::Value) -> Vec<ApprovalCard> {
    response
        .get("data")
        .and_then(|d| d.get("pending").or_else(|| d.get("requests")))
        .and_then(|v| v.as_array())
        .map(|arr| {
            serde_json::from_value::<Vec<ApprovalCard>>(arr.clone().into()).unwrap_or_default()
        })
        .unwrap_or_default()
}

pub fn sort_for_session(cards: &mut [ApprovalCard], session_id: Option<&str>) {
    cards.sort_by(|a, b| {
        let am = a.session_id == session_id.unwrap_or_default();
        let bm = b.session_id == session_id.unwrap_or_default();
        bm.cmp(&am).then_with(|| a.id.cmp(&b.id))
    });
}

pub fn detail_lines(card: &ApprovalCard) -> Vec<String> {
    let mut lines = vec![format!("⏸ 审批请求 — [{}]", card.kind)];
    if let Some(title) = card.title.as_deref().filter(|s| !s.is_empty()) {
        lines.push(format!("title: {title}"));
    }
    if let Some(summary) = card.summary.as_deref().filter(|s| !s.is_empty()) {
        lines.push(format!("summary: {summary}"));
    }
    if let Some(obj) = card.payload.as_object() {
        for key in ["command", "path", "request", "tool", "cwd", "workerId"] {
            if let Some(v) = obj.get(key)
                && let Some(s) = v.as_str()
            {
                lines.push(format!("{key}: {s}"));
            }
        }
    }
    lines.push("✓ 允许 (y)   ✗ 拒绝 (n)".into());
    lines
}

pub fn render_approvals(cards: &[ApprovalCard], cursor: Option<usize>) -> Vec<String> {
    let mut out = Vec::new();
    for (i, card) in cards.iter().enumerate() {
        let marker = if cursor == Some(i) { ">" } else { " " };
        for (line_no, line) in detail_lines(card).into_iter().enumerate() {
            if line_no == 0 {
                out.push(format!("{marker} {line}"));
            } else {
                out.push(format!("  {line}"));
            }
        }
        if i + 1 < cards.len() {
            out.push(String::new());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(id: &str, sid: &str, kind: &str) -> ApprovalCard {
        ApprovalCard {
            id: id.into(),
            kind: kind.into(),
            session_id: sid.into(),
            title: None,
            summary: None,
            payload: serde_json::json!({"command":"./deploy.sh"}),
        }
    }

    #[test]
    fn parses_pending_alias_and_fields() {
        let resp = serde_json::json!({"data":{"pending":[
            {"id":"apr_1","kind":"ui_ask","sessionId":"s1","summary":"deploy?","payload":{"command":"./deploy.sh"}}
        ]}});
        let cards = parse_approvals(&resp);
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].session_id, "s1");
        assert_eq!(cards[0].summary.as_deref(), Some("deploy?"));
    }

    #[test]
    fn tolerates_missing_and_empty_data() {
        assert!(parse_approvals(&serde_json::json!({"data":{}})).is_empty());
        assert!(parse_approvals(&serde_json::json!({})).is_empty());
        let bad = serde_json::json!({"data":{"pending":[{}]}});
        // id is required and malformed entries are dropped fail-closed.
        assert!(parse_approvals(&bad).is_empty());
    }

    #[test]
    fn current_session_cards_sort_first_then_id() {
        let mut cards = vec![
            card("b", "other", "ui_ask"),
            card("z", "s1", "ui_ask"),
            card("a", "other", "ui_ask"),
        ];
        sort_for_session(&mut cards, Some("s1"));
        assert_eq!(cards[0].id, "z");
        assert_eq!(cards[1].id, "a");
        assert_eq!(cards[2].id, "b");
    }

    #[test]
    fn detail_lines_include_actions_and_payload_code_fields() {
        let lines = detail_lines(&card("a", "s", "file_snapshot"));
        assert!(lines[0].contains("审批请求"));
        assert!(lines.iter().any(|l| l.contains("command: ./deploy.sh")));
        assert!(
            lines
                .iter()
                .any(|l| l.contains("允许 (y)") && l.contains("拒绝 (n)"))
        );
    }

    #[test]
    fn render_marks_cursor_card() {
        let cards = vec![card("a", "s", "ui_ask"), card("b", "s", "ui_ask")];
        let lines = render_approvals(&cards, Some(1));
        assert!(lines[0].starts_with("  ⏸"));
        let second_start = lines.iter().position(|l| l.starts_with("> ⏸")).unwrap();
        assert!(lines[second_start..].iter().any(|l| l.contains("command:")));
    }
}
