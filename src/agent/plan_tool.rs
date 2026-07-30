//! Tools for managing a structured plan via the builtin PlanExtension.
//!
//! These five tools replace the previous WASM `plan-extension`. They share
//! state with `PlanExtension` (in-memory step list + optional file path),
//! so the agent can build a plan incrementally while in plan mode and the
//! final plan is persisted to disk on `plan_exit`.
//!
//! Tool list:
//!   - PlanEnterTool  (plan_enter) — enter plan mode + remember file path
//!   - PlanExitTool   (plan_exit)  — exit plan mode (persists plan to file)
//!   - PlanAddTool    (plan_add)   — append a step
//!   - PlanListTool   (plan_list)  — list all steps with [x] marks
//!   - PlanDoneTool   (plan_done)  — mark a step done by 0-based index
//!
//! The actual `plan_enter`/`plan_exit` mode switching is also observed by
//! PlanExtension's `after_tool_call` hook, so both the tool result AND the
//! mode transition happen consistently.

use super::error::{AgentError, AgentResult};
use super::plan_extension::{PlanExtension, PlanStep};
use super::tool::Tool;
use async_trait::async_trait;
use std::sync::Arc;

/// Shared plan state. All five tools hold a clone of this Arc.
pub type SharedPlan = Arc<PlanExtension>;

/// Build the five plan tools sharing the same state.
/// Register all of them with `ToolRegistry::register`.
///
/// IMPORTANT: pass the SAME `SharedPlan` instance that was used to register
/// the PlanExtension (as an Extension). Otherwise the Tool side and the
/// Extension side diverge — plan_add writes to the Tool instance, but the
/// Extension's plan_exit persists the Extension instance's (empty) state.
/// This was the root cause of "PLAN.md is empty after plan_exit" bug.
pub fn plan_tools_with(shared: SharedPlan) -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(PlanEnterTool(shared.clone())),
        Box::new(PlanExitTool(shared.clone())),
        Box::new(PlanAddTool(shared.clone())),
        Box::new(PlanListTool(shared.clone())),
        Box::new(PlanApproveTool(shared.clone())),
        Box::new(PlanDoneTool(shared)),
    ]
}

/// Convenience: create a fresh shared state and build the tools.
/// Use this ONLY when you don't also need to register a PlanExtension
/// (i.e. plan mode hooks won't fire). For full functionality prefer
/// `plan_tools_with(existing_shared)`.
pub fn plan_tools() -> Vec<Box<dyn Tool>> {
    plan_tools_with(Arc::new(PlanExtension::new()))
}

// ---------------------------------------------------------------------------
// plan_approve {index} — Q4 fix: human approval gate before plan_done
// ---------------------------------------------------------------------------

pub struct PlanApproveTool(pub SharedPlan);

#[async_trait]
impl Tool for PlanApproveTool {
    fn name(&self) -> &str {
        "plan_approve"
    }
    fn description(&self) -> &str {
        "Approve a plan step (marks it as 'approved', required before plan_done can mark it 'done'). \
         In auto mode (default), this is a no-op pass-through. The host can override this to \
         require real human confirmation."
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "index": {"type": "integer", "description": "0-based step index to approve."}
            },
            "required": ["index"]
        })
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        _rt: &dyn crate::runtime::Runtime,
    ) -> AgentResult<String> {
        let index = args
            .get("index")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| AgentError::Tool("missing or invalid index".into()))?
            as usize;

        let mut found = false;
        let mut already_done = false;
        if let Ok(mut g) = self.0.plan_steps.lock()
            && index < g.len() {
                if g[index].done {
                    already_done = true;
                } else {
                    g[index].approved = true;
                    found = true;
                }
            }

        if already_done {
            return Ok(format!(
                "{{\"status\":\"noop\",\"index\":{},\"reason\":\"already done\"}}",
                index
            ));
        }
        if found {
            Ok(format!("{{\"status\":\"approved\",\"index\":{}}}", index))
        } else {
            Ok(format!(
                "{{\"status\":\"error\",\"reason\":\"index {} out of range (plan has {} steps)\"}}",
                index,
                self.0.plan_steps.lock().map(|g| g.len()).unwrap_or(0)
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// plan_enter {plan_path: string}
// ---------------------------------------------------------------------------

pub struct PlanEnterTool(pub SharedPlan);

#[async_trait]
impl Tool for PlanEnterTool {
    fn name(&self) -> &str {
        "plan_enter"
    }
    fn description(&self) -> &str {
        "Enter planning mode. Provide a plan_path where the final plan will be saved. \
         Resets any existing in-memory steps."
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "plan_path": {
                    "type": "string",
                    "description": "Path to write the final plan to (must be inside the project root)."
                },
                "strict_mode": {
                    "type": "boolean",
                    "description": "If true, plan_exit requires all steps to be approved (via plan_approve) before exiting. Default: false.",
                    "default": false
                }
            },
            "required": ["plan_path"]
        })
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        _rt: &dyn crate::runtime::Runtime,
    ) -> AgentResult<String> {
        let path = args
            .get("plan_path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentError::Tool("missing plan_path".into()))?
            .to_string();
        // Read optional strict_mode flag (default false — backward compat).
        let strict = args
            .get("strict_mode")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        self.0
            .strict_mode
            .store(strict, std::sync::atomic::Ordering::Relaxed);

        // Reset steps (mirror the Extension hook's behavior).
        if let Ok(mut g) = self.0.plan_steps.lock() {
            g.clear();
        }
        // Remember the path (the Extension hook also does this, but we set it
        // here too so the Tool-side state is consistent even if the hook is
        // ever bypassed).
        if let Ok(mut g) = self.0.plan_path.lock() {
            *g = Some(path.clone());
        }

        // Create an empty file as a side effect (so the user can see it exists
        // even before plan_add writes content). Best-effort.
        let _ = std::fs::write(&path, "");

        Ok(format!(
            "{{\"status\":\"ok\",\"plan_path\":\"{}\",\"mode\":\"plan\",\"strict_mode\":{}}}",
            path.replace('"', "\\\""),
            strict
        ))
    }
}

// ---------------------------------------------------------------------------
// plan_exit
// ---------------------------------------------------------------------------

pub struct PlanExitTool(pub SharedPlan);

#[async_trait]
impl Tool for PlanExitTool {
    fn name(&self) -> &str {
        "plan_exit"
    }
    fn description(&self) -> &str {
        "Exit planning mode and return to normal workflow. Persists the current \
         plan to the file path set by plan_enter."
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        _rt: &dyn crate::runtime::Runtime,
    ) -> AgentResult<String> {
        // strict_mode gate: if enabled, ALL non-empty steps must be approved
        // (or already done) before exiting. This check lives in execute()
        // (not after_tool_call) because agent.call_tool() — the RPC path —
        // doesn't invoke after_tool_call. Only agent.run() does.
        let strict = self
            .0
            .strict_mode
            .load(std::sync::atomic::Ordering::Relaxed);
        if strict {
            let unapproved: Vec<usize> = self
                .0
                .plan_steps
                .lock()
                .map(|g| {
                    g.iter()
                        .enumerate()
                        .filter(|(_, s)| !s.approved && !s.done)
                        .map(|(i, _)| i)
                        .collect()
                })
                .unwrap_or_default();
            if !unapproved.is_empty() {
                let list: Vec<String> = unapproved.iter().map(|i| i.to_string()).collect();
                return Ok(format!(
                    "{{\"status\":\"blocked\",\"reason\":\"strict_mode: steps [{}] not approved. \
                     Have the user call plan_approve for each, then retry plan_exit.\"}}",
                    list.join(",")
                ));
            }
        }
        // Persist current plan to disk.
        let path = self.0.plan_path.lock().ok().and_then(|g| g.clone());
        let steps = self
            .0
            .plan_steps
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();
        let body = PlanExtension::render_steps_pub(&steps);
        if let Some(p) = &path {
            let _ = std::fs::write(p, &body);
        }
        // The Extension hook will also flip plan_mode off; we don't touch the
        // atomic here to avoid races.
        Ok(format!(
            "{{\"status\":\"ok\",\"mode\":\"normal\",\"persisted\":{}}}",
            if path.is_some() { "true" } else { "false" }
        ))
    }
}

// ---------------------------------------------------------------------------
// plan_add {step: string}
// ---------------------------------------------------------------------------

pub struct PlanAddTool(pub SharedPlan);

#[async_trait]
impl Tool for PlanAddTool {
    fn name(&self) -> &str {
        "plan_add"
    }
    fn description(&self) -> &str {
        "Append a step to the plan. Returns the 0-based index of the new step."
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "step": {"type": "string", "description": "The step description."}
            },
            "required": ["step"]
        })
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        _rt: &dyn crate::runtime::Runtime,
    ) -> AgentResult<String> {
        let step = args
            .get("step")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentError::Tool("missing step".into()))?
            .to_string();

        let index = if let Ok(mut g) = self.0.plan_steps.lock() {
            let i = g.len();
            g.push(PlanStep::new(step.clone()));
            i
        } else {
            0
        };

        // Best-effort persist after each add (so the file always reflects
        // current state, useful if the agent forgets plan_exit).
        let path = self.0.plan_path.lock().ok().and_then(|g| g.clone());
        if let Some(p) = path {
            let steps = self
                .0
                .plan_steps
                .lock()
                .map(|g| g.clone())
                .unwrap_or_default();
            let body = PlanExtension::render_steps_pub(&steps);
            let _ = std::fs::write(&p, body);
        }

        Ok(format!(
            "{{\"status\":\"added\",\"step\":\"{}\",\"index\":{}}}",
            step.replace('"', "\\\""),
            index
        ))
    }
}

// ---------------------------------------------------------------------------
// plan_list
// ---------------------------------------------------------------------------

pub struct PlanListTool(pub SharedPlan);

#[async_trait]
impl Tool for PlanListTool {
    fn name(&self) -> &str {
        "plan_list"
    }
    fn description(&self) -> &str {
        "List all steps in the plan. Done steps are prefixed with [x]."
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        _rt: &dyn crate::runtime::Runtime,
    ) -> AgentResult<String> {
        let steps = self
            .0
            .plan_steps
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();
        // Build JSON array of step objects with status flags.
        // [x] = done, [a] = approved (not yet done), [ ] = pending.
        let arr: Vec<String> = steps
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let mark = if s.done {
                    "[x]"
                } else if s.approved {
                    "[a]"
                } else {
                    "[ ]"
                };
                let rendered = format!("{} {}", mark, s.text);
                format!(
                    "{{\"index\":{},\"text\":\"{}\",\"done\":{},\"approved\":{}}}",
                    i,
                    rendered.replace('\\', "\\\\").replace('"', "\\\""),
                    s.done,
                    s.approved
                )
            })
            .collect();
        Ok(format!(
            "{{\"steps\":[{}],\"count\":{}}}",
            arr.join(","),
            steps.len()
        ))
    }
}

// ---------------------------------------------------------------------------
// plan_done {index: number}
// ---------------------------------------------------------------------------

pub struct PlanDoneTool(pub SharedPlan);

#[async_trait]
impl Tool for PlanDoneTool {
    fn name(&self) -> &str {
        "plan_done"
    }
    fn description(&self) -> &str {
        "Mark the step at the given 0-based index as done."
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "index": {"type": "integer", "description": "0-based step index."}
            },
            "required": ["index"]
        })
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        _rt: &dyn crate::runtime::Runtime,
    ) -> AgentResult<String> {
        let index = args
            .get("index")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| AgentError::Tool("missing or invalid index".into()))?
            as usize;

        let mut found = false;
        let not_approved_in_strict = if let Ok(g) = self.0.plan_steps.lock() {
            if index < g.len() {
                let strict = self
                    .0
                    .strict_mode
                    .load(std::sync::atomic::Ordering::Relaxed);
                strict && !g[index].approved
            } else {
                false
            }
        } else {
            false
        };

        if not_approved_in_strict {
            return Ok(format!(
                "{{\"status\":\"error\",\"reason\":\"step {} is not approved (strict_mode). Call plan_approve first, or have the user approve it.\"}}",
                index
            ));
        }

        if let Ok(mut g) = self.0.plan_steps.lock()
            && index < g.len() {
                if !g[index].approved {
                    // Auto-approve in default mode (no human gate).
                    g[index].approved = true;
                }
                g[index].done = true;
                found = true;
            }

        // Best-effort persist.
        let path = self.0.plan_path.lock().ok().and_then(|g| g.clone());
        if let Some(p) = path {
            let steps = self
                .0
                .plan_steps
                .lock()
                .map(|g| g.clone())
                .unwrap_or_default();
            let body = PlanExtension::render_steps_pub(&steps);
            let _ = std::fs::write(&p, body);
        }

        if found {
            Ok(format!("{{\"status\":\"done\",\"index\":{}}}", index))
        } else {
            Ok(format!(
                "{{\"status\":\"error\",\"reason\":\"index {} out of range (plan has {} steps)\"}}",
                index,
                self.0.plan_steps.lock().map(|g| g.len()).unwrap_or(0)
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::LocalRuntime;

    fn rt() -> LocalRuntime {
        LocalRuntime::new()
    }

    #[tokio::test]
    async fn plan_enter_returns_ok_and_sets_path() {
        let shared: SharedPlan = Arc::new(PlanExtension::new());
        let t = PlanEnterTool(shared.clone());
        let r = t
            .execute(
                serde_json::json!({"plan_path": "/tmp/_test_plan.md"}),
                &rt(),
            )
            .await
            .unwrap();
        assert!(r.contains("\"status\":\"ok\""));
        assert!(r.contains("\"mode\":\"plan\""));
        // path remembered
        let p = shared.plan_path.lock().unwrap().clone();
        assert_eq!(p.as_deref(), Some("/tmp/_test_plan.md"));
        let _ = std::fs::remove_file("/tmp/_test_plan.md");
    }

    #[tokio::test]
    async fn plan_add_appends_and_returns_index() {
        let shared: SharedPlan = Arc::new(PlanExtension::new());
        let add = PlanAddTool(shared.clone());
        let r1 = add
            .execute(serde_json::json!({"step": "first"}), &rt())
            .await
            .unwrap();
        assert!(r1.contains("\"index\":0"));
        let r2 = add
            .execute(serde_json::json!({"step": "second"}), &rt())
            .await
            .unwrap();
        assert!(r2.contains("\"index\":1"));
        assert_eq!(shared.plan_steps.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn plan_list_returns_all_steps() {
        let shared: SharedPlan = Arc::new(PlanExtension::new());
        let add = PlanAddTool(shared.clone());
        let _ = add
            .execute(serde_json::json!({"step": "alpha"}), &rt())
            .await;
        let _ = add
            .execute(serde_json::json!({"step": "beta"}), &rt())
            .await;
        let list = PlanListTool(shared.clone());
        let r = list.execute(serde_json::json!({}), &rt()).await.unwrap();
        assert!(r.contains("alpha"));
        assert!(r.contains("beta"));
        assert!(r.contains("\"count\":2"));
    }

    #[tokio::test]
    async fn plan_done_marks_step_and_shows_x() {
        let shared: SharedPlan = Arc::new(PlanExtension::new());
        let add = PlanAddTool(shared.clone());
        let _ = add
            .execute(serde_json::json!({"step": "do thing"}), &rt())
            .await;
        let done = PlanDoneTool(shared.clone());
        let r = done
            .execute(serde_json::json!({"index": 0}), &rt())
            .await
            .unwrap();
        assert!(r.contains("\"status\":\"done\""));
        // list should now show [x]
        let list = PlanListTool(shared);
        let r = list.execute(serde_json::json!({}), &rt()).await.unwrap();
        assert!(r.contains("[x] do thing"));
    }

    #[tokio::test]
    async fn plan_done_out_of_range_returns_error_status() {
        let shared: SharedPlan = Arc::new(PlanExtension::new());
        let done = PlanDoneTool(shared);
        let r = done
            .execute(serde_json::json!({"index": 99}), &rt())
            .await
            .unwrap();
        assert!(r.contains("\"status\":\"error\""));
    }

    #[tokio::test]
    async fn plan_exit_persists_to_file() {
        let path = "/tmp/_ion_plan_exit_test.md";
        let _ = std::fs::remove_file(path);
        let shared: SharedPlan = Arc::new(PlanExtension::new());
        let enter = PlanEnterTool(shared.clone());
        let _ = enter
            .execute(serde_json::json!({"plan_path": path}), &rt())
            .await;
        let add = PlanAddTool(shared.clone());
        let _ = add
            .execute(serde_json::json!({"step": "persisted step"}), &rt())
            .await;
        let exit = PlanExitTool(shared);
        let r = exit.execute(serde_json::json!({}), &rt()).await.unwrap();
        assert!(r.contains("\"persisted\":true"));
        let body = std::fs::read_to_string(path).unwrap();
        assert!(body.contains("persisted step"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn plan_tools_returns_six_tools() {
        // enter + exit + add + list + approve + done = 6 (added approve for Q4)
        let v = plan_tools();
        assert_eq!(v.len(), 6);
    }
}
