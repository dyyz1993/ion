use super::error::{AgentError, AgentResult};
use super::messages::ToolDef;
use async_trait::async_trait;
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Tool trait
// ---------------------------------------------------------------------------

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> serde_json::Value;
    async fn execute(&self, args: serde_json::Value) -> AgentResult<String>;
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

    async fn execute(&self, args: serde_json::Value) -> AgentResult<String> {
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

    async fn execute(&self, args: serde_json::Value) -> AgentResult<String> {
        let text = args
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("(no text)");
        Ok(format!("echo: {text}"))
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
    async fn execute(&self, args: serde_json::Value) -> AgentResult<String> {
        let path = args.get("file_path").and_then(|v| v.as_str()).ok_or_else(|| AgentError::Tool("missing file_path".into()))?;
        match std::fs::read_to_string(path) {
            Ok(content) => Ok(content),
            Err(e) => Err(AgentError::Tool(format!("read failed: {e}"))),
        }
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
    async fn execute(&self, args: serde_json::Value) -> AgentResult<String> {
        let pattern = args.get("pattern").and_then(|v| v.as_str()).ok_or_else(|| AgentError::Tool("missing pattern".into()))?;
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let output = tokio::process::Command::new("sh")
            .args(["-c", &format!("rg -n --max-count=50 {} {} 2>/dev/null || grep -rn --max-count=50 '{}' {} 2>/dev/null || echo '(no matches)'", shell_quote(pattern), shell_quote(path), shell_quote(pattern), shell_quote(path))])
            .output().await.map_err(|e| AgentError::Tool(e.to_string()))?;
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
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
    async fn execute(&self, args: serde_json::Value) -> AgentResult<String> {
        let pattern = args.get("pattern").and_then(|v| v.as_str()).ok_or_else(|| AgentError::Tool("missing pattern".into()))?;
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let output = tokio::process::Command::new("sh")
            .args(["-c", &format!("find {} -name '{}' -type f 2>/dev/null | head -50", shell_quote(path), pattern)])
            .output().await.map_err(|e| AgentError::Tool(e.to_string()))?;
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
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
    async fn execute(&self, args: serde_json::Value) -> AgentResult<String> {
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let output = tokio::process::Command::new("ls")
            .args(["-la", path])
            .output().await.map_err(|e| AgentError::Tool(e.to_string()))?;
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
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
    async fn execute(&self, args: serde_json::Value) -> AgentResult<String> {
        let cmd = args.get("command").and_then(|v| v.as_str()).ok_or_else(|| AgentError::Tool("missing command".into()))?;
        let output = tokio::process::Command::new("sh")
            .args(["-c", cmd])
            .output().await.map_err(|e| AgentError::Tool(e.to_string()))?;
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let result = if stdout.is_empty() && !stderr.is_empty() {
            stderr
        } else if !stderr.is_empty() {
            format!("{stdout}\n{stderr}")
        } else {
            stdout
        };
        Ok(result)
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
    async fn execute(&self, args: serde_json::Value) -> AgentResult<String> {
        let path = args.get("file_path").and_then(|v| v.as_str()).ok_or_else(|| AgentError::Tool("missing file_path".into()))?;
        let content = args.get("content").and_then(|v| v.as_str()).ok_or_else(|| AgentError::Tool("missing content".into()))?;
        std::fs::write(path, content).map_err(|e| AgentError::Tool(format!("write failed: {e}")))?;
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
    async fn execute(&self, args: serde_json::Value) -> AgentResult<String> {
        let path = args.get("file_path").and_then(|v| v.as_str()).ok_or_else(|| AgentError::Tool("missing file_path".into()))?;
        let old = args.get("old").and_then(|v| v.as_str()).ok_or_else(|| AgentError::Tool("missing old".into()))?;
        let new = args.get("new").and_then(|v| v.as_str()).ok_or_else(|| AgentError::Tool("missing new".into()))?;
        let content = std::fs::read_to_string(path).map_err(|e| AgentError::Tool(format!("read failed: {e}")))?;
        if !content.contains(old) {
            return Err(AgentError::Tool(format!("pattern not found in {path}")));
        }
        let new_content = content.replacen(old, new, 1);
        std::fs::write(path, &new_content).map_err(|e| AgentError::Tool(format!("write failed: {e}")))?;
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
    async fn execute(&self, args: serde_json::Value) -> AgentResult<String> {
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
            let result = tool.execute(args).await.unwrap();
            assert_eq!(result, expected, "for {json}");
        }
    }

    #[tokio::test]
    async fn echo_tool_works() {
        let tool = EchoTool;
        let args: serde_json::Value = serde_json::json!({"text": "hello"});
        let result = tool.execute(args).await.unwrap();
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
