use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

pub enum StreamEvent {
    Tool(ToolStreamEvent),
    ApprovalChanged,
}

#[allow(dead_code)]
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

pub fn is_approval_change(value: &serde_json::Value) -> bool {
    let event = value.get("event").unwrap_or(value);
    let custom = value
        .get("ui_type")
        .or_else(|| event.get("customType"))
        .or_else(|| event.get("type"))
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    matches!(
        custom,
        "ApprovalRequest"
            | "ApprovalResolved"
            | "ApprovalRemoved"
            | "ApprovalReset"
            | "AskResolved"
            | "AskTimedOut"
    )
}
pub struct SessionEventStream {
    receiver: mpsc::UnboundedReceiver<StreamEvent>,
}

pub struct RawEventStream {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    _writer: tokio::net::unix::OwnedWriteHalf,
}

impl RawEventStream {
    pub async fn subscribe(
        path: &Path,
        session_id: Option<&str>,
        ui: bool,
    ) -> Result<Self, String> {
        let stream = UnixStream::connect(path)
            .await
            .map_err(|e| format!("stream connect: {e}"))?;
        let (r, mut w) = stream.into_split();
        let req = ion_protocol::Request::subscribe(session_id, None, ui, None);
        w.write_all(format!("{req}\n").as_bytes())
            .await
            .map_err(|e| format!("stream write: {e}"))?;
        w.flush().await.map_err(|e| format!("stream flush: {e}"))?;
        Ok(Self {
            reader: BufReader::new(r),
            _writer: w,
        })
    }

    pub async fn next_frame(&mut self) -> Option<serde_json::Value> {
        let mut line = String::new();
        loop {
            let n = self.reader.read_line(&mut line).await.ok()?;
            if n == 0 {
                return None;
            }
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) {
                return Some(v);
            }
            line.clear();
        }
    }
}

impl SessionEventStream {
    pub async fn subscribe(path: &Path, session_id: &str) -> Result<Self, String> {
        if let Ok(event_socket) = std::env::var("ION_TUI_EVENT_SOCKET")
            && !event_socket.is_empty()
        {
            return Self::subscribe_overrides(
                path,
                std::path::Path::new(&event_socket),
                session_id,
            )
            .await;
        }
        let session = RawEventStream::subscribe(path, Some(session_id), false).await?;
        let (event_tx, receiver) = mpsc::unbounded_channel();
        spawn_frame_pump(session, event_tx.clone(), false);
        let ui = RawEventStream::subscribe(path, None, true).await.ok();
        if let Some(ui) = ui {
            spawn_frame_pump(ui, event_tx, true);
        }
        Ok(Self { receiver })
    }

    async fn subscribe_overrides(
        rpc_path: &Path,
        event_path: &Path,
        session_id: &str,
    ) -> Result<Self, String> {
        let session = RawEventStream::subscribe(event_path, Some(session_id), false).await?;
        let (event_tx, receiver) = mpsc::unbounded_channel();
        spawn_frame_pump(session, event_tx.clone(), false);
        let ui = RawEventStream::subscribe(event_path, None, true).await.ok();
        if let Some(ui) = ui {
            spawn_frame_pump(ui, event_tx, true);
        }
        let _ = rpc_path;
        Ok(Self { receiver })
    }

    pub async fn next_event(&mut self) -> Option<StreamEvent> {
        self.receiver.recv().await
    }
}

fn spawn_frame_pump(
    mut stream: RawEventStream,
    sender: mpsc::UnboundedSender<StreamEvent>,
    ui: bool,
) {
    tokio::spawn(async move {
        while let Some(frame) = stream.next_frame().await {
            let event = if ui {
                is_approval_change(&frame).then_some(StreamEvent::ApprovalChanged)
            } else if is_approval_change(&frame) {
                Some(StreamEvent::ApprovalChanged)
            } else {
                parse_tool_event(&frame).map(StreamEvent::Tool)
            };
            if let Some(event) = event
                && sender.send(event).is_err()
            {
                break;
            }
        }
    });
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
        let mut stream = SessionEventStream::subscribe(&path, "s").await.unwrap();
        let ev = stream.next_event().await.unwrap();
        assert!(
            matches!(ev, StreamEvent::Tool(ToolStreamEvent::Update { ref partial, .. }) if partial=="real")
        );
    }
}

#[cfg(test)]
mod approval_stream_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn detects_ui_approval_changes() {
        for custom in ["ApprovalRequest", "ApprovalResolved", "ApprovalRemoved"] {
            let frame = json!({"type":"ui_event","ui_type":custom});
            assert!(
                is_approval_change(&frame),
                "{custom} should trigger refresh"
            );
        }
        let unrelated = json!({"event":{"type":"text_delta","delta":"x"}});
        assert!(!is_approval_change(&unrelated));
    }
}

#[cfg(test)]
mod combined_stream_tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    #[tokio::test]
    async fn combined_stream_surfaces_approval_and_tool() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            // SessionEventStream subscribes session first, then UI.
            let (session, _) = listener.accept().await.unwrap();
            let (ui, _) = listener.accept().await.unwrap();
            let session_task = tokio::spawn(mock_stream_connection(
                session,
                vec![
                    json!({"event":{"type":"tool_execution_update","sessionId":"s","toolCallId":"c","partialResult":"out"}}),
                ],
            ));
            mock_stream_connection(
                ui,
                vec![
                    json!({"type":"subscribed","stream":"ui"}),
                    json!({"type":"ui_event","ui_type":"ApprovalRequest"}),
                ],
            )
            .await;
            session_task.await.unwrap();
        });
        let mut stream = SessionEventStream::subscribe(&path, "s").await.unwrap();
        let mut saw_approval = false;
        let mut saw_tool = false;
        for _ in 0..2 {
            match stream.next_event().await.unwrap() {
                StreamEvent::ApprovalChanged => saw_approval = true,
                StreamEvent::Tool(ToolStreamEvent::Update { partial, .. }) => {
                    saw_tool = partial == "out";
                }
                _ => {}
            }
        }
        assert!(saw_approval);
        assert!(saw_tool);
    }

    async fn mock_stream_connection(
        stream: tokio::net::UnixStream,
        events: Vec<serde_json::Value>,
    ) {
        let (r, mut w) = stream.into_split();
        let mut line = String::new();
        let mut reader = BufReader::new(r);
        reader.read_line(&mut line).await.unwrap();
        for event in events {
            w.write_all(format!("{}\n", event).as_bytes())
                .await
                .unwrap();
        }
        w.flush().await.unwrap();
    }
}
