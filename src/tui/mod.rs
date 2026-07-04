pub mod app;
pub mod layout;
pub mod manager_conn;
pub mod theme;
pub mod state;
pub mod view;

/// Launch the TUI dashboard
pub async fn run_dashboard() -> Result<(), Box<dyn std::error::Error>> {
    let mut conn = manager_conn::ManagerConn::connect().await
        .map_err(|e| format!("Cannot connect to Manager: {e}"))?;

    let overview = conn.poll_overview().await
        .map_err(|e| format!("Failed to get overview: {e}"))?;

    let mut app = app::AppState::new(conn, overview);
    app.run().await
}
