use crate::connection::Connection;
use crate::input::Action;
use crate::state::{
    EventLine, RunListEntry, TaskRow, parse_event_page, parse_run_list, parse_tasks, render_events,
    render_lines, render_task_lines, task_lines,
};

/// Headless-ish application core shared by the TUI loop and CI smoke mode.
pub struct App {
    pub runs: Vec<RunListEntry>,
    pub selected: Option<usize>,
    pub status: String,
    pub events: Vec<EventLine>,
    pub tasks: Vec<TaskRow>,
    pub detail_run_id: Option<String>,
    pub expanded: std::collections::HashSet<String>,
    #[allow(dead_code)]
    pub cursor: i64,
    #[allow(dead_code)]
    pub event_run_id: Option<String>,
}

impl App {
    pub fn new() -> Self {
        Self {
            runs: Vec::new(),
            selected: None,
            status: "ready".into(),
            events: Vec::new(),
            tasks: Vec::new(),
            detail_run_id: None,
            expanded: Default::default(),
            cursor: 0,
            event_run_id: None,
        }
    }

    pub async fn refresh(&mut self, conn: &mut Connection) -> Result<(), String> {
        let resp = conn.rpc("run.list", serde_json::json!({}), 5000).await?;
        self.runs = parse_run_list(&resp);
        if self.selected.take().is_some() {
            self.selected = if self.runs.is_empty() { None } else { Some(0) };
        }
        self.status = format!("{} runs", self.runs.len());
        Ok(())
    }

    pub fn selected_run_id(&self) -> Option<&str> {
        self.selected
            .and_then(|i| self.runs.get(i))
            .map(|r| r.run_id.as_str())
    }

    pub fn select_next(&mut self) {
        if self.runs.is_empty() {
            return;
        }
        let next = match self.selected {
            None => 0,
            Some(i) => (i + 1).min(self.runs.len() - 1),
        };
        self.selected = Some(next);
    }

    pub fn select_prev(&mut self) {
        if self.runs.is_empty() {
            return;
        }
        let next = match self.selected {
            None => 0,
            Some(i) => i.saturating_sub(1),
        };
        self.selected = Some(next);
    }

    /// Dispatch an action and confirm via run.get readback.
    pub async fn dispatch(
        &mut self,
        conn: &mut Connection,
        action: Action,
    ) -> Result<bool, String> {
        match action {
            Action::Quit => Ok(false),
            Action::Pause { run_id, reason } => {
                conn.rpc(
                    "run.pause",
                    serde_json::json!({"runId": run_id, "reason": reason}),
                    5000,
                )
                .await?;
                self.readback(conn, &run_id).await
            }
            Action::Resume { run_id } => {
                conn.rpc("run.resume", serde_json::json!({"runId": run_id}), 5000)
                    .await?;
                self.readback(conn, &run_id).await
            }
            Action::ToggleAutoResume { run_id, enabled } => {
                conn.rpc(
                    "run.auto_resume",
                    serde_json::json!({"runId": run_id, "enabled": enabled}),
                    5000,
                )
                .await?;
                self.readback(conn, &run_id).await
            }
        }
    }

    /// Refresh the list after a write so headless mode can assert state.
    async fn readback(&mut self, conn: &mut Connection, run_id: &str) -> Result<bool, String> {
        self.refresh(conn).await?;
        let state = self
            .runs
            .iter()
            .find(|r| r.run_id == run_id)
            .map(|r| r.state.clone());
        self.status = match state {
            Some(s) => format!("{run_id} -> {s}"),
            None => format!("{run_id} missing after write"),
        };
        Ok(true)
    }

    /// Poll one page of events for the selected run and advance the cursor.
    #[allow(dead_code)]
    pub async fn poll_events(&mut self, conn: &mut Connection) -> Result<(), String> {
        let Some(run_id) = self.selected_run_id().map(String::from) else {
            return Ok(());
        };
        if self.event_run_id.as_deref() != Some(run_id.as_str()) {
            self.cursor = 0;
            self.events.clear();
            self.event_run_id = Some(run_id.clone());
        }
        let resp = conn
            .rpc(
                "events.page",
                serde_json::json!({ "runId": run_id, "afterCursor": { "seq": self.cursor } }),
                5000,
            )
            .await?;
        let (new_cursor, events) = parse_event_page(&resp);
        if new_cursor > self.cursor {
            self.cursor = new_cursor;
            self.events.extend(events);
            let keep = self.events.len().saturating_sub(1000);
            if keep > 0 {
                self.events.drain(..keep);
            }
        }
        Ok(())
    }

    /// Load task detail for the selected run (run.get) when selection changes.
    pub async fn load_detail(&mut self, conn: &mut Connection) -> Result<(), String> {
        let Some(run_id) = self.selected_run_id().map(String::from) else {
            self.tasks.clear();
            self.detail_run_id = None;
            return Ok(());
        };
        if self.detail_run_id.as_deref() == Some(run_id.as_str()) {
            return Ok(());
        }
        let resp = conn
            .rpc("run.get", serde_json::json!({"runId": run_id}), 5000)
            .await?;
        self.tasks = parse_tasks(&resp);
        self.detail_run_id = Some(run_id);
        Ok(())
    }

    pub fn toggle_task(&mut self, task_id: &str) {
        if self.expanded.contains(task_id) {
            self.expanded.remove(task_id);
        } else {
            self.expanded.insert(task_id.to_string());
        }
    }

    /// Whether a task is "active" for collapse defaults.
    #[allow(dead_code)]
    pub fn task_active(&self, task_id: &str) -> bool {
        self.tasks.iter().any(|t| {
            t.task_id == task_id
                && (t.state == "running" || t.state == "claimed" || t.state == "executing")
        })
    }

    pub fn render_task_detail(&self) -> Vec<String> {
        if self.tasks.is_empty() {
            return Vec::new();
        }
        let mut lines = vec!["--- tasks ---".into()];
        lines.extend(render_task_lines(&task_lines(&self.tasks, &self.expanded)));
        lines
    }

    pub fn render(&self) -> Vec<String> {
        let mut lines = render_lines(&self.runs);
        lines.push(String::new());
        lines.push(format!("status: {}", self.status));
        if let Some(i) = self.selected
            && let Some(run) = self.runs.get(i)
        {
            lines.push(format!("selected: {}", run.run_id));
        }
        let event_lines = render_events(&self.events, 20);
        if !event_lines.is_empty() {
            lines.push("--- events ---".into());
            lines.extend(event_lines);
        }
        lines.extend(self.render_task_detail());
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(run_id: &str, state: &str) -> RunListEntry {
        RunListEntry {
            run_id: run_id.into(),
            session_id: "s".into(),
            state: state.into(),
            version: 0,
            updated_ms: 0,
        }
    }

    #[test]
    fn selection_clamps_and_renders() {
        let mut app = App::new();
        app.runs = vec![entry("a", "running"), entry("b", "paused")];
        app.select_next();
        assert_eq!(app.selected_run_id(), Some("a"));
        app.select_next();
        assert_eq!(app.selected_run_id(), Some("b"));
        app.select_next();
        assert_eq!(app.selected_run_id(), Some("b"));
        app.select_prev();
        assert_eq!(app.selected_run_id(), Some("a"));
        let lines = app.render();
        assert!(lines.iter().any(|l| l.contains("selected: a")));
        assert!(lines.iter().any(|l| l.starts_with("status: ")));
    }

    #[test]
    fn empty_list_renders_placeholder() {
        let app = App::new();
        assert_eq!(app.render()[0], "(no durable runs)");
        assert_eq!(app.selected_run_id(), None);
    }
}
