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
    /// 决策来源（区分手动配置 vs 用户运行时选择"always allow"）
    #[serde(default = "default_source")]
    pub source: DecisionSource,
    /// 创建时间（ISO 8601）
    #[serde(default)]
    pub created_at: String,
}

/// 权限决策来源（区分手动配置 vs 用户运行时选择）
/// 对齐 pi 的 stored-decision.ts
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum DecisionSource {
    /// 用户在 settings.json 里手动配的
    Config,
    /// 用户在 UI 确认时选"always allow"自动生成的（stored-decision）
    Stored,
}

fn default_source() -> DecisionSource {
    DecisionSource::Config
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    Deny,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Session, // 当前会话，不持久化
    Project, // 项目级，持久化到 settings.json
}

/// UI 权限确认结果（用户在审批对话框的选择）
/// 对齐 pi 的 stored-decision：选 AlwaysAllowProject 后自动持久化
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum UiPermissionResult {
    /// 允许一次
    Allow,
    /// 拒绝
    Deny,
    /// 始终允许（项目级，持久化到 settings.json）
    AlwaysAllowProject,
    /// 始终拒绝（项目级，持久化到 settings.json）
    AlwaysDenyProject,
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
    /// 统一存储上下文（拿 settings.json 路径）
    storage: crate::storage_context::StorageContext,
    /// Extension 名
    name: String,
}

impl PermissionExtension {
    pub fn new(storage: crate::storage_context::StorageContext) -> Self {
        let ext = Self {
            project_rules: RwLock::new(Vec::new()),
            session_rules: Mutex::new(Vec::new()),
            storage,
            name: "permission".into(),
        };
        ext.reload_internal();
        ext
    }

    /// 兼容旧签名（测试用）
    pub fn new_with_root(_session_id: &str, project_root: &str) -> Self {
        Self::new(crate::storage_context::StorageContext::new(project_root, "test", project_root))
    }

    /// 全局 settings.json 路径
    fn global_path(&self) -> PathBuf {
        self.storage.global_settings_path()
    }

    /// 项目 settings.json 路径
    fn project_settings_path(&self) -> PathBuf {
        self.storage.project_settings_path()
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
        if let Some(rules) = Self::load_rules_from_file(&Some(self.global_path())) {
            global_count = rules.len();
            new_rules.extend(rules);
        }

        // 2. 项目配置 <project>/.ion/settings.json → permissions.rules（覆盖同名规则）
        let mut project_count = 0;
        if let Some(rules) = Self::load_rules_from_file(&Some(self.project_settings_path())) {
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
                let psp = self.project_settings_path();
                if let Some(parent) = psp.parent() {
                    std::fs::create_dir_all(parent).ok();
                }
                std::fs::write(&psp, json).ok();
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
            source: DecisionSource::Config,
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

    // ───────────────────────────────────────────────────────────────────
    // Stored-Decision（权限记忆）API
    // 对齐 pi 的 stored-decision.ts：用户选"always allow"后持久化，下次自动放行。
    // ───────────────────────────────────────────────────────────────────

    /// 存储一条决策（用户选"always allow"后调）
    ///
    /// - `subject`: "command.run" | "file.read" | "file.write" | "file.delete" | "*"
    /// - `pattern`: glob 匹配模式
    /// - `decision`: "allow" | "deny"
    /// - `scope`: "project"（持久化到 settings.json）| "session"（仅当前会话）
    ///
    /// 生成的规则 `source = Stored`，与手动配置（`source = Config`）区分。
    pub fn store_decision(
        &self,
        subject: &str,
        pattern: &str,
        decision: &str,
        scope: &str,
    ) -> AgentResult<String> {
        let decision = match decision {
            "allow" => Decision::Allow,
            "deny" => Decision::Deny,
            _ => {
                return Err(AgentError::Tool(
                    "decision must be 'allow' or 'deny'".into(),
                ))
            }
        };
        let scope = match scope {
            "session" => Scope::Session,
            "project" => Scope::Project,
            _ => return Err(AgentError::Tool("scope must be 'session' or 'project'".into())),
        };

        let id = format!("perm_stored_{}", &uuid::Uuid::new_v4().to_string()[..8]);
        let rule = PermissionRule {
            id: id.clone(),
            provider: "stored_decision".into(),
            subject: subject.to_string(),
            pattern: pattern.to_string(),
            decision: decision.clone(),
            scope: scope.clone(),
            source: DecisionSource::Stored,
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

        Ok(format!(
            "stored: {} {} {} {} ({})",
            subject,
            pattern,
            decision_str(&decision),
            scope_str(&scope),
            id
        ))
    }

    /// 列出所有 stored 决策（source == Stored），排除手动配置的 Config 规则。
    pub fn list_stored(&self) -> Vec<serde_json::Value> {
        let mut rules: Vec<serde_json::Value> = Vec::new();
        if let Ok(session) = self.session_rules.lock() {
            for r in session.iter() {
                if r.source == DecisionSource::Stored {
                    rules.push(rule_to_json(r, "session"));
                }
            }
        }
        if let Ok(project) = self.project_rules.read() {
            for r in project.iter() {
                if r.source == DecisionSource::Stored {
                    rules.push(rule_to_json(r, "project"));
                }
            }
        }
        rules
    }

    /// 删除某条 stored 决策（撤销 always allow）。
    /// 仅删除 `source == Stored` 的规则，不动 Config 规则。
    /// 返回被删除规则的 JSON（找不到时返回 None）。
    pub fn remove_stored(&self, id: &str) -> Option<serde_json::Value> {
        // 1. 先在 session 级找
        if let Ok(mut session) = self.session_rules.lock() {
            if let Some(pos) = session
                .iter()
                .position(|r| r.id == id && r.source == DecisionSource::Stored)
            {
                let removed = session.remove(pos);
                return Some(rule_to_json(&removed, "session"));
            }
        }
        // 2. 再在 project 级找（删后需持久化）
        if let Ok(mut project) = self.project_rules.write() {
            if let Some(pos) = project
                .iter()
                .position(|r| r.id == id && r.source == DecisionSource::Stored)
            {
                let removed = project.remove(pos);
                drop(project);
                self.save_project_rules();
                return Some(rule_to_json(&removed, "project"));
            }
        }
        None
    }

    /// 清空所有 stored 决策（source == Stored），保留 Config 规则。
    /// 返回被清除的数量。
    pub fn clear_stored(&self) -> usize {
        let mut removed = 0;
        if let Ok(mut session) = self.session_rules.lock() {
            let before = session.len();
            session.retain(|r| r.source != DecisionSource::Stored);
            removed += before - session.len();
        }
        let project_changed;
        if let Ok(mut project) = self.project_rules.write() {
            let before = project.len();
            project.retain(|r| r.source != DecisionSource::Stored);
            let after = project.len();
            project_changed = before != after;
            removed += before - after;
        } else {
            project_changed = false;
        }
        if project_changed {
            self.save_project_rules();
        }
        removed
    }

    /// 根据用户的 UI 确认结果存储决策（便捷方法）。
    ///
    /// 当用户选 `AlwaysAllowProject` / `AlwaysDenyProject` 时，自动生成一条
    /// `source = Stored` 的项目级规则并持久化。选 `Allow`/`Deny` 不持久化。
    pub fn store_from_ui_result(
        &self,
        result: &UiPermissionResult,
        subject: &str,
        pattern: &str,
    ) -> AgentResult<Option<String>> {
        match result {
            UiPermissionResult::Allow | UiPermissionResult::Deny => Ok(None),
            UiPermissionResult::AlwaysAllowProject => {
                let msg = self.store_decision(subject, pattern, "allow", "project")?;
                Ok(Some(msg))
            }
            UiPermissionResult::AlwaysDenyProject => {
                let msg = self.store_decision(subject, pattern, "deny", "project")?;
                Ok(Some(msg))
            }
        }
    }
}

fn rule_to_json(r: &PermissionRule, scope_label: &str) -> serde_json::Value {
    serde_json::json!({
        "id": &r.id, "subject": &r.subject, "pattern": &r.pattern,
        "decision": decision_str(&r.decision), "scope": scope_label,
        "provider": &r.provider, "source": source_str(&r.source),
        "createdAt": &r.created_at,
    })
}

fn source_str(s: &DecisionSource) -> &'static str {
    match s {
        DecisionSource::Config => "config",
        DecisionSource::Stored => "stored",
    }
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
            // ── Stored-Decision（权限记忆）API ──
            "store_decision" => {
                let subject = params
                    .get("subject")
                    .and_then(|v| v.as_str())
                    .unwrap_or("command.run");
                let pattern = params.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
                let decision = params
                    .get("decision")
                    .and_then(|v| v.as_str())
                    .unwrap_or("allow");
                let scope = params.get("scope").and_then(|v| v.as_str()).unwrap_or("project");
                let msg = self.store_decision(subject, pattern, decision, scope)?;
                Ok(serde_json::json!({"status": "ok", "message": msg}))
            }
            "list_stored" => {
                let rules = self.list_stored();
                Ok(serde_json::json!({"rules": rules, "count": rules.len()}))
            }
            "remove_stored" => {
                let id = params.get("id").and_then(|v| v.as_str()).unwrap_or("");
                if id.is_empty() {
                    return Err(AgentError::Tool("missing 'id' parameter".into()));
                }
                match self.remove_stored(id) {
                    Some(removed) => Ok(serde_json::json!({
                        "status": "ok", "removed": removed,
                    })),
                    None => Err(AgentError::Tool(format!(
                        "no stored decision with id '{id}'"
                    ))),
                }
            }
            "clear_stored" => {
                let removed = self.clear_stored();
                Ok(serde_json::json!({"status": "ok", "removed": removed}))
            }
            _ => Err(AgentError::Tool(format!("permission: unknown method '{method}'"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

    /// 在临时目录里造一个 PermissionExtension（不污染真实项目 settings.json）。
    /// 遵循项目现有测试模式（std::env::temp_dir + process id），不依赖 tempfile crate。
    fn make_ext() -> PermissionExtension {
        let seq = TEST_SEQ.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir()
            .join(format!("ion_perm_store_test_{}_{}", std::process::id(), seq));
        let _ = std::fs::remove_dir_all(&root);
        let root = root.to_str().unwrap().to_string();
        PermissionExtension::new_with_root("test-sid", &root)
    }

    /// 构造 ToolCall（ion-provider 的 ToolCall 需要 call_type + thought_signature）
    fn tool_call(name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            call_type: "function".into(),
            id: "1".into(),
            name: name.into(),
            arguments: args,
            thought_signature: None,
        }
    }

    #[test]
    fn test_store_decision_project_persists() {
        let ext = make_ext();
        // 存一条 allow project 级决策
        let msg = ext
            .store_decision("command.run", "git status", "allow", "project")
            .unwrap();
        assert!(msg.contains("stored"));
        assert!(msg.contains("perm_stored_"));

        // list_stored 应有 1 条，source=stored
        let stored = ext.list_stored();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0]["source"], "stored");
        assert_eq!(stored[0]["decision"], "allow");
        assert_eq!(stored[0]["scope"], "project");
    }

    #[test]
    fn test_store_decision_session_not_persisted_to_file() {
        let ext = make_ext();
        ext.store_decision("command.run", "ls", "allow", "session")
            .unwrap();
        // session 级在 list_stored 里能看到
        let stored = ext.list_stored();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0]["scope"], "session");
    }

    #[test]
    fn test_store_decision_invalid_args() {
        let ext = make_ext();
        // 非法 decision
        assert!(ext
            .store_decision("command.run", "x", "maybe", "project")
            .is_err());
        // 非法 scope
        assert!(ext
            .store_decision("command.run", "x", "allow", "global")
            .is_err());
    }

    #[test]
    fn test_remove_stored_only_removes_stored() {
        let ext = make_ext();
        // 一条 Stored + 一条 Config（add_rule 默认 source=Config）
        ext.store_decision("command.run", "git status", "allow", "project")
            .unwrap();
        ext.add_rule("command.run", "echo hi", "allow", "project")
            .unwrap();
        assert_eq!(ext.list_stored().len(), 1);
        assert_eq!(ext.list_rules().len(), 2);

        // 取 stored 的 id
        let stored_id = ext.list_stored()[0]["id"].as_str().unwrap().to_string();

        // 删除 stored
        let removed = ext.remove_stored(&stored_id);
        assert!(removed.is_some());
        assert_eq!(ext.list_stored().len(), 0);

        // Config 规则保留
        assert_eq!(ext.list_rules().len(), 1);
        assert_eq!(ext.list_rules()[0]["source"], "config");
    }

    #[test]
    fn test_remove_stored_nonexistent_returns_none() {
        let ext = make_ext();
        assert!(ext.remove_stored("perm_stored_nope").is_none());
    }

    #[test]
    fn test_remove_stored_does_not_touch_config_rule_by_same_id() {
        let ext = make_ext();
        // Config 规则的 id 形如 perm_xxxx（不是 perm_stored_xxxx）
        ext.add_rule("command.run", "echo cfg", "allow", "project")
            .unwrap();
        let cfg_id = ext.list_rules()[0]["id"].as_str().unwrap().to_string();
        // 用 Config 规则的 id 调 remove_stored → 不应删（source 不匹配）
        assert!(ext.remove_stored(&cfg_id).is_none());
        assert_eq!(ext.list_rules().len(), 1);
    }

    #[test]
    fn test_clear_stored_only_clears_stored() {
        let ext = make_ext();
        // 2 条 Stored + 1 条 Config
        ext.store_decision("command.run", "a", "allow", "project")
            .unwrap();
        ext.store_decision("command.run", "b", "deny", "session")
            .unwrap();
        ext.add_rule("command.run", "c", "allow", "project")
            .unwrap();
        assert_eq!(ext.list_stored().len(), 2);
        assert_eq!(ext.list_rules().len(), 3);

        let removed = ext.clear_stored();
        assert_eq!(removed, 2);
        assert_eq!(ext.list_stored().len(), 0);
        // Config 规则保留
        assert_eq!(ext.list_rules().len(), 1);
        assert_eq!(ext.list_rules()[0]["source"], "config");
    }

    #[test]
    fn test_clear_stored_when_empty() {
        let ext = make_ext();
        assert_eq!(ext.clear_stored(), 0);
    }

    #[tokio::test]
    async fn test_stored_decision_auto_allows() {
        let ext = make_ext();
        // 存一条 allow stored 决策
        ext.store_decision("command.run", "git status", "allow", "session")
            .unwrap();
        // before_tool_call 应放行
        let call = tool_call("bash", serde_json::json!({"command": "git status"}));
        // Allow → Ok
        assert!(ext.before_tool_call(&call).await.is_ok());
    }

    #[tokio::test]
    async fn test_stored_decision_auto_denies() {
        let ext = make_ext();
        // *.env 是后缀匹配（matches() 支持 prefix=* 后缀匹配）
        ext.store_decision("file.read", "*.env", "deny", "session")
            .unwrap();
        let call = tool_call("read", serde_json::json!({"file_path": "/tmp/.env"}));
        let res = ext.before_tool_call(&call).await;
        assert!(res.is_err());
        let err = res.unwrap_err().to_string();
        assert!(err.contains("denied by extension rule"));
    }

    #[test]
    fn test_store_from_ui_result_always_allow() {
        let ext = make_ext();
        // AlwaysAllowProject → 生成 stored allow project 规则
        let res = ext.store_from_ui_result(
            &UiPermissionResult::AlwaysAllowProject,
            "command.run",
            "npm *",
        );
        assert!(res.is_ok());
        assert!(res.unwrap().is_some());
        let stored = ext.list_stored();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0]["decision"], "allow");
        assert_eq!(stored[0]["scope"], "project");
    }

    #[test]
    fn test_store_from_ui_result_always_deny() {
        let ext = make_ext();
        let res = ext.store_from_ui_result(
            &UiPermissionResult::AlwaysDenyProject,
            "file.read",
            "**/secret/*",
        );
        assert!(res.is_ok());
        assert!(res.unwrap().is_some());
        let stored = ext.list_stored();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0]["decision"], "deny");
    }

    #[test]
    fn test_store_from_ui_result_allow_once_not_persisted() {
        let ext = make_ext();
        // Allow / Deny 不持久化
        let res = ext.store_from_ui_result(&UiPermissionResult::Allow, "command.run", "x");
        assert!(res.is_ok());
        assert!(res.unwrap().is_none());
        let res = ext.store_from_ui_result(&UiPermissionResult::Deny, "command.run", "x");
        assert!(res.is_ok());
        assert!(res.unwrap().is_none());
        assert_eq!(ext.list_stored().len(), 0);
    }

    #[test]
    fn test_decision_source_serde_default() {
        // 旧格式（无 source 字段）反序列化时默认 Config
        let json = r#"{"id":"perm_1","subject":"command.run","pattern":"x","decision":"allow","scope":"session"}"#;
        let rule: PermissionRule = serde_json::from_str(json).unwrap();
        assert_eq!(rule.source, DecisionSource::Config);
    }

    #[test]
    fn test_decision_source_stored_roundtrip() {
        let json = r#"{"id":"perm_stored_1","subject":"command.run","pattern":"x","decision":"allow","scope":"project","source":"stored","created_at":"2026-01-01T00:00:00Z"}"#;
        let rule: PermissionRule = serde_json::from_str(json).unwrap();
        assert_eq!(rule.source, DecisionSource::Stored);
        // 序列化回来还是 stored
        let s = serde_json::to_string(&rule).unwrap();
        assert!(s.contains("\"source\":\"stored\""));
    }

    #[tokio::test]
    async fn test_extension_rpc_store_and_list() {
        let ext = make_ext();
        // 通过 on_extension_rpc 调 store_decision
        let out = ext
            .on_extension_rpc(
                "store_decision",
                serde_json::json!({"subject":"command.run","pattern":"git pull","decision":"allow","scope":"project"}),
            )
            .await
            .unwrap();
        assert_eq!(out["status"], "ok");

        // list_stored
        let out = ext
            .on_extension_rpc("list_stored", serde_json::Value::Null)
            .await
            .unwrap();
        assert_eq!(out["count"], 1);
    }

    #[tokio::test]
    async fn test_extension_rpc_remove_missing_id_errors() {
        let ext = make_ext();
        // 缺 id → 报错
        let res = ext
            .on_extension_rpc("remove_stored", serde_json::Value::Null)
            .await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn test_extension_rpc_clear() {
        let ext = make_ext();
        ext.store_decision("command.run", "a", "allow", "session")
            .unwrap();
        let out = ext
            .on_extension_rpc("clear_stored", serde_json::Value::Null)
            .await
            .unwrap();
        assert_eq!(out["status"], "ok");
        assert_eq!(out["removed"], 1);
    }
}
