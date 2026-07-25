//! Monitor Extension — scheduled script monitors that trigger LLM conversations.
//!
//! A singleton extension for scenario 3 (ion serve) that:
//! 1. Loads monitor definitions from `.ion/monitors/*.json`
//! 2. Runs each monitor's bash script on an interval
//! 3. If the script outputs non-empty stdout (exit 0), triggers an LLM conversation
//! 4. Agent can self-manage monitors via extension_rpc (add/remove/enable/disable)
//!
//! This enables autonomous loops: monitor → detect → trigger agent → fix → repeat.

use crate::agent::error::{AgentError, AgentResult};
use crate::agent::extension::Extension;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// A single monitor definition (from .ion/monitors/*.json or added via RPC).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct MonitorDef {
    /// Unique name for this monitor (e.g. "github-issues").
    pub name: String,
    /// Interval in seconds between script runs.
    #[serde(default = "default_interval")]
    pub interval_secs: u64,
    /// Bash script to execute. Non-empty stdout + exit 0 = trigger.
    pub script: String,
    /// Agent to trigger when monitor fires (e.g. "developer").
    #[serde(default = "default_agent")]
    pub agent: String,
    /// Prompt template. {output} is replaced with script stdout.
    #[serde(default = "default_prompt")]
    pub prompt_template: String,
    /// Whether this monitor is active.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_interval() -> u64 { 300 }
fn default_agent() -> String { "developer".into() }
fn default_prompt() -> String { "Monitor triggered:\n{output}".into() }
fn default_enabled() -> bool { true }

/// Runtime status for a monitor.
#[derive(Clone, Debug, serde::Serialize)]
pub struct MonitorStatus {
    pub name: String,
    pub enabled: bool,
    pub last_run: Option<String>,
    pub last_result: String,
    pub trigger_count: u64,
}

/// MonitorExtension — singleton, only registered in serve mode.
pub struct MonitorExtension {
    /// Monitor definitions (shared with interval loops).
    monitors: Arc<Mutex<Vec<MonitorDef>>>,
    /// Runtime statuses.
    statuses: Arc<Mutex<HashMap<String, MonitorStatus>>>,
    name: String,
}

impl MonitorExtension {
    pub fn new() -> Self {
        Self {
            monitors: Arc::new(Mutex::new(Vec::new())),
            statuses: Arc::new(Mutex::new(HashMap::new())),
            name: "monitor".into(),
        }
    }

    /// Load monitor definitions from `.ion/monitors/*.json`.
    fn load_from_dir(dir: &std::path::Path) -> Vec<MonitorDef> {
        let mut result = Vec::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map(|e| e == "json").unwrap_or(false) {
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        if let Ok(def) = serde_json::from_str::<MonitorDef>(&content) {
                            tracing::info!("[monitor] loaded: {} from {}", def.name, path.display());
                            result.push(def);
                        }
                    }
                }
            }
        }
        result
    }

    /// Run a monitor's script and return (success, output).
    fn run_script(script: &str) -> (bool, String) {
        match std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .output()
        {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
                let success = output.status.success();
                (success, stdout)
            }
            Err(e) => (false, format!("script error: {e}")),
        }
    }

    /// Format prompt by replacing {output} with script output.
    fn format_prompt(template: &str, output: &str) -> String {
        template.replace("{output}", output)
    }
}

#[async_trait::async_trait]
impl Extension for MonitorExtension {
    fn name(&self) -> &str { &self.name }

    fn is_singleton(&self) -> bool { true }
    fn singleton_key(&self) -> &str { "monitor" }

    async fn on_singleton_init(&self) -> AgentResult<()> {
        // Load monitor definitions from project .ion/monitors/
        let project_monitors = std::path::Path::new(".ion/monitors");
        let global_monitors = crate::paths::root().join("monitors");

        let mut loaded = Vec::new();
        loaded.extend(Self::load_from_dir(&project_monitors));
        loaded.extend(Self::load_from_dir(&global_monitors));

        tracing::info!("[monitor] loaded {} monitor definition(s)", loaded.len());

        let mut monitors = self.monitors.lock().await;
        *monitors = loaded;
        Ok(())
    }

    async fn on_singleton_post_init(
        &self,
        registry: &Arc<tokio::sync::Mutex<crate::worker_registry::WorkerRegistry>>,
    ) -> AgentResult<()> {
        let monitors = self.monitors.lock().await.clone();
        let statuses = Arc::clone(&self.statuses);

        for def in monitors.into_iter().filter(|m| m.enabled) {
            let reg = Arc::clone(registry);
            let stats = Arc::clone(&statuses);
            let name = def.name.clone();
            let interval = def.interval_secs;
            let script = def.script.clone();
            let agent = def.agent.clone();
            let prompt_tpl = def.prompt_template.clone();

            // Initialize status
            {
                let mut s = stats.lock().await;
                s.insert(name.clone(), MonitorStatus {
                    name: name.clone(),
                    enabled: true,
                    last_run: None,
                    last_result: "starting".into(),
                    trigger_count: 0,
                });
            }

            tracing::info!(
                "[monitor] starting '{}' (interval={}s, agent={})",
                name, interval, agent
            );

            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(
                    tokio::time::Duration::from_secs(interval.max(1))
                );
                ticker.set_missed_tick_behavior(
                    tokio::time::MissedTickBehavior::Skip
                );

                loop {
                    ticker.tick().await;

                    // Run the monitor script
                    let (success, output) = Self::run_script(&script);
                    let now = chrono_or_systime();

                    // Update status
                    {
                        let mut s = stats.lock().await;
                        if let Some(status) = s.get_mut(&name) {
                            status.last_run = Some(now.clone());
                            status.last_result = if success {
                                if output.is_empty() { "idle".into() } else { "triggered".into() }
                            } else {
                                "error".into()
                            };
                        }
                    }

                    if !success {
                        tracing::warn!("[monitor] '{}' script failed: {}", name, output);
                        continue;
                    }

                    if output.is_empty() {
                        // No event — idle, keep looping
                        continue;
                    }

                    // Event detected — trigger LLM conversation
                    tracing::info!(
                        "[monitor] '{}' triggered! output={} bytes, triggering agent={}",
                        name, output.len(), agent
                    );

                    // Increment trigger count
                    {
                        let mut s = stats.lock().await;
                        if let Some(status) = s.get_mut(&name) {
                            status.trigger_count += 1;
                        }
                    }

                    // Build the prompt
                    let prompt = Self::format_prompt(&prompt_tpl, &output);

                    // Send to a worker — try to find an existing one, or create new
                    {
                        // First: check for idle worker (brief lock)
                        let idle_worker = {
                            let reg_guard = reg.lock().await;
                            reg_guard.workers.iter()
                                .find(|(_, w)| w.agent == agent
                                    && w.status != crate::worker_registry::WorkerStatus::Dead
                                    && w.status != crate::worker_registry::WorkerStatus::Busy)
                                .map(|(id, _)| id.clone())
                        };

                        if let Some(wid) = idle_worker {
                            // Send prompt to existing idle worker
                            let mut reg_guard = reg.lock().await;
                            let _ = reg_guard.send_command(&wid, "prompt", serde_json::json!({
                                "text": prompt
                            })).await;
                            tracing::info!("[monitor] sent to existing worker {}", wid);
                        } else {
                            // No idle worker — create a new one (needs registry Arc, not guard)
                            let mut reg_guard = reg.lock().await;
                            match reg_guard.create_worker(
                                crate::worker_registry::WorkerCreateConfig {
                                    agent: Some(agent.clone()),
                                    model: None,
                                    provider: None,
                                    session: None,
                                    project_path: None,
                                    worktree: None,
                                    relation: Some(crate::worker_registry::WorkerRelation::System),
                                    channels: None,
                                    parent: None,
                                    creator: None,
                                    report_channel: None,
                                    report_to: None,
                                    initial_prompt: Some(prompt),
                                    skip_mcp: None,
                                    allowed_tools: None,
                                    disallowed_tools: None,
                                    max_turns: None,
                                    hook_depth: Some(0),
                                    system_prompt_override: None,
                                },
                                &reg,
                            ).await {
                                Ok(info) => tracing::info!(
                                    "[monitor] created worker {} for {}",
                                    info.worker_id, name
                                ),
                                Err(e) => tracing::error!(
                                    "[monitor] failed to create worker: {e}"
                                ),
                            }
                        }
                    }
                }
            });
        }

        Ok(())
    }

    async fn on_extension_rpc(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> AgentResult<serde_json::Value> {
        match method {
            "list" => {
                let monitors = self.monitors.lock().await;
                let stats = self.statuses.lock().await;
                let result: Vec<serde_json::Value> = monitors.iter().map(|m| {
                    let status = stats.get(&m.name);
                    serde_json::json!({
                        "name": m.name,
                        "interval_secs": m.interval_secs,
                        "agent": m.agent,
                        "enabled": m.enabled,
                        "trigger_count": status.map(|s| s.trigger_count).unwrap_or(0),
                        "last_run": status.and_then(|s| s.last_run.clone()),
                        "last_result": status.map(|s| s.last_result.as_str()).unwrap_or("unknown"),
                    })
                }).collect();
                Ok(serde_json::json!({"monitors": result}))
            }

            "add" => {
                let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                if name.is_empty() {
                    return Err(AgentError::Tool("missing 'name'".into()));
                }
                let def = MonitorDef {
                    name: name.clone(),
                    interval_secs: params.get("interval_secs").and_then(|v| v.as_u64()).unwrap_or(300),
                    script: params.get("script").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    agent: params.get("agent").and_then(|v| v.as_str()).unwrap_or("developer").to_string(),
                    prompt_template: params.get("prompt_template").and_then(|v| v.as_str())
                        .unwrap_or("Monitor triggered:\n{output}").to_string(),
                    enabled: true,
                };
                let mut monitors = self.monitors.lock().await;
                monitors.retain(|m| m.name != def.name);
                monitors.push(def.clone());

                // Save to file
                let monitor_dir = std::path::Path::new(".ion/monitors");
                let _ = std::fs::create_dir_all(monitor_dir);
                let path = monitor_dir.join(format!("{name}.json"));
                if let Ok(json) = serde_json::to_string_pretty(&def) {
                    let _ = std::fs::write(path, json);
                }

                Ok(serde_json::json!({"added": name, "note": "restart serve to activate new monitors"}))
            }

            "remove" => {
                let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let mut monitors = self.monitors.lock().await;
                let before = monitors.len();
                monitors.retain(|m| m.name != name);
                let removed = before > monitors.len();

                // Delete file
                let path = std::path::Path::new(".ion/monitors").join(format!("{name}.json"));
                let _ = std::fs::remove_file(path);

                Ok(serde_json::json!({"removed": removed, "name": name}))
            }

            "enable" | "disable" => {
                let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let enable = method == "enable";
                let mut monitors = self.monitors.lock().await;
                let mut found = false;
                for m in monitors.iter_mut() {
                    if m.name == name {
                        m.enabled = enable;
                        found = true;
                        break;
                    }
                }
                if found {
                    Ok(serde_json::json!({"name": name, "enabled": enable}))
                } else {
                    Err(AgentError::Tool(format!("monitor '{name}' not found")))
                }
            }

            "status" => {
                let stats = self.statuses.lock().await;
                let result: Vec<&MonitorStatus> = stats.values().collect();
                Ok(serde_json::json!({"statuses": result}))
            }

            _ => Err(AgentError::Tool(format!("unknown method: {method}")))
        }
    }
}

/// Get current time string without chrono dependency.
fn chrono_or_systime() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => format!("epoch:{}", d.as_secs()),
        Err(_) => "unknown".into(),
    }
}
