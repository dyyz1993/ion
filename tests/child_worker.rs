//! ChildProcessWorker integration test.
//!
//! Tests that a real subprocess worker (the `mock-worker` binary) can be
//! spawned and driven via `ChildProcessWorker`.  We build the binary on the
//! fly to ensure it's up to date.

use std::process::Command;

use ion::worker::Worker;
use ion::worker::child::ChildProcessWorker;

/// Helper: build the mock-worker binary and return its path.
fn build_mock_worker() -> String {
    let output = Command::new("cargo")
        .args(["build", "--bin", "mock-worker", "-q"])
        .output()
        .expect("failed to build mock-worker");
    assert!(
        output.status.success(),
        "mock-worker build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Path to the binary in the target directory
    let target_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("debug")
        .join("mock-worker");

    target_dir.to_str().unwrap().to_string()
}

#[tokio::test]
async fn child_worker_prompt_roundtrip() {
    let bin = build_mock_worker();
    let mut worker = ChildProcessWorker::new(bin.clone(), vec![]);

    worker.connect().await.unwrap();

    let result = worker.prompt("hello from test".into()).await.unwrap();
    assert!(result.success);
    assert!(result.output.contains("mock-echo: hello from test"));

    worker.dispose().await.unwrap();
}

#[tokio::test]
async fn child_worker_state() {
    let bin = build_mock_worker();
    let mut worker = ChildProcessWorker::new(bin, vec![]);

    worker.connect().await.unwrap();

    let state = worker.state().await.unwrap();
    assert_eq!(state.message_count, 0);

    worker.dispose().await.unwrap();
}

#[tokio::test]
async fn child_worker_steer_and_dispose() {
    let bin = build_mock_worker();
    let mut worker = ChildProcessWorker::new(bin, vec![]);

    worker.connect().await.unwrap();
    worker.steer("navigate somewhere".into()).await.unwrap();
    worker.dispose().await.unwrap();
}
