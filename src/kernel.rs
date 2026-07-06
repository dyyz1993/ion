use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

// ---------------------------------------------------------------------------
// PermissionEngine — 内核权限引擎
// ---------------------------------------------------------------------------

/// 动作类型
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Read,
    Write,
    Execute,
    Edit,
    Delete,
    Network,
}

impl Action {
    pub fn from_tool(tool_name: &str) -> Self {
        match tool_name {
            "read" | "grep" | "find" | "ls" => Self::Read,
            "write" => Self::Write,
            "edit" => Self::Edit,
            "bash" => Self::Execute,
            _ => Self::Execute,
        }
    }
}

/// 权限检查结果
#[derive(Clone, Debug)]
pub enum PermissionResult {
    /// 允许
    Allow,
    /// 拒绝（带原因）
    Deny(String),
    /// 需要用户确认
    Ask { title: String, message: String },
}

/// 权限规则（由插件注册）
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PermissionRule {
    /// 规则名
    pub name: String,
    /// 匹配的动作
    pub actions: Vec<Action>,
    /// 匹配的模式 (glob 风格, e.g. "**/.env*", "/tmp/**")
    pub pattern: String,
    /// 策略
    pub policy: PermissionPolicy,
    /// 优先级 (数字越大越先匹配)
    pub priority: i32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionPolicy {
    /// 允许
    Allow,
    /// 拒绝
    Deny,
    /// 需要用户确认
    Ask,
}

/// 权限引擎（线程安全，可从任何扩展访问）
pub struct PermissionEngine {
    rules: Arc<RwLock<Vec<PermissionRule>>>,
    /// 用户的确认回调（由 UI 层设置）
    #[allow(clippy::type_complexity)]
    confirm_handler: Arc<RwLock<Option<Box<dyn Fn(&str, &str) -> bool + Send + Sync>>>>,
}

impl PermissionEngine {
    pub fn new() -> Self {
        Self {
            rules: Arc::new(RwLock::new(Vec::new())),
            confirm_handler: Arc::new(RwLock::new(None)),
        }
    }

    /// 注册一条规则（插件调这个）
    pub fn register_rule(&self, rule: PermissionRule) {
        let mut rules = self.rules.write().unwrap();
        rules.push(rule);
        // 按优先级排序（高优先级在前）
        rules.sort_by(|a, b| b.priority.cmp(&a.priority));
    }

    /// 设置用户确认回调（UI 层调这个）
    pub fn set_confirm_handler(&self, handler: Box<dyn Fn(&str, &str) -> bool + Send + Sync>) {
        *self.confirm_handler.write().unwrap() = Some(handler);
    }

    /// 检查权限（扩展在 before_tool_call 调这个）
    pub fn check(&self, path: &str, action: Action) -> PermissionResult {
        let rules = self.rules.read().unwrap();

        for rule in rules.iter() {
            if !rule.actions.contains(&action) {
                continue;
            }
            if !glob_match(&rule.pattern, path) {
                continue;
            }

            return match &rule.policy {
                PermissionPolicy::Allow => PermissionResult::Allow,
                PermissionPolicy::Deny => PermissionResult::Deny(format!(
                    "规则 '{}' 拒绝了 {} on {}",
                    rule.name, format!("{:?}", action).to_lowercase(), path
                )),
                PermissionPolicy::Ask => {
                    let title = format!("权限请求: {}", rule.name);
                    let message = format!("工具想要 {} 路径: {}\n规则: {}", format!("{:?}", action).to_lowercase(), path, rule.name);
                    PermissionResult::Ask { title, message }
                }
            };
        }

        // 默认允许
        PermissionResult::Allow
    }

    /// 检查并自动处理 Ask（如果有确认回调）
    pub fn check_and_confirm(&self, path: &str, action: Action) -> PermissionResult {
        match self.check(path, action) {
            PermissionResult::Ask { title, message } => {
                let handler = self.confirm_handler.read().unwrap();
                if let Some(ref handler) = *handler {
                    if handler(&title, &message) {
                        return PermissionResult::Allow;
                    } else {
                        return PermissionResult::Deny("用户拒绝".into());
                    }
                }
                PermissionResult::Ask { title, message }
            }
            other => other,
        }
    }

    /// 获取所有规则
    pub fn get_rules(&self) -> Vec<PermissionRule> {
        self.rules.read().unwrap().clone()
    }

    /// 清除所有规则
    pub fn clear(&self) {
        self.rules.write().unwrap().clear();
    }
}

impl Default for PermissionEngine {
    fn default() -> Self {
        Self::new()
    }
}

/// 简单的 glob 匹配 (支持 ** 和 *)
fn glob_match(pattern: &str, path: &str) -> bool {
    // 标准化路径
    let path = path.trim_start_matches("./");

    if pattern == "**" || pattern == "*" {
        return true;
    }

    // 把 glob 转成正则
    let mut regex = String::new();
    regex.push('^');
    for ch in pattern.chars() {
        match ch {
            '*' => {
                // 检查前面是否已经有一个 .*，避免重复
                if !regex.ends_with(".*") {
                    regex.push_str(".*");
                }
            }
            '?' => regex.push('.'),
            '.' | '+' | '(' | ')' | '|' | '^' | '$' | '\\' | '{' | '}' | '[' | ']' => {
                regex.push('\\');
                regex.push(ch);
            }
            _ => regex.push(ch),
        }
    }
    regex.push('$');

    // 简单匹配（不用正则库，直接用字符串匹配模拟）
    // 对于简单模式如 "**/.env*" → 以 ".env" 开头的文件名
    if pattern.contains("**/") {
        let suffix = pattern.split("**/").last().unwrap_or("");
        if suffix.ends_with('*') {
            let prefix = suffix.trim_end_matches('*');
            return path.ends_with(prefix) || path.contains(prefix);
        }
        return path.ends_with(suffix);
    }

    if pattern.starts_with("*.") {
        let ext = pattern.trim_start_matches("*.");
        return path.ends_with(&format!(".{ext}"));
    }

    if pattern.ends_with("/**") {
        let prefix = pattern.trim_end_matches("/**");
        return path.starts_with(prefix);
    }

    // 精确匹配
    path == pattern
}

// ---------------------------------------------------------------------------
// UiSystem — UI 事件系统
// ---------------------------------------------------------------------------

/// UI 事件级别
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiLevel {
    Info,
    Success,
    Warning,
    Error,
}

/// UI 事件（推送给前端/UI）
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UiEvent {
    pub event_type: String,
    pub title: String,
    pub message: String,
    pub level: UiLevel,
    pub data: Option<serde_json::Value>,
}

impl UiEvent {
    pub fn info(title: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            event_type: "notify".into(),
            title: title.into(),
            message: message.into(),
            level: UiLevel::Info,
            data: None,
        }
    }

    pub fn success(title: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            event_type: "notify".into(),
            title: title.into(),
            message: message.into(),
            level: UiLevel::Success,
            data: None,
        }
    }

    pub fn warning(title: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            event_type: "notify".into(),
            title: title.into(),
            message: message.into(),
            level: UiLevel::Warning,
            data: None,
        }
    }

    pub fn error(title: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            event_type: "notify".into(),
            title: title.into(),
            message: message.into(),
            level: UiLevel::Error,
            data: None,
        }
    }
}

/// UI 事件系统（内核暴露给插件）
pub struct UiSystem {
    /// 事件订阅者
    subscribers: Arc<RwLock<Vec<tokio::sync::mpsc::Sender<UiEvent>>>>,
    /// 确认回调（UI 层设置，插件调用）
    #[allow(clippy::type_complexity)]
    confirm_handler: Arc<RwLock<Option<Box<dyn Fn(&str, &str) -> bool + Send + Sync>>>>,
}

impl UiSystem {
    pub fn new() -> Self {
        Self {
            subscribers: Arc::new(RwLock::new(Vec::new())),
            confirm_handler: Arc::new(RwLock::new(None)),
        }
    }

    /// 推送事件给所有订阅者
    pub fn emit(&self, event: UiEvent) {
        let subs = self.subscribers.read().unwrap();
        for tx in subs.iter() {
            let _ = tx.try_send(event.clone());
        }
    }

    /// 快捷方法
    pub fn notify(&self, title: &str, message: &str, level: UiLevel) {
        self.emit(UiEvent {
            event_type: "notify".into(),
            title: title.into(),
            message: message.into(),
            level,
            data: None,
        });
    }

    /// 请求用户确认（同步，阻塞等待 UI 回复）
    pub fn confirm(&self, title: &str, message: &str) -> bool {
        let handler = self.confirm_handler.read().unwrap();
        if let Some(ref handler) = *handler {
            return handler(title, message);
        }
        // 没有UI时默认允许
        true
    }

    /// 设置确认回调
    pub fn set_confirm_handler(&self, handler: Box<dyn Fn(&str, &str) -> bool + Send + Sync>) {
        *self.confirm_handler.write().unwrap() = Some(handler);
    }

    /// 是否已设置确认回调
    pub fn has_confirm_handler(&self) -> bool {
        self.confirm_handler.read().unwrap().is_some()
    }

    /// 订阅 UI 事件（UI 层调这个）
    pub fn subscribe(&self) -> tokio::sync::mpsc::Receiver<UiEvent> {
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        self.subscribers.write().unwrap().push(tx);
        rx
    }
}

impl Default for UiSystem {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// SecurityProfile — 统一安全配置模式
// ---------------------------------------------------------------------------

/// 安全配置模式 — 一键切换整套权限策略
///
/// 这个枚举是安全配置的总入口，控制：
/// - PermissionEngine 规则（哪些文件能读/写）
/// - CommandGuard 白名单 + 风险检测（哪些命令能执行）
///
/// config.json 中配置:
/// ```json
/// { "security": { "mode": "standard" } }
/// ```
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SecurityProfile {
    /// 🔓 完全放开：无权限检查，无命令守卫
    Permissive,
    /// 👁 只读模式：所有写/编辑/删除操作被拒绝，只能读和执行
    ReadOnly,
    /// 🔒 标准模式：保护敏感文件 + 高危命令拦截（默认）
    Standard,
    /// 🔒🔒 严格模式：默认拒绝所有写操作，需显式白名单放行
    Strict,
}

impl SecurityProfile {
    /// 一键配置 PermissionEngine + CommandGuard
    pub fn setup(&self, engine: &PermissionEngine, guard: &mut crate::command_guard::CommandGuard) {
        match self {
            SecurityProfile::Permissive => {
                engine.clear();
                guard.whitelist.clear();
                guard.risk_patterns.clear();
                guard.add_whitelist("*");
            }
            SecurityProfile::ReadOnly => {
                engine.clear();
                // 拒绝所有写、编辑、删除
                engine.register_rule(PermissionRule {
                    name: "readonly-deny-write".into(),
                    actions: vec![Action::Write, Action::Edit, Action::Delete],
                    pattern: "**".into(),
                    policy: PermissionPolicy::Deny,
                    priority: 100,
                });
                // 读和命令还是放行的，但高危命令依然拦截
                // CommandGuard 使用默认配置
            }
            SecurityProfile::Standard => {
                engine.clear();
                for rule in standard_rules() { engine.register_rule(rule); }
            }
            SecurityProfile::Strict => {
                engine.clear();
                for rule in strict_rules() { engine.register_rule(rule); }
            }
        }
    }
}

impl Default for SecurityProfile {
    fn default() -> Self { Self::Standard }
}

fn standard_rules() -> Vec<PermissionRule> {
    vec![
        PermissionRule { name: "protect-env".into(), actions: vec![Action::Read, Action::Write], pattern: "**/.env*".into(), policy: PermissionPolicy::Deny, priority: 100 },
        PermissionRule { name: "protect-ssh".into(), actions: vec![Action::Read, Action::Write], pattern: "**/.ssh/**".into(), policy: PermissionPolicy::Deny, priority: 100 },
        PermissionRule { name: "protect-aws".into(), actions: vec![Action::Read, Action::Write], pattern: "**/.aws/**".into(), policy: PermissionPolicy::Deny, priority: 100 },
        PermissionRule { name: "protect-git-config".into(), actions: vec![Action::Write], pattern: "**/.git/config".into(), policy: PermissionPolicy::Deny, priority: 100 },
        PermissionRule { name: "protect-ion-config".into(), actions: vec![Action::Write], pattern: "**/.ion/**".into(), policy: PermissionPolicy::Deny, priority: 100 },
    ]
}

fn strict_rules() -> Vec<PermissionRule> {
    let mut rules = standard_rules();
    rules.push(PermissionRule { name: "default-deny-write".into(), actions: vec![Action::Write, Action::Edit, Action::Delete], pattern: "**".into(), policy: PermissionPolicy::Deny, priority: 1 });
    rules
}

// ---------------------------------------------------------------------------
// CommandHook — 声明式命令钩子
// ---------------------------------------------------------------------------

/// 命令钩子配置（JSON 声明式）
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommandHook {
    /// 执行的命令
    pub command: String,
    /// 条件表达式 (e.g. "tool === 'edit'")
    #[serde(default)]
    pub condition: Option<String>,
    /// 异步执行（不阻塞 Agent 循环）
    #[serde(default)]
    pub r#async: bool,
    /// 只执行一次
    #[serde(default)]
    pub once: bool,
    /// 超时（毫秒，默认 30000）
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
}

fn default_timeout() -> u64 {
    30000
}

/// 执行命令钩子
pub async fn execute_command_hook(hook: &CommandHook, context: &HashMap<String, String>) -> Result<String, String> {
    // 简单条件检查（支持 "tool === 'edit'" 格式）
    if let Some(ref cond) = hook.condition {
        if !evaluate_condition(cond, context) {
            return Ok(String::new()); // 条件不满足，跳过
        }
    }

    // 模板替换 {{variable}}
    let mut cmd = hook.command.clone();
    for (key, value) in context {
        cmd = cmd.replace(&format!("{{{{{key}}}}}"), value);
    }

    tracing::info!("[hook] 执行: {cmd}");

    let output = tokio::time::timeout(
        std::time::Duration::from_millis(hook.timeout_ms),
        tokio::process::Command::new("sh").args(["-c", &cmd]).output(),
    )
    .await
    .map_err(|_| format!("命令超时: {cmd}"))?
    .map_err(|e| format!("执行失败: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if output.status.success() {
        if !stderr.is_empty() {
            tracing::debug!("[hook] stderr: {stderr}");
        }
        Ok(stdout)
    } else {
        Err(format!("退出码 {:?}: {stderr}", output.status.code()))
    }
}

/// 简单条件评估 (e.g. "tool === 'edit'", "tool === 'edit' || tool === 'write'")
fn evaluate_condition(expr: &str, context: &HashMap<String, String>) -> bool {
    let expr = expr.trim();

    // 支持 || (OR)
    if expr.contains("||") {
        return expr.split("||").any(|part| evaluate_condition(part.trim(), context));
    }

    // 支持 && (AND)
    if expr.contains("&&") {
        return expr.split("&&").all(|part| evaluate_condition(part.trim(), context));
    }

    // 解析 "key === 'value'" 或 "key === value"
    if let Some((left, right)) = expr.split_once("===") {
        let left = left.trim();
        let right = right.trim().trim_matches('\'').trim_matches('"');

        // 从 context 拿值
        let actual = context.get(left).map(|s| s.as_str()).unwrap_or(left);
        return actual == right;
    }

    // 解析 "key !== 'value'"
    if let Some((left, right)) = expr.split_once("!==") {
        let left = left.trim();
        let right = right.trim().trim_matches('\'').trim_matches('"');
        let actual = context.get(left).map(|s| s.as_str()).unwrap_or(left);
        return actual != right;
    }

    // 默认 true
    true
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_allow_by_default() {
        let engine = PermissionEngine::new();
        match engine.check("/tmp/test.txt", Action::Read) {
            PermissionResult::Allow => {}
            other => panic!("expected Allow, got {other:?}"),
        }
    }

    #[test]
    fn permission_deny_sensitive() {
        let engine = PermissionEngine::new();
        engine.register_rule(PermissionRule {
            name: "no-env".into(),
            actions: vec![Action::Read, Action::Write],
            pattern: "**/.env*".into(),
            policy: PermissionPolicy::Deny,
            priority: 100,
        });

        match engine.check("/app/.env", Action::Read) {
            PermissionResult::Deny(_) => {}
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn permission_priority_order() {
        let engine = PermissionEngine::new();
        engine.register_rule(PermissionRule {
            name: "deny-all".into(),
            actions: vec![Action::Read],
            pattern: "**".into(),
            policy: PermissionPolicy::Deny,
            priority: 1,
        });
        engine.register_rule(PermissionRule {
            name: "allow-tmp".into(),
            actions: vec![Action::Read],
            pattern: "/tmp/**".into(),
            policy: PermissionPolicy::Allow,
            priority: 100,
        });

        // /tmp 应该 Allow (高优先级)
        match engine.check("/tmp/test.txt", Action::Read) {
            PermissionResult::Allow => {}
            other => panic!("expected Allow for /tmp, got {other:?}"),
        }

        // /app 应该 Deny (低优先级默认规则)
        match engine.check("/app/test.txt", Action::Read) {
            PermissionResult::Deny(_) => {}
            other => panic!("expected Deny for /app, got {other:?}"),
        }
    }

    #[test]
    fn glob_match_patterns() {
        assert!(glob_match("**/.env*", "/app/.env"));
        assert!(glob_match("**/.env*", "/app/.env.production"));
        assert!(!glob_match("**/.env*", "/app/config.txt"));

        assert!(glob_match("/tmp/**", "/tmp/test.txt"));
        assert!(!glob_match("/tmp/**", "/app/test.txt"));

        assert!(glob_match("**", "/anywhere/file.txt"));
    }

    #[test]
    fn condition_evaluation() {
        let mut ctx = HashMap::new();
        ctx.insert("tool".into(), "edit".into());

        assert!(evaluate_condition("tool === 'edit'", &ctx));
        assert!(!evaluate_condition("tool === 'write'", &ctx));
        assert!(evaluate_condition("tool === 'edit' || tool === 'write'", &ctx));
        assert!(evaluate_condition("tool !== 'bash'", &ctx));
    }

    #[test]
    fn ui_event_helpers() {
        let ui = UiSystem::new();
        let mut rx = ui.subscribe();
        ui.notify("test", "hello", UiLevel::Info);
        let event = rx.try_recv().unwrap();
        assert_eq!(event.title, "test");
        assert_eq!(event.message, "hello");
    }

    #[test]
    fn ui_confirm_no_handler() {
        let ui = UiSystem::new();
        // 没有设置 handler，默认 true
        assert!(ui.confirm("title", "message"));
    }

    #[tokio::test]
    async fn command_hook_execution() {
        let hook = CommandHook {
            command: "echo hello".into(),
            condition: None,
            r#async: false,
            once: false,
            timeout_ms: 5000,
        };
        let result = execute_command_hook(&hook, &HashMap::new()).await.unwrap();
        assert!(result.contains("hello"));
    }

    #[tokio::test]
    async fn command_hook_with_condition() {
        let hook = CommandHook {
            command: "echo skip".into(),
            condition: Some("tool === 'bash'".into()),
            r#async: false,
            once: false,
            timeout_ms: 5000,
        };
        let mut ctx = HashMap::new();
        ctx.insert("tool".into(), "read".into());
        let result = execute_command_hook(&hook, &ctx).await.unwrap();
        assert!(result.is_empty()); // 条件不满足，跳过
    }
}
