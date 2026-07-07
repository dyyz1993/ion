use std::collections::HashMap;
use std::path::PathBuf;

/// ION configuration stored in ~/.ion/config.json
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct IonConfig {
    /// Default provider name (e.g. "opencode", "anthropic")
    #[serde(default)]
    pub default_provider: Option<String>,

    /// Default model ID (e.g. "deepseek-v4-flash")
    #[serde(default)]
    pub default_model: Option<String>,

    /// Default API key (stored in plaintext — in production use keyring)
    #[serde(default)]
    pub api_key: Option<String>,

    /// Default base URL override
    #[serde(default)]
    pub base_url: Option<String>,

    /// Per-provider API keys (provider_name → key)
    #[serde(default)]
    pub provider_api_keys: HashMap<String, String>,

    /// Custom provider definitions (matching ~/.pi/agent/models.json format)
    #[serde(default)]
    pub providers: HashMap<String, CustomProvider>,

    /// Built-in extension control — disable specific built-in extensions.
    ///
    /// Example config.json:
    /// ```json
    /// {
    ///   "extensions": {
    ///     "memory": { "enabled": false },
    ///     "bash":   { "enabled": true }
    ///   }
    /// }
    /// ```
    /// Omitted extensions default to enabled.
    #[serde(default)]
    pub extensions: HashMap<String, ExtensionConfig>,

    /// Runtime configuration (remote hosts, sandbox, routes)
    #[serde(default)]
    pub runtime: RuntimeConfig,
}

/// Runtime configuration
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RuntimeConfig {
    /// Default runtime mode (legacy): "local" | "sandbox" | "remote"
    #[serde(default = "default_runtime_mode")]
    pub default_mode: String,
    /// Default backend name (new style). When set, overrides `default_mode`.
    /// Must reference a backend in `backends`. Falls back to "local" if missing.
    #[serde(default)]
    pub default: String,
    /// Backend definitions (new style): name → spec
    #[serde(default)]
    pub backends: HashMap<String, BackendConfig>,
    /// Remote execution hosts (legacy, kept for backward compat)
    #[serde(default)]
    pub remote: RemoteConfig,
    /// Sandbox configuration (legacy)
    #[serde(default)]
    pub sandbox: SandboxConfig,
    /// Routing rules: command/path prefixes → backend name
    #[serde(default)]
    pub routes: Vec<RouteRule>,
    /// CommandGuard configuration
    #[serde(default)]
    pub command_guard: CommandGuardConfig,
}

fn default_runtime_mode() -> String { "local".into() }

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            default_mode: "local".into(),
            default: String::new(),
            backends: HashMap::new(),
            remote: RemoteConfig::default(),
            sandbox: SandboxConfig::default(),
            routes: Vec::new(),
            command_guard: CommandGuardConfig::default(),
        }
    }
}

/// CommandGuard config — 控制命令安全检查的行为。
///
/// 示例配置：
/// ```json
/// {
///   "runtime": {
///     "command_guard": {
///       "mode": "whitelist",
///       "whitelist": ["npm", "cargo", "git", "node", "python3"],
///       "risk_patterns": [
///         {"pattern": "rm -rf /", "message": "删除根", "level": "high"}
///       ]
///     }
///   }
/// }
/// ```
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CommandGuardConfig {
    /// Guard mode: "whitelist" (默认) | "blacklist" | "open"
    #[serde(default = "default_guard_mode")]
    pub mode: String,
    /// Command whitelist (prefix match)
    #[serde(default)]
    pub whitelist: Vec<String>,
    /// Custom risk patterns (merged with defaults)
    #[serde(default)]
    pub risk_patterns: Vec<GuardPatternConfig>,
}

fn default_guard_mode() -> String { "whitelist".into() }

impl Default for CommandGuardConfig {
    fn default() -> Self {
        Self {
            mode: "whitelist".into(),
            whitelist: Vec::new(),
            risk_patterns: Vec::new(),
        }
    }
}

/// A single risk pattern (JSON config)
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct GuardPatternConfig {
    pub pattern: String,
    pub message: String,
    pub level: String,
    #[serde(default)]
    pub suggestion: Option<String>,
}

impl RuntimeConfig {
    /// Resolve the effective default backend name.
    /// Priority: `default` field > `default_mode` field > "local".
    pub fn effective_default(&self) -> &str {
        if !self.default.is_empty() {
            // Verify backend exists; if not, fall back
            if self.backends.contains_key(&self.default) {
                return &self.default;
            }
            // default references non-existent backend — caller should warn
        }
        // Map legacy default_mode to backend-style naming
        match self.default_mode.as_str() {
            "remote" => {
                // Use default_host from remote config if available
                if !self.remote.default_host.is_empty() {
                    // Legacy mode — caller handles via compat shim
                }
                "remote_default" // sentinel — caller maps to remote host
            }
            "sandbox" => "sandbox_default",
            _ => "local",
        }
    }

    /// Returns true if using new-style backends configuration
    pub fn uses_backends(&self) -> bool {
        !self.backends.is_empty() || !self.default.is_empty()
    }
}

/// A backend definition. Each backend is a named, configurable Runtime instance.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct BackendConfig {
    /// Backend type: "local" | "remote" | "sandbox" | "container"
    #[serde(rename = "type")]
    pub backend_type: String,
    /// Container driver (only for type="container"): "apple" | "docker" | "podman"
    #[serde(default)]
    pub driver: String,
    // ── remote fields ──
    #[serde(default)]
    pub hostname: String,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub proxy_jump: String,
    // ── sandbox fields ──
    #[serde(default)]
    pub profile: String,
    // ── container fields ──
    /// OCI image (e.g. "docker.io/library/node:22-alpine")
    #[serde(default)]
    pub image: String,
    /// Container exposed port
    #[serde(default)]
    pub container_port: Option<u16>,
    /// Memory limit (e.g. "2G")
    #[serde(default)]
    pub memory: String,
    /// CPU limit (number of CPUs)
    #[serde(default)]
    pub cpus: Option<u32>,
    /// Volume to mount (Apple Container named volume)
    #[serde(default)]
    pub volume: String,
    /// Worktree mount path inside container (default: /workspace)
    #[serde(default = "default_container_workspace")]
    pub mount_path: String,
    /// Host-side worktree path (optional, mounts to mount_path inside container)
    #[serde(default)]
    pub workspace: String,
}

fn default_container_workspace() -> String { "/workspace".into() }

impl Default for BackendConfig {
    fn default() -> Self {
        Self {
            backend_type: "local".into(),
            driver: String::new(),
            hostname: String::new(),
            user: String::new(),
            port: None,
            key: String::new(),
            proxy_jump: String::new(),
            profile: "workspace".into(),
            image: String::new(),
            container_port: None,
            memory: String::new(),
            cpus: None,
            volume: String::new(),
            mount_path: "/workspace".into(),
            workspace: String::new(),
        }
    }
}

/// Remote execution configuration
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RemoteConfig {
    /// Default host name
    #[serde(default)]
    pub default_host: String,
    /// Host definitions
    #[serde(default)]
    pub hosts: HashMap<String, RemoteHost>,
}

impl Default for RemoteConfig {
    fn default() -> Self { Self { default_host: String::new(), hosts: HashMap::new() } }
}

/// A single remote host definition
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RemoteHost {
    /// SSH user
    #[serde(default)]
    pub user: String,
    /// Hostname or IP
    #[serde(default)]
    pub hostname: String,
    /// SSH port
    #[serde(default = "default_ssh_port")]
    pub port: u16,
    /// SSH key path (optional, uses default if empty)
    #[serde(default)]
    pub key: String,
    /// Transport protocol: "ssh" | "http" | "grpc"
    #[serde(default = "default_transport")]
    pub transport: String,
    /// SSH proxy jump host (e.g. "shanbox")
    #[serde(default)]
    pub proxy_jump: String,
}

fn default_ssh_port() -> u16 { 22 }
fn default_transport() -> String { "ssh".into() }

impl Default for RemoteHost {
    fn default() -> Self {
        Self {
            user: String::new(),
            hostname: String::new(),
            port: 22,
            key: String::new(),
            transport: "ssh".into(),
            proxy_jump: String::new(),
        }
    }
}

/// Sandbox configuration
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SandboxConfig {
    /// Sandbox profile: "readonly" | "workspace" | "full-access"
    #[serde(default)]
    pub profile: String,
    /// Allow agent to request execution outside sandbox
    #[serde(default)]
    pub allow_escape_with_approval: bool,
    /// Escape approval mode: "ask" | "auto_approve" | "deny"
    #[serde(default = "default_escape_mode")]
    pub escape_approval_mode: String,
}

fn default_escape_mode() -> String { "ask".into() }

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            profile: "workspace".into(),
            allow_escape_with_approval: true,
            escape_approval_mode: "ask".into(),
        }
    }
}

/// A routing rule: matches command or path prefix → selects a named backend.
///
/// Either `command` or `path` (or both) must be non-empty.
/// `target` references a backend name from `backends` map.
///
/// ```json
/// {"command": "npm *",     "target": "local"}
/// {"path":    "/home/*",   "target": "sh-sandbox"}
/// {"tool":    "bash",      "pattern": "kubectl *", "target": "cluster"}  // legacy form
/// ```
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct RouteRule {
    /// Command prefix pattern (e.g. "npm *", "cargo *"). Matched against execute_command input.
    /// Only the command's leading token(s) + glob are matched, not full shell semantics.
    #[serde(default)]
    pub command: String,
    /// Path prefix pattern (e.g. "/Users/xuyingzhou/.ion/*"). Matched against read/write/edit/... paths.
    /// Path is canonicalized before matching.
    #[serde(default)]
    pub path: String,

    /// Target backend name (new style) — must exist in `backends`.
    #[serde(default)]
    pub target: String,

    // ── Legacy fields (kept for backward compatibility with old configs) ──
    /// Tool name to match (legacy)
    #[serde(default)]
    pub tool: String,
    /// Pattern (legacy, acts like command + path combined)
    #[serde(default)]
    pub pattern: String,
    /// Target runtime (legacy): "local" | "remote" | "sandbox"
    #[serde(default)]
    pub runtime: String,
    /// Target host (legacy, for remote runtime)
    #[serde(default)]
    pub host: String,
}

impl RouteRule {
    /// Returns true if this rule has any matching criteria (otherwise it's a no-op).
    pub fn has_matcher(&self) -> bool {
        !self.command.is_empty() || !self.path.is_empty() || !self.pattern.is_empty()
    }

    /// Returns the effective target name (new `target` field preferred over legacy `runtime`/`host`).
    pub fn effective_target(&self) -> String {
        if !self.target.is_empty() {
            return self.target.clone();
        }
        // Legacy form: build a target name from runtime + host
        match self.runtime.as_str() {
            "remote" if !self.host.is_empty() => self.host.clone(),
            "remote" => "remote_default".into(),
            "sandbox" => "sandbox_default".into(),
            "local" | "" => "local".into(),
            other => other.into(),
        }
    }
}

/// Per-extension configuration (currently just enable/disable).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ExtensionConfig {
    /// Whether this extension is enabled. Defaults to true.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}

impl IonConfig {
    /// Check if a built-in extension is enabled (defaults to true if not configured).
    pub fn is_extension_enabled(&self, name: &str) -> bool {
        self.extensions
            .get(name)
            .map(|c| c.enabled)
            .unwrap_or(true)
    }
}

/// A custom provider definition (matches the pi reference models.json schema)
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CustomProvider {
    pub name: String,
    pub api: String,
    pub base_url: String,
    pub api_key: Option<String>,
    pub headers: Option<HashMap<String, String>>,
    pub models: Vec<CustomModel>,
    pub model_overrides: Option<HashMap<String, ModelOverride>>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CustomModel {
    pub id: String,
    pub name: Option<String>,
    pub reasoning: Option<bool>,
    pub context_window: Option<u64>,
    pub max_tokens: Option<u64>,
    pub cost: Option<CostConfig>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CostConfig {
    pub input: f64,
    pub output: f64,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ModelOverride {
    pub base_url: Option<String>,
    pub api_key: Option<String>,
}

impl Default for IonConfig {
    fn default() -> Self {
        Self {
            default_provider: Some("opencode".into()),
            default_model: None,
            api_key: None,
            base_url: None,
            provider_api_keys: HashMap::new(),
            providers: HashMap::new(),
            extensions: HashMap::new(),
            runtime: RuntimeConfig::default(),
        }
    }
}

impl IonConfig {
    /// Path to config file: ~/.ion/config.json
    pub fn path() -> PathBuf {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_else(|_| ".".into());
        PathBuf::from(home).join(".ion").join("config.json")
    }

    /// Load config from file, or return defaults if not found.
    pub fn load() -> Self {
        let path = Self::path();
        if !path.exists() {
            return IonConfig::default();
        }
        let mut cfg = match std::fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_else(|e| {
                eprintln!("Warning: failed to parse {}: {e}", path.display());
                IonConfig::default()
            }),
            Err(_) => IonConfig::default(),
        };
        // Merge project-level config from <cwd>/.ion/config.json (deep merge on runtime)
        if let Some(project_cfg) = Self::load_project() {
            cfg.merge_project(project_cfg);
        }
        cfg
    }

    /// Load project-level config from `<cwd>/.ion/config.json`.
    /// Returns None if not present.
    fn load_project() -> Option<IonConfig> {
        let cwd = std::env::current_dir().ok()?;
        let proj_path = cwd.join(".ion").join("config.json");
        if !proj_path.exists() { return None; }
        let content = std::fs::read_to_string(&proj_path).ok()?;
        serde_json::from_str(&content).ok()
    }

    /// Merge project-level config into self. Project overrides global for set fields.
    /// Currently only `runtime` is deep-merged; other fields take project value if set.
    fn merge_project(&mut self, project: IonConfig) {
        // Runtime: project's default_mode overrides global if it's different from "local" default
        if !project.runtime.default_mode.is_empty()
            && project.runtime.default_mode != self.runtime.default_mode {
            self.runtime.default_mode = project.runtime.default_mode;
        }
        // Other top-level fields: project takes precedence if Some
        if project.default_provider.is_some() {
            self.default_provider = project.default_provider;
        }
        if project.default_model.is_some() {
            self.default_model = project.default_model;
        }
        if project.base_url.is_some() {
            self.base_url = project.base_url;
        }
    }

    /// Save config to file.
    pub fn save(&self) -> Result<(), Box<dyn std::error::Error>> {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, content)?;
        Ok(())
    }

    /// Resolve API key: CLI arg > auth.json > config file > env var.
    pub fn resolve_api_key(&self, cli_key: Option<&str>, provider: &str) -> Option<String> {
        // Delegate to AuthStorage which has the full priority chain
        crate::auth::AuthStorage::resolve_api_key(cli_key, provider)
    }
}

// ---------------------------------------------------------------------------
// defaultModelPerProvider (from pi reference model-resolver.ts)
// ---------------------------------------------------------------------------

/// Returns the default model ID for a given provider name.
pub fn default_model_for_provider(provider: &str) -> &'static str {
    match provider {
        "anthropic" => "claude-opus-4-8",
        "openai" => "gpt-5.4",
        "deepseek" => "deepseek-v4-pro",
        "google" => "gemini-3.1-pro-preview",
        "opencode" => "deepseek-v4-flash",
        "openrouter" => "deepseek-v4-flash",
        "xai" => "grok-3",
        "groq" => "deepseek-v4-flash",
        "mistral" => "mistral-large",
        "github-copilot" => "gpt-5.4",
        "amazon-bedrock" => "amazon.nova-pro-v1:0",
        "azure-openai-responses" => "gpt-5.4",
        "google-vertex" => "gemini-3.1-pro-preview",
        "openai-codex" => "gpt-5.5-codex",
        "zai-coding-cn" => "deepseek-v4-flash",
        "xiaomi" => "mimo-z1",
        "fireworks" => "deepseek-v4-flash",
        "together" => "deepseek-v4-flash",
        "cerebras" => "cerebras-gpt",
        _ => "deepseek-v4-flash",
    }
}
