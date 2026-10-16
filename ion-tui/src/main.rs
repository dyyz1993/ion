mod app;
mod approvals;
mod connection;
mod conversation;
mod input;
mod sessions;
mod state;
mod stream;
mod ui;

use std::path::PathBuf;

use app::App;
use input::Action;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let socket = args
        .iter()
        .position(|a| a == "--socket")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from)
        .unwrap_or_else(default_socket);
    let headless = std::env::var("ION_TUI_HEADLESS")
        .map(|v| v == "1")
        .unwrap_or(false);
    let initial_attachments: Vec<String> = args
        .iter()
        .enumerate()
        .filter(|(_, a)| *a == "--attach")
        .filter_map(|(i, _)| args.get(i + 1))
        .filter(|v| !v.starts_with("--"))
        .cloned()
        .collect();
    if let Err(e) = run(socket, headless, initial_attachments).await {
        eprintln!("ion-tui error: {e}");
        std::process::exit(1);
    }
}

fn default_socket() -> PathBuf {
    if let Ok(custom) = std::env::var("ION_HOST_SOCKET") {
        return PathBuf::from(custom);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".ion").join("host.sock")
}

async fn run(
    socket: PathBuf,
    headless: bool,
    initial_attachments: Vec<String>,
) -> Result<(), String> {
    let mut conn = connection::Connection::connect(&socket).await?;
    let mut app = App::new();
    for a in initial_attachments {
        app.add_attachment(a);
    }
    app.refresh(&mut conn).await?;
    app.select_next(); // default: first run selected so events poll has a target
    // v0.5: build session tree before first draw; retry up to 3x because the
    // host closes idle write halves between startup RPCs.
    let mut last_err = String::new();
    for _ in 0..5 {
        match app.refresh_sessions(&socket).await {
            Ok(()) => {
                last_err.clear();
                break;
            }
            Err(e) => {
                last_err = e.clone();
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
    if !last_err.is_empty() {
        return Err(last_err);
    }
    if let Err(e) = app.load_detail(&mut conn).await {
        if e.contains("writer closed") {
            conn = connection::Connection::connect(&socket).await?;
            app.load_detail(&mut conn).await?;
        } else {
            return Err(e);
        }
    }
    app.sync_composer_from_selected();
    if let Err(e) = app.poll_events(&mut conn).await {
        if e.contains("writer closed") {
            conn = connection::Connection::connect(&socket).await?;
            app.poll_events(&mut conn).await?;
        } else {
            return Err(e);
        }
    }

    let _ = app.refresh_approvals(&socket).await;
    if headless {
        // Text render: sessions tree + runs + events + tasks (conversation body
        // needs a selected session; print placeholder line for CI assertions).
        for line in app.render_sessions_text() {
            println!("{line}");
        }
        println!("--- conversation ---");
        for line in app.render_conversation_text() {
            println!("{line}");
        }
        for line in app.render() {
            println!("{line}");
        }
        return Ok(());
    }

    // Full-screen ratatui loop with crossterm events.
    let mut terminal = ratatui::init();
    let result = tui_loop(&mut terminal, &mut app, &mut conn, &socket).await;
    ratatui::restore();
    result
}

use futures::{FutureExt, StreamExt};

async fn crossterm_event() -> std::io::Result<crossterm::event::Event> {
    static STREAM: std::sync::LazyLock<tokio::sync::Mutex<crossterm::event::EventStream>> =
        std::sync::LazyLock::new(|| tokio::sync::Mutex::new(Default::default()));
    let mut stream = STREAM.lock().await;
    stream
        .next()
        .await
        .unwrap_or_else(|| Err(std::io::Error::other("event stream ended")))
}

async fn tui_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    conn: &mut connection::Connection,
    socket: &std::path::Path,
) -> Result<(), String> {
    let mut tool_stream = match app.conversation_session.clone() {
        Some(sid) => stream::SessionEventStream::subscribe(socket, &sid)
            .await
            .ok(),
        None => None,
    };
    let mut panels = ui::PanelState::default();
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(1000));
    loop {
        let _ = app.poll_events(conn).await;
        let _ = app.load_detail(conn).await;
        let _ = app.refresh_sessions(socket).await;
        let _ = app.refresh_approvals(socket).await;
        let desired_stream_sid = app.selected_session_id().map(str::to_string);
        let current_stream_sid = app.conversation_session.clone();
        if desired_stream_sid != current_stream_sid
            && let Some(sid) = desired_stream_sid
        {
            tool_stream = stream::SessionEventStream::subscribe(socket, &sid)
                .await
                .ok();
        }
        if let Some(sid) = app.selected_session_id().map(String::from)
            && app.conversation_session.as_deref() != Some(sid.as_str())
            && let Err(e) = app.load_conversation(conn, &sid).await
            && !e.contains("writer closed")
        {
            app.status = e;
        }
        terminal
            .draw(|f| ui::draw(f, app, &panels))
            .map_err(|e| format!("draw: {e}"))?;
        tokio::select! {
            _ = tick.tick() => {}
            event = async {
                match tool_stream.as_mut() {
                    Some(stream) => stream.next_event().await,
                    None => None,
                }
            } => {
                match event {
                    Some(stream::StreamEvent::Tool(event)) => app.apply_tool_event(event),
                    Some(stream::StreamEvent::ApprovalChanged) => {
                        let _ = app.refresh_approvals(socket).await;
                    }
                    None => {}
                }
            }
            event = crossterm_event().fuse() => {
                let event = event.map_err(|e| format!("read: {e}"))?;
                match event {
                    crossterm::event::Event::Key(key) if key.kind == crossterm::event::KeyEventKind::Press => {
                        use crossterm::event::KeyCode;
                        match key.code {
                            KeyCode::Char('q') => break,
                            KeyCode::Esc if !(panels.focus == ui::Focus::Conversation
                                && app.attachment_picker.open) =>
                            {
                                if panels.focus == ui::Focus::Conversation {
                                    if let Err(e) = app.abort_selected(socket).await {
                                        app.status = e;
                                    }
                                } else {
                                    break;
                                }
                            }
                            KeyCode::Tab => panels.toggle_focus(),
                            KeyCode::Char('1') => panels.focus = ui::Focus::Sessions,
                            KeyCode::Char('2') => panels.focus = ui::Focus::Conversation,
                            KeyCode::Char('3') => panels.focus = ui::Focus::Runs,
                            KeyCode::Char('4') => panels.focus = ui::Focus::Events,
                            KeyCode::Char('j') | KeyCode::Down => match panels.focus {
                                ui::Focus::Sessions => app.session_cursor_next(),
                                ui::Focus::Runs => app.select_next(),
                                ui::Focus::Conversation => {
                                    if !app.approvals.is_empty() {
                                        app.approval_next();
                                    } else {
                                        app.turn_next();
                                    }
                                }
                                _ => {}
                            },
                            KeyCode::Char('k') | KeyCode::Up => match panels.focus {
                                ui::Focus::Sessions => app.session_cursor_prev(),
                                ui::Focus::Runs => app.select_prev(),
                                ui::Focus::Conversation => {
                                    if !app.approvals.is_empty() {
                                        app.approval_prev();
                                    } else {
                                        app.turn_prev();
                                    }
                                }
                                _ => {}
                            },
                            KeyCode::Enter if panels.focus == ui::Focus::Conversation
                                && app.attachment_picker.open =>
                            {
                                app.confirm_attachment_picker();
                            }
                            KeyCode::Esc if panels.focus == ui::Focus::Conversation
                                && app.attachment_picker.open =>
                            {
                                app.close_attachment_picker();
                            }
                            KeyCode::Backspace if panels.focus == ui::Focus::Conversation
                                && app.attachment_picker.open =>
                            {
                                app.attachment_picker.backspace();
                            }
                            KeyCode::Enter => match panels.focus {
                                ui::Focus::Sessions => {
                                    if let Some(sid) =
                                        app.selected_session_id().map(String::from)
                                        && let Err(e) = app.load_conversation(conn, &sid).await
                                        && !e.contains("writer closed")
                                    {
                                        app.status = e;
                                    }
                                }
                                ui::Focus::Conversation => {
                                    let result = app.send_draft(socket).await;
                                    let result = match result {
                                        Err(e) if e.contains("writer closed") => {
                                            *conn =
                                                connection::Connection::connect(socket).await?;
                                            app.send_draft(socket).await
                                        }
                                        result => result,
                                    };
                                    app.status = match result {
                                        Ok(()) => "prompt sent".into(),
                                        Err(e) => e,
                                    };
                                }
                                _ => {}
                            },
                            KeyCode::Backspace => {
                                if panels.focus == ui::Focus::Conversation {
                                    app.composer.backspace();
                                }
                            }
                            KeyCode::Char('a') if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) => {
                                if panels.focus == ui::Focus::Conversation {
                                    app.open_attachment_picker();
                                }
                            }
                            KeyCode::Char('x') if panels.focus == ui::Focus::Conversation => {
                                if let Some(name) = app.remove_last_attachment() {
                                    app.status = format!("removed attachment: {name}");
                                }
                            }
                            KeyCode::Char('y') if panels.focus == ui::Focus::Conversation => {
                                if let Err(e) = app.respond_approval(socket, true).await {
                                    app.status = e;
                                }
                            }
                            KeyCode::Char('n') if panels.focus == ui::Focus::Conversation => {
                                if let Err(e) = app.respond_approval(socket, false).await {
                                    app.status = e;
                                }
                            }
                            KeyCode::Char('s') if panels.focus == ui::Focus::Conversation => {
                                if let Err(e) = app.abort_selected(socket).await {
                                    app.status = e;
                                }
                            }
                            KeyCode::Char('e') if panels.focus == ui::Focus::Conversation => {
                                // Prefer the just-completed tool card; otherwise toggle history.
                                let turn_count =
                                    crate::conversation::group_turns(&app.conversation_lines).len();
                                if !app.completed_tools.is_empty() {
                                    app.completed_tools_expanded = !app.completed_tools_expanded;
                                } else if turn_count >= 2 {
                                    let idx = turn_count - 2;
                                    if app.expanded_turns.contains(&idx) {
                                        app.expanded_turns.remove(&idx);
                                    } else {
                                        app.expanded_turns.insert(idx);
                                    }
                                }
                            }
                            KeyCode::Char('e') => {
                                // Toggle the first task of the selected run (detail list).
                                if let Some(first) = app.tasks.first() {
                                    let id = first.task_id.clone();
                                    app.toggle_task(&id);
                                }
                            }
                            KeyCode::Char(c)
                                if !matches!(c, 'p' | 'r' | 'a' | 'q' | 'j' | 'k' | 'e' | 'y' | 'n' | 's') =>
                            {
                                if panels.focus == ui::Focus::Conversation {
                                    if app.attachment_picker.open {
                                        app.attachment_picker.insert(c);
                                    } else {
                                        app.composer.insert(c);
                                    }
                                }
                            }
                            KeyCode::Char('p') | KeyCode::Char('r') | KeyCode::Char('a') => {
                                let key_name = match key.code {
                                    KeyCode::Char(c) => c.to_string(),
                                    _ => String::new(),
                                };
                                if let Some(action) =
                                    Action::from_key(&key_name, app.selected_run_id())
                                    && let Err(e) = app.dispatch(conn, action).await
                                {
                                    if e.contains("writer closed") {
                                        *conn =
                                            connection::Connection::connect(socket).await?;
                                        app.refresh(conn).await?;
                                    } else {
                                        app.status = e;
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    crossterm::event::Event::Resize(_, _) => {}
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod prompt_tests {
    use crate::app::App;
    use crate::state::RunListEntry;

    #[tokio::test]
    async fn send_draft_sends_prompt_and_reloads_conversation() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            for _ in 0..2 {
                let (stream, _) = listener.accept().await.unwrap();
                mock_prompt_host(stream).await;
            }
        });
        let mut app = App::new();
        app.runs = vec![RunListEntry {
            run_id: "r".into(),
            session_id: "s".into(),
            state: "running".into(),
            version: 1,
            updated_ms: 0,
        }];
        app.conversation_session = Some("s".into());
        app.composer.insert('h');
        app.composer.insert('i');
        app.composer.attach("image.png");
        app.send_draft(&path).await.unwrap();
        assert_eq!(app.composer.draft, "");
        assert_eq!(app.composer.cursor, 0);
        assert!(app.composer.attachments.is_empty());
        assert!(
            app.conversation_lines
                .iter()
                .any(|l| l.body == "hello from mock")
        );
    }

    async fn mock_prompt_host(stream: tokio::net::UnixStream) {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let (r, mut w) = stream.into_split();
        let mut reader = BufReader::new(r);
        let mut prompt_seen = false;
        let mut messages_seen = false;
        let mut line = String::new();
        while reader.read_line(&mut line).await.unwrap() != 0 {
            if line.contains("\"method\":\"prompt\"") {
                let resp = serde_json::json!({
                    "type":"response", "id":"tui-0", "success":true, "data":null
                });
                w.write_all(format!("{}\n", ion_protocol::serialize_line(&resp)).as_bytes())
                    .await
                    .unwrap();
                prompt_seen = true;
            } else if line.contains("\"method\":\"get_session_messages\"") {
                let resp = serde_json::json!({
                    "type":"response", "id":"tui-1", "success":true,
                    "data": {"messages": [
                        {"message": {"role":"user", "content":"hello from mock"}}
                    ]}
                });
                w.write_all(format!("{}\n", ion_protocol::serialize_line(&resp)).as_bytes())
                    .await
                    .unwrap();
                messages_seen = true;
                break;
            }
            line.clear();
        }
        w.flush().await.unwrap();
        let _ = (prompt_seen, messages_seen);
    }
}

#[cfg(test)]
mod turn_navigation_tests {
    use crate::app::App;
    use serde_json::json;

    fn app_with_turns() -> App {
        let mut app = App::new();
        let msgs = vec![
            json!({"message": {"role":"user","content":"turn one"}}),
            json!({"message": {"role":"tool","toolName":"bash","content":"done"},"id":"t1"}),
            json!({"message": {"role":"user","content":"turn two"}}),
        ];
        app.conversation_lines = crate::conversation::message_lines(&msgs, &Default::default());
        app
    }

    #[test]
    fn turn_navigation_walks_history_and_expands_each_turn() {
        let mut app = app_with_turns();
        app.turn_next();
        assert_eq!(app.turn_cursor, Some(0));
        assert!(app.expanded_turns.contains(&0));
        app.turn_next();
        assert_eq!(app.turn_cursor, Some(1));
        assert!(app.expanded_turns.contains(&1));
        app.turn_prev();
        assert_eq!(app.turn_cursor, Some(0));
    }

    #[test]
    fn attachments_add_remove_and_dedupe() {
        let mut app = App::new();
        app.add_attachment("a.png");
        app.add_attachment("a.png");
        app.add_attachment("b.txt");
        assert_eq!(app.composer.attachments, vec!["a.png", "b.txt"]);
        assert_eq!(app.remove_last_attachment().as_deref(), Some("b.txt"));
        assert_eq!(app.remove_last_attachment().as_deref(), Some("a.png"));
        assert!(app.remove_last_attachment().is_none());
    }
}

#[cfg(test)]
mod approval_state_tests {
    use crate::app::App;
    use crate::approvals::ApprovalCard;

    fn card(id: &str, sid: &str) -> ApprovalCard {
        ApprovalCard {
            id: id.into(),
            kind: "ui_ask".into(),
            session_id: sid.into(),
            title: None,
            summary: None,
            payload: serde_json::json!({}),
        }
    }

    #[test]
    fn approval_cursor_moves_and_clears() {
        let mut app = App::new();
        app.approvals = vec![card("a", "s"), card("b", "s")];
        app.approval_next();
        assert_eq!(app.selected_approval_id().as_deref(), Some("a"));
        app.approval_next();
        assert_eq!(app.selected_approval_id().as_deref(), Some("b"));
        app.approval_prev();
        assert_eq!(app.selected_approval_id().as_deref(), Some("a"));
        app.approvals.clear();
        app.approval_cursor = None;
    }

    #[test]
    fn busy_state_reflected_in_headless_output() {
        let mut app = App::new();
        app.selected_busy = true;
        let out = app.render_conversation_text();
        assert!(out.iter().any(|l| l.contains("⏹ 停止 (Esc/s)")));
        app.selected_busy = false;
        let out = app.render_conversation_text();
        assert!(!out.iter().any(|l| l.contains("⏹ 停止")));
    }
}

#[cfg(test)]
mod approval_rpc_tests {
    use crate::app::App;
    use std::path::PathBuf;

    async fn serve(path: PathBuf, script: Vec<&'static str>) {
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            for expected in script {
                let (stream, _) = listener.accept().await.unwrap();
                let (r, mut w) = stream.into_split();
                let mut reader = tokio::io::BufReader::new(r);
                let mut line = String::new();
                use tokio::io::AsyncBufReadExt;
                reader.read_line(&mut line).await.unwrap();
                assert!(line.contains(expected), "expected {expected} in {line}");
                let id = serde_json::from_str::<serde_json::Value>(&line)
                    .unwrap()
                    .get("id")
                    .cloned()
                    .unwrap_or_default();
                let resp = serde_json::json!({"type":"response","id":id,"success":true,"data":{"pending":[],"sessions":[]}});
                use tokio::io::AsyncWriteExt;
                w.write_all(format!("{}\n", ion_protocol::serialize_line(&resp)).as_bytes())
                    .await
                    .unwrap();
                w.flush().await.unwrap();
            }
        });
    }

    #[tokio::test]
    async fn respond_approval_calls_rpc_and_reloads_pending() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("sock");
        serve(path.clone(), vec!["approval_respond", "approvals_pending"]).await;
        let mut app = App::new();
        app.approvals = vec![crate::approvals::ApprovalCard {
            id: "apr_1".into(),
            kind: "ui_ask".into(),
            session_id: "s".into(),
            title: None,
            summary: None,
            payload: serde_json::json!({}),
        }];
        app.approval_cursor = Some(0);
        app.respond_approval(&path, true).await.unwrap();
        assert!(app.approvals.is_empty());
        assert_eq!(app.approval_cursor, None);
    }

    #[tokio::test]
    async fn abort_selected_calls_send_to_session_and_refreshes() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("sock");
        serve(
            path.clone(),
            vec!["send_to_session", "list_all_sessions", "list_sessions"],
        )
        .await;
        let mut app = App::new();
        app.session_rows = vec![crate::sessions::TreeRow {
            session_id: "s".into(),
            label: "s".into(),
            indent: 0,
            kind: crate::sessions::SessionKind::Root,
            busy: true,
            durable_active: false,
            agent: Some("build".into()),
            ctx_used: 0,
            ctx_max: 128_000,
            runtime_ms: 0,
        }];
        app.session_cursor = Some(0);
        app.abort_selected(&path).await.unwrap();
        assert!(!app.selected_busy);
    }
}

#[cfg(test)]
mod picker_live_integration_tests {
    use crate::app::App;
    use crate::state::TaskRow;

    #[test]
    fn picker_confirm_adds_attachment_and_close_clears() {
        let mut app = App::new();
        app.open_attachment_picker();
        for c in "截图.png".chars() {
            app.attachment_picker.insert(c);
        }
        app.confirm_attachment_picker();
        assert_eq!(app.composer.attachments, vec!["截图.png"]);
        app.open_attachment_picker();
        app.attachment_picker.insert('x');
        app.close_attachment_picker();
        assert!(!app.attachment_picker.open);
        assert_eq!(app.composer.attachments.len(), 1);
    }

    #[test]
    fn live_tool_derives_from_running_task_and_busy_card() {
        let mut app = App::new();
        app.tasks = vec![TaskRow {
            task_id: "t".into(),
            logical_key: "call-bash:0".into(),
            kind: "bash".into(),
            state: "running".into(),
            version: 0,
        }];
        app.session_rows = vec![crate::sessions::TreeRow {
            session_id: "s".into(),
            label: "s".into(),
            indent: 0,
            kind: crate::sessions::SessionKind::Root,
            busy: true,
            durable_active: false,
            agent: None,
            ctx_used: 0,
            ctx_max: 128_000,
            runtime_ms: 1_400,
        }];
        app.session_cursor = Some(0);
        app.sync_composer_from_selected();
        let card = app.live_tool.expect("live card");
        assert_eq!(card.name, "bash");
        assert!(card.render()[0].contains("[tool] bash — call-bash:0"));
    }
}

#[cfg(test)]
mod tool_stream_state_tests {
    use crate::app::App;
    use crate::stream::ToolStreamEvent;

    #[test]
    fn stream_events_update_selected_live_card() {
        let mut app = App::new();
        app.conversation_session = Some("s".into());
        app.apply_tool_event(ToolStreamEvent::Started {
            session_id: "other".into(),
            tool_call_id: "c".into(),
            tool_name: "bash".into(),
            command: "noop".into(),
        });
        assert!(app.live_tool.is_none());
        app.apply_tool_event(ToolStreamEvent::Started {
            session_id: "s".into(),
            tool_call_id: "c".into(),
            tool_name: "bash".into(),
            command: "tail -f log".into(),
        });
        app.apply_tool_event(ToolStreamEvent::Update {
            session_id: "s".into(),
            tool_call_id: "c".into(),
            partial: "first".into(),
        });
        app.apply_tool_event(ToolStreamEvent::Update {
            session_id: "s".into(),
            tool_call_id: "c".into(),
            partial: "second".into(),
        });
        app.apply_tool_event(ToolStreamEvent::Ended {
            session_id: "s".into(),
            tool_call_id: "c".into(),
            duration_ms: 2500,
        });
        let card = app.completed_tools.first().unwrap();
        assert_eq!(card.name, "bash");
        assert_eq!(card.command, "tail -f log");
        assert_eq!(card.elapsed_ms, 2500);
        assert!(card.output_tail.contains(&"first".to_string()));
        assert!(card.output_tail.contains(&"second".to_string()));
        assert!(card.output_tail.contains(&"— completed —".to_string()));
        assert!(
            app.render_conversation_text()
                .contains(&format!("▸ 已完成工具 ({})", app.completed_tools.len()))
        );
    }
}
