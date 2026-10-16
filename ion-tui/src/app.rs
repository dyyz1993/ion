use crate::approvals::{ApprovalCard, parse_approvals, sort_for_session};
use crate::connection::Connection;
use crate::conversation::{message_lines, parse_messages};
use crate::input::Action;
use crate::sessions::{TreeRow, build_tree, parse_busy_ids, parse_sessions};
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
    pub session_rows: Vec<TreeRow>,
    pub session_cursor: Option<usize>,
    pub conversation_session: Option<String>,
    pub conversation_lines: Vec<crate::conversation::MsgLine>,
    pub composer: crate::conversation::Composer,
    pub expanded_turns: std::collections::HashSet<usize>,
    pub turn_cursor: Option<usize>,
    pub approvals: Vec<ApprovalCard>,
    pub approval_cursor: Option<usize>,
    pub selected_busy: bool,
    pub attachment_picker: crate::conversation::AttachmentPicker,
    pub live_tool: Option<crate::conversation::LiveToolCard>,
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
            session_rows: Vec::new(),
            session_cursor: None,
            conversation_session: None,
            conversation_lines: Vec::new(),
            composer: Default::default(),
            expanded_turns: Default::default(),
            turn_cursor: None,
            approvals: Vec::new(),
            approval_cursor: None,
            selected_busy: false,
            attachment_picker: Default::default(),
            live_tool: None,
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

    /// Refresh session tree from two RPCs and rebuild rows. Each call uses a
    /// short-lived connection because the host aggressively closes idle write
    /// halves; a single in-place retry on a fresh connection is reliable.
    pub async fn refresh_sessions(&mut self, socket: &std::path::Path) -> Result<(), String> {
        async fn fetch(
            socket: &std::path::Path,
            method: &str,
        ) -> Result<serde_json::Value, String> {
            let mut conn = Connection::connect(socket).await?;
            let out = match conn.rpc(method, serde_json::json!({}), 5000).await {
                Ok(v) => v,
                Err(e) if e.contains("writer closed") => {
                    conn = Connection::connect(socket).await?;
                    conn.rpc(method, serde_json::json!({}), 5000).await?
                }
                Err(e) => return Err(e),
            };
            drop(conn);
            Ok(out)
        }
        let all = fetch(socket, "list_all_sessions").await?;
        let live = fetch(socket, "list_sessions").await?;
        let sessions = parse_sessions(&all);
        let busy = parse_busy_ids(&live);
        let durable: std::collections::HashSet<String> = self
            .runs
            .iter()
            .filter(|r| !matches!(r.state.as_str(), "succeeded" | "failed" | "cancelled"))
            .map(|r| r.session_id.clone())
            .collect();
        self.session_rows = build_tree(&sessions, &busy, &durable);
        if std::env::var("ION_TUI_DEBUG")
            .map(|v| v == "1")
            .unwrap_or(false)
        {
            eprintln!(
                "[tui-debug] refresh_sessions: {} sessions -> {} rows (busy={})",
                sessions.len(),
                self.session_rows.len(),
                busy.len()
            );
        }
        self.sync_composer_from_selected();
        if self.session_cursor.is_none() && !self.session_rows.is_empty() {
            // First selectable (non-group) row.
            self.session_cursor = self
                .session_rows
                .iter()
                .position(|r| !r.session_id.starts_with("group:"));
        }
        Ok(())
    }

    pub fn session_cursor_next(&mut self) {
        let selectable: Vec<usize> = self
            .session_rows
            .iter()
            .enumerate()
            .filter(|(_, r)| !r.session_id.starts_with("group:"))
            .map(|(i, _)| i)
            .collect();
        if selectable.is_empty() {
            return;
        }
        let next = match self.session_cursor {
            None => selectable[0],
            Some(cur) => {
                let pos = selectable
                    .iter()
                    .position(|&i| i >= cur)
                    .map(|p| p + 1)
                    .unwrap_or(0);
                selectable[pos.min(selectable.len() - 1)]
            }
        };
        self.session_cursor = Some(next);
    }

    pub fn session_cursor_prev(&mut self) {
        let selectable: Vec<usize> = self
            .session_rows
            .iter()
            .enumerate()
            .filter(|(_, r)| !r.session_id.starts_with("group:"))
            .map(|(i, _)| i)
            .collect();
        if selectable.is_empty() {
            return;
        }
        let prev = match self.session_cursor {
            None => selectable[selectable.len() - 1],
            Some(cur) => {
                let pos = selectable.iter().position(|&i| i >= cur).unwrap_or(1);
                selectable[pos.saturating_sub(1)]
            }
        };
        self.session_cursor = Some(prev);
    }

    pub fn selected_session_id(&self) -> Option<&str> {
        self.session_cursor
            .and_then(|i| self.session_rows.get(i))
            .filter(|r| !r.session_id.starts_with("group:"))
            .map(|r| r.session_id.as_str())
    }

    /// Update composer metadata from the selected session card.
    async fn rpc_with_retry(
        socket: &std::path::Path,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        async fn rpc(
            socket: &std::path::Path,
            method: &str,
            params: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            let mut conn = Connection::connect(socket).await?;
            match conn.rpc(method, params.clone(), 10_000).await {
                Ok(v) => Ok(v),
                Err(e) if e.contains("writer closed") => {
                    conn = Connection::connect(socket).await?;
                    conn.rpc(method, params, 10_000).await
                }
                Err(e) => Err(e),
            }
        }
        rpc(socket, method, params.clone()).await
    }

    pub async fn refresh_approvals(&mut self, socket: &std::path::Path) -> Result<(), String> {
        let resp = Self::rpc_with_retry(socket, "approvals_pending", serde_json::json!({})).await?;
        let mut cards = parse_approvals(&resp);
        sort_for_session(&mut cards, self.conversation_session.as_deref());
        self.approvals = cards;
        if self.approvals.is_empty() {
            self.approval_cursor = None;
        } else if self
            .approval_cursor
            .is_none_or(|i| i >= self.approvals.len())
        {
            self.approval_cursor = Some(0);
        }
        Ok(())
    }

    pub fn approval_next(&mut self) {
        if self.approvals.is_empty() {
            return;
        }
        let next = match self.approval_cursor {
            None => 0,
            Some(i) => (i + 1).min(self.approvals.len() - 1),
        };
        self.approval_cursor = Some(next);
    }

    pub fn approval_prev(&mut self) {
        if self.approvals.is_empty() {
            return;
        }
        let prev = match self.approval_cursor {
            None => self.approvals.len() - 1,
            Some(i) => i.saturating_sub(1),
        };
        self.approval_cursor = Some(prev);
    }

    pub fn selected_approval_id(&self) -> Option<String> {
        self.approval_cursor
            .and_then(|i| self.approvals.get(i))
            .map(|a| a.id.clone())
    }

    pub async fn respond_approval(
        &mut self,
        socket: &std::path::Path,
        approve: bool,
    ) -> Result<(), String> {
        let Some(id) = self.selected_approval_id() else {
            return Err("no approval selected".into());
        };
        Self::rpc_with_retry(
            socket,
            "approval_respond",
            serde_json::json!({
                "id": id,
                "decision": if approve {"approve"} else {"reject"}
            }),
        )
        .await?;
        self.refresh_approvals(socket).await?;
        if let Some(sid) = self.conversation_session.clone() {
            let resp = Self::rpc_with_retry(
                socket,
                "get_session_messages",
                serde_json::json!({"session": sid, "limit":200, "view":"live"}),
            )
            .await?;
            let msgs = parse_messages(&resp);
            self.conversation_lines = message_lines(&msgs, &self.expanded);
        }
        Ok(())
    }

    pub async fn abort_selected(&mut self, socket: &std::path::Path) -> Result<(), String> {
        let Some(session) = self.selected_session_id().map(String::from) else {
            return Err("select a session first".into());
        };
        Self::rpc_with_retry(
            socket,
            "send_to_session",
            serde_json::json!({
                "session": session,
                "rpc_method": "abort",
                "params": {}
            }),
        )
        .await?;
        self.refresh_sessions(socket).await?;
        if let Some(sid) = self.conversation_session.clone() {
            let resp = Self::rpc_with_retry(
                socket,
                "get_session_messages",
                serde_json::json!({"session": sid, "limit":200, "view":"live"}),
            )
            .await?;
            let msgs = parse_messages(&resp);
            self.conversation_lines = message_lines(&msgs, &self.expanded);
        }
        Ok(())
    }

    pub fn sync_composer_from_selected(&mut self) {
        let Some(row) = self
            .session_cursor
            .and_then(|i| self.session_rows.get(i))
            .filter(|r| !r.session_id.starts_with("group:"))
        else {
            return;
        };
        self.composer.model = "glm-5.2".into();
        self.composer.ctx_used = row.ctx_used;
        self.composer.ctx_max = row.ctx_max;
        self.composer.runtime_ms = row.runtime_ms;
        self.selected_busy = row.busy;
        if let Some(task) = self
            .tasks
            .iter()
            .find(|t| matches!(t.state.as_str(), "running" | "claimed" | "executing"))
        {
            self.live_tool = Some(crate::conversation::LiveToolCard {
                name: task.kind.clone(),
                command: task.logical_key.clone(),
                output_tail: vec!["running…".into()],
                elapsed_ms: row.runtime_ms,
            });
        } else {
            self.live_tool = None;
        }
        self.composer.child_count = usize::from(row.busy);
        self.composer.peer_count = usize::from(row.durable_active);
        self.conversation_session = Some(row.session_id.clone());
    }

    pub fn add_attachment(&mut self, name: impl Into<String>) {
        let name = name.into();
        if !self.composer.attachments.contains(&name) {
            self.composer.attachments.push(name);
        }
    }

    pub fn turn_next(&mut self) {
        let count = crate::conversation::group_turns(&self.conversation_lines).len();
        if count == 0 {
            return;
        }
        let next = match self.turn_cursor {
            None => 0,
            Some(i) => (i + 1).min(count - 1),
        };
        self.turn_cursor = Some(next);
        self.expanded_turns.insert(next);
    }

    pub fn turn_prev(&mut self) {
        let count = crate::conversation::group_turns(&self.conversation_lines).len();
        if count == 0 {
            return;
        }
        let prev = match self.turn_cursor {
            None => count - 1,
            Some(i) => i.saturating_sub(1),
        };
        self.turn_cursor = Some(prev);
        self.expanded_turns.insert(prev);
    }

    pub fn open_attachment_picker(&mut self) {
        self.attachment_picker.open();
    }

    pub fn confirm_attachment_picker(&mut self) {
        if let Some(path) = self.attachment_picker.confirm() {
            self.add_attachment(path);
        }
    }

    pub fn close_attachment_picker(&mut self) {
        self.attachment_picker.close();
    }

    pub fn remove_last_attachment(&mut self) -> Option<String> {
        self.composer.attachments.pop()
    }

    /// Send the composer draft to the selected session via prompt RPC.
    pub async fn send_draft(&mut self, socket: &std::path::Path) -> Result<(), String> {
        async fn rpc_with_retry(
            socket: &std::path::Path,
            method: &str,
            params: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            let mut conn = Connection::connect(socket).await?;
            match conn.rpc(method, params.clone(), 10_000).await {
                Ok(v) => Ok(v),
                Err(e) if e.contains("writer closed") => {
                    conn = Connection::connect(socket).await?;
                    conn.rpc(method, params, 10_000).await
                }
                Err(e) => Err(e),
            }
        }
        let Some(session_id) = self.conversation_session.clone() else {
            return Err("select a session first".into());
        };
        let text = self.composer.draft.trim().to_string();
        if text.is_empty() {
            return Err("composer is empty".into());
        }
        rpc_with_retry(
            socket,
            "prompt",
            serde_json::json!({
                "session": session_id,
                "text": text,
                "attachments": self.composer.attachments,
            }),
        )
        .await?;
        self.composer.draft.clear();
        self.composer.cursor = 0;
        self.composer.attachments.clear();
        // Reload conversation readback so the sent turn appears without optimistic state.
        let response = rpc_with_retry(
            socket,
            "get_session_messages",
            serde_json::json!({"session": session_id, "limit": 200, "view": "live"}),
        )
        .await?;
        let msgs = parse_messages(&response);
        self.conversation_lines = message_lines(&msgs, &self.expanded);
        self.conversation_session = Some(session_id);
        Ok(())
    }

    /// Load conversation for a session (get_session_messages).
    pub async fn load_conversation(
        &mut self,
        conn: &mut Connection,
        session_id: &str,
    ) -> Result<(), String> {
        if self.conversation_session.as_deref() == Some(session_id) {
            return Ok(());
        }
        let resp = conn
            .rpc(
                "get_session_messages",
                serde_json::json!({"session": session_id, "limit": 200, "view": "live"}),
                5000,
            )
            .await?;
        let msgs = parse_messages(&resp);
        self.conversation_lines = message_lines(&msgs, &self.expanded);
        self.conversation_session = Some(session_id.to_string());
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

    pub fn render_sessions_text(&self) -> Vec<String> {
        let mut lines = vec!["--- sessions ---".into()];
        if self.session_rows.is_empty() {
            lines.push("(no sessions)".into());
            return lines;
        }
        for (i, r) in self.session_rows.iter().enumerate() {
            if r.session_id.starts_with("group:") {
                lines.push(r.label.to_string());
                continue;
            }
            let cursor = if self.session_cursor == Some(i) {
                "> "
            } else {
                "  "
            };
            let pad = "  ".repeat(r.indent);
            let mut line = format!("{cursor}{pad}{}", r.label);
            if r.busy {
                line.push_str(" ●");
            }
            if r.durable_active {
                line.push_str(" ◐");
            }
            lines.push(line);
        }
        lines
    }

    pub fn render_conversation_text(&self) -> Vec<String> {
        let mut lines = crate::approvals::render_approvals(&self.approvals, self.approval_cursor);
        if !self.approvals.is_empty() {
            lines.push(String::new());
        }
        let conversation = if self.conversation_lines.is_empty() {
            vec!["(no messages)".into()]
        } else {
            let turns = crate::conversation::group_turns(&self.conversation_lines);
            crate::conversation::render_turns(&turns, &self.expanded_turns)
        };
        lines.extend(conversation);
        if let Some(card) = &self.live_tool {
            lines.extend(card.render());
        }
        if self.attachment_picker.open {
            lines.push(format!("attach path: {}▊", self.attachment_picker.path));
        }
        let attachments = if self.composer.attachments.is_empty() {
            "(none)".to_string()
        } else {
            self.composer.attachments.join(" · ")
        };
        lines.push(format!("📎 {attachments}"));
        let input_label = if self.approvals.is_empty() {
            format!("input: {}▊", self.composer.draft)
        } else {
            "input: 等待审批决策…".to_string()
        };
        lines.push(input_label);
        let p = self.composer.ctx_percent();
        lines.push(format!(
            "{} · ctx {} {}% ({}/{}) · {}ms · 子{} · 同{}",
            self.composer.model,
            self.composer.ctx_bar(10),
            p,
            self.composer.ctx_used,
            self.composer.ctx_max,
            self.composer.runtime_ms,
            self.composer.child_count,
            self.composer.peer_count
        ));
        if self.selected_busy {
            lines.push("⏹ 停止 (Esc/s)".into());
        }
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
