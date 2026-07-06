//! BackendRegistry — 路由层
//!
//! 三层数据模型：
//! 1. backends: HashMap<String, Box<dyn Runtime>>  — 后端定义
//! 2. default: String                              — 默认 backend 名
//! 3. routes: Vec<RouteRule>                        — 例外规则（仅列与 default 不一致的）
//!
//! 路由匹配顺序：
//!   execute_command → 遍历 routes，匹配 command 前缀
//!   read/write/... → 遍历 routes，匹配（规范化后的）path 前缀
//!   都未匹配 → default
//!
//! 所有 backend 通过名字引用。新加 backend 类型 = 在 from_config 里加一行构造分支。

use crate::config::{BackendConfig, CommandGuardConfig, RouteRule, RuntimeConfig};
use crate::runtime::{
    LocalRuntime, RemoteRuntime, Runtime, SandboxRuntime, SecuredRuntime,
    sh_quote,
};
use crate::kernel::SecurityProfile;
use std::collections::HashMap;

// NOTE: SecuredRuntime<R: Runtime> requires R to be a concrete Runtime type.
// Box<dyn Runtime> does not implement Runtime, so we must wrap *before* boxing.

/// 用 SecurityProfile 默认值 + CommandGuardConfig 配置驱动创建 SecuredRuntime
fn build_secured<R: crate::runtime::Runtime + 'static>(
    inner: R,
    guard_cfg: Option<&CommandGuardConfig>,
) -> Box<dyn Runtime> {
    let mut secured = SecuredRuntime::new(inner).with_profile(SecurityProfile::default());

    // 如果用户配置了 command_guard，覆盖默认值
    if let Some(cfg) = guard_cfg {
        if !cfg.mode.is_empty() && cfg.mode != "whitelist" {
            // 非默认模式 → 重建 CommandGuard
            let mode_override = match cfg.mode.as_str() {
                "blacklist" => crate::command_guard::GuardMode::Blacklist,
                "open" => crate::command_guard::GuardMode::Open,
                "whitelist" | _ => crate::command_guard::GuardMode::Whitelist,
            };
            let mut guard = crate::command_guard::CommandGuard::with_mode(mode_override);

            // 用户 whitelist 覆盖默认
            if !cfg.whitelist.is_empty() {
                guard.whitelist = cfg.whitelist.clone();
            }

            // 追加用户自定义风险模式
            for rp in &cfg.risk_patterns {
                let level = match rp.level.as_str() {
                    "high" => crate::command_guard::RiskLevel::High,
                    "medium" => crate::command_guard::RiskLevel::Medium,
                    "low" | _ => crate::command_guard::RiskLevel::Low,
                };
                guard.risk_patterns.push(crate::command_guard::RiskPattern {
                    pattern: rp.pattern.clone(),
                    message: rp.message.clone(),
                    level,
                    suggestion: rp.suggestion.clone(),
                });
            }

            secured = secured.with_command_guard(guard);
        } else if !cfg.whitelist.is_empty() || !cfg.risk_patterns.is_empty() {
            // whitelist 模式，但修改了 whitelist/risk_patterns
            let mut guard = crate::command_guard::CommandGuard::with_mode(crate::command_guard::GuardMode::Whitelist);
            if !cfg.whitelist.is_empty() {
                guard.whitelist = cfg.whitelist.clone();
            }
            for rp in &cfg.risk_patterns {
                let level = match rp.level.as_str() {
                    "high" => crate::command_guard::RiskLevel::High,
                    "medium" => crate::command_guard::RiskLevel::Medium,
                    "low" | _ => crate::command_guard::RiskLevel::Low,
                };
                guard.risk_patterns.push(crate::command_guard::RiskPattern {
                    pattern: rp.pattern.clone(),
                    message: rp.message.clone(),
                    level,
                    suggestion: rp.suggestion.clone(),
                });
            }
            secured = secured.with_command_guard(guard);
        }
    }

    Box::new(secured)
}
// The `wrap_*` helpers below each take a concrete runtime and return a secured Box.

// ---------------------------------------------------------------------------
// BackendRegistry — 路由层
// ---------------------------------------------------------------------------

pub struct BackendRegistry {
    /// 所有已注册的后端（名字 → Runtime 实例）
    backends: HashMap<String, Box<dyn Runtime>>,
    /// 默认 backend 名（未匹配 routes 时走这里）
    default_name: String,
    /// 例外路由规则
    routes: Vec<RouteRule>,
}

impl BackendRegistry {
    /// 从配置构造。
    ///
    /// 新风格 (backends 字段非空)：按配置创建每个 backend。
    /// 旧风格 (default_mode + remote + sandbox)：构造兼容后端。
    pub fn from_config(cfg: &RuntimeConfig, workspace: &str) -> Self {
        // ── 新风格：使用 backends 配置 ──
        if cfg.uses_backends() {
            return Self::from_backends_config(cfg, workspace);
        }

        // ── 旧风格兼容：把 default_mode + remote/sandbox 映射成 backends ──
        Self::from_legacy_config(cfg, workspace)
    }

    fn from_backends_config(cfg: &RuntimeConfig, workspace: &str) -> Self {
        let mut backends: HashMap<String, Box<dyn Runtime>> = HashMap::new();

        // 总是注入一个 "local" 兜底（如果用户没显式定义）
        if !cfg.backends.contains_key("local") {
            backends.insert(
                "local".into(),
                build_secured(LocalRuntime::new(), Some(&cfg.command_guard)),
            );
        }

        for (name, spec) in &cfg.backends {
            match Self::build_backend(name, spec, &cfg.remote, workspace, Some(&cfg.command_guard)) {
                Ok(rt) => { backends.insert(name.clone(), rt); }
                Err(e) => {
                    tracing::warn!("[backend-registry] backend '{}' 创建失败: {} — 引用时回退 default", name, e);
                }
            }
        }

        // default_name：检查实际构建好的 backends map（而非原始 cfg.backends）。
        // build_backend 可能失败（如未知 type），所以必须用运行时 map 验证。
        let default_name = if backends.contains_key(&cfg.default) {
            cfg.default.clone()
        } else if !cfg.default.is_empty() {
            tracing::warn!("[backend-registry] default='{}' 未成功创建或不存在，回退 local", cfg.default);
            "local".into()
        } else {
            "local".into()
        };

        Self {
            backends,
            default_name,
            routes: cfg.routes.clone(),
        }
    }

    fn from_legacy_config(cfg: &RuntimeConfig, workspace: &str) -> Self {
        let mut backends: HashMap<String, Box<dyn Runtime>> = HashMap::new();
        backends.insert(
            "local".into(),
            build_secured(LocalRuntime::new(), Some(&cfg.command_guard)),
        );

        let default_name = match cfg.default_mode.as_str() {
            "remote" => {
                let host_name = if cfg.remote.default_host.is_empty() {
                    "remote_default"
                } else {
                    &cfg.remote.default_host
                };
                if let Some(host_cfg) = cfg.remote.hosts.get(&cfg.remote.default_host) {
                    let remote = RemoteRuntime::from_config(LocalRuntime::new(), host_cfg);
                    backends.insert(
                        host_name.into(),
                        build_secured(remote, Some(&cfg.command_guard)),
                    );
                    host_name.into()
                } else {
                    tracing::warn!("[backend-registry] legacy remote host '{}' 未定义，回退 local", cfg.remote.default_host);
                    "local".into()
                }
            }
            "sandbox" => {
                let sandbox = SandboxRuntime::new(LocalRuntime::new(), &cfg.sandbox.profile, workspace);
                backends.insert(
                    "sandbox_default".into(),
                    build_secured(sandbox, Some(&cfg.command_guard)),
                );
                "sandbox_default".into()
            }
            _ => "local".into(),
        };

        // 旧 routes 的 runtime/host 自动映射成 target
        let routes: Vec<RouteRule> = cfg.routes.iter().map(|r| {
            let mut nr = r.clone();
            if nr.target.is_empty() {
                nr.target = r.effective_target();
            }
            // 把旧 pattern 字段映射成 command 或 path（启发式：以 / 开头当 path，否则当 command）
            if nr.command.is_empty() && nr.path.is_empty() && !nr.pattern.is_empty() {
                if nr.pattern.starts_with('/') {
                    nr.path = nr.pattern.clone();
                } else if nr.tool == "bash" || nr.tool.is_empty() {
                    nr.command = nr.pattern.clone();
                } else {
                    nr.path = nr.pattern.clone();
                }
            }
            nr
        }).collect();

        Self { backends, default_name, routes }
    }

    /// 根据 BackendConfig 构造单个 Runtime 实例
    fn build_backend(name: &str, spec: &BackendConfig, remote_pool: &crate::config::RemoteConfig, workspace: &str, guard_cfg: Option<&CommandGuardConfig>) -> Result<Box<dyn Runtime>, String> {
        match spec.backend_type.as_str() {
            "local" => Ok(build_secured(LocalRuntime::new(), guard_cfg)),
            "remote" => {
                let host = if spec.hostname.is_empty() {
                    return Err("remote backend 缺少 hostname".into());
                } else {
                    spec.hostname.clone()
                };
                let user = spec.user.clone();
                let port = spec.port.unwrap_or(22);
                let key = spec.key.clone();
                let proxy = spec.proxy_jump.clone();
                let remote = RemoteRuntime::new(LocalRuntime::new(), &user, &host, port, &key, &proxy);
                Ok(build_secured(remote, guard_cfg))
            }
            "sandbox" => {
                let profile = if spec.profile.is_empty() { "workspace" } else { &spec.profile };
                let sandbox = SandboxRuntime::new(LocalRuntime::new(), profile, workspace);
                Ok(build_secured(sandbox, guard_cfg))
            }
            "container" => {
                let driver = if spec.driver.is_empty() { "apple" } else { &spec.driver };
                match driver {
                    "apple" => {
                        if spec.image.is_empty() {
                            return Err("apple container backend 缺少 image".into());
                        }
                        let container_name = format!("ion-{}", name);
                        let container = AppleContainerRuntime::new(
                            spec.image.clone(),
                            container_name,
                            spec.workspace.clone(),
                            spec.mount_path.clone(),
                            spec.container_port.unwrap_or(0),
                            spec.memory.clone(),
                            spec.cpus,
                            spec.volume.clone(),
                        );
                        Ok(build_secured(container, guard_cfg))
                    }
                    "docker" | "podman" => Err(format!("{} driver 暂未实现", driver)),
                    other => Err(format!("未知 container driver: {}", other)),
                }
            }
            other => Err(format!("未知 backend type: {}", other)),
        }
    }

    /// 命令路由：遍历 routes 匹配 command 前缀
    fn resolve_command(&self, command: &str) -> &dyn Runtime {
        for rule in &self.routes {
            if !rule.command.is_empty() && glob_match(&rule.command, command) {
                let target = rule.effective_target();
                if let Some(rt) = self.backends.get(&target) {
                    return rt.as_ref();
                }
                // target 未注册，跳过此规则（继续匹配下一条或走 default）
                tracing::warn!("[backend-registry] command rule target '{}' 未注册，跳过", target);
            }
            // 兼容旧 pattern：当 tool 为空或 bash 时尝试匹配
            if !rule.pattern.is_empty() && rule.command.is_empty() && rule.path.is_empty() {
                if (rule.tool.is_empty() || rule.tool == "bash" || rule.tool == "*")
                    && glob_match(&rule.pattern, command) {
                    let target = rule.effective_target();
                    if let Some(rt) = self.backends.get(&target) {
                        return rt.as_ref();
                    }
                }
            }
        }
        self.backends.get(&self.default_name)
            .map(|b| b.as_ref())
            .unwrap_or_else(|| panic!("[backend-registry] default backend '{}' 未注册 (内部错误)", self.default_name))
    }

    /// 路径路由：规范化后遍历 routes 匹配最长前缀
    fn resolve_path(&self, path: &str) -> &dyn Runtime {
        let canon = canonicalize_path(path);

        // 多条路径规则匹配时取最长前缀
        let mut best: Option<(usize, &dyn Runtime)> = None;
        for rule in &self.routes {
            if rule.path.is_empty() { continue; }
            let rule_canon = canonicalize_path(&rule.path);
            // pattern 末尾的 * 是通配符提示，取前缀
            let prefix = rule_canon.trim_end_matches('*');
            if canon.starts_with(prefix) {
                let target = rule.effective_target();
                if let Some(rt) = self.backends.get(&target) {
                    let score = prefix.len();
                    if best.map(|(s, _)| score > s).unwrap_or(true) {
                        best = Some((score, rt.as_ref()));
                    }
                }
            }
        }
        if let Some((_, rt)) = best { return rt; }

        self.backends.get(&self.default_name)
            .map(|b| b.as_ref())
            .unwrap_or_else(|| panic!("[backend-registry] default backend '{}' 未注册 (内部错误)", self.default_name))
    }

    /// 列出所有后端名（调试用）
    pub fn list_backends(&self) -> Vec<String> {
        self.backends.keys().cloned().collect()
    }
}

// ---------------------------------------------------------------------------
// Runtime trait 实现：BackendRegistry 自己也是一个 Runtime
// 路由所有方法到对应的 backend
// ---------------------------------------------------------------------------

#[async_trait::async_trait]
impl Runtime for BackendRegistry {
    fn runtime_type(&self) -> String {
        format!("router(default={}, backends={})", self.default_name, self.backends.len())
    }

    async fn execute_command(&self, command: &str, timeout_secs: u64) -> Result<(String, String, i32), String> {
        self.resolve_command(command).execute_command(command, timeout_secs).await
    }
    async fn execute_command_stream(
        &self, command: &str, timeout_secs: u64,
        on_update: &(dyn Fn(String) + Send + Sync),
    ) -> Result<String, String> {
        self.resolve_command(command).execute_command_stream(command, timeout_secs, on_update).await
    }
    async fn read_file(&self, path: &str) -> Result<String, String> {
        self.resolve_path(path).read_file(path).await
    }
    async fn write_file(&self, path: &str, content: &str) -> Result<(), String> {
        self.resolve_path(path).write_file(path, content).await
    }
    async fn edit_file(&self, path: &str, old: &str, new: &str) -> Result<(), String> {
        self.resolve_path(path).edit_file(path, old, new).await
    }
    async fn path_exists(&self, path: &str) -> bool {
        self.resolve_path(path).path_exists(path).await
    }
    async fn list_dir(&self, path: &str) -> Result<Vec<String>, String> {
        self.resolve_path(path).list_dir(path).await
    }
    async fn remove_file(&self, path: &str) -> Result<(), String> {
        self.resolve_path(path).remove_file(path).await
    }
    async fn grep_search(&self, pattern: &str, path: &str) -> Result<Vec<String>, String> {
        self.resolve_path(path).grep_search(pattern, path).await
    }
    async fn find_files(&self, path: &str, name: &str) -> Result<Vec<String>, String> {
        self.resolve_path(path).find_files(path, name).await
    }
    async fn file_info(&self, path: &str) -> Result<Vec<crate::runtime::FileEntry>, String> {
        self.resolve_path(path).file_info(path).await
    }
    async fn check_command(&self, cmd: &str) -> Result<(), String> {
        self.resolve_command(cmd).check_command(cmd).await
    }
    async fn spawn_process(&self, req: crate::runtime::SpawnProcessRequest) -> Result<crate::runtime::ProcessHandle, String> {
        self.resolve_command(&req.command).spawn_process(req).await
    }
    async fn kill_process(&self, pid: u32) -> Result<(), String> {
        // kill 不需要路由——但默认走 default
        self.backends.get(&self.default_name)
            .unwrap_or_else(|| panic!("default backend missing"))
            .kill_process(pid).await
    }
    async fn send_stdin(&self, pid: u32, input: &str) -> Result<(), String> {
        self.backends.get(&self.default_name)
            .unwrap_or_else(|| panic!("default backend missing"))
            .send_stdin(pid, input).await
    }
}

// ---------------------------------------------------------------------------
// 辅助函数
// ---------------------------------------------------------------------------

/// 路径规范化：~ 展开、.. 解析、// 合并
///
/// 不访问文件系统（不做 symlink 解析），只做字符串级规范化。
/// 这保证：
/// 1. 规范化结果确定（不受文件系统状态影响）
/// 2. 不产生副作用（不会因为路径不存在而失败）
/// 3. 安全：`..` 不能逃逸到挂载点之外
pub fn canonicalize_path(path: &str) -> String {
    let mut p = path.to_string();

    // 1. 展开 ~ 和 ~user
    if p == "~" {
        p = std::env::var("HOME").unwrap_or_else(|_| "~".into());
    } else if let Some(rest) = p.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            p = format!("{}/{}", home, rest);
        }
    }
    // ~user 形式：不展开（需要查 passwd），保持原样

    // 2. 把 // 合并成 /
    while p.contains("//") {
        p = p.replace("//", "/");
    }

    // 3. 解析 . 和 ..
    let mut parts: Vec<&str> = Vec::new();
    let is_absolute = p.starts_with('/');
    for seg in p.split('/') {
        match seg {
            "" | "." => {} // 跳过
            ".." => {
                // 弹出最后一个非 .. 的段
                if !parts.is_empty() && *parts.last().unwrap() != ".." {
                    parts.pop();
                } else if !is_absolute {
                    parts.push("..");
                }
                // 绝对路径下 .. 在根目录处被丢弃
            }
            other => parts.push(other),
        }
    }

    let joined = parts.join("/");
    if is_absolute {
        format!("/{}", joined)
    } else if joined.is_empty() {
        ".".into()
    } else {
        joined
    }
}

/// 简单 glob：`*` 匹配任意字符
///
/// 语义：
/// - `pattern == "*"` → 匹配任何 s
/// - pattern 以 `*` 结尾（如 "npm *"）：把 `*` 当通配符，前面部分需作为前缀匹配
///   - "npm *" 去掉 `*` 后是 "npm "（含空格）→ s 必须以 "npm " 开头（"npm install" 匹配，裸 "npm" 不匹配）
/// - pattern 不以 `*` 结尾：精确匹配
fn glob_match(pattern: &str, s: &str) -> bool {
    if pattern == "*" { return true; }
    if let Some(prefix) = pattern.strip_suffix('*') {
        // prefix 末尾通常带空格（如 "npm "），保留原样做前缀匹配
        // 例：pattern "npm *" → prefix "npm " → s 必须以 "npm " 开头
        s.starts_with(prefix)
    } else if let Some(p) = pattern.strip_prefix('*') {
        s.ends_with(p)
    } else {
        pattern == s
    }
}

// ---------------------------------------------------------------------------
// AppleContainerRuntime — Apple Container VM 隔离后端
// ---------------------------------------------------------------------------

/// 通过 Apple Container (`/usr/local/bin/container`) 在 Linux VM 中执行命令。
///
/// 特点：
/// - 每个实例对应一个独立的容器（独立 IP、独立文件系统）
/// - 多个实例可并存（同端口不同 IP）
/// - 容器名格式：`ion-{backend_name}`（如 `ion-apple-dev`）
/// - 内部实现：所有命令通过 `container exec <name> sh -c '{cmd}'` 执行
///
/// **生命周期：**
/// - `new()` 只记录配置，不创建容器（lazy 模式）
/// - 第一次调用时 `container inspect` 检查容器是否存在
/// - 不存在则 `container run` 创建（后台 `sleep infinity` 保活）
/// - 后续调用直接 `container exec`
/// - `Drop` 时调用 `container stop` 清理
pub struct AppleContainerRuntime {
    image: String,
    /// 容器名（如 "ion-apple-dev"）。用于 exec/inspect/stop。
    container_name: String,
    /// 主机侧 worktree 路径（可选，挂载到 mount_path）
    workspace: String,
    /// 容器内挂载点（如 /workspace）
    mount_path: String,
    /// 提示用端口号，不传给 container run（Apple Container 用 IP 暴露端口）
    port: u16,
    memory: String,
    cpus: Option<u32>,
    /// 共享卷名（可选）
    volume: String,
    /// 是否已启动
    started: tokio::sync::OnceCell<bool>,
}

impl AppleContainerRuntime {
    pub fn new(
        image: String,
        container_name: String,
        workspace: String,
        mount_path: String,
        port: u16,
        memory: String,
        cpus: Option<u32>,
        volume: String,
    ) -> Self {
        Self {
            image,
            container_name,
            workspace,
            mount_path,
            port,
            memory,
            cpus,
            volume,
            started: tokio::sync::OnceCell::new(),
        }
    }

    /// 懒启动容器。先 inspect 检查是否已存在，不存在则 run。
    async fn ensure_started(&self) -> Result<&str, String> {
        self.started.get_or_try_init(|| async {
            // 1. 确认 container 服务运行
            let start_out = tokio::process::Command::new("/usr/local/bin/container")
                .args(["system", "start"])
                .output().await;
            if let Err(e) = start_out {
                return Err(format!("container system start 失败: {e}"));
            }
            if let Ok(out) = start_out {
                if !out.status.success() {
                    let err = String::from_utf8_lossy(&out.stderr);
                    if !err.trim().is_empty() && !err.contains("already running") {
                        return Err(format!("container system start 失败: {err}"));
                    }
                }
            }

            // 2. 检查容器是否已存在
            let inspect_out = tokio::process::Command::new("/usr/local/bin/container")
                .args(["inspect", &self.container_name])
                .output().await;
            if let Ok(out) = inspect_out {
                if out.status.success() {
                    // 容器已存在，直接使用
                    return Ok(true);
                }
            }

            // 3. 构造 container run 命令（符合真实 CLI 语法，参考 run-worktree-container.sh）
            let mut cmd = tokio::process::Command::new("/usr/local/bin/container");
            cmd.arg("run");
            cmd.arg("--name").arg(&self.container_name);
            cmd.arg("--detach");
            cmd.arg("--rm");
            cmd.arg("--network").arg("default");

            // 挂载 worktree（可选）
            if !self.workspace.is_empty() && !self.mount_path.is_empty() {
                cmd.arg("-v").arg(format!("{}:{}", self.workspace, self.mount_path));
                cmd.arg("-w").arg(&self.mount_path);
            }
            if !self.memory.is_empty() {
                cmd.arg("--memory").arg(&self.memory);
            }
            if let Some(cpus) = self.cpus {
                cmd.arg("--cpus").arg(cpus.to_string());
            }
            if !self.volume.is_empty() {
                cmd.arg("--volume").arg(&self.volume);
            }

            // 镜像和启动命令（位置参数）
            cmd.arg(&self.image);
            cmd.arg("sh").arg("-lc").arg("sleep infinity");

            let output = cmd.output().await
                .map_err(|e| format!("container run 失败: {e}"))?;
            if !output.status.success() {
                let err = String::from_utf8_lossy(&output.stderr);
                return Err(format!("container run 失败 ({})", err.trim()));
            }

            Ok(true)
        }).await?;
        Ok(&self.container_name)
    }

    /// 在容器内执行命令
    async fn exec_in_container(&self, cmd: &str, timeout_secs: u64) -> Result<(String, String, i32), String> {
        let name = self.ensure_started().await?;
        let exec_cmd = format!(
            "/usr/local/bin/container exec {} sh -c {}",
            name, sh_quote(cmd)
        );
        let local = LocalRuntime::new();
        local.execute_command(&exec_cmd, timeout_secs).await
    }

    /// 获取容器的 IP 地址
    #[allow(dead_code)]
    pub async fn get_ip(&self) -> Result<String, String> {
        let name = self.ensure_started().await?;
        let local = LocalRuntime::new();
        // 用 container inspect 解析 JSON 取 IP
        let (out, _, _) = local.execute_command(
            &format!("/usr/local/bin/container inspect {}", name),
            10,
        ).await?;
        // 解析 JSON：取 networks[0].ipv4Address
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&out) {
            if let Some(arr) = v.as_array() {
                if let Some(first) = arr.first() {
                    if let Some(networks) = first.get("networks") {
                        if let Some(net_arr) = networks.as_array() {
                            if let Some(net) = net_arr.first() {
                                if let Some(ip) = net.get("ipv4Address").and_then(|i| i.as_str()) {
                                    return Ok(ip.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
        Err(format!("无法从 container inspect 解析 IP: {out}"))
    }

    /// 停止容器。Worker 关闭时调用。
    pub async fn stop(&self) -> Result<(), String> {
        let local = LocalRuntime::new();
        let (_, err, code) = local.execute_command(
            &format!("/usr/local/bin/container stop {}", &self.container_name),
            15,
        ).await?;
        if code != 0 {
            return Err(format!("container stop 失败: {err}"));
        }
        Ok(())
    }
}

impl Drop for AppleContainerRuntime {
    fn drop(&mut self) {
        // 尝试同步 stop 容器。如果异步 runtime 不可用，静默跳过。
        let name = self.container_name.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new();
            if let Ok(rt) = rt {
                rt.block_on(async {
                    let local = LocalRuntime::new();
                    let _ = local.execute_command(
                        &format!("/usr/local/bin/container stop {}", name),
                        10,
                    ).await;
                });
            }
        });
    }
}

#[async_trait::async_trait]
impl Runtime for AppleContainerRuntime {
    fn runtime_type(&self) -> String {
        format!("apple-container({})", self.image)
    }

    async fn execute_command(&self, command: &str, timeout_secs: u64) -> Result<(String, String, i32), String> {
        self.exec_in_container(command, timeout_secs).await
    }

    async fn execute_command_stream(
        &self, command: &str, timeout_secs: u64,
        on_update: &(dyn Fn(String) + Send + Sync),
    ) -> Result<String, String> {
        let (out, _, _) = self.exec_in_container(command, timeout_secs).await?;
        on_update(out.clone());
        Ok(out)
    }

    async fn read_file(&self, path: &str) -> Result<String, String> {
        let (out, err, code) = self.exec_in_container(&format!("cat {}", sh_quote(path)), 30).await?;
        if code != 0 { Err(format!("read: {err}")) } else { Ok(out) }
    }
    async fn write_file(&self, path: &str, content: &str) -> Result<(), String> {
        let esc = content.replace('\'', "'\\''");
        let (_, err, code) = self.exec_in_container(
            &format!("mkdir -p $(dirname {}) && cat > {} << 'IONEOF'\n{}\nIONEOF", sh_quote(path), sh_quote(path), esc),
            30,
        ).await?;
        if code != 0 { Err(format!("write: {err}")) } else { Ok(()) }
    }
    async fn edit_file(&self, path: &str, old: &str, new: &str) -> Result<(), String> {
        let content = self.read_file(path).await?;
        self.write_file(path, &content.replace(old, new)).await
    }
    async fn path_exists(&self, path: &str) -> bool {
        self.exec_in_container(&format!("test -e {}", sh_quote(path)), 10).await
            .map(|(_, _, c)| c == 0).unwrap_or(false)
    }
    async fn list_dir(&self, path: &str) -> Result<Vec<String>, String> {
        let (out, _, _) = self.exec_in_container(&format!("ls -1 {}", sh_quote(path)), 15).await?;
        Ok(out.lines().map(String::from).collect())
    }
    async fn remove_file(&self, path: &str) -> Result<(), String> {
        let (_, err, code) = self.exec_in_container(&format!("rm -f {}", sh_quote(path)), 15).await?;
        if code != 0 { Err(format!("rm: {err}")) } else { Ok(()) }
    }
    async fn grep_search(&self, pattern: &str, path: &str) -> Result<Vec<String>, String> {
        let (out, _, _) = self.exec_in_container(
            &format!("grep -rn {} {} 2>/dev/null || true", sh_quote(pattern), sh_quote(path)),
            30,
        ).await?;
        Ok(out.lines().map(String::from).collect())
    }
    async fn find_files(&self, path: &str, name: &str) -> Result<Vec<String>, String> {
        let (out, _, _) = self.exec_in_container(
            &format!("find {} -name {} 2>/dev/null || true", sh_quote(path), sh_quote(name)),
            30,
        ).await?;
        Ok(out.lines().map(String::from).filter(|l| !l.is_empty()).collect())
    }
    async fn file_info(&self, path: &str) -> Result<Vec<crate::runtime::FileEntry>, String> {
        let (out, _, _) = self.exec_in_container(
            &format!("ls -la {} 2>/dev/null || true", sh_quote(path)),
            15,
        ).await?;
        let mut v = Vec::new();
        for line in out.lines().skip(1) {
            if !line.is_empty() {
                let p: Vec<&str> = line.split_whitespace().collect();
                if p.len() >= 9 {
                    v.push(crate::runtime::FileEntry {
                        name: p[8..].join(" "),
                        is_dir: line.starts_with('d'),
                        size: p[4].parse().unwrap_or(0),
                        modified: p[5..8].join(" "),
                    });
                }
            }
        }
        Ok(v)
    }
    async fn check_command(&self, _cmd: &str) -> Result<(), String> { Ok(()) }
    async fn spawn_process(&self, _req: crate::runtime::SpawnProcessRequest) -> Result<crate::runtime::ProcessHandle, String> {
        Err("AppleContainerRuntime: spawn_process 暂不支持".into())
    }
    async fn kill_process(&self, _pid: u32) -> Result<(), String> {
        Err("AppleContainerRuntime: kill_process 暂不支持".into())
    }
    async fn send_stdin(&self, _pid: u32, _input: &str) -> Result<(), String> {
        Err("AppleContainerRuntime: send_stdin 暂不支持".into())
    }
}

// ---------------------------------------------------------------------------
// 单元测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ── canonicalize_path 测试（R3.2）──

    #[test]
    fn test_canonicalize_tilde() {
        // SAFETY: save and restore HOME to avoid test pollution
        let original_home = std::env::var("HOME").ok();
        unsafe { std::env::set_var("HOME", "/Users/test"); }
        assert_eq!(canonicalize_path("~/.ion/skill.md"), "/Users/test/.ion/skill.md");
        match original_home {
            Some(h) => unsafe { std::env::set_var("HOME", h); },
            None => unsafe { std::env::remove_var("HOME"); },
        }
    }

    #[test]
    fn test_canonicalize_dotdot() {
        assert_eq!(canonicalize_path("/usr/local/../bin"), "/usr/bin");
    }

    #[test]
    fn test_canonicalize_double_slash() {
        assert_eq!(canonicalize_path("/usr//local///bin"), "/usr/local/bin");
    }

    #[test]
    fn test_canonicalize_dot_segments() {
        assert_eq!(canonicalize_path("/a/./b/../c"), "/a/c");
    }

    #[test]
    fn test_canonicalize_traversal_to_root() {
        // /tmp/../../../etc → /etc（不能逃出根）
        assert_eq!(canonicalize_path("/tmp/../../../etc"), "/etc");
    }

    #[test]
    fn test_canonicalize_preserves_tilde_user() {
        // ~user 形式保持不变（不查 passwd）
        assert_eq!(canonicalize_path("~root/.ion"), "~root/.ion");
    }

    // ── glob_match 测试（R2.3）──

    #[test]
    fn test_glob_star_wildcard() {
        assert!(glob_match("npm *", "npm install lodash"));
        assert!(glob_match("npm *", "npm run build"));
    }

    #[test]
    fn test_glob_no_arg_no_match() {
        // "npm *" 不匹配裸 "npm"
        assert!(!glob_match("npm *", "npm"));
    }

    #[test]
    fn test_glob_substring_no_match() {
        // "npm *" 不应匹配 "npm-check-updates"（因为它不等于 "npm" 也不以 "npm " 开头）
        assert!(!glob_match("npm *", "npm-check-updates"));
    }

    #[test]
    fn test_glob_compound_command_no_match() {
        // "npm *" 不匹配 "npm install && cargo build" 的整体 — 它以 "npm " 开头所以会匹配
        // 这是预期行为：一旦匹配到 npm，整条命令都路由到 local（cargo build 也在 local 执行）
        assert!(glob_match("npm *", "npm install && cargo build"));
    }

    // ── BackendRegistry 测试（R1, R6, R6b）──

    fn empty_cfg_with_default(default: &str) -> RuntimeConfig {
        let mut cfg = RuntimeConfig::default();
        cfg.default = default.into();
        cfg.backends.insert("local".into(), BackendConfig {
            backend_type: "local".into(),
            ..Default::default()
        });
        cfg
    }

    #[test]
    fn test_default_local_no_routes() {
        let cfg = empty_cfg_with_default("local");
        let reg = BackendRegistry::from_config(&cfg, "/tmp");
        // resolve_command / resolve_path 走 default=local
        assert_eq!(reg.default_name, "local");
        assert_eq!(reg.backends.len(), 1);
    }

    #[test]
    fn test_default_unknown_falls_back_to_local() {
        let mut cfg = RuntimeConfig::default();
        cfg.default = "nonexistent".into();
        cfg.backends.insert("local".into(), BackendConfig {
            backend_type: "local".into(),
            ..Default::default()
        });
        let reg = BackendRegistry::from_config(&cfg, "/tmp");
        assert_eq!(reg.default_name, "local"); // 回退
    }

    #[test]
    fn test_route_target_unknown_skipped() {
        let mut cfg = empty_cfg_with_default("local");
        cfg.routes.push(RouteRule {
            command: "npm *".into(),
            target: "nonexistent_backend".into(),
            ..Default::default()
        });
        let reg = BackendRegistry::from_config(&cfg, "/tmp");
        // npm 规则 target 不存在 → 应跳过 → 走 default=local
        let rt = reg.resolve_command("npm install");
        assert_eq!(rt.runtime_type(), "secured(local)");
    }

    #[test]
    fn test_route_empty_matcher_skipped() {
        let mut cfg = empty_cfg_with_default("local");
        cfg.routes.push(RouteRule {
            command: "".into(),
            path: "".into(),
            pattern: "".into(),
            target: "local".into(),
            ..Default::default()
        });
        let reg = BackendRegistry::from_config(&cfg, "/tmp");
        // 空 matcher 的规则应被跳过（不应 panic）
        assert_eq!(reg.routes.len(), 1);
    }

    #[test]
    fn test_path_normalization_before_match() {
        // 路径规则用规范化路径匹配
        let mut cfg = empty_cfg_with_default("local");
        // 加一个 remote backend 让 default 有选择
        cfg.backends.insert("remote1".into(), BackendConfig {
            backend_type: "remote".into(),
            hostname: "example.com".into(),
            ..Default::default()
        });
        cfg.default = "remote1".into();
        // 配置规则：/Users/xuyingzhou/.ion/* → local
        cfg.routes.push(RouteRule {
            path: "/Users/xuyingzhou/.ion/*".into(),
            target: "local".into(),
            ..Default::default()
        });
        let reg = BackendRegistry::from_config(&cfg, "/tmp");

        // /Users/xuyingzhou/.ion/../.ion/skill.md → 规范化 → 匹配 local
        let rt = reg.resolve_path("/Users/xuyingzhou/.ion/../.ion/skill.md");
        assert_eq!(rt.runtime_type(), "secured(local)");

        // /tmp/test → 未匹配 → default=remote1
        let rt = reg.resolve_path("/tmp/test");
        assert!(rt.runtime_type().contains("remote"));
    }

    #[test]
    fn test_longest_prefix_match() {
        let mut cfg = RuntimeConfig::default();
        cfg.backends.insert("local".into(), BackendConfig {
            backend_type: "local".into(),
            ..Default::default()
        });
        cfg.backends.insert("remote1".into(), BackendConfig {
            backend_type: "remote".into(),
            hostname: "example.com".into(),
            ..Default::default()
        });
        cfg.default = "remote1".into();
        // 两条路径规则：第一条范围广，第二条更精确
        cfg.routes.push(RouteRule {
            path: "/Users/xuyingzhou/*".into(),
            target: "remote1".into(),
            ..Default::default()
        });
        cfg.routes.push(RouteRule {
            path: "/Users/xuyingzhou/.ion/*".into(),
            target: "local".into(),
            ..Default::default()
        });
        let reg = BackendRegistry::from_config(&cfg, "/tmp");

        // /Users/xuyingzhou/.ion/skill.md → 最长前缀匹配 → local
        let rt = reg.resolve_path("/Users/xuyingzhou/.ion/skill.md");
        assert_eq!(rt.runtime_type(), "secured(local)");

        // /Users/xuyingzhou/Project/foo → 仅匹配广规则 → remote1
        let rt = reg.resolve_path("/Users/xuyingzhou/Project/foo.rs");
        assert!(rt.runtime_type().contains("remote"));
    }

    #[test]
    fn test_legacy_compat_default_remote() {
        // 旧风格：default_mode=remote + remote.hosts.shanbox
        let mut cfg = RuntimeConfig::default();
        cfg.default_mode = "remote".into();
        cfg.remote.default_host = "shanbox".into();
        cfg.remote.hosts.insert("shanbox".into(), crate::config::RemoteHost {
            hostname: "shanbox".into(),
            ..Default::default()
        });
        let reg = BackendRegistry::from_config(&cfg, "/tmp");
        assert_eq!(reg.default_name, "shanbox");
        assert!(reg.backends.contains_key("local")); // 自动注入 local 兜底
        assert!(reg.backends.contains_key("shanbox"));
    }

    #[test]
    fn test_legacy_compat_default_local() {
        let mut cfg = RuntimeConfig::default();
        cfg.default_mode = "local".into();
        let reg = BackendRegistry::from_config(&cfg, "/tmp");
        assert_eq!(reg.default_name, "local");
    }

    #[test]
    fn test_legacy_compat_default_remote_host_missing() {
        let mut cfg = RuntimeConfig::default();
        cfg.default_mode = "remote".into();
        cfg.remote.default_host = "nonexistent".into();
        // 不配置 hosts
        let reg = BackendRegistry::from_config(&cfg, "/tmp");
        // host 不存在 → 回退 local
        assert_eq!(reg.default_name, "local");
    }

    #[test]
    fn test_backend_type_unknown() {
        let mut cfg = RuntimeConfig::default();
        cfg.default = "weird".into();
        cfg.backends.insert("weird".into(), BackendConfig {
            backend_type: "unknown_type".into(),
            ..Default::default()
        });
        cfg.backends.insert("local".into(), BackendConfig {
            backend_type: "local".into(),
            ..Default::default()
        });
        let reg = BackendRegistry::from_config(&cfg, "/tmp");
        // unknown_type 创建失败 → "weird" 未注册 → default 回退 local
        assert_eq!(reg.default_name, "local");
    }
}
