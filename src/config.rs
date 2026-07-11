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
    ///     "memory":       { "enabled": false },
    ///     "bash":         { "enabled": true },
    ///     "workflow_gate": { "enabled": false }
    ///   }
    /// }
    /// ```
    /// Omitted extensions default to enabled.
    /// Available: memory, bash, streaming, permission, workflow_gate
    #[serde(default)]
    pub extensions: HashMap<String, ExtensionConfig>,

    /// Model tier aliases (fast/pro/max → provider/model-id)
    /// 用户可以用 --model fast 代替 --model deepseek/deepseek-v4-flash
    #[serde(default = "default_tier_models")]
    pub tier_models: HashMap<String, String>,

    /// MCP server 配置（详见 docs/design/MCP_SYSTEM.md）
    /// 放 ② 项目维度（含本地路径/密钥，不依赖 git 同步）
    #[serde(default)]
    pub mcp_servers: HashMap<String, McpServerConfig>,

    /// Runtime configuration (remote hosts, sandbox, routes)
    #[serde(default)]
    pub runtime: RuntimeConfig,
}

/// 默认 tier aliases（对齐 pi DEFAULT_TIER_ALIASES）
fn default_tier_models() -> HashMap<String, String> {
    let mut m = HashMap::new();
    m.insert("fast".into(), "deepseek/deepseek-v4-flash".into());
    m.insert("pro".into(), "deepseek/deepseek-v4-pro".into());
    m.insert("max".into(), "zai/glm-4.6".into());
    m
}

/// 单个 MCP server 的配置。
/// 两种传输方式用 untagged enum 区分（兼容 pi 配置格式）：
/// - 有 `command` 字段 → stdio（spawn 子进程）
/// - 有 `type: "streamable-http"` → HTTP 远程 server
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum McpServerConfig {
    /// stdio 传输：spawn 子进程（最常用）
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: HashMap<String, String>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        disabled: bool,
    },
    /// Streamable HTTP 传输：远程 server
    Http {
        /// 必须是 "streamable-http"
        #[serde(rename = "type")]
        kind: String,
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
        #[serde(default)]
        disabled: bool,
    },
}

impl McpServerConfig {
    /// 是否被禁用
    pub fn is_disabled(&self) -> bool {
        match self {
            McpServerConfig::Stdio { disabled, .. } => *disabled,
            McpServerConfig::Http { disabled, .. } => *disabled,
        }
    }

    /// 传输方式（"stdio" / "streamable-http"），用于 get_mcp_servers 展示
    pub fn transport(&self) -> &'static str {
        match self {
            McpServerConfig::Stdio { .. } => "stdio",
            McpServerConfig::Http { .. } => "streamable-http",
        }
    }
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
    /// 默认关闭的扩展（不在 config 里声明时，这些扩展默认不启用）
    const DEFAULT_DISABLED: &'static [&'static str] = &["file-snapshot"];

    pub fn is_extension_enabled(&self, name: &str) -> bool {
        self.extensions
            .get(name)
            .map(|c| c.enabled)
            .unwrap_or(!Self::DEFAULT_DISABLED.contains(&name))
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
            tier_models: default_tier_models(),
            mcp_servers: HashMap::new(),
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

        // runtime.default_mode 是项目级设置，不从全局继承。
        // 默认 local，只有项目级 .ion/config.json 显式设置或 --remote flag 才能改。
        // 这里先重置为 local，后面让项目级 config 或 flag 来覆盖。
        cfg.runtime.default_mode = "local".into();

        // 项目级 config（cwd 或 ION_PROJECT_ROOT）可以设置 runtime mode
        if let Some(project_cfg) = Self::load_project() {
            // runtime.default_mode 只在项目级显式设置（非空且非默认 local）时覆盖
            if !project_cfg.runtime.default_mode.is_empty()
                && project_cfg.runtime.default_mode != "local" {
                cfg.runtime.default_mode = project_cfg.runtime.default_mode.clone();
            }
            // 其他字段（provider/model 等）走通用 merge
            cfg.merge_project(project_cfg);
        }

        // ② 项目维度配置（~/.ion/projects/<key>/config.json）—— 优先级高于 ③ 仓库内
        // 含本地路径/密钥的配置（MCP server 等），worktree 共享，不依赖 git 同步
        if let Some(dim_cfg) = Self::load_project_dimension() {
            // runtime.default_mode 同样处理
            if !dim_cfg.runtime.default_mode.is_empty()
                && dim_cfg.runtime.default_mode != "local" {
                cfg.runtime.default_mode = dim_cfg.runtime.default_mode.clone();
            }
            cfg.merge_project(dim_cfg);
        }

        // CLI override via env var (set by main() when --local/--remote is passed)
        // 最高优先级
        match std::env::var("ION_RUNTIME_OVERRIDE").as_deref() {
            Ok("local") => cfg.runtime.default_mode = "local".into(),
            Ok("remote") => cfg.runtime.default_mode = "remote".into(),
            _ => {}
        }
        cfg
    }

    /// Load project-level config from `<cwd>/.ion/config.json`.
    /// If `ION_PROJECT_ROOT` env var is set (worker in worktree), use that instead of cwd.
    /// Returns None if not present.
    fn load_project() -> Option<IonConfig> {
        // 优先用 ION_PROJECT_ROOT（worktree 场景下 cwd 是 worktree 目录，没有 .ion/）
        let base_dir = std::env::var("ION_PROJECT_ROOT")
            .map(std::path::PathBuf::from)
            .or_else(|_| std::env::current_dir())
            .ok()?;
        let proj_path = base_dir.join(".ion").join("config.json");
        if !proj_path.exists() { return None; }
        let content = std::fs::read_to_string(&proj_path).ok()?;
        serde_json::from_str(&content).ok()
    }

    /// 加载 ② 项目维度配置（`~/.ion/projects/<project_key>/config.json`）。
    /// project_key 用 git common dir hash，主仓库和 worktree 算出同一个 key → 天然共享。
    /// 存放含本地路径/密钥的配置（MCP server 等），不依赖 git 同步。
    fn load_project_dimension() -> Option<IonConfig> {
        // 用 config_root（ION_PROJECT_ROOT 优先）算 project_key
        let config_root = crate::paths::project_root_for_config()
            .to_string_lossy().to_string();
        let pkey = crate::paths::project_key_git(&config_root);
        let dim_path = crate::paths::project_dimension_config_path(&pkey);
        if !dim_path.exists() { return None; }
        let content = std::fs::read_to_string(&dim_path).ok()?;
        serde_json::from_str(&content).ok()
    }

    /// Merge project-level config into self.
    ///
    /// 合并策略（修复缺口 #1）：
    /// - `Option<T>` 字段：项目级 `Some` 时覆盖全局
    /// - `HashMap` 字段：按 key 深度合并（项目级 key 覆盖同名，全局的其他 key 保留）
    /// - `Vec` 字段：项目级非空时整体替换（避免半合并歧义）
    /// - `runtime`：default_mode 不在此处理（load 里单独处理，不从全局继承），
    ///   其余子字段（backends/routes/command_guard/default/remote/sandbox）走合并
    /// - `api_key`：走 auth.json 链，不在 config 合并里污染
    fn merge_project(&mut self, mut project: IonConfig) {
        // Option 字段：项目级 Some 时覆盖
        if project.default_provider.is_some() {
            self.default_provider = project.default_provider;
        }
        if project.default_model.is_some() {
            self.default_model = project.default_model;
        }
        if project.base_url.is_some() {
            self.base_url = project.base_url;
        }
        // api_key 不在 config 合并（走 auth.json）；但项目级显式写了也尊重
        if project.api_key.is_some() {
            self.api_key = project.api_key;
        }

        // tier_models：serde 默认值陷阱修复
        // 项目级 config 文件不写 tier_models 时，serde 会填默认值（fast/pro/max）。
        // 如果项目级的 tier_models 和默认值完全相同，说明用户没自定义，跳过合并。
        let default_tm = default_tier_models();
        let project_tm = std::mem::take(&mut project.tier_models);
        let project_tm_is_default = project_tm.len() == default_tm.len()
            && project_tm.iter().all(|(k, v)| default_tm.get(k) == Some(v));
        if !project_tm_is_default {
            for (k, v) in project_tm {
                self.tier_models.insert(k, v);
            }
        }

        // HashMap 字段：按 key 合并（项目级覆盖同名 key，全局的其他 key 保留）
        for (k, v) in project.provider_api_keys {
            self.provider_api_keys.insert(k, v);
        }
        for (k, v) in project.providers {
            self.providers.insert(k, v);
        }
        for (k, v) in project.extensions {
            self.extensions.insert(k, v);
        }
        for (k, v) in project.tier_models {
            self.tier_models.insert(k, v);
        }
        // mcp_servers：按 server name 合并（项目维度覆盖全局同名）
        for (k, v) in project.mcp_servers {
            self.mcp_servers.insert(k, v);
        }

        // runtime 字段：default_mode 已在 load() 单独处理（不从全局继承），其余合并
        // default（新式默认 backend 名）：项目级非空时覆盖
        if !project.runtime.default.is_empty() {
            self.runtime.default = project.runtime.default;
        }
        // backends：按 name 合并
        for (k, v) in project.runtime.backends {
            self.runtime.backends.insert(k, v);
        }
        // routes：项目级非空时整体替换（避免两条前缀相同的规则冲突）
        if !project.runtime.routes.is_empty() {
            self.runtime.routes = project.runtime.routes;
        }
        // command_guard：mode/whitelist/risk_patterns 分别处理
        // （mode 项目级非默认值时覆盖；whitelist/risk_patterns 非空时替换）
        if project.runtime.command_guard.mode != default_guard_mode() {
            self.runtime.command_guard.mode = project.runtime.command_guard.mode;
        }
        if !project.runtime.command_guard.whitelist.is_empty() {
            self.runtime.command_guard.whitelist = project.runtime.command_guard.whitelist;
        }
        if !project.runtime.command_guard.risk_patterns.is_empty() {
            self.runtime.command_guard.risk_patterns = project.runtime.command_guard.risk_patterns;
        }
        // remote/sandbox：项目级显式配置时覆盖（向后兼容旧式配置）
        // remote.hosts 按 name 合并；default_host 非空时覆盖
        for (k, v) in project.runtime.remote.hosts {
            self.runtime.remote.hosts.insert(k, v);
        }
        if !project.runtime.remote.default_host.is_empty() {
            self.runtime.remote.default_host = project.runtime.remote.default_host;
        }
        // sandbox：profile 非默认时覆盖
        if project.runtime.sandbox.profile != SandboxConfig::default().profile {
            self.runtime.sandbox.profile = project.runtime.sandbox.profile;
        }
        self.runtime.sandbox.allow_escape_with_approval = project.runtime.sandbox.allow_escape_with_approval;
        if project.runtime.sandbox.escape_approval_mode != SandboxConfig::default().escape_approval_mode {
            self.runtime.sandbox.escape_approval_mode = project.runtime.sandbox.escape_approval_mode;
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

#[cfg(test)]
mod merge_tests {
    use super::*;

    /// 构造一个带各种字段的全局 config 作为合并基准
    fn global_config() -> IonConfig {
        let mut cfg = IonConfig::default();
        cfg.default_provider = Some("global-prov".into());
        cfg.default_model = Some("global-model".into());
        cfg.api_key = Some("global-key".into());
        cfg.tier_models.insert("fast".into(), "global/fast".into());
        cfg.tier_models.insert("pro".into(), "global/pro".into());
        cfg.extensions.insert(
            "memory".into(),
            ExtensionConfig { enabled: true },
        );
        cfg.runtime.backends.insert(
            "local".into(),
            BackendConfig {
                backend_type: "local".into(),
                ..Default::default()
            },
        );
        cfg.runtime.command_guard.mode = "whitelist".into();
        cfg.runtime.command_guard.whitelist = vec!["npm".into(), "cargo".into()];
        cfg
    }

    #[test]
    fn a1_extensions_merge_enables_default_disabled() {
        // 全局不配 file-snapshot（默认 disabled）；项目级显式启用
        let mut g = IonConfig::default();
        let mut p = IonConfig::default();
        p.extensions.insert(
            "file-snapshot".into(),
            ExtensionConfig { enabled: true },
        );
        g.merge_project(p);
        // 合并后 file-snapshot 应该 enabled=true
        assert_eq!(
            g.extensions.get("file-snapshot").map(|c| c.enabled),
            Some(true),
            "项目级 file-snapshot.enabled=true 应该合并进来"
        );
    }

    #[test]
    fn a2_extensions_override_disables_memory() {
        // 全局 memory.enabled=true；项目级覆盖为 false
        let mut g = global_config();
        let mut p = IonConfig::default();
        p.extensions.insert(
            "memory".into(),
            ExtensionConfig { enabled: false },
        );
        g.merge_project(p);
        assert_eq!(
            g.extensions.get("memory").map(|c| c.enabled),
            Some(false),
            "项目级覆盖全局 extensions.memory.enabled=false"
        );
    }

    #[test]
    fn a3_tier_models_merge_preserves_global_keys() {
        // 全局只配 fast+pro；项目级补 max
        // 注意：IonConfig::default() 会给 tier_models 填默认值（fast/pro/max），
        // 所以测试时需要 clear 掉项目级的默认值，模拟"用户项目级只写了 max"的场景
        let mut g = IonConfig::default();
        g.tier_models.clear();
        g.tier_models.insert("fast".into(), "global/fast".into());
        g.tier_models.insert("pro".into(), "global/pro".into());

        let mut p = IonConfig::default();
        p.tier_models.clear();
        p.tier_models.insert("max".into(), "proj/max".into());
        g.merge_project(p);
        // 全局的 fast+pro 保留，项目级 max 合入
        assert_eq!(g.tier_models.get("fast"), Some(&"global/fast".to_string()));
        assert_eq!(g.tier_models.get("pro"), Some(&"global/pro".to_string()));
        assert_eq!(g.tier_models.get("max"), Some(&"proj/max".to_string()));
    }

    #[test]
    fn a4_tier_models_override_same_key() {
        // 全局 fast→global/flash；项目级 fast→proj/pro
        let mut g = IonConfig::default();
        g.tier_models.clear();
        g.tier_models.insert("fast".into(), "global/fast".into());

        let mut p = IonConfig::default();
        p.tier_models.clear();
        p.tier_models.insert("fast".into(), "proj/pro".into());
        g.merge_project(p);
        assert_eq!(
            g.tier_models.get("fast"),
            Some(&"proj/pro".to_string()),
            "项目级 tier_models.fast 覆盖全局"
        );
    }

    #[test]
    fn a5_runtime_backends_merge() {
        // 全局配 local；项目级补 remote-a
        let mut g = global_config();
        let mut p = IonConfig::default();
        p.runtime.backends.insert(
            "remote-a".into(),
            BackendConfig {
                backend_type: "remote".into(),
                hostname: "host-a".into(),
                ..Default::default()
            },
        );
        g.merge_project(p);
        assert!(g.runtime.backends.contains_key("local"), "全局 backend 保留");
        assert!(
            g.runtime.backends.contains_key("remote-a"),
            "项目级 backend 合入"
        );
    }

    #[test]
    fn a6_command_guard_mode_override() {
        // 全局 mode=whitelist；项目级 mode=open
        let mut g = global_config();
        let mut p = IonConfig::default();
        p.runtime.command_guard.mode = "open".into();
        g.merge_project(p);
        assert_eq!(g.runtime.command_guard.mode, "open");
        // 全局 whitelist 不该被清空（项目级没配 whitelist）
        assert!(
            !g.runtime.command_guard.whitelist.is_empty(),
            "全局 whitelist 应保留"
        );
    }

    #[test]
    fn a7_providers_merge() {
        // 全局配 zai；项目级补 anthropic
        let mut g = IonConfig::default();
        g.providers.insert(
            "zai".into(),
            CustomProvider {
                name: "zai".into(),
                api: "anthropic-messages".into(),
                base_url: "https://zai".into(),
                api_key: None,
                headers: None,
                models: vec![],
                model_overrides: None,
            },
        );
        let mut p = IonConfig::default();
        p.providers.insert(
            "anthropic".into(),
            CustomProvider {
                name: "anthropic".into(),
                api: "anthropic-messages".into(),
                base_url: "https://anthropic".into(),
                api_key: None,
                headers: None,
                models: vec![],
                model_overrides: None,
            },
        );
        g.merge_project(p);
        assert!(g.providers.contains_key("zai"), "全局 provider 保留");
        assert!(g.providers.contains_key("anthropic"), "项目级 provider 合入");
    }

    #[test]
    fn a8_api_key_not_cleared_by_empty_project() {
        // 全局 api_key=global-key；项目级不写 api_key
        let mut g = global_config();
        let p = IonConfig::default();
        g.merge_project(p);
        assert_eq!(
            g.api_key,
            Some("global-key".into()),
            "项目级没配 api_key 时，全局的不该被清空"
        );
    }

    #[test]
    fn a8b_api_key_override_when_project_specifies() {
        // 项目级显式写了 api_key → 覆盖
        let mut g = global_config();
        let mut p = IonConfig::default();
        p.api_key = Some("proj-key".into());
        g.merge_project(p);
        assert_eq!(g.api_key, Some("proj-key".into()));
    }

    #[test]
    fn a9_tier_models_serde_default_trap() {
        // serde 默认值陷阱修复：
        // 全局配了 tier_models.fast = "custom/model"
        // 项目级 config 文件没写 tier_models，但 serde 会填默认值（fast/pro/max）
        // merge 后全局的 fast 不该被项目级的默认值覆盖
        let mut g = IonConfig::default();
        g.tier_models.clear();
        g.tier_models.insert("fast".into(), "custom/model".into());

        // 项目级：IonConfig::default() 会给 tier_models 填默认值（模拟从文件反序列化）
        let p = IonConfig::default(); // tier_models = 默认的 fast/pro/max

        g.merge_project(p);

        // 全局的 fast 应该保留（项目级的 tier_models 是默认值，不该覆盖）
        assert_eq!(
            g.tier_models.get("fast"),
            Some(&"custom/model".to_string()),
            "项目级 serde 默认值不该覆盖全局的显式 tier_models"
        );
    }
}
