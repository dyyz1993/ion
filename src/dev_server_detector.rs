//! Dev Server Detector Extension
//!
//! Detects dev server ports from bash tool output (or probes common ports
//! as a fallback when a background dev server command is issued but stdout
//! hasn't printed a port yet).  Injects a `<dev_servers>` XML block into the
//! system prompt so the LLM knows which dev servers are running and on which
//! ports.
//!
//! Architecture: worker-level extension (not singleton).  Each worker gets its
//! own instance, so session isolation is automatic — no `HashMap<session_id, …>`
//! is needed.

use crate::agent::error::{AgentError, AgentResult};
use crate::agent::extension::{Extension, ToolExecutionContext};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Configuration for the dev server detector extension.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DevServerDetectorConfig {
    /// Whether the extension is enabled (default false).
    pub enabled: bool,
    /// Per-port probe timeout in seconds (default 15).
    pub probe_timeout_secs: u64,
    /// Candidate ports for background probing.
    pub probe_ports: Vec<u16>,
}

impl Default for DevServerDetectorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            probe_timeout_secs: 15,
            probe_ports: vec![3000, 5173, 8080, 8000, 4200, 8888, 4173, 5000],
        }
    }
}

// ---------------------------------------------------------------------------
// Detected server record
// ---------------------------------------------------------------------------

/// A single detected dev server.
#[derive(Debug, Clone)]
pub struct DetectedServer {
    /// Port number the server is listening on.
    pub port: u16,
    /// The command that triggered detection (truncated to 200 chars).
    pub source_cmd: String,
    /// How the port was detected: "stdout_regex" or "probe".
    pub detected_via: String,
    /// When the server was first detected.
    pub first_seen: Instant,
    /// Whether the port is alive (confirmed by probe).
    pub alive: bool,
}

// ---------------------------------------------------------------------------
// Extension struct
// ---------------------------------------------------------------------------

/// Dev server detector extension (worker-level, not singleton).
///
/// State lives on `self`; worker boundary provides session isolation.
pub struct DevServerDetectorExtension {
    /// Detected dev servers for this worker/session.
    servers: Arc<Mutex<Vec<DetectedServer>>>,
    /// Last injected signature — used for de-duplication so the XML block is
    /// only appended when the port set actually changes.
    last_injected_signature: Arc<Mutex<Option<String>>>,
    /// Extension configuration (static after construction).
    config: DevServerDetectorConfig,
    /// Extension name (for extension_rpc routing).
    name: String,
}

impl DevServerDetectorExtension {
    /// Create a new instance with default configuration.
    pub fn new() -> Self {
        Self {
            servers: Arc::new(Mutex::new(Vec::new())),
            last_injected_signature: Arc::new(Mutex::new(None)),
            config: DevServerDetectorConfig::default(),
            name: "dev_server_detector".to_string(),
        }
    }
}

impl Default for DevServerDetectorExtension {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Extension trait impl
// ---------------------------------------------------------------------------

#[async_trait]
impl Extension for DevServerDetectorExtension {
    fn name(&self) -> &str {
        &self.name
    }

    // ── Hook 1: detect ports from bash output ──────────────────────────

    async fn on_tool_execution_end(&self, ctx: &ToolExecutionContext) -> AgentResult<()> {
        // Step 1: care about both bash (sync) and bash (background) tools.
        // bash is the correct tool for starting dev servers (non-blocking),
        // so we must detect ports from its output too.
        if ctx.tool_name != "bash" {
            return Ok(());
        }

        // Extract the command string from ctx.args["command"]
        let cmd: String = ctx
            .args
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // Step 2: scan stdout for port numbers
        let stdout = &ctx.result;
        let ports_from_stdout = extract_ports_from_stdout(stdout);

        // Step 3: record any ports found — drop lock before any await
        {
            let mut servers = self.servers.lock().await;
            for port in &ports_from_stdout {
                add_server_if_new(&mut servers, *port, &cmd, "stdout_regex");
            }
        }

        // Step 4: if no ports in stdout but the command looks like a dev server,
        // spawn a background probe as a fallback (e.g. `npm run dev &` may not
        // have printed its port yet).
        let looks_like_dev_server = is_dev_server_command(&cmd);
        if ports_from_stdout.is_empty() && looks_like_dev_server {
            let servers = self.servers.clone();
            let config = self.config.clone();
            let cmd = cmd.clone();
            tokio::spawn(async move {
                probe_and_record(servers, cmd, config).await;
            });
        }

        Ok(())
    }

    // ── Hook 2: inject <dev_servers> XML into system prompt ────────────

    async fn on_system_prompt(&self, prompt: &mut String) -> AgentResult<()> {
        let to_inject: Option<String>;
        {
            let mut servers = self.servers.lock().await;

            // Step 1: filter out dead servers (alive=false and older than 30s)
            servers.retain(|s| s.alive || s.first_seen.elapsed() < Duration::from_secs(30));

            if servers.is_empty() {
                // No servers detected — also reset signature so the next detection
                // triggers a fresh injection.
                let mut last_sig = self.last_injected_signature.lock().await;
                *last_sig = None;
                return Ok(());
            }

            // Step 2: compute signature for de-duplication
            let signature = compute_signature(&servers);

            // Step 3: skip if signature unchanged
            let mut last_sig = self.last_injected_signature.lock().await;
            if last_sig.as_deref() == Some(&signature) {
                return Ok(());
            }

            // Step 4: build XML and update signature
            to_inject = Some(format_dev_servers_xml(&servers));
            *last_sig = Some(signature);
        } // drop both locks

        if let Some(xml) = to_inject {
            prompt.push_str("\n\n");
            prompt.push_str(&xml);
        }

        Ok(())
    }

    // ── Hook 3: extension_rpc for CLI queries ──────────────────────────

    async fn on_extension_rpc(
        &self,
        method: &str,
        _params: serde_json::Value,
    ) -> AgentResult<serde_json::Value> {
        match method {
            // List all detected servers
            "list" => {
                let servers = self.servers.lock().await;
                let server_list: Vec<serde_json::Value> = servers
                    .iter()
                    .map(|s| {
                        serde_json::json!({
                            "port": s.port,
                            "framework": infer_framework(&s.source_cmd),
                            "source_cmd": s.source_cmd,
                            "detected_via": s.detected_via,
                            "age_secs": s.first_seen.elapsed().as_secs(),
                            "alive": s.alive,
                        })
                    })
                    .collect();
                let count = server_list.len();
                Ok(serde_json::json!({
                    "servers": server_list,
                    "count": count,
                }))
            }

            // Clear all detected servers
            "clear" => {
                let mut servers = self.servers.lock().await;
                let cleared = servers.len();
                servers.clear();
                // Also reset the signature so the next detection injects fresh XML
                let mut last_sig = self.last_injected_signature.lock().await;
                *last_sig = None;
                Ok(serde_json::json!({ "cleared": cleared }))
            }

            // Manually trigger a port probe
            "probe" => {
                let config = self.config.clone();
                let servers_arc = self.servers.clone();

                // Probe all candidate ports
                let alive_ports = probe_ports_sync(&config).await;

                let probed_ports: Vec<u16> = config.probe_ports.clone();
                let newly_detected = alive_ports.len();

                // Record alive ports
                {
                    let mut servers = servers_arc.lock().await;
                    for port in &alive_ports {
                        add_server_if_new(&mut servers, *port, "manual_probe", "probe");
                    }
                }

                Ok(serde_json::json!({
                    "probed_ports": probed_ports,
                    "alive": alive_ports,
                    "newly_detected": newly_detected,
                }))
            }

            _ => Err(AgentError::Tool(format!(
                "dev_server_detector: unknown method '{method}'"
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Parse leading ASCII digits from a string slice into a u16.
/// Stops at the first non-digit character.
fn parse_leading_digits(s: &str) -> Option<u16> {
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Extract port numbers from stdout text using hand-written scanning (no regex).
///
/// Recognised patterns (case-insensitive):
/// - `localhost:PORT`, `127.0.0.1:PORT`, `0.0.0.0:PORT`
/// - `port NNNN`
/// - `listening on :NNNN` / `... on :NNNN`
fn extract_ports_from_stdout(stdout: &str) -> Vec<u16> {
    let mut ports = Vec::new();

    for line in stdout.lines() {
        let line_lower = line.to_lowercase();

        // Pattern 1: localhost:PORT / 127.0.0.1:PORT / 0.0.0.0:PORT
        for prefix in &["localhost:", "127.0.0.1:", "0.0.0.0:"] {
            if let Some(idx) = line_lower.find(prefix) {
                let after = &line[idx + prefix.len()..];
                if let Some(port) = parse_leading_digits(after) {
                    if (1024..=65535).contains(&port) && !ports.contains(&port) {
                        ports.push(port);
                    }
                }
            }
        }

        // Pattern 2: "port NNNN"
        if let Some(idx) = line_lower.find("port ") {
            let after = &line[idx + 5..];
            if let Some(port) = parse_leading_digits(after) {
                if (1024..=65535).contains(&port) && !ports.contains(&port) {
                    ports.push(port);
                }
            }
        }

        // Pattern 3: "listening on :NNNN" / "on :NNNN"
        if let Some(idx) = line_lower.rfind(':') {
            let after = &line[idx + 1..];
            if let Some(port) = parse_leading_digits(after) {
                let before = line_lower[..idx].trim_end();
                if (before.ends_with("on") || before.ends_with("listening"))
                    && (1024..=65535).contains(&port)
                    && !ports.contains(&port)
                {
                    ports.push(port);
                }
            }
        }
    }

    ports
}

/// Check whether a command string looks like a dev server startup command.
fn is_dev_server_command(cmd: &str) -> bool {
    let lower = cmd.to_lowercase();
    let patterns = [
        "npm run dev",
        "npm start",
        "yarn dev",
        "yarn start",
        "pnpm dev",
        "pnpm start",
        "bun dev",
        "bun run dev",
        "vite",
        "next dev",
        "nuxt dev",
        "ng serve",
        "webpack-dev-server",
        "parcel",
        "python -m http.server",
        "python3 -m http.server",
        "manage.py runserver",
        "flask run",
        "uvicorn ",
        "fastapi ",
        "go run main.go",
        "air",
        "php artisan serve",
        "rails server",
        "rails s",
        "docker compose up",
        "docker-compose up",
    ];
    patterns.iter().any(|p| lower.contains(p))
}

/// Add a server to the list only if the port is not already recorded.
fn add_server_if_new(servers: &mut Vec<DetectedServer>, port: u16, source_cmd: &str, via: &str) {
    // Skip if port already in the list
    if servers.iter().any(|s| s.port == port) {
        return;
    }
    let truncated_cmd: String = source_cmd.chars().take(200).collect();
    servers.push(DetectedServer {
        port,
        source_cmd: truncated_cmd,
        detected_via: via.to_string(),
        first_seen: Instant::now(),
        alive: true,
    });
}

/// Compute a normalised signature string from the server list.
///
/// Two server lists produce the same signature iff they have the same
/// (port, alive) pairs.  Used for de-duplication of system prompt injection.
fn compute_signature(servers: &[DetectedServer]) -> String {
    let mut sigs: Vec<String> = servers
        .iter()
        .map(|s| format!("{}:{}", s.port, s.alive))
        .collect();
    sigs.sort();
    sigs.join(",")
}

/// Format the `<dev_servers>` XML block for system prompt injection.
fn format_dev_servers_xml(servers: &[DetectedServer]) -> String {
    let mut out = format!("<dev_servers count=\"{}\">\n", servers.len());
    for s in servers {
        let framework = infer_framework(&s.source_cmd);
        let cmd_short: String = s.source_cmd.chars().take(80).collect();
        let age = format_duration(s.first_seen.elapsed());
        out.push_str(&format!(
            "<server port=\"{}\" framework=\"{}\" cmd=\"{}\" age=\"{}\" via=\"{}\"/>\n",
            s.port, framework, cmd_short, age, s.detected_via
        ));
    }
    out.push_str("</dev_servers>");
    out
}

/// Infer the framework name from the source command string.
fn infer_framework(cmd: &str) -> &'static str {
    let lower = cmd.to_lowercase();
    if lower.contains("next") {
        "next"
    } else if lower.contains("vite") {
        "vite"
    } else if lower.contains("flask") {
        "flask"
    } else if lower.contains("django") || lower.contains("manage.py") {
        "django"
    } else if lower.contains("uvicorn") || lower.contains("fastapi") {
        "fastapi"
    } else if lower.contains("angular") || lower.contains("ng serve") {
        "angular"
    } else if lower.contains("webpack") {
        "webpack"
    } else if lower.contains("rails") {
        "rails"
    } else if lower.contains("nuxt") {
        "nuxt"
    } else if lower.contains("http.server") {
        "python-http"
    } else {
        "unknown"
    }
}

/// Format a Duration into a human-readable string like "30s", "5m", "1h".
fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 3600)
    }
}

/// Probe a list of ports concurrently and return those that are alive.
///
/// Each port gets a 2-second connect timeout.
async fn probe_ports_sync(config: &DevServerDetectorConfig) -> Vec<u16> {
    use tokio::net::TcpStream;

    let timeout = Duration::from_secs(2);
    let mut handles = Vec::new();

    for &port in &config.probe_ports {
        let port = port as u16;
        handles.push(tokio::spawn(async move {
            let addr = format!("127.0.0.1:{port}");
            match tokio::time::timeout(timeout, TcpStream::connect(&addr)).await {
                Ok(Ok(_)) => Some(port),
                _ => None,
            }
        }));
    }

    let mut alive = Vec::new();
    for handle in handles {
        if let Ok(Some(port)) = handle.await {
            alive.push(port);
        }
    }
    alive
}

/// Background probe: concurrently connect to all candidate ports and record
/// any that are alive into the shared server list.
async fn probe_and_record(
    servers: Arc<Mutex<Vec<DetectedServer>>>,
    source_cmd: String,
    config: DevServerDetectorConfig,
) {
    let alive_ports = probe_ports_sync(&config).await;

    if !alive_ports.is_empty() {
        let mut servers = servers.lock().await;
        for port in alive_ports {
            add_server_if_new(&mut servers, port, &source_cmd, "probe");
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_ports_vite() {
        let stdout =
            "  VITE v5.0.0\n  Local:   http://localhost:5173/\n  Network: http://192.168.1.5:5173/";
        let ports = extract_ports_from_stdout(stdout);
        assert_eq!(ports, vec![5173]);
    }

    #[test]
    fn test_extract_ports_nextjs() {
        let stdout = "  Next.js 14.0.0\n  - Local:   http://localhost:3000";
        let ports = extract_ports_from_stdout(stdout);
        assert_eq!(ports, vec![3000]);
    }

    #[test]
    fn test_extract_ports_python() {
        let stdout = "Serving HTTP on 0.0.0.0 port 8000 (http://0.0.0.0:8000/) ...";
        let ports = extract_ports_from_stdout(stdout);
        assert!(ports.contains(&8000), "expected 8000 in {ports:?}");
    }

    #[test]
    fn test_extract_ports_flask() {
        let stdout = " * Running on http://127.0.0.1:5000";
        let ports = extract_ports_from_stdout(stdout);
        assert_eq!(ports, vec![5000]);
    }

    #[test]
    fn test_extract_ports_none() {
        let stdout = "total 48\ndrwxr-xr-x  3 user  staff   96 Jan  1 12:00 src\n-rw-r--r--  1 user  staff  123 Jan  1 12:00 README.md";
        let ports = extract_ports_from_stdout(stdout);
        assert!(ports.is_empty(), "expected empty, got {ports:?}");
    }

    #[test]
    fn test_is_dev_server_command_npm() {
        assert!(is_dev_server_command("npm run dev"));
        assert!(is_dev_server_command("cd /app && npm run dev &"));
    }

    #[test]
    fn test_is_dev_server_command_ls() {
        assert!(!is_dev_server_command("ls -la"));
        assert!(!is_dev_server_command("cat file.txt"));
    }

    #[test]
    fn test_compute_signature_dedup() {
        let now = Instant::now();
        let servers_a = vec![
            DetectedServer {
                port: 3000,
                source_cmd: "npm run dev".into(),
                detected_via: "stdout_regex".into(),
                first_seen: now,
                alive: true,
            },
            DetectedServer {
                port: 5173,
                source_cmd: "vite".into(),
                detected_via: "probe".into(),
                first_seen: now,
                alive: true,
            },
        ];
        let servers_b = vec![
            DetectedServer {
                port: 5173,
                source_cmd: "different cmd".into(),
                detected_via: "stdout_regex".into(),
                first_seen: now,
                alive: true,
            },
            DetectedServer {
                port: 3000,
                source_cmd: "another".into(),
                detected_via: "probe".into(),
                first_seen: now,
                alive: true,
            },
        ];
        // Same ports + alive status => same signature (order independent)
        assert_eq!(compute_signature(&servers_a), compute_signature(&servers_b));
    }

    #[test]
    fn test_format_xml_basic() {
        let now = Instant::now();
        let servers = vec![DetectedServer {
            port: 3000,
            source_cmd: "npm run dev".into(),
            detected_via: "stdout_regex".into(),
            first_seen: now,
            alive: true,
        }];
        let xml = format_dev_servers_xml(&servers);
        assert!(
            xml.contains("<dev_servers"),
            "missing <dev_servers> in:\n{xml}"
        );
        assert!(
            xml.contains("port=\"3000\""),
            "missing port=\"3000\" in:\n{xml}"
        );
        assert!(xml.contains("</dev_servers>"), "missing closing tag");
    }

    #[test]
    fn test_parse_leading_digits() {
        assert_eq!(parse_leading_digits("5173/"), Some(5173));
        assert_eq!(parse_leading_digits("3000 "), Some(3000));
        assert_eq!(parse_leading_digits("abc"), None);
        assert_eq!(parse_leading_digits(""), None);
    }

    #[test]
    fn test_infer_framework() {
        assert_eq!(infer_framework("npm run dev && next dev"), "next");
        assert_eq!(infer_framework("vite"), "vite");
        assert_eq!(infer_framework("flask run"), "flask");
        assert_eq!(infer_framework("ls -la"), "unknown");
    }

    #[test]
    fn test_add_server_if_new_dedup() {
        let mut servers = vec![];
        add_server_if_new(&mut servers, 3000, "npm run dev", "stdout_regex");
        assert_eq!(servers.len(), 1);
        // Adding the same port again should not duplicate
        add_server_if_new(&mut servers, 3000, "different cmd", "probe");
        assert_eq!(servers.len(), 1);
        // A different port should be added
        add_server_if_new(&mut servers, 5173, "vite", "probe");
        assert_eq!(servers.len(), 2);
    }
}
