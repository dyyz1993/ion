use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

/// Manager Unix socket client
/// Uses a single connection for request-response (poll_overview, send_prompt)
/// Reconnects automatically on disconnect
pub struct ManagerConn {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    buf: String,
    connected: bool,
}

impl ManagerConn {
    /// Connect to the Manager Unix socket
    pub async fn connect() -> Result<Self, String> {
        let sock = crate::paths::manager_socket_path();
        let stream = UnixStream::connect(&sock).await
            .map_err(|e| format!("Cannot connect to Manager at {}: {e}", sock.display()))?;
        let (r, w) = stream.into_split();
        Ok(Self {
            reader: BufReader::new(r),
            writer: w,
            buf: String::new(),
            connected: true,
        })
    }

    pub fn is_connected(&self) -> bool { self.connected }

    /// Poll get_overview. Creates a new connection each call since Manager
    /// socket handler is single-request-per-connection for non-subscribe commands.
    /// Returns the inner `data` payload (not the response wrapper).
    pub async fn poll_overview(&mut self) -> Result<Value, String> {
        match Self::connect().await {
            Ok(mut fresh) => {
                let result = fresh.request_inner(
                    &serde_json::json!({"method":"get_overview","id":"poll"})
                ).await;
                // Manager wraps response: {"type":"response","success":true,"data":{...}}
                // Extract the inner payload
                let payload = result.and_then(|v| {
                    v.get("data").cloned()
                        .ok_or_else(|| "response missing 'data' field".to_string())
                });
                self.connected = payload.is_ok();
                payload
            }
            Err(e) => {
                self.connected = false;
                Err(e)
            }
        }
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
        match Self::connect().await {
            Ok(mut fresh) => {
                let result = fresh.request_inner(&req).await;
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
