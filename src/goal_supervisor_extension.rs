//! Goal Supervisor Extension — B1 Stage A (data structures + goal_set tool skeleton).
//!
//! This module provides the data model and a first tool (`goal_set`) for the
//! Goal Supervisor, an iteration-control extension that drives an agent toward
//! an objective, runs verification checks, and decides retry vs. stop.
//!
//! Stage A scope (this file):
//!   - All data structures (Check, CheckResult, GoalState, GoalSupervisorConfig, ...)
//!   - `GoalSupervisorExtension` (AgentExtension): name = "goal_supervisor",
//!     `on_agent_end` is a stub that returns `Ok(())` — it will be wired up in
//!     Stage C to actually run the verification checks.
//!   - `GoalSetTool` (Tool): name = "goal_set" — lets the agent declare a new
//!     goal (objective + checks). Setting a new goal overrides any previous goal,
//!     which is treated as cancelled.
//!
//! See docs/design/GOAL_SUPERVISOR.md and GOAL_SUPERVISOR_B1_TASK.md for the
//! full design (Stages A/B/C). This file implements Stage A only.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::agent::error::{AgentError, AgentResult};
use crate::agent::extension::Extension;
use crate::agent::tool::Tool;

// ===========================================================================
// Data structures (B1_TASK.md section 2.1)
// ===========================================================================

/// Classification of a verification check.
///
/// - `Ci`: the check is a continuous-integration style command (e.g. `cargo test`).
///   These are run on every verification cycle.
/// - `Contingency`: the check is a fallback/safety-net command that only runs
///   if the CI-style checks do not conclusively pass.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckType {
    Ci,
    Contingency,
}

/// How a check's success is determined.
///
/// Uses an internally-tagged representation (`#[serde(tag = "kind")]`) so each
/// variant serializes as an object like `{"kind": "exit_code", "expected": 0}`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind")]
#[serde(rename_all = "snake_case")]
pub enum PassCriteria {
    /// The check command must exit with exactly `expected`.
    ExitCode { expected: i32 },
    /// The check command's stdout must be empty when matched against `pattern`.
    /// (Semantics: "grep returns nothing" => pass. Useful for "no TODOs left".)
    GrepEmpty { pattern: String },
    /// The given `path` must exist on the filesystem.
    FileExists { path: String },
}

/// A single verification check attached to a goal.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Check {
    /// Human-readable name of the check (e.g. "unit tests").
    pub name: String,
    /// Whether this is a CI-style or contingency check.
    pub check_type: CheckType,
    /// Why this check exists (helps the agent / reviewer understand intent).
    pub rationale: String,
    /// Shell command to execute for this check.
    pub command: String,
    /// Criteria for deciding pass/fail of the command's output.
    pub pass_criteria: PassCriteria,
    /// If true, this check must pass for the goal to be considered complete.
    /// If false, the check is advisory (failure does not block completion).
    pub must_pass: bool,
}

/// Outcome of running a single check.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    /// The check passed.
    Pass,
    /// The check ran but failed (e.g. wrong exit code).
    Fail,
    /// The check could not be run (e.g. command not found, timeout).
    Error,
    /// The check was not executed (e.g. a preceding must_pass check failed).
    Skipped,
}

/// Evidence captured while running a check.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Evidence {
    /// Exit code of the check command, if it ran to completion.
    pub exit_code: Option<i32>,
    /// Truncated stdout from the check command.
    pub stdout_excerpt: Option<String>,
    /// Path to a saved artifact (e.g. full log file), if any.
    pub artifact_path: Option<String>,
    /// Pattern matches found (for GrepEmpty criteria, the lines that matched).
    pub matches: Option<Vec<String>>,
}

/// The recorded result of one check execution.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CheckResult {
    /// Name of the check this result belongs to.
    pub name: String,
    /// Pass/Fail/Error/Skipped status.
    pub status: CheckStatus,
    /// Captured evidence (None if the check was skipped).
    pub evidence: Option<Evidence>,
    /// How long the check took to run, in milliseconds.
    pub duration_ms: u64,
    /// Optional human-readable reason for the status (especially for Fail/Error).
    pub reason: Option<String>,
}

/// Lifecycle status of a goal.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    /// The agent is actively working toward the objective.
    Running,
    /// Verification checks are currently being executed.
    Checking,
    /// All must_pass checks passed — the goal is achieved.
    Complete,
    /// The iteration/cost/duration budget was exhausted without success.
    Exhausted,
    /// The goal cannot make progress (e.g. a hard error) and needs human input.
    Blocked,
    /// The goal was superseded by a newer goal_set call or explicitly cancelled.
    Cancelled,
}

/// Full state of the current goal.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct GoalState {
    /// Unique id for this goal instance (UUID v4, generated on goal_set).
    pub goal_id: String,
    /// The natural-language objective the agent must achieve.
    pub objective: String,
    /// Verification checks attached to this goal.
    pub checks: Vec<Check>,
    /// Current lifecycle status.
    pub status: GoalStatus,
    /// How many iteration cycles have run so far.
    pub iteration_count: u32,
    /// When the goal was started (epoch seconds, "epoch:NNN" format).
    pub started_at: String,
    /// Cumulative estimated cost across all iterations, in USD.
    pub total_cost_usd: f64,
    /// The last action plan proposed by the agent (for repetition detection).
    pub last_action_plan: Option<String>,
}

/// Configuration for the Goal Supervisor.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct GoalSupervisorConfig {
    /// Master switch. When false, the supervisor is inert.
    pub enabled: bool,
    /// If true, run verification checks when the agent ends a turn/run.
    pub check_on_agent_end: bool,
    /// Maximum number of iteration cycles before declaring Exhausted.
    pub max_iterations: u32,
    /// Hard wall-clock budget, in minutes.
    pub max_total_duration_min: u32,
    /// Hard cost budget, in USD.
    pub max_total_cost_usd: f64,
    /// If the agent repeats the same action plan this many times, declare Blocked.
    pub repetition_threshold: u32,
    /// Delay (ms) between iterations, to avoid hammering the provider.
    pub delay_ms: u64,
}

impl Default for GoalSupervisorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            check_on_agent_end: true,
            max_iterations: 10,
            max_total_duration_min: 60,
            max_total_cost_usd: 1.0,
            repetition_threshold: 3,
            delay_ms: 2000,
        }
    }
}

// ===========================================================================
// Shared state
// ===========================================================================

/// Shared goal state. Both the extension and the tools hold a clone of this Arc
/// so they observe the same `GoalState`. Modeled after `SharedPlan` in
/// `plan_extension.rs` / `plan_tool.rs`.
pub type SharedGoalState = Arc<Mutex<Option<GoalState>>>;

// ===========================================================================
// GoalSupervisorExtension — AgentExtension (Stage C will wire up on_agent_end)
// ===========================================================================

/// The Goal Supervisor extension.
///
/// Stage A: only stores the goal state and config; `on_agent_end` is a no-op
/// stub. Stage C will implement the check-running + retry/stop logic here.
pub struct GoalSupervisorExtension {
    /// Shared with `GoalSetTool` (and future goal_* tools).
    pub state: SharedGoalState,
    /// Supervisor configuration (limits, thresholds).
    pub config: GoalSupervisorConfig,
    /// Session id this supervisor instance is bound to (for logging/RPC).
    pub session_id: Option<String>,
}

impl GoalSupervisorExtension {
    /// Create a new supervisor with default config and fresh (empty) state.
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(None)),
            config: GoalSupervisorConfig::default(),
            session_id: None,
        }
    }

    /// Attach a specific session id (e.g. read from disk on startup).
    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// Override the default config.
    pub fn with_config(mut self, config: GoalSupervisorConfig) -> Self {
        self.config = config;
        self
    }
}

impl Default for GoalSupervisorExtension {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Extension for GoalSupervisorExtension {
    fn name(&self) -> &str {
        "goal_supervisor"
    }

    /// Stage C will run the verification checks here and decide retry vs. stop.
    /// For Stage A this is a stub.
    async fn on_agent_end(&self, _ctx: &crate::agent::agent_loop::AgentContext) -> AgentResult<()> {
        // Intentionally a no-op for Stage A. Check execution is wired up in
        // Stage C (see GOAL_SUPERVISOR_B1_TASK.md).
        Ok(())
    }
}

// ===========================================================================
// GoalSetTool — lets the agent declare / replace the current goal
// ===========================================================================

/// Tool that sets the supervisor's goal.
///
/// Setting a new goal overrides (cancels) any previous goal: the previous
/// `GoalState` is replaced wholesale. The new goal starts in `Running` status
/// with `iteration_count = 0`.
pub struct GoalSetTool(pub SharedGoalState);

#[async_trait]
impl Tool for GoalSetTool {
    fn name(&self) -> &str {
        "goal_set"
    }

    fn description(&self) -> &str {
        "Declare a goal for the Goal Supervisor: an objective plus verification \
         checks. Setting a new goal cancels (overrides) any previous goal. The \
         supervisor will iterate toward the objective and run the checks to \
         decide completion, retry, or exhaustion."
    }

    /// JSON schema for the goal_set arguments (B1_TASK.md section 2.3).
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "objective": {
                    "type": "string",
                    "description": "Natural-language description of the goal the agent must achieve."
                },
                "checks": {
                    "type": "array",
                    "description": "Verification checks. Each check has: name, check_type \
                                    (\"ci\" or \"contingency\"), rationale, command, pass_criteria, \
                                    must_pass. pass_criteria is an object with a \"kind\" of \
                                    \"exit_code\" ({expected}), \"grep_empty\" ({pattern}), or \
                                    \"file_exists\" ({path}).",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": {"type": "string"},
                            "check_type": {"type": "string", "enum": ["ci", "contingency"]},
                            "rationale": {"type": "string"},
                            "command": {"type": "string"},
                            "pass_criteria": {"type": "object"},
                            "must_pass": {"type": "boolean"}
                        },
                        "required": ["name", "check_type", "rationale", "command", "pass_criteria", "must_pass"]
                    }
                }
            },
            "required": ["objective"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _rt: &dyn crate::runtime::Runtime,
    ) -> AgentResult<String> {
        // Required: objective.
        let objective = args
            .get("objective")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentError::Tool("goal_set: missing 'objective'".into()))?
            .to_string();

        // Optional: checks (default empty). Parse each check defensively; a
        // malformed check is reported back as a tool error rather than panicking.
        let mut checks: Vec<Check> = Vec::new();
        if let Some(arr) = args.get("checks").and_then(|v| v.as_array()) {
            for (i, item) in arr.iter().enumerate() {
                let check: Check = serde_json::from_value(item.clone()).map_err(|e| {
                    AgentError::Tool(format!("goal_set: invalid check at index {i}: {e}"))
                })?;
                checks.push(check);
            }
        }

        let goal_id = uuid::Uuid::new_v4().to_string();
        let started_at = now_epoch_string();

        let new_state = GoalState {
            goal_id: goal_id.clone(),
            objective: objective.clone(),
            checks,
            status: GoalStatus::Running,
            iteration_count: 0,
            started_at: started_at.clone(),
            total_cost_usd: 0.0,
            last_action_plan: None,
        };

        // Replace any previous goal (the old one is implicitly cancelled).
        let previous_id = {
            let mut guard = self
                .0
                .lock()
                .map_err(|e| AgentError::Tool(format!("goal_set: state lock poisoned: {e}")))?;
            let prev = guard.as_ref().map(|s| s.goal_id.clone());
            *guard = Some(new_state);
            prev
        };

        let confirmation = {
            // Compute check count before building JSON (json! macro can't host expression blocks).
            let check_count = {
                let g = self.0.lock().map_err(|e| AgentError::Tool(format!("goal_set: state lock poisoned: {e}")))?;
                g.as_ref().map(|s| s.checks.len()).unwrap_or(0)
            };
            serde_json::json!({
                "status": "ok",
                "goal_id": goal_id,
                "objective": objective,
                "started_at": started_at,
                "previous_goal_id": previous_id,
                "previous_cancelled": previous_id.is_some(),
                "check_count": check_count,
            })
        };

        Ok(confirmation.to_string())
    }
}

// ===========================================================================
// Helpers
// ===========================================================================

/// Return the current time as an "epoch:NNN" string (seconds since UNIX_EPOCH).
/// Mirrors the timestamp format used by monitor_extension.rs.
fn now_epoch_string() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => format!("epoch:{}", d.as_secs()),
        Err(_) => "epoch:0".into(),
    }
}

// ===========================================================================
// Unit tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::LocalRuntime;

    fn rt() -> LocalRuntime {
        LocalRuntime::new()
    }

    #[test]
    fn test_config_defaults() {
        let cfg = GoalSupervisorConfig::default();
        assert!(cfg.enabled, "enabled should default to true");
        assert!(
            cfg.check_on_agent_end,
            "check_on_agent_end should default to true"
        );
        assert_eq!(cfg.max_iterations, 10, "max_iterations default");
        assert_eq!(cfg.max_total_duration_min, 60, "max_total_duration_min default");
        assert_eq!(cfg.max_total_cost_usd, 1.0, "max_total_cost_usd default");
        assert_eq!(cfg.repetition_threshold, 3, "repetition_threshold default");
        assert_eq!(cfg.delay_ms, 2000, "delay_ms default");
    }

    #[tokio::test]
    async fn test_goal_set_overrides() {
        // Two goal_set calls share the same SharedGoalState. The second call
        // must cancel the first: only the second goal remains in state, and
        // the tool result reports the previous goal id + cancelled flag.
        let shared: SharedGoalState = Arc::new(Mutex::new(None));
        let tool = GoalSetTool(shared.clone());

        // Set goal A.
        let args_a = serde_json::json!({
            "objective": "goal A",
            "checks": [{
                "name": "ci-a",
                "check_type": "ci",
                "rationale": "first check",
                "command": "true",
                "pass_criteria": {"kind": "exit_code", "expected": 0},
                "must_pass": true
            }]
        });
        let res_a = tool.execute(args_a, &rt()).await.expect("goal A set ok");
        let json_a: serde_json::Value = serde_json::from_str(&res_a).expect("valid json");
        assert_eq!(json_a["status"], "ok");
        assert!(json_a["previous_cancelled"].as_bool() == Some(false));
        let goal_a_id = json_a["goal_id"].as_str().unwrap().to_string();

        // State now holds goal A.
        {
            let guard = shared.lock().unwrap();
            let state = guard.as_ref().expect("goal A present");
            assert_eq!(state.objective, "goal A");
            assert_eq!(state.status, GoalStatus::Running);
            assert_eq!(state.checks.len(), 1);
            assert_eq!(state.goal_id, goal_a_id);
        }

        // Set goal B — overrides A.
        let args_b = serde_json::json!({
            "objective": "goal B",
            "checks": []
        });
        let res_b = tool.execute(args_b, &rt()).await.expect("goal B set ok");
        let json_b: serde_json::Value = serde_json::from_str(&res_b).expect("valid json");
        assert_eq!(json_b["status"], "ok");
        assert_eq!(json_b["previous_goal_id"].as_str(), Some(goal_a_id.as_str()));
        assert!(json_b["previous_cancelled"].as_bool() == Some(true));

        // Only goal B remains.
        {
            let guard = shared.lock().unwrap();
            let state = guard.as_ref().expect("goal B present");
            assert_eq!(state.objective, "goal B", "objective replaced by goal B");
            assert_eq!(state.goal_id, json_b["goal_id"].as_str().unwrap());
            assert_ne!(state.goal_id, goal_a_id, "goal B has a different id than A");
            assert!(state.checks.is_empty(), "goal B has no checks");
            assert_eq!(state.status, GoalStatus::Running);
            assert_eq!(state.iteration_count, 0);
        }
    }

    #[test]
    fn test_check_serialization() {
        // A Check should round-trip through serialize -> deserialize unchanged,
        // including the internally-tagged PassCriteria.
        let original = Check {
            name: "unit tests".into(),
            check_type: CheckType::Ci,
            rationale: "ensure no regressions".into(),
            command: "cargo test".into(),
            pass_criteria: PassCriteria::ExitCode { expected: 0 },
            must_pass: true,
        };

        let json = serde_json::to_string(&original).expect("serialize");
        let parsed: Check = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(parsed.name, original.name);
        assert_eq!(parsed.check_type, original.check_type);
        assert_eq!(parsed.rationale, original.rationale);
        assert_eq!(parsed.command, original.command);
        assert_eq!(parsed.pass_criteria, original.pass_criteria);
        assert_eq!(parsed.must_pass, original.must_pass);

        // Verify the PassCriteria tag renders as expected.
        assert!(
            json.contains("\"kind\":\"exit_code\""),
            "PassCriteria should be internally tagged with kind, got: {json}"
        );
        assert!(json.contains("\"expected\":0"), "expected field present");

        // Round-trip the other two PassCriteria variants too.
        for pc in [
            PassCriteria::GrepEmpty {
                pattern: "TODO".into(),
            },
            PassCriteria::FileExists {
                path: "/tmp/out".into(),
            },
        ] {
            let c = Check {
                name: "x".into(),
                check_type: CheckType::Contingency,
                rationale: "r".into(),
                command: "c".into(),
                pass_criteria: pc.clone(),
                must_pass: false,
            };
            let j = serde_json::to_string(&c).unwrap();
            let back: Check = serde_json::from_str(&j).unwrap();
            assert_eq!(back.pass_criteria, pc, "round-trip for {:?}", pc);
        }
    }

    #[test]
    fn test_goal_set_tool_name_and_params() {
        let shared: SharedGoalState = Arc::new(Mutex::new(None));
        let tool = GoalSetTool(shared);
        assert_eq!(tool.name(), "goal_set");
        let params = tool.parameters();
        // objective is required; checks is optional.
        assert_eq!(params["type"], "object");
        assert!(params["required"].as_array().unwrap().iter().any(|v| v == "objective"));
    }

    #[tokio::test]
    async fn test_goal_set_missing_objective_errors() {
        let shared: SharedGoalState = Arc::new(Mutex::new(None));
        let tool = GoalSetTool(shared);
        let res = tool.execute(serde_json::json!({}), &rt()).await;
        assert!(res.is_err(), "missing objective must error");
    }

    #[tokio::test]
    async fn test_goal_set_bad_check_errors() {
        let shared: SharedGoalState = Arc::new(Mutex::new(None));
        let tool = GoalSetTool(shared);
        // check missing required fields -> deserialization error.
        let args = serde_json::json!({
            "objective": "ok",
            "checks": [{"name": "bad"}]
        });
        let res = tool.execute(args, &rt()).await;
        assert!(res.is_err(), "malformed check must error");
    }
}
