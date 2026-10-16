mod app;
mod connection;
mod input;
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
    if let Err(e) = app.poll_events(&mut conn).await {
        if e.contains("writer closed") {
            conn = connection::Connection::connect(&socket).await?;
            app.poll_events(&mut conn).await?;
        } else {
            return Err(e);
        }
    }

    if headless {
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
                            KeyCode::Char('j') | KeyCode::Down => app.select_next(),
                            KeyCode::Char('k') | KeyCode::Up => app.select_prev(),
                            KeyCode::Tab => panels.toggle_focus(),
                            KeyCode::Char('e') => {
                                if let Some(run_id) = app.selected_run_id() {
                                    let running = app.runs.iter().any(|r| {
                                        r.run_id == run_id
                                            && (r.state == "running" || r.state == "recovering")
                                    });
                                    panels.toggle(run_id, running);
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
