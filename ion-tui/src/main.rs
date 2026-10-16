mod app;
mod connection;
mod conversation;
mod input;
mod sessions;
mod state;
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
    if let Err(e) = app.poll_events(&mut conn).await {
        if e.contains("writer closed") {
            conn = connection::Connection::connect(&socket).await?;
            app.poll_events(&mut conn).await?;
        } else {
            return Err(e);
        }
    }

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
    let mut panels = ui::PanelState::default();
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(1000));
    loop {
        let _ = app.poll_events(conn).await;
        let _ = app.load_detail(conn).await;
        let _ = app.refresh_sessions(socket).await;
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
            event = crossterm_event().fuse() => {
                let event = event.map_err(|e| format!("read: {e}"))?;
                match event {
                    crossterm::event::Event::Key(key) if key.kind == crossterm::event::KeyEventKind::Press => {
                        use crossterm::event::KeyCode;
                        match key.code {
                            KeyCode::Char('q') | KeyCode::Esc => break,
                            KeyCode::Tab => panels.toggle_focus(),
                            KeyCode::Char('1') => panels.focus = ui::Focus::Sessions,
                            KeyCode::Char('2') => panels.focus = ui::Focus::Conversation,
                            KeyCode::Char('3') => panels.focus = ui::Focus::Runs,
                            KeyCode::Char('4') => panels.focus = ui::Focus::Events,
                            KeyCode::Char('j') | KeyCode::Down => match panels.focus {
                                ui::Focus::Sessions => app.session_cursor_next(),
                                ui::Focus::Runs => app.select_next(),
                                ui::Focus::Conversation => app.turn_next(),
                                _ => {}
                            },
                            KeyCode::Char('k') | KeyCode::Up => match panels.focus {
                                ui::Focus::Sessions => app.session_cursor_prev(),
                                ui::Focus::Runs => app.select_prev(),
                                ui::Focus::Conversation => app.turn_prev(),
                                _ => {}
                            },
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
                                    // TTY file picker is v0.8; paste/current dir path mode.
                                    app.status = "attachment picker pending (v0.8)".into();
                                }
                            }
                            KeyCode::Char('x') if panels.focus == ui::Focus::Conversation => {
                                if let Some(name) = app.remove_last_attachment() {
                                    app.status = format!("removed attachment: {name}");
                                }
                            }
                            KeyCode::Char('e') if panels.focus == ui::Focus::Conversation => {
                                // Toggle the newest history turn (0-based index len-2).
                                let turn_count =
                                    crate::conversation::group_turns(&app.conversation_lines).len();
                                if turn_count >= 2 {
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
                                if !matches!(c, 'p' | 'r' | 'a' | 'q' | 'j' | 'k' | 'e') =>
                            {
                                if panels.focus == ui::Focus::Conversation {
                                    app.composer.insert(c);
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
