use crate::error::{IonError, IonResult};
use crate::types::{SessionState, TaskResult};
use crate::worker::Worker;
use async_trait::async_trait;
use std::time::Duration;

/// A fake worker useful for testing the orchestration layer without spawning
/// real subprocesses.
///
/// `StubWorker` implements the `Worker` trait with configurable behavior:
/// - `echo_prompt`: simply echoes back the input as `TaskResult`.
/// - `delay`: simulates a slow task (milliseconds).
/// - `fail_after`: number of `prompt()` calls before returning an error
///   (useful for testing crash recovery in the pool).
/// - `should_fail_next`: if set, the next `prompt()` call will fail.
#[derive(Clone, Debug)]
pub struct StubWorker {
    name: String,
    delay: Duration,
    prompt_count: u64,
    fail_after: u64, // 0 = never fail
    should_fail_next: bool,
}

impl StubWorker {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            delay: Duration::from_millis(10),
            prompt_count: 0,
            fail_after: 0,
            should_fail_next: false,
        }
    }

    /// Set a delay for each `prompt()` call.
    pub fn with_delay(mut self, ms: u64) -> Self {
        self.delay = Duration::from_millis(ms);
        self
    }

    /// Fail after N successful prompt calls.
    pub fn with_fail_after(mut self, n: u64) -> Self {
        self.fail_after = n;
        self
    }

    /// Set whether the *next* `prompt()` call should fail.
    pub fn set_should_fail(&mut self, val: bool) {
        self.should_fail_next = val;
    }
}

#[async_trait]
impl Worker for StubWorker {
    async fn connect(&mut self) -> IonResult<()> {
        tracing::debug!("[{}] connected", self.name);
        Ok(())
    }

    async fn prompt(&mut self, text: String) -> IonResult<TaskResult> {
        self.prompt_count += 1;

        if self.should_fail_next {
            self.should_fail_next = false;
            return Err(IonError::Worker(format!(
                "stub {} simulated failure on call #{}",
                self.name, self.prompt_count
            )));
        }

        if self.fail_after > 0 && self.prompt_count > self.fail_after {
            return Err(IonError::Worker(format!(
                "stub {} exceeded fail_after={}",
                self.name, self.fail_after
            )));
        }

        // Simulate processing time
        tokio::time::sleep(self.delay).await;

        Ok(TaskResult::ok(format!(
            "[stub {}] echo: {}",
            self.name, text
        )))
    }

    async fn steer(&mut self, msg: String) -> IonResult<()> {
        tracing::debug!("[{}] steer: {}", self.name, msg);
        Ok(())
    }

    async fn state(&mut self) -> IonResult<SessionState> {
        Ok(SessionState {
            message_count: self.prompt_count,
            turn_index: 0,
            summary: Some(format!("stub worker {}", self.name)),
        })
    }

    async fn dispose(&mut self) -> IonResult<()> {
        tracing::debug!("[{}] disposed", self.name);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stub_worker_echoes() {
        let mut w = StubWorker::new("test");
        w.connect().await.unwrap();
        let result = w.prompt("hello world".into()).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("echo: hello world"));
    }

    #[tokio::test]
    async fn stub_worker_failure() {
        let mut w = StubWorker::new("test").with_fail_after(0);
        w.set_should_fail(true);
        let result = w.prompt("fail".into()).await;
        assert!(result.is_err());
    }
}
