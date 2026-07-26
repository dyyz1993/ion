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
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
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
    /// Files changed since last check (for incremental priority scanning).
    changed_files: Arc<Mutex<Vec<String>>>,
    /// Consecutive check count (for loop detection — max 10 per session).
    check_count: Arc<AtomicU32>,
    /// Timestamp of last successful check (for cooldown — min 5s between checks).
    last_check_time: Arc<Mutex<Option<std::time::Instant>>>,
    name: String,
}

impl LspExtension {
    pub fn new() -> Self {
        Self {
            diagnostics: Arc::new(Mutex::new(Vec::new())),
            dirty: Arc::new(AtomicBool::new(false)),
            has_errors: Arc::new(AtomicBool::new(false)),
            project_root: Arc::new(Mutex::new(None)),
            changed_files: Arc::new(Mutex::new(Vec::new())),
            check_count: Arc::new(AtomicU32::new(0)),
            last_check_time: Arc::new(Mutex::new(None)),
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
            changed_files: Arc::new(Mutex::new(Vec::new())),
            check_count: Arc::new(AtomicU32::new(0)),
            last_check_time: Arc::new(Mutex::new(None)),
            name: "lsp".into(),
        }
    }

    /// Detect project language by looking for marker files.
    /// Returns (project_root, language, check_command).
    async fn detect_project(&self) -> Option<(String, String, String)> {
        // Check cache first
        {
            let cached = self.project_root.lock().await;
            if let Some(ref root) = *cached {
                // Re-detect language each time (cheap)
                let lang_cmd = Self::detect_language_command(root)?;
                return Some((root.clone(), lang_cmd.0, lang_cmd.1));
            }
        }

        // Walk up from cwd looking for project markers
        let cwd = std::env::current_dir().ok()?;
        let mut dir = cwd.as_path();
        loop {
            // Try each language marker
            if let Some((lang, cmd)) = Self::detect_language_command(&dir.to_string_lossy()) {
                let root = dir.to_string_lossy().to_string();
                *self.project_root.lock().await = Some(root.clone());
                return Some((root, lang, cmd));
            }
            dir = dir.parent()?;
        }
    }

    /// Given a directory, detect the language and return (language_name, check_command).
    /// Checks for marker files in priority order.
    fn detect_language_command(dir: &str) -> Option<(String, String)> {
        let path = std::path::Path::new(dir);

        // Rust: Cargo.toml
        if path.join("Cargo.toml").exists() {
            return Some(("rust".into(),
                "cargo check --message-format=json 2>/dev/null".into()));
        }

        // TypeScript/JavaScript: package.json + tsconfig.json or .ts files
        if path.join("package.json").exists() || path.join("tsconfig.json").exists() {
            // Prefer tsc if available, fall back to npx tsc
            let cmd = if std::process::Command::new("which")
                .arg("tsc").output()
                .map(|o| o.status.success())
                .unwrap_or(false)
            {
                "tsc --noEmit --pretty false 2>&1"
            } else {
                "npx --yes tsc --noEmit --pretty false 2>&1"
            };
            return Some(("typescript".into(), cmd.into()));
        }

        // Python: setup.py / pyproject.toml / requirements.txt / .py files
        if path.join("pyproject.toml").exists()
            || path.join("setup.py").exists()
            || path.join("requirements.txt").exists()
        {
            // Use py_compile for syntax check (always available in Python 3)
            // Or ruff if available for linting
            let cmd = if std::process::Command::new("which")
                .arg("ruff").output()
                .map(|o| o.status.success())
                .unwrap_or(false)
            {
                "ruff check --output-format=json . 2>/dev/null"
            } else {
                "python3 -m py_compile $(find . -name '*.py' -not -path './venv/*' -not -path './.venv/*' 2>/dev/null) 2>&1"
            };
            return Some(("python".into(), cmd.into()));
        }

        // Go: go.mod
        if path.join("go.mod").exists() {
            return Some(("go".into(),
                "go vet ./... 2>&1".into()));
        }

        // HTML: index.html or *.html files
        if path.join("index.html").exists()
            || std::fs::read_dir(path).ok()
                .map(|entries| entries.filter_map(|e| e.ok())
                    .any(|e| e.path().extension().map(|ext| ext == "html" || ext == "htm").unwrap_or(false)))
                .unwrap_or(false)
        {
            return Some(("html".into(),
                // Use a basic HTML validation: check for unclosed tags
                "python3 -c \"\nimport sys, re, os, json\nerrs = []\nfor f in os.listdir('.'):\n    if not f.endswith(('.html', '.htm')): continue\n    content = open(f).read()\n    opens = re.findall(r'<([a-z]+)[^>]*>', content, re.I)\n    closes = re.findall(r'</([a-z]+)>', content, re.I)\n    from collections import Counter\n    o, c = Counter(opens), Counter(closes)\n    for tag in o:\n        if tag not in ('br','hr','img','input','meta','link','hr','source'):\n            if o[tag] > c.get(tag, 0):\n                errs.append({'file': f, 'line': 1, 'col': 1, 'severity': 'warning', 'message': f'Unclosed <{tag}> tag', 'code': 'html'})\nimport json\nprint(json.dumps([{'reason': 'compiler-message', 'message': {'level': e['severity'], 'code': {'code': e['code']}, 'message': e['message'], 'spans': [{'file_name': e['file'], 'line_start': e['line'], 'column_start': e['col']}]}} for e in errs]))\n\" 2>/dev/null".into()));
        }

        None
    }

    /// Parse generic linter output into Diagnostic format.
    /// Handles both cargo check JSON and simple text output.
    fn parse_linter_output(stdout: &str, language: &str) -> Vec<Diagnostic> {
        match language {
            "rust" => Self::parse_cargo_check_json(stdout),
            "typescript" => Self::parse_tsc_output(stdout),
            "python" => {
                // Try JSON first (ruff), fall back to text parsing (py_compile)
                if stdout.trim().starts_with("[") {
                    Self::parse_ruff_json(stdout)
                } else {
                    Self::parse_py_compile_output(stdout)
                }
            }
            "go" => Self::parse_go_vet_output(stdout),
            "html" => Self::parse_cargo_check_json(stdout), // HTML checker outputs cargo-like JSON
            _ => Vec::new(),
        }
    }

    /// Parse `tsc --noEmit --pretty false` output.
    /// Format: file.ts(line,col): error TS1234: message
    fn parse_tsc_output(stdout: &str) -> Vec<Diagnostic> {
        let mut diags = Vec::new();
        for line in stdout.lines() {
            // Pattern: file.ts(line,col): error TS1234: message
            if let Some(rest) = line.strip_suffix("") {
                if let Some(close_paren) = rest.find("):") {
                    let before = &rest[..close_paren];
                    let after = &rest[close_paren + 2..];

                    // Extract file + line + col from "file.ts(line,col"
                    if let Some(open_paren) = before.rfind('(') {
                        let file = &before[..open_paren];
                        let pos = &before[open_paren + 1..];
                        let parts: Vec<&str> = pos.split(',').collect();
                        let line_num: u32 = parts.first()
                            .and_then(|s| s.trim().parse::<u32>().ok())
                            .unwrap_or(0);
                        let col: u32 = parts.get(1)
                            .and_then(|s| s.trim().parse::<u32>().ok())
                            .unwrap_or(0);

                        let (severity, rest_msg) = if after.starts_with(" error") {
                            ("error", &after[6..])
                        } else if after.starts_with(" warning") {
                            ("warning", &after[9..])
                        } else {
                            ("warning", after)
                        };

                        // Extract code (TS1234)
                        let (code, message) = if let Some(colon) = rest_msg.find(": ") {
                            (rest_msg[..colon].trim().to_string(), rest_msg[colon + 2..].trim().to_string())
                        } else {
                            (String::new(), rest_msg.trim().to_string())
                        };

                        diags.push(Diagnostic {
                            file: file.to_string(),
                            line: line_num,
                            column: col,
                            severity: severity.into(),
                            message,
                            code,
                        });
                    }
                }
            }
        }
        diags
    }

    /// Parse `ruff check --output-format=json` output.
    fn parse_ruff_json(stdout: &str) -> Vec<Diagnostic> {
        let parsed: Vec<serde_json::Value> = serde_json::from_str(stdout).unwrap_or_default();
        parsed.iter().filter_map(|item| {
            let location = item.get("location")?;
            Some(Diagnostic {
                file: item.get("filename").and_then(|v| v.as_str()).unwrap_or("unknown").to_string(),
                line: location.get("row").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                column: location.get("column").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                severity: if item.get("url").map(|u| u.as_str().unwrap_or("").contains("error")).unwrap_or(false) {
                    "error".into()
                } else {
                    "warning".into()
                },
                message: item.get("message").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                code: item.get("code").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            })
        }).collect()
    }

    /// Parse `python3 -m py_compile` output.
    /// Format: File "file.py", line 42\n    code\nSyntaxError: message
    fn parse_py_compile_output(stdout: &str) -> Vec<Diagnostic> {
        let mut diags = Vec::new();
        for line in stdout.lines() {
            if line.starts_with("  File \"") {
                // Extract: File "path", line N
                let start = line.find("\"").map(|i| i + 1).unwrap_or(0);
                let end = line.rfind("\"").unwrap_or(0);
                let file = &line[start..end];
                if let Some(line_pos) = line.find(", line ") {
                    let rest = &line[line_pos + 7..];
                    let line_num = rest.split(',').next().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
                    diags.push(Diagnostic {
                        file: file.to_string(),
                        line: line_num,
                        column: 0,
                        severity: "error".into(),
                        message: "SyntaxError".into(),
                        code: "syntax".into(),
                    });
                }
            } else if line.starts_with("SyntaxError:") || line.starts_with("IndentationError:") {
                // Attach message to last diagnostic
                if let Some(last) = diags.last_mut() {
                    last.message = line.trim().to_string();
                }
            }
        }
        diags
    }

    /// Parse `go vet ./...` output.
    /// Format: ./file.go:42:5: message
    fn parse_go_vet_output(stdout: &str) -> Vec<Diagnostic> {
        let mut diags = Vec::new();
        for line in stdout.lines() {
            // Pattern: ./file.go:line:col: message
            let parts: Vec<&str> = line.splitn(4, ':').collect();
            if parts.len() >= 4 {
                let file = parts[0].trim_start_matches("./").to_string();
                let line_num = parts[1].trim().parse().unwrap_or(0);
                let col = parts[2].trim().parse().unwrap_or(0);
                let message = parts[3].trim().to_string();
                diags.push(Diagnostic {
                    file,
                    line: line_num,
                    column: col,
                    severity: "warning".into(),
                    message,
                    code: "go_vet".into(),
                });
            }
        }
        diags
    }

    /// Run diagnostics check with multi-language support + 3 layers of protection.
    ///
    /// Automatically detects project language (Rust/TS/Python/Go/HTML) and runs
    /// the appropriate linter. Includes:
    /// 1. Changed-file priority sorting
    /// 2. Timeout protection (ION_LSP_TIMEOUT, default 120s)
    /// 3. Loop detection (max 10 checks/session)
    async fn run_cargo_check(&self) -> Result<Vec<Diagnostic>, String> {
        let timeout_secs = std::env::var("ION_LSP_TIMEOUT")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(120);

        // Protection 3: Loop detection
        let count = self.check_count.fetch_add(1, Ordering::SeqCst);
        if count >= 10 {
            tracing::warn!("[lsp] check count {} >= 10, stopping", count);
            return Err("loop_limit_reached".into());
        }

        // Cooldown (min 3s between checks)
        {
            let last = self.last_check_time.lock().await;
            if let Some(t) = *last {
                if t.elapsed() < std::time::Duration::from_secs(3) {
                    return Ok(self.diagnostics.lock().await.clone());
                }
            }
        }
        *self.last_check_time.lock().await = Some(std::time::Instant::now());

        // Detect project language
        let (root, language, check_cmd) = match self.detect_project().await {
            Some(info) => info,
            None => {
                tracing::info!("[lsp] no recognized project found (no Cargo.toml/package.json/pyproject.toml/go.mod/index.html)");
                return Ok(Vec::new());
            }
        };

        tracing::info!("[lsp] detected {} project at {}, running: {}", language, root, check_cmd);

        // Run with timeout
        let output = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&check_cmd)
            .output();

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs),
            output,
        )
        .await;

        let stdout = match result {
            Ok(Ok(output)) => String::from_utf8_lossy(&output.stdout).to_string(),
            Ok(Err(e)) => {
                tracing::warn!("[lsp] {} check failed: {e}", language);
                return Ok(Vec::new());
            }
            Err(_) => {
                tracing::warn!("[lsp] {} check timed out after {timeout_secs}s", language);
                return Err("timeout".into());
            }
        };

        // Parse with language-specific parser
        let mut all_diags = Self::parse_linter_output(&stdout, &language);

        // Priority: changed-file diagnostics first
        let changed = self.changed_files.lock().await;
        if !changed.is_empty() {
            all_diags.sort_by_key(|d| {
                if changed.iter().any(|f| d.file.contains(f.as_str())) { 0 } else { 1 }
            });
            let prioritized = all_diags.iter()
                .filter(|d| changed.iter().any(|f| d.file.contains(f.as_str())))
                .count();
            tracing::info!(
                "[lsp] {} check: {} diagnostics ({} prioritized for changed files)",
                language, all_diags.len(), prioritized
            );
        } else {
            tracing::info!("[lsp] {} check: {} diagnostics", language, all_diags.len());
        }

        drop(changed);
        self.changed_files.lock().await.clear();

        Ok(all_diags)
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
    /// COMPRESSED: Only inject errors + first 5 warnings to save tokens.
    /// Excess warnings are summarized as count only.
    fn format_diagnostics_xml(diagnostics: &[Diagnostic]) -> String {
        if diagnostics.is_empty() {
            return "<diagnostics count=\"0\" status=\"clean\">\nProject compiles cleanly.\n</diagnostics>".into();
        }

        let errors: Vec<&Diagnostic> = diagnostics.iter().filter(|d| d.severity == "error").collect();
        let warnings: Vec<&Diagnostic> = diagnostics.iter().filter(|d| d.severity == "warning").collect();

        let has_errors = !errors.is_empty();

        let mut xml = format!(
            "<diagnostics count=\"{}\" has_errors=\"{}\">\n",
            diagnostics.len(),
            has_errors
        );

        // Inject ALL errors (these block compilation — LLM must fix them)
        for d in &errors {
            let code_part = if d.code.is_empty() { String::new() } else { format!(" code=\"{}\"", d.code) };
            xml.push_str(&format!(
                "<error file=\"{}\" line=\"{}\" col=\"{}\"{}>\n{}\n</error>\n",
                d.file, d.line, d.column, code_part, d.message
            ));
        }

        // Inject first 5 warnings only (excess = token waste)
        const MAX_WARNINGS: usize = 5;
        for d in warnings.iter().take(MAX_WARNINGS) {
            let code_part = if d.code.is_empty() { String::new() } else { format!(" code=\"{}\"", d.code) };
            xml.push_str(&format!(
                "<warning file=\"{}\" line=\"{}\" col=\"{}\"{}>\n{}\n</warning>\n",
                d.file, d.line, d.column, code_part, d.message
            ));
        }

        // Summarize excess warnings (don't waste tokens listing them all)
        if warnings.len() > MAX_WARNINGS {
            xml.push_str(&format!(
                "<summary>{} additional warning(s) omitted (run lsp_check for full list)</summary>\n",
                warnings.len() - MAX_WARNINGS
            ));
        }

        xml.push_str(&format!(
            "\nSummary: {} issue(s) ({} error(s), {} warning(s))\n",
            diagnostics.len(),
            errors.len(),
            warnings.len()
        ));
        xml.push_str("</diagnostics>");
        xml
    }

    /// Format diagnostics as human-readable text (for LspCheckTool).
    /// COMPRESSED: errors always shown, warnings limited to first 10.
    fn format_diagnostics_text(diagnostics: &[Diagnostic]) -> String {
        if diagnostics.is_empty() {
            return "✅ No diagnostics. Project compiles cleanly.".into();
        }

        let errors: Vec<&Diagnostic> = diagnostics.iter().filter(|d| d.severity == "error").collect();
        let warnings: Vec<&Diagnostic> = diagnostics.iter().filter(|d| d.severity == "warning").collect();

        let mut text = format!(
            "📋 Diagnostics ({} issue(s)): {} error(s), {} warning(s)\n\n",
            diagnostics.len(), errors.len(), warnings.len()
        );

        // All errors
        for d in &errors {
            text.push_str(&format!(
                "🔴 {}:{}:{} [{}] {}\n",
                d.file, d.line, d.column,
                if d.code.is_empty() { "error" } else { &d.code },
                d.message
            ));
        }

        // First 10 warnings
        const MAX_TEXT_WARNINGS: usize = 10;
        for d in warnings.iter().take(MAX_TEXT_WARNINGS) {
            text.push_str(&format!(
                "🟡 {}:{}:{} [{}] {}\n",
                d.file, d.line, d.column,
                if d.code.is_empty() { "warning" } else { &d.code },
                d.message
            ));
        }

        if warnings.len() > MAX_TEXT_WARNINGS {
            text.push_str(&format!(
                "\n... and {} more warning(s) omitted\n",
                warnings.len() - MAX_TEXT_WARNINGS
            ));
        }

        text
    }

    /// Do a fresh check and update stored diagnostics.
    async fn do_check(&self) -> Result<Vec<Diagnostic>, String> {
        let result = self.run_cargo_check().await;
        match result {
            Ok(diags) => {
                let has_errs = diags.iter().any(|d| d.severity == "error");
                self.has_errors.store(has_errs, Ordering::SeqCst);
                self.dirty.store(false, Ordering::SeqCst);
                let mut store = self.diagnostics.lock().await;
                *store = diags.clone();
                Ok(diags)
            }
            Err(e) => {
                tracing::warn!("[lsp] check failed: {e}");
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

    /// After write/edit tool completes, mark dirty + track changed file.
    async fn on_tool_execution_end(&self, ctx: &ToolExecutionContext) -> AgentResult<()> {
        if ctx.tool_name == "write" || ctx.tool_name == "edit" {
            self.dirty.store(true, Ordering::SeqCst);
            // Track which file changed for priority diagnostics
            if let Some(file) = ctx.args.get("file_path").and_then(|v| v.as_str()) {
                let mut changed = self.changed_files.lock().await;
                if !changed.iter().any(|f| f == file) {
                    changed.push(file.to_string());
                }
            }
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
