//! PermissionExtension — 权限策略层
//!
//! 在内核 PermissionEngine 之上提供策略层能力：
//! - 从 settings.json 加载规则（全局 + 项目，8 层配置源）
//! - 会话级规则（动态添加，不持久化）
//! - 项目级规则（持久化到 settings.json）
//! - 通过 `on_extension_rpc` 提供 CLI 管理接口
//!
//! Subject 命名对齐 pi:
//!   "command.run"  — bash 命令
//!   "file.read"    — 文件读取
//!   "file.write"   — 文件写入
//!   "file.delete"  — 文件删除
//!
//! Scope 对齐 pi:
//!   "session"  — 当前会话，不持久化
//!   "project"  — 项目级，持久化到 settings.json

use super::error::AgentError;
use super::error::AgentResult;
use super::extension::Extension;
use super::messages::ToolCall;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Mutex, RwLock};

/// 一条权限规则（对齐 pi PermissionRule 格式）
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PermissionRule {
    /// 规则唯一 ID
    #[serde(default)]
    pub id: String,
    /// 来源 provider（"user" | "extension_rpc" | "config"）
    #[serde(default)]
    pub provider: String,
    /// 主体（"command.run" | "file.read" | "file.write" | "file.delete"）
    pub subject: String,
    /// 匹配模式（glob，如 "npm *"、"**/.env*"）
    pub pattern: String,
    /// 决策
    pub decision: Decision,
    /// 作用域
    pub scope: Scope,
    /// 创建时间（ISO 8601）
    #[serde(default)]
    pub created_at: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum Decision {
    Allow,
    Deny,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum Scope {
    Session, // 当前会话，不持久化
    Project, // 项目级，持久化到 settings.json
}

/// 工具名 → subject 映射
fn tool_to_subject(tool: &str) -> &str {
    match tool {
        "bash" | "bash_run" | "bash_kill" | "bash_send" => "command.run",
        "read" | "grep" | "find" | "ls" => "file.read",
        "write" | "edit" => "file.write",
        "remove_file" => "file.delete",
        _ if tool.starts_with("mcp__") => "mcp_tool",
        _ => "",
    }
}

/// PermissionExtension — 权限策略扩展
pub struct PermissionExtension {
    /// 项目级规则（持久化）
    project_rules: RwLock<Vec<PermissionRule>>,
    /// 会话级规则（内存）
    session_rules: Mutex<Vec<PermissionRule>>,
    /// 全局 settings.json 路径
    global_path: PathBuf,
    /// 项目 settings.json 路径
    project_settings_path: PathBuf,
    /// Extension 名
    name: String,
}

impl PermissionExtension {
    pub fn new(_session_id: &str, project_root: &str) -> Self {
        let project_settings = PathBuf::from(project_root).join(".ion").join("settings.json");
        let global_path = crate::paths::settings_path();

        let ext = Self {
            project_rules: RwLock::new(Vec::new()),
            session_rules: Mutex::new(Vec::new()),
            global_path,
            project_settings_path: project_settings,
            name: "permission".into(),
        };
        ext.reload_internal();
        ext
    }

    /// 重新加载规则（热重载）：重新读取全局 + 项目 settings.json
    /// 不修改会话级规则，不清空。
    pub fn reload(&self) -> Result<String, String> {
        let (count_global, count_project) = self.reload_internal();
        Ok(format!("reloaded: {} global rules, {} project rules", count_global, count_project))
    }

    /// 内部重载实现
    fn reload_internal(&self) -> (usize, usize) {
        let mut new_rules: Vec<PermissionRule> = Vec::new();

        // 1. 全局配置 ~/.ion/settings.json → permissions.rules
        let mut global_count = 0;
        if let Some(rules) = Self::load_rules_from_file(&Some(self.global_path.clone())) {
            global_count = rules.len();
            new_rules.extend(rules);
        }

        // 2. 项目配置 <project>/.ion/settings.json → permissions.rules（覆盖同名规则）
        let mut project_count = 0;
        if let Some(rules) = Self::load_rules_from_file(&Some(self.project_settings_path.clone())) {
            project_count = rules.len();
            new_rules.extend(rules);
        }

        if let Ok(mut rules) = self.project_rules.write() {
            *rules = new_rules;
        }

        (global_count, project_count)
    }

    /// 从 settings.json 加载 rules
    fn load_rules_from_file(path: &Option<PathBuf>) -> Option<Vec<PermissionRule>> {
        let path = path.as_ref()?;
        let content = std::fs::read_to_string(path).ok()?;
        let json: serde_json::Value = serde_json::from_str(&content).ok()?;
        let rules = json.get("permissions")?.get("rules")?;
        serde_json::from_value(rules.clone()).ok()
    }

    /// 持久化项目级规则到 settings.json
    fn save_project_rules(&self) {
        if let Ok(rules) = self.project_rules.read() {
            if let Ok(json) = serde_json::to_string_pretty(&serde_json::json!({
                "permissions": { "rules": &*rules }
            })) {
                if let Some(parent) = self.project_settings_path.parent() {
                    std::fs::create_dir_all(parent).ok();
                }
                std::fs::write(&self.project_settings_path, json).ok();
            }
        }
    }

    /// 检查工具调用是否匹配规则
    fn check_tool(&self, call: &ToolCall) -> Option<Decision> {
        let subject = tool_to_subject(&call.name);
        if subject.is_empty() {
            return None; // 不认识的工具，放行
        }

        // MCP 工具：pattern 匹配工具名（mcp__server__tool）
        // 其他工具：pattern 匹配路径/命令
        let match_value = if subject == "mcp_tool" {
            call.name.as_str()
        } else {
            call.arguments.get("path")
                .or_else(|| call.arguments.get("command"))
                .or_else(|| call.arguments.get("file_path"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
        };

        // 1. 先查会话级规则（优先级最高）
        if let Ok(rules) = self.session_rules.lock() {
            for rule in rules.iter() {
                if Self::matches(rule, subject, match_value) {
                    return Some(rule.decision.clone());
                }
            }
        }

        // 2. 再查项目级规则
        if let Ok(rules) = self.project_rules.read() {
            for rule in rules.iter() {
                if Self::matches(rule, subject, match_value) {
                    return Some(rule.decision.clone());
                }
            }
        }

        None
    }

    fn matches(rule: &PermissionRule, subject: &str, path: &str) -> bool {
        // subject 匹配
        if rule.subject != "*" && rule.subject != subject {
            return false;
        }
        // pattern 匹配
        if rule.pattern.is_empty() {
            return true;
        }
        if rule.pattern == "*" {
            return true;
        }
        // 前缀匹配
        if rule.pattern.ends_with('*') {
            let prefix = rule.pattern.trim_end_matches('*');
            return path.starts_with(prefix);
        }
        // 后缀匹配
        if rule.pattern.starts_with('*') {
            let suffix = rule.pattern.trim_start_matches('*');
            return path.ends_with(suffix);
        }
        // 精确匹配
        rule.pattern == path
    }

    /// 添加一条规则
    pub fn add_rule(&self, subject: &str, pattern: &str, decision: &str, scope: &str) -> AgentResult<String> {
        let decision = match decision {
            "allow" => Decision::Allow,
            "deny" => Decision::Deny,
            _ => return Err(AgentError::Tool("decision must be 'allow' or 'deny'".into())),
        };
        let scope = match scope {
            "session" => Scope::Session,
            "project" => Scope::Project,
            _ => return Err(AgentError::Tool("scope must be 'session' or 'project'".into())),
        };

        let rule = PermissionRule {
            id: format!("perm_{}", &uuid::Uuid::new_v4().to_string()[..8]),
            provider: "extension_rpc".into(),
            subject: subject.to_string(),
            pattern: pattern.to_string(),
            decision: decision.clone(),
            scope: scope.clone(),
            created_at: chrono_now(),
        };

        match scope {
            Scope::Session => {
                self.session_rules.lock().unwrap().push(rule);
            }
            Scope::Project => {
                self.project_rules.write().unwrap().push(rule);
                self.save_project_rules();
            }
        }

        Ok(format!("rule added: {} {} {} {}", subject, pattern, decision_str(&decision), scope_str(&scope)))
    }

    /// 列出所有规则
    pub fn list_rules(&self) -> Vec<serde_json::Value> {
        let mut rules: Vec<serde_json::Value> = Vec::new();
        if let Ok(session) = self.session_rules.lock() {
            for r in session.iter() {
                rules.push(rule_to_json(r, "session"));
            }
        }
        if let Ok(project) = self.project_rules.read() {
            for r in project.iter() {
                rules.push(rule_to_json(r, "project"));
            }
        }
        rules
    }
}

fn rule_to_json(r: &PermissionRule, scope_label: &str) -> serde_json::Value {
    serde_json::json!({
        "id": &r.id, "subject": &r.subject, "pattern": &r.pattern,
        "decision": decision_str(&r.decision), "scope": scope_label,
        "provider": &r.provider,
    })
}

fn decision_str(d: &Decision) -> &str {
    match d { Decision::Allow => "allow", Decision::Deny => "deny" }
}

fn scope_str(s: &Scope) -> &str {
    match s { Scope::Session => "session", Scope::Project => "project" }
}

fn chrono_now() -> String {
    // Simple ISO-like timestamp without chrono dependency
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    format!("2026-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        (secs / 2592000 % 12) + 1,  // approximate month
        (secs / 86400 % 30) + 1,    // approximate day
        (secs / 3600 % 24),
        (secs / 60 % 60),
        (secs % 60))
}

#[async_trait]
impl Extension for PermissionExtension {
    fn name(&self) -> &str { &self.name }

    async fn before_tool_call(&self, call: &ToolCall) -> AgentResult<()> {
        if let Some(decision) = self.check_tool(call) {
            match decision {
                Decision::Allow => return Ok(()),
                Decision::Deny => {
                    return Err(AgentError::Tool(format!(
                        "[Permission] '{}' denied by extension rule", call.name
                    )));
                }
            }
        }
        Ok(())
    }

    async fn on_extension_rpc(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> AgentResult<serde_json::Value> {
        match method {
            "add_rule" => {
                let subject = params.get("subject").and_then(|v| v.as_str()).unwrap_or("command.run");
                let pattern = params.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
                let decision = params.get("decision").and_then(|v| v.as_str()).unwrap_or("allow");
                let scope = params.get("scope").and_then(|v| v.as_str()).unwrap_or("session");
                let msg = self.add_rule(subject, pattern, decision, scope)?;
                Ok(serde_json::json!({"status": "ok", "message": msg}))
            }
            "list_rules" => {
                let rules = self.list_rules();
                Ok(serde_json::json!({"rules": rules, "count": rules.len()}))
            }
            "reload" => {
                let msg = self.reload().map_err(|e| AgentError::Tool(e))?;
                Ok(serde_json::json!({"status": "ok", "message": msg}))
            }
            _ => Err(AgentError::Tool(format!("permission: unknown method '{method}'"))),
        }
    }
}
