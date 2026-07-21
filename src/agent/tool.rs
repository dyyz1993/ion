use super::error::{AgentError, AgentResult};
use super::messages::ToolDef;
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::PathBuf;
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
// CalculatorAdvancedTool — scientific calculator
// ---------------------------------------------------------------------------

pub struct CalculatorAdvancedTool;

#[async_trait]
impl Tool for CalculatorAdvancedTool {
    fn name(&self) -> &str {
        "calculator_advanced"
    }

    fn description(&self) -> &str {
        "Scientific calculator supporting trigonometry (sin/cos/tan), logarithms (log/ln), and roots (sqrt/cbrt). Args: operation (one of sin/cos/tan/log/ln/sqrt/cbrt), value (f64)."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "operation": {
                    "type": "string",
                    "description": "Operation to perform: sin, cos, tan, log, ln, sqrt, cbrt"
                },
                "value": {
                    "type": "number",
                    "description": "Input value for the operation"
                }
            },
            "required": ["operation", "value"]
        })
    }

    async fn execute(&self, args: serde_json::Value, _rt: &dyn crate::runtime::Runtime) -> AgentResult<String> {
        let operation = args
            .get("operation")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentError::Tool("missing 'operation' argument".into()))?;

        let value = args
            .get("value")
            .and_then(|v| v.as_f64())
            .ok_or_else(|| AgentError::Tool("missing or invalid 'value' argument".into()))?;

        let result = match operation {
            "sin" => value.to_radians().sin(),
            "cos" => value.to_radians().cos(),
            "tan" => value.to_radians().tan(),
            "log" => value.log10(),
            "ln" => value.ln(),
            "sqrt" => value.sqrt(),
            "cbrt" => value.cbrt(),
            _ => {
                return Err(AgentError::Tool(format!(
                    "unsupported operation: '{}'. Supported: sin, cos, tan, log, ln, sqrt, cbrt",
                    operation
                )));
            }
        };

        Ok(format!("{:.6}", result))
    }
}

// ---------------------------------------------------------------------------
// UuidGeneratorTool — generate a UUID v4 string
// ---------------------------------------------------------------------------

pub struct UuidGeneratorTool;

#[async_trait]
impl Tool for UuidGeneratorTool {
    fn name(&self) -> &str { "uuid" }
    fn description(&self) -> &str {
        "Generate a UUID v4 string. No arguments needed."
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    async fn execute(&self, _args: serde_json::Value, _rt: &dyn crate::runtime::Runtime) -> AgentResult<String> {
        Ok(uuid::Uuid::new_v4().to_string())
    }
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

/// 对文件内容按行切片并加上 `cat -n` 风格行号。
///
/// - `offset`: 1-based 起始行号，默认 1
/// - `limit`:  返回行数，默认全部
///
/// 行号右对齐，宽度按文件总行数计算，后接 `\t`：
/// ```text
///    101\tfn process(data: &[u8]) {
///    102\t    let decoded = decode(data)?;
/// ```
fn format_lines(content: &str, offset: usize, limit: Option<usize>) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    if total == 0 {
        return "(empty file)".into();
    }
    if offset > total {
        return format!("(file has {total} lines, offset {offset} is out of range)");
    }

    let start = offset.saturating_sub(1); // 0-based
    let end = match limit {
        Some(l) => (start + l).min(total),
        None => total,
    };
    let selected = &lines[start..end];
    let width = total.to_string().len();

    let mut result = String::with_capacity(selected.len() * 80);
    for (i, line) in selected.iter().enumerate() {
        let line_num = start + i + 1; // 1-based
        result.push_str(&format!("{:>width$}\t{line}\n", line_num, width = width));
    }

    // 切片时追加范围提示
    if start > 0 || end < total {
        result.push_str(&format!("(showing lines {}-{} of {})\n", start + 1, end, total));
    }
    result
}

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str { "read" }
    fn description(&self) -> &str { "Read the contents of a file. Supports offset (1-based start line) and limit (number of lines) to read a specific range. Output includes line numbers." }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"file_path":{"type":"string","description":"Path to the file to read"},"offset":{"type":"integer","description":"1-based line number to start reading from (default: 1)"},"limit":{"type":"integer","description":"Maximum number of lines to read (default: all)"}},"required":["file_path"]})
    }
    async fn execute(&self, args: serde_json::Value, rt: &dyn crate::runtime::Runtime) -> AgentResult<String> {
        let path = args.get("file_path").and_then(|v| v.as_str()).ok_or_else(|| AgentError::Tool("missing file_path".into()))?;
        let content = rt.read_file(path).await.map_err(|e| AgentError::Tool(format!("read failed: {e}")))?;
        let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
        let limit = args.get("limit").and_then(|v| v.as_u64()).map(|v| v as usize);
        Ok(format_lines(&content, offset, limit))
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
        // BashTool 默认 180s；可通过 ION_BASH_TIMEOUT 环境变量覆盖（单位秒）。
        // improver 跑 container exec cargo build/test 时建议设 ION_BASH_TIMEOUT=1800。
        let bash_timeout = std::env::var("ION_BASH_TIMEOUT")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(180);
        let (stdout, stderr, _exit_code) = rt.execute_command(cmd, bash_timeout).await.map_err(|e| AgentError::Tool(e))?;
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
        let bash_timeout = std::env::var("ION_BASH_TIMEOUT")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(180);
        // 走 Runtime 的流式执行（经过 SecuredRuntime CommandGuard 检查）
        let update_fn = |s: String| { on_update(s); };
        rt.execute_command_stream(cmd, bash_timeout, &update_fn)
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
    async fn execute_stream(
        &self,
        args: serde_json::Value,
        on_update: ToolUpdateFn,
        rt: &dyn crate::runtime::Runtime,
    ) -> AgentResult<String> {
        let path = args.get("file_path").and_then(|v| v.as_str()).ok_or_else(|| AgentError::Tool("missing file_path".into()))?;
        let content = args.get("content").and_then(|v| v.as_str()).ok_or_else(|| AgentError::Tool("missing content".into()))?;

        // 先读旧内容（算 diff 用）
        let old_content = rt.read_file(path).await.unwrap_or_default();
        let old_lines = old_content.lines().count() as i64;

        // 流式写入：分块写，每块 update 行数
        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();
        let chunk_size = (total_lines / 10).max(1).min(50); // 分 10 块或每块最多 50 行
        let mut written = String::new();

        for chunk in lines.chunks(chunk_size) {
            for line in chunk {
                written.push_str(line);
                written.push('\n');
            }
            let current_lines = written.lines().count() as i64;
            let added = current_lines;
            let removed = if old_lines > current_lines { old_lines - current_lines } else { 0 };
            on_update(format!("+{} -{} lines (writing {}...)", added, removed, path));
        }

        rt.write_file(path, content).await.map_err(|e| AgentError::Tool(format!("write failed: {e}")))?;

        let new_lines = content.lines().count() as i64;
        let added = new_lines;
        let removed = if old_lines > new_lines { old_lines - new_lines } else { 0 };
        Ok(format!("wrote {} bytes to {} (+{} -{} lines)", content.len(), path, added, removed))
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
        rt.edit_file(path, old, new).await.map_err(|e| AgentError::Tool(e))?;
        Ok(format!("replaced 1 occurrence in {path}"))
    }
    async fn execute_stream(
        &self,
        args: serde_json::Value,
        on_update: ToolUpdateFn,
        rt: &dyn crate::runtime::Runtime,
    ) -> AgentResult<String> {
        let path = args.get("file_path").and_then(|v| v.as_str()).ok_or_else(|| AgentError::Tool("missing file_path".into()))?;
        let old = args.get("old").and_then(|v| v.as_str()).ok_or_else(|| AgentError::Tool("missing old".into()))?;
        let new = args.get("new").and_then(|v| v.as_str()).ok_or_else(|| AgentError::Tool("missing new".into()))?;

        // 推送 diff 预览
        let old_lines = old.lines().count() as i64;
        let new_lines = new.lines().count() as i64;
        let added = new_lines;
        let removed = old_lines;
        on_update(format!("+{} -{} lines (editing {}...)", added, removed, path));

        rt.edit_file(path, old, new).await.map_err(|e| AgentError::Tool(e))?;
        Ok(format!("replaced 1 occurrence in {path} (+{} -{} lines)", added, removed))
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

/// 远程工具 — 把 HTTP API 端点注册为 ION 工具。
/// LLM 调用时走 HTTP POST 到指定 URL，参数作为 JSON body 发送。
pub struct RemoteTool {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
    pub url: String,
    pub method: String,
    pub headers: HashMap<String, String>,
}

#[async_trait]
impl Tool for RemoteTool {
    fn name(&self) -> &str { &self.name }
    fn description(&self) -> &str { &self.description }
    fn parameters(&self) -> serde_json::Value { self.parameters.clone() }

    async fn execute(&self, args: serde_json::Value, _rt: &dyn crate::runtime::Runtime) -> AgentResult<String> {
        let client = reqwest::Client::new();
        let method = match self.method.to_uppercase().as_str() {
            "GET" => reqwest::Method::GET,
            "PUT" => reqwest::Method::PUT,
            "DELETE" => reqwest::Method::DELETE,
            _ => reqwest::Method::POST,
        };

        let is_get_or_delete = matches!(method, reqwest::Method::GET | reqwest::Method::DELETE);
        let mut req = client.request(method, &self.url);
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        // GET/DELETE: 参数作为 query string；POST/PUT: 参数作为 JSON body
        let resp = if is_get_or_delete {
            // 把 args 的字段作为 query 参数
            let query_pairs: Vec<(String, String)> = args.as_object()
                .map(|obj| obj.iter().filter_map(|(k, v)| {
                    v.as_str().map(|s| (k.clone(), s.to_string()))
                }).collect())
                .unwrap_or_default();
            req.query(&query_pairs).send().await
        } else {
            req.header("Content-Type", "application/json")
                .body(args.to_string())
                .send().await
        };

        match resp {
            Ok(r) => {
                let status = r.status();
                let text = r.text().await.unwrap_or_default();
                if status.is_success() {
                    Ok(text)
                } else {
                    Ok(format!("HTTP {} {}: {}", status.as_u16(), status.canonical_reason().unwrap_or(""), text))
                }
            }
            Err(e) => Err(AgentError::Tool(format!("remote tool '{}' request failed: {e}", self.name))),
        }
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

    // ── format_lines / ReadTool offset+limit ──────────────────────────

    #[test]
    fn format_lines_full() {
        let content = "alpha\nbeta\ngamma\n";
        let out = format_lines(content, 1, None);
        // 应包含 3 行，行号右对齐宽度 1
        assert!(out.contains("1\talpha"), "got: {out}");
        assert!(out.contains("2\tbeta"));
        assert!(out.contains("3\tgamma"));
        // 全文读不需要范围提示
        assert!(!out.contains("showing lines"));
    }

    #[test]
    fn format_lines_offset_limit() {
        let content: String = (1..=20).map(|i| format!("line{i}\n")).collect();
        let out = format_lines(&content, 10, Some(5));
        // 行 10~14
        assert!(out.contains("10\tline10"), "got: {out}");
        assert!(out.contains("14\tline14"));
        assert!(!out.contains("line9"));
        assert!(!out.contains("line15"));
        // 应有范围提示
        assert!(out.contains("showing lines 10-14 of 20"));
    }

    #[test]
    fn format_lines_offset_out_of_range() {
        let content = "only\none\nline\n";
        let out = format_lines(content, 50, None);
        assert!(out.contains("out of range"), "got: {out}");
    }

    #[test]
    fn format_lines_limit_exceeds_end() {
        let content = "a\nb\nc\n";
        let out = format_lines(content, 2, Some(100));
        // 行 2~3
        assert!(out.contains("2\tb"), "got: {out}");
        assert!(out.contains("3\tc"));
        assert!(!out.contains("1\ta"));
    }

    #[test]
    fn format_lines_empty_file() {
        let out = format_lines("", 1, None);
        assert_eq!(out, "(empty file)");
    }

    #[tokio::test]
    async fn read_tool_offset_limit_via_file() {
        use std::io::Write;
        let dir = std::env::temp_dir();
        let path = dir.join("ion_read_test_offset.txt");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            for i in 1..=30 {
                writeln!(f, "row{i}").unwrap();
            }
        }
        let tool = ReadTool;
        let args = serde_json::json!({"file_path": path.to_str().unwrap(), "offset": 10, "limit": 5});
        let rt = crate::runtime::LocalRuntime::new();
        let result = tool.execute(args, &rt).await.unwrap();
        assert!(result.contains("10\trow10"), "got: {result}");
        assert!(result.contains("14\trow14"));
        assert!(!result.contains("row9"));
        assert!(!result.contains("row15"));
        assert!(result.contains("showing lines 10-14 of 30"));
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn read_tool_full_file_compatible() {
        use std::io::Write;
        let dir = std::env::temp_dir();
        let path = dir.join("ion_read_test_full.txt");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(f, "hello").unwrap();
            writeln!(f, "world").unwrap();
        }
        let tool = ReadTool;
        let args = serde_json::json!({"file_path": path.to_str().unwrap()});
        let rt = crate::runtime::LocalRuntime::new();
        let result = tool.execute(args, &rt).await.unwrap();
        // 不传 offset/limit，仍返回全文且带行号
        assert!(result.contains("1\thello"), "got: {result}");
        assert!(result.contains("2\tworld"));
        // 全文读不附加范围提示
        assert!(!result.contains("showing lines"));
        let _ = std::fs::remove_file(&path);
    }
}

#[cfg(test)]
mod tests_advanced_calc {
    use super::*;

    #[tokio::test]
    async fn test_sin() {
        let tool = CalculatorAdvancedTool;
        let rt = crate::runtime::LocalRuntime::new();
        let args = serde_json::json!({"operation":"sin","value":0.0});
        let result = tool.execute(args, &rt).await.unwrap();
        assert!(result.contains("0.000000"), "sin(0) should be 0, got {}", result);
    }

    #[tokio::test]
    async fn test_sqrt() {
        let tool = CalculatorAdvancedTool;
        let rt = crate::runtime::LocalRuntime::new();
        let args = serde_json::json!({"operation":"sqrt","value":9.0});
        let result = tool.execute(args, &rt).await.unwrap();
        assert_eq!(result, "3.000000", "sqrt(9) should be 3, got {}", result);
    }

    #[tokio::test]
    async fn test_log() {
        let tool = CalculatorAdvancedTool;
        let rt = crate::runtime::LocalRuntime::new();
        let args = serde_json::json!({"operation":"log","value":100.0});
        let result = tool.execute(args, &rt).await.unwrap();
        assert_eq!(result, "2.000000", "log10(100) should be 2, got {}", result);
    }

    #[tokio::test]
    async fn test_invalid_operation() {
        let tool = CalculatorAdvancedTool;
        let rt = crate::runtime::LocalRuntime::new();
        let args = serde_json::json!({"operation":"invalid","value":1.0});
        let result = tool.execute(args, &rt).await;
        assert!(result.is_err(), "invalid operation should return Err");
    }

    #[tokio::test]
    async fn test_cos() {
        let tool = CalculatorAdvancedTool;
        let rt = crate::runtime::LocalRuntime::new();
        let args = serde_json::json!({"operation":"cos","value":0.0});
        let result = tool.execute(args, &rt).await.unwrap();
        assert!(result.contains("1.000000"), "cos(0) should be 1, got {}", result);
    }

    #[tokio::test]
    async fn test_tan() {
        let tool = CalculatorAdvancedTool;
        let rt = crate::runtime::LocalRuntime::new();
        let args = serde_json::json!({"operation":"tan","value":0.0});
        let result = tool.execute(args, &rt).await.unwrap();
        assert!(result.contains("0.000000"), "tan(0) should be 0, got {}", result);
    }

    #[tokio::test]
    async fn test_ln() {
        let tool = CalculatorAdvancedTool;
        let rt = crate::runtime::LocalRuntime::new();
        let args = serde_json::json!({"operation":"ln","value":1.0});
        let result = tool.execute(args, &rt).await.unwrap();
        assert!(result.contains("0.000000"), "ln(1) should be 0, got {}", result);
    }

    #[tokio::test]
    async fn test_cbrt() {
        let tool = CalculatorAdvancedTool;
        let rt = crate::runtime::LocalRuntime::new();
        let args = serde_json::json!({"operation":"cbrt","value":8.0});
        let result = tool.execute(args, &rt).await.unwrap();
        assert!(result.contains("2.000000"), "cbrt(8) should be 2, got {}", result);
    }

    #[tokio::test]
    async fn test_uuid_generator() {
        let tool = UuidGeneratorTool;
        let rt = crate::runtime::LocalRuntime::new();
        let result = tool.execute(serde_json::json!({}), &rt).await.unwrap();
        assert_eq!(result.len(), 36, "UUID v4 should be 36 chars");
        assert_eq!(result.matches('-').count(), 4, "UUID v4 has 4 dashes");
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
                "model": {
                    "type": "string",
                    "description": "Model id for the spawned worker (e.g. 'deepseek-v4-flash', 'glm-4.6'). If omitted, inherits parent's model. Lets you use different models for different workers."
                },
                "provider": {
                    "type": "string",
                    "description": "Provider name (e.g. 'opencode', 'zhipuai'). Required when model is set to a non-default provider."
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
        // 可选 model/provider：让 LLM 能给不同 worker 指定不同模型
        let model = args.get("model").and_then(|v| v.as_str()).map(String::from);
        let provider = args.get("provider").and_then(|v| v.as_str()).map(String::from);

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
            hook_depth: None,  // LLM 的 spawn_worker 不设（只有 hooks agent handler 才设）
            system_prompt_override: None,  // 普通 spawn_worker 不覆盖
            model,
            provider,
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
                crate::runtime::SpawnRelation::System => "system",
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

// ---------------------------------------------------------------------------
// Skill tool — let the LLM load a skill on demand (inject mode)
// ---------------------------------------------------------------------------

/// Skill 工具 — 让 LLM 按需加载 skill（inject 模式）
///
/// 对齐 pi 的 `core/tools/skill.ts`。扫描 `skill_dirs` 下的 `.md` 文件，
/// 文件名（不含 .md）即 skill 名。`inject` 模式返回 skill 正文作为工具结果，
/// LLM 下一轮即可看到。`fork` 模式（隔离 subtask）尚未实现，返回提示文本。
pub struct SkillTool {
    /// skill 根目录（全局 `~/.ion/agent/skills/` + 项目级 `<project>/.ion/skills/`）
    pub skill_dirs: Vec<PathBuf>,
}

impl SkillTool {
    /// 列出所有可用 skill（扫描 skill_dirs 下的 .md 文件 + 子目录/SKILL.md）
    fn list_skills(&self) -> String {
        let mut entries: Vec<(String, String, String, Option<String>)> = Vec::new(); // (name, source, description, context_mode)
        for dir in &self.skill_dirs {
            let source = dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("skills");
            let Ok(read) = std::fs::read_dir(dir) else { continue };
            for entry in read.flatten() {
                let path = entry.path();

                // 格式 1：<dir>/<name>.md（ION 格式，平铺 .md 文件）
                if path.is_file() {
                    let Some(fname) = path.file_name().and_then(|n| n.to_str()) else { continue };
                    if !fname.ends_with(".md") { continue; }
                    let name = fname.trim_end_matches(".md").to_string();
                    let content = std::fs::read_to_string(&path).unwrap_or_default();
                    let desc = parse_skill_description(&content);
                    let mode = parse_skill_context_mode(&content);
                    entries.push((name, source.to_string(), desc, mode));
                }
                // 格式 2：<dir>/<name>/SKILL.md（~/.agents/skills/ 格式，目录形式）
                else if path.is_dir() {
                    let skill_md = path.join("SKILL.md");
                    if skill_md.is_file() {
                        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
                        let clean_name = strip_version_suffix(name);
                        let content = std::fs::read_to_string(&skill_md).unwrap_or_default();
                        let desc = parse_skill_description(&content);
                        let mode = parse_skill_context_mode(&content);
                        entries.push((clean_name, source.to_string(), desc, mode));
                    }
                }
            }
        }

        if entries.is_empty() {
            return "No skills available.".to_string();
        }

        // 按名字去重（全局 + 项目可能同名，保留先出现的）
        entries.dedup_by(|a, b| a.0 == b.0);

        let mut out = String::from("Available skills:\n");
        for (name, source, desc, mode) in &entries {
            let mode_tag = match mode {
                Some(m) => format!(" (推荐:{})", m),
                None => String::new(),
            };
            if desc.is_empty() {
                out.push_str(&format!("  - {name}{mode_tag} [{source}]\n"));
            } else {
                out.push_str(&format!("  - {name}{mode_tag} [{source}]: {desc}\n"));
            }
        }
        out.push_str("\nUse skill_name='<name>' to load a skill.");
        out
    }

    /// 按名字查找 skill 文件，返回其路径
    /// 支持两种格式：
    /// 1. <dir>/<name>.md（ION 格式）
    /// 2. <dir>/<name>/SKILL.md（~/.agents/skills/ 格式，可能有版本后缀）
    fn find_skill(&self, name: &str) -> Option<PathBuf> {
        for dir in &self.skill_dirs {
            // 格式 1：平铺 .md
            let candidate = dir.join(format!("{name}.md"));
            if candidate.is_file() {
                return Some(candidate);
            }
            // 格式 2：目录/SKILL.md（精确名字匹配）
            let dir_candidate = dir.join(name).join("SKILL.md");
            if dir_candidate.is_file() {
                return Some(dir_candidate);
            }
            // 格式 2 变体：目录有版本后缀（如 "code-review-excellence-0.1.0"）
            // 用户传 "code-review-excellence"，匹配带后缀的目录
            if let Ok(read) = std::fs::read_dir(dir) {
                for entry in read.flatten() {
                    let path = entry.path();
                    if !path.is_dir() { continue; }
                    let Some(dir_name) = path.file_name().and_then(|n| n.to_str()) else { continue };
                    // 去掉版本后缀后是否匹配
                    let clean = strip_version_suffix(dir_name);
                    if clean == name {
                        let skill_md = path.join("SKILL.md");
                        if skill_md.is_file() {
                            return Some(skill_md);
                        }
                    }
                }
            }
        }
        None
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        "skill"
    }

    fn description(&self) -> &str {
        "Load a specialized skill by name. Skills encode domain-specific workflows (code-audit, deployment, testing, etc.) that guide multi-step tasks. \
         IMPORTANT: When the user's request matches an available skill, you SHOULD call this tool FIRST instead of manually using bash/read/write — the skill provides a proven workflow that's better than ad-hoc tool use.\n\
         Two modes:\n\
         - inject (default): load skill instructions into your current context, then execute the workflow yourself with bash/read/write/etc.\n\
         - fork: spawn an isolated sub-worker to execute the skill (keeps your main context clean). Use fork for long-running or complex skills.\n\
         Pass skill_name='list' to see all available skills with their descriptions."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "skill_name": {
                    "type": "string",
                    "description": "Name of the skill to load (e.g., 'code-audit', 'deployment'). Use 'list' to see all available skills."
                },
                "context": {
                    "type": "string",
                    "enum": ["inject", "fork"],
                    "description": "How to apply the skill. 'inject' = load instructions into current context (default). 'fork' = run in isolated sub-worker (keeps main context clean, for complex/long tasks)."
                },
                "user_request": {
                    "type": "string",
                    "description": "Required for fork mode: the user's original request/goal. This will be passed to the sub-worker as its task, so it knows what specifically to accomplish (not just 'run the skill')."
                }
            },
            "required": ["skill_name"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        rt: &dyn crate::runtime::Runtime,
    ) -> AgentResult<String> {
        let name = args
            .get("skill_name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentError::Tool("missing 'skill_name'".into()))?;
        // context 模式优先级：LLM 显式传的 > skill frontmatter 声明的 > 默认 inject
        let context_mode = {
            let from_args = args.get("context").and_then(|v| v.as_str());
            if from_args.is_some() {
                from_args.unwrap().to_string()
            } else {
                // LLM 没传 context——看 skill 声明的推荐模式
                if let Some(skill_path) = self.find_skill(name) {
                    if let Ok(content) = std::fs::read_to_string(&skill_path) {
                        parse_skill_context_mode(&content).unwrap_or_else(|| "inject".to_string())
                    } else {
                        "inject".to_string()
                    }
                } else {
                    "inject".to_string()
                }
            }
        };

        // fork 模式：读 skill 内容 → spawn_worker 起子任务 → skill 注入 system prompt（不被压缩）
        if context_mode == "fork" {
            // 查找 skill 文件
            let skill_path = self.find_skill(name).ok_or_else(|| {
                AgentError::Tool(format!("skill '{name}' not found in {:?}", self.skill_dirs))
            })?;
            let content = std::fs::read_to_string(&skill_path)?;
            let (_frontmatter, body) = parse_skill_content(&content);

            // 构造 system prompt：skill 内容 + 角色说明
            // system prompt 不参与 compaction，skill 内容在整个执行过程中不会丢失
            let system_prompt = format!(
                "You are executing a skill. Follow the instructions below precisely.\n\n\
                 ---\nSkill: {name}\n---\n\n{body}"
            );

            // spawn 子 Worker 执行 skill
            // 用户的具体需求（fork 模式必须传，让子 Worker 知道要干什么）
            let user_request = args
                .get("user_request")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();

            let task = if !user_request.is_empty() {
                // 用户传了具体需求——把需求 + skill 指引组合成 task
                format!(
                    "User request: {user_request}\n\n\
                     Follow the skill instructions in your system prompt to accomplish this. \
                     Do NOT call the skill tool (it's already loaded). \
                     Use the available tools (read/write/bash/etc) to do the actual work. \
                     When done, summarize what you accomplished."
                )
            } else {
                // 没传需求——通用 task（兜底）
                format!(
                    "Follow the skill instructions that are already in your system prompt. \
                     Do NOT call the skill tool (it's already loaded). \
                     Just execute the workflow described in your system prompt using \
                     the available tools (read/write/bash/etc). \
                     When done, summarize what you accomplished."
                )
            };

            let req = crate::runtime::SpawnWorkerRequest {
                relation: crate::runtime::SpawnRelation::Child,
                agent: "default".into(),
                task,
                name: Some(format!("skill-{name}")),
                report_channel: None,
                wait: true,  // 同步等结果
                worktree: None,
                hook_depth: None,
                system_prompt_override: Some(system_prompt),
                model: None,
                provider: None,
            };

            let resp = match rt.spawn_worker(req).await {
                Ok(resp) => resp,
                Err(e) => {
                    // fork 失败时给 LLM 明确的 fallback 指引：
                    // 场景 1（LocalRuntime）不支持 spawn_worker，LLM 应改用 inject 模式
                    return Ok(format!(
                        "Skill '{name}' fork mode failed: {e}\n\n\
                         Fork mode requires a host engine (use `ion --host` or `ion serve`).\n\
                         Falling back: please retry with `context=inject` to load the skill\n\
                         into the current context instead."
                    ));
                }
            };

            let output = resp.first_turn_output.unwrap_or_default();
            // fork 返回值：drain 收集了子 Worker 所有轮次的 text_delta 拼接。
            // 我们只返回最后一段有意义的总结（最后一个段落），不是全部拼接。
            // 方法：找最后一次出现的 "## Summary" 或 "### " 或 "confirmed" 等总结标志，
            // 从那里截取到末尾。如果没有标志，返回全部（兜底）。
            let final_summary = extract_final_summary(&output);
            return Ok(format!("Skill '{name}' executed in fork mode:\n\n{final_summary}"));
        }

        // list 模式：列出可用 skill
        if name == "list" {
            return Ok(self.list_skills());
        }

        // 查找 skill 文件
        let skill_path = self.find_skill(name).ok_or_else(|| {
            AgentError::Tool(format!("skill '{name}' not found in {:?}", self.skill_dirs))
        })?;
        let content = std::fs::read_to_string(&skill_path)?;

        // 解析 frontmatter + body
        let (_frontmatter, body) = parse_skill_content(&content);

        // inject 模式：返回 skill 正文（agent loop 把它当作工具结果，LLM 下一轮可见）
        Ok(format!("Skill '{name}' loaded:\n\n{body}"))
    }
}

/// 从 fork 子 Worker 的完整输出（多轮 text_delta 拼接）中提取最终总结。
///
/// drain 收集了所有轮次的 text，但只有最后一轮是"最终总结"。
/// 我们找最后一个总结标志（"## Summary" / "verified as complete" / "confirmed"），
/// 从那里截取到末尾。如果没有找到标志，返回最后 500 字符（兜底）。
fn extract_final_summary(full_output: &str) -> String {
    // 常见总结标志（按优先级排序）
    let markers = [
        "verified as complete",
        "has been **verified",
        "The report is ready",
        "## Summary",
        "### Summary",
        "## ✅",
        "confirmed",
        "What was accomplished",
    ];

    // 找最后一个出现的标志
    let mut best_pos: Option<usize> = None;
    for marker in &markers {
        if let Some(pos) = full_output.rfind(marker) {
            match best_pos {
                Some(bp) if bp > pos => {} // 已有更靠后的
                _ => best_pos = Some(pos),
            }
        }
    }

    if let Some(pos) = best_pos {
        // 从标志位置往前找段落开头（最近的 \n\n）
        let before = &full_output[..pos];
        let start = before.rfind("\n\n").map(|p| p + 2).unwrap_or(pos);
        full_output[start..].trim().to_string()
    } else {
        // 兜底：返回最后 500 字符（安全处理 UTF-8 边界）
        let len = full_output.len();
        if len > 500 {
            // 从 len - 500 开始，往前调整到字符边界（UTF-8 每个字符 1-4 字节）
            let mut start = len - 500;
            // UTF-8 后续字节以 10xxxxxx 开头（0x80..0xC0），跳过它们
            while start < len && !full_output.is_char_boundary(start) {
                start += 1;
            }
            // 从 start 找下一个换行（避免截断行中间）
            let start = full_output[start..].find('\n').map(|p| start + p + 1).unwrap_or(start);
            full_output[start..].trim().to_string()
        } else {
            full_output.trim().to_string()
        }
    }
}

/// 去掉 skill 目录名的版本后缀。
/// 如 "code-review-excellence-0.1.0" → "code-review-excellence"
/// "debug-pro-1.0.0" → "debug-pro"
fn strip_version_suffix(name: &str) -> String {
    // 匹配末尾的 -<version>（如 -0.1.0, -1.0.0, -1）
    if let Some(pos) = name.rfind('-') {
        let suffix = &name[pos + 1..];
        // 版本号格式：纯数字 + 点（如 0.1.0, 1.0.0, 1）
        if !suffix.is_empty()
            && suffix.chars().all(|c| c.is_ascii_digit() || c == '.')
        {
            return name[..pos].to_string();
        }
    }
    name.to_string()
}

/// 解析 skill 文件：返回 (frontmatter 原始文本, body 正文)
///
/// 支持 YAML frontmatter（`---` 包裹）。如果没有 frontmatter，frontmatter 为空，
/// body 为全部内容。
fn parse_skill_content(content: &str) -> (String, String) {
    let trimmed = content.trim_start();
    if let Some(rest) = trimmed.strip_prefix("---") {
        // 找闭合的 ---
        if let Some(end) = rest.find("\n---") {
            let frontmatter = rest[..end].to_string();
            // 跳过闭合的 "---" 和它后面的换行
            let after = &rest[end + 4..];
            let body = after.trim_start_matches('\n').to_string();
            return (frontmatter, body);
        }
    }
    (String::new(), content.to_string())
}

/// 从 frontmatter 提取 description 字段（用于 list 输出）
fn parse_skill_description(content: &str) -> String {
    let (frontmatter, _) = parse_skill_content(content);
    for line in frontmatter.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("description:") {
            let val = rest.trim().trim_matches(|c| c == '"' || c == '\'');
            if !val.is_empty() {
                return val.to_string();
            }
        }
    }
    String::new()
}

/// 从 frontmatter 提取推荐 context 模式（inject / fork）
/// skill 可以在 frontmatter 里声明：
///   context: fork    ← 这个 skill 推荐用 fork 模式（适合复杂/长任务）
///   context: inject  ← 这个 skill 推荐用 inject 模式（默认）
/// 如果没声明，返回 None（LLM 自己决定）
fn parse_skill_context_mode(content: &str) -> Option<String> {
    let (frontmatter, _) = parse_skill_content(content);
    for line in frontmatter.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("context:") {
            let val = rest.trim().trim_matches(|c| c == '"' || c == '\'');
            match val {
                "fork" => return Some("fork".to_string()),
                "inject" => return Some("inject".to_string()),
                _ => {}
            }
        }
    }
    None
}

#[cfg(test)]
mod skill_tests {
    use super::*;

    #[test]
    fn test_parse_skill_content_with_frontmatter() {
        let content = "---\nname: test\ndescription: A test skill\n---\n# Test\nDo the thing.";
        let (fm, body) = parse_skill_content(content);
        assert!(fm.contains("name: test"));
        assert!(fm.contains("description: A test skill"));
        assert!(body.starts_with("# Test"));
        assert!(body.contains("Do the thing."));
    }

    #[test]
    fn test_parse_skill_content_without_frontmatter() {
        let content = "# Plain Skill\nJust text.";
        let (fm, body) = parse_skill_content(content);
        assert!(fm.is_empty());
        assert_eq!(body, content);
    }

    #[test]
    fn test_parse_skill_description() {
        let content = "---\nname: review\ndescription: Code review guidance\n---\nbody";
        assert_eq!(parse_skill_description(content), "Code review guidance");
    }

    #[test]
    fn test_parse_skill_description_missing() {
        let content = "# No frontmatter here";
        assert_eq!(parse_skill_description(content), "");
    }

    #[test]
    fn test_skill_tool_list_empty() {
        let tool = SkillTool {
            skill_dirs: vec![PathBuf::from("/nonexistent/path")],
        };
        let out = tool.list_skills();
        assert!(out.contains("No skills available"));
    }

    #[test]
    fn test_skill_tool_find_not_found() {
        let tool = SkillTool {
            skill_dirs: vec![PathBuf::from("/nonexistent/path")],
        };
        assert!(tool.find_skill("ghost").is_none());
    }
}

// ---------------------------------------------------------------------------
// RandomNumber tool — generate a random number in [0, max)
// ---------------------------------------------------------------------------

pub struct RandomNumberTool;

#[async_trait]
impl Tool for RandomNumberTool {
    fn name(&self) -> &str {
        "random"
    }

    fn description(&self) -> &str {
        "Generate a random number in [0, max). Args: max (number, default 100)."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "max": {
                    "type": "number",
                    "description": "Upper bound (exclusive), default 100"
                }
            },
            "required": []
        })
    }

    async fn execute(&self, args: serde_json::Value, _rt: &dyn crate::runtime::Runtime) -> AgentResult<String> {
        let max: u32 = args
            .get("max")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32)
            .unwrap_or(100);

        if max == 0 {
            return Err(AgentError::Tool("max must be > 0".into()));
        }

        let val = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos()) % max;
        Ok(val.to_string())
    }
}

#[cfg(test)]
mod tests_random_number {
    use super::*;

    #[tokio::test]
    async fn test_random_number_tool() {
        let tool = RandomNumberTool;
        let args = serde_json::json!({"max": 10});
        let result = tool.execute(args, &crate::runtime::LocalRuntime::new()).await.unwrap();
        let num: u32 = result.parse().unwrap();
        assert!(num < 10, "random number {} should be < 10", num);
    }
}
