//! PermissionExtension — 权限策略层
//!
//! 在内核 PermissionEngine 之上提供策略层能力：
//! - 预配置规则（从 config.json 加载）
//! - "本次允许"（内存，当前会话）
//! - "全局允许"（持久化到文件）
//! - 通过 `on_extension_rpc` 提供 CLI 管理接口
//!
//! ## 架构
//!
//! ```text
//! before_tool_call 钩子
//!   │
//!   ├── 先查 PermissionExtension 自有规则表
//!   │   ├── 匹配 Allow → Ok(()) (跳过 SecuredRuntime)
//!   │   ├── 匹配 Deny  → Err("denied")
//!   │   └── 不匹配    → 放行，让 SecuredRuntime 处理
//!   │
//!   └── SecuredRuntime 处理（PermissionEngine.check + UI Ask）
//!       └── 用户通过 Ask 允许后，可通过 CLI 添加"记住"规则
//! ```

use super::error::AgentResult;
use super::extension::Extension;
use super::messages::ToolCall;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

/// 一条权限规则
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PermissionRule {
    /// 规则名（描述）
    pub name: String,
    /// 匹配的工具名（"read", "write", "bash" 等，空=所有）
    pub tool: String,
    /// 匹配的路径模式（glob, 空=所有路径）
    pub pattern: String,
    /// 决策
    pub decision: Decision,
    /// 作用域
    pub scope: Scope,
    /// 创建时间戳
    pub created_at: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum Decision {
    Allow,
    Deny,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum Scope {
    Once,    // 仅本次（内存，不持久化）
    Session, // 本会话（不持久化到磁盘）
    Global,  // 全局（持久化到文件）
}

/// PermissionExtension — 权限策略扩展
///
/// 在 `before_tool_call` 钩子中检查规则表，匹配则直接 Allow/Deny。
pub struct PermissionExtension {
    /// 持久化规则表（全局 + 会话）
    persistent_rules: RwLock<Vec<PermissionRule>>,
    /// 内存规则（Once 作用域）
    once_rules: Mutex<Vec<PermissionRule>>,
    /// 规则文件路径
    rules_path: PathBuf,
    /// Extension 名
    name: String,
}

impl PermissionExtension {
    pub fn new(session_id: &str, data_dir: &str) -> Self {
        let rules_dir = PathBuf::from(data_dir).join("permission");
        std::fs::create_dir_all(&rules_dir).ok();
        let rules_path = rules_dir.join(format!("rules_{}.json", session_id));

        let persistent_rules = Self::load_rules(&rules_path);

        Self {
            persistent_rules: RwLock::new(persistent_rules),
            once_rules: Mutex::new(Vec::new()),
            rules_path,
            name: "permission".into(),
        }
    }

    fn load_rules(path: &PathBuf) -> Vec<PermissionRule> {
        match std::fs::read_to_string(path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }

    fn save_rules(&self) {
        if let Ok(rules) = self.persistent_rules.read() {
            if let Ok(json) = serde_json::to_string_pretty(&*rules) {
                std::fs::write(&self.rules_path, json).ok();
            }
        }
    }

    /// 检查工具调用是否匹配规则
    fn check_tool(&self, call: &ToolCall) -> Option<Decision> {
        let tool_name = &call.name;
        let path = call.arguments.get("path")
            .or_else(|| call.arguments.get("command"))
            .and_then(|v| v.as_str())
            .unwrap_or("");

        // 1. 先查 once 规则（内存，优先级最高）
        if let Ok(rules) = self.once_rules.lock() {
            for rule in rules.iter() {
                if Self::matches(rule, tool_name, path) {
                    return Some(rule.decision.clone());
                }
            }
        }

        // 2. 再查持久化规则
        if let Ok(rules) = self.persistent_rules.read() {
            for rule in rules.iter() {
                if Self::matches(rule, tool_name, path) {
                    return Some(rule.decision.clone());
                }
            }
        }

        None
    }

    fn matches(rule: &PermissionRule, tool: &str, path: &str) -> bool {
        if !rule.tool.is_empty() && rule.tool != tool {
            return false;
        }
        if !rule.pattern.is_empty() {
            // simple glob matching: * matches anything, otherwise prefix/suffix/exact
            if rule.pattern == "*" {
                return true;
            }
            if rule.pattern.ends_with('*') {
                let prefix = rule.pattern.trim_end_matches('*');
                if !path.starts_with(prefix) {
                    return false;
                }
            } else if rule.pattern.starts_with('*') {
                let suffix = rule.pattern.trim_start_matches('*');
                if !path.ends_with(suffix) {
                    return false;
                }
            } else if rule.pattern != path {
                return false;
            }
        }
        true
    }

    /// 添加一条规则（通过 extension_rpc 调用）
    pub fn add_rule(&self, tool: &str, pattern: &str, decision: &str, scope: &str) -> AgentResult<String> {
        let decision = match decision {
            "allow" => Decision::Allow,
            "deny" => Decision::Deny,
            _ => return Err(super::error::AgentError::Tool("decision must be 'allow' or 'deny'".into())),
        };
        let scope = match scope {
            "once" => Scope::Once,
            "session" => Scope::Session,
            "global" => Scope::Global,
            _ => return Err(super::error::AgentError::Tool("scope must be 'once', 'session', or 'global'".into())),
        };

        let rule = PermissionRule {
            name: format!("rule_{}", &uuid::Uuid::new_v4().to_string()[..8]),
            tool: tool.to_string(),
            pattern: pattern.to_string(),
            decision: decision.clone(),
            scope: scope.clone(),
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64,
        };

        match scope {
            Scope::Once => {
                self.once_rules.lock().unwrap().push(rule);
            }
            Scope::Session | Scope::Global => {
                self.persistent_rules.write().unwrap().push(rule);
                self.save_rules();
            }
        }

        Ok(format!("rule added: {} {} {} {}", tool, pattern, decision_str(&decision), scope_str(&scope)))
    }

    /// 列出所有规则（通过 extension_rpc 调用）
    pub fn list_rules(&self) -> Vec<serde_json::Value> {
        let mut rules: Vec<serde_json::Value> = Vec::new();
        if let Ok(once) = self.once_rules.lock() {
            for r in once.iter() {
                rules.push(serde_json::json!({
                    "name": &r.name, "tool": &r.tool, "pattern": &r.pattern,
                    "decision": decision_str(&r.decision), "scope": "once"
                }));
            }
        }
        if let Ok(persistent) = self.persistent_rules.read() {
            for r in persistent.iter() {
                rules.push(serde_json::json!({
                    "name": &r.name, "tool": &r.tool, "pattern": &r.pattern,
                    "decision": decision_str(&r.decision), "scope": scope_str(&r.scope)
                }));
            }
        }
        rules
    }
}

fn decision_str(d: &Decision) -> &str {
    match d { Decision::Allow => "allow", Decision::Deny => "deny" }
}

fn scope_str(s: &Scope) -> &str {
    match s { Scope::Once => "once", Scope::Session => "session", Scope::Global => "global" }
}

#[async_trait]
impl Extension for PermissionExtension {
    fn name(&self) -> &str {
        &self.name
    }

    /// 在工具执行前检查权限规则
    async fn before_tool_call(&self, call: &ToolCall) -> AgentResult<()> {
        if let Some(decision) = self.check_tool(call) {
            match decision {
                Decision::Allow => return Ok(()),
                Decision::Deny => {
                    return Err(super::error::AgentError::Tool(format!(
                        "[Permission] '{}' denied by extension rule", call.name
                    )));
                }
            }
        }
        Ok(()) // 无匹配规则 → 放行给 SecuredRuntime
    }

    /// CLI 管理接口
    async fn on_extension_rpc(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> AgentResult<serde_json::Value> {
        match method {
            "add_rule" => {
                let tool = params.get("tool").and_then(|v| v.as_str()).unwrap_or("");
                let pattern = params.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
                let decision = params.get("decision").and_then(|v| v.as_str()).unwrap_or("allow");
                let scope = params.get("scope").and_then(|v| v.as_str()).unwrap_or("once");
                let msg = self.add_rule(tool, pattern, decision, scope)?;
                Ok(serde_json::json!({"status": "ok", "message": msg}))
            }
            "list_rules" => {
                let rules = self.list_rules();
                Ok(serde_json::json!({"rules": rules, "count": rules.len()}))
            }
            _ => Err(super::error::AgentError::Tool(format!("permission: unknown method '{method}'"))),
        }
    }
}
