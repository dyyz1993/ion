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
