use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

/// Manager Unix socket client
/// Auto-starts the Manager process if not running.
pub struct ManagerConn {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    buf: String,
    connected: bool,
}

impl ManagerConn {
    /// Connect to the Manager Unix socket. Auto-starts the Manager if needed.
    pub async fn connect() -> Result<Self, String> {
        let sock = crate::paths::manager_socket_path();

        // Try connecting directly first
        match Self::try_connect(&sock).await {
            Ok(conn) => return Ok(conn),
            Err(e) => {
                // If Manager not running, auto-start it
                if !sock.exists() {
                    eprintln!("[ion] Manager not running — auto-starting...");
                    if let Err(start_err) = Self::start_manager() {
                        return Err(format!("Failed to start Manager: {start_err}"));
                    }
                    // Wait for socket to appear
                    for i in 0..25 {
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                        if sock.exists() {
                            return Self::try_connect(&sock).await;
                        }
                        if i % 10 == 9 {
                            eprintln!("[ion] Waiting for Manager... ({})", (i + 1) * 200);
                        }
                    }
                    return Err("Manager did not start within 5 seconds".into());
                }
                return Err(e);
            }
        }
    }

    /// Try connecting without auto-start
    async fn try_connect(sock: &std::path::Path) -> Result<Self, String> {
        let stream = UnixStream::connect(sock).await
            .map_err(|e| format!("Cannot connect to Manager at {}: {e}", sock.display()))?;
        let (r, w) = stream.into_split();
        Ok(Self {
            reader: BufReader::new(r),
            writer: w,
            buf: String::new(),
            connected: true,
        })
    }

    /// Auto-start the Manager process
    fn start_manager() -> Result<std::process::Child, String> {
        // Find the ion binary (same as current exe)
        let ion_bin = std::env::current_exe()
            .map_err(|e| format!("Cannot find ion binary: {e}"))?;

        let child = std::process::Command::new(&ion_bin)
            .arg("manager")
            .arg("start")
            .stdout(std::process::Stdio::null())    // Manager logs to stderr
            .stderr(std::process::Stdio::inherit()) // Show on terminal
            .spawn()
            .map_err(|e| format!("Failed to start Manager: {e}"))?;

        Ok(child)
    }

    pub fn is_connected(&self) -> bool { self.connected }

    /// Poll get_overview. Creates a new connection each call since Manager
    /// socket handler is single-request-per-connection for non-subscribe commands.
    /// Returns the inner `data` payload (not the response wrapper).
    pub async fn poll_overview(&mut self) -> Result<Value, String> {
        let result = self.request_once(
            &serde_json::json!({"method":"get_overview","id":"poll"})
        ).await?;
        // Manager wraps: {"type":"response","success":true,"data":{...}}
        result.get("data").cloned()
            .ok_or_else(|| "response missing 'data' field".to_string())
    }

    /// Send a chat message (prompt) to a session.
    /// Uses a fresh connection (Manager is single-request-per-connection).
    pub async fn send_prompt(&mut self, session: &str, text: &str) -> Result<Value, String> {
        let req = serde_json::json!({
            "id": "tui-send",
            "method": "prompt",
            "session": session,
            "params": {"text": text}
        });
        self.request_once(&req).await
    }

    /// Create a new session (auto-spawns a worker).
    pub async fn create_session(&mut self, project_path: &str, agent: &str) -> Result<Value, String> {
        let req = serde_json::json!({
            "id": "tui-create",
            "method": "create_session",
            "params": {
                "project_path": project_path,
                "agent": agent,
            }
        });
        self.request_once(&req).await
    }

    /// Generic request via a fresh connection.
    async fn request_once(&mut self, req: &Value) -> Result<Value, String> {
        match Self::connect().await {
            Ok(mut fresh) => {
                let result = fresh.request_inner(req).await;
                self.connected = result.is_ok();
                result
            }
            Err(e) => {
                self.connected = false;
                Err(e)
            }
        }
    }

    /// Low-level request-response, uses the current connection.
    /// After a successful call the Manager closes the connection,
    /// so callers should reconnect.
    async fn request_inner(&mut self, req: &Value) -> Result<Value, String> {
        let line = format!("{req}\n");
        self.writer.write_all(line.as_bytes()).await
            .map_err(|e| format!("write: {e}"))?;
        self.writer.flush().await
            .map_err(|e| format!("flush: {e}"))?;
        self.buf.clear();
        self.reader.read_line(&mut self.buf).await
            .map_err(|e| format!("read: {e}"))
            .and_then(|n| if n == 0 {
                Err("connection closed".into())
            } else {
                serde_json::from_str(self.buf.trim())
                    .map_err(|e| format!("parse: {e}"))
            })
    }
}
