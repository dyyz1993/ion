use crate::error::{IonError, IonResult};
use crate::types::{SessionState, TaskResult};
use crate::worker::{Worker, WorkerStatus};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

// ---------------------------------------------------------------------------
// JSONL protocol messages
// ---------------------------------------------------------------------------

#[allow(dead_code)]
#[derive(Serialize)]
#[serde(tag = "type")]
enum Request {
    #[serde(rename = "prompt")]
    Prompt { id: u64, text: String },
    #[serde(rename = "steer")]
    Steer { id: u64, msg: String },
    #[serde(rename = "state")]
    State { id: u64 },
    #[serde(rename = "dispose")]
    Dispose { id: u64 },
}

#[allow(dead_code)]
#[derive(Deserialize)]
#[serde(tag = "type")]
enum Response {
    #[serde(rename = "ok")]
    Ok { id: u64 },
    #[serde(rename = "result")]
    PromptResult {
        id: u64,
        success: bool,
        output: String,
    },
    #[serde(rename = "state")]
    StateResult {
        id: u64,
        message_count: u64,
        turn_index: u64,
        summary: Option<String>,
    },
    #[serde(rename = "error")]
    Error { id: u64, message: String },
}

// ---------------------------------------------------------------------------
// ChildProcessWorker
// ---------------------------------------------------------------------------

pub struct ChildProcessWorker {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    stdout: Option<BufReader<ChildStdout>>,
    cmd: String,
    args: Vec<String>,
    next_id: u64,
    status: WorkerStatus,
}

impl ChildProcessWorker {
    pub fn new(cmd: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            child: None,
            stdin: None,
            stdout: None,
            cmd: cmd.into(),
            args,
            next_id: 1,
            status: WorkerStatus::Idle,
        }
    }

    async fn send_request(&mut self, request: &Request) -> IonResult<()> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| IonError::Worker("stdin not available".into()))?;
        let line = serde_json::to_string(request)?;
        let mut buf = line.into_bytes();
        buf.push(b'\n');
        stdin.write_all(&buf).await?;
        stdin.flush().await?;
        Ok(())
    }

    async fn read_response(&mut self) -> IonResult<Response> {
        let stdout = self
            .stdout
            .as_mut()
            .ok_or_else(|| IonError::Worker("stdout not available".into()))?;
        let mut line = String::new();
        stdout.read_line(&mut line).await?;
        if line.is_empty() {
            return Err(IonError::Worker("child process closed stdout".into()));
        }
        let resp: Response = serde_json::from_str(&line)?;
        Ok(resp)
    }

    async fn call(&mut self, request: Request) -> IonResult<Response> {
        self.send_request(&request).await?;
        self.read_response().await
    }
}

#[async_trait]
impl Worker for ChildProcessWorker {
    async fn connect(&mut self) -> IonResult<()> {
        let mut child = Command::new(&self.cmd)
            .args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| IonError::Worker("failed to take stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| IonError::Worker("failed to take stdout".into()))?;

        self.child = Some(child);
        self.stdin = Some(stdin);
        self.stdout = Some(BufReader::new(stdout));
        self.status = WorkerStatus::Idle;

        // Send a register handshake
        let id = self.next_id;
        self.next_id += 1;
        let reg = format!(r#"{{"type":"register","id":{id}}}"#);
        let mut buf = reg.into_bytes();
        buf.push(b'\n');
        if let Some(ref mut stdin) = self.stdin {
            stdin.write_all(&buf).await?;
            stdin.flush().await?;
        }
        // Read the handshake response
        if let Some(ref mut stdout) = self.stdout {
            let mut line = String::new();
            stdout.read_line(&mut line).await?;
            tracing::debug!("child worker registered: {}", line.trim());
        }

        Ok(())
    }

    async fn prompt(&mut self, text: String) -> IonResult<TaskResult> {
        let id = self.next_id;
        self.next_id += 1;
        let resp = self
            .call(Request::Prompt {
                id,
                text: text.clone(),
            })
            .await?;
        match resp {
            Response::PromptResult {
                success, output, ..
            } => Ok(TaskResult {
                success,
                output,
                tokens_used: None,
            }),
            Response::Error { message, .. } => Err(IonError::Worker(message)),
            other => Err(IonError::Rpc(format!(
                "unexpected response type: {:?}",
                std::mem::discriminant(&other)
            ))),
        }
    }

    async fn steer(&mut self, msg: String) -> IonResult<()> {
        let id = self.next_id;
        self.next_id += 1;
        let resp = self.call(Request::Steer { id, msg }).await?;
        match resp {
            Response::Ok { .. } => Ok(()),
            Response::Error { message, .. } => Err(IonError::Worker(message)),
            _ => Err(IonError::Rpc("unexpected response".into())),
        }
    }

    async fn state(&mut self) -> IonResult<SessionState> {
        let id = self.next_id;
        self.next_id += 1;
        let resp = self.call(Request::State { id }).await?;
        match resp {
            Response::StateResult {
                message_count,
                turn_index,
                summary,
                ..
            } => Ok(SessionState {
                message_count,
                turn_index,
                summary,
            }),
            Response::Error { message, .. } => Err(IonError::Worker(message)),
            _ => Err(IonError::Rpc("unexpected response".into())),
        }
    }

    async fn dispose(&mut self) -> IonResult<()> {
        if let Some(ref mut child) = self.child {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        self.child = None;
        self.stdin = None;
        self.stdout = None;
        self.status = WorkerStatus::Dead;
        Ok(())
    }
}

impl Drop for ChildProcessWorker {
    fn drop(&mut self) {
        // RAII: if the worker is dropped, kill the child process
        if let Some(ref mut child) = self.child {
            let _ = child.start_kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // ChildProcessWorker construction
    // -----------------------------------------------------------------------

    #[test]
    fn new_sets_command_and_args() {
        let worker = ChildProcessWorker::new("echo", vec!["hello".to_string()]);
        // command is stored internally; verify via args length and next_id default
        assert_eq!(worker.args.len(), 1);
        assert_eq!(worker.args[0], "hello");
    }

    #[test]
    fn new_initializes_next_id_to_one() {
        let worker = ChildProcessWorker::new("cmd", vec![]);
        assert_eq!(worker.next_id, 1);
    }

    #[test]
    fn new_initializes_status_to_idle() {
        let worker = ChildProcessWorker::new("cmd", vec![]);
        assert_eq!(worker.status, WorkerStatus::Idle);
    }

    #[test]
    fn new_starts_without_child_or_streams() {
        let worker = ChildProcessWorker::new("cmd", vec![]);
        assert!(worker.child.is_none());
        assert!(worker.stdin.is_none());
        assert!(worker.stdout.is_none());
    }

    #[test]
    fn new_accepts_empty_args() {
        let worker = ChildProcessWorker::new("cmd", vec![]);
        assert!(worker.args.is_empty());
    }

    #[test]
    fn new_accepts_multiple_args() {
        let worker = ChildProcessWorker::new(
            "cmd",
            vec!["--flag".to_string(), "value".to_string(), "extra".to_string()],
        );
        assert_eq!(worker.args.len(), 3);
        assert_eq!(worker.args[2], "extra");
    }

    // -----------------------------------------------------------------------
    // Request serialization (tagged JSONL protocol)
    // -----------------------------------------------------------------------

    #[test]
    fn request_prompt_serializes_to_tagged_json() {
        let req = Request::Prompt { id: 1, text: "hi".to_string() };
        let json = serde_json::to_string(&req).unwrap();
        // type tag is renamed to "prompt"
        assert!(json.contains(r#""type":"prompt""#));
        assert!(json.contains(r#""id":1"#));
        assert!(json.contains(r#""text":"hi""#));
    }

    #[test]
    fn request_steer_serializes_with_correct_tag() {
        let req = Request::Steer { id: 5, msg: "redirect".to_string() };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""type":"steer""#));
        assert!(json.contains(r#""msg":"redirect""#));
    }

    #[test]
    fn request_state_serializes_with_correct_tag() {
        let req = Request::State { id: 9 };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""type":"state""#));
        assert!(json.contains(r#""id":9"#));
    }

    #[test]
    fn request_dispose_serializes_with_correct_tag() {
        let req = Request::Dispose { id: 7 };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""type":"dispose""#));
        assert!(json.contains(r#""id":7"#));
    }

    // -----------------------------------------------------------------------
    // Response deserialization (reverse direction of the protocol)
    // -----------------------------------------------------------------------

    #[test]
    fn response_ok_deserializes_from_tagged_json() {
        let json = r#"{"type":"ok","id":42}"#;
        let resp: Response = serde_json::from_str(json).unwrap();
        match resp {
            Response::Ok { id } => assert_eq!(id, 42),
            _ => panic!("expected Ok variant"),
        }
    }

    #[test]
    fn response_prompt_result_deserializes() {
        let json = r#"{"type":"result","id":1,"success":true,"output":"done"}"#;
        let resp: Response = serde_json::from_str(json).unwrap();
        match resp {
            Response::PromptResult { id, success, output } => {
                assert_eq!(id, 1);
                assert!(success);
                assert_eq!(output, "done");
            }
            _ => panic!("expected PromptResult variant"),
        }
    }

    #[test]
    fn response_state_result_deserializes_with_optional_summary() {
        let json = r#"{"type":"state","id":3,"message_count":10,"turn_index":2,"summary":"hi"}"#;
        let resp: Response = serde_json::from_str(json).unwrap();
        match resp {
            Response::StateResult { id, message_count, turn_index, summary } => {
                assert_eq!(id, 3);
                assert_eq!(message_count, 10);
                assert_eq!(turn_index, 2);
                assert_eq!(summary.as_deref(), Some("hi"));
            }
            _ => panic!("expected StateResult variant"),
        }
    }

    #[test]
    fn response_state_result_accepts_null_summary() {
        let json = r#"{"type":"state","id":0,"message_count":0,"turn_index":0,"summary":null}"#;
        let resp: Response = serde_json::from_str(json).unwrap();
        match resp {
            Response::StateResult { summary, .. } => assert!(summary.is_none()),
            _ => panic!("expected StateResult variant"),
        }
    }

    #[test]
    fn response_error_deserializes() {
        let json = r#"{"type":"error","id":99,"message":"boom"}"#;
        let resp: Response = serde_json::from_str(json).unwrap();
        match resp {
            Response::Error { id, message } => {
                assert_eq!(id, 99);
                assert_eq!(message, "boom");
            }
            _ => panic!("expected Error variant"),
        }
    }

    #[test]
    fn response_rejects_unknown_type_tag() {
        let json = r#"{"type":"unknown","id":1}"#;
        let result: Result<Response, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }
}
