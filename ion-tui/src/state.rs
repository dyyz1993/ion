/// Serializable view of one row in `run.list` output (snake_case host shape).
#[derive(Clone, Debug, serde::Deserialize)]
pub struct RunListEntry {
    #[serde(alias = "runId")]
    pub run_id: String,
    #[serde(alias = "sessionId")]
    pub session_id: String,
    pub state: String,
    #[serde(default)]
    pub version: i64,
    #[serde(default, alias = "updatedMs")]
    #[allow(dead_code)]
    pub updated_ms: i64,
}

/// Parse the `run.list` host response envelope into display rows.
pub fn parse_run_list(response: &serde_json::Value) -> Vec<RunListEntry> {
    let data = response.get("data").unwrap_or(&serde_json::Value::Null);
    serde_json::from_value(data.clone()).unwrap_or_default()
}

/// Extract state from a `run.get` envelope (used by tests + headless smoke).
#[allow(dead_code)]
pub fn run_state(response: &serde_json::Value) -> Option<String> {
    response
        .get("data")
        .and_then(|d| d.get("state"))
        .and_then(|s| s.as_str())
        .map(String::from)
}

/// Redraw what the TUI would show (headless mode dumps this to stdout).
pub fn render_lines(entries: &[RunListEntry]) -> Vec<String> {
    if entries.is_empty() {
        return vec!["(no durable runs)".into()];
    }
    entries
        .iter()
        .map(|r| {
            format!(
                "{:<20} {:<36} {:<12} v{}",
                r.run_id, r.session_id, r.state, r.version
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_run_list_extracts_rows() {
        let resp: serde_json::Value = serde_json::json!({
            "type": "response", "id": "rpc-client", "success": true,
            "data": [
                {"runId": "run-a", "sessionId": "sess-a", "state": "running", "version": 3, "updatedMs": 1},
                {"runId": "run-b", "sessionId": "sess-b", "state": "succeeded", "version": 0, "updatedMs": 2}
            ]
        });
        let rows = parse_run_list(&resp);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].run_id, "run-a");
        assert_eq!(rows[0].state, "running");
        assert_eq!(rows[1].version, 0);
    }

    #[test]
    fn parse_run_list_tolerates_missing_data() {
        let resp = serde_json::json!({"type":"response","success":false,"error":"not found"});
        assert!(parse_run_list(&resp).is_empty());
    }

    #[test]
    fn render_lines_empty_and_populated() {
        assert_eq!(render_lines(&[]), vec!["(no durable runs)".to_string()]);
        let row = RunListEntry {
            run_id: "run-x".into(),
            session_id: "sess-x".into(),
            state: "queued".into(),
            version: 5,
            updated_ms: 0,
        };
        let lines = render_lines(&[row]);
        assert!(lines[0].starts_with("run-x"));
        assert!(lines[0].contains("queued"));
        assert!(lines[0].contains("v5"));
    }

    #[test]
    fn run_state_reads_nested_data() {
        let resp = serde_json::json!({"data": {"state": "paused"}});
        assert_eq!(run_state(&resp).as_deref(), Some("paused"));
    }
}

/// One polled event from `events.subscribe` (already unwrapped from the page).
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct EventLine {
    pub seq: i64,
    pub kind: String,
}

#[allow(dead_code)]
pub fn parse_event_page(response: &serde_json::Value) -> (i64, Vec<EventLine>) {
    let data = response
        .get("data")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let cursor_seq = data
        .get("cursor")
        .and_then(|c| c.get("seq"))
        .and_then(|s| s.as_i64())
        .unwrap_or(0);
    let events = data
        .get("events")
        .and_then(|e| e.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|ev| {
                    let kind = ev.get("kind").and_then(|k| k.as_str())?;
                    let seq = ev.get("seq").and_then(|s| s.as_i64()).unwrap_or(cursor_seq);
                    Some(EventLine {
                        seq,
                        kind: kind.into(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    (cursor_seq, events)
}

#[allow(dead_code)]
pub fn render_events(lines: &[EventLine], max: usize) -> Vec<String> {
    lines
        .iter()
        .rev()
        .take(max)
        .rev()
        .map(|e| format!("seq={:>3} {}", e.seq, e.kind))
        .collect()
}

#[cfg(test)]
mod event_tests {
    use super::*;

    #[test]
    fn parse_event_page_extracts_cursor_and_kinds() {
        let resp = serde_json::json!({
            "data": {
                "cursor": {"seq": 3, "runId": "r1", "storeId": "s1"},
                "events": [
                    {"kind": "run.created"},
                    {"kind": "run.paused"},
                    {"kind": "run.resumed"}
                ]
            }
        });
        let (cursor, events) = parse_event_page(&resp);
        assert_eq!(cursor, 3);
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].seq, 3); // payload seq wins over inferred
        assert_eq!(events[0].kind, "run.created");
        assert_eq!(events[2].kind, "run.resumed");
    }

    #[test]
    fn parse_event_page_tolerates_empty() {
        let resp = serde_json::json!({"data": {"cursor": {"seq": 0}, "events": []}});
        let (cursor, events) = parse_event_page(&resp);
        assert_eq!(cursor, 0);
        assert!(events.is_empty());
    }

    #[test]
    fn render_events_keeps_latest_tail() {
        let lines: Vec<EventLine> = (1..=5)
            .map(|i| EventLine {
                seq: i,
                kind: format!("ev{i}"),
            })
            .collect();
        let rendered = render_events(&lines, 3);
        assert_eq!(rendered.len(), 3);
        assert!(rendered[0].contains("ev3"));
        assert!(rendered[2].contains("ev5"));
    }
}
