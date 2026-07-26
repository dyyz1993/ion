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

// ── v2: concurrency + consumer policy enums ──
//
// `mode` decides what to do when the previous trigger is still in flight:
//   - SerialSkip   : no idle worker -> skip this tick (emit monitor_skipped)
//   - SerialQueue  : no idle worker -> enqueue, replay when worker goes idle
//   - Concurrent   : always spawn, up to `max_concurrent` workers in parallel
//
// `trigger_mode` decides how a positive trigger is consumed:
//   - AutoSpawn     : spawn the configured agent worker directly
//   - ChannelNotify : push rendered prompt to the `main` channel for an
//                     already-running coordinator/developer to pick up
//   - EventOnly     : emit a `monitor_triggered` event but spawn nothing
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MonitorMode {
    SerialSkip,
    SerialQueue,
    Concurrent,
}

impl Default for MonitorMode {
    fn default() -> Self {
        MonitorMode::SerialSkip
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerMode {
    AutoSpawn,
    ChannelNotify,
    EventOnly,
}

impl Default for TriggerMode {
    fn default() -> Self {
        TriggerMode::AutoSpawn
    }
}

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

    // ── v2 fields (concurrency + consumer policy) ──
    /// Concurrency policy (serial_skip | serial_queue | concurrent).
    #[serde(default)]
    pub mode: MonitorMode,
    /// Consumer policy (auto_spawn | channel_notify | event_only).
    #[serde(default)]
    pub trigger_mode: TriggerMode,
    /// Max concurrent workers (concurrent mode cap).
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: u32,
    /// Cooldown seconds between triggers (debounce).
    #[serde(default = "default_cooldown")]
    pub cooldown_secs: u64,
}

fn default_interval() -> u64 { 300 }
fn default_agent() -> String { "developer".into() }
fn default_prompt() -> String { "Monitor triggered:\n{output}".into() }
fn default_enabled() -> bool { true }
fn default_max_concurrent() -> u32 { 3 }
fn default_cooldown() -> u64 { 60 }

/// Default queue capacity for serial_queue mode (overflow protection).
pub const MONITOR_QUEUE_CAPACITY: usize = 10;

/// Validate a monitor name against the safe charset ^[a-zA-Z0-9_-]{1,32}$.
/// This guards against path traversal (e.g. "../../etc/cron.d/evil").
fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 32 {
        return Err(format!(
            "name length must be 1-32, got {}",
            name.len()
        ));
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return Err(format!(
            "name may only contain [a-zA-Z0-9_-], got '{name}'"
        ));
    }
    Ok(())
}

/// Runtime status for a monitor.
#[derive(Clone, Debug, serde::Serialize)]
pub struct MonitorStatus {
    pub name: String,
    pub enabled: bool,
    pub last_run: Option<String>,
    pub last_result: String,
    pub trigger_count: u64,
    // ── v2 fields ──
    pub skip_count: u64,
    pub queue_length: usize,
    pub active_workers: u32,
    pub last_error: Option<String>,
    pub consecutive_failures: u32,
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

    // ===== v2: RPC param parsing + validation helpers (merged from T2) =====

    /// Parse a `MonitorDef` from RPC `params`.
    ///
    /// All fields default gracefully so that `validate_def` is the single
    /// authority on what is or isn't acceptable. Missing required fields land
    /// here as empty strings / sentinel values and surface as validation
    /// errors rather than panics.
    fn parse_def(params: &serde_json::Value) -> MonitorDef {
        MonitorDef {
            name: params.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            interval_secs: params
                .get("interval_secs")
                .and_then(|v| v.as_u64())
                .unwrap_or(default_interval()),
            script: params.get("script").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            agent: params
                .get("agent")
                .and_then(|v| v.as_str())
                .unwrap_or(&default_agent())
                .to_string(),
            prompt_template: params
                .get("prompt_template")
                .and_then(|v| v.as_str())
                .unwrap_or(&default_prompt())
                .to_string(),
            enabled: params.get("enabled").and_then(|v| v.as_bool()).unwrap_or(default_enabled()),
            mode: Self::parse_mode(params.get("mode")),
            trigger_mode: Self::parse_trigger_mode(params.get("trigger_mode")),
            max_concurrent: params
                .get("max_concurrent")
                .and_then(|v| v.as_u64())
                .map(|n| n as u32)
                .unwrap_or(default_max_concurrent()),
            cooldown_secs: params
                .get("cooldown_secs")
                .and_then(|v| v.as_u64())
                .unwrap_or(default_cooldown()),
        }
    }

    /// Decode `mode` from a JSON value using the snake_case rename.
    /// Falls back to the default (SerialSkip) on missing/invalid input.
    fn parse_mode(v: Option<&serde_json::Value>) -> MonitorMode {
        serde_json::from_value(v.cloned().unwrap_or(serde_json::Value::Null))
            .unwrap_or_default()
    }

    /// Decode `trigger_mode` from a JSON value using the snake_case rename.
    /// Falls back to the default (AutoSpawn) on missing/invalid input.
    fn parse_trigger_mode(v: Option<&serde_json::Value>) -> TriggerMode {
        serde_json::from_value(v.cloned().unwrap_or(serde_json::Value::Null))
            .unwrap_or_default()
    }

    /// Run a script capturing stdout, stderr, exit status separately.
    /// Used by the `test` (dry-run) RPC to report full diagnostics.
    fn run_script_capturing(script: &str) -> ScriptRun {
        match std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .output()
        {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                let exit_ok = output.status.success();
                let exit_code = output.status.code().unwrap_or(-1);
                ScriptRun { stdout, stderr, exit_ok, exit_code }
            }
            Err(e) => ScriptRun {
                stdout: String::new(),
                stderr: format!("script spawn error: {e}"),
                exit_ok: false,
                exit_code: -1,
            },
        }
    }

    /// Validate a monitor definition (semantic checks). Returns (errors, warnings).
    /// Implements v2 bugs 1-3 fixes plus prompt_template placeholder check.
    ///
    /// NOTE: `validate_name` is a top-level fn in this file (T1 base), so we
    /// call it directly rather than via `Self::`.
    fn validate_def(def: &MonitorDef) -> (Vec<String>, Vec<String>) {
        let mut errors: Vec<String> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();

        // name (Bug 1: path traversal)
        if let Err(msg) = validate_name(&def.name) {
            errors.push(msg);
        }

        // interval_secs (Bug 2: 0 causes busy-loop) and range 1-86400
        if def.interval_secs < 1 {
            errors.push("interval_secs must be >= 1 (0 causes a busy-loop)".into());
        } else if def.interval_secs > 86400 {
            errors.push("interval_secs must be <= 86400 (1 day max)".into());
        } else if def.interval_secs > 3600 {
            warnings.push(format!(
                "interval_secs={} may be too long, suggested range 300-3600",
                def.interval_secs
            ));
        }

        // script non-empty
        if def.script.trim().is_empty() {
            errors.push("script must not be empty".into());
        }

        // agent non-empty
        if def.agent.trim().is_empty() {
            errors.push("agent must not be empty".into());
        }

        // prompt_template must contain {output} placeholder
        if !def.prompt_template.contains("{output}") {
            errors.push("prompt_template is missing the {output} placeholder".into());
        }

        // mode / trigger_mode / max_concurrent / cooldown sanity
        if def.mode == MonitorMode::Concurrent && def.max_concurrent == 0 {
            errors.push("max_concurrent must be >= 1 when mode=concurrent".into());
        }
        if def.max_concurrent > 100 {
            warnings.push(format!(
                "max_concurrent={} is very high, typical 1-10",
                def.max_concurrent
            ));
        }
        if def.cooldown_secs > def.interval_secs {
            warnings.push(format!(
                "cooldown_secs={} greater than interval_secs={} may suppress triggers",
                def.cooldown_secs, def.interval_secs
            ));
        }

        (errors, warnings)
    }
}

/// Captured output of a script run, used by the `test` dry-run RPC.
#[derive(Clone, Debug)]
struct ScriptRun {
    stdout: String,
    stderr: String,
    exit_ok: bool,
    exit_code: i32,
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
            // v2: capture concurrency + consumer policy for the trigger loop
            let mode = def.mode;
            let trigger_mode = def.trigger_mode;
            let max_concurrent = def.max_concurrent;
            let cooldown_secs = def.cooldown_secs;
            // v2: per-monitor runtime state (queue, active counter, last trigger time)
            let pending_queue = Arc::new(Mutex::new(
                std::collections::VecDeque::<String>::new()
            ));
            let active_count = Arc::new(std::sync::atomic::AtomicU32::new(0));
            // Initialize last_trigger far in the past so the first tick can fire.
            let last_trigger = Arc::new(Mutex::new(
                std::time::Instant::now()
                    .checked_sub(std::time::Duration::from_secs(cooldown_secs.max(1) + 1))
                    .unwrap_or_else(std::time::Instant::now)
            ));

            // Initialize status
            {
                let mut s = stats.lock().await;
                s.insert(name.clone(), MonitorStatus {
                    name: name.clone(),
                    enabled: true,
                    last_run: None,
                    last_result: "starting".into(),
                    trigger_count: 0,
                    // v2 status fields
                    skip_count: 0,
                    queue_length: 0,
                    active_workers: 0,
                    last_error: None,
                    consecutive_failures: 0,
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

                    // Update status (+ track consecutive failures for auto-disable)
                    {
                        let mut s = stats.lock().await;
                        if let Some(status) = s.get_mut(&name) {
                            status.last_run = Some(now.clone());
                            if !success {
                                status.consecutive_failures += 1;
                                status.last_error = Some(if output.is_empty() {
                                    "exit code non-zero".into()
                                } else {
                                    output.clone()
                                });
                                status.last_result = "error".into();
                                // v2: auto-disable after 5 consecutive failures
                                if status.consecutive_failures >= 5 {
                                    status.enabled = false;
                                    status.last_result = "auto_disabled".into();
                                    tracing::warn!(
                                        "[monitor] '{}' auto-disabled after {} consecutive failures",
                                        name, status.consecutive_failures
                                    );
                                }
                            } else {
                                status.consecutive_failures = 0;
                                status.last_result = if output.is_empty() {
                                    "idle".into()
                                } else {
                                    "triggered".into()
                                };
                            }
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

                    // v2: cooldown check (debounce)
                    {
                        let last = *last_trigger.lock().await;
                        if last.elapsed() < std::time::Duration::from_secs(cooldown_secs) {
                            tracing::info!(
                                "[monitor] '{}' cooldown ({}s), skipping trigger",
                                name, cooldown_secs
                            );
                            let mut s = stats.lock().await;
                            if let Some(status) = s.get_mut(&name) {
                                status.last_result = "cooldown".into();
                            }
                            continue;
                        }
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

                    // v2: route by trigger_mode
                    match trigger_mode {
                        TriggerMode::EventOnly => {
                            // Only emit; never spawn a worker.
                            tracing::info!(
                                "[monitor] '{}' event_only: emitted trigger, no spawn",
                                name
                            );
                            {
                                let mut s = stats.lock().await;
                                if let Some(status) = s.get_mut(&name) {
                                    status.last_result = "triggered_event_only".into();
                                }
                            }
                            *last_trigger.lock().await = std::time::Instant::now();
                            continue;
                        }
                        TriggerMode::ChannelNotify => {
                            // Push the rendered prompt to the main channel for an
                            // already-running coordinator/developer to pick up.
                            // If no subscribers exist, degrade to event_only.
                            let has_sub = {
                                let reg_guard = reg.lock().await;
                                reg_guard
                                    .channels
                                    .get("main")
                                    .map(|subs| !subs.is_empty())
                                    .unwrap_or(false)
                            };
                            if has_sub {
                                let mut reg_guard = reg.lock().await;
                                reg_guard
                                    .channel_send(
                                        "main",
                                        &format!("monitor:{name}"),
                                        serde_json::json!({ "text": prompt }),
                                    )
                                    .await;
                                tracing::info!(
                                    "[monitor] '{}' channel_notify: pushed to main",
                                    name
                                );
                            } else {
                                tracing::warn!(
                                    "[monitor] '{}' channel_notify: no_subscriber, fallback to event_only",
                                    name
                                );
                            }
                            {
                                let mut s = stats.lock().await;
                                if let Some(status) = s.get_mut(&name) {
                                    status.last_result = "channel_notified".into();
                                }
                            }
                            *last_trigger.lock().await = std::time::Instant::now();
                            continue;
                        }
                        TriggerMode::AutoSpawn => {
                            // fall through to the mode-based concurrency logic below
                        }
                    }

                    // v2: AutoSpawn concurrency policy
                    match mode {
                        MonitorMode::SerialSkip => {
                            let idle_worker = {
                                let reg_guard = reg.lock().await;
                                reg_guard.workers.iter()
                                    .find(|(_, w)| w.agent == agent
                                        && w.status != crate::worker_registry::WorkerStatus::Dead
                                        && w.status != crate::worker_registry::WorkerStatus::Busy)
                                    .map(|(id, _)| id.clone())
                            };

                            if let Some(wid) = idle_worker {
                                let mut reg_guard = reg.lock().await;
                                let _ = reg_guard.send_command(&wid, "prompt", serde_json::json!({
                                    "text": prompt
                                })).await;
                                tracing::info!("[monitor] sent to existing worker {}", wid);
                            } else {
                                // All busy -> skip this tick + count it.
                                tracing::info!(
                                    "[monitor] '{}' serial_skip: all workers busy, skipping",
                                    name
                                );
                                let mut s = stats.lock().await;
                                if let Some(status) = s.get_mut(&name) {
                                    status.skip_count += 1;
                                    status.last_result = "skipped".into();
                                }
                                *last_trigger.lock().await = std::time::Instant::now();
                                continue;
                            }
                        }
                        MonitorMode::SerialQueue => {
                            // First: if we have an idle worker, replay the oldest queued
                            // prompt (FIFO) and consume the new one afterwards.
                            let idle_worker = {
                                let reg_guard = reg.lock().await;
                                reg_guard.workers.iter()
                                    .find(|(_, w)| w.agent == agent
                                        && w.status != crate::worker_registry::WorkerStatus::Dead
                                        && w.status != crate::worker_registry::WorkerStatus::Busy)
                                    .map(|(id, _)| id.clone())
                            };

                            if let Some(wid) = idle_worker {
                                // Drain one queued item first, then this tick's prompt.
                                let to_send = {
                                    let mut q = pending_queue.lock().await;
                                    q.pop_front().unwrap_or(prompt.clone())
                                };
                                let mut reg_guard = reg.lock().await;
                                let _ = reg_guard.send_command(&wid, "prompt", serde_json::json!({
                                    "text": to_send
                                })).await;
                                tracing::info!("[monitor] dequeued to worker {}", wid);
                            } else {
                                // No idle worker -> enqueue (with overflow protection).
                                let mut q = pending_queue.lock().await;
                                if q.len() >= MONITOR_QUEUE_CAPACITY {
                                    let dropped = q.pop_front();
                                    tracing::warn!(
                                        "[monitor] '{}' queue_overflow (cap={}), dropped oldest: {:?}",
                                        name, MONITOR_QUEUE_CAPACITY, dropped
                                    );
                                    let mut s = stats.lock().await;
                                    if let Some(status) = s.get_mut(&name) {
                                        status.last_result = "queue_overflow".into();
                                    }
                                }
                                q.push_back(prompt.clone());
                                tracing::info!(
                                    "[monitor] '{}' serial_queue: enqueued (len={})",
                                    name, q.len()
                                );
                                let mut s = stats.lock().await;
                                if let Some(status) = s.get_mut(&name) {
                                    status.queue_length = q.len();
                                    status.last_result = "queued".into();
                                }
                                *last_trigger.lock().await = std::time::Instant::now();
                                continue;
                            }
                        }
                        MonitorMode::Concurrent => {
                            let active = active_count.load(
                                std::sync::atomic::Ordering::Relaxed
                            );
                            if active < max_concurrent {
                                let ac = Arc::clone(&active_count);
                                let ac_name = name.clone();
                                active_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                {
                                    let mut s = stats.lock().await;
                                    if let Some(status) = s.get_mut(&name) {
                                        status.active_workers =
                                            active_count.load(std::sync::atomic::Ordering::Relaxed);
                                    }
                                }
                                let prompt_for_spawn = prompt.clone();
                                let agent_for_spawn = agent.clone();
                                let reg_for_spawn = Arc::clone(&reg);
                                tokio::spawn(async move {
                                    let _ac = ActiveGuard::new(ac, ac_name.clone());
                                    let mut reg_guard = reg_for_spawn.lock().await;
                                    match reg_guard.create_worker(
                                        crate::worker_registry::WorkerCreateConfig {
                                            agent: Some(agent_for_spawn.clone()),
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
                                            initial_prompt: Some(prompt_for_spawn),
                                            skip_mcp: None,
                                            allowed_tools: None,
                                            disallowed_tools: None,
                                            max_turns: None,
                                            hook_depth: Some(0),
                                            system_prompt_override: None,
                                        },
                                        &reg_for_spawn,
                                    ).await {
                                        Ok(info) => tracing::info!(
                                            "[monitor] created worker {} for {}",
                                            info.worker_id, ac_name
                                        ),
                                        Err(e) => tracing::error!(
                                            "[monitor] failed to create worker: {e}"
                                        ),
                                    }
                                });
                            } else {
                                tracing::info!(
                                    "[monitor] '{}' concurrent: throttled (active={}/{})",
                                    name, active, max_concurrent
                                );
                                let mut s = stats.lock().await;
                                if let Some(status) = s.get_mut(&name) {
                                    status.skip_count += 1;
                                    status.last_result = "throttled".into();
                                }
                                *last_trigger.lock().await = std::time::Instant::now();
                                continue;
                            }
                        }
                    }

                    *last_trigger.lock().await = std::time::Instant::now();
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
                        // v2 fields
                        "mode": m.mode,
                        "trigger_mode": m.trigger_mode,
                        "max_concurrent": m.max_concurrent,
                        "cooldown_secs": m.cooldown_secs,
                        // runtime counters
                        "trigger_count": status.map(|s| s.trigger_count).unwrap_or(0),
                        "skip_count": status.map(|s| s.skip_count).unwrap_or(0),
                        "queue_length": status.map(|s| s.queue_length).unwrap_or(0),
                        "active_workers": status.map(|s| s.active_workers).unwrap_or(0),
                        "last_run": status.and_then(|s| s.last_run.clone()),
                        "last_result": status.map(|s| s.last_result.as_str()).unwrap_or("unknown"),
                        "last_error": status.and_then(|s| s.last_error.clone()),
                    })
                }).collect();
                Ok(serde_json::json!({"monitors": result}))
            }

            "add" => {
                let def = Self::parse_def(&params);

                // Validate name early so an empty name produces a clear error.
                if def.name.is_empty() {
                    return Err(AgentError::Tool("missing 'name'".into()));
                }

                // v2 Bug 3 fix: reject duplicate names rather than silently overwriting.
                {
                    let monitors = self.monitors.lock().await;
                    if monitors.iter().any(|m| m.name == def.name) {
                        return Err(AgentError::Tool(format!(
                            "monitor '{}' already exists, use 'update' instead",
                            def.name
                        )));
                    }
                }

                // v2: enforce semantic validation before persisting.
                let (errors, _warnings) = Self::validate_def(&def);
                if !errors.is_empty() {
                    return Err(AgentError::Tool(format!(
                        "monitor validation failed: {}",
                        errors.join("; ")
                    )));
                }

                let name = def.name.clone();
                let mut monitors = self.monitors.lock().await;
                monitors.push(def.clone());

                // Save to file
                let monitor_dir = std::path::Path::new(".ion/monitors");
                let _ = std::fs::create_dir_all(monitor_dir);
                let path = monitor_dir.join(format!("{name}.json"));
                if let Ok(json) = serde_json::to_string_pretty(&def) {
                    let _ = std::fs::write(&path, json);
                }

                Ok(serde_json::json!({
                    "added": name,
                    "validated": true,
                    "file": path.display().to_string(),
                    "note": "restart serve to activate new monitors"
                }))
            }

            // v2 — semantic validation only, never persists to disk.
            "validate" => {
                let def = Self::parse_def(&params);
                let (errors, warnings) = Self::validate_def(&def);
                if errors.is_empty() {
                    Ok(serde_json::json!({
                        "valid": true,
                        "warnings": warnings
                    }))
                } else {
                    Ok(serde_json::json!({
                        "valid": false,
                        "errors": errors,
                        "warnings": warnings
                    }))
                }
            }

            // v2 — dry-run the script without touching the scheduler.
            "test" => {
                // Dry-run: only need script + prompt_template, do NOT require name/interval
                // (caller is just checking what the script would output)
                let script = params.get("script").and_then(|v| v.as_str()).unwrap_or("");
                let prompt_template = params.get("prompt_template").and_then(|v| v.as_str())
                    .unwrap_or("Monitor triggered:\n{output}");

                if script.trim().is_empty() {
                    return Ok(serde_json::json!({
                        "valid": false,
                        "errors": ["script must not be empty"],
                        "would_trigger": false
                    }));
                }

                // bash -n syntax check
                let syntax_ok = std::process::Command::new("bash")
                    .arg("-n").arg("-c").arg(script).status()
                    .map(|s| s.success()).unwrap_or(false);
                if !syntax_ok {
                    return Ok(serde_json::json!({
                        "valid": true,
                        "script_exit_ok": false,
                        "script_stderr": "bash -n syntax check failed",
                        "would_trigger": false
                    }));
                }

                let run = Self::run_script_capturing(script);
                let would_trigger = run.exit_ok && !run.stdout.is_empty();
                let rendered_prompt = if would_trigger {
                    Some(Self::format_prompt(prompt_template, &run.stdout))
                } else {
                    None
                };

                Ok(serde_json::json!({
                    "valid": true,
                    "script_exit_ok": run.exit_ok,
                    "script_exit_code": run.exit_code,
                    "script_stdout": run.stdout,
                    "script_stderr": run.stderr,
                    "would_trigger": would_trigger,
                    "rendered_prompt": rendered_prompt
                }))
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

// ── v2: ActiveGuard (RAII decrement for concurrent-mode worker counting) ──
//
// When MonitorMode::Concurrent spawns a worker it increments `active_count`
// before the tokio task starts. This guard ensures the counter is always
// decremented when the spawn task finishes (success or error), so subsequent
// ticks can spawn new workers up to `max_concurrent`.
struct ActiveGuard {
    count: Arc<std::sync::atomic::AtomicU32>,
    name: String,
}

impl ActiveGuard {
    fn new(count: Arc<std::sync::atomic::AtomicU32>, name: String) -> Self {
        Self { count, name }
    }
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.count.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        tracing::info!(
            "[monitor] '{}' active worker done, count now {}",
            self.name,
            self.count.load(std::sync::atomic::Ordering::Relaxed)
        );
    }
}
