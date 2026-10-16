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
    serde_json::from_value(data.clone())
        .or_else(|_| serde_json::from_value(response.clone()))
        .unwrap_or_default()
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

/// Serializable task row inside a `run.get` snapshot.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct TaskRow {
    #[serde(alias = "taskId")]
    pub task_id: String,
    #[serde(alias = "logicalKey")]
    pub logical_key: String,
    pub kind: String,
    pub state: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub version: i64,
}

/// Extract task rows from a `run.get` envelope.
pub fn parse_tasks(response: &serde_json::Value) -> Vec<TaskRow> {
    response
        .get("data")
        .and_then(|d| d.get("tasks"))
        .and_then(|t| t.as_array())
        .map(|arr| serde_json::from_value(arr.clone().into()).unwrap_or_default())
        .unwrap_or_default()
}

/// A task collapsed-list row: expanded only when running unless toggled.
#[derive(Clone, Debug)]
pub struct TaskLine {
    pub task_id: String,
    pub logical_key: String,
    pub kind: String,
    pub state: String,
    pub expanded: bool,
}

pub fn task_lines(
    tasks: &[TaskRow],
    expanded: &std::collections::HashSet<String>,
) -> Vec<TaskLine> {
    tasks
        .iter()
        .map(|t| {
            let running = t.state == "running" || t.state == "claimed" || t.state == "executing";
            let default_open = running;
            let expanded = if default_open {
                !expanded.contains(&format!("-{}", t.task_id))
            } else {
                expanded.contains(&t.task_id)
            };
            TaskLine {
                task_id: t.task_id.clone(),
                logical_key: t.logical_key.clone(),
                kind: t.kind.clone(),
                state: t.state.clone(),
                expanded,
            }
        })
        .collect()
}

pub fn render_task_lines(lines: &[TaskLine]) -> Vec<String> {
    lines
        .iter()
        .map(|t| {
            let arrow = if t.expanded { "v" } else { ">" };
            if t.expanded {
                format!(
                    "{arrow} [{}] {} {} state={} (expanded)",
                    t.kind, t.task_id, t.logical_key, t.state
                )
            } else {
                format!(
                    "{arrow} [{}] {} {} state={}",
                    t.kind, t.task_id, t.logical_key, t.state
                )
            }
        })
        .collect()
}

#[cfg(test)]
mod task_tests {
    use super::*;

    fn row(id: &str, state: &str) -> TaskRow {
        TaskRow {
            task_id: id.into(),
            logical_key: format!("{id}:0"),
            kind: "tool".into(),
            state: state.into(),
            version: 1,
        }
    }

    #[test]
    fn parse_tasks_reads_run_get_snapshot() {
        let resp = serde_json::json!({
            "data": {"tasks": [
                {"taskId": "t1", "logicalKey": "k1", "kind": "tool", "state": "running", "version": 2},
                {"taskId": "t2", "logicalKey": "k2", "kind": "tool", "state": "succeeded", "version": 3}
            ]}
        });
        let tasks = parse_tasks(&resp);
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].task_id, "t1");
        assert_eq!(tasks[1].state, "succeeded");
    }

    #[test]
    fn running_default_expanded_others_collapsed_and_toggle_respects_state() {
        let tasks = vec![row("t-running", "running"), row("t-done", "succeeded")];
        let mut expanded = std::collections::HashSet::new();
        let lines = task_lines(&tasks, &expanded);
        assert!(lines[0].expanded, "running should default expanded");
        assert!(!lines[1].expanded, "succeeded should default collapsed");

        // Collapse the running one explicitly.
        expanded.insert("-t-running".into());
        let lines = task_lines(&tasks, &expanded);
        assert!(!lines[0].expanded);

        // Expand the done one explicitly.
        expanded.remove("-t-running");
        expanded.insert("t-done".into());
        let lines = task_lines(&tasks, &expanded);
        assert!(lines[0].expanded);
        assert!(lines[1].expanded);
    }

    #[test]
    fn render_task_lines_marks_expansion() {
        let tasks = vec![row("t1", "running"), row("t2", "succeeded")];
        let lines = task_lines(&tasks, &Default::default());
        let rendered = render_task_lines(&lines);
        assert!(rendered[0].starts_with("v "));
        assert!(rendered[0].contains("(expanded)"));
        assert!(rendered[1].starts_with("> "));
        assert!(!rendered[1].contains("(expanded)"));
    }
}
