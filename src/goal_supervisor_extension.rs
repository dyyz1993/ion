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
    /// Recent tool calls for drift monitoring (Task 4): (tool_name, target_file_or_cmd_summary).
    #[serde(default)]
    pub recent_tools: Vec<(String, String)>,
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

    /// Inject a shared state handle (e.g. the one already given to GoalSetTool).
    /// This lets the Extension and the Tool see the same goal state.
    pub fn with_shared_state(mut self, state: SharedGoalState) -> Self {
        self.state = state;
        self
    }

    // -----------------------------------------------------------------------
    // Check execution (Stage B)
    // -----------------------------------------------------------------------

    /// Run every check attached to the current goal, in declaration order.
    ///
    /// The checks are snapshotted out of the shared state up front so the mutex
    /// is not held across the (potentially long-running) command executions.
    ///
    /// Returns an error if no goal is currently set.
    pub async fn run_all_checks(&self) -> Result<Vec<CheckResult>, String> {
        // Clone the checks out of shared state without holding the lock across awaits.
        let checks: Vec<Check> = {
            let guard = self
                .state
                .lock()
                .map_err(|e| format!("run_all_checks: state lock poisoned: {e}"))?;
            match guard.as_ref() {
                Some(state) => state.checks.clone(),
                None => return Err("run_all_checks: no goal is set".into()),
            }
        };

        let mut results = Vec::with_capacity(checks.len());
        for check in &checks {
            let result = Self::run_single_check(check).await?;
            results.push(result);
        }
        Ok(results)
    }

    /// Execute a single check and evaluate its pass criteria.
    ///
    /// Steps:
    ///   1. Run `check.command` via `sh -c` using `tokio::process::Command`,
    ///      capturing stdout/stderr/exit_code.
    ///   2. Collect `Evidence` (exit_code, a stdout excerpt of the first 2000
    ///      chars, an artifact file holding the full stdout, and the grep
    ///      matches for `GrepEmpty`).
    ///   3. Evaluate `check.pass_criteria`.
    ///
    /// If evidence cannot be collected (e.g. the command fails to spawn, or the
    /// artifact file cannot be written), the result is `Fail` with reason
    /// "no evidence".
    pub async fn run_single_check(check: &Check) -> Result<CheckResult, String> {
        let start = std::time::Instant::now();

        // 1. Execute the command through the shell.
        let output = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&check.command)
            .output()
            .await;

        let (exit_code, stdout) = match output {
            Ok(out) => {
                let code = out.status.code().unwrap_or(-1);
                let so = String::from_utf8_lossy(&out.stdout).to_string();
                (code, so)
            }
            Err(_) => {
                // Could not run the command -> no evidence available.
                let duration_ms = start.elapsed().as_millis() as u64;
                return Ok(CheckResult {
                    name: check.name.clone(),
                    status: CheckStatus::Fail,
                    evidence: None,
                    duration_ms,
                    reason: Some("no evidence".into()),
                });
            }
        };

        // 2. Collect evidence.
        // stdout excerpt: first 2000 characters.
        let stdout_excerpt: String = stdout.chars().take(2000).collect();

        // For GrepEmpty, capture the matching lines up front so the evidence is
        // complete and self-describing.
        let matches: Option<Vec<String>> = match &check.pass_criteria {
            PassCriteria::GrepEmpty { pattern } => Some(
                stdout
                    .lines()
                    .filter(|line| line.contains(pattern.as_str()))
                    .map(|line| line.to_string())
                    .collect(),
            ),
            _ => None,
        };

        // Write the full stdout to an artifact file. If this fails we have no
        // usable evidence -> Fail with reason "no evidence".
        let artifact_path = match write_artifact(&check.name, &stdout) {
            Ok(p) => Some(p),
            Err(_) => {
                let duration_ms = start.elapsed().as_millis() as u64;
                return Ok(CheckResult {
                    name: check.name.clone(),
                    status: CheckStatus::Fail,
                    evidence: None,
                    duration_ms,
                    reason: Some("no evidence".into()),
                });
            }
        };

        let evidence = Evidence {
            exit_code: Some(exit_code),
            stdout_excerpt: Some(stdout_excerpt),
            artifact_path,
            matches,
        };

        // 3. Evaluate pass criteria against the command output / evidence.
        let status = match &check.pass_criteria {
            PassCriteria::ExitCode { expected } => {
                if exit_code == *expected {
                    CheckStatus::Pass
                } else {
                    CheckStatus::Fail
                }
            }
            PassCriteria::GrepEmpty { pattern } => {
                // Pass iff no line of stdout contains the pattern.
                let any_match = stdout
                    .lines()
                    .any(|line| line.contains(pattern.as_str()));
                if any_match {
                    CheckStatus::Fail
                } else {
                    CheckStatus::Pass
                }
            }
            PassCriteria::FileExists { path } => {
                if std::path::Path::new(path).exists() {
                    CheckStatus::Pass
                } else {
                    CheckStatus::Fail
                }
            }
        };

        let duration_ms = start.elapsed().as_millis() as u64;
        Ok(CheckResult {
            name: check.name.clone(),
            status,
            evidence: Some(evidence),
            duration_ms,
            reason: None,
        })
    }

    // -----------------------------------------------------------------------
    // State machine + guards (Stage C)
    // -----------------------------------------------------------------------

    /// Return the first guard that trips, or None if all guards pass.
    ///
    /// Guards checked (in order):
    ///   1. max_iterations      — iteration_count >= config.max_iterations
    ///   5. max_total_duration  — elapsed minutes >= config.max_total_duration_min
    ///   6. max_total_cost      — total_cost_usd >= config.max_total_cost_usd
    ///   3. repetitive          — similarity(last_action_plan, current) >= threshold
    ///
    /// Guards 2 (confidence) and 4 (repetition-strategy) are decision-time
    /// checks handled in `inject_continue`, not here.
    pub fn check_guards(&self, current_plan: Option<&str>) -> Option<String> {
        let guard = self.state.lock().ok()?;
        let state = guard.as_ref()?;

        // 1. max_iterations
        if state.iteration_count >= self.config.max_iterations {
            return Some("max_iterations".into());
        }

        // 5. max_total_duration — started_at is "epoch:NNN" (seconds).
        let started_secs = state
            .started_at
            .strip_prefix("epoch:")
            .and_then(|n| n.parse::<u64>().ok())
            .unwrap_or(0);
        let now_secs = now_epoch_ms() / 1000;
        let elapsed_min = now_secs.saturating_sub(started_secs) / 60;
        if elapsed_min >= self.config.max_total_duration_min as u64 {
            return Some("max_duration".into());
        }

        // 6. max_total_cost
        if state.total_cost_usd >= self.config.max_total_cost_usd {
            return Some("max_cost".into());
        }

        // 3. repetitive — repetition_threshold is a count of consecutive
        //    identical action plans. We approximate "identical" via high
        //    text similarity (>= 0.8). Each matching iteration counts as one.
        if let (Some(prev), Some(curr)) = (state.last_action_plan.as_deref(), current_plan) {
            let sim = calculate_similarity(prev, curr);
            if sim >= 0.8 && state.iteration_count >= self.config.repetition_threshold {
                return Some("repetitive".into());
            }
        }

        None
    }

    /// Increment the iteration counter and record the latest action plan.
    pub fn record_iteration(&self, action_plan: Option<String>) {
        if let Ok(mut guard) = self.state.lock() {
            if let Some(state) = guard.as_mut() {
                state.iteration_count += 1;
                state.last_action_plan = action_plan;
            }
        }
    }

    /// Set the goal status (Running / Complete / Exhausted / Blocked / Cancelled).
    pub fn set_status(&self, status: GoalStatus) {
        if let Ok(mut guard) = self.state.lock() {
            if let Some(state) = guard.as_mut() {
                state.status = status;
            }
        }
    }

    // -----------------------------------------------------------------------
    // Progress analysis (Deepening Task 1)
    // -----------------------------------------------------------------------

    /// Analyze recent progress by reading iterations.jsonl history.
    ///
    /// Returns a ProgressReport classifying the trend and giving a recommendation.
    /// Pure heuristic — no LLM call. Called from on_gate_check before RetryWith.
    pub fn analyze_progress(&self, current_plan: Option<&str>) -> ProgressReport {
        let (objective, iteration_count) = {
            let guard = self.state.lock().ok();
            match guard.as_ref().and_then(|g| g.as_ref()) {
                Some(s) => (s.objective.clone(), s.iteration_count),
                None => return ProgressReport::default(),
            }
        };

        // Check drift FIRST (doesn't need history): is the action plan related to the objective?
        let drifting = match current_plan {
            Some(plan) if !plan.is_empty() => {
                calculate_similarity(plan, &objective) < 0.15
            }
            _ => false,
        };
        if drifting {
            return ProgressReport {
                trend: ProgressTrend::Drifting,
                failed_history: vec![],
                recommendation: format!(
                    "Your recent work seems unrelated to the objective: \"{}\". \
                     Re-focus on the goal.",
                    &objective[..objective.len().min(80)]
                ),
            };
        }

        // Check tool-level drift (Task 4): if recent tools are all bash/read with no
        // write/edit, the agent isn't making changes — likely stuck exploring.
        let tool_drift = {
            let guard = self.state.lock().ok();
            match guard.as_ref().and_then(|g| g.as_ref()) {
                Some(s) if s.recent_tools.len() >= 5 => {
                    let recent = &s.recent_tools;
                    let has_write = recent.iter().any(|(name, _)| {
                        name == "write" || name == "edit" || name == "write_file" || name == "edit_file"
                    });
                    let all_bash_read = recent.iter().all(|(name, _)| {
                        name == "bash" || name == "bash_run" || name == "read" || name == "read_file"
                    });
                    !has_write && all_bash_read
                }
                _ => false,
            }
        };
        if tool_drift {
            return ProgressReport {
                trend: ProgressTrend::Drifting,
                failed_history: vec![],
                recommendation: "You've been only reading/running commands without writing \
                                 any code changes. Start implementing the fix."
                    .into(),
            };
        }

        // Read recent failed_checks history from iterations.jsonl.
        let session = self.session_id.as_deref().unwrap_or("default");
        let failed_history = read_recent_failed_history(&dirs_for(session), 3);

        // Need at least 2 data points to classify trend.
        if failed_history.len() < 2 {
            return ProgressReport {
                trend: ProgressTrend::Converging,
                failed_history,
                recommendation: "Early iterations — keep working.".into(),
            };
        }

        // Determine trend from failed_history.
        let trend = classify_trend(&failed_history);

        let recommendation = match trend {
            ProgressTrend::Converging => {
                "Progress looks good — failed checks are decreasing. Keep going.".into()
            }
            ProgressTrend::Oscillating => {
                "Different checks keep failing each iteration (oscillating). \
                 Consider calling goal_refine to adjust or split the checks."
                    .into()
            }
            ProgressTrend::Stagnant => {
                "The same checks keep failing (stagnant). The approach may be wrong — \
                 try a fundamentally different strategy, or call goal_refine to relax checks."
                    .into()
            }
            ProgressTrend::Drifting => {
                format!(
                    "Your recent work seems unrelated to the objective: \"{}\". \
                     Re-focus on the goal.",
                    &objective[..objective.len().min(80)]
                )
            }
        };

        ProgressReport { trend, failed_history, recommendation }
    }

    // -----------------------------------------------------------------------
    // Logging (Stage C)
    // -----------------------------------------------------------------------

    /// Append one iteration record to iterations.jsonl under the goal-run dir.
    ///
    /// Layout: `~/.ion/agent/goal-runs/<session_id>/iterations.jsonl`
    /// If `session_id` is None, falls back to the literal dir name "default".
    pub fn log_iteration(&self, results: &[CheckResult]) -> Result<(), String> {
        let session = self.session_id.as_deref().unwrap_or("default");
        let dir = dirs_for(session);
        std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir goal-runs: {e}"))?;

        let all_passed = results.iter().all(|r| r.status == CheckStatus::Pass);
        let failed: Vec<&str> = results
            .iter()
            .filter(|r| r.status != CheckStatus::Pass)
            .map(|r| r.name.as_str())
            .collect();

        let (goal_id, objective, iter) = {
            let guard = self
                .state
                .lock()
                .map_err(|e| format!("log_iteration: lock: {e}"))?;
            let state = guard
                .as_ref()
                .ok_or_else(|| "log_iteration: no goal".to_string())?;
            (state.goal_id.clone(), state.objective.clone(), state.iteration_count)
        };

        let record = serde_json::json!({
            "iter": iter,
            "timestamp": now_iso8601(),
            "session_id": session,
            "goal_id": goal_id,
            "objective": objective,
            "checks_run": results.iter().map(|r| {
                let status_str = match r.status {
                    CheckStatus::Pass => "pass",
                    CheckStatus::Fail => "fail",
                    CheckStatus::Error => "error",
                    CheckStatus::Skipped => "skipped",
                };
                serde_json::json!({
                    "name": r.name,
                    "status": status_str,
                    "evidence": r.evidence.as_ref().map(|e| serde_json::json!({
                        "exit_code": e.exit_code,
                        "stdout_excerpt": e.stdout_excerpt,
                        "artifact_path": e.artifact_path,
                        "matches": e.matches,
                    })),
                    "duration_ms": r.duration_ms,
                })
            }).collect::<Vec<_>>(),
            "all_passed": all_passed,
            "failed_checks": failed,
        });

        let path = dir.join("iterations.jsonl");
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("open iterations.jsonl: {e}"))?;
        use std::io::Write;
        writeln!(file, "{}", serde_json::to_string(&record).map_err(|e| e.to_string())?)
            .map_err(|e| format!("write iterations.jsonl: {e}"))?;
        Ok(())
    }

    /// Write the final-report.json for this goal run.
    pub fn write_final_report(&self, status: &str, reason: &str) -> Result<(), String> {
        let session = self.session_id.as_deref().unwrap_or("default");
        let dir = dirs_for(session);
        std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir goal-runs: {e}"))?;

        let (goal_id, iterations, started_at) = {
            let guard = self
                .state
                .lock()
                .map_err(|e| format!("write_final_report: lock: {e}"))?;
            let state = guard
                .as_ref()
                .ok_or_else(|| "write_final_report: no goal".to_string())?;
            (state.goal_id.clone(), state.iteration_count, state.started_at.clone())
        };

        // B2-b: auto-generate outcome + diagnosis_hint from status/reason.
        // This gives the goal-evolver actionable context without manual annotation.
        let (outcome, diagnosis_hint) = match status {
            "complete" => (
                "fixed",
                Some(format!(
                    "Goal completed after {} iterations (stopped: {}). \
                     All verification checks passed.",
                    iterations, reason
                )),
            ),
            "exhausted" => {
                let hint = match reason {
                    "max_iterations" => format!(
                        "Agent hit max_iterations ({}) without completing all checks. \
                         Possible causes: checks too strict, agent lacks skill, or \
                         objective too large for single-goal loop. Consider splitting \
                         into sub-goals or strengthening the model tier.",
                        iterations
                    ),
                    "max_duration" => format!(
                        "Goal exceeded time budget after {} iterations. \
                         If each iteration is slow (e.g. long builds), the closed-loop \
                         feedback is too slow. Consider a faster check command or \
                         longer duration budget.",
                        iterations
                    ),
                    "max_cost" => format!(
                        "Goal exceeded cost budget after {} iterations. \
                         Large objectives consume many tokens per iteration. \
                         Consider splitting into smaller sub-goals.",
                        iterations
                    ),
                    "repetitive" => format!(
                        "Agent repeated the same approach {}+ times without progress. \
                         The skill may be missing a required step, or the agent needs \
                         a stronger model to find a different approach.",
                        iterations
                    ),
                    other => format!("Goal exhausted for reason: {} after {} iterations.", other, iterations),
                };
                ("abandoned", Some(hint))
            }
            _ => ("unknown".into(), None),
        };

        let report = serde_json::json!({
            "goal_id": goal_id,
            "final_status": status,
            "total_iterations": iterations,
            "started_at": started_at,
            "finished_at": now_epoch_ms(),
            "stopped_reason": reason,
            "outcome": outcome,
            "outcome_detail": {
                "diagnosis_hint": diagnosis_hint,
            },
        });

        let path = dir.join("final-report.json");
        std::fs::write(&path, serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?)
            .map_err(|e| format!("write final-report.json: {e}"))?;
        Ok(())
    }
}

impl Default for GoalSupervisorExtension {
    fn default() -> Self {
        Self::new()
    }
}

// ===========================================================================
// Free helper functions (Stage C)
// ===========================================================================

/// Generate objective-specific verification checks via LLM (B2).
///
/// Asks the LLM to produce a JSON array of Check objects tailored to the
/// objective. On any failure (LLM error, parse error), returns None → CI fallback.
pub async fn generate_checks_via_llm(
    registry: &ion_provider::registry::ApiRegistry,
    model: &ion_provider::types::Model,
    objective: &str,
) -> Option<Vec<Check>> {
    let system_prompt = format!(
        "You are a verification engineer. Given a coding objective, generate a JSON array of \
         verification checks. Each check must have: name, check_type (\"ci\" or \"contingency\"), \
         rationale, command (shell command, exit 0 on success), pass_criteria ({{\"kind\":\"exit_code\",\"expected\":0}}), \
         must_pass (true).\n\n\
         Rules:\n- Include objective-specific checks (grep for required functions/symbols).\n\
         - Include cargo build + cargo test as CI checks.\n\
         - Commands must be shell-safe.\n- Output ONLY the JSON array, no markdown.\n\n\
         Objective: {objective}"
    );
    let context = ion_provider::types::Context {
        system_prompt: Some(system_prompt),
        messages: vec![],
        tools: None,
    };
    let options = ion_provider::types::StreamOptions {
        max_tokens: Some(2000),
        api_key: None,
        reasoning: None,
        timeout_ms: None,
        max_retries: None,
        response_format: None,
    };
    let msg = ion_provider::registry::complete(registry, model, &context, Some(&options)).await.ok()?;
    let text: String = msg.content.iter().filter_map(|c| match c {
        ion_provider::types::AssistantContentBlock::Text(t) => Some(t.text.as_str()),
        _ => None,
    }).collect::<Vec<_>>().join("");
    // Strip markdown fences.
    let text = text.trim().strip_prefix("```json").or_else(|| text.trim().strip_prefix("```")).unwrap_or(&text).trim().trim_end_matches("```").trim();
    match serde_json::from_str::<Vec<serde_json::Value>>(text) {
        Ok(arr) => {
            let checks: Vec<Check> = arr.iter().filter_map(|item| serde_json::from_value(item.clone()).ok()).collect();
            if checks.is_empty() { None } else { Some(checks) }
        }
        Err(_) => None,
    }
}

/// Generate default CI checks when goal_set is called without explicit checks.
///
/// These are generic CI checks that apply to most code-change goals:
/// - cargo build (compiles)
/// - cargo test (tests pass)
/// - cargo clippy (no new warnings)
/// - no U+FFFD garbled chars (ION-specific)
///
/// B2: This is the fallback when the caller doesn't specify checks.
/// Future: an LLM call could generate objective-specific checks here.
pub fn default_ci_checks() -> Vec<Check> {
    vec![
        Check {
            name: "cargo_build".into(),
            check_type: CheckType::Ci,
            rationale: "Code must compile".into(),
            command: "cargo build --lib 2>&1 | tail -1".into(),
            pass_criteria: PassCriteria::ExitCode { expected: 0 },
            must_pass: true,
        },
        Check {
            name: "cargo_test".into(),
            check_type: CheckType::Ci,
            rationale: "Tests must pass".into(),
            command: "cargo test --lib 2>&1 | tail -1".into(),
            pass_criteria: PassCriteria::ExitCode { expected: 0 },
            must_pass: true,
        },
        Check {
            name: "no_ufffd".into(),
            check_type: CheckType::Ci,
            rationale: "No garbled UTF-8 (U+FFFD) in source".into(),
            command: "! grep -rq $'\\xef\\xbf\\xbd' src/".into(),
            pass_criteria: PassCriteria::ExitCode { expected: 0 },
            must_pass: true,
        },
    ]
}

// ===========================================================================
// Progress analysis types + helpers (Deepening Task 1)
// ===========================================================================

#[derive(Clone, Debug, PartialEq)]
pub enum ProgressTrend {
    Converging,
    Oscillating,
    Drifting,
    Stagnant,
}

#[derive(Clone, Debug, Default)]
pub struct ProgressReport {
    pub trend: ProgressTrend,
    pub failed_history: Vec<Vec<String>>,
    pub recommendation: String,
}

impl Default for ProgressTrend {
    fn default() -> Self {
        ProgressTrend::Converging
    }
}

/// Read the last N entries' failed_checks from iterations.jsonl.
fn read_recent_failed_history(dir: &std::path::Path, n: usize) -> Vec<Vec<String>> {
    let path = dir.join("iterations.jsonl");
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return vec![],
    };
    let mut all: Vec<Vec<String>> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| {
            let v: serde_json::Value = serde_json::from_str(line).ok()?;
            v.get("failed_checks")?
                .as_array()?
                .iter()
                .filter_map(|f| f.as_str().map(String::from))
                .collect::<Vec<_>>()
                .into()
        })
        .collect();
    let start = all.len().saturating_sub(n);
    all.drain(start..).collect()
}

/// Classify the trend from a list of failed_checks history (oldest first).
///
/// - Converging: failed set size is decreasing
/// - Stagnant: failed set identical across all entries
/// - Oscillating: failed count stable but elements change
fn classify_trend(history: &[Vec<String>]) -> ProgressTrend {
    if history.len() < 2 {
        return ProgressTrend::Converging;
    }

    // Check stagnant: all entries have the same set.
    let first_set: std::collections::HashSet<&str> =
        history[0].iter().map(|s| s.as_str()).collect();
    let all_same = history.iter().all(|h| {
        let s: std::collections::HashSet<&str> = h.iter().map(|s| s.as_str()).collect();
        s == first_set
    });
    if all_same {
        return ProgressTrend::Stagnant;
    }

    // Check converging: sizes are monotonically non-increasing and at least one decrease.
    let sizes: Vec<usize> = history.iter().map(|h| h.len()).collect();
    let mut decreasing = false;
    for i in 1..sizes.len() {
        if sizes[i] < sizes[i - 1] {
            decreasing = true;
        }
        if sizes[i] > sizes[i - 1] {
            // Size increased — not converging.
            return ProgressTrend::Oscillating;
        }
    }
    if decreasing {
        ProgressTrend::Converging
    } else {
        // Same size but different elements.
        ProgressTrend::Oscillating
    }
}

/// Jaccard similarity over whitespace-split tokens (length > 2).
/// Returns 0.0 if either string has no qualifying tokens.
pub fn calculate_similarity(a: &str, b: &str) -> f64 {
    fn tokenize<'a>(s: &'a str) -> std::collections::HashSet<&'a str> {
        s.split(|c: char| c.is_whitespace() || matches!(c, ',' | '.' | ';' | '!' | '?' | '\n'))
            .filter(|t| t.len() > 2)
            .collect()
    }
    let set_a = tokenize(a);
    let set_b = tokenize(b);
    if set_a.is_empty() || set_b.is_empty() {
        return 0.0;
    }
    let inter = set_a.intersection(&set_b).count() as f64;
    let union = (set_a.len() + set_b.len()) as f64 - inter;
    if union == 0.0 {
        return 0.0;
    }
    inter / union
}

/// Current time in unix milliseconds.
pub fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Current time as ISO-8601 string (best-effort, no chrono dep).
pub fn now_iso8601() -> String {
    // Simple RFC3339-ish stamp using unix seconds. Good enough for log sorting.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("epoch:{secs}")
}

/// Directory for a goal run: `~/.ion/agent/goal-runs/<session>/`.
pub fn dirs_for(session: &str) -> std::path::PathBuf {
    let base = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    std::path::PathBuf::from(base)
        .join(".ion/agent/goal-runs")
        .join(session)
}

#[async_trait]
impl Extension for GoalSupervisorExtension {
    fn name(&self) -> &str {
        "goal_supervisor"
    }

    /// Track tool usage for drift monitoring (Task 4).
    /// Records recent tool calls into GoalState.recent_tools (sliding window K=10).
    async fn on_tool_execution_end(
        &self,
        ctx: &crate::agent::extension::ToolExecutionContext,
    ) -> AgentResult<()> {
        // Extract a summary of what the tool touched.
        let summary = match ctx.tool_name.as_str() {
            "bash" | "bash_run" => {
                ctx.args.get("command").and_then(|v| v.as_str()).unwrap_or("").to_string()
            }
            "write" | "edit" | "write_file" | "edit_file" => {
                ctx.args.get("file_path").and_then(|v| v.as_str()).unwrap_or("").to_string()
            }
            "read" | "read_file" => {
                ctx.args.get("file_path").and_then(|v| v.as_str()).unwrap_or("").to_string()
            }
            _ => ctx.tool_name.clone(),
        };

        // Append to recent_tools, keep last 10.
        if let Ok(mut guard) = self.state.lock() {
            if let Some(state) = guard.as_mut() {
                state.recent_tools.push((ctx.tool_name.clone(), summary));
                if state.recent_tools.len() > 10 {
                    let excess = state.recent_tools.len() - 10;
                    state.recent_tools.drain(0..excess);
                }
            }
        }
        Ok(())
    }

    /// Stage C: the on_agent_end hook is kept for status bookkeeping only.
    /// The real closed-loop enforcement happens in on_gate_check (below),
    /// which is the kernel-mandated gate that the LLM cannot skip.
    async fn on_agent_end(&self, _ctx: &crate::agent::agent_loop::AgentContext) -> AgentResult<()> {
        // No-op: gate check already handled completion/exhaustion decisions.
        Ok(())
    }

    /// Kernel-enforced gate: runs when the LLM decides to Stop.
    ///
    /// If a goal is active and checks have not all passed, returns
    /// `RetryWith(continue_message)` to force another loop iteration. The LLM
    /// sees the failure evidence and must fix it before it can stop.
    ///
    /// This is the core of the goal closed-loop (GOAL_SUPERVISOR.md section 1).
    async fn on_gate_check(
        &self,
        ctx: &crate::agent::extension::TurnContext,
    ) -> AgentResult<crate::agent::extension::GateDecision> {
        use crate::agent::extension::GateDecision;

        // Skip if disabled or no active goal.
        if !self.config.check_on_agent_end {
            return Ok(GateDecision::Allow);
        }
        let has_goal = self.state.lock().map(|g| g.is_some()).unwrap_or(false);
        if !has_goal {
            return Ok(GateDecision::Allow);
        }

        // 1. Run all checks (deterministic execution + evidence collection).
        let results = self
            .run_all_checks()
            .await
            .map_err(AgentError::Tool)?;

        // 2. Log this iteration.
        let _ = self.log_iteration(&results);

        // 3. If all passed -> goal complete, allow stop.
        let all_pass = results.iter().all(|r| r.status == CheckStatus::Pass);
        if all_pass {
            self.set_status(GoalStatus::Complete);
            let _ = self.write_final_report("complete", "all_checks_passed");
            return Ok(GateDecision::Allow);
        }

        // 4. Extract the current action plan from the last assistant message
        //    (used for repetition detection).
        let current_plan = ctx
            .messages
            .iter()
            .rev()
            .find_map(|m| match m {
                ion_provider::types::Message::Assistant(a) => {
                    // Concatenate all Text blocks into one string.
                    let text: String = a
                        .content
                        .iter()
                        .filter_map(|b| match b {
                            ion_provider::types::AssistantContentBlock::Text(t) => Some(t.text.as_str()),
                            _ => None, // Ignore Thinking / ToolCall blocks.
                        })
                        .collect::<Vec<_>>()
                        .join(" ");
                    if text.is_empty() { None } else { Some(text) }
                }
                _ => None,
            });

        // 5. Check guards (max_iter / max_duration / max_cost / repetitive).
        if let Some(reason) = self.check_guards(current_plan.as_deref()) {
            self.set_status(GoalStatus::Exhausted);
            let _ = self.write_final_report("exhausted", &reason);
            // Guard tripped -> stop the loop (don't retry forever).
            return Ok(GateDecision::Allow);
        }

        // 6. Increment iteration counter + remember the action plan.
        self.record_iteration(current_plan.clone());

        // 7. Build the continue message with failure evidence.
        let failed: Vec<&CheckResult> = results
            .iter()
            .filter(|r| r.status != CheckStatus::Pass)
            .collect();
        let mut msg = String::from(
            "Goal not complete. The following checks failed:\n",
        );
        for r in &failed {
            let evidence_excerpt = r
                .evidence
                .as_ref()
                .and_then(|e| e.stdout_excerpt.as_deref())
                .unwrap_or("(no evidence)");
            msg.push_str(&format!("- {} (evidence: {})\n", r.name, evidence_excerpt));
        }
        msg.push_str("Fix the failing checks before stopping.");

        // Progress analysis (Task 1 deepening): classify trend + give recommendation.
        let progress = self.analyze_progress(current_plan.as_deref());
        msg.push_str(&format!(
            "\n📊 Progress: {:?}. {}",
            progress.trend, progress.recommendation
        ));

        // 4 (repetition strategy): if repetitive, nudge a different approach.
        if current_plan
            .as_deref()
            .map(|p| self.check_guards(Some(p)).as_deref() == Some("repetitive"))
            .unwrap_or(false)
        {
            msg.push_str(" NOTE: previous attempt was similar. Try a different approach.");
        }

        self.set_status(GoalStatus::Running);
        Ok(GateDecision::RetryWith(msg))
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
pub struct GoalSetTool {
    pub state: SharedGoalState,
    pub registry: Option<Arc<ion_provider::registry::ApiRegistry>>,
    pub model: Option<ion_provider::types::Model>,
}

impl GoalSetTool {
    pub fn new(state: SharedGoalState) -> Self {
        Self { state, registry: None, model: None }
    }
    pub fn with_llm(state: SharedGoalState, registry: Arc<ion_provider::registry::ApiRegistry>, model: ion_provider::types::Model) -> Self {
        Self { state, registry: Some(registry), model: Some(model) }
    }
}

#[async_trait]
impl Tool for GoalSetTool {
    fn name(&self) -> &str {
        "goal_set"
    }

    fn description(&self) -> &str {
        "Set a goal for the Goal Supervisor. Just describe what you want to achieve \
         in natural language — the system will automatically generate verification \
         checks and iterate until the goal is complete. Setting a new goal cancels \
         any previous goal. Example: goal_set({\"objective\": \"add a login function with tests\"})"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "objective": {
                    "type": "string",
                    "description": "What you want to achieve, in natural language. \
                                    Example: 'implement a tic-tac-toe game with win detection'"
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

        // Optional: checks. If not provided (or empty), generate default CI
        // checks so the goal is still verifiable (B2: auto-generate fallback).
        let mut checks: Vec<Check> = Vec::new();
        if let Some(arr) = args.get("checks").and_then(|v| v.as_array()) {
            for (i, item) in arr.iter().enumerate() {
                let check: Check = serde_json::from_value(item.clone()).map_err(|e| {
                    AgentError::Tool(format!("goal_set: invalid check at index {i}: {e}"))
                })?;
                checks.push(check);
            }
        }
        // B2: if no checks were provided, use default CI checks.
        if checks.is_empty() {
            // B2: try LLM generation first, fall back to CI defaults.
            checks = if let (Some(reg), Some(mdl)) = (&self.registry, &self.model) {
                generate_checks_via_llm(reg, mdl, &objective).await
                    .unwrap_or_else(default_ci_checks)
            } else {
                default_ci_checks()
            };
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
            last_action_plan: None, recent_tools: vec![],
        };

        // Replace any previous goal (the old one is implicitly cancelled).
        let previous_id = {
            let mut guard = self.state.lock()
                .map_err(|e| AgentError::Tool(format!("goal_set: state lock poisoned: {e}")))?;
            let prev = guard.as_ref().map(|s| s.goal_id.clone());
            *guard = Some(new_state);
            prev
        };

        let confirmation = {
            // Compute check count before building JSON (json! macro can't host expression blocks).
            let check_count = {
                let g = self.state.lock().map_err(|e| AgentError::Tool(format!("goal_set: state lock poisoned: {e}")))?;
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
// GoalRefineTool — incrementally adjust a running goal (Task 2 deepening)
// ===========================================================================

/// Tool that refines (incrementally updates) the current goal without resetting progress.
///
/// Unlike goal_set (which replaces the entire goal, clearing iteration_count),
/// goal_refine patches the objective and/or checks while preserving progress metrics.
/// This lets the agent adapt when progress analysis suggests adjustments.
pub struct GoalRefineTool(pub SharedGoalState);

#[async_trait]
impl Tool for GoalRefineTool {
    fn name(&self) -> &str {
        "goal_refine"
    }

    fn description(&self) -> &str {
        "Incrementally adjust the current goal without resetting progress. \
         Supports: objective_patch (new objective text), checks_add (array of Check), \
         checks_remove (array of check names to drop). Preserves iteration_count and cost."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "objective_patch": {
                    "type": "string",
                    "description": "Optional: updated objective text"
                },
                "checks_add": {
                    "type": "array",
                    "description": "Checks to add (same format as goal_set checks)",
                    "items": {"type": "object"}
                },
                "checks_remove": {
                    "type": "array",
                    "description": "Names of checks to remove",
                    "items": {"type": "string"}
                }
            }
        })
    }

    async fn execute(&self, args: serde_json::Value, _rt: &dyn crate::runtime::Runtime) -> AgentResult<String> {
        let objective_patch = args.get("objective_patch").and_then(|v| v.as_str());
        let checks_add: Vec<Check> = if let Some(arr) = args.get("checks_add").and_then(|v| v.as_array()) {
            arr.iter().filter_map(|item| serde_json::from_value(item.clone()).ok()).collect()
        } else {
            vec![]
        };
        let checks_remove: Vec<String> = args.get("checks_remove")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();

        let mut guard = self.0.lock().map_err(|e| AgentError::Tool(format!("goal_refine: lock: {e}")))?;
        let state = guard.as_mut().ok_or_else(|| AgentError::Tool("goal_refine: no active goal".into()))?;

        // Apply objective patch.
        if let Some(new_obj) = objective_patch {
            state.objective = new_obj.to_string();
        }

        // Remove checks by name.
        if !checks_remove.is_empty() {
            state.checks.retain(|c| !checks_remove.contains(&c.name));
        }

        // Add new checks.
        state.checks.extend(checks_add);

        // Note: iteration_count, started_at, total_cost_usd, last_action_plan preserved.
        let check_names: Vec<&str> = state.checks.iter().map(|c| c.name.as_str()).collect();
        Ok(format!(
            "Goal refined. Objective: \"{}\". Checks: [{}]. Progress preserved (iteration_count={}).",
            &state.objective[..state.objective.len().min(60)],
            check_names.join(", "),
            state.iteration_count
        ))
    }
}

// ===========================================================================
// GoalDiagnoseTool — spawn diagnostic agent for stuck goals (Task 3)
// ===========================================================================

/// Tool that spawns a goal-diagnostician agent to analyze why a goal is stuck.
///
/// Packages the objective, failed checks, and progress history into a prompt,
/// spawns the diagnostician agent via Runtime::spawn_worker, and returns its analysis.
pub struct GoalDiagnoseTool(pub SharedGoalState);

#[async_trait]
impl Tool for GoalDiagnoseTool {
    fn name(&self) -> &str {
        "goal_diagnose"
    }

    fn description(&self) -> &str {
        "Spawn a diagnostic agent to analyze why the current goal is stuck and \
         recommend adjustments. Use when progress analysis shows Oscillating, \
         Stagnant, or Drifting trends. Returns diagnosis + recommendations."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {}
        })
    }

    async fn execute(&self, _args: serde_json::Value, rt: &dyn crate::runtime::Runtime) -> AgentResult<String> {
        // Package current goal state into a diagnostic prompt.
        let (objective, checks, iteration_count) = {
            let guard = self.0.lock().map_err(|e| AgentError::Tool(format!("goal_diagnose: lock: {e}")))?;
            let state = guard.as_ref().ok_or_else(|| AgentError::Tool("goal_diagnose: no active goal".into()))?;
            (state.objective.clone(), state.checks.clone(), state.iteration_count)
        };

        let check_summary: Vec<String> = checks.iter().map(|c| {
            format!("- {} ({}): {}", c.name, match c.check_type { CheckType::Ci => "ci", CheckType::Contingency => "contingency" }, c.rationale)
        }).collect();

        let task = format!(
            "Diagnose why this goal is stuck after {} iterations:\n\n\
             OBJECTIVE: {}\n\n\
             CHECKS:\n{}\n\n\
             Read the goal-runs log in ~/.ion/agent/goal-runs/ for iteration history. \
             Analyze why the agent can't complete the checks and recommend adjustments \
             (goal_refine to relax checks, split the goal, or change approach).",
            iteration_count,
            objective,
            check_summary.join("\n")
        );

        // Spawn diagnostician agent.
        let req = crate::runtime::SpawnWorkerRequest {
            relation: crate::runtime::SpawnRelation::Peer,
            agent: "goal-diagnostician".into(),
            task,
            name: None,
            report_channel: None,
            wait: true,
            worktree: None,
            hook_depth: Some(1),
            system_prompt_override: None,
            model: None,
            provider: None,
        };

        match rt.spawn_worker(req).await {
            Ok(resp) => {
                Ok(resp.first_turn_output.unwrap_or_else(|| "Diagnostician returned no output".into()))
            }
            Err(e) => {
                Err(AgentError::Tool(format!("goal_diagnose: spawn failed: {e}")))
            }
        }
    }
}
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

/// Write `stdout` to `/tmp/goal-checks/<check_name>-<timestamp>.log` and return
/// the absolute path of the artifact file.
///
/// The artifact directory is created with `create_dir_all` if it does not yet
/// exist. Returns an error if the directory cannot be created or the file
/// cannot be written.
fn write_artifact(check_name: &str, stdout: &str) -> std::io::Result<String> {
    use std::time::{SystemTime, UNIX_EPOCH};

    let dir = "/tmp/goal-checks";
    std::fs::create_dir_all(dir)?;

    // Sanitize the check name so it is safe to embed in a filename: keep only
    // alphanumeric characters, underscores, and hyphens; collapse the rest.
    let safe_name: String = check_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();

    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);

    let path = format!("{dir}/{safe_name}-{ts}.log");
    std::fs::write(&path, stdout)?;
    Ok(path)
}

// ===========================================================================
// Unit tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::LocalRuntime;

    // ── Progress analysis tests (Task 1) ──

    #[test]
    fn test_classify_trend_converging() {
        let h = vec![
            vec!["a".into(), "b".into(), "c".into()],
            vec!["a".into(), "b".into()],
            vec!["a".into()],
        ];
        assert_eq!(classify_trend(&h), ProgressTrend::Converging);
    }

    #[test]
    fn test_classify_trend_stagnant() {
        let h = vec![
            vec!["a".into(), "b".into()],
            vec!["a".into(), "b".into()],
            vec!["a".into(), "b".into()],
        ];
        assert_eq!(classify_trend(&h), ProgressTrend::Stagnant);
    }

    #[test]
    fn test_classify_trend_oscillating() {
        // Same count, different elements.
        let h = vec![
            vec!["a".into(), "b".into()],
            vec!["b".into(), "c".into()],
            vec!["a".into(), "c".into()],
        ];
        assert_eq!(classify_trend(&h), ProgressTrend::Oscillating);
    }

    #[test]
    fn test_classify_trend_single_entry() {
        // Only 1 entry → can't classify, default Converging.
        let h = vec![vec!["a".into()]];
        assert_eq!(classify_trend(&h), ProgressTrend::Converging);
    }

    #[test]
    fn test_analyze_progress_drift_detection() {
        // Action plan completely unrelated to objective → Drifting.
        let ext = GoalSupervisorExtension::new();
        {
            let mut guard = ext.state.lock().unwrap();
            *guard = Some(GoalState {
                goal_id: "g1".into(),
                objective: "fix the authentication bug in login module".into(),
                checks: vec![],
                status: GoalStatus::Running,
                iteration_count: 2,
                started_at: format!("epoch:{}", now_epoch_ms() / 1000),
                total_cost_usd: 0.0,
                last_action_plan: None, recent_tools: vec![],
            });
        }
        // Plan about something totally unrelated.
        let report = ext.analyze_progress(Some("update the README documentation formatting"));
        assert_eq!(report.trend, ProgressTrend::Drifting);
        assert!(report.recommendation.contains("Re-focus"));
    }

    // ── GoalRefineTool tests (Task 2) ──

    #[tokio::test]
    async fn test_goal_refine_add_check() {
        let shared: SharedGoalState = Arc::new(Mutex::new(Some(GoalState {
            goal_id: "g1".into(),
            objective: "original".into(),
            checks: vec![],
            status: GoalStatus::Running,
            iteration_count: 3,
            started_at: "epoch:100".into(),
            total_cost_usd: 0.5,
            last_action_plan: None, recent_tools: vec![],
        })));
        let tool = GoalRefineTool(shared.clone());
        let args = serde_json::json!({
            "checks_add": [{"name": "new_check", "check_type": "ci", "rationale": "test", "command": "true", "pass_criteria": {"kind": "exit_code", "expected": 0}, "must_pass": true}]
        });
        let _ = tool.execute(args, &rt()).await.expect("refine ok");
        let guard = shared.lock().unwrap();
        let state = guard.as_ref().unwrap();
        assert_eq!(state.checks.len(), 1, "new check added");
        assert_eq!(state.checks[0].name, "new_check");
        // Progress preserved.
        assert_eq!(state.iteration_count, 3, "iteration_count preserved");
        assert_eq!(state.total_cost_usd, 0.5, "cost preserved");
    }

    #[tokio::test]
    async fn test_goal_refine_remove_check() {
        let shared: SharedGoalState = Arc::new(Mutex::new(Some(GoalState {
            goal_id: "g1".into(),
            objective: "test".into(),
            checks: vec![
                Check { name: "keep".into(), check_type: CheckType::Ci, rationale: "r".into(), command: "true".into(), pass_criteria: PassCriteria::ExitCode { expected: 0 }, must_pass: true },
                Check { name: "drop".into(), check_type: CheckType::Ci, rationale: "r".into(), command: "true".into(), pass_criteria: PassCriteria::ExitCode { expected: 0 }, must_pass: true },
            ],
            status: GoalStatus::Running,
            iteration_count: 2,
            started_at: "epoch:100".into(),
            total_cost_usd: 0.1,
            last_action_plan: None, recent_tools: vec![],
        })));
        let tool = GoalRefineTool(shared.clone());
        let args = serde_json::json!({"checks_remove": ["drop"]});
        let _ = tool.execute(args, &rt()).await.expect("refine ok");
        let guard = shared.lock().unwrap();
        let state = guard.as_ref().unwrap();
        assert_eq!(state.checks.len(), 1, "one check removed");
        assert_eq!(state.checks[0].name, "keep");
    }

    #[tokio::test]
    async fn test_goal_refine_patch_objective() {
        let shared: SharedGoalState = Arc::new(Mutex::new(Some(GoalState {
            goal_id: "g1".into(),
            objective: "old objective".into(),
            checks: vec![],
            status: GoalStatus::Running,
            iteration_count: 1,
            started_at: "epoch:100".into(),
            total_cost_usd: 0.0,
            last_action_plan: None, recent_tools: vec![],
        })));
        let tool = GoalRefineTool(shared.clone());
        let args = serde_json::json!({"objective_patch": "new refined objective"});
        let _ = tool.execute(args, &rt()).await.expect("refine ok");
        let guard = shared.lock().unwrap();
        assert_eq!(guard.as_ref().unwrap().objective, "new refined objective");
    }

    #[tokio::test]
    async fn test_goal_refine_no_goal_errors() {
        let shared: SharedGoalState = Arc::new(Mutex::new(None));
        let tool = GoalRefineTool(shared);
        let result = tool.execute(serde_json::json!({}), &rt()).await;
        assert!(result.is_err(), "refine without active goal must error");
    }

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

    #[test]
    fn test_default_ci_checks_not_empty() {
        let checks = default_ci_checks();
        assert!(!checks.is_empty(), "default CI checks must not be empty");
        assert!(checks.iter().any(|c| c.name == "cargo_build"), "must have cargo_build");
        assert!(checks.iter().any(|c| c.name == "cargo_test"), "must have cargo_test");
        assert!(checks.iter().any(|c| c.name == "no_ufffd"), "must have no_ufffd");
        // All must be must_pass=true and check_type=Ci
        assert!(checks.iter().all(|c| c.must_pass), "all default checks must_pass");
        assert!(checks.iter().all(|c| c.check_type == CheckType::Ci), "all default checks are CI");
    }

    #[tokio::test]
    async fn test_goal_set_no_checks_uses_defaults() {
        // When goal_set is called without checks, default CI checks should be used.
        let shared: SharedGoalState = Arc::new(Mutex::new(None));
        let tool = GoalSetTool::new(shared.clone());
        let args = serde_json::json!({"objective": "fix the bug"});
        let result = tool.execute(args, &rt()).await.expect("goal_set ok");
        // Verify state has default checks
        let guard = shared.lock().unwrap();
        let state = guard.as_ref().expect("goal set");
        assert!(!state.checks.is_empty(), "default checks should be generated");
        assert!(state.checks.len() >= 3, "at least 3 default CI checks, got {}", state.checks.len());
    }

    #[tokio::test]
    async fn test_goal_set_overrides() {
        // Two goal_set calls share the same SharedGoalState. The second call
        // must cancel the first: only the second goal remains in state, and
        // the tool result reports the previous goal id + cancelled flag.
        let shared: SharedGoalState = Arc::new(Mutex::new(None));
        let tool = GoalSetTool::new(shared.clone());

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
            // B2: empty checks now auto-fills with default CI checks.
            assert!(!state.checks.is_empty(), "goal B has default CI checks (B2 auto-fill)");
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
        let tool = GoalSetTool::new(shared);
        assert_eq!(tool.name(), "goal_set");
        let params = tool.parameters();
        // objective is required; checks is optional.
        assert_eq!(params["type"], "object");
        assert!(params["required"].as_array().unwrap().iter().any(|v| v == "objective"));
    }

    #[tokio::test]
    async fn test_goal_set_missing_objective_errors() {
        let shared: SharedGoalState = Arc::new(Mutex::new(None));
        let tool = GoalSetTool::new(shared);
        let res = tool.execute(serde_json::json!({}), &rt()).await;
        assert!(res.is_err(), "missing objective must error");
    }

    #[tokio::test]
    async fn test_goal_set_bad_check_errors() {
        let shared: SharedGoalState = Arc::new(Mutex::new(None));
        let tool = GoalSetTool::new(shared);
        // check missing required fields -> deserialization error.
        let args = serde_json::json!({
            "objective": "ok",
            "checks": [{"name": "bad"}]
        });
        let res = tool.execute(args, &rt()).await;
        assert!(res.is_err(), "malformed check must error");
    }

    // -----------------------------------------------------------------------
    // Stage B: run_single_check / run_all_checks tests
    // -----------------------------------------------------------------------

    /// Helper: build a Check with sane defaults for the check-execution tests.
    fn make_check(name: &str, command: &str, pc: PassCriteria) -> Check {
        Check {
            name: name.into(),
            check_type: CheckType::Ci,
            rationale: "test".into(),
            command: command.into(),
            pass_criteria: pc,
            must_pass: true,
        }
    }

    #[tokio::test]
    async fn test_check_exit_code_pass() {
        // `true` exits 0; ExitCode(0) -> Pass.
        let check = make_check("exit-ok", "true", PassCriteria::ExitCode { expected: 0 });
        let res = GoalSupervisorExtension::run_single_check(&check)
            .await
            .expect("check ran");
        assert_eq!(res.status, CheckStatus::Pass, "expected Pass");
        assert_eq!(res.name, "exit-ok");
        assert!(res.evidence.is_some(), "evidence collected");
        assert_eq!(
            res.evidence.as_ref().unwrap().exit_code,
            Some(0),
            "exit_code captured"
        );
    }

    #[tokio::test]
    async fn test_check_exit_code_fail() {
        // `false` exits 1; ExitCode(0) -> Fail.
        let check = make_check("exit-fail", "false", PassCriteria::ExitCode { expected: 0 });
        let res = GoalSupervisorExtension::run_single_check(&check)
            .await
            .expect("check ran");
        assert_eq!(res.status, CheckStatus::Fail, "expected Fail");
        let ev = res.evidence.as_ref().expect("evidence present");
        assert_ne!(ev.exit_code, Some(0), "exit_code is non-zero");
    }

    #[tokio::test]
    async fn test_check_grep_empty_pass() {
        // stdout "hello" contains no "xyz" -> GrepEmpty passes.
        let check = make_check(
            "grep-empty-ok",
            "echo hello",
            PassCriteria::GrepEmpty { pattern: "xyz".into() },
        );
        let res = GoalSupervisorExtension::run_single_check(&check)
            .await
            .expect("check ran");
        assert_eq!(res.status, CheckStatus::Pass, "no match -> Pass");
        let ev = res.evidence.as_ref().expect("evidence present");
        // For GrepEmpty, the evidence.matches vector must be empty (no matches).
        let m = ev.matches.as_ref().expect("matches present for grep");
        assert!(m.is_empty(), "no matches captured");
    }

    #[tokio::test]
    async fn test_check_grep_empty_fail() {
        // stdout "hello" contains "hello" -> GrepEmpty fails.
        let check = make_check(
            "grep-empty-fail",
            "echo hello",
            PassCriteria::GrepEmpty { pattern: "hello".into() },
        );
        let res = GoalSupervisorExtension::run_single_check(&check)
            .await
            .expect("check ran");
        assert_eq!(res.status, CheckStatus::Fail, "match -> Fail");
        let ev = res.evidence.as_ref().expect("evidence present");
        let m = ev.matches.as_ref().expect("matches present for grep");
        assert_eq!(m.len(), 1, "one matching line captured");
        assert!(m[0].contains("hello"), "matched line content");
    }

    #[tokio::test]
    async fn test_check_file_exists_pass() {
        // /tmp always exists -> FileExists passes.
        let check = make_check(
            "file-exists-ok",
            "true",
            PassCriteria::FileExists { path: "/tmp".into() },
        );
        let res = GoalSupervisorExtension::run_single_check(&check)
            .await
            .expect("check ran");
        assert_eq!(res.status, CheckStatus::Pass, "/tmp exists -> Pass");
        assert!(res.evidence.is_some(), "evidence still collected");
    }

    #[tokio::test]
    async fn test_check_file_exists_fail() {
        // A path that does not exist -> FileExists fails.
        let check = make_check(
            "file-exists-fail",
            "true",
            PassCriteria::FileExists { path: "/nonexistent/xyz".into() },
        );
        let res = GoalSupervisorExtension::run_single_check(&check)
            .await
            .expect("check ran");
        assert_eq!(res.status, CheckStatus::Fail, "missing path -> Fail");
    }

    #[tokio::test]
    async fn test_evidence_artifact_written() {
        // Running a check must produce an artifact file on disk whose path is
        // recorded in the evidence, and whose contents match the stdout.
        let check = make_check(
            "artifact-check",
            "echo artifact-line",
            PassCriteria::ExitCode { expected: 0 },
        );
        let res = GoalSupervisorExtension::run_single_check(&check)
            .await
            .expect("check ran");
        assert_eq!(res.status, CheckStatus::Pass);
        let ev = res.evidence.as_ref().expect("evidence present");
        let path = ev.artifact_path.as_ref().expect("artifact path set");
        assert!(path.starts_with("/tmp/goal-checks/"), "artifact under goal-checks dir");
        assert!(
            path.contains("artifact-check"),
            "artifact filename includes the check name"
        );
        // The artifact file must actually exist on disk.
        assert!(std::path::Path::new(path).exists(), "artifact file exists on disk");
        // And its contents should hold the full stdout.
        let body = std::fs::read_to_string(path).expect("read artifact");
        assert!(body.contains("artifact-line"), "artifact holds stdout content");
    }

    #[tokio::test]
    async fn test_run_all_checks_no_goal_errors() {
        // With no goal set, run_all_checks should return an error.
        let ext = GoalSupervisorExtension::new();
        let res = ext.run_all_checks().await;
        assert!(res.is_err(), "no goal -> Err");
    }

    #[tokio::test]
    async fn test_run_all_checks_runs_each() {
        // With a goal set holding two checks, run_all_checks returns one
        // CheckResult per check in order.
        let shared: SharedGoalState = Arc::new(Mutex::new(None));
        let ext = GoalSupervisorExtension::new().with_config(GoalSupervisorConfig::default());
        // Reuse the extension's own state by cloning it in.
        {
            let state = GoalState {
                goal_id: "g1".into(),
                objective: "o".into(),
                checks: vec![
                    make_check("c1", "true", PassCriteria::ExitCode { expected: 0 }),
                    make_check("c2", "true", PassCriteria::ExitCode { expected: 0 }),
                ],
                status: GoalStatus::Running,
                iteration_count: 0,
                started_at: "epoch:0".into(),
                total_cost_usd: 0.0,
                last_action_plan: None, recent_tools: vec![],
            };
            *shared.lock().unwrap() = Some(state);
        }
        // The extension uses its own `state`; we emulate by constructing one
        // bound to the shared state above.
        let ext = GoalSupervisorExtension {
            state: shared,
            config: GoalSupervisorConfig::default(),
            session_id: None,
        };
        let results = ext.run_all_checks().await.expect("all checks run");
        assert_eq!(results.len(), 2, "one result per check");
        assert_eq!(results[0].name, "c1");
        assert_eq!(results[1].name, "c2");
        assert_eq!(results[0].status, CheckStatus::Pass);
        assert_eq!(results[1].status, CheckStatus::Pass);
    }

    // ── Stage C tests: guards, similarity, logging ──

    #[test]
    fn test_calculate_similarity_identical() {
        let s = "fix the login bug and add tests";
        assert_eq!(calculate_similarity(s, s), 1.0);
    }

    #[test]
    fn test_calculate_similarity_disjoint() {
        // No shared tokens longer than 2 chars.
        let a = "alpha beta gamma";
        let b = "delta epsilon zeta";
        assert_eq!(calculate_similarity(a, b), 0.0);
    }

    #[test]
    fn test_calculate_similarity_partial() {
        // Shared: "fix", "login", "bug"; union also includes unique tokens.
        let a = "fix the login bug";
        let b = "fix the login bug and add tests";
        let sim = calculate_similarity(a, b);
        assert!(sim > 0.5, "partial overlap should be > 0.5, got {sim}");
        assert!(sim < 1.0, "not identical, should be < 1.0, got {sim}");
    }

    #[test]
    fn test_guard_max_iterations() {
        // iteration_count at the limit -> guard trips.
        let ext = GoalSupervisorExtension::new()
            .with_config(GoalSupervisorConfig {
                max_iterations: 3,
                ..Default::default()
            });
        {
            let mut guard = ext.state.lock().unwrap();
            *guard = Some(GoalState {
                goal_id: "g1".into(),
                objective: "test".into(),
                checks: vec![],
                status: GoalStatus::Running,
                iteration_count: 3,
                started_at: format!("epoch:{}", now_epoch_ms()/1000),
                total_cost_usd: 0.0,
                last_action_plan: None, recent_tools: vec![],
            });
        }
        let hit = ext.check_guards(None);
        assert_eq!(hit.as_deref(), Some("max_iterations"));
    }

    #[test]
    fn test_guard_max_duration() {
        // started_at far in the past -> max_duration trips.
        let ext = GoalSupervisorExtension::new()
            .with_config(GoalSupervisorConfig {
                max_total_duration_min: 1,
                ..Default::default()
            });
        let old = format!("epoch:{}", now_epoch_ms().saturating_sub(120 * 60 * 1000) / 1000); // 120 min ago
        {
            let mut guard = ext.state.lock().unwrap();
            *guard = Some(GoalState {
                goal_id: "g1".into(),
                objective: "test".into(),
                checks: vec![],
                status: GoalStatus::Running,
                iteration_count: 0,
                started_at: old,
                total_cost_usd: 0.0,
                last_action_plan: None, recent_tools: vec![],
            });
        }
        let hit = ext.check_guards(None);
        assert_eq!(hit.as_deref(), Some("max_duration"));
    }

    #[test]
    fn test_guard_max_cost() {
        let ext = GoalSupervisorExtension::new()
            .with_config(GoalSupervisorConfig {
                max_total_cost_usd: 1.0,
                ..Default::default()
            });
        {
            let mut guard = ext.state.lock().unwrap();
            *guard = Some(GoalState {
                goal_id: "g1".into(),
                objective: "test".into(),
                checks: vec![],
                status: GoalStatus::Running,
                iteration_count: 0,
                started_at: format!("epoch:{}", now_epoch_ms()/1000),
                total_cost_usd: 1.5,
                last_action_plan: None, recent_tools: vec![],
            });
        }
        let hit = ext.check_guards(None);
        assert_eq!(hit.as_deref(), Some("max_cost"));
    }

    #[test]
    fn test_guard_repetitive() {
        // Same action plan twice -> repetitive trips (needs iteration_count >= threshold).
        let ext = GoalSupervisorExtension::new()
            .with_config(GoalSupervisorConfig {
                repetition_threshold: 1,
                ..Default::default()
            });
        let plan = "fix the login bug by updating auth module";
        {
            let mut guard = ext.state.lock().unwrap();
            *guard = Some(GoalState {
                goal_id: "g1".into(),
                objective: "test".into(),
                checks: vec![],
                status: GoalStatus::Running,
                iteration_count: 1,
                started_at: format!("epoch:{}", now_epoch_ms()/1000),
                total_cost_usd: 0.0,
                last_action_plan: Some(plan.into()), recent_tools: vec![],
            });
        }
        let hit = ext.check_guards(Some(plan));
        assert_eq!(hit.as_deref(), Some("repetitive"));
    }

    #[test]
    fn test_guard_none_when_healthy() {
        // Fresh goal, low iter, low cost, distinct plan -> no guard trips.
        let ext = GoalSupervisorExtension::new();
        {
            let mut guard = ext.state.lock().unwrap();
            *guard = Some(GoalState {
                goal_id: "g1".into(),
                objective: "test".into(),
                checks: vec![],
                status: GoalStatus::Running,
                iteration_count: 0,
                started_at: format!("epoch:{}", now_epoch_ms()/1000),
                total_cost_usd: 0.0,
                last_action_plan: Some("first attempt plan alpha".into()), recent_tools: vec![],
            });
        }
        let hit = ext.check_guards(Some("a completely different plan beta"));
        assert!(hit.is_none(), "healthy goal should not trip any guard, got {hit:?}");
    }

    #[tokio::test]
    async fn test_log_iteration_writes_jsonl() {
        // Use a unique session id to avoid clashing with real goal-runs.
        let session = format!("test_log_{}", now_epoch_ms());
        let ext = GoalSupervisorExtension::new()
            .with_session_id(session.clone());
        {
            let mut guard = ext.state.lock().unwrap();
            *guard = Some(GoalState {
                goal_id: "g_log".into(),
                objective: "log test".into(),
                checks: vec![],
                status: GoalStatus::Running,
                iteration_count: 1,
                started_at: format!("epoch:{}", now_epoch_ms()/1000),
                total_cost_usd: 0.0,
                last_action_plan: None, recent_tools: vec![],
            });
        }
        let results = vec![CheckResult {
            name: "ci_test".into(),
            status: CheckStatus::Pass,
            evidence: Some(Evidence {
                exit_code: Some(0),
                stdout_excerpt: Some("ok".into()),
                artifact_path: None,
                matches: None,
            }),
            duration_ms: 10,
            reason: None,
        }];
        ext.log_iteration(&results).expect("log writes ok");

        let path = dirs_for(&session).join("iterations.jsonl");
        let content = std::fs::read_to_string(&path).expect("log file exists");
        let line = content.trim();
        let json: serde_json::Value = serde_json::from_str(line).expect("valid json");
        assert_eq!(json["iter"], 1);
        assert_eq!(json["goal_id"], "g_log");
        assert_eq!(json["all_passed"], true);
        assert!(json["checks_run"].is_array());

        // Cleanup test artifact.
        let _ = std::fs::remove_file(&path);
    }
}
