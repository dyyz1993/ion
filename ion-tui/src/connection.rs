use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

pub struct Connection {
    writer: tokio::net::unix::OwnedWriteHalf,
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
}

impl Connection {
    pub async fn connect(path: &Path) -> Result<Self, String> {
        let stream = UnixStream::connect(path)
            .await
            .map_err(|e| format!("connect {}: {e}", path.display()))?;
        let (read_half, write_half) = stream.into_split();
        Ok(Self {
            writer: write_half,
            reader: BufReader::new(read_half),
        })
    }

    /// Send one RPC request, skip event frames, and return the first response
    /// with a matching id. `timeout_ms` applies per read.
    pub async fn rpc(
        &mut self,
        method: &str,
        params: serde_json::Value,
        timeout_ms: u64,
    ) -> Result<serde_json::Value, String> {
        let id = format!(
            "tui-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos()
        );
        let req = ion_protocol::Request::rpc(&id, method, params);
        self.writer
            .write_all(format!("{req}\n").as_bytes())
            .await
            .map_err(|e| classify_write_error(method, e))?;
        self.writer
            .flush()
            .await
            .map_err(|e| classify_write_error(method, e))?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        loop {
            let remain = deadline.saturating_duration_since(std::time::Instant::now());
            if remain.is_zero() {
                return Err(format!("timeout waiting for {method} response"));
            }
            let mut line = String::new();
            match tokio::time::timeout(remain, self.reader.read_line(&mut line)).await {
                Err(_) => return Err(format!("timeout waiting for {method} response")),
                Ok(Ok(0)) => return Err("host closed connection".into()),
                Ok(Err(e)) => return Err(format!("read: {e}")),
                Ok(Ok(_)) => {}
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let v: serde_json::Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if ion_protocol::Request::has_request_id(&v) {
                return Ok(v);
            }
        }
    }
}

fn classify_write_error(method: &str, e: std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::BrokenPipe {
        format!("write {method}: writer closed (broken pipe)")
    } else {
        format!("write {method}: {e}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rpc_skips_events_and_matches_response() {
        let dir = tempfile_dir();
        let path = dir.path().join("test.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            mock_host(stream).await;
        });
        let mut conn = Connection::connect(&path).await.unwrap();
        let resp = conn
            .rpc("run.list", serde_json::json!({}), 2000)
            .await
            .unwrap();
        assert_eq!(resp["id"], "tui-0");
        assert_eq!(resp["data"][0]["runId"], "run-a");
    }

    async fn mock_host(stream: UnixStream) {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let (r, mut w) = stream.into_split();
        let mut reader = BufReader::new(r);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let _ = reader;
        let event =
            ion_protocol::pump_event_frame("w1", "s1", serde_json::json!({"kind": "noise"}));
        w.write_all(format!("{}\n", ion_protocol::serialize_line(&event)).as_bytes())
            .await
            .unwrap();
        let response = serde_json::json!({
            "type": "response", "id": "tui-0", "success": true,
            "data": [{"runId": "run-a", "sessionId": "s1", "state": "running", "version": 1, "updatedMs": 0}]
        });
        w.write_all(format!("{}\n", ion_protocol::serialize_line(&response)).as_bytes())
            .await
            .unwrap();
        w.flush().await.unwrap();
    }

    fn tempfile_dir() -> tempfile::TempDir {
        tempfile::TempDir::new().unwrap()
    }
}
