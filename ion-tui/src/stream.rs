use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolStreamEvent {
    Started {
        session_id: String,
        tool_call_id: String,
        tool_name: String,
        command: String,
    },
    Update {
        session_id: String,
        tool_call_id: String,
        partial: String,
    },
    Ended {
        session_id: String,
        tool_call_id: String,
        duration_ms: u64,
    },
}

pub fn parse_tool_event(value: &serde_json::Value) -> Option<ToolStreamEvent> {
    let event = value.get("event").unwrap_or(value);
    let ev_type = event.get("type").and_then(|v| v.as_str())?;
    let sid = event
        .get("sessionId")
        .or_else(|| event.get("session_id"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let call_id = event
        .get("toolCallId")
        .or_else(|| event.get("tool_call_id"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    match ev_type {
        "tool_execution_start" => {
            let name = event
                .get("toolName")
                .and_then(|v| v.as_str())
                .unwrap_or("tool");
            let args = event.get("args").cloned().unwrap_or_default();
            let command = args
                .get("command")
                .or_else(|| args.get("file_path"))
                .or_else(|| args.get("path"))
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            Some(ToolStreamEvent::Started {
                session_id: sid,
                tool_call_id: call_id,
                tool_name: name.to_string(),
                command,
            })
        }
        "tool_execution_update" => Some(ToolStreamEvent::Update {
            session_id: sid,
            tool_call_id: call_id,
            partial: event
                .get("partialResult")
                .or_else(|| event.get("partial"))
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
        }),
        "tool_execution_end" => Some(ToolStreamEvent::Ended {
            session_id: sid,
            tool_call_id: call_id,
            duration_ms: event
                .get("durationMs")
                .or_else(|| event.get("duration_ms"))
                .and_then(|v| v.as_u64())
                .unwrap_or_default(),
        }),
        _ => None,
    }
}

pub struct ToolEventStream {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    _writer: tokio::net::unix::OwnedWriteHalf,
}

impl ToolEventStream {
    pub async fn subscribe(path: &Path, session_id: &str) -> Result<Self, String> {
        let stream = UnixStream::connect(path)
            .await
            .map_err(|e| format!("stream connect: {e}"))?;
        let (r, mut w) = stream.into_split();
        let req = ion_protocol::Request::subscribe(Some(session_id), None, false, None);
        w.write_all(format!("{req}\n").as_bytes())
            .await
            .map_err(|e| format!("stream write: {e}"))?;
        w.flush().await.map_err(|e| format!("stream flush: {e}"))?;
        Ok(Self {
            reader: BufReader::new(r),
            _writer: w,
        })
    }

    pub async fn next_tool_event(&mut self) -> Option<ToolStreamEvent> {
        let mut line = String::new();
        loop {
            let n = self.reader.read_line(&mut line).await.ok()?;
            if n == 0 {
                return None;
            }
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim())
                && let Some(event) = parse_tool_event(&v)
            {
                return Some(event);
            }
            line.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn frame(ev: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"type":"event","event":ev})
    }

    #[test]
    fn parses_start_update_end() {
        let start = parse_tool_event(&frame(json!({
            "type":"tool_execution_start","sessionId":"s","toolCallId":"c",
            "toolName":"bash","args":{"command":"cargo test"}
        })))
        .unwrap();
        assert!(
            matches!(start, ToolStreamEvent::Started { ref session_id, ref command, .. } if session_id=="s" && command=="cargo test")
        );
        let update = parse_tool_event(&frame(json!({
            "type":"tool_execution_update","sessionId":"s","toolCallId":"c","partialResult":"line"
        })))
        .unwrap();
        assert!(matches!(update, ToolStreamEvent::Update { ref partial, .. } if partial=="line"));
        let end = parse_tool_event(&frame(json!({
            "type":"tool_execution_end","sessionId":"s","toolCallId":"c","durationMs":123
        })))
        .unwrap();
        assert!(matches!(
            end,
            ToolStreamEvent::Ended {
                duration_ms: 123,
                ..
            }
        ));
    }

    #[test]
    fn ignores_non_tool_events() {
        assert!(parse_tool_event(&frame(json!({"type":"text_delta","delta":"x"}))).is_none());
    }

    #[tokio::test]
    async fn stream_skips_non_tool_and_returns_update() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (r, mut w) = stream.into_split();
            let line = String::new();
            let mut reader = BufReader::new(r);
            let mut req = Vec::new();
            reader.read_until(b'\n', &mut req).await.unwrap();
            let _ = line;
            for v in [
                serde_json::json!({"type":"event","event":{"type":"text_delta","delta":"skip"}}),
                serde_json::json!({"type":"event","event":{"type":"tool_execution_update","sessionId":"s","toolCallId":"c","partialResult":"real"}}),
            ] {
                w.write_all(format!("{}\n", ion_protocol::serialize_line(&v)).as_bytes())
                    .await
                    .unwrap();
            }
            w.flush().await.unwrap();
        });
        let mut stream = ToolEventStream::subscribe(&path, "s").await.unwrap();
        let ev = stream.next_tool_event().await.unwrap();
        assert!(matches!(ev, ToolStreamEvent::Update { ref partial, .. } if partial=="real"));
    }
}
