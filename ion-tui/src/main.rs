use std::path::PathBuf;

mod connection;
mod state;

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
    match run(socket, headless).await {
        Ok(()) => {}
        Err(e) => {
            eprintln!("ion-tui error: {e}");
            std::process::exit(1);
        }
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
    let response = conn.rpc("run.list", serde_json::json!({}), 5000).await?;
    let entries = state::parse_run_list(&response);
    if headless {
        for line in state::render_lines(&entries) {
            println!("{line}");
        }
        return Ok(());
    }
    // TODO(P2): ratatui interactive loop.
    Ok(())
}
