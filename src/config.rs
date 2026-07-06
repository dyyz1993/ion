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
    /// Default runtime mode: "local" | "sandbox" | "remote"
    #[serde(default = "default_runtime_mode")]
    pub default_mode: String,
    /// Remote execution hosts
    #[serde(default)]
    pub remote: RemoteConfig,
    /// Sandbox configuration
    #[serde(default)]
    pub sandbox: SandboxConfig,
    /// Command-level routing rules
    #[serde(default)]
    pub routes: Vec<RouteRule>,
}

fn default_runtime_mode() -> String { "local".into() }

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            default_mode: "local".into(),
            remote: RemoteConfig::default(),
            sandbox: SandboxConfig::default(),
            routes: Vec::new(),
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

/// A routing rule: matches tool + pattern → selects runtime
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RouteRule {
    /// Tool name to match (e.g. "bash", "read", "*")
    #[serde(default)]
    pub tool: String,
    /// Command pattern (glob)
    #[serde(default)]
    pub pattern: String,
    /// Target runtime: "local" | "remote" | "sandbox"
    pub runtime: String,
    /// Target host (for remote runtime)
    #[serde(default)]
    pub host: String,
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
        match std::fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_else(|e| {
                eprintln!("Warning: failed to parse {}: {e}", path.display());
                IonConfig::default()
            }),
            Err(_) => IonConfig::default(),
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
