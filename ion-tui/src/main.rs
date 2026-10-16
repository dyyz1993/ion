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
    if let Err(e) = run(socket, headless).await {
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

async fn run(socket: PathBuf, headless: bool) -> Result<(), String> {
    let mut conn = connection::Connection::connect(&socket).await?;
    let mut app = App::new();
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
                                _ => {}
                            },
                            KeyCode::Char('k') | KeyCode::Up => match panels.focus {
                                ui::Focus::Sessions => app.session_cursor_prev(),
                                ui::Focus::Runs => app.select_prev(),
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
                                    // v0.6 composer send placeholder: validate draft,
                                    // keep it in status until prompt RPC is wired.
                                    if app.composer.draft.trim().is_empty() {
                                        app.status = "composer: empty".into();
                                    } else {
                                        app.status =
                                            format!("ready to send: {}", app.composer.draft);
                                    }
                                }
                                _ => {}
                            },
                            KeyCode::Backspace => {
                                if panels.focus == ui::Focus::Conversation {
                                    app.composer.backspace();
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
