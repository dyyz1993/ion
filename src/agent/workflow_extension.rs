//! WorkflowExtension — 内核级 workflow gate 校验引擎
//!
//! 当 agent 的 `.md` frontmatter 包含 `workflow:` 定义时，
//! 本 extension 会在 LLM 决定 Stop 时自动执行 gate 命令。
//! Gate 不通过 → 注入失败原因 → 强制继续循环（和 retry_on_no_tool_use 一样可靠）。
//!
//! 用法（在 agent .md 的 frontmatter 里）:
//! ```yaml
//! workflow:
//!   gate_command: "ls hello.py 2>/dev/null && echo PASS || echo FAIL"
//!   gate_expected: "PASS"
//!   max_retries: 3
//! ```

use super::extension::{Extension, GateDecision, TurnContext};
use super::error::AgentResult;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

/// Workflow gate 配置（从 agent .md frontmatter 解析）
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct WorkflowGateConfig {
    /// Gate bash 命令（在 agent 的 cwd 执行）
    pub gate_command: String,
    /// 期望在命令输出中看到的字符串（包含 = PASS）
    #[serde(default = "default_expected")]
    pub gate_expected: String,
    /// 最大重试次数（默认 3）
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
}

fn default_expected() -> String { "PASS".into() }
fn default_max_retries() -> u32 { 3 }

/// WorkflowExtension — 内核级 gate 校验
///
/// 注册后，每当 LLM 决定 Stop，本 extension 会:
/// 1. 执行 gate_command（bash）
/// 2. 检查输出是否包含 gate_expected
/// 3. 不包含 → 返回 RetryWith("GATE FAILED: <reason>")，内核强制继续
/// 4. 超过 max_retries → 返回 Allow（放行，避免无限循环）
pub struct WorkflowExtension {
    config: WorkflowGateConfig,
    retry_count: AtomicU32,
    /// 标记 gate 是否已通过（通过后不再检查）
    passed: Mutex<bool>,
}

impl WorkflowExtension {
    pub fn new(config: WorkflowGateConfig) -> Self {
        Self {
            config,
            retry_count: AtomicU32::new(0),
            passed: Mutex::new(false),
        }
    }
}

#[async_trait::async_trait]
impl Extension for WorkflowExtension {
    fn name(&self) -> &str { "workflow" }

    async fn on_gate_check(&self, _ctx: &TurnContext) -> AgentResult<GateDecision> {
        // 已通过 → 放行
        if let Ok(passed) = self.passed.lock() && *passed {
            return Ok(GateDecision::Allow);
        }

        // 超过最大重试 → 放行（避免无限循环）
        // 注意：max_retries 要足够大（workflow 有 10 stage，每个 stage 可能多个 turn）。
        // 如果 max_retries 太小（比如 30），wf 在跑前几个 stage 时就用完了，
        // 后续 gate 永久放行，wf 提前宣告 PIPELINE COMPLETE。
        let retries = self.retry_count.fetch_add(1, Ordering::SeqCst);
        // 默认上限 100（够 10 stage workflow 每个跑 10 turn）
        let effective_max = if self.config.max_retries == 0 { 100 } else { self.config.max_retries };
        if retries >= effective_max {
            tracing::warn!(
                "Workflow gate: max retries ({}) exceeded, allowing stop",
                effective_max
            );
            return Ok(GateDecision::Allow);
        }

        // 执行 gate 命令
        let output = std::process::Command::new("bash")
            .arg("-c")
            .arg(&self.config.gate_command)
            .output();

        let output_str = match output {
            Ok(o) => {
                let stdout = String::from_utf8_lossy(&o.stdout).to_string();
                let stderr = String::from_utf8_lossy(&o.stderr).to_string();
                if stderr.is_empty() { stdout } else { format!("{stdout}\n{stderr}") }
            }
            Err(e) => format!("gate command failed to execute: {e}"),
        };

        // 检查期望字符串
        if output_str.contains(&self.config.gate_expected) {
            tracing::info!("Workflow gate PASSED (attempt {})", retries + 1);
            if let Ok(mut passed) = self.passed.lock() {
                *passed = true;
            }
            Ok(GateDecision::Allow)
        } else {
            let preview: String = output_str.trim().chars().take(200).collect();
            tracing::warn!(
                "Workflow gate FAILED (attempt {}/{}): expected '{}', got: {}",
                retries + 1, self.config.max_retries,
                self.config.gate_expected,
                preview
            );
            Ok(GateDecision::RetryWith(format!(
                "GATE CHECK FAILED (attempt {}/{}).\n\
                 Gate command: `{}`\n\
                 Expected output to contain: '{}'\n\
                 Actual output: {}\n\
                 Fix the issue so the gate passes. Do NOT just say it's done — make the gate command produce the expected output.",
                retries + 1,
                self.config.max_retries,
                self.config.gate_command,
                self.config.gate_expected,
                preview,
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::extension::TurnContext;

    fn make_turn_ctx() -> TurnContext {
        TurnContext {
            turn_index: 0,
            messages: vec![],
            has_tool_calls: false,
            stop_reason: Some("Stop".into()),
        }
    }

    #[test]
    fn test_workflow_gate_config_defaults() {
        let config: WorkflowGateConfig = serde_yaml::from_str(
            "gate_command: \"echo PASS\""
        ).unwrap();
        assert_eq!(config.gate_expected, "PASS");
        assert_eq!(config.max_retries, 3);
    }

    #[test]
    fn test_workflow_gate_config_custom() {
        let config: WorkflowGateConfig = serde_yaml::from_str(
            "gate_command: \"ls hello.py\"\ngate_expected: \"hello.py\"\nmax_retries: 5"
        ).unwrap();
        assert_eq!(config.gate_expected, "hello.py");
        assert_eq!(config.max_retries, 5);
    }

    #[tokio::test]
    async fn test_gate_pass_when_command_succeeds() {
        // Gate: echo PASS → output contains "PASS" → Allow
        let ext = WorkflowExtension::new(WorkflowGateConfig {
            gate_command: "echo PASS".into(),
            gate_expected: "PASS".into(),
            max_retries: 3,
        });
        let decision = ext.on_gate_check(&make_turn_ctx()).await.unwrap();
        assert!(matches!(decision, GateDecision::Allow));
    }

    #[tokio::test]
    async fn test_gate_fail_returns_retry_with() {
        // Gate: echo FAIL → output does NOT contain "PASS" → RetryWith
        let ext = WorkflowExtension::new(WorkflowGateConfig {
            gate_command: "echo FAIL".into(),
            gate_expected: "PASS".into(),
            max_retries: 3,
        });
        let decision = ext.on_gate_check(&make_turn_ctx()).await.unwrap();
        match decision {
            GateDecision::RetryWith(msg) => {
                assert!(msg.contains("GATE CHECK FAILED"));
                assert!(msg.contains("echo FAIL"));
            }
            _ => panic!("expected RetryWith, got Allow"),
        }
    }

    #[tokio::test]
    async fn test_gate_retries_exhausted_allows_stop() {
        // After max_retries, gate should Allow (avoid infinite loop)
        let ext = WorkflowExtension::new(WorkflowGateConfig {
            gate_command: "echo NOPE".into(),
            gate_expected: "YES".into(),
            max_retries: 2,
        });
        // First 2 attempts: RetryWith
        let d1 = ext.on_gate_check(&make_turn_ctx()).await.unwrap();
        assert!(matches!(d1, GateDecision::RetryWith(_)));
        let d2 = ext.on_gate_check(&make_turn_ctx()).await.unwrap();
        assert!(matches!(d2, GateDecision::RetryWith(_)));
        // 3rd attempt (exceeds max_retries=2): Allow
        let d3 = ext.on_gate_check(&make_turn_ctx()).await.unwrap();
        assert!(matches!(d3, GateDecision::Allow));
    }

    #[tokio::test]
    async fn test_gate_passes_after_success() {
        // Once gate passes, subsequent checks should immediately Allow
        let ext = WorkflowExtension::new(WorkflowGateConfig {
            gate_command: "echo OK".into(),
            gate_expected: "OK".into(),
            max_retries: 3,
        });
        // First check: pass
        let d1 = ext.on_gate_check(&make_turn_ctx()).await.unwrap();
        assert!(matches!(d1, GateDecision::Allow));
        // Second check: should still Allow (already passed)
        let d2 = ext.on_gate_check(&make_turn_ctx()).await.unwrap();
        assert!(matches!(d2, GateDecision::Allow));
    }
}
