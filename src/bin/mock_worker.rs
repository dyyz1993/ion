/// A mock worker that speaks JSONL over stdin/stdout.
///
/// This is a standalone binary that the `ChildProcessWorker` can spawn.
/// It reads JSONL commands from stdin and writes JSONL responses to stdout.
///
/// Protocol:
///   {"type":"register","id":1}
///   {"type":"ok","id":1}
///   {"type":"prompt","id":2,"text":"..."}
///   {"type":"result","id":2,"success":true,"output":"..."}
///   {"type":"steer","id":3,"msg":"..."}
///   {"type":"ok","id":3}
///   {"type":"state","id":4}
///   {"type":"state","id":4,"message_count":0,"turn_index":0,"summary":null}
///   {"type":"dispose","id":5}
///   {"type":"ok","id":5}
use serde::{Deserialize, Serialize};
use std::io::{self, BufRead, Write};
use std::time::Duration;

#[allow(dead_code)]
#[derive(Deserialize)]
#[serde(tag = "type")]
enum Request {
    #[serde(rename = "register")]
    Register { id: u64 },
    #[serde(rename = "prompt")]
    Prompt { id: u64, text: String },
    #[serde(rename = "steer")]
    Steer { id: u64, msg: String },
    #[serde(rename = "state")]
    State { id: u64 },
    #[serde(rename = "dispose")]
    Dispose { id: u64 },
}

#[derive(Serialize)]
#[serde(tag = "type")]
enum Response {
    #[serde(rename = "ok")]
    Ok { id: u64 },
    #[serde(rename = "result")]
    Result {
        id: u64,
        success: bool,
        output: String,
    },
    #[serde(rename = "state")]
    State {
        id: u64,
        message_count: u64,
        turn_index: u64,
        summary: Option<String>,
    },
    #[serde(rename = "error")]
    Error { id: u64, message: String },
}

fn send(resp: &Response) {
    let line = serde_json::to_string(resp).unwrap();
    let mut stdout = io::stdout().lock();
    let _ = writeln!(stdout, "{line}");
    let _ = stdout.flush();
}

fn main() {
    let stdin = io::stdin().lock();
    for line in stdin.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }

        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                // Send error response for id 0
                send(&Response::Error {
                    id: 0,
                    message: format!("invalid JSON: {e}"),
                });
                continue;
            }
        };

        match req {
            Request::Register { id } => {
                send(&Response::Ok { id });
            }
            Request::Prompt { id, text } => {
                // Simulate a tiny bit of work
                std::thread::sleep(Duration::from_millis(10));
                send(&Response::Result {
                    id,
                    success: true,
                    output: format!("mock-echo: {text}"),
                });
            }
            Request::Steer { id, msg: _ } => {
                send(&Response::Ok { id });
            }
            Request::State { id } => {
                send(&Response::State {
                    id,
                    message_count: 0,
                    turn_index: 0,
                    summary: Some("mock-worker".into()),
                });
            }
            Request::Dispose { id } => {
                send(&Response::Ok { id });
                break;
            }
        }
    }
}
