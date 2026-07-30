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
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, OnceCell};

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
/// Concurrency policy for monitor triggers.
///
/// Derives `Clone, Copy, Debug, PartialEq, Eq` (see derive attribute below)
/// so it can be compared, printed via `{:?}`, and used in `match` guards.
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

/// Dispatch / trigger mode for how a monitor's action is delivered.
///
/// Derives `Clone, Copy, Debug, PartialEq, Eq` (see derive attribute below)
/// so it can be compared, printed via `{:?}`, and used in `match` guards.
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

impl std::fmt::Display for MonitorMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MonitorMode::SerialSkip => write!(f, "serial_skip"),
            MonitorMode::SerialQueue => write!(f, "serial_queue"),
            MonitorMode::Concurrent => write!(f, "concurrent"),
        }
    }
}

impl std::fmt::Display for TriggerMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TriggerMode::AutoSpawn => write!(f, "auto_spawn"),
            TriggerMode::ChannelNotify => write!(f, "channel_notify"),
            TriggerMode::EventOnly => write!(f, "event_only"),
        }
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

fn default_interval() -> u64 {
    300
}
fn default_agent() -> String {
    "developer".into()
}
fn default_prompt() -> String {
    "Monitor triggered:\n{output}".into()
}
fn default_enabled() -> bool {
    true
}
fn default_max_concurrent() -> u32 {
    3
}
fn default_cooldown() -> u64 {
    60
}

/// Default queue capacity for serial_queue mode (overflow protection).
pub const MONITOR_QUEUE_CAPACITY: usize = 10;

/// Validate a monitor name against the safe charset ^[a-zA-Z0-9_-]{1,32}$.
/// This guards against path traversal (e.g. "../../etc/cron.d/evil").
fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 32 {
        return Err(format!("name length must be 1-32, got {}", name.len()));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(format!("name may only contain [a-zA-Z0-9_-], got '{name}'"));
    }
    Ok(())
}

/// Public helper: check if a monitor name is valid (^[a-zA-Z0-9_-]{1,32}$).
/// Returns `true` if the name passes validation, `false` otherwise.
///
/// Wraps [`validate_name`] so consumers outside the crate can do a
/// boolean check without dealing with the `Result<(), String>` return.
pub fn is_valid_monitor_name(name: &str) -> bool {
    validate_name(name).is_ok()
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
    /// Track the worker id that THIS monitor spawned (for serial_skip "previous still running" check).
    /// None = no worker spawned yet, or the previous one is gone.
    pub last_spawned_worker: Option<String>,
}

/// A single active pipeline entry (persisted to disk so serve restarts don't
/// re-trigger the same monitor/key and spawn a duplicate worker).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ActivePipeline {
    /// Monitor name that produced this pipeline (e.g. "github-issues").
    pub monitor: String,
    /// Logical key within that monitor (e.g. "issue-42").
    pub key: String,
    /// Worker id handling this pipeline, if known.
    #[serde(default)]
    pub worker_id: Option<String>,
    /// ISO-8601-ish timestamp of when it was marked active.
    #[serde(default)]
    pub started_at: String,
    /// Current pipeline stage: "developer" / "reviewer" / "merger" / "publisher".
    #[serde(default)]
    pub stage: String,
}

impl ActivePipeline {
    /// Returns true if this pipeline has been active for more than 1 hour.
    pub fn is_expired(&self, now_epoch_secs: i64) -> bool {
        let started: i64 = self
            .started_at
            .strip_prefix("epoch:")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        now_epoch_secs - started > 3600
    }
}

impl std::fmt::Display for MonitorStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "{}: triggers={}, skips={}, last={}",
            self.name, self.trigger_count, self.skip_count, self.last_result
        )
    }
}

/// MonitorExtension — singleton, only registered in serve mode.
pub struct MonitorExtension {
    /// Monitor definitions (shared with interval loops).
    monitors: Arc<Mutex<Vec<MonitorDef>>>,
    /// Runtime statuses.
    statuses: Arc<Mutex<HashMap<String, MonitorStatus>>>,
    /// T3: Active pipeline state, persisted across serve restarts.
    active_pipelines: Arc<Mutex<Vec<ActivePipeline>>>,
    /// Registry reference — captured from `on_singleton_post_init` so that
    /// the `add` RPC (which goes through `on_extension_rpc` and has no
    /// registry parameter) can spawn new monitor loops at runtime.
    registry: OnceCell<Arc<tokio::sync::Mutex<crate::worker_registry::WorkerRegistry>>>,
    name: String,
}

impl MonitorExtension {
    pub fn new() -> Self {
        Self {
            monitors: Arc::new(Mutex::new(Vec::new())),
            statuses: Arc::new(Mutex::new(HashMap::new())),
            active_pipelines: Arc::new(Mutex::new(Vec::new())),
            registry: OnceCell::new(),
            name: "monitor".into(),
        }
    }

    /// Returns the number of loaded monitor definitions.
    pub async fn monitor_count(&self) -> usize {
        self.monitors.lock().await.len()
    }

    // ── T3: active pipeline state persistence ──
    //
    // State file location: `$HOME/.ion/agent/active-pipelines.json`.
    // On startup we load it (via on_singleton_init) so a restarted coordinator
    // knows which monitor keys are already being processed and won't spawn a
    // duplicate worker.

    /// Return the on-disk path for the persisted active pipeline state.
    fn active_path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_default();
        PathBuf::from(home).join(".ion/agent/active-pipelines.json")
    }

    /// Load persisted active pipelines from disk. Missing/unreadable/unparseable
    /// files are treated as "no active pipelines" (return empty vec) rather than
    /// an error, so a clean first-run never blocks startup.
    fn load_active() -> Vec<ActivePipeline> {
        let path = Self::active_path();
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<ActivePipelinesFile>(&s).ok())
            .map(|f| f.active)
            .unwrap_or_default()
    }

    /// Persist the current active pipeline list to disk. Creates the parent
    /// directory if needed. Failures are swallowed (logged by tracing) so a
    /// transiently unwritable disk never breaks the in-memory state machine.
    fn save_active(active: &[ActivePipeline]) {
        let path = Self::active_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(&serde_json::json!({ "active": active })) {
            let _ = std::fs::write(&path, json);
        } else {
            tracing::warn!(
                "[monitor] failed to serialize active pipelines for {:?}",
                path
            );
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
                        match serde_json::from_str::<MonitorDef>(&content) {
                            Ok(def) => {
                                tracing::info!(
                                    "[monitor] loaded: {} from {}",
                                    def.name,
                                    path.display()
                                );
                                result.push(def);
                            }
                            Err(e) => {
                                tracing::warn!("[monitor] failed to parse {}: {e}", path.display())
                            }
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

    /// Health monitor event emitter (reuses emit_event but with different extension name).
    async fn emit_health_event(
        registry: &Arc<tokio::sync::Mutex<crate::worker_registry::WorkerRegistry>>,
        custom_type: &str,
        data: serde_json::Value,
    ) {
        tracing::warn!("[health] {}: {}", custom_type, data);
        let bus_opt = {
            let reg = registry.lock().await;
            reg.event_bus.clone()
        };
        if let Some(bus) = bus_opt {
            let mut bus_guard = bus.lock().await;
            let event = crate::event_bus::ExtensionEvent::new("monitor", custom_type)
                .with_data(data)
                .with_visibility(crate::event_bus::EventVisibility::LlmAndUi);
            bus_guard.broadcast(&event);
        }
    }

    /// Broadcast an event to the host EventBus (so `ion subscribe` and all
    /// subscribers receive it). Falls back to tracing log if EventBus is not
    /// configured (e.g. tests).
    async fn emit_event(
        custom_type: &str,
        data: serde_json::Value,
        registry: &Arc<tokio::sync::Mutex<crate::worker_registry::WorkerRegistry>>,
    ) {
        // Always log first (cheap, useful for debugging even without subscribers).
        tracing::info!("[monitor] {}: {}", custom_type, data);

        let bus_opt = {
            let reg = registry.lock().await;
            reg.event_bus.clone()
        };
        if let Some(bus) = bus_opt {
            let mut bus_guard = bus.lock().await;
            // Use LlmAndUi visibility so both subscribe CLI and worker LLMs
            // can observe monitor lifecycle events. This lets coordinator/developer
            // workers react to monitor_triggered / monitor_skipped etc.
            let event = crate::event_bus::ExtensionEvent::new("monitor", custom_type)
                .with_data(data)
                .with_visibility(crate::event_bus::EventVisibility::LlmAndUi);
            bus_guard.broadcast(&event);
        }
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
            name: params
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            interval_secs: params
                .get("interval_secs")
                .and_then(|v| v.as_u64())
                .unwrap_or(default_interval()),
            script: params
                .get("script")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
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
            enabled: params
                .get("enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(default_enabled()),
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
        serde_json::from_value(v.cloned().unwrap_or(serde_json::Value::Null)).unwrap_or_default()
    }

    /// Decode `trigger_mode` from a JSON value using the snake_case rename.
    /// Falls back to the default (AutoSpawn) on missing/invalid input.
    fn parse_trigger_mode(v: Option<&serde_json::Value>) -> TriggerMode {
        serde_json::from_value(v.cloned().unwrap_or(serde_json::Value::Null)).unwrap_or_default()
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
                ScriptRun {
                    stdout,
                    stderr,
                    exit_ok,
                    exit_code,
                }
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
    /// Spawn the interval loop for a monitor definition.
    ///
    /// Called from:
    /// - `on_singleton_post_init` for each monitor loaded at startup
    /// - `add` RPC handler so newly-added monitors activate immediately
    ///   (without requiring a serve restart).
    ///
    /// This initializes the status entry and spawns a tokio task that ticks
    /// every `interval_secs`, runs the script, and routes the output per
    /// `trigger_mode` and `mode`.
    async fn spawn_monitor_for_def(
        def: MonitorDef,
        registry: Arc<tokio::sync::Mutex<crate::worker_registry::WorkerRegistry>>,
        statuses: Arc<Mutex<HashMap<String, MonitorStatus>>>,
    ) {
        let reg = registry;
        let stats = statuses;
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
        let pending_queue = Arc::new(Mutex::new(std::collections::VecDeque::<String>::new()));
        let active_count = Arc::new(std::sync::atomic::AtomicU32::new(0));
        // Initialize last_trigger far in the past so the first tick can fire.
        let last_trigger = Arc::new(Mutex::new(
            std::time::Instant::now()
                .checked_sub(std::time::Duration::from_secs(cooldown_secs.max(1) + 1))
                .unwrap_or_else(std::time::Instant::now),
        ));

        // Initialize status
        {
            let mut s = stats.lock().await;
            s.insert(
                name.clone(),
                MonitorStatus {
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
                    last_spawned_worker: None,
                },
            );
        }

        tracing::info!(
            "[monitor] starting '{}' (interval={}s, agent={})",
            name,
            interval,
            agent
        );

        tokio::spawn(async move {
            let mut ticker =
                tokio::time::interval(tokio::time::Duration::from_secs(interval.max(1)));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

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
                                    name,
                                    status.consecutive_failures
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
                    Self::emit_event(
                        "monitor_script_failed",
                        serde_json::json!({
                            "name": &name, "stderr": &output
                        }),
                        &reg,
                    )
                    .await;
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
                        Self::emit_event(
                            "monitor_cooldown",
                            serde_json::json!({
                                "name": &name, "cooldown_secs": cooldown_secs
                            }),
                            &reg,
                        )
                        .await;
                        let mut s = stats.lock().await;
                        if let Some(status) = s.get_mut(&name) {
                            status.last_result = "cooldown".into();
                        }
                        continue;
                    }
                }

                // Event detected — emit monitor_triggered (the canonical event)
                Self::emit_event(
                    "monitor_triggered",
                    serde_json::json!({
                        "name": &name,
                        "output_bytes": output.len(),
                        "output": &output,
                        "agent": &agent,
                        "mode": serde_json::to_value(&mode).unwrap_or_default(),
                        "trigger_mode": serde_json::to_value(&trigger_mode).unwrap_or_default(),
                    }),
                    &reg,
                )
                .await;

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
                        Self::emit_event(
                            "monitor_event_only",
                            serde_json::json!({
                                "name": &name
                            }),
                            &reg,
                        )
                        .await;
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
                            Self::emit_event(
                                "monitor_channel_notify",
                                serde_json::json!({
                                    "name": &name, "channel": "main"
                                }),
                                &reg,
                            )
                            .await;
                        } else {
                            Self::emit_event(
                                "monitor_no_subscriber",
                                serde_json::json!({
                                    "name": &name, "fallback": "event_only"
                                }),
                                &reg,
                            )
                            .await;
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
                        // Semantic: if THIS monitor's previously-spawned worker is still
                        // alive (running or idle), skip this tick. Otherwise spawn a new one.
                        // This is different from "any worker with matching agent is busy" —
                        // we only care about workers WE spawned, not user's workers.
                        let prev_worker_alive = {
                            let s_guard = stats.lock().await;
                            if let Some(st) = s_guard.get(&name) {
                                if let Some(ref wid) = st.last_spawned_worker {
                                    // Check if this worker id still exists in registry
                                    let reg_guard = reg.lock().await;
                                    reg_guard.workers.contains_key(wid)
                                        && reg_guard
                                            .workers
                                            .get(wid)
                                            .map(|w| {
                                                w.status
                                                    != crate::worker_registry::WorkerStatus::Dead
                                            })
                                            .unwrap_or(false)
                                } else {
                                    false
                                }
                            } else {
                                false
                            }
                        };

                        if prev_worker_alive {
                            // Previous worker still running -> skip this tick.
                            Self::emit_event(
                                "monitor_skipped",
                                serde_json::json!({
                                    "name": &name, "mode": "serial_skip",
                                    "reason": "previous_worker_running"
                                }),
                                &reg,
                            )
                            .await;
                            let mut s = stats.lock().await;
                            if let Some(status) = s.get_mut(&name) {
                                status.skip_count += 1;
                                status.last_result = "skipped".into();
                            }
                            *last_trigger.lock().await = std::time::Instant::now();
                            continue;
                        }

                        // No previous worker (or it's gone) -> spawn a new one.
                        let prompt_for_spawn = prompt.clone();
                        let agent_for_spawn = agent.clone();
                        let reg_for_spawn = Arc::clone(&reg);
                        let stats_for_spawn = Arc::clone(&stats);
                        let monitor_name_for_spawn = name.clone();
                        // Use prepare_worker_spawn (NO lock) + register_prepared_worker (short lock).
                        // This avoids holding the registry lock during worktree creation + child spawn.
                        let reg_for_spawn_clone = Arc::clone(&reg_for_spawn);
                        tokio::spawn(async move {
                            let spawn_config = crate::worker_registry::WorkerCreateConfig {
                                agent: Some(agent_for_spawn.clone()),
                                initial_prompt: Some(prompt_for_spawn.clone()),
                                relation: Some(crate::worker_registry::WorkerRelation::System),
                                hook_depth: Some(0),
                                ..Default::default()
                            };

                            // Phase 1: prepare (NO lock — worktree + spawn child process)
                            let prepared =
                                match crate::worker_registry::WorkerRegistry::prepare_worker_spawn(
                                    &spawn_config,
                                )
                                .await
                                {
                                    Ok(p) => p,
                                    Err(e) => {
                                        tracing::warn!(
                                            "[monitor] prepare_worker_spawn failed for {}: {}",
                                            monitor_name_for_spawn,
                                            e
                                        );
                                        return;
                                    }
                                };

                            // Phase 2: register (SHORT lock — just insert into registry)
                            // CRITICAL: the lock guard must be dropped before calling emit_event,
                            // because emit_event itself acquires the registry lock (to read event_bus).
                            // Holding the guard across emit_event → deadlock.
                            let spawn_result = {
                                let mut reg_guard = match tokio::time::timeout(
                                    std::time::Duration::from_secs(5),
                                    reg_for_spawn_clone.lock(),
                                )
                                .await
                                {
                                    Ok(g) => g,
                                    Err(_) => {
                                        tracing::warn!(
                                            "[monitor] timeout waiting for registry lock (register phase), skipping spawn for {}",
                                            monitor_name_for_spawn
                                        );
                                        return;
                                    }
                                };
                                reg_guard
                                    .register_prepared_worker(
                                        prepared,
                                        &spawn_config,
                                        &reg_for_spawn_clone,
                                    )
                                    .await
                            }; // reg_guard dropped here — lock released
                            match spawn_result {
                                Ok(info) => {
                                    Self::emit_event(
                                        "monitor_spawned",
                                        serde_json::json!({
                                            "name": &monitor_name_for_spawn,
                                            "worker_id": &info.worker_id,
                                            "mode": "serial_skip"
                                        }),
                                        &reg_for_spawn_clone,
                                    )
                                    .await;
                                    // Record the spawned worker id so next tick can check it.
                                    let mut s = stats_for_spawn.lock().await;
                                    if let Some(st) = s.get_mut(&monitor_name_for_spawn) {
                                        st.last_spawned_worker = Some(info.worker_id.clone());
                                    }
                                }
                                Err(e) => tracing::error!(
                                    "[monitor] failed to spawn worker for {}: {e}",
                                    monitor_name_for_spawn
                                ),
                            }
                        });
                    }
                    MonitorMode::SerialQueue => {
                        // Semantic: same as SerialSkip but if previous worker is still busy,
                        // enqueue instead of dropping.
                        let prev_worker_busy = {
                            let s_guard = stats.lock().await;
                            if let Some(st) = s_guard.get(&name) {
                                if let Some(ref wid) = st.last_spawned_worker {
                                    let reg_guard = reg.lock().await;
                                    reg_guard
                                        .workers
                                        .get(wid)
                                        .map(|w| {
                                            w.status == crate::worker_registry::WorkerStatus::Busy
                                        })
                                        .unwrap_or(false)
                                } else {
                                    false
                                }
                            } else {
                                false
                            }
                        };

                        if prev_worker_busy {
                            // Previous worker busy -> enqueue.
                            let mut q = pending_queue.lock().await;
                            if q.len() >= MONITOR_QUEUE_CAPACITY {
                                let dropped = q.pop_front();
                                Self::emit_event(
                                    "monitor_queue_overflow",
                                    serde_json::json!({
                                        "name": &name, "capacity": MONITOR_QUEUE_CAPACITY,
                                        "dropped": dropped
                                    }),
                                    &reg,
                                )
                                .await;
                                let mut s = stats.lock().await;
                                if let Some(status) = s.get_mut(&name) {
                                    status.last_result = "queue_overflow".into();
                                }
                            }
                            q.push_back(prompt.clone());
                            let qlen = q.len();
                            Self::emit_event("monitor_queued", serde_json::json!({
                                    "name": &name, "queue_length": qlen, "capacity": MONITOR_QUEUE_CAPACITY
                                }), &reg).await;
                            let mut s = stats.lock().await;
                            if let Some(status) = s.get_mut(&name) {
                                status.queue_length = qlen;
                                status.last_result = "queued".into();
                            }
                            *last_trigger.lock().await = std::time::Instant::now();
                            continue;
                        }

                        // Previous worker gone or idle -> consume queued first, else use current prompt.
                        let to_send = {
                            let mut q = pending_queue.lock().await;
                            q.pop_front().unwrap_or(prompt.clone())
                        };

                        // Spawn new worker for this prompt.
                        let agent_for_spawn = agent.clone();
                        let reg_for_spawn = Arc::clone(&reg);
                        let reg_for_spawn_clone = Arc::clone(&reg_for_spawn);
                        let stats_for_spawn = Arc::clone(&stats);
                        let monitor_name_for_spawn = name.clone();
                        tokio::spawn(async move {
                            let mut reg_guard = reg_for_spawn.lock().await;
                            match reg_guard
                                .create_worker(
                                    crate::worker_registry::WorkerCreateConfig {
                                        agent: Some(agent_for_spawn.clone()),
                                        initial_prompt: Some(to_send.clone()),
                                        relation: Some(
                                            crate::worker_registry::WorkerRelation::System,
                                        ),
                                        hook_depth: Some(0),
                                        ..Default::default()
                                    },
                                    &reg_for_spawn,
                                )
                                .await
                            {
                                Ok(info) => {
                                    Self::emit_event(
                                        "monitor_spawned",
                                        serde_json::json!({
                                            "name": &monitor_name_for_spawn,
                                            "worker_id": &info.worker_id,
                                            "mode": "serial_queue"
                                        }),
                                        &reg_for_spawn_clone,
                                    )
                                    .await;
                                    let mut s = stats_for_spawn.lock().await;
                                    if let Some(st) = s.get_mut(&monitor_name_for_spawn) {
                                        st.last_spawned_worker = Some(info.worker_id.clone());
                                    }
                                }
                                Err(e) => tracing::error!(
                                    "[monitor] failed to spawn worker for {}: {e}",
                                    monitor_name_for_spawn
                                ),
                            }
                        });
                    }
                    MonitorMode::Concurrent => {
                        let active = active_count.load(std::sync::atomic::Ordering::Relaxed);
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
                            let reg_for_spawn_clone = Arc::clone(&reg_for_spawn);
                            tokio::spawn(async move {
                                let _ac = ActiveGuard::new(ac, ac_name.clone());
                                let mut reg_guard = reg_for_spawn.lock().await;
                                match reg_guard
                                    .create_worker(
                                        crate::worker_registry::WorkerCreateConfig {
                                            agent: Some(agent_for_spawn.clone()),
                                            model: None,
                                            provider: None,
                                            session: None,
                                            project_path: None,
                                            worktree: None,
                                            relation: Some(
                                                crate::worker_registry::WorkerRelation::System,
                                            ),
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
                                    )
                                    .await
                                {
                                    Ok(info) => {
                                        Self::emit_event(
                                            "monitor_spawned",
                                            serde_json::json!({
                                                "name": &ac_name,
                                                "worker_id": &info.worker_id,
                                                "mode": "concurrent"
                                            }),
                                            &reg_for_spawn_clone,
                                        )
                                        .await
                                    }
                                    Err(e) => {
                                        tracing::error!("[monitor] failed to create worker: {e}")
                                    }
                                }
                            });
                        } else {
                            Self::emit_event(
                                "monitor_throttled",
                                serde_json::json!({
                                    "name": &name, "active": active, "max": max_concurrent
                                }),
                                &reg,
                            )
                            .await;
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
}

/// Captured output of a script run, used by the `test` dry-run RPC.
#[derive(Clone, Debug, PartialEq)] // Debug already derived elsewhere; only add PartialEq
struct ScriptRun {
    stdout: String,
    stderr: String,
    exit_ok: bool,
    exit_code: i32,
}

#[async_trait::async_trait]
impl Extension for MonitorExtension {
    fn name(&self) -> &str {
        &self.name
    }

    fn is_singleton(&self) -> bool {
        true
    }
    fn singleton_key(&self) -> &str {
        "monitor"
    }

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

        // T3: load persisted active pipeline state so a restarted serve knows
        // which monitor keys are already being processed.
        let persisted = Self::load_active();
        if !persisted.is_empty() {
            tracing::info!(
                "[monitor] restored {} active pipeline(s) from disk",
                persisted.len()
            );
        }
        let mut active = self.active_pipelines.lock().await;
        *active = persisted;

        Ok(())
    }

    async fn on_singleton_post_init(
        &self,
        registry: &Arc<tokio::sync::Mutex<crate::worker_registry::WorkerRegistry>>,
    ) -> AgentResult<()> {
        // Capture the registry so the `add` RPC (which has no registry param)
        // can spawn new monitor loops at runtime.
        let _ = self.registry.set(Arc::clone(registry));

        let monitors = self.monitors.lock().await.clone();
        let statuses = Arc::clone(&self.statuses);

        // ── Built-in health monitor (meta self-healing) ──
        // Checks serve health every 60s: dead workers, stale count, memory.
        // Emits monitor_serve_unhealthy event when anomalies detected.
        // This is NOT a user-defined monitor — it's always present in serve mode.
        {
            let reg = Arc::clone(registry);
            let _stats = Arc::clone(&statuses); // reserved for future health metrics
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(tokio::time::Duration::from_secs(60));
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    ticker.tick().await;
                    let (dead, stale, busy, idle, total) = {
                        let g = reg.lock().await;
                        let workers: Vec<_> = g.workers.values().collect();
                        let dead = workers
                            .iter()
                            .filter(|w| w.status == crate::worker_registry::WorkerStatus::Dead)
                            .count();
                        let stale = workers
                            .iter()
                            .filter(|w| w.status == crate::worker_registry::WorkerStatus::Stale)
                            .count();
                        let busy = workers
                            .iter()
                            .filter(|w| w.status == crate::worker_registry::WorkerStatus::Busy)
                            .count();
                        let idle = workers
                            .iter()
                            .filter(|w| w.status == crate::worker_registry::WorkerStatus::Idle)
                            .count();
                        (dead, stale, busy, idle, workers.len())
                    };

                    // GC dead workers if > 3
                    if dead > 3 {
                        tracing::warn!(
                            "[health] {} dead workers, triggering gc_dead_workers",
                            dead
                        );
                        let mut g = reg.lock().await;
                        g.gc_dead_workers(300); // remove dead workers older than 5 min
                        Self::emit_health_event(
                            &reg,
                            "monitor_serve_unhealthy",
                            serde_json::json!({
                                "issue": "too_many_dead_workers",
                                "dead_count": dead,
                                "total_workers": total,
                                "action": "gc_triggered"
                            }),
                        )
                        .await;
                    }

                    // Alert if > 5 stale workers (possible zombie accumulation)
                    if stale > 5 {
                        tracing::warn!(
                            "[health] {} stale workers detected (possible zombie accumulation)",
                            stale
                        );
                        Self::emit_health_event(
                            &reg,
                            "monitor_serve_unhealthy",
                            serde_json::json!({
                                "issue": "too_many_stale_workers",
                                "stale_count": stale,
                                "total_workers": total
                            }),
                        )
                        .await;
                    }

                    // Periodic health log (every check, not just anomalies)
                    tracing::info!(
                        "[health] workers: total={} busy={} idle={} stale={} dead={}",
                        total,
                        busy,
                        idle,
                        stale,
                        dead
                    );
                }
            });
            tracing::info!("[monitor] built-in health monitor started (60s interval)");
        }

        for def in monitors.into_iter().filter(|m| m.enabled) {
            Self::spawn_monitor_for_def(def, Arc::clone(registry), Arc::clone(&statuses)).await;
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
                let (errors, _warnings) = MonitorExtension::validate_def(&def);
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

                // Spawn the interval loop immediately (if we have a registry reference).
                // This makes `add` activate the monitor without requiring a serve restart.
                let activated = if let Some(reg) = self.registry.get() {
                    if def.enabled {
                        Self::spawn_monitor_for_def(
                            def.clone(),
                            Arc::clone(reg),
                            Arc::clone(&self.statuses),
                        )
                        .await;
                        true
                    } else {
                        false
                    }
                } else {
                    false
                };

                Ok(serde_json::json!({
                    "added": name,
                    "validated": true,
                    "file": path.display().to_string(),
                    "activated": activated,
                    "note": if activated {
                        "monitor loop started".to_string()
                    } else {
                        "restart serve to activate new monitors".to_string()
                    }
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
                let prompt_template = params
                    .get("prompt_template")
                    .and_then(|v| v.as_str())
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
                    .arg("-n")
                    .arg("-c")
                    .arg(script)
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
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
                let name = params
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                // 1. Remove from monitors Vec
                let removed = {
                    let mut monitors = self.monitors.lock().await;
                    let before = monitors.len();
                    monitors.retain(|m| m.name != name);
                    before > monitors.len()
                };
                // monitors lock dropped here

                // 2. Delete file
                let path = std::path::Path::new(".ion/monitors").join(format!("{name}.json"));
                let _ = std::fs::remove_file(path);

                // 3. Remove from statuses (separate lock scope)
                self.statuses.lock().await.remove(&name);

                // 4. Remove from active_pipelines (separate lock scope)
                self.active_pipelines
                    .lock()
                    .await
                    .retain(|p| p.monitor != name);

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

            // ── T3: active pipeline state RPCs ──

            // mark_active: record that monitor/key is being processed and persist.
            // params: { monitor, key, worker_id?, stage? }
            "mark_active" => {
                let monitor = params
                    .get("monitor")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let key = params
                    .get("key")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if monitor.is_empty() || key.is_empty() {
                    return Err(AgentError::Tool(
                        "mark_active requires non-empty 'monitor' and 'key'".into(),
                    ));
                }
                let worker_id = params
                    .get("worker_id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let stage = params
                    .get("stage")
                    .and_then(|v| v.as_str())
                    .unwrap_or("developer")
                    .to_string();
                let started_at = chrono_or_systime();

                let mut active = self.active_pipelines.lock().await;
                // Update-in-place if this monitor/key is already tracked,
                // otherwise push a new entry. This keeps the list de-duplicated.
                let entry = active
                    .iter_mut()
                    .find(|p| p.monitor == monitor && p.key == key);
                let (was_update, worker_id_out, stage_out, started_out) = match entry {
                    Some(p) => {
                        p.worker_id = worker_id.clone().or_else(|| p.worker_id.clone());
                        p.stage = stage.clone();
                        p.started_at = started_at.clone();
                        (
                            true,
                            p.worker_id.clone(),
                            p.stage.clone(),
                            p.started_at.clone(),
                        )
                    }
                    None => {
                        let pipeline = ActivePipeline {
                            monitor: monitor.clone(),
                            key: key.clone(),
                            worker_id: worker_id.clone(),
                            started_at: started_at.clone(),
                            stage: stage.clone(),
                        };
                        let snapshot = (
                            pipeline.worker_id.clone(),
                            pipeline.stage.clone(),
                            pipeline.started_at.clone(),
                        );
                        active.push(pipeline);
                        (false, snapshot.0, snapshot.1, snapshot.2)
                    }
                };
                Self::save_active(&active);
                drop(active);

                Ok(serde_json::json!({
                    "marked": true,
                    "updated": was_update,
                    "monitor": monitor,
                    "key": key,
                    "worker_id": worker_id_out,
                    "stage": stage_out,
                    "started_at": started_out,
                }))
            }

            // release_active: remove monitor/key from the active list and persist.
            // params: { monitor, key }
            "release_active" => {
                let monitor = params
                    .get("monitor")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let key = params
                    .get("key")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if monitor.is_empty() || key.is_empty() {
                    return Err(AgentError::Tool(
                        "release_active requires non-empty 'monitor' and 'key'".into(),
                    ));
                }
                let mut active = self.active_pipelines.lock().await;
                let before = active.len();
                active.retain(|p| !(p.monitor == monitor && p.key == key));
                let released = before > active.len();
                Self::save_active(&active);
                drop(active);

                Ok(serde_json::json!({
                    "released": released,
                    "monitor": monitor,
                    "key": key,
                }))
            }

            // check_active: returns whether monitor/key is currently active.
            // params: { monitor, key }
            "check_active" => {
                let monitor = params.get("monitor").and_then(|v| v.as_str()).unwrap_or("");
                let key = params.get("key").and_then(|v| v.as_str()).unwrap_or("");
                let active = self.active_pipelines.lock().await;
                let is_active = active.iter().any(|p| p.monitor == monitor && p.key == key);
                Ok(serde_json::json!({
                    "active": is_active,
                    "monitor": monitor,
                    "key": key,
                }))
            }

            // list_active: returns all active pipelines.
            "list_active" => {
                let active = self.active_pipelines.lock().await;
                Ok(serde_json::json!({
                    "active": active.clone(),
                    "count": active.len(),
                }))
            }

            _ => Err(AgentError::Tool(format!("unknown method: {method}"))),
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
        self.count
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        tracing::info!(
            "[monitor] '{}' active worker done, count now {}",
            self.name,
            self.count.load(std::sync::atomic::Ordering::Relaxed)
        );
    }
}

// ── T3: on-disk schema wrapper for the active pipeline state file ──
//
// `active-pipelines.json` is an object with a single "active" array. We
// deserialize through this struct so missing/extra fields are tolerated
// gracefully by serde defaults.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct ActivePipelinesFile {
    #[serde(default)]
    active: Vec<ActivePipeline>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: create a valid MonitorDef for testing
    fn valid_def() -> MonitorDef {
        MonitorDef {
            name: "test-monitor".into(),
            interval_secs: 300,
            script: "echo hello".into(),
            agent: "developer".into(),
            prompt_template: "Output: {output}".into(),
            enabled: true,
            mode: MonitorMode::SerialSkip,
            trigger_mode: TriggerMode::AutoSpawn,
            max_concurrent: 3,
            cooldown_secs: 60,
        }
    }

    // ── validate_name edge cases ──

    #[test]
    fn test_validate_name_valid() {
        assert!(validate_name("valid-name_123").is_ok());
        assert!(validate_name("a").is_ok());
        assert!(validate_name("A_B-C").is_ok());
    }

    #[test]
    fn test_validate_name_empty() {
        assert!(validate_name("").is_err());
    }

    #[test]
    fn test_validate_name_too_long() {
        let long_name = "a".repeat(33);
        assert!(validate_name(&long_name).is_err());
        // Exactly 32 chars should be OK
        let max_name = "a".repeat(32);
        assert!(validate_name(&max_name).is_ok());
    }

    #[test]
    fn test_validate_name_invalid_chars() {
        // Spaces, dots, slashes, special chars
        assert!(validate_name("has space").is_err());
        assert!(validate_name("has.dot").is_err());
        assert!(validate_name("has/slash").is_err());
        assert!(validate_name("has\\backslash").is_err());
        assert!(validate_name("has@at").is_err());
        assert!(validate_name("café").is_err()); // non-ASCII
    }

    #[test]
    fn test_validate_name_path_traversal() {
        // Security: path traversal attempts must be rejected
        assert!(validate_name("../etc/passwd").is_err());
        assert!(validate_name("..").is_err());
        assert!(validate_name("../../cron.d/evil").is_err());
    }

    // ── validate_def edge cases ──
    // Note: validate_def returns (errors, warnings) — errors first!
    // validate_def is inside impl MonitorExtension, so call as MonitorExtension::validate_def

    #[test]
    fn test_validate_def_valid_no_warnings_no_errors() {
        let def = valid_def();
        let (errors, warnings) = MonitorExtension::validate_def(&def);
        assert!(errors.is_empty(), "expected no errors, got: {:?}", errors);
        assert!(
            warnings.is_empty(),
            "expected no warnings, got: {:?}",
            warnings
        );
    }

    #[test]
    fn test_validate_def_empty_name() {
        let mut def = valid_def();
        def.name = "".into();
        let (errors, _) = MonitorExtension::validate_def(&def);
        assert!(!errors.is_empty(), "expected error for empty name");
        assert!(errors.iter().any(|e| e.contains("name")));
    }

    #[test]
    fn test_validate_def_invalid_name_chars() {
        let mut def = valid_def();
        def.name = "bad name!".into();
        let (errors, _) = MonitorExtension::validate_def(&def);
        assert!(!errors.is_empty());
        assert!(errors.iter().any(|e| e.contains("name")));
    }

    #[test]
    fn test_validate_def_zero_interval() {
        let mut def = valid_def();
        def.interval_secs = 0;
        let (errors, _) = MonitorExtension::validate_def(&def);
        assert!(errors.iter().any(|e| e.contains("interval")));
    }

    #[test]
    fn test_validate_def_interval_too_large() {
        let mut def = valid_def();
        def.interval_secs = 100000;
        let (errors, _) = MonitorExtension::validate_def(&def);
        assert!(errors.iter().any(|e| e.contains("interval")));
    }

    #[test]
    fn test_validate_def_long_interval_warning() {
        let mut def = valid_def();
        def.interval_secs = 7200; // > 3600, should warn but not error
        let (errors, warnings) = MonitorExtension::validate_def(&def);
        assert!(errors.is_empty(), "should not error for 7200");
        assert!(warnings.iter().any(|w| w.contains("interval")));
    }

    #[test]
    fn test_validate_def_empty_script() {
        let mut def = valid_def();
        def.script = "".into();
        let (errors, _) = MonitorExtension::validate_def(&def);
        assert!(errors.iter().any(|e| e.contains("script")));
    }

    #[test]
    fn test_validate_def_whitespace_only_script() {
        let mut def = valid_def();
        def.script = "   ".into();
        let (errors, _) = MonitorExtension::validate_def(&def);
        assert!(errors.iter().any(|e| e.contains("script")));
    }

    #[test]
    fn test_validate_def_empty_agent() {
        let mut def = valid_def();
        def.agent = "".into();
        let (errors, _) = MonitorExtension::validate_def(&def);
        assert!(errors.iter().any(|e| e.contains("agent")));
    }

    #[test]
    fn test_validate_def_prompt_missing_placeholder() {
        let mut def = valid_def();
        def.prompt_template = "no placeholder here".into();
        let (errors, _) = MonitorExtension::validate_def(&def);
        assert!(errors.iter().any(|e| e.contains("output")));
    }

    #[test]
    fn test_validate_def_concurrent_zero_max() {
        let mut def = valid_def();
        def.mode = MonitorMode::Concurrent;
        def.max_concurrent = 0;
        let (errors, _) = MonitorExtension::validate_def(&def);
        assert!(errors.iter().any(|e| e.contains("max_concurrent")));
    }

    #[test]
    fn test_validate_def_serial_skip_zero_max_ok() {
        let mut def = valid_def();
        def.mode = MonitorMode::SerialSkip;
        def.max_concurrent = 0;
        let (errors, _) = MonitorExtension::validate_def(&def);
        // max_concurrent=0 only errors with Concurrent mode
        assert!(!errors.iter().any(|e| e.contains("max_concurrent")));
    }

    #[test]
    fn test_validate_def_high_max_concurrent_warning() {
        let mut def = valid_def();
        def.max_concurrent = 200;
        let (_, warnings) = MonitorExtension::validate_def(&def);
        assert!(warnings.iter().any(|w| w.contains("max_concurrent")));
    }

    #[test]
    fn test_validate_def_cooldown_gt_interval_warning() {
        let mut def = valid_def();
        def.interval_secs = 10;
        def.cooldown_secs = 60;
        let (errors, warnings) = MonitorExtension::validate_def(&def);
        assert!(errors.is_empty(), "should not error");
        assert!(warnings.iter().any(|w| w.contains("cooldown")));
    }

    #[tokio::test]
    async fn test_remove_clears_in_memory_state() {
        // Issue #16: remove() must clear statuses and active_pipelines too.
        let ext = MonitorExtension::new();
        let name = "ghost-monitor".to_string();

        // Seed monitors Vec
        {
            let mut monitors = ext.monitors.lock().await;
            let mut def = valid_def();
            def.name = name.clone();
            monitors.push(def);
        }
        // Seed statuses HashMap
        {
            let mut statuses = ext.statuses.lock().await;
            statuses.insert(
                name.clone(),
                MonitorStatus {
                    name: name.clone(),
                    enabled: true,
                    last_run: None,
                    last_result: "ok".into(),
                    trigger_count: 1,
                    skip_count: 0,
                    queue_length: 0,
                    active_workers: 0,
                    last_error: None,
                    consecutive_failures: 0,
                    last_spawned_worker: None,
                },
            );
        }
        // Seed active_pipelines Vec
        {
            let mut active = ext.active_pipelines.lock().await;
            active.push(ActivePipeline {
                monitor: name.clone(),
                key: "issue-1".into(),
                worker_id: None,
                started_at: "2025-01-01T00:00:00Z".into(),
                stage: "developer".into(),
            });
        }

        // Verify seeded
        assert_eq!(ext.monitors.lock().await.len(), 1);
        assert!(ext.statuses.lock().await.contains_key(&name));
        assert_eq!(ext.active_pipelines.lock().await.len(), 1);

        // Simulate the remove handler body (monitors → file → statuses → pipelines)
        let removed = {
            let mut monitors = ext.monitors.lock().await;
            let before = monitors.len();
            monitors.retain(|m| m.name != name);
            before > monitors.len()
        };
        assert!(removed);

        ext.statuses.lock().await.remove(&name);
        ext.active_pipelines
            .lock()
            .await
            .retain(|p| p.monitor != name);

        // All three should be empty now
        assert!(ext.monitors.lock().await.is_empty());
        assert!(!ext.statuses.lock().await.contains_key(&name));
        assert!(ext.active_pipelines.lock().await.is_empty());
    }

    // ── Issue #20: monitor_count() convenience method ──

    #[tokio::test]
    async fn test_monitor_count() {
        let ext = MonitorExtension::new();
        assert_eq!(ext.monitor_count().await, 0);
        ext.monitors.lock().await.push(valid_def());
        assert_eq!(ext.monitor_count().await, 1);
    }

    // ── Issue #21: Display impl for MonitorStatus ──

    #[test]
    fn test_display_monitor_status() {
        let s = MonitorStatus {
            name: "test-mon".into(),
            enabled: true,
            last_run: None,
            last_result: "triggered".into(),
            trigger_count: 5,
            skip_count: 2,
            queue_length: 0,
            active_workers: 1,
            last_error: None,
            consecutive_failures: 0,
            last_spawned_worker: None,
        };
        let display = format!("{}", s);
        assert!(display.contains("test-mon"));
        assert!(display.contains("triggers=5"));
        assert!(display.contains("skips=2"));
    }

    // ── Issue #22: ScriptRun Debug + PartialEq ──

    #[test]
    fn test_script_run_equality() {
        let a = ScriptRun {
            stdout: "hello".into(),
            stderr: "".into(),
            exit_ok: true,
            exit_code: 0,
        };
        let b = ScriptRun {
            stdout: "hello".into(),
            stderr: "".into(),
            exit_ok: true,
            exit_code: 0,
        };
        let c = ScriptRun {
            stdout: "world".into(),
            stderr: "".into(),
            exit_ok: true,
            exit_code: 0,
        };
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    // ── Issue #23: ActivePipeline::is_expired() ──

    #[test]
    fn test_active_pipeline_expired() {
        let old = ActivePipeline {
            monitor: "test".into(),
            key: "issue-1".into(),
            worker_id: None,
            started_at: "epoch:1".into(), // very old
            stage: "developer".into(),
        };
        let fresh = ActivePipeline {
            monitor: "test".into(),
            key: "issue-2".into(),
            worker_id: None,
            started_at: "epoch:99999999999".into(), // far future
            stage: "developer".into(),
        };
        let now: i64 = 1785060000; // fixed timestamp
        assert!(old.is_expired(now), "old pipeline should be expired");
        assert!(
            !fresh.is_expired(now),
            "fresh pipeline should not be expired"
        );
    }

    // ── Issue #24: validate_def serial_skip with max_concurrent=0 ──

    #[test]
    fn test_validate_def_serial_skip_zero_max_concurrent() {
        let ext = MonitorExtension::new();
        let def = MonitorDef {
            name: "test".into(),
            interval_secs: 60,
            script: "echo hi".into(),
            agent: "build".into(),
            prompt_template: "Got: {output}".into(),
            enabled: true,
            mode: MonitorMode::SerialSkip,
            trigger_mode: TriggerMode::AutoSpawn,
            max_concurrent: 0,
            cooldown_secs: 60,
        };
        let (errors, _warnings) = MonitorExtension::validate_def(&def);
        assert!(
            errors.iter().all(|e| !e.contains("max_concurrent")),
            "unexpected max_concurrent error: {:?}",
            errors
        );
    }
}
