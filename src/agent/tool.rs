use super::error::{AgentError, AgentResult};
use super::messages::ToolDef;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// ToolUpdateFn — callback for streaming partial results during tool execution
// ---------------------------------------------------------------------------

pub type ToolUpdateFn = Arc<dyn Fn(String) + Send + Sync>;

// ---------------------------------------------------------------------------
// Tool trait
// ---------------------------------------------------------------------------

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> serde_json::Value;

    /// Execute and return final result (non-streaming).
    async fn execute(&self, args: serde_json::Value, rt: &dyn crate::runtime::Runtime) -> AgentResult<String>;

    /// Execute with streaming updates. Default: fall back to `execute`, call `on_update` once.
    /// Override for real-time streaming (e.g. BashTool reading stdout line by line).
    async fn execute_stream(
        &self,
        args: serde_json::Value,
        on_update: ToolUpdateFn,
        rt: &dyn crate::runtime::Runtime,
    ) -> AgentResult<String> {
        let result = self.execute(args, rt).await?;
        on_update(result.clone());
        Ok(result)
    }
}

// ---------------------------------------------------------------------------
// ToolRegistry
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        let name = tool.name().to_string();
        self.tools.insert(name, tool);
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(|t| t.as_ref())
    }

    pub fn tool_defs(&self) -> Vec<ToolDef> {
        self.tools
            .values()
            .map(|t| ToolDef {
                name: t.name().to_string(),
                description: t.description().to_string(),
                parameters: t.parameters(),
            })
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub fn filter(&mut self, allowed: Vec<&str>) {
        self.tools
            .retain(|name, _| allowed.contains(&name.as_str()));
    }

    /// 限制工具白名单（接受 Vec<String>，switch_agent 时调用）
    pub fn restrict_to(&mut self, allowed: &[String]) {
        self.tools
            .retain(|name, _| allowed.iter().any(|a| a == name));
    }

    pub fn remove(&mut self, name: &str) {
        self.tools.remove(name);
    }
}

// ---------------------------------------------------------------------------
// Calculator tool
// ---------------------------------------------------------------------------

pub struct CalculatorTool;

#[async_trait]
impl Tool for CalculatorTool {
    fn name(&self) -> &str {
        "calculator"
    }

    fn description(&self) -> &str {
        "Evaluate a mathematical expression and return the result."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "expression": {
                    "type": "string",
                    "description": "The mathematical expression to evaluate, e.g. '2 + 3 * 4'"
                }
            },
            "required": ["expression"]
        })
    }

    async fn execute(&self, args: serde_json::Value, _rt: &dyn crate::runtime::Runtime) -> AgentResult<String> {
        let expr = args
            .get("expression")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentError::Tool("missing 'expression' argument".into()))?;

        // Simple evaluation using meval crate or manual. Let me use a basic
        // approach: try to evaluate using a simple parser.
        let result = eval_expr(expr)?;
        Ok(result.to_string())
    }
}

/// Very simple expression evaluator (handles +, -, *, /, parentheses).
/// Not robust for production but good enough for demo.
fn eval_expr(expr: &str) -> AgentResult<f64> {
    let expr = expr.trim();
    if expr.is_empty() {
        return Err(AgentError::Tool("empty expression".into()));
    }

    // Try parsing with a simple recursive approach
    // This is intentionally simple — real impl would use meval crate
    // For demo purposes, we support: numbers, +, -, *, /, ()
    let cleaned: String = expr.chars().filter(|c| !c.is_whitespace()).collect();
    let result = evaluate_expr(&cleaned)?;
    Ok(result)
}

fn evaluate_expr(expr: &str) -> AgentResult<f64> {
    let expr = expr.trim();
    if expr.is_empty() {
        return Err(AgentError::Tool("empty expression".into()));
    }

    // Try to parse as a number first
    if let Ok(n) = expr.parse::<f64>() {
        return Ok(n);
    }

    // Find the last operator outside parentheses (lowest precedence: +, -)
    let mut paren_depth = 0;
    let mut last_add = None;
    let mut last_mul = None;

    for (i, ch) in expr.char_indices().rev() {
        match ch {
            ')' => paren_depth += 1,
            '(' => paren_depth -= 1,
            '+' if paren_depth == 0 => {
                last_add = Some(i);
                break; // left-associative, take the rightmost +/-
            }
            '-' if paren_depth == 0 && i > 0 => {
                // Check it's subtraction, not negation
                let prev = expr[..i].chars().last().unwrap_or(' ');
                if prev.is_ascii_digit() || prev == ')' || prev == 'e' || prev == 'E' {
                    last_add = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }

    if let Some(pos) = last_add {
        let left = evaluate_expr(&expr[..pos])?;
        let right = evaluate_expr(&expr[pos + 1..])?;
        let op = expr[pos..].chars().next().unwrap_or('+');
        return Ok(match op {
            '+' => left + right,
            '-' => left - right,
            _ => unreachable!(),
        });
    }

    // Find * or / for multiplication/division
    paren_depth = 0;
    for (i, ch) in expr.char_indices().rev() {
        match ch {
            ')' => paren_depth += 1,
            '(' => paren_depth -= 1,
            '*' | '/' if paren_depth == 0 => {
                last_mul = Some(i);
                break;
            }
            _ => {}
        }
    }

    if let Some(pos) = last_mul {
        let left = evaluate_expr(&expr[..pos])?;
        let right = evaluate_expr(&expr[pos + 1..])?;
        let op = expr[pos..].chars().next().unwrap_or('*');
        return Ok(match op {
            '*' => left * right,
            '/' => {
                if right == 0.0 {
                    return Err(AgentError::Tool("division by zero".into()));
                }
                left / right
            }
            _ => unreachable!(),
        });
    }

    // Handle parentheses
    if expr.starts_with('(') && expr.ends_with(')') {
        // Check balanced
        let inner = &expr[1..expr.len() - 1];
        return evaluate_expr(inner);
    }

    // Handle negation: -number
    if expr.starts_with('-') && expr.len() > 1 {
        let rest = &expr[1..];
        if let Ok(n) = rest.parse::<f64>() {
            return Ok(-n);
        }
        // Prefixed negation (e.g., -3+2)
        let val = evaluate_expr(rest)?;
        return Ok(-val);
    }

    // Check if it's a function (for demo, just error)
    if expr.contains('(') {
        return Err(AgentError::Tool(format!(
            "unsupported function in expression: {expr}"
        )));
    }

    Err(AgentError::Tool(format!(
        "cannot evaluate expression: {expr}"
    )))
}

// ---------------------------------------------------------------------------
// Echo tool (for testing)
// ---------------------------------------------------------------------------

pub struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }

    fn description(&self) -> &str {
        "Echo back the input text."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "text": {
                    "type": "string",
                    "description": "Text to echo back"
                }
            },
            "required": ["text"]
        })
    }

    async fn execute(&self, args: serde_json::Value, _rt: &dyn crate::runtime::Runtime) -> AgentResult<String> {
        let text = args
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("(no text)");
        Ok(format!("echo: {text}"))
    }
}

// ---------------------------------------------------------------------------
// BranchSession tool — Session Tree 分支/回滚（Agent 自主调用）
// ---------------------------------------------------------------------------

pub struct BranchSessionTool;

#[async_trait]
impl Tool for BranchSessionTool {
    fn name(&self) -> &str {
        "branch_session"
    }

    fn description(&self) -> &str {
        "Branch or rollback within the current session's message tree. \
         The original path is preserved (only-append). \
         Use this to explore alternative approaches or undo a wrong direction."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "from_entry": {
                    "type": "string",
                    "description": "Entry ID to branch from (defaults to current leaf)"
                },
                "name": {
                    "type": "string",
                    "description": "Optional name for the branch"
                },
                "is_rollback": {
                    "type": "boolean",
                    "description": "If true, treat as rollback (records a tombstone). Default false."
                },
                "reason": {
                    "type": "string",
                    "description": "Reason for rollback (only used when is_rollback=true, plain text)"
                }
            },
            "required": []
        })
    }

    async fn execute(&self, args: serde_json::Value, _rt: &dyn crate::runtime::Runtime) -> AgentResult<String> {
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .map_err(|e| AgentError::Tool(format!("cwd error: {}", e)))?;

        // 读当前 session 文件
        let path = crate::session_jsonl::session_path(&cwd);
        let entries: Vec<serde_json::Value> = match std::fs::read_to_string(&path) {
            Ok(content) => content.lines()
                .filter(|l| !l.trim().is_empty())
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect(),
            Err(_) => {
                return Ok("❌ no session file found in current directory".into());
            }
        };

        let is_rollback = args.get("is_rollback").and_then(|v| v.as_bool()).unwrap_or(false);
        let current_leaf = crate::session_tree::resolve_current_leaf(&entries);

        // 目标 entry：from_entry 参数，或当前 leaf
        let target = args.get("from_entry")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| current_leaf.clone())
            .ok_or_else(|| AgentError::Tool(
                "no from_entry specified and no current leaf found".into()
            ))?;

        // 验证 entry 存在
        if !crate::session_tree::entry_exists(&entries, &target) {
            return Ok(format!("❌ entry '{}' not found in session", target));
        }

        // compaction 安全检查
        if let Some(c_id) = crate::session_tree::check_compaction_safety(&entries, &target) {
            return Ok(format!(
                "❌ Cannot branch at {}: it is before a compaction point ({}). \
                 Branching across compaction loses summarized context.",
                target, c_id
            ));
        }

        // 执行 branch 或 rollback
        let new_entries = if is_rollback {
            let reason = args.get("reason").and_then(|v| v.as_str());
            crate::session_tree::make_rollback(&target, current_leaf.as_deref(), reason)
                .map_err(|e| AgentError::Tool(e))?
        } else {
            let name = args.get("name").and_then(|v| v.as_str());
            crate::session_tree::make_branch(&target, name)
                .map_err(|e| AgentError::Tool(e))?
        };

        // 追加到文件（only-append）
        for e in &new_entries {
            crate::session_jsonl::append_raw_entry(&cwd, e);
        }

        // 构造反馈
        let label = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let op = if is_rollback { "rollback" } else { "branch" };
        let label_info = if label.is_empty() { String::new() } else { format!(", labeled: {}", label) };
        Ok(format!(
            "✅ {}: moved leaf to {}{}\nNext message will continue from this branch point.",
            op, target, label_info
        ))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Read tool — read file contents
// ---------------------------------------------------------------------------

pub struct ReadTool;

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str { "read" }
    fn description(&self) -> &str { "Read the contents of a file at the given path." }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"file_path":{"type":"string","description":"Path to the file to read"}},"required":["file_path"]})
    }
    async fn execute(&self, args: serde_json::Value, rt: &dyn crate::runtime::Runtime) -> AgentResult<String> {
        let path = args.get("file_path").and_then(|v| v.as_str()).ok_or_else(|| AgentError::Tool("missing file_path".into()))?;
        rt.read_file(path).await.map_err(|e| AgentError::Tool(format!("read failed: {e}")))
    }
}

// ---------------------------------------------------------------------------
// Grep tool — search file contents
// ---------------------------------------------------------------------------

pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str { "grep" }
    fn description(&self) -> &str { "Search for a pattern in files. Uses ripgrep if available, otherwise grep." }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"pattern":{"type":"string","description":"Search pattern"},"path":{"type":"string","description":"File or directory to search"}},"required":["pattern"]})
    }
    async fn execute(&self, args: serde_json::Value, rt: &dyn crate::runtime::Runtime) -> AgentResult<String> {
        let pattern = args.get("pattern").and_then(|v| v.as_str()).ok_or_else(|| AgentError::Tool("missing pattern".into()))?;
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let cmd = format!("rg -n --max-count=50 {} {} 2>/dev/null || grep -rn --max-count=50 '{}' {} 2>/dev/null || echo '(no matches)'", shell_quote(pattern), shell_quote(path), shell_quote(pattern), shell_quote(path));
        let (stdout, _, _) = rt.execute_command(&cmd, 30).await.map_err(|e| AgentError::Tool(e))?;
        Ok(stdout)
    }
}

fn shell_quote(s: &str) -> String {
    let escaped: String = s.replace("'", "'\\''");
    format!("'{escaped}'")
}
// ---------------------------------------------------------------------------
// Find tool — find files by glob
// ---------------------------------------------------------------------------

pub struct FindTool;

#[async_trait]
impl Tool for FindTool {
    fn name(&self) -> &str { "find" }
    fn description(&self) -> &str { "Find files matching a glob pattern." }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"pattern":{"type":"string","description":"Glob pattern (e.g. **/*.rs)"},"path":{"type":"string","description":"Starting directory"}},"required":["pattern"]})
    }
    async fn execute(&self, args: serde_json::Value, rt: &dyn crate::runtime::Runtime) -> AgentResult<String> {
        let pattern = args.get("pattern").and_then(|v| v.as_str()).ok_or_else(|| AgentError::Tool("missing pattern".into()))?;
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let cmd = format!("find {} -name '{}' -type f 2>/dev/null | head -50", shell_quote(path), pattern);
        let (stdout, _, _) = rt.execute_command(&cmd, 30).await.map_err(|e| AgentError::Tool(e))?;
        Ok(stdout)
    }
}

// ---------------------------------------------------------------------------
// Ls tool — list directory
// ---------------------------------------------------------------------------

pub struct LsTool;

#[async_trait]
impl Tool for LsTool {
    fn name(&self) -> &str { "ls" }
    fn description(&self) -> &str { "List files and directories at a given path." }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"path":{"type":"string","description":"Directory to list"}},"required":[]})
    }
    async fn execute(&self, args: serde_json::Value, rt: &dyn crate::runtime::Runtime) -> AgentResult<String> {
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let (stdout, _, _) = rt.execute_command(&format!("ls -la '{}'", path.replace("'", "'\''")), 30).await.map_err(|e| AgentError::Tool(e))?;
        Ok(stdout)
    }
}

// ---------------------------------------------------------------------------
// Bash tool — execute shell commands
// ---------------------------------------------------------------------------

pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str { "bash" }
    fn description(&self) -> &str { "Execute a shell command and return its output." }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"command":{"type":"string","description":"Shell command to execute"}},"required":["command"]})
    }
    async fn execute(&self, args: serde_json::Value, rt: &dyn crate::runtime::Runtime) -> AgentResult<String> {
        let cmd = args.get("command").and_then(|v| v.as_str()).ok_or_else(|| AgentError::Tool("missing command".into()))?;
        let (stdout, stderr, _exit_code) = rt.execute_command(cmd, 180).await.map_err(|e| AgentError::Tool(e))?;
        let result = if stdout.is_empty() && !stderr.is_empty() { stderr }
            else if !stderr.is_empty() { format!("{stdout}\n{stderr}") }
            else { stdout };
        Ok(result)
    }

    /// Stream stdout line by line via Runtime (goes through SecuredRuntime CommandGuard).
    async fn execute_stream(
        &self,
        args: serde_json::Value,
        on_update: ToolUpdateFn,
        rt: &dyn crate::runtime::Runtime,
    ) -> AgentResult<String> {
        let cmd = args.get("command").and_then(|v| v.as_str()).ok_or_else(|| AgentError::Tool("missing command".into()))?;
        // 走 Runtime 的流式执行（经过 SecuredRuntime CommandGuard 检查）
        let update_fn = |s: String| { on_update(s); };
        rt.execute_command_stream(cmd, 180, &update_fn)
            .await
            .map_err(|e| AgentError::Tool(e))
    }
}

// ---------------------------------------------------------------------------
// Write tool — write/overwrite files
// ---------------------------------------------------------------------------

pub struct WriteTool;

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str { "write" }
    fn description(&self) -> &str { "Write content to a file, creating it if it doesn't exist." }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"file_path":{"type":"string","description":"Path to write to"},"content":{"type":"string","description":"Content to write"}},"required":["file_path","content"]})
    }
    async fn execute(&self, args: serde_json::Value, rt: &dyn crate::runtime::Runtime) -> AgentResult<String> {
        let path = args.get("file_path").and_then(|v| v.as_str()).ok_or_else(|| AgentError::Tool("missing file_path".into()))?;
        let content = args.get("content").and_then(|v| v.as_str()).ok_or_else(|| AgentError::Tool("missing content".into()))?;
        rt.write_file(path, content).await.map_err(|e| AgentError::Tool(format!("write failed: {e}")))?;
        Ok(format!("wrote {} bytes to {}", content.len(), path))
    }
}

// ---------------------------------------------------------------------------
// Edit tool — search-and-replace in files
// ---------------------------------------------------------------------------

pub struct EditTool;

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str { "edit" }
    fn description(&self) -> &str { "Search and replace text in a file (first occurrence)." }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"file_path":{"type":"string","description":"Path to edit"},"old":{"type":"string","description":"Text to search for"},"new":{"type":"string","description":"Replacement text"}},"required":["file_path","old","new"]})
    }
    async fn execute(&self, args: serde_json::Value, rt: &dyn crate::runtime::Runtime) -> AgentResult<String> {
        let path = args.get("file_path").and_then(|v| v.as_str()).ok_or_else(|| AgentError::Tool("missing file_path".into()))?;
        let old = args.get("old").and_then(|v| v.as_str()).ok_or_else(|| AgentError::Tool("missing old".into()))?;
        let new = args.get("new").and_then(|v| v.as_str()).ok_or_else(|| AgentError::Tool("missing new".into()))?;
        // 用 Runtime 的 edit_file（内部调 read + replace + write）
        rt.edit_file(path, old, new).await.map_err(|e| AgentError::Tool(e))?;
        Ok(format!("replaced 1 occurrence in {path}"))
    }
}

// ---------------------------------------------------------------------------
// GenericTool — for extension-defined tools (JSON definitions)
// ---------------------------------------------------------------------------

pub struct GenericTool {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[async_trait]
impl Tool for GenericTool {
    fn name(&self) -> &str { &self.name }
    fn description(&self) -> &str { &self.description }
    fn parameters(&self) -> serde_json::Value { self.parameters.clone() }
    async fn execute(&self, args: serde_json::Value, _rt: &dyn crate::runtime::Runtime) -> AgentResult<String> {
        // Return a JSON response showing the tool was called with these args
        Ok(serde_json::json!({
            "tool": self.name,
            "args": args,
            "result": "executed successfully (extension-defined tool)"
        }).to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn calculator_works() {
        let tool = CalculatorTool;
        let cases = vec![
            (r#"{"expression": "2 + 3"}"#, "5"),
            (r#"{"expression": "10 * 5"}"#, "50"),
            (r#"{"expression": "20 / 4"}"#, "5"),
            (r#"{"expression": "(2 + 3) * 4"}"#, "20"),
        ];

        for (json, expected) in cases {
            let args: serde_json::Value = serde_json::from_str(json).unwrap();
            let result = tool.execute(args, &crate::runtime::LocalRuntime::new()).await.unwrap();
            assert_eq!(result, expected, "for {json}");
        }
    }

    #[tokio::test]
    async fn echo_tool_works() {
        let tool = EchoTool;
        let args: serde_json::Value = serde_json::json!({"text": "hello"});
        let result = tool.execute(args, &crate::runtime::LocalRuntime::new()).await.unwrap();
        assert_eq!(result, "echo: hello");
    }

    #[test]
    fn registry_works() {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(CalculatorTool));
        reg.register(Box::new(EchoTool));

        assert!(reg.get("calculator").is_some());
        assert!(reg.get("echo").is_some());
        assert!(reg.get("nonexistent").is_none());

        let defs = reg.tool_defs();
        assert_eq!(defs.len(), 2);
    }
}

// ---------------------------------------------------------------------------
// Git tools — 6 个 Git 操作工具
// ---------------------------------------------------------------------------

/// git_status: 查看工作区状态
pub struct GitStatusTool;

#[async_trait]
impl Tool for GitStatusTool {
    fn name(&self) -> &str { "git_status" }
    fn description(&self) -> &str { "Show git working tree status (git status --short)." }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"path":{"type":"string","description":"Repository path (default: cwd)"}}})
    }
    async fn execute(&self, args: serde_json::Value, rt: &dyn crate::runtime::Runtime) -> AgentResult<String> {
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        run_git_with(rt, path, &["status", "--short"]).await
    }
}

/// git_diff: 查看 diff
pub struct GitDiffTool;

#[async_trait]
impl Tool for GitDiffTool {
    fn name(&self) -> &str { "git_diff" }
    fn description(&self) -> &str { "Show git diff (unstaged changes)." }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"path":{"type":"string","description":"Repository path"},"staged":{"type":"boolean","description":"Show staged changes (default: false)"}}})
    }
    async fn execute(&self, args: serde_json::Value, rt: &dyn crate::runtime::Runtime) -> AgentResult<String> {
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let staged = args.get("staged").and_then(|v| v.as_bool()).unwrap_or(false);
        if staged {
            run_git_with(rt, path, &["diff", "--cached"]).await
        } else {
            run_git_with(rt, path, &["diff"]).await
        }
    }
}

/// git_log: 查看提交历史
pub struct GitLogTool;

#[async_trait]
impl Tool for GitLogTool {
    fn name(&self) -> &str { "git_log" }
    fn description(&self) -> &str { "Show recent git commits (git log --oneline)." }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"path":{"type":"string","description":"Repository path"},"count":{"type":"integer","description":"Number of commits (default: 10)"}}})
    }
    async fn execute(&self, args: serde_json::Value, rt: &dyn crate::runtime::Runtime) -> AgentResult<String> {
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let count = args.get("count").and_then(|v| v.as_u64()).unwrap_or(10);
        run_git_with(rt, path, &["log", "--oneline", "-n", &count.to_string()]).await
    }
}

/// git_add: 暂存文件
pub struct GitAddTool;

#[async_trait]
impl Tool for GitAddTool {
    fn name(&self) -> &str { "git_add" }
    fn description(&self) -> &str { "Stage files for commit (git add)." }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"path":{"type":"string","description":"Repository path"},"files":{"type":"string","description":"Files to add (default: '.' for all)"}},"required":[]})
    }
    async fn execute(&self, args: serde_json::Value, rt: &dyn crate::runtime::Runtime) -> AgentResult<String> {
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let files = args.get("files").and_then(|v| v.as_str()).unwrap_or(".");
        run_git_with(rt, path, &["add", files]).await
    }
}

/// git_commit: 提交
pub struct GitCommitTool;

#[async_trait]
impl Tool for GitCommitTool {
    fn name(&self) -> &str { "git_commit" }
    fn description(&self) -> &str { "Create a git commit." }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"path":{"type":"string","description":"Repository path"},"message":{"type":"string","description":"Commit message"}},"required":["message"]})
    }
    async fn execute(&self, args: serde_json::Value, rt: &dyn crate::runtime::Runtime) -> AgentResult<String> {
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let message = args.get("message").and_then(|v| v.as_str())
            .ok_or_else(|| AgentError::Tool("missing message".into()))?;
        run_git_with(rt, path, &["commit", "-m", message]).await
    }
}

/// git_branch: 查看/创建分支
pub struct GitBranchTool;

#[async_trait]
impl Tool for GitBranchTool {
    fn name(&self) -> &str { "git_branch" }
    fn description(&self) -> &str { "List branches or create a new one." }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"path":{"type":"string","description":"Repository path"},"create":{"type":"string","description":"Create a new branch with this name"}}})
    }
    async fn execute(&self, args: serde_json::Value, rt: &dyn crate::runtime::Runtime) -> AgentResult<String> {
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        if let Some(branch) = args.get("create").and_then(|v| v.as_str()) {
            run_git_with(rt, path, &["checkout", "-b", branch]).await
        } else {
            run_git_with(rt, path, &["branch", "-a"]).await
        }
    }
}

/// 执行 git 命令的辅助函数
async fn run_git_with(rt: &dyn crate::runtime::Runtime, path: &str, args: &[&str]) -> AgentResult<String> {
    let cmd = format!("git -C {} {}", shell_quote(path), args.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" "));
    let (stdout, stderr, exit_code) = rt.execute_command(&cmd, 60).await.map_err(|e| AgentError::Tool(e))?;
    if exit_code == 0 {
        Ok(if stdout.is_empty() && !stderr.is_empty() { stderr } else { stdout })
    } else {
        Ok(format!("git exit {exit_code}: {stderr}"))
    }
}

// ---------------------------------------------------------------------------
// Worker 编排工具 — spawn_worker / send_to_worker
// ---------------------------------------------------------------------------
//
// 这两个工具把内核的 create_worker / send_to_worker 能力暴露给 LLM。
// 设计原则对齐 AGENTS.md：能力在内核实现，通过 Runtime（Tool 的把手）暴露。
//
// LocalRuntime（ion CLI）不支持这两个工具（Runtime::spawn_worker 默认返回 Err），
// 只有 WorkerRuntime（ion-worker 子进程）才有真实实现。
//
// 关键语义：
// - spawn_worker(child)  → 阻塞，等子 worker 首轮 agent_end，返回 first_turn_output
// - spawn_worker(peer)   → 立即返回 worker_id，子任务在后台跑，靠 CHANNEL_SEND 汇报
// - send_to_worker       → fire-and-forget，给任意 worker 发 prompt（resume/对话）

pub struct SpawnWorkerTool;

#[async_trait]
impl Tool for SpawnWorkerTool {
    fn name(&self) -> &str { "spawn_worker" }

    fn description(&self) -> &str {
        "Spawn a child or peer Worker to execute a task autonomously. Returns a JSON object.\n\
         - relation='child' + wait=true (default): BLOCKS until the child finishes its first turn, returns {status:'first_turn_completed', first_turn_output}.\n\
         - relation='child' + wait=false: returns IMMEDIATELY with worker_id, call await_worker(worker_id) later to collect output (use for parallel children).\n\
         - relation='peer': returns IMMEDIATELY with worker_id, peer auto-reports to creator via follow_up when done (no polling needed).\n\
         The spawned worker remains alive after first turn — use resume_worker to continue conversation."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "relation": {
                    "type": "string",
                    "enum": ["child", "peer"],
                    "description": "child = parent-owned, synchronous by default; peer = independent, reports via follow_up"
                },
                "agent": {
                    "type": "string",
                    "description": "Agent role name (must match .ion/agents/<name>.md). e.g. 'developer', 'reviewer', 'coordinator'"
                },
                "task": {
                    "type": "string",
                    "description": "Detailed task spec for the new worker."
                },
                "wait": {
                    "type": "boolean",
                    "default": true,
                    "description": "(child only) true = block until first turn done; false = return immediately, use await_worker later"
                },
                "report_channel": {
                    "type": "string",
                    "default": "main",
                    "description": "(peer only) Channel name for status broadcasts"
                },
                "worktree": {
                    "type": "boolean",
                    "default": false,
                    "description": "If true, run this worker in an isolated git worktree (new branch). Useful for developers so they don't pollute the main branch."
                }
            },
            "required": ["relation", "agent", "task"]
        })
    }

    async fn execute(&self, args: serde_json::Value, rt: &dyn crate::runtime::Runtime) -> AgentResult<String> {
        let relation_str = args.get("relation").and_then(|v| v.as_str()).unwrap_or("child");
        let agent = args.get("agent").and_then(|v| v.as_str())
            .ok_or_else(|| AgentError::Tool("missing required arg: agent".into()))?
            .to_string();
        let task = args.get("task").and_then(|v| v.as_str())
            .ok_or_else(|| AgentError::Tool("missing required arg: task".into()))?
            .to_string();
        let report_channel = args.get("report_channel").and_then(|v| v.as_str()).map(String::from);
        let wait = args.get("wait").and_then(|v| v.as_bool()).unwrap_or(true);
        let worktree = args.get("worktree").and_then(|v| v.as_bool());

        let relation = match relation_str {
            "peer" => crate::runtime::SpawnRelation::Peer,
            _ => crate::runtime::SpawnRelation::Child,
        };

        let req = crate::runtime::SpawnWorkerRequest {
            relation: relation.clone(),
            agent,
            task,
            name: None,
            report_channel: report_channel.clone(),
            wait,
            worktree,
        };

        let resp = rt.spawn_worker(req).await.map_err(AgentError::Tool)?;

        // 结构化 JSON 返回（LLM 靠 status 字段判断下一步）
        let truncated_output = resp.first_turn_output.as_ref().map(|out| {
            let preview: String = out.chars().take(800).collect();
            if out.chars().count() > 800 {
                format!("{}... (truncated, {} total chars)", preview, out.chars().count())
            } else {
                preview
            }
        });

        let result = serde_json::json!({
            "type": "worker_spawned",
            "relation": match resp.relation {
                crate::runtime::SpawnRelation::Child => "child",
                crate::runtime::SpawnRelation::Peer => "peer",
            },
            "worker_id": resp.worker_id,
            "status": resp.status,
            "first_turn_output": truncated_output,
            "report_channel": resp.report_channel,
        });
        Ok(result.to_string())
    }
}

pub struct SendToWorkerTool;

#[async_trait]
impl Tool for SendToWorkerTool {
    fn name(&self) -> &str { "send_to_worker" }

    fn description(&self) -> &str {
        "Send a message to another Worker (identified by worker_id). Fire-and-forget (async): returns \
         immediately as {type:'message_sent', status:'delivered_async'}, does NOT wait for target to respond. \
         Use cases: (1) parent → child additional instructions, (2) peer-to-peer async chat, \
         (3) coordinator → peer additional task. To SYNCHRONOUSLY continue a child conversation and get \
         the response, use resume_worker instead."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "worker_id": { "type": "string", "description": "Target worker_id (returned by spawn_worker)" },
                "text": { "type": "string", "description": "Message content (becomes a new prompt for the target)" }
            },
            "required": ["worker_id", "text"]
        })
    }

    async fn execute(&self, args: serde_json::Value, rt: &dyn crate::runtime::Runtime) -> AgentResult<String> {
        let worker_id = args.get("worker_id").and_then(|v| v.as_str())
            .ok_or_else(|| AgentError::Tool("missing required arg: worker_id".into()))?.to_string();
        let text = args.get("text").and_then(|v| v.as_str())
            .ok_or_else(|| AgentError::Tool("missing required arg: text".into()))?.to_string();

        rt.send_to_worker(&worker_id, &text).await.map_err(AgentError::Tool)?;
        let result = serde_json::json!({
            "type": "message_sent",
            "target": worker_id,
            "status": "delivered_async"
        });
        Ok(result.to_string())
    }
}

pub struct ResumeWorkerTool;

#[async_trait]
impl Tool for ResumeWorkerTool {
    fn name(&self) -> &str { "resume_worker" }

    fn description(&self) -> &str {
        "Send a message to an existing Worker AND block until its next turn completes (synchronous resume). \
         Returns {type:'worker_resumed', worker_id, status:'turn_completed', response_output}. \
         Use this to continue a conversation with a child worker started via spawn_worker(child), \
         or to follow up with any worker when you need its response."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "worker_id": { "type": "string", "description": "Target worker_id (must already exist)" },
                "text": { "type": "string", "description": "Message to send (becomes a new prompt for the target)" }
            },
            "required": ["worker_id", "text"]
        })
    }

    async fn execute(&self, args: serde_json::Value, rt: &dyn crate::runtime::Runtime) -> AgentResult<String> {
        let worker_id = args.get("worker_id").and_then(|v| v.as_str())
            .ok_or_else(|| AgentError::Tool("missing required arg: worker_id".into()))?.to_string();
        let text = args.get("text").and_then(|v| v.as_str())
            .ok_or_else(|| AgentError::Tool("missing required arg: text".into()))?.to_string();

        let out = rt.resume_worker(&worker_id, &text).await.map_err(AgentError::Tool)?;
        let truncated: String = out.chars().take(800).collect();
        let truncated = if out.chars().count() > 800 {
            format!("{}... (truncated, {} total chars)", truncated, out.chars().count())
        } else { truncated };

        let result = serde_json::json!({
            "type": "worker_resumed",
            "worker_id": worker_id,
            "status": "turn_completed",
            "response_output": truncated,
        });
        Ok(result.to_string())
    }
}

pub struct AwaitWorkerTool;

#[async_trait]
impl Tool for AwaitWorkerTool {
    fn name(&self) -> &str { "await_worker" }

    fn description(&self) -> &str {
        "Block until the target Worker finishes its next turn, returns {type:'worker_awaited', \
         worker_id, status:'turn_completed', first_turn_output}. Use this to collect results from \
         children spawned with wait=false. Pair with spawn_worker(child, wait=false) for parallel work: \
         spawn N children non-blocking, then call await_worker on each."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "worker_id": { "type": "string", "description": "Target worker_id to wait on" }
            },
            "required": ["worker_id"]
        })
    }

    async fn execute(&self, args: serde_json::Value, rt: &dyn crate::runtime::Runtime) -> AgentResult<String> {
        let worker_id = args.get("worker_id").and_then(|v| v.as_str())
            .ok_or_else(|| AgentError::Tool("missing required arg: worker_id".into()))?.to_string();

        let out = rt.await_worker(&worker_id).await.map_err(AgentError::Tool)?;
        let truncated: String = out.chars().take(800).collect();
        let truncated = if out.chars().count() > 800 {
            format!("{}... (truncated, {} total chars)", truncated, out.chars().count())
        } else { truncated };

        let result = serde_json::json!({
            "type": "worker_awaited",
            "worker_id": worker_id,
            "status": "turn_completed",
            "first_turn_output": truncated,
        });
        Ok(result.to_string())
    }
}

pub struct ChannelSendTool;

#[async_trait]
impl Tool for ChannelSendTool {
    fn name(&self) -> &str { "channel_send" }

    fn description(&self) -> &str {
        "Broadcast a message to all Workers subscribed to a channel. Returns \
         {type:'channel_sent', channel, status:'broadcast'}. Use for fan-out announcements, \
         status updates, or coordinating multiple peers. Subscribers receive the message as a \
         follow_up (auto-processed next turn)."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "channel": { "type": "string", "description": "Channel name (e.g. 'main', 'review')" },
                "text": { "type": "string", "description": "Message content" }
            },
            "required": ["channel", "text"]
        })
    }

    async fn execute(&self, args: serde_json::Value, rt: &dyn crate::runtime::Runtime) -> AgentResult<String> {
        let channel = args.get("channel").and_then(|v| v.as_str())
            .ok_or_else(|| AgentError::Tool("missing required arg: channel".into()))?.to_string();
        let text = args.get("text").and_then(|v| v.as_str())
            .ok_or_else(|| AgentError::Tool("missing required arg: text".into()))?.to_string();

        rt.channel_send(&channel, &text).await.map_err(AgentError::Tool)?;
        let result = serde_json::json!({
            "type": "channel_sent",
            "channel": channel,
            "status": "broadcast"
        });
        Ok(result.to_string())
    }
}

pub struct KillWorkerTool;

#[async_trait]
impl Tool for KillWorkerTool {
    fn name(&self) -> &str { "kill_worker" }

    fn description(&self) -> &str {
        "Terminate a Worker. Returns {type:'worker_killed', worker_id, status:'terminated'}. \
         Use when a worker is stuck, redundant, or its work is no longer needed."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "worker_id": { "type": "string", "description": "Target worker_id to terminate" }
            },
            "required": ["worker_id"]
        })
    }

    async fn execute(&self, args: serde_json::Value, rt: &dyn crate::runtime::Runtime) -> AgentResult<String> {
        let worker_id = args.get("worker_id").and_then(|v| v.as_str())
            .ok_or_else(|| AgentError::Tool("missing required arg: worker_id".into()))?.to_string();

        rt.kill_worker(&worker_id).await.map_err(AgentError::Tool)?;
        let result = serde_json::json!({
            "type": "worker_killed",
            "worker_id": worker_id,
            "status": "terminated"
        });
        Ok(result.to_string())
    }
}

// ---------------------------------------------------------------------------
// GlobalMemory tools — 跨项目记忆检索（V0.2，serve 模式可用）
// ---------------------------------------------------------------------------

pub struct GlobalMemorySearchTool;

#[async_trait]
impl Tool for GlobalMemorySearchTool {
    fn name(&self) -> &str { "global_memory_search" }

    fn description(&self) -> &str {
        "Search the global cross-project memory database using FTS5 full-text search. \
         Available in serve mode (ion serve). Returns matching entries from all projects."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Full-text search query"},
                "project": {"type": "string", "description": "Optional: limit to a specific project"}
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, args: serde_json::Value, _rt: &dyn crate::runtime::Runtime) -> AgentResult<String> {
        let query = args.get("query").and_then(|v| v.as_str())
            .ok_or_else(|| AgentError::Tool("missing 'query'".into()))?;
        let project = args.get("project").and_then(|v| v.as_str());
        let db_path = crate::global_memory::GlobalMemoryStore::db_path();
        let store = crate::global_memory::GlobalMemoryStore::open(&db_path)
            .map_err(|e| AgentError::Tool(format!("open global memory: {}", e)))?;
        let results = store.search(query, project)
            .map_err(|e| AgentError::Tool(e))?;
        if results.is_empty() {
            return Ok("No matching memories found.".into());
        }
        let mut out = format!("Found {} memories:\n", results.len());
        for (i, e) in results.iter().enumerate() {
            out.push_str(&format!(
                "{}. [{}] {} (importance:{}, category:{})\n",
                i + 1, e.project, e.content.chars().take(80).collect::<String>(),
                e.importance, e.category
            ));
        }
        Ok(out)
    }
}

pub struct GlobalMemorySaveTool;

#[async_trait]
impl Tool for GlobalMemorySaveTool {
    fn name(&self) -> &str { "global_memory_save" }

    fn description(&self) -> &str {
        "Save a memory to the global cross-project database. \
         Use for cross-session/cross-project knowledge that should persist."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "content": {"type": "string", "description": "Memory content"},
                "category": {"type": "string", "description": "Category: preference/decision/note"},
                "tags": {"type": "string", "description": "Comma-separated tags"},
                "project": {"type": "string", "description": "Project name"},
                "importance": {"type": "integer", "description": "1-10 (default 5)"}
            },
            "required": ["content", "project"]
        })
    }

    async fn execute(&self, args: serde_json::Value, _rt: &dyn crate::runtime::Runtime) -> AgentResult<String> {
        let content = args.get("content").and_then(|v| v.as_str())
            .ok_or_else(|| AgentError::Tool("missing 'content'".into()))?;
        let project = args.get("project").and_then(|v| v.as_str())
            .ok_or_else(|| AgentError::Tool("missing 'project'".into()))?;
        let category = args.get("category").and_then(|v| v.as_str()).unwrap_or("");
        let tags = args.get("tags").and_then(|v| v.as_str()).unwrap_or("");
        let importance = args.get("importance").and_then(|v| v.as_i64()).unwrap_or(5) as i32;
        let db_path = crate::global_memory::GlobalMemoryStore::db_path();
        let store = crate::global_memory::GlobalMemoryStore::open(&db_path)
            .map_err(|e| AgentError::Tool(format!("open global memory: {}", e)))?;
        let id = store.save(content, category, tags, project, importance)
            .map_err(|e| AgentError::Tool(e))?;
        Ok(format!("✅ Saved to global memory: {}", id))
    }
}
