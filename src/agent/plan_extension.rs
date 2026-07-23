use super::error::AgentResult;
use super::extension::Extension;
use super::messages::ToolCall;
use async_trait::async_trait;
use ion_provider::types::ToolResult;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

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

/// PlanExtension manages the "plan mode" lifecycle.
///
/// When `plan_enter` is called:
/// - Sets plan_mode = true
/// - Saves the plan output path
/// - On subsequent turns, injects planning instructions into the system prompt
/// - Restricts available tools to research/write tools only
///
/// When `plan_exit` is called:
/// - Sets plan_mode = false
/// - Stops injecting plan instructions
/// - All tools become available again
pub struct PlanExtension {
    plan_mode: AtomicBool,
    plan_path: Mutex<Option<String>>,
    /// Tool names allowed during plan mode.
    /// Note: "plan_exit" is included so the agent can exit plan mode.
    allowed_tools: Vec<String>,
}

impl PlanExtension {
    pub fn new() -> Self {
        Self {
            plan_mode: AtomicBool::new(false),
            plan_path: Mutex::new(None),
            allowed_tools: vec![
                "plan_exit".into(),
                "read".into(),
                "grep".into(),
                "find".into(),
                "ls".into(),
                "bash".into(),
                "write".into(),
                "edit".into(),
            ],
        }
    }

    pub fn is_plan_mode(&self) -> bool {
        self.plan_mode.load(Ordering::Relaxed)
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
                tracing::info!("[plan] entered plan mode, path={:?}", path);
            }
            "plan_exit" => {
                self.plan_mode.store(false, Ordering::Relaxed);
                set_path(&self.plan_path, None);
                tracing::info!("[plan] exited plan mode");
            }
            _ => {}
        }
        Ok(())
    }

    // ── Reject non-allowed tools during plan mode ──

    async fn before_tool_call(&self, call: &ToolCall) -> AgentResult<()> {
        if self.plan_mode.load(Ordering::Relaxed) && call.name != "plan_enter" && !self.allowed_tools.contains(&call.name) {
            return Err(super::error::AgentError::Tool(format!(
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

            prompt.push_str(&format!(
                "\n\n[PLAN MODE]\n\
                 Plan output path: {}\n\n\
                 You are currently in planning mode. Your task is to:\n\
                 1. Research the codebase to understand the current state and requirements\n\
                 2. Create a detailed, step-by-step plan covering all files \
                 that need to be created or modified\n\
                 3. Write the complete plan to the specified path using the `write` tool\n\
                 4. Call `plan_exit` when done to return to normal agent workflow\n\n\
                 Available tools: read, grep, find, ls, bash, write, edit, plan_exit\n",
                path
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_initializes_plan_mode_as_false() {
        // A freshly created extension must not be in plan mode.
        let ext = PlanExtension::new();
        assert!(!ext.is_plan_mode());
    }

    #[test]
    fn new_initializes_plan_path_as_none() {
        // Internal invariant: the path mutex starts empty.
        let ext = PlanExtension::new();
        assert_eq!(lock_path(&ext.plan_path), "(not specified)");
    }

    #[test]
    fn new_populates_expected_allowed_tools() {
        // The default allow-list should contain the research/edit tools and plan_exit.
        let ext = PlanExtension::new();
        let tools = &ext.allowed_tools;
        assert!(tools.contains(&"read".to_string()));
        assert!(tools.contains(&"grep".to_string()));
        assert!(tools.contains(&"find".to_string()));
        assert!(tools.contains(&"ls".to_string()));
        assert!(tools.contains(&"bash".to_string()));
        assert!(tools.contains(&"write".to_string()));
        assert!(tools.contains(&"edit".to_string()));
        assert!(tools.contains(&"plan_exit".to_string()));
    }

    #[test]
    fn allowed_tools_has_expected_count() {
        // Guard against accidentally adding/removing tools without intent.
        let ext = PlanExtension::new();
        assert_eq!(ext.allowed_tools.len(), 8);
    }

    #[test]
    fn is_plan_mode_reflects_atomic_state() {
        // Manually flip the flag and confirm is_plan_mode reads it.
        let ext = PlanExtension::new();
        ext.plan_mode.store(true, Ordering::Relaxed);
        assert!(ext.is_plan_mode());
        ext.plan_mode.store(false, Ordering::Relaxed);
        assert!(!ext.is_plan_mode());
    }

    #[test]
    fn lock_path_returns_default_when_none() {
        // When the mutex holds None, lock_path yields the placeholder.
        let m = Mutex::new(None);
        assert_eq!(lock_path(&m), "(not specified)");
    }

    #[test]
    fn lock_path_returns_value_when_some() {
        // When the mutex holds Some(path), lock_path yields the stored string.
        let m = Mutex::new(Some(String::from("/tmp/plan.md")));
        assert_eq!(lock_path(&m), "/tmp/plan.md");
    }

    #[test]
    fn set_path_stores_a_value() {
        // After setting Some, lock_path should observe the new value.
        let m = Mutex::new(None);
        set_path(&m, Some(String::from("/out/plan.txt")));
        assert_eq!(lock_path(&m), "/out/plan.txt");
    }

    #[test]
    fn set_path_clears_to_none() {
        // Setting None after a value should reset back to the default placeholder.
        let m = Mutex::new(Some(String::from("/old/plan.md")));
        set_path(&m, None);
        assert_eq!(lock_path(&m), "(not specified)");
    }
}
