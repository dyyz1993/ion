//! LSP Extension — cargo check diagnostics integration.
//!
//! Provides compiler error/warning feedback to the LLM after code changes.
//! Uses `cargo check --message-format=json` (simplified LSP, no rust-analyzer).
//!
//! ## How it works
//!
//! 1. Agent calls `write` or `edit` tool → `on_tool_execution_end` sets `dirty=true`
//! 2. Next LLM call → `on_context` runs `cargo check`, parses JSON, injects `<diagnostics>` XML
//! 3. LLM sees errors → fixes code → writes again → cycle repeats
//!
//! ## Config
//!
//! ```json
//! { "extensions": { "lsp": { "enabled": true } } }
//! ```
//!
//! Only active in directories with a `Cargo.toml`.

use crate::agent::error::{AgentError, AgentResult};
use crate::agent::extension::{Extension, ToolExecutionContext};
use crate::agent::tool::Tool;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;

// ── Diagnostic struct ──────────────────────────────────────────

/// A single compiler diagnostic from `cargo check`.
#[derive(Clone, Debug, serde::Serialize, PartialEq)]
pub struct Diagnostic {
    pub file: String,
    pub line: u32,
    pub column: u32,
    pub severity: String, // "error" | "warning"
    pub message: String,
    pub code: String, // "E0308" | "unused_variables" etc
}

// ── LspExtension ──────────────────────────────────────────────

/// LSP Extension — cargo check diagnostics integration.
///
/// Registered as a worker-level extension (not singleton). Each worker
/// gets its own LspExtension instance so diagnostics are scoped to
/// that worker's project directory.
pub struct LspExtension {
    /// Current diagnostics (shared with LspCheckTool).
    diagnostics: Arc<Mutex<Vec<Diagnostic>>>,
    /// True = files changed, need re-check before next context injection.
    dirty: Arc<AtomicBool>,
    /// True = last cargo check had errors.
    has_errors: Arc<AtomicBool>,
    /// Project root (where Cargo.toml is). None = not a Rust project.
    project_root: Arc<Mutex<Option<String>>>,
    name: String,
}

impl LspExtension {
    pub fn new() -> Self {
        Self {
            diagnostics: Arc::new(Mutex::new(Vec::new())),
            dirty: Arc::new(AtomicBool::new(false)),
            has_errors: Arc::new(AtomicBool::new(false)),
            project_root: Arc::new(Mutex::new(None)),
            name: "lsp".into(),
        }
    }

    /// Get shared diagnostics handle (for LspCheckTool registration).
    pub fn get_shared_diagnostics(&self) -> Arc<Mutex<Vec<Diagnostic>>> {
        Arc::clone(&self.diagnostics)
    }

    /// Get shared dirty flag (for LspCheckTool registration).
    pub fn get_shared_dirty(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.dirty)
    }

    /// Get shared has_errors flag (for LspCheckTool registration).
    pub fn get_shared_has_errors(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.has_errors)
    }

    /// Create an LspExtension with shared diagnostics (for LspCheckTool).
    pub fn new_with_shared(
        diagnostics: Arc<Mutex<Vec<Diagnostic>>>,
        dirty: Arc<AtomicBool>,
        has_errors: Arc<AtomicBool>,
    ) -> Self {
        Self {
            diagnostics,
            dirty,
            has_errors,
            project_root: Arc::new(Mutex::new(None)),
            name: "lsp".into(),
        }
    }

    /// Check if Cargo.toml exists in the current directory or parents.
    async fn detect_project_root(&self) -> Option<String> {
        // Check cache first
        {
            let cached = self.project_root.lock().await;
            if cached.is_some() {
                return cached.clone();
            }
        }

        // Walk up from cwd looking for Cargo.toml
        let cwd = std::env::current_dir().ok()?;
        let mut dir = cwd.as_path();
        loop {
            if dir.join("Cargo.toml").exists() {
                let root = dir.to_string_lossy().to_string();
                *self.project_root.lock().await = Some(root.clone());
                return Some(root);
            }
            dir = dir.parent()?;
        }
    }

    /// Run `cargo check --message-format=json` and parse diagnostics.
    async fn run_cargo_check(&self) -> Result<Vec<Diagnostic>, String> {
        let timeout_secs = std::env::var("ION_LSP_TIMEOUT")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(120);

        let cmd = "cargo check --message-format=json 2>/dev/null";
        let output = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .output();

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs),
            output,
        )
        .await;

        match result {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                Ok(Self::parse_cargo_check_json(&stdout))
            }
            Ok(Err(e)) => {
                tracing::warn!("[lsp] cargo check failed: {e}");
                Ok(vec![])
            }
            Err(_) => {
                tracing::warn!("[lsp] cargo check timed out after {timeout_secs}s");
                Err("timeout".into())
            }
        }
    }

    /// Parse `cargo check --message-format=json` output.
    /// Each line is a JSON object; we only care about `reason == "compiler-message"`.
    fn parse_cargo_check_json(stdout: &str) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        for line in stdout.lines() {
            if line.trim().is_empty() {
                continue;
            }

            // Parse the JSON line
            let parsed: serde_json::Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => continue,
            };

            // Only care about compiler messages
            if parsed.get("reason").and_then(|v| v.as_str()) != Some("compiler-message") {
                continue;
            }

            let msg = match parsed.get("message") {
                Some(m) => m,
                None => continue,
            };

            let severity = msg
                .get("level")
                .and_then(|v| v.as_str())
                .unwrap_or("warning")
                .to_string();

            let code = msg
                .get("code")
                .and_then(|c| c.get("code"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let message = msg
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            // Get location from first span
            let (file, line_num, col) = msg
                .get("spans")
                .and_then(|s| s.as_array())
                .and_then(|arr| arr.first())
                .map(|span| {
                    let f = span
                        .get("file_name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_string();
                    let l = span
                        .get("line_start")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as u32;
                    let c = span
                        .get("column_start")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as u32;
                    (f, l, c)
                })
                .unwrap_or(("unknown".into(), 0, 0));

            diagnostics.push(Diagnostic {
                file,
                line: line_num,
                column: col,
                severity,
                message,
                code,
            });
        }

        diagnostics
    }

    /// Format diagnostics as XML block for context injection.
    fn format_diagnostics_xml(diagnostics: &[Diagnostic]) -> String {
        if diagnostics.is_empty() {
            return "<diagnostics count=\"0\" status=\"clean\">\nProject compiles cleanly.\n</diagnostics>".into();
        }

        let error_count = diagnostics.iter().filter(|d| d.severity == "error").count();
        let warning_count = diagnostics.len() - error_count;
        let has_errors = error_count > 0;

        let mut xml = format!(
            "<diagnostics count=\"{}\" has_errors=\"{}\">\n",
            diagnostics.len(),
            has_errors
        );

        for d in diagnostics {
            let icon = if d.severity == "error" { "error" } else { "warning" };
            let code_part = if d.code.is_empty() {
                String::new()
            } else {
                format!(" code=\"{}\"", d.code)
            };
            xml.push_str(&format!(
                "<{} file=\"{}\" line=\"{}\" col=\"{}\"{}>\n",
                icon, d.file, d.line, d.column, code_part
            ));
            xml.push_str(&format!("{}\n", d.message));
            xml.push_str(&format!("</{}>\n", icon));
        }

        xml.push_str(&format!(
            "\nSummary: {} issue(s) ({} error(s), {} warning(s))\n",
            diagnostics.len(),
            error_count,
            warning_count
        ));
        xml.push_str("</diagnostics>");
        xml
    }

    /// Format diagnostics as human-readable text (for LspCheckTool).
    fn format_diagnostics_text(diagnostics: &[Diagnostic]) -> String {
        if diagnostics.is_empty() {
            return "✅ No diagnostics. Project compiles cleanly.".into();
        }

        let error_count = diagnostics.iter().filter(|d| d.severity == "error").count();
        let warning_count = diagnostics.len() - error_count;

        let mut text = format!(
            "📋 Diagnostics ({} issue(s)):\n\n",
            diagnostics.len()
        );

        for d in diagnostics {
            let icon = if d.severity == "error" { "🔴" } else { "🟡" };
            let code_part = if d.code.is_empty() {
                String::new()
            } else {
                format!(" [{}]", d.code)
            };
            text.push_str(&format!(
                "{} {}:{}:{}{} {}\n",
                icon, d.file, d.line, d.column, code_part, d.message
            ));
        }

        text.push_str(&format!(
            "\nStatus: {} issue(s) ({} error(s), {} warning(s))",
            diagnostics.len(),
            error_count,
            warning_count
        ));
        text
    }

    /// Do a fresh cargo check and update stored diagnostics.
    async fn do_check(&self) -> Result<Vec<Diagnostic>, String> {
        let has_cargo = self.detect_project_root().await;
        if has_cargo.is_none() {
            tracing::info!("[lsp] no Cargo.toml found, skipping");
            let empty = Vec::new();
            *self.diagnostics.lock().await = empty.clone();
            self.dirty.store(false, Ordering::SeqCst);
            self.has_errors.store(false, Ordering::SeqCst);
            return Ok(empty);
        }

        let result = self.run_cargo_check().await;
        match result {
            Ok(diags) => {
                let has_errs = diags.iter().any(|d| d.severity == "error");
                self.has_errors.store(has_errs, Ordering::SeqCst);
                self.dirty.store(false, Ordering::SeqCst);
                tracing::info!(
                    "[lsp] cargo check completed: {} diagnostics ({} errors)",
                    diags.len(),
                    diags.iter().filter(|d| d.severity == "error").count()
                );
                let mut store = self.diagnostics.lock().await;
                *store = diags.clone();
                Ok(diags)
            }
            Err(e) => {
                tracing::warn!("[lsp] cargo check failed: {e}");
                Err(e)
            }
        }
    }
}

// ── Extension impl ────────────────────────────────────────────

#[async_trait::async_trait]
impl Extension for LspExtension {
    fn name(&self) -> &str {
        &self.name
    }

    /// After write/edit tool completes, mark dirty so next on_context triggers cargo check.
    async fn on_tool_execution_end(&self, ctx: &ToolExecutionContext) -> AgentResult<()> {
        if ctx.tool_name == "write" || ctx.tool_name == "edit" {
            self.dirty.store(true, Ordering::SeqCst);
            tracing::info!("[lsp] {} detected, marking dirty for re-check", ctx.tool_name);
        }
        Ok(())
    }

    /// Inject <diagnostics> XML into messages if dirty.
    async fn on_context(&self, messages: &mut Vec<crate::agent::messages::Message>) -> AgentResult<()> {
        if !self.dirty.load(Ordering::SeqCst) {
            return Ok(());
        }

        // Run cargo check
        let diags = self.do_check().await.unwrap_or_default();

        // Inject diagnostics as a custom message
        let xml = Self::format_diagnostics_xml(&diags);
        use ion_provider::types::{CustomMessage, CustomContent};

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        messages.push(crate::agent::messages::Message::Custom(CustomMessage {
            role: "custom".into(),
            custom_type: "diagnostics".into(),
            content: CustomContent::Text(xml),
            display: true,
            details: Some(serde_json::json!({
                "source": "lsp",
                "count": diags.len(),
                "has_errors": diags.iter().any(|d| d.severity == "error"),
            })),
            timestamp: now as i64,
        }));

        Ok(())
    }

    /// Handle extension_rpc for CLI/debug access.
    async fn on_extension_rpc(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> AgentResult<serde_json::Value> {
        match method {
            "check" => {
                let diags = self.do_check().await.map_err(|e| AgentError::Tool(e))?;
                Ok(serde_json::json!({
                    "count": diags.len(),
                    "has_errors": diags.iter().any(|d| d.severity == "error"),
                    "diagnostics": diags,
                }))
            }
            "status" => {
                let count = self.diagnostics.lock().await.len();
                Ok(serde_json::json!({
                    "enabled": true,
                    "dirty": self.dirty.load(Ordering::SeqCst),
                    "has_errors": self.has_errors.load(Ordering::SeqCst),
                    "diagnostic_count": count,
                }))
            }
            "clear" => {
                *self.diagnostics.lock().await = Vec::new();
                self.dirty.store(false, Ordering::SeqCst);
                self.has_errors.store(false, Ordering::SeqCst);
                Ok(serde_json::json!({"cleared": true}))
            }
            _ => Err(AgentError::Tool(format!("unknown lsp method: {method}"))),
        }
    }
}

// ── LspCheckTool ──────────────────────────────────────────────

/// LLM tool: `lsp_check` — get current project diagnostics.
///
/// The LLM can call this explicitly after writing code to check for errors.
pub struct LspCheckTool {
    diagnostics: Arc<Mutex<Vec<Diagnostic>>>,
    dirty: Arc<AtomicBool>,
    has_errors: Arc<AtomicBool>,
}

impl LspCheckTool {
    pub fn new(
        diagnostics: Arc<Mutex<Vec<Diagnostic>>>,
        dirty: Arc<AtomicBool>,
        has_errors: Arc<AtomicBool>,
    ) -> Self {
        Self {
            diagnostics,
            dirty,
            has_errors,
        }
    }
}

#[async_trait::async_trait]
impl Tool for LspCheckTool {
    fn name(&self) -> &str {
        "lsp_check"
    }

    fn description(&self) -> &str {
        "Get current project diagnostics (compiler errors and warnings). \
         Automatically runs `cargo check` and returns formatted results. \
         Use this after writing or editing Rust code to verify it compiles."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "description": "No parameters needed. Automatically detects Cargo.toml and runs cargo check."
        })
    }

    async fn execute(
        &self,
        _args: serde_json::Value,
        _rt: &dyn crate::runtime::Runtime,
    ) -> AgentResult<String> {
        // Mark dirty to force re-check
        self.dirty.store(true, Ordering::SeqCst);

        // Run cargo check via a temporary LspExtension-like path
        // (We reuse the extension's logic by creating a temp extension)
        let ext = LspExtension::new_with_shared(
            Arc::clone(&self.diagnostics),
            Arc::clone(&self.dirty),
            Arc::clone(&self.has_errors),
        );

        let diags = ext.do_check().await.map_err(|e| AgentError::Tool(e))?;
        Ok(LspExtension::format_diagnostics_text(&diags))
    }
}

// ── Tests ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_no_output() {
        let diags = LspExtension::parse_cargo_check_json("");
        assert!(diags.is_empty());
    }

    #[test]
    fn test_parse_non_json_lines() {
        let input = "Compiling foo v0.1.0\n    Finished dev profile\n";
        let diags = LspExtension::parse_cargo_check_json(input);
        assert!(diags.is_empty());
    }

    #[test]
    fn test_parse_one_error() {
        let input = r#"{"reason":"compiler-message","message":{"level":"error","code":{"code":"E0308"},"message":"mismatched types","spans":[{"file_name":"src/lib.rs","line_start":42,"column_start":5}]}}"#;
        let diags = LspExtension::parse_cargo_check_json(input);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].severity, "error");
        assert_eq!(diags[0].file, "src/lib.rs");
        assert_eq!(diags[0].line, 42);
        assert_eq!(diags[0].column, 5);
        assert_eq!(diags[0].code, "E0308");
        assert_eq!(diags[0].message, "mismatched types");
    }

    #[test]
    fn test_parse_one_warning() {
        let input = r#"{"reason":"compiler-message","message":{"level":"warning","code":{"code":"unused_variables"},"message":"unused variable: `x`","spans":[{"file_name":"src/main.rs","line_start":10,"column_start":9}]}}"#;
        let diags = LspExtension::parse_cargo_check_json(input);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].severity, "warning");
        assert_eq!(diags[0].code, "unused_variables");
    }

    #[test]
    fn test_parse_mixed() {
        let input = r#"{"reason":"compiler-artifact","package_id":"foo"}
{"reason":"compiler-message","message":{"level":"error","code":{"code":"E0308"},"message":"type mismatch","spans":[{"file_name":"src/a.rs","line_start":1,"column_start":1}]}}
{"reason":"compiler-message","message":{"level":"warning","code":{"code":"dead_code"},"message":"dead code","spans":[{"file_name":"src/b.rs","line_start":2,"column_start":1}]}}
{"reason":"build-script-executed","package_id":"foo"}"#;
        let diags = LspExtension::parse_cargo_check_json(input);
        assert_eq!(diags.len(), 2);
        assert_eq!(diags[0].severity, "error");
        assert_eq!(diags[1].severity, "warning");
    }

    #[test]
    fn test_parse_no_spans() {
        let input = r#"{"reason":"compiler-message","message":{"level":"error","code":{"code":"E0001"},"message":"error without span","spans":[]}}"#;
        let diags = LspExtension::parse_cargo_check_json(input);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].file, "unknown");
        assert_eq!(diags[0].line, 0);
    }

    #[test]
    fn test_format_empty() {
        let xml = LspExtension::format_diagnostics_xml(&[]);
        assert!(xml.contains("count=\"0\""));
        assert!(xml.contains("clean"));
    }

    #[test]
    fn test_format_with_errors() {
        let diags = vec![
            Diagnostic {
                file: "src/lib.rs".into(),
                line: 42,
                column: 5,
                severity: "error".into(),
                message: "type mismatch".into(),
                code: "E0308".into(),
            },
            Diagnostic {
                file: "src/main.rs".into(),
                line: 10,
                column: 1,
                severity: "warning".into(),
                message: "unused variable".into(),
                code: "unused_variables".into(),
            },
        ];
        let xml = LspExtension::format_diagnostics_xml(&diags);
        assert!(xml.contains("count=\"2\""));
        assert!(xml.contains("has_errors=\"true\""));
        assert!(xml.contains("<error file=\"src/lib.rs\""));
        assert!(xml.contains("<warning file=\"src/main.rs\""));
        assert!(xml.contains("E0308"));
        assert!(xml.contains("type mismatch"));
    }

    #[test]
    fn test_format_text_empty() {
        let text = LspExtension::format_diagnostics_text(&[]);
        assert!(text.contains("✅"));
        assert!(text.contains("compiles cleanly"));
    }

    #[test]
    fn test_format_text_with_errors() {
        let diags = vec![Diagnostic {
            file: "src/lib.rs".into(),
            line: 42,
            column: 5,
            severity: "error".into(),
            message: "type mismatch".into(),
            code: "E0308".into(),
        }];
        let text = LspExtension::format_diagnostics_text(&diags);
        assert!(text.contains("🔴"));
        assert!(text.contains("src/lib.rs:42:5"));
        assert!(text.contains("[E0308]"));
        assert!(text.contains("type mismatch"));
    }

    #[test]
    fn test_diagnostic_equality() {
        let a = Diagnostic {
            file: "src/lib.rs".into(),
            line: 1,
            column: 1,
            severity: "error".into(),
            message: "test".into(),
            code: "E0001".into(),
        };
        let b = a.clone();
        assert_eq!(a, b);
    }
}
