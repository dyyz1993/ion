mod app;
mod connection;
mod input;
mod state;

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

    if headless {
        for line in app.render() {
            println!("{line}");
        }
        return Ok(());
    }

    use tokio::io::AsyncBufReadExt;
    let stdin = tokio::io::stdin();
    let mut lines = tokio::io::BufReader::new(stdin).lines();
    let mut eof = false;
    while !eof {
        for line in app.render() {
            println!("{line}");
        }
        println!("keys: j/k select, p pause, r resume, a auto-resume, q quit");
        let key = match tokio::time::timeout(std::time::Duration::from_secs(10), lines.next_line())
            .await
        {
            Err(_) => continue,
            Ok(Err(e)) => return Err(e.to_string()),
            Ok(Ok(None)) => {
                eof = true;
                continue;
            }
            Ok(Ok(Some(k))) => k,
        };
        let key = key.trim().to_string();
        eof = key == "q";
        if matches!(key.as_str(), "j" | "down") {
            app.select_next();
            continue;
        }
        if matches!(key.as_str(), "k" | "up") {
            app.select_prev();
            continue;
        }
        let Some(action) = Action::from_key(&key, app.selected_run_id()) else {
            app.status = format!("unknown key: {key}");
            continue;
        };
        if action == Action::Quit {
            break;
        }
        if let Err(e) = app.dispatch(&mut conn, action.clone()).await {
            // Host may close idle pipes between key presses; reconnect once and retry,
            // tolerating a second broken pipe on the reconnect window.
            if e.contains("writer closed") {
                conn = connection::Connection::connect(&socket).await?;
                if let Err(retry_err) = app.dispatch(&mut conn, action).await
                    && !retry_err.contains("writer closed")
                {
                    return Err(retry_err);
                }
            } else {
                return Err(e);
            }
        }
    }
    Ok(())
}
