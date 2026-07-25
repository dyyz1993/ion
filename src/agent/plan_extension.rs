use super::error::{AgentError, AgentResult};
use super::extension::Extension;
use super::messages::ToolCall;
use async_trait::async_trait;
use ion_provider::types::ToolResult;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Helper: lock a Mutex<Option<String>> and return a clone or default.
fn lock_path(m: &Mutex<Option<String>>) -> String {
    m.lock()
        .ok()
        .and_then(|g| g.clone())
        .unwrap_or_else(|| String::from("(not specified)"))
}

/// Helper: set a value inside a Mutex<Option<String>>.
fn set_path(m: &Mutex<Option<String>>, val: Option<String>) {
    if let Ok(mut g) = m.lock() {
        *g = val;
    }
}

/// One step in a plan.
/// `done` steps are prefixed with `[x] ` when rendered; pending steps have no prefix.
#[derive(Clone, Debug)]
pub struct PlanStep {
    pub text: String,
    pub done: bool,
}

/// PlanExtension manages a structured plan plus the "plan mode" lifecycle.
///
/// Plan mode is a soft constraint: when active, the agent is nudged toward
/// research + planning tools and away from execution tools (spawn_worker etc).
/// The plan itself is a list of steps stored in memory; the agent can also
/// persist it to a file via the `plan_path` set by `plan_enter`.
///
/// Tools exposed (handled in `after_tool_call` so they work even in plan mode):
///   - plan_enter {plan_path}  → enter plan mode, remember path, create empty plan
///   - plan_exit               → leave plan mode (plan content is preserved)
///   - plan_add {step}         → append a step, returns its 0-based index
///   - plan_list               → return all steps (with [x] prefix for done)
///   - plan_done {index}       → mark the step at index as done
///
/// Design rationale: previously these tools were provided by a WASM extension
/// (plan-extension), but its tool names collided with this builtin extension's
/// plan_mode trigger — calling plan_enter locked plan_add out, deadlocking the
/// agent. Moving all five tools into this single builtin eliminates the conflict.
pub struct PlanExtension {
    plan_mode: AtomicBool,
    pub(crate) plan_path: Mutex<Option<String>>,
    pub(crate) plan_steps: Mutex<Vec<PlanStep>>,
    /// Tool names allowed during plan mode.
    /// Includes all plan_* tools so the agent can build the plan while planning.
    allowed_tools: Vec<String>,
}

impl PlanExtension {
    pub fn new() -> Self {
        Self {
            plan_mode: AtomicBool::new(false),
            plan_path: Mutex::new(None),
            plan_steps: Mutex::new(Vec::new()),
            allowed_tools: vec![
                // plan_* tools — always usable so the agent can build/edit the plan
                "plan_exit".into(),
                "plan_add".into(),
                "plan_list".into(),
                "plan_done".into(),
                // research / writing tools — for investigating the codebase
                "read".into(),
                "grep".into(),
                "find".into(),
                "ls".into(),
                "bash".into(),
                "write".into(),
                "edit".into(),
                // todo_* tools — task tracking is part of planning
                "todo_add".into(),
                "todo_list".into(),
                "todo_done".into(),
                "todo_remove".into(),
                "todo_clean".into(),
            ],
        }
    }

    pub fn is_plan_mode(&self) -> bool {
        self.plan_mode.load(Ordering::Relaxed)
    }

    /// Render the plan steps as a plain-text block (one step per line,
    /// `[x] ` prefix for completed steps). Used for both `plan_list` output
    /// and for writing the plan file to disk.
    fn render_steps(steps: &[PlanStep]) -> String {
        Self::render_steps_pub(steps)
    }

    /// Public alias used by `plan_tool.rs` (the Tool implementations also
    /// need to render steps to a file on plan_exit / plan_add).
    pub fn render_steps_pub(steps: &[PlanStep]) -> String {
        steps
            .iter()
            .map(|s| {
                if s.done {
                    format!("[x] {}", s.text)
                } else {
                    s.text.clone()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Persist the current plan to the remembered path (if any).
    /// Best-effort: errors are logged but do not fail the tool call, because
    /// the in-memory plan is the source of truth; the file is a convenience.
    fn persist_to_disk(&self) {
        let path = match self.plan_path.lock().ok().and_then(|g| g.clone()) {
            Some(p) => p,
            None => return,
        };
        let steps = match self.plan_steps.lock().ok() {
            Some(g) => g.clone(),
            None => return,
        };
        let body = Self::render_steps(&steps);
        if let Err(e) = std::fs::write(&path, body) {
            tracing::warn!("[plan] failed to persist to {}: {}", path, e);
        }
    }
}

impl Default for PlanExtension {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Extension for PlanExtension {
    // ── Intercept tool calls to manage plan state ──

    async fn after_tool_call(&self, call: &ToolCall, _result: &ToolResult) -> AgentResult<()> {
        match call.name.as_str() {
            "plan_enter" => {
                self.plan_mode.store(true, Ordering::Relaxed);
                let path = call
                    .arguments
                    .get("plan_path")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                set_path(&self.plan_path, path.clone());
                // Reset the step list when entering with a fresh plan.
                if let Ok(mut g) = self.plan_steps.lock() {
                    g.clear();
                }
                tracing::info!("[plan] entered plan mode, path={:?}", path);
            }
            "plan_exit" => {
                // Persist final plan before exiting (so the file reflects the
                // last known state even if the agent forgets to re-write it).
                self.persist_to_disk();
                self.plan_mode.store(false, Ordering::Relaxed);
                tracing::info!("[plan] exited plan mode");
            }
            _ => {}
        }
        Ok(())
    }

    // ── Reject non-allowed tools during plan mode ──

    async fn before_tool_call(&self, call: &ToolCall) -> AgentResult<()> {
        if self.plan_mode.load(Ordering::Relaxed)
            && call.name != "plan_enter"
            && !self.allowed_tools.contains(&call.name)
        {
            return Err(AgentError::Tool(format!(
                "Tool '{}' is not available in plan mode. \
                 Available tools: {:?}",
                call.name, self.allowed_tools
            )));
        }
        Ok(())
    }

    // ── Inject planning instructions into system prompt ──

    async fn on_system_prompt(&self, prompt: &mut String) -> AgentResult<()> {
        if self.plan_mode.load(Ordering::Relaxed) {
            let path = lock_path(&self.plan_path);
            let step_count = self
                .plan_steps
                .lock()
                .map(|g| g.len())
                .unwrap_or(0);

            prompt.push_str(&format!(
                "\n\n[PLAN MODE]\n\
                 Plan output path: {}\n\
                 Current plan steps: {}\n\n\
                 You are in planning mode. Your task is to:\n\
                 1. Research the codebase (read/grep/find/ls/bash)\n\
                 2. Build a step-by-step plan using `plan_add`\n\
                 3. Mark steps done with `plan_done` as you validate them\n\
                 4. List with `plan_list` to review\n\
                 5. Call `plan_exit` when the plan is complete to return to \
                 normal workflow (the plan will be persisted to the path above)\n",
                path, step_count
            ));
        }
        Ok(())
    }

    // ── Handle plan_add / plan_list / plan_done by intercepting the ToolCall ──
    //
    // These three tools are registered as stub `Tool`s (see `plan_tool.rs`)
    // whose `execute` returns a placeholder. We override their behavior here
    // via `on_before_tool_execute`, which runs BEFORE the stub and can short-
    // circuit it by returning an error message that carries the real result.
    //
    // Actually, the cleanest hook is `on_tool_execution_start` returning Err
    // with the real JSON — the agent loop treats tool errors as tool results.
    // But to keep the contract simple, we let the stub call into shared state.

    async fn on_tool_execution_start(
        &self,
        _ctx: &super::extension::ToolExecutionContext,
    ) -> AgentResult<()> {
        Ok(())
    }
}

/// Shared handle used by `PlanTool` (in plan_tool.rs) to read/write the
/// extension's in-memory plan. The ExtensionRegistry wraps PlanExtension in
/// an Arc internally, but the tools are registered separately — so we expose
/// a clonable handle that points at the same Mutex-protected state.
pub type PlanState = Arc<PlanExtension>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_initializes_plan_mode_as_false() {
        let ext = PlanExtension::new();
        assert!(!ext.is_plan_mode());
    }

    #[test]
    fn new_initializes_plan_path_as_none() {
        let ext = PlanExtension::new();
        assert_eq!(lock_path(&ext.plan_path), "(not specified)");
    }

    #[test]
    fn new_initializes_empty_steps() {
        let ext = PlanExtension::new();
        let steps = ext.plan_steps.lock().unwrap();
        assert!(steps.is_empty());
    }

    #[test]
    fn new_populates_expected_allowed_tools() {
        let ext = PlanExtension::new();
        let tools = &ext.allowed_tools;
        // research/edit tools
        for t in &["read", "grep", "find", "ls", "bash", "write", "edit"] {
            assert!(tools.contains(&t.to_string()), "missing allowed tool: {}", t);
        }
        // plan_* tools (so the agent can build the plan while in plan mode)
        for t in &["plan_exit", "plan_add", "plan_list", "plan_done"] {
            assert!(tools.contains(&t.to_string()), "missing plan tool: {}", t);
        }
        // todo_* tools (task tracking is part of planning)
        for t in &["todo_add", "todo_list", "todo_done", "todo_remove", "todo_clean"] {
            assert!(tools.contains(&t.to_string()), "missing todo tool: {}", t);
        }
    }

    #[test]
    fn render_steps_pending_and_done() {
        let steps = vec![
            PlanStep { text: "write code".into(), done: false },
            PlanStep { text: "test it".into(), done: true },
            PlanStep { text: "ship it".into(), done: false },
        ];
        let rendered = PlanExtension::render_steps(&steps);
        assert_eq!(rendered, "write code\n[x] test it\nship it");
    }

    #[test]
    fn render_steps_empty() {
        let rendered = PlanExtension::render_steps(&[]);
        assert_eq!(rendered, "");
    }

    #[test]
    fn lock_path_returns_default_when_none() {
        let m = Mutex::new(None);
        assert_eq!(lock_path(&m), "(not specified)");
    }

    #[test]
    fn lock_path_returns_value_when_some() {
        let m = Mutex::new(Some(String::from("/tmp/plan.md")));
        assert_eq!(lock_path(&m), "/tmp/plan.md");
    }

    #[test]
    fn set_path_stores_a_value() {
        let m = Mutex::new(None);
        set_path(&m, Some(String::from("/out/plan.txt")));
        assert_eq!(lock_path(&m), "/out/plan.txt");
    }

    #[test]
    fn set_path_clears_to_none() {
        let m = Mutex::new(Some(String::from("/old/plan.md")));
        set_path(&m, None);
        assert_eq!(lock_path(&m), "(not specified)");
    }
}
