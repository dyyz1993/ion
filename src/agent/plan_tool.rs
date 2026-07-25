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
pub fn plan_tools() -> Vec<Box<dyn Tool>> {
    // Note: PlanExtension is also registered separately as an Extension so
    // its before/after_tool_call hooks fire. The Tool side just mutates state.
    let shared: SharedPlan = Arc::new(PlanExtension::new());
    vec![
        Box::new(PlanEnterTool(shared.clone())),
        Box::new(PlanExitTool(shared.clone())),
        Box::new(PlanAddTool(shared.clone())),
        Box::new(PlanListTool(shared.clone())),
        Box::new(PlanDoneTool(shared)),
    ]
}

// Note on state divergence: the PlanExtension registered as an Extension and
// the PlanExtension inside SharedPlan are *separate instances*. This is fine
// because:
//   - plan_enter/exit mode switching is handled by the *Extension* instance's
//     after_tool_call hook (it observes the tool name).
//   - plan_add/list/done only touch the in-memory step list, which lives in
//     the *Tool side* SharedPlan instance.
//   - plan_enter writes the path into BOTH instances (Extension hook + Tool
//     execute), keeping them consistent for path/mode.
//
// If deeper coupling is needed later, the ExtensionRegistry could expose the
// Extension instance and the tools could share it. For now this is simple
// and correct.

// ---------------------------------------------------------------------------
// plan_enter {plan_path: string}
// ---------------------------------------------------------------------------

pub struct PlanEnterTool(pub SharedPlan);

#[async_trait]
impl Tool for PlanEnterTool {
    fn name(&self) -> &str { "plan_enter" }
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
                }
            },
            "required": ["plan_path"]
        })
    }
    async fn execute(&self, args: serde_json::Value, _rt: &dyn crate::runtime::Runtime) -> AgentResult<String> {
        let path = args.get("plan_path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentError::Tool("missing plan_path".into()))?
            .to_string();

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
            "{{\"status\":\"ok\",\"plan_path\":\"{}\",\"mode\":\"plan\"}}",
            path.replace('"', "\\\"")
        ))
    }
}

// ---------------------------------------------------------------------------
// plan_exit
// ---------------------------------------------------------------------------

pub struct PlanExitTool(pub SharedPlan);

#[async_trait]
impl Tool for PlanExitTool {
    fn name(&self) -> &str { "plan_exit" }
    fn description(&self) -> &str {
        "Exit planning mode and return to normal workflow. Persists the current \
         plan to the file path set by plan_enter."
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    async fn execute(&self, _args: serde_json::Value, _rt: &dyn crate::runtime::Runtime) -> AgentResult<String> {
        // Persist current plan to disk.
        let path = self.0.plan_path.lock().ok().and_then(|g| g.clone());
        let steps = self.0.plan_steps.lock().map(|g| g.clone()).unwrap_or_default();
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
    fn name(&self) -> &str { "plan_add" }
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
    async fn execute(&self, args: serde_json::Value, _rt: &dyn crate::runtime::Runtime) -> AgentResult<String> {
        let step = args.get("step")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentError::Tool("missing step".into()))?
            .to_string();

        let index = if let Ok(mut g) = self.0.plan_steps.lock() {
            let i = g.len();
            g.push(PlanStep { text: step.clone(), done: false });
            i
        } else {
            0
        };

        // Best-effort persist after each add (so the file always reflects
        // current state, useful if the agent forgets plan_exit).
        let path = self.0.plan_path.lock().ok().and_then(|g| g.clone());
        if let Some(p) = path {
            let steps = self.0.plan_steps.lock().map(|g| g.clone()).unwrap_or_default();
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
    fn name(&self) -> &str { "plan_list" }
    fn description(&self) -> &str { "List all steps in the plan. Done steps are prefixed with [x]." }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    async fn execute(&self, _args: serde_json::Value, _rt: &dyn crate::runtime::Runtime) -> AgentResult<String> {
        let steps = self.0.plan_steps.lock().map(|g| g.clone()).unwrap_or_default();
        // Build JSON array of step strings (with [x] prefix for done).
        let arr: Vec<String> = steps.iter().map(|s| {
            let rendered = if s.done { format!("[x] {}", s.text) } else { s.text.clone() };
            format!("\"{}\"", rendered.replace('\\', "\\\\").replace('"', "\\\""))
        }).collect();
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
    fn name(&self) -> &str { "plan_done" }
    fn description(&self) -> &str { "Mark the step at the given 0-based index as done." }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "index": {"type": "integer", "description": "0-based step index."}
            },
            "required": ["index"]
        })
    }
    async fn execute(&self, args: serde_json::Value, _rt: &dyn crate::runtime::Runtime) -> AgentResult<String> {
        let index = args.get("index")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| AgentError::Tool("missing or invalid index".into()))?
            as usize;

        let mut found = false;
        if let Ok(mut g) = self.0.plan_steps.lock() {
            if index < g.len() {
                g[index].done = true;
                found = true;
            }
        }

        // Best-effort persist.
        let path = self.0.plan_path.lock().ok().and_then(|g| g.clone());
        if let Some(p) = path {
            let steps = self.0.plan_steps.lock().map(|g| g.clone()).unwrap_or_default();
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

    fn rt() -> LocalRuntime { LocalRuntime::new() }

    #[tokio::test]
    async fn plan_enter_returns_ok_and_sets_path() {
        let shared: SharedPlan = Arc::new(PlanExtension::new());
        let t = PlanEnterTool(shared.clone());
        let r = t.execute(serde_json::json!({"plan_path": "/tmp/_test_plan.md"}), &rt()).await.unwrap();
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
        let r1 = add.execute(serde_json::json!({"step": "first"}), &rt()).await.unwrap();
        assert!(r1.contains("\"index\":0"));
        let r2 = add.execute(serde_json::json!({"step": "second"}), &rt()).await.unwrap();
        assert!(r2.contains("\"index\":1"));
        assert_eq!(shared.plan_steps.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn plan_list_returns_all_steps() {
        let shared: SharedPlan = Arc::new(PlanExtension::new());
        let add = PlanAddTool(shared.clone());
        let _ = add.execute(serde_json::json!({"step": "alpha"}), &rt()).await;
        let _ = add.execute(serde_json::json!({"step": "beta"}), &rt()).await;
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
        let _ = add.execute(serde_json::json!({"step": "do thing"}), &rt()).await;
        let done = PlanDoneTool(shared.clone());
        let r = done.execute(serde_json::json!({"index": 0}), &rt()).await.unwrap();
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
        let r = done.execute(serde_json::json!({"index": 99}), &rt()).await.unwrap();
        assert!(r.contains("\"status\":\"error\""));
    }

    #[tokio::test]
    async fn plan_exit_persists_to_file() {
        let path = "/tmp/_ion_plan_exit_test.md";
        let _ = std::fs::remove_file(path);
        let shared: SharedPlan = Arc::new(PlanExtension::new());
        let enter = PlanEnterTool(shared.clone());
        let _ = enter.execute(serde_json::json!({"plan_path": path}), &rt()).await;
        let add = PlanAddTool(shared.clone());
        let _ = add.execute(serde_json::json!({"step": "persisted step"}), &rt()).await;
        let exit = PlanExitTool(shared);
        let r = exit.execute(serde_json::json!({}), &rt()).await.unwrap();
        assert!(r.contains("\"persisted\":true"));
        let body = std::fs::read_to_string(path).unwrap();
        assert!(body.contains("persisted step"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn plan_tools_returns_five_tools() {
        let v = plan_tools();
        assert_eq!(v.len(), 5);
    }
}
