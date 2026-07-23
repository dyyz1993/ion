//! Rules Engine Extension
//!
//! Scans `.ion/rules/*.md` files in the project directory, parses YAML
//! frontmatter for `applyTo` glob patterns, and injects matched rules
//! into the system prompt based on the current working directory's files.
//!
//! Aligned with pi's `extensions/rules-engine/` design:
//! - Each `.md` file has YAML frontmatter with `applyTo: "**/*.rs"` glob patterns
//! - The body of the `.md` file is the rule content
//! - Rules are injected into the system prompt via the `on_system_prompt` hook
//! - Also exposes `list` and `match` RPC methods
//!
//! Rules are reloaded on each `on_system_prompt` call (no caching), following
//! the same pattern as `HooksConfig::load_fresh` (hot-reload, zero state).

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::agent::error::{AgentError, AgentResult};
use crate::agent::extension::Extension;

// ---------------------------------------------------------------------------
// Data structures
// ---------------------------------------------------------------------------

/// A single rule loaded from a `.ion/rules/*.md` file.
#[derive(Clone, Debug)]
pub struct Rule {
    /// Human-readable name (derived from the file stem).
    pub name: String,
    /// Glob patterns like `**/*.rs` that determine which files this rule applies to.
    pub apply_to: Vec<String>,
    /// The markdown body (content after the YAML frontmatter).
    pub content: String,
    /// The source file path (relative or absolute) where this rule was loaded from.
    pub source: String,
}

impl Rule {
    /// Returns true if any of this rule's `apply_to` patterns match `file_path`.
    pub fn matches_file(&self, file_path: &str) -> bool {
        self.apply_to
            .iter()
            .any(|pattern| glob_match(pattern, file_path))
    }

    /// Returns true if this rule applies to the project, given a set of
    /// representative project file paths. A rule matches if ANY of its
    /// patterns matches ANY of the provided file paths.
    pub fn matches_any(&self, files: &[String]) -> bool {
        self.apply_to.iter().any(|pattern| {
            files.iter().any(|f| glob_match(pattern, f))
        })
    }
}

// ---------------------------------------------------------------------------
// RulesEngineExtension
// ---------------------------------------------------------------------------

/// Rules Engine Extension.
///
/// Implements the `Extension` trait. On each `on_system_prompt` call it reloads
/// rules from `.ion/rules/*.md`, filters them against the current project files,
/// and appends the matched rules as an XML block to the system prompt.
///
/// RPC methods:
/// - `"list"`: return all loaded rules (name + source + applyTo).
/// - `"match"`: given a file path, return the rules that match it.
pub struct RulesEngineExtension {
    /// The project root directory used to locate `.ion/rules/` and scan files.
    project_dir: PathBuf,
}

impl RulesEngineExtension {
    /// Create a new extension bound to the current working directory.
    pub fn new() -> Self {
        let project_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self { project_dir }
    }

    /// Create a new extension with an explicit project directory (useful for tests).
    pub fn with_project_dir(project_dir: PathBuf) -> Self {
        Self { project_dir }
    }

    /// Load all rules from `<project_dir>/.ion/rules/*.md`.
    ///
    /// Returns an empty vector if the rules directory does not exist.
    /// Files that fail to read or parse are silently skipped (logged at debug).
    pub fn load_rules(&self) -> Vec<Rule> {
        let rules_dir = self.project_dir.join(".ion").join("rules");
        load_rules_from_dir(&rules_dir)
    }

    /// Collect a representative set of file paths in the project directory.
    ///
    /// Walks the project tree (skipping common ignore directories like `.git`,
    /// `target`, `node_modules`) and returns relative paths. This is used to
    /// determine which rules apply based on `applyTo` glob patterns.
    pub fn collect_project_files(&self) -> Vec<String> {
        let mut files = Vec::new();
        collect_files(&self.project_dir, &self.project_dir, &mut files);
        files
    }

    /// Format a list of matched rules as an XML block for system-prompt injection.
    fn format_rules_xml(rules: &[Rule]) -> String {
        let mut xml = String::from("\n<project_rules>\n");
        for rule in rules {
            xml.push_str(&format!(
                "<rule name=\"{}\" source=\"{}\">\n{}\n</rule>\n",
                escape_xml_attr(&rule.name),
                escape_xml_attr(&rule.source),
                rule.content.trim(),
            ));
        }
        xml.push_str("</project_rules>");
        xml
    }
}

impl Default for RulesEngineExtension {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Extension for RulesEngineExtension {
    fn name(&self) -> &str {
        "rules-engine"
    }

    /// Reload rules from disk, filter by project files, and inject matched
    /// rules as an XML block into the system prompt.
    async fn on_system_prompt(&self, prompt: &mut String) -> AgentResult<()> {
        let rules = self.load_rules();
        if rules.is_empty() {
            return Ok(());
        }

        let project_files = self.collect_project_files();
        let matched: Vec<&Rule> = rules
            .iter()
            .filter(|r| r.matches_any(&project_files))
            .collect();

        if matched.is_empty() {
            return Ok(());
        }

        let owned: Vec<Rule> = matched.into_iter().cloned().collect();
        prompt.push_str(&Self::format_rules_xml(&owned));
        Ok(())
    }

    /// Handle RPC methods:
    /// - `"list"`: return all loaded rules.
    /// - `"match"`: return rules matching a file path given in `params.file`.
    async fn on_extension_rpc(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> AgentResult<serde_json::Value> {
        match method {
            "list" => {
                let rules = self.load_rules();
                let entries: Vec<serde_json::Value> = rules
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "name": r.name,
                            "source": r.source,
                            "applyTo": r.apply_to,
                        })
                    })
                    .collect();
                Ok(serde_json::json!({ "rules": entries }))
            }
            "match" => {
                let file = params
                    .get("file")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let rules = self.load_rules();
                let matched: Vec<serde_json::Value> = rules
                    .iter()
                    .filter(|r| r.matches_file(file))
                    .map(|r| {
                        serde_json::json!({
                            "name": r.name,
                            "source": r.source,
                            "applyTo": r.apply_to,
                            "content": r.content,
                        })
                    })
                    .collect();
                Ok(serde_json::json!({ "file": file, "rules": matched }))
            }
            _ => Err(AgentError::Tool(
                "extension rpc method not found".into(),
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Rule loading + frontmatter parsing
// ---------------------------------------------------------------------------

/// Load all rules from a given directory (typically `<project>/.ion/rules`).
/// Only `.md` files are scanned. Files are sorted by name for deterministic order.
fn load_rules_from_dir(rules_dir: &Path) -> Vec<Rule> {
    let mut rules = Vec::new();
    let entries = match std::fs::read_dir(rules_dir) {
        Ok(e) => e,
        Err(_) => return rules, // directory does not exist — no rules
    };
    // Collect and sort .md file paths for deterministic ordering.
    let mut md_files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("md"))
        .collect();
    md_files.sort();

    for path in md_files {
        if let Some(rule) = load_rule_file(&path) {
            rules.push(rule);
        }
    }
    rules
}

/// Load a single rule from a `.md` file.
fn load_rule_file(path: &Path) -> Option<Rule> {
    let content = std::fs::read_to_string(path).ok()?;
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unnamed")
        .to_string();
    let source = path.to_string_lossy().to_string();
    let (apply_to, body) = parse_frontmatter(&content);
    if body.trim().is_empty() {
        return None;
    }
    Some(Rule {
        name,
        apply_to,
        content: body,
        source,
    })
}

/// Parse a markdown file's YAML frontmatter.
///
/// Expected format:
/// ```text
/// ---
/// applyTo: "**/*.rs"
/// ---
/// Rule body content here.
/// ```
///
/// Returns `(apply_to_patterns, body)`. If there is no frontmatter, returns
/// `(empty_vec, full_content)`.
///
/// The `applyTo` value may be:
/// - A single string: `applyTo: "**/*.rs"`
/// - Comma-separated: `applyTo: "**/*.rs, **/*.toml"`
/// - A YAML inline array: `applyTo: ["**/*.rs", "**/*.toml"]`
/// - A YAML block array:
///   ```yaml
///   applyTo:
///     - "**/*.rs"
///     - "**/*.toml"
///   ```
pub fn parse_frontmatter(content: &str) -> (Vec<String>, String) {
    // Frontmatter must start at the very beginning with `---`.
    let trimmed_start = content.strip_prefix("---");
    let Some(after_first_marker) = trimmed_start else {
        // No frontmatter at all — entire content is the body.
        return (Vec::new(), content.to_string());
    };

    // Find the closing `---` marker. It must appear at the start of a line.
    let close_pos = find_frontmatter_close(after_first_marker);
    let Some(close) = close_pos else {
        // No closing marker — treat entire content as body (malformed frontmatter).
        return (Vec::new(), content.to_string());
    };

    let frontmatter = &after_first_marker[..close];
    // Skip past the closing `---` (3 chars) plus any trailing newline.
    let body_start = close + 3;
    let body = if body_start >= after_first_marker.len() {
        String::new()
    } else {
        after_first_marker[body_start..].trim_start_matches(['\r', '\n']).to_string()
    };

    let apply_to = parse_apply_to(frontmatter);
    (apply_to, body)
}

/// Find the byte offset of the closing `---` frontmatter marker.
/// The marker must be at the start of a line (preceded by a newline or at offset 0).
fn find_frontmatter_close(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = 0;
    let len = bytes.len();
    while i < len {
        // Check if the current line starts with `---`.
        if i + 2 < len && bytes[i] == b'-' && bytes[i + 1] == b'-' && bytes[i + 2] == b'-' {
            // Ensure it's a standalone marker line: next char is newline or end.
            let after = i + 3;
            if after >= len || bytes[after] == b'\n' || bytes[after] == b'\r' {
                return Some(i);
            }
        }
        // Advance to the next line.
        while i < len && bytes[i] != b'\n' {
            i += 1;
        }
        if i < len {
            i += 1; // skip the newline
        }
    }
    None
}

/// Parse the `applyTo` field from frontmatter text into a list of glob patterns.
///
/// Handles four forms:
/// 1. Single value: `applyTo: "**/*.rs"`
/// 2. Comma-separated: `applyTo: "**/*.rs, **/*.toml"`
/// 3. Inline YAML array: `applyTo: ["**/*.rs", "**/*.toml"]`
/// 4. Block YAML array:
///    ```yaml
///    applyTo:
///      - "**/*.rs"
///      - "**/*.toml"
///    ```
fn parse_apply_to(frontmatter: &str) -> Vec<String> {
    let mut patterns = Vec::new();
    let mut in_block_array = false;

    for line in frontmatter.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        // Block array item: `- "pattern"` or `- pattern`
        if in_block_array && trimmed.starts_with('-') {
            let val = trimmed[1..].trim();
            let cleaned = clean_yaml_value(val);
            if !cleaned.is_empty() {
                patterns.push(cleaned);
            }
            continue;
        }

        // Detect `applyTo:` key (case-insensitive, allows optional spaces).
        if let Some(rest) = strip_key(trimmed, "applyTo") {
            in_block_array = false;
            let val = rest.trim();
            if val.is_empty() {
                // Block array follows on subsequent lines.
                in_block_array = true;
                continue;
            }
            // Inline array: ["a", "b"]
            if val.starts_with('[') {
                for item in split_yaml_inline_array(val) {
                    let cleaned = clean_yaml_value(&item);
                    if !cleaned.is_empty() {
                        patterns.push(cleaned);
                    }
                }
                continue;
            }
            // Single value or comma-separated values.
            // First, strip any surrounding quotes from the whole value (e.g.,
            // `applyTo: "**/*.rs, **/*.toml"` has quotes around everything).
            let stripped = clean_yaml_value(val);
            if stripped.contains(',') {
                // Comma-separated values inside a (possibly quoted) string.
                for item in stripped.split(',') {
                    let cleaned = item.trim().to_string();
                    if !cleaned.is_empty() {
                        patterns.push(cleaned);
                    }
                }
            } else if !stripped.is_empty() {
                patterns.push(stripped);
            }
        }
    }

    patterns
}

/// Strip a YAML key prefix (`key:`) from a line, returning the remainder.
/// Comparison is case-insensitive.
fn strip_key<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let lower = line.to_lowercase();
    let prefix = format!("{key}:");
    let lower_prefix = prefix.to_lowercase();
    lower.strip_prefix(&lower_prefix).map(|_| {
        // Return the slice from the original line after the key + colon.
        let colon_pos = line.find(':').unwrap_or(key.len());
        &line[colon_pos + 1..]
    })
}

/// Remove surrounding quotes and whitespace from a YAML scalar value.
fn clean_yaml_value(val: &str) -> String {
    let mut s = val.trim().to_string();
    // Remove matching surrounding quotes (single or double).
    if s.len() >= 2 {
        let first = s.chars().next().unwrap();
        let last = s.chars().last().unwrap();
        if (first == '"' && last == '"') || (first == '\'' && last == '\'') {
            s = s[1..s.len() - 1].to_string();
        }
    }
    s.trim().to_string()
}

/// Split a YAML inline array string like `["a", "b", "c"]` into its elements.
fn split_yaml_inline_array(val: &str) -> Vec<String> {
    let inner = val
        .trim()
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(val);
    let mut items = Vec::new();
    let mut current = String::new();
    let mut in_quote: Option<char> = None;
    for ch in inner.chars() {
        match in_quote {
            Some(q) => {
                if ch == q {
                    in_quote = None;
                    items.push(current.clone());
                    current.clear();
                } else {
                    current.push(ch);
                }
            }
            None => {
                match ch {
                    '"' | '\'' => in_quote = Some(ch),
                    ',' => {
                        if !current.is_empty() {
                            items.push(current.clone());
                            current.clear();
                        }
                    }
                    _ => current.push(ch),
                }
            }
        }
    }
    if !current.is_empty() {
        items.push(current);
    }
    items
}

// ---------------------------------------------------------------------------
// Glob matching (manual — no external crate)
// ---------------------------------------------------------------------------

/// Match a glob pattern against a file path.
///
/// Supports:
/// - `**` : matches any number of path segments (including zero), cross-directory.
/// - `*`  : matches any characters within a single path segment (no `/`).
/// - `?`  : matches a single character within a segment.
/// - Literal characters match exactly.
///
/// Both the pattern and the path use `/` as the path separator.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    glob_rec(pattern.as_bytes(), 0, path.as_bytes(), 0)
}

/// Recursive glob matcher with backtracking.
///
/// `pi` is the current position in the pattern, `ti` is the current position
/// in the text (file path).
fn glob_rec(pat: &[u8], pi: usize, text: &[u8], ti: usize) -> bool {
    // If we've consumed the entire pattern, the text must also be fully consumed.
    if pi >= pat.len() {
        return ti >= text.len();
    }

    // Handle `**` (double star) — matches across path separators.
    if pi + 1 < pat.len() && pat[pi] == b'*' && pat[pi + 1] == b'*' {
        // Skip all consecutive `*` characters after the initial `**`.
        let mut next = pi + 2;
        while next < pat.len() && pat[next] == b'*' {
            next += 1;
        }
        // If the pattern ends after `**`, everything remaining in the text matches.
        if next >= pat.len() {
            return true;
        }
        // If `**` is followed by `/`, it can match zero or more directory segments.
        // The `/` after `**` is consumed when matching zero segments.
        if pat[next] == b'/' {
            // Option 1: `**/` matches zero segments — skip `/**/` and match the rest.
            if glob_rec(pat, next + 1, text, ti) {
                return true;
            }
            // Option 2: `**` matches the current segment, then try again.
            // Find the end of the current text segment.
            let mut t = ti;
            while t < text.len() && text[t] != b'/' {
                t += 1;
            }
            // If we found a separator, advance past it and recurse with `**` again.
            if t < text.len() {
                return glob_rec(pat, pi, text, t + 1);
            }
            // No more separators — `**` cannot match additional segments.
            return false;
        }
        // `**` not followed by `/` — behaves like `*` but can cross segments.
        // Try matching the rest of the pattern at every remaining text position.
        for i in ti..=text.len() {
            if glob_rec(pat, next, text, i) {
                return true;
            }
        }
        return false;
    }

    // Handle single `*` — matches within a single path segment (no `/`).
    if pat[pi] == b'*' {
        // Try matching the rest of the pattern at every position within
        // the current segment.
        let mut t = ti;
        loop {
            if glob_rec(pat, pi + 1, text, t) {
                return true;
            }
            // Stop at segment boundary (can't cross `/` with single `*`).
            if t >= text.len() || text[t] == b'/' {
                return false;
            }
            t += 1;
        }
    }

    // Handle `?` — matches exactly one non-separator character.
    if pat[pi] == b'?' {
        if ti >= text.len() || text[ti] == b'/' {
            return false;
        }
        return glob_rec(pat, pi + 1, text, ti + 1);
    }

    // Literal character match.
    if ti < text.len() && pat[pi] == text[ti] {
        return glob_rec(pat, pi + 1, text, ti + 1);
    }

    false
}

// ---------------------------------------------------------------------------
// Project file scanning
// ---------------------------------------------------------------------------

/// Common directories to skip when scanning the project tree.
const IGNORE_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".ion",
    ".cache",
    "dist",
    "build",
    ".next",
    ".venv",
    "__pycache__",
    ".DS_Store",
];

/// Recursively collect file paths (relative to `base`) into `out`.
fn collect_files(base: &Path, current: &Path, out: &mut Vec<String>) {
    let entries = match std::fs::read_dir(current) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if path.is_dir() {
            if IGNORE_DIRS.contains(&name_str.as_ref()) {
                continue;
            }
            collect_files(base, &path, out);
        } else if path.is_file() {
            if let Ok(rel) = path.strip_prefix(base) {
                out.push(rel.to_string_lossy().to_string());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// XML helpers
// ---------------------------------------------------------------------------

/// Escape special XML characters in an attribute value.
fn escape_xml_attr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Frontmatter parsing ----

    #[test]
    fn test_parse_frontmatter_single_value() {
        let input = "---\napplyTo: \"**/*.rs\"\n---\nUse snake_case for functions.";
        let (apply_to, body) = parse_frontmatter(input);
        assert_eq!(apply_to, vec!["**/*.rs"]);
        assert_eq!(body, "Use snake_case for functions.");
    }

    #[test]
    fn test_parse_frontmatter_comma_separated() {
        let input = "---\napplyTo: \"**/*.rs, **/*.toml\"\n---\nBody content.";
        let (apply_to, body) = parse_frontmatter(input);
        assert_eq!(apply_to, vec!["**/*.rs", "**/*.toml"]);
        assert_eq!(body, "Body content.");
    }

    #[test]
    fn test_parse_frontmatter_inline_array() {
        let input = "---\napplyTo: [\"**/*.rs\", \"**/*.py\"]\n---\nRules body.";
        let (apply_to, _body) = parse_frontmatter(input);
        assert_eq!(apply_to, vec!["**/*.rs", "**/*.py"]);
    }

    #[test]
    fn test_parse_frontmatter_block_array() {
        let input = "---\napplyTo:\n  - \"**/*.rs\"\n  - \"**/*.ts\"\n---\nBlock body.";
        let (apply_to, body) = parse_frontmatter(input);
        assert_eq!(apply_to, vec!["**/*.rs", "**/*.ts"]);
        assert_eq!(body, "Block body.");
    }

    #[test]
    fn test_parse_frontmatter_no_frontmatter() {
        let input = "Just some markdown content without frontmatter.";
        let (apply_to, body) = parse_frontmatter(input);
        assert!(apply_to.is_empty());
        assert_eq!(body, input);
    }

    #[test]
    fn test_parse_frontmatter_unquoted_value() {
        let input = "---\napplyTo: **/*.rs\n---\nBody.";
        let (apply_to, _body) = parse_frontmatter(input);
        assert_eq!(apply_to, vec!["**/*.rs"]);
    }

    // ---- Glob matching ----

    #[test]
    fn test_glob_match_rs() {
        assert!(glob_match("**/*.rs", "src/main.rs"));
        assert!(glob_match("**/*.rs", "main.rs"));
        assert!(glob_match("**/*.rs", "deep/nested/path/lib.rs"));
    }

    #[test]
    fn test_glob_match_all() {
        assert!(glob_match("**/*", "src/main.rs"));
        assert!(glob_match("**/*", "README.md"));
        assert!(glob_match("**/*", "a/b/c/d.txt"));
    }

    #[test]
    fn test_glob_no_match() {
        assert!(!glob_match("**/*.py", "src/main.rs"));
        assert!(!glob_match("**/*.py", "main.rs"));
    }

    #[test]
    fn test_glob_single_star_within_segment() {
        assert!(glob_match("src/*.rs", "src/main.rs"));
        assert!(!glob_match("src/*.rs", "src/nested/main.rs"));
    }

    #[test]
    fn test_glob_exact_path() {
        assert!(glob_match("Cargo.toml", "Cargo.toml"));
        assert!(!glob_match("Cargo.toml", "src/Cargo.toml"));
    }

    #[test]
    fn test_glob_question_mark() {
        assert!(glob_match("main.??", "main.rs"));
        assert!(!glob_match("main.???", "main.rs"));
    }

    // ---- Rule struct ----

    #[test]
    fn test_rule_matches_file() {
        let rule = Rule {
            name: "rust".into(),
            apply_to: vec!["**/*.rs".into()],
            content: "Use snake_case.".into(),
            source: ".ion/rules/rust.md".into(),
        };
        assert!(rule.matches_file("src/main.rs"));
        assert!(!rule.matches_file("src/main.py"));
    }

    #[test]
    fn test_rule_matches_any() {
        let rule = Rule {
            name: "multi".into(),
            apply_to: vec!["**/*.rs".into(), "**/*.py".into()],
            content: "Body".into(),
            source: ".ion/rules/multi.md".into(),
        };
        let files = vec!["src/main.py".to_string()];
        assert!(rule.matches_any(&files));

        let files2 = vec!["README.md".to_string()];
        assert!(!rule.matches_any(&files2));
    }

    // ---- XML formatting ----

    #[test]
    fn test_rule_injection_format() {
        let rules = vec![Rule {
            name: "rust-conventions".into(),
            apply_to: vec!["**/*.rs".into()],
            content: "Use snake_case for functions.".into(),
            source: ".ion/rules/rust.md".into(),
        }];
        let xml = RulesEngineExtension::format_rules_xml(&rules);
        assert!(xml.contains("<project_rules>"));
        assert!(xml.contains("</project_rules>"));
        assert!(xml.contains("<rule name=\"rust-conventions\""));
        assert!(xml.contains("source=\".ion/rules/rust.md\""));
        assert!(xml.contains("Use snake_case for functions."));
        assert!(xml.contains("</rule>"));
    }

    #[test]
    fn test_rule_injection_format_escapes_special() {
        let rules = vec![Rule {
            name: "test <&>".into(),
            apply_to: vec!["**/*".into()],
            content: "Content with \"quotes\"".into(),
            source: "path & file".into(),
        }];
        let xml = RulesEngineExtension::format_rules_xml(&rules);
        // The name and source are attributes and must be escaped.
        assert!(xml.contains("name=\"test &lt;&amp;&gt;\""));
        assert!(xml.contains("source=\"path &amp; file\""));
    }

    #[test]
    fn test_format_rules_xml_empty() {
        let xml = RulesEngineExtension::format_rules_xml(&[]);
        assert!(xml.contains("<project_rules>"));
        assert!(xml.contains("</project_rules>"));
    }

    // ---- find_frontmatter_close ----

    #[test]
    fn test_find_frontmatter_close_basic() {
        let s = "applyTo: \"**/*.rs\"\n---\nbody";
        let pos = find_frontmatter_close(s);
        assert!(pos.is_some());
        // The close marker `---` should be found before "body".
        let pos = pos.unwrap();
        assert_eq!(&s[pos..pos + 3], "---");
    }

    #[test]
    fn test_find_frontmatter_close_none() {
        let s = "applyTo: \"**/*.rs\"\nno closing marker";
        assert!(find_frontmatter_close(s).is_none());
    }

    // ---- Integration: load rules from a temp directory ----

    #[test]
    fn test_load_rules_from_temp_dir() {
        let tmp = std::env::temp_dir().join(format!(
            "ion_rules_test_{}",
            uuid::Uuid::new_v4().to_string()[..8].to_string()
        ));
        let rules_dir = tmp.join(".ion").join("rules");
        std::fs::create_dir_all(&rules_dir).unwrap();

        std::fs::write(
            rules_dir.join("rust.md"),
            "---\napplyTo: \"**/*.rs\"\n---\nUse snake_case.",
        )
        .unwrap();
        std::fs::write(
            rules_dir.join("python.md"),
            "---\napplyTo: \"**/*.py\"\n---\nFollow PEP 8.",
        )
        .unwrap();

        let ext = RulesEngineExtension::with_project_dir(tmp.clone());
        let rules = ext.load_rules();
        assert_eq!(rules.len(), 2);
        // Files are sorted by name.
        assert_eq!(rules[0].name, "python");
        assert_eq!(rules[1].name, "rust");

        // Collect project files (the .md files themselves).
        let files = ext.collect_project_files();
        // The .ion directory is ignored, so only the rules dir content...
        // Actually .ion is in IGNORE_DIRS, so no files collected from there.
        assert!(files.is_empty() || files.iter().all(|f| !f.contains(".ion")));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_load_rules_empty_dir() {
        let tmp = std::env::temp_dir().join(format!(
            "ion_rules_empty_{}",
            uuid::Uuid::new_v4().to_string()[..8].to_string()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let ext = RulesEngineExtension::with_project_dir(tmp.clone());
        let rules = ext.load_rules();
        assert!(rules.is_empty());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_load_rules_nonexistent_dir() {
        let ext = RulesEngineExtension::with_project_dir(PathBuf::from(
            "/nonexistent/path/that/does/not/exist",
        ));
        let rules = ext.load_rules();
        assert!(rules.is_empty());
    }

    // ---- Extension trait ----

    #[test]
    fn test_extension_name() {
        let ext = RulesEngineExtension::new();
        assert_eq!(ext.name(), "rules-engine");
    }

    #[tokio::test]
    async fn test_on_system_prompt_no_rules() {
        let tmp = std::env::temp_dir().join(format!(
            "ion_rules_norules_{}",
            uuid::Uuid::new_v4().to_string()[..8].to_string()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let ext = RulesEngineExtension::with_project_dir(tmp.clone());
        let mut prompt = String::from("base prompt");
        ext.on_system_prompt(&mut prompt).await.unwrap();
        assert_eq!(prompt, "base prompt"); // unchanged when no rules
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_on_system_prompt_with_matching_rule() {
        let tmp = std::env::temp_dir().join(format!(
            "ion_rules_match_{}",
            uuid::Uuid::new_v4().to_string()[..8].to_string()
        ));
        let rules_dir = tmp.join(".ion").join("rules");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(
            rules_dir.join("rust.md"),
            "---\napplyTo: \"**/*.rs\"\n---\nUse snake_case.",
        )
        .unwrap();
        // Create a .rs file so the rule matches the project.
        std::fs::write(tmp.join("main.rs"), "fn main() {}").unwrap();

        let ext = RulesEngineExtension::with_project_dir(tmp.clone());
        let mut prompt = String::from("base");
        ext.on_system_prompt(&mut prompt).await.unwrap();
        assert!(prompt.contains("<project_rules>"));
        assert!(prompt.contains("Use snake_case."));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_on_extension_rpc_list() {
        let tmp = std::env::temp_dir().join(format!(
            "ion_rules_rpc_{}",
            uuid::Uuid::new_v4().to_string()[..8].to_string()
        ));
        let rules_dir = tmp.join(".ion").join("rules");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(
            rules_dir.join("rust.md"),
            "---\napplyTo: \"**/*.rs\"\n---\nBody.",
        )
        .unwrap();

        let ext = RulesEngineExtension::with_project_dir(tmp.clone());
        let result = ext
            .on_extension_rpc("list", serde_json::json!({}))
            .await
            .unwrap();
        let rules = result.get("rules").unwrap().as_array().unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0]["name"], "rust");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_on_extension_rpc_match() {
        let tmp = std::env::temp_dir().join(format!(
            "ion_rules_rpcmatch_{}",
            uuid::Uuid::new_v4().to_string()[..8].to_string()
        ));
        let rules_dir = tmp.join(".ion").join("rules");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(
            rules_dir.join("rust.md"),
            "---\napplyTo: \"**/*.rs\"\n---\nRust body.",
        )
        .unwrap();
        std::fs::write(
            rules_dir.join("python.md"),
            "---\napplyTo: \"**/*.py\"\n---\nPython body.",
        )
        .unwrap();

        let ext = RulesEngineExtension::with_project_dir(tmp.clone());
        let result = ext
            .on_extension_rpc("match", serde_json::json!({ "file": "src/main.rs" }))
            .await
            .unwrap();
        let rules = result.get("rules").unwrap().as_array().unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0]["name"], "rust");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_on_extension_rpc_unknown_method() {
        let ext = RulesEngineExtension::new();
        let result = ext.on_extension_rpc("unknown", serde_json::json!({})).await;
        assert!(result.is_err());
    }
}
