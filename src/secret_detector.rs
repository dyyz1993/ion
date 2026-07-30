//! Secret Detector — detect and redact secrets before sending to LLM or saving to memory.
//!
//! Two-layer detection (aligned with pi's `secret-detector.ts`):
//! 1. Known format regex (AWS / Anthropic / OpenAI / GitHub / GitLab / PEM / JWT / Bearer / DB connection strings)
//! 2. Shannon entropy detection (high-entropy strings ≥ 24 chars, catches base64/hex keys)
//!
//! Redacted format: `[REDACTED:label]` (preserves type info for downstream LLM).

use std::collections::HashMap;

/// A detected secret with its type and redacted form.
#[derive(Clone, Debug)]
pub struct DetectedSecret {
    pub secret_type: String,
    pub original: String,
    pub redacted: String,
}

/// Known secret patterns (13 types, aligned with pi).
/// Each entry: (pattern, label).
/// Patterns use simple string matching (not regex) for safety + speed.
#[allow(dead_code)]
fn known_patterns() -> Vec<(&'static str, &'static str)> {
    vec![
        // AWS Access Key (starts with AKIA, 20 chars)
        ("AKIA[0-9A-Z]{16}", "AWS_ACCESS_KEY"),
        // AWS Secret Key (40 chars base64-ish after "aws_secret" or in key=value)
        (
            "aws_secret_access_key[^a-zA-Z0-9]{1,20}[A-Za-z0-9/+=]{40}",
            "AWS_SECRET_KEY",
        ),
        // Anthropic API key (sk-ant-*)
        ("sk-ant-[A-Za-z0-9_\\-]{20,}", "ANTHROPIC_KEY"),
        // OpenAI API key (sk-*)
        ("sk-[A-Za-z0-9]{20,}", "OPENAI_KEY"),
        // GitHub token (ghp_*, gho_*, ghs_*, ghu_*)
        ("gh[posu]_[A-Za-z0-9]{36}", "GITHUB_TOKEN"),
        // GitLab token (glpat-*)
        ("glpat-[A-Za-z0-9_\\-]{20}", "GITLAB_TOKEN"),
        // PEM private key header
        ("-----BEGIN[A-Z ]*PRIVATE KEY-----", "PEM_KEY"),
        // JWT (eyJ... three base64 segments)
        (
            "eyJ[A-Za-z0-9_\\-]{10,}\\.eyJ[A-Za-z0-9_\\-]{10,}\\.[A-Za-z0-9_\\-]{10,}",
            "JWT",
        ),
        // Bearer token
        ("[Bb]earer\\s+[A-Za-z0-9_\\-\\.]{20,}", "BEARER_TOKEN"),
        // Database connection string (postgres://user:pass@host)
        (
            "(postgres|mysql|mongodb|redis)://[^\\s:]+:[^\\s@]+@",
            "DB_CONNECTION",
        ),
        // Generic API key in key=value format
        (
            "(?i)(api[_\\-]?key|secret|token|password|passwd|pwd)\\s*[=:]\\s*[\"']?[A-Za-z0-9/+=_\\-]{16,}",
            "GENERIC_SECRET",
        ),
        // Slack token
        ("xox[baprs]-[A-Za-z0-9\\-]{10,}", "SLACK_TOKEN"),
        // Google API key (AIza...)
        ("AIza[0-9A-Za-z_\\-]{35}", "GOOGLE_KEY"),
    ]
}

/// Shannon entropy calculation.
/// Returns bits per character — higher = more random = more likely a secret.
fn shannon_entropy(s: &str) -> f64 {
    if s.is_empty() {
        return 0.0;
    }
    let mut freq: HashMap<char, usize> = HashMap::new();
    for c in s.chars() {
        *freq.entry(c).or_insert(0) += 1;
    }
    let len = s.len() as f64;
    let mut entropy = 0.0;
    for &count in freq.values() {
        let p = count as f64 / len;
        if p > 0.0 {
            entropy -= p * p.log2();
        }
    }
    entropy
}

/// Check if a string looks like a high-entropy secret.
/// Criteria: ≥24 chars, ≥4.5 bits/char Shannon entropy, not a common path/URL/code.
fn is_high_entropy_secret(s: &str) -> bool {
    // Too short — skip
    if s.len() < 24 {
        return false;
    }
    // Too long — probably a log line or code block, not a secret
    if s.len() > 500 {
        return false;
    }
    // Skip if contains spaces (secrets don't have spaces)
    if s.contains(' ') {
        return false;
    }
    // Skip common non-secret patterns
    let lower = s.to_lowercase();
    if lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("/users/")
        || lower.starts_with("/tmp/")
        || lower.contains("function")
        || lower.contains("return")
        || lower.contains("import")
    {
        return false;
    }
    // Only consider strings that look like base64 or hex
    let is_base64ish = s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '=');
    let is_hexish = s.chars().all(|c| c.is_ascii_hexdigit());
    if !is_base64ish && !is_hexish {
        return false;
    }
    // Check entropy
    shannon_entropy(s) >= 4.5
}

/// Detect all secrets in a text string.
/// Returns list of (secret_type, original_value, redacted_value).
pub fn detect_secrets(text: &str) -> Vec<DetectedSecret> {
    let mut results = Vec::new();

    // Layer 1: Known format patterns (scan for prefixes)
    let known = scan_known_prefixes(text);
    for (matched, label) in known {
        if matched.len() >= 8 {
            results.push(DetectedSecret {
                redacted: format!("[REDACTED:{}]", label),
                original: matched,
                secret_type: label,
            });
        }
    }

    // Layer 2: High-entropy detection
    // Split text into tokens and check each
    for token in text.split(|c: char| {
        c.is_whitespace() || c == '"' || c == '\'' || c == '`' || c == ',' || c == ';'
    }) {
        let token = token.trim_matches(|c: char| {
            c == '"' || c == '\'' || c == '`' || c == '(' || c == ')' || c == '[' || c == ']'
        });
        if is_high_entropy_secret(token) {
            // Don't double-report if already caught by known patterns
            let already = results.iter().any(|r| r.original.contains(token));
            if !already {
                results.push(DetectedSecret {
                    secret_type: "HIGH_ENTROPY".into(),
                    original: token.to_string(),
                    redacted: "[REDACTED:HIGH_ENTROPY]".into(),
                });
            }
        }
    }

    results
}

/// Simple pattern matching — scans for known secret prefixes in text.
/// Returns matching substrings with their detected type.
fn scan_known_prefixes(text: &str) -> Vec<(String, String)> {
    let mut results = Vec::new(); // (matched_substring, label)
    let lower = text.to_lowercase();

    // AWS Access Key
    for pos in lower.match_indices("akia") {
        let start = pos.0;
        let end = (start + 20).min(text.len());
        let candidate = &text[start..end];
        if candidate.len() == 20 && candidate[4..].chars().all(|c| c.is_ascii_alphanumeric()) {
            results.push((candidate.to_string(), "AWS_ACCESS_KEY".into()));
        }
    }

    // Anthropic key (sk-ant-)
    for pos in lower.match_indices("sk-ant-") {
        let start = pos.0;
        let end = (start + 50).min(text.len());
        let candidate = &text[start..end];
        if candidate.len() > 15 {
            results.push((candidate.to_string(), "ANTHROPIC_KEY".into()));
        }
    }

    // OpenAI key (sk- but not sk-ant-)
    for pos in lower.match_indices("sk-") {
        if text[pos.0..].starts_with("sk-ant-") {
            continue;
        }
        let start = pos.0;
        // Extract until whitespace/newline (don't grab 50 chars blindly)
        let candidate: String = text[start..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        if candidate.len() > 15 {
            results.push((candidate, "OPENAI_KEY".into()));
        }
    }

    // GitHub tokens (ghp_ gho_ ghs_ ghu_)
    for prefix in &["ghp_", "gho_", "ghs_", "ghu_"] {
        for pos in lower.match_indices(prefix) {
            let start = pos.0;
            let candidate: String = text[start..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if candidate.len() >= 40 {
                results.push((candidate, "GITHUB_TOKEN".into()));
            }
        }
    }

    // GitLab token (glpat-)
    for pos in lower.match_indices("glpat-") {
        let start = pos.0;
        let candidate: String = text[start..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        if candidate.len() > 15 {
            results.push((candidate, "GITLAB_TOKEN".into()));
        }
    }

    // PEM private key
    if lower.contains("-----begin") && lower.contains("private key-----") {
        results.push(("-----BEGIN PRIVATE KEY-----".into(), "PEM_KEY".into()));
    }

    // Google API key (AIza)
    for pos in lower.match_indices("aiza") {
        let start = pos.0;
        let end = (start + 39).min(text.len());
        let candidate = &text[start..end];
        if candidate.len() == 39 {
            results.push((candidate.to_string(), "GOOGLE_KEY".into()));
        }
    }

    // Slack token (xox)
    for pos in lower.match_indices("xox") {
        let start = pos.0;
        let end = (start + 30).min(text.len());
        let candidate = &text[start..end];
        if candidate.len() > 15 {
            results.push((candidate.to_string(), "SLACK_TOKEN".into()));
        }
    }

    // Generic key=value (api_key=, secret=, token=, password=)
    for kw in &[
        "api_key", "apikey", "api-key", "secret", "token", "password", "passwd", "pwd",
    ] {
        if let Some(pos) = lower.find(kw) {
            let after = &text[pos + kw.len()..];
            let trimmed = after.trim_start();
            if trimmed.starts_with('=') || trimmed.starts_with(':') {
                let val_offset = after.len() - trimmed.len() + 1;
                let val_start = pos + kw.len() + val_offset;
                if val_start < text.len() {
                    let val_text = &text[val_start..];
                    let skip = val_text
                        .char_indices()
                        .skip_while(|(_, c)| c.is_whitespace() || *c == '"' || *c == '\'')
                        .next()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                    let val_start = val_start + skip;
                    if val_start < text.len() {
                        let val_end = text[val_start..]
                            .char_indices()
                            .find(|(_, c)| {
                                c.is_whitespace()
                                    || *c == '"'
                                    || *c == '\''
                                    || *c == '\n'
                                    || *c == ','
                            })
                            .map(|(i, _)| val_start + i)
                            .unwrap_or(text.len());
                        if val_end > val_start + 8 {
                            results.push((
                                text[val_start..val_end].to_string(),
                                "GENERIC_SECRET".into(),
                            ));
                        }
                    }
                }
            }
        }
    }

    // JWT (eyJ prefix — three base64 segments separated by dots, no spaces)
    for pos in lower.match_indices("eyj") {
        let start = pos.0;
        // Capture until whitespace or end of string
        let candidate: String = text[start..]
            .chars()
            .take_while(|c| !c.is_whitespace())
            .collect();
        if candidate.len() >= 20 {
            results.push((candidate, "JWT".into()));
        }
    }

    // Bearer token (Bearer <token>)
    for pos in lower.match_indices("bearer ") {
        let start = pos.0 + 7; // skip past "bearer "
        if start >= text.len() {
            continue;
        }
        let candidate: String = text[start..]
            .chars()
            .take_while(|c| !c.is_whitespace())
            .collect();
        if candidate.len() >= 10 {
            results.push((candidate, "BEARER_TOKEN".into()));
        }
    }

    // Database connection strings (postgres:// mysql:// mongodb:// redis://)
    for scheme in &["postgres://", "mysql://", "mongodb://", "redis://"] {
        for pos in lower.match_indices(scheme) {
            let start = pos.0;
            let candidate: String = text[start..]
                .chars()
                .take_while(|c| !c.is_whitespace())
                .collect();
            if candidate.len() > scheme.len() {
                results.push((candidate, "DB_CONNECTION".into()));
            }
        }
    }

    // AWS Secret Access Key (aws_secret_access_key=<40-char value>)
    for pos in lower.match_indices("aws_secret_access_key") {
        let kw_len = "aws_secret_access_key".len();
        let after = pos.0 + kw_len;
        if after >= text.len() {
            continue;
        }
        // Find separator '=' or ':'
        let rest = &text[after..];
        if let Some(sep_rel) = rest.find(|c| c == '=' || c == ':') {
            let val_start = after + sep_rel + 1;
            if val_start >= text.len() {
                continue;
            }
            // Skip whitespace and quotes
            let val_text = &text[val_start..];
            let skip = val_text
                .char_indices()
                .skip_while(|(_, c)| c.is_whitespace() || *c == '"' || *c == '\'')
                .map(|(i, _)| i)
                .next()
                .unwrap_or(0);
            let val_start = val_start + skip;
            if val_start >= text.len() {
                continue;
            }
            // Take 40-char value
            let candidate: String = text[val_start..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '/' || *c == '+' || *c == '=')
                .collect();
            if candidate.len() >= 20 {
                results.push((candidate, "AWS_SECRET_KEY".into()));
            }
        }
    }

    results
}

/// Redact all detected secrets in a text string.
/// Returns the redacted text.
pub fn redact_secrets(text: &str) -> String {
    let secrets = detect_secrets(text);
    if secrets.is_empty() {
        return text.to_string();
    }

    let mut result = text.to_string();
    for secret in &secrets {
        result = result.replace(&secret.original, &secret.redacted);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_aws_key() {
        let secrets = detect_secrets("AKIAIOSFODNN7EXAMPLE");
        assert!(secrets.iter().any(|s| s.secret_type == "AWS_ACCESS_KEY"));
    }

    #[test]
    fn test_detect_openai_key() {
        let secrets = detect_secrets("sk-proj-abcdef1234567890abcdefghij");
        assert!(secrets.iter().any(|s| s.secret_type == "OPENAI_KEY"));
    }

    #[test]
    fn test_detect_anthropic_key() {
        let secrets = detect_secrets("sk-ant-api03-abcdef1234567890abcdefghij");
        assert!(secrets.iter().any(|s| s.secret_type == "ANTHROPIC_KEY"));
    }

    #[test]
    fn test_detect_github_token() {
        // ghp_ + 36 alphanumeric chars
        let token = format!("ghp_{}", "a".repeat(36));
        let secrets = detect_secrets(&token);
        assert!(
            secrets.iter().any(|s| s.secret_type == "GITHUB_TOKEN"),
            "expected GITHUB_TOKEN in {:?}",
            secrets
        );
    }

    #[test]
    fn test_detect_generic_key_value() {
        let secrets = detect_secrets("api_key=sk_test_1234567890abcdef");
        assert!(secrets.iter().any(|s| s.secret_type == "GENERIC_SECRET"));
    }

    #[test]
    fn test_redact_replaces_secret() {
        let original = "My API key is sk-proj-abcdef1234567890abcdefghij";
        let redacted = redact_secrets(original);
        assert!(redacted.contains("[REDACTED:"));
        assert!(!redacted.contains("sk-proj-abcdef1234567890abcdefghij"));
    }

    #[test]
    fn test_no_false_positive_on_normal_text() {
        let secrets = detect_secrets("Hello world, this is a normal message");
        assert!(secrets.is_empty());
    }

    #[test]
    fn test_no_false_positive_on_code() {
        let secrets = detect_secrets("fn main() { println!(\"hello\"); }");
        assert!(secrets.is_empty());
    }

    #[test]
    fn test_no_false_positive_on_url() {
        let secrets = detect_secrets("https://github.com/user/repo/pull/123");
        assert!(secrets.is_empty());
    }

    #[test]
    fn test_no_false_positive_on_path() {
        let secrets = detect_secrets("/Users/xuyingzhou/Project/study-rust/ion/src/lib.rs");
        assert!(secrets.is_empty());
    }

    #[test]
    fn test_shannon_entropy() {
        // Low entropy (repetitive)
        assert!(shannon_entropy("aaaaaaaaaaaaaaaa") < 1.0);
        // High entropy (random-ish)
        assert!(shannon_entropy("x9Kf2mQ7vR4pW8nT3bZ6yL1") > 3.5);
        // Empty
        assert_eq!(shannon_entropy(""), 0.0);
    }

    #[test]
    fn test_high_entropy_detection() {
        // Long random string should be detected
        let secrets = detect_secrets("The token is x9Kf2mQ7vR4pW8nT3bZ6yL1aB5cD8");
        assert!(secrets.iter().any(|s| s.secret_type == "HIGH_ENTROPY"));
    }

    #[test]
    fn test_redact_preserves_non_secret_text() {
        let original = "config:\n  api_key=sk-proj-abcdef1234567890abcdefghij\n  port=8080";
        let redacted = redact_secrets(original);
        // port=8080 should survive (it's not a secret)
        assert!(
            redacted.contains("port=8080"),
            "port=8080 missing from: {}",
            redacted
        );
        // api_key value should be redacted
        assert!(
            redacted.contains("[REDACTED:"),
            "no redaction in: {}",
            redacted
        );
        // The raw key should be gone
        assert!(
            !redacted.contains("sk-proj-abcdef1234567890abcdefghij"),
            "raw key still present"
        );
    }

    #[test]
    fn test_pem_key_detection() {
        let secrets = detect_secrets("-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEA...");
        assert!(
            secrets.iter().any(|s| s.secret_type == "PEM_KEY"),
            "expected PEM_KEY in {:?}",
            secrets
        );
    }

    #[test]
    fn test_detect_jwt() {
        // JWTs begin with eyJ and contain three base64 segments separated by dots
        let input = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
        let secrets = detect_secrets(input);
        assert!(
            secrets.iter().any(|s| s.secret_type == "JWT"),
            "expected JWT in {:?}",
            secrets
        );
    }

    #[test]
    fn test_detect_bearer_token() {
        // Bearer token value follows the "Bearer " prefix
        let input = "Authorization: Bearer abcdefghijklmnopqrstuvwxyz0123456789";
        let secrets = detect_secrets(input);
        assert!(
            secrets.iter().any(|s| s.secret_type == "BEARER_TOKEN"),
            "expected BEARER_TOKEN in {:?}",
            secrets
        );
    }

    #[test]
    fn test_detect_db_connection() {
        // Connection string with embedded credentials
        let input = "postgres://myuser:mypassword123@db.example.com:5432/mydb";
        let secrets = detect_secrets(input);
        assert!(
            secrets.iter().any(|s| s.secret_type == "DB_CONNECTION"),
            "expected DB_CONNECTION in {:?}",
            secrets
        );
    }

    #[test]
    fn test_detect_aws_secret_key() {
        // AWS secret key assigned via env-style syntax
        let input = "aws_secret_access_key=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
        let secrets = detect_secrets(input);
        assert!(
            secrets.iter().any(|s| s.secret_type == "AWS_SECRET_KEY"),
            "expected AWS_SECRET_KEY in {:?}",
            secrets
        );
    }

    #[test]
    fn test_detect_gitlab_token() {
        // GitLab personal access token with glpat- prefix
        let token = format!("glpat-{}", "a".repeat(20));
        let secrets = detect_secrets(&token);
        assert!(
            secrets.iter().any(|s| s.secret_type == "GITLAB_TOKEN"),
            "expected GITLAB_TOKEN in {:?}",
            secrets
        );
    }

    #[test]
    fn test_detect_slack_token() {
        // Slack bot token with xoxb- prefix
        let secrets = detect_secrets("xoxb-1234567890-abcdefghij");
        assert!(
            secrets.iter().any(|s| s.secret_type == "SLACK_TOKEN"),
            "expected SLACK_TOKEN in {:?}",
            secrets
        );
    }

    #[test]
    fn test_detect_google_api_key() {
        // Google API key with AIza prefix (39 chars total)
        let key = format!("AIza{}", "a".repeat(35));
        let secrets = detect_secrets(&key);
        assert!(
            secrets.iter().any(|s| s.secret_type == "GOOGLE_KEY"),
            "expected GOOGLE_KEY in {:?}",
            secrets
        );
    }

    #[test]
    fn test_detect_multiple_secrets() {
        // Multiple secret types in a single input string
        let input =
            "Found AWS key AKIAIOSFODNN7EXAMPLE and OpenAI key sk-proj-abcdef1234567890abcdefghij";
        let secrets = detect_secrets(input);
        assert!(
            secrets.iter().any(|s| s.secret_type == "AWS_ACCESS_KEY"),
            "expected AWS_ACCESS_KEY"
        );
        assert!(
            secrets.iter().any(|s| s.secret_type == "OPENAI_KEY"),
            "expected OPENAI_KEY"
        );
    }
}
