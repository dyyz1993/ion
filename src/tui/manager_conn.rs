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

    /// Poll get_overview
    pub async fn poll_overview(&mut self) -> Result<Value, String> {
        self.request(&serde_json::json!({"method":"get_overview","id":"poll"})).await
    }

    /// Send a chat message (prompt) to a session
    pub async fn send_prompt(&mut self, session: &str, text: &str) -> Result<Value, String> {
        let req = serde_json::json!({
            "method": "send",
            "session": session,
            "rpc_method": "prompt",
            "params": {"text": text}
        });
        self.request(&req).await
    }

    /// Low-level request-response
    async fn request(&mut self, req: &Value) -> Result<Value, String> {
        if !self.connected {
            *self = Self::connect().await?;
        }
        let line = format!("{req}\n");
        self.writer.write_all(line.as_bytes()).await
            .map_err(|e| { self.connected = false; format!("write: {e}") })?;
        self.writer.flush().await
            .map_err(|e| { self.connected = false; format!("flush: {e}") })?;
        self.buf.clear();
        match self.reader.read_line(&mut self.buf).await {
            Ok(0) => { self.connected = false; Err("connection closed".into()) }
            Ok(_) => serde_json::from_str(self.buf.trim())
                .map_err(|e| format!("parse: {e}")),
            Err(e) => { self.connected = false; Err(format!("read: {e}")) }
        }
    }
}
