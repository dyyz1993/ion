//! Browser Fetch Tool — SPA/CSR 感知网页抓取（内核直 spawn `browser` 二进制）
//!
//! 对接自研 Rust 浏览器引擎（~/Project/study-rust/browser）的 `browser fetch` CLI。
//! 定位：只负责 JS 动态渲染页面（SPA/CSR）；静态页/纯 API 调用由 bash + curl 负责，
//! 工具描述里显式引导 LLM 分流。
//!
//! 集成铁律（2026-09-10 定稿，与 browser 侧 M82/M83 对齐）：
//! - **进程边界**：内核直接 spawn 二进制（不走 bash、不产生 shell），永远不合并代码
//! - **超时双保险**：上游引擎自带全局预算（默认 60s，`--timeout-ms` 对所有 wait 策略
//!   生效）；ION 侧再套一层硬超时（timeout_ms + 15s 余量）——不信任外部二进制不挂
//! - **warnings 透传**：上游反爬壳页检测信号（`--json` 的 warnings + stderr 的
//!   `[warn]` 行）必须原样带到 LLM 手里，agent 自判换路径
//! - **max_length 截断**：ION 侧内容截断保护，防撑爆 LLM 上下文

use crate::agent::error::{AgentError, AgentResult};
use crate::agent::tool::Tool;
use async_trait::async_trait;
use std::path::PathBuf;
use std::time::Instant;

/// ION 侧默认超时（毫秒）——上游 60s 全局预算之上留余量
pub const DEFAULT_TIMEOUT_MS: u64 = 75_000;
/// ION 硬超时在上游预算之外追加的余量（毫秒）——上游先到点返回部分内容，这是纯兜底
pub const TIMEOUT_MARGIN_MS: u64 = 15_000;
/// 内容默认截断上限（字符）
pub const DEFAULT_MAX_LENGTH: usize = 50_000;

// ---------------------------------------------------------------------------
// FetchTool
// ---------------------------------------------------------------------------

pub struct FetchTool;

#[async_trait]
impl Tool for FetchTool {
    fn name(&self) -> &str {
        "fetch"
    }

    fn description(&self) -> &str {
        "Fetch full page content with JavaScript executed (SPA/CSR-aware) — use this when a \
         page's real content is loaded dynamically and plain HTTP would return an empty shell. \
         For static pages or direct API calls, prefer bash + curl. Returns JSON: \
         {url, title, content, elapsed_ms, truncated, warnings}. Check `warnings`: \
         non-empty means the page may be an anti-bot shell, not real content."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "Page URL (http:// or https://)"
                },
                "format": {
                    "type": "string",
                    "enum": ["markdown", "html", "text", "links", "images"],
                    "default": "markdown",
                    "description": "Output extraction format (default: markdown)"
                },
                "wait_strategy": {
                    "type": "string",
                    "enum": ["load", "dom-ready", "timeout"],
                    "default": "load",
                    "description": "JS wait strategy. 'load' waits for the event loop to settle (max 60s upstream budget); 'timeout' returns after timeout_ms"
                },
                "timeout_ms": {
                    "type": "integer",
                    "default": 75000,
                    "description": "Upstream JS budget in ms (engine returns partial content with a warning when it fires). ION hard-kills at timeout_ms + 15s"
                },
                "selector": {
                    "type": "string",
                    "description": "Optional CSS selector to extract only matching subtrees (e.g. '#content_left')"
                },
                "max_length": {
                    "type": "integer",
                    "default": 50000,
                    "description": "Max content length in chars; longer content is truncated (truncated=true)"
                }
            },
            "required": ["url"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _rt: &dyn crate::runtime::Runtime,
    ) -> AgentResult<String> {
        let cfg = crate::config::IonConfig::load();
        run_fetch(&args, &cfg.fetch).await
    }
}

// ---------------------------------------------------------------------------
// 配置
// ---------------------------------------------------------------------------

pub use crate::config::FetchConfig;

// ---------------------------------------------------------------------------
// 核心实现（独立成函数便于测试）
// ---------------------------------------------------------------------------

pub async fn run_fetch(args: &serde_json::Value, cfg: &FetchConfig) -> AgentResult<String> {
    let url = args
        .get("url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AgentError::Tool("missing 'url' argument".into()))?;
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(AgentError::Tool(format!(
            "invalid url '{url}': only http:// and https:// are supported"
        )));
    }

    // URL 白名单（空 = 允许全部）
    if !url_allowed(url, &cfg.allow_urls) {
        return Err(AgentError::Tool(format!(
            "url '{url}' is not allowed by fetch.allow_urls whitelist in ~/.ion/config.json"
        )));
    }

    let format = args
        .get("format")
        .and_then(|v| v.as_str())
        .unwrap_or("markdown")
        .to_string();
    let wait_strategy = args
        .get("wait_strategy")
        .and_then(|v| v.as_str())
        .unwrap_or("load")
        .to_string();
    let timeout_ms = args
        .get("timeout_ms")
        .and_then(|v| v.as_u64())
        .or(cfg.default_timeout_ms)
        .unwrap_or(DEFAULT_TIMEOUT_MS);
    let max_length = args
        .get("max_length")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .or(cfg.max_length)
        .unwrap_or(DEFAULT_MAX_LENGTH);

    let binary = resolve_browser_binary().ok_or_else(|| {
        AgentError::Tool(
            "browser binary not found: looked in ION_BROWSER_PATH, config fetch.path, PATH, \
             ~/.ion/bin/browser. Install: cargo install --git https://github.com/dyyz1993/browser \
             browser-cli"
                .to_string(),
        )
    })?;

    let mut cli_args = vec![
        "fetch".to_string(),
        url.to_string(),
        "--format".to_string(),
        format.clone(),
        "--json".to_string(),
        "--timeout-ms".to_string(),
        timeout_ms.to_string(),
    ];
    if wait_strategy != "load" {
        cli_args.push("--wait-strategy".into());
        cli_args.push(wait_strategy.clone());
    }
    if let Some(selector) = args.get("selector").and_then(|v| v.as_str()) {
        cli_args.push("--selector".into());
        cli_args.push(selector.to_string());
    }

    // ION 硬超时 = 上游预算 + 余量：上游先到点返回部分内容，这是防挂死纯兜底
    let hard_timeout_ms = timeout_ms.saturating_add(TIMEOUT_MARGIN_MS);
    let start = Instant::now();
    let output = tokio::time::timeout(
        std::time::Duration::from_millis(hard_timeout_ms),
        tokio::process::Command::new(&binary)
            .args(&cli_args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output(),
    )
    .await
    .map_err(|_| {
        AgentError::Tool(format!(
            "fetch timed out after {}ms (upstream budget {}ms) — process killed by ION",
            hard_timeout_ms, timeout_ms
        ))
    })?
    .map_err(|e| AgentError::Tool(format!("spawn browser failed: {e}")))?;

    let elapsed_ms = start.elapsed().as_millis() as u64;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if !output.status.success() {
        let tail: String = stderr.lines().rev().take(5).collect::<Vec<_>>().join("\n");
        return Err(AgentError::Tool(format!(
            "browser fetch failed (exit {}): {}",
            output.status.code().unwrap_or(-1),
            tail
        )));
    }

    let parsed: serde_json::Value = serde_json::from_str(stdout.trim())
        .map_err(|e| AgentError::Tool(format!("failed to parse browser --json output: {e}")))?;

    // warnings = --json 的 warnings 数组 + stderr 的 [warn] 行（预算/反爬信号都在这）
    let mut warnings: Vec<String> = parsed
        .get("warnings")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|w| w.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    for line in stderr.lines() {
        let t = line.trim();
        let body = t.strip_prefix("[warn] ").unwrap_or(t);
        // stderr 行与 --json warnings 常为同一信号（如 "content is very short"），去重
        if !body.is_empty() && !warnings.iter().any(|w| w.contains(body)) {
            warnings.push(body.to_string());
        }
    }

    let title = parsed
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let content = extract_content(&parsed).unwrap_or_default();
    let (content, truncated) = truncate_chars(&content, max_length);

    let result = serde_json::json!({
        "url": url,
        "title": title,
        "format": format,
        "content": content,
        "elapsed_ms": elapsed_ms,
        "truncated": truncated,
        "warnings": warnings,
    });
    Ok(serde_json::to_string_pretty(&result).unwrap_or_default())
}

// ---------------------------------------------------------------------------
// 纯函数（单元测试覆盖）
// ---------------------------------------------------------------------------

/// 二进制查找：ION_BROWSER_PATH env > config fetch.path > PATH 里的 `browser`
/// > `~/.ion/bin/browser`
pub fn resolve_browser_binary() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("ION_BROWSER_PATH") {
        if !p.trim().is_empty() {
            let pb = PathBuf::from(p.trim());
            if pb.exists() {
                return Some(pb);
            }
        }
    }
    let cfg = crate::config::IonConfig::load();
    if let Some(p) = cfg.fetch.path {
        if !p.trim().is_empty() {
            let pb = PathBuf::from(p.trim());
            if pb.exists() {
                return Some(pb);
            }
        }
    }
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let cand = dir.join("browser");
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        let cand = PathBuf::from(home).join(".ion").join("bin").join("browser");
        if cand.exists() {
            return Some(cand);
        }
    }
    None
}

/// URL 白名单：patterns 为空 = 允许全部；否则按 `*`/`?` 通配匹配完整 URL
pub fn url_allowed(url: &str, patterns: &[String]) -> bool {
    if patterns.is_empty() {
        return true;
    }
    patterns.iter().any(|p| wildcard_match(p, url))
}

/// 经典 `*`/`?` 通配匹配（迭代版，防回溯爆炸）
pub fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// 从 `--json` 输出提取正文：content 可能是字符串，也可能是
/// `{markdown|text|html: "..."}` 对象（M83 起 --json 尊重 --format）
pub fn extract_content(parsed: &serde_json::Value) -> Option<String> {
    match parsed.get("content") {
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        Some(serde_json::Value::Object(map)) => {
            for key in ["markdown", "text", "html", "links", "images"] {
                if let Some(serde_json::Value::String(s)) = map.get(key) {
                    return Some(s.clone());
                }
            }
            // 兜底：任意一个字符串字段
            map.values().find_map(|v| v.as_str().map(String::from))
        }
        _ => None,
    }
}

/// 按字符（非字节）截断，保证 UTF-8 安全
pub fn truncate_chars(s: &str, max: usize) -> (String, bool) {
    if s.chars().count() <= max {
        return (s.to_string(), false);
    }
    (s.chars().take(max).collect(), true)
}

// ---------------------------------------------------------------------------
// 单元测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_match_basic() {
        assert!(wildcard_match("*", "https://anything.com/x"));
        assert!(wildcard_match(
            "https://*.example.com/*",
            "https://api.example.com/v1/items"
        ));
        assert!(wildcard_match(
            "https://example.com/*",
            "https://example.com/"
        ));
        assert!(!wildcard_match(
            "https://example.com/*",
            "https://evil.com/"
        ));
        assert!(!wildcard_match(
            "https://*.example.com/*",
            "https://example.com.evil.com/"
        ));
        assert!(wildcard_match(
            "http://localhost:????/x",
            "http://localhost:8802/x"
        ));
    }

    #[test]
    fn url_allowed_empty_means_all() {
        assert!(url_allowed("https://anything.com/", &[]));
        let patterns = vec!["https://*.example.com/*".to_string()];
        assert!(url_allowed("https://a.example.com/x", &patterns));
        assert!(!url_allowed("https://other.com/", &patterns));
    }

    #[test]
    fn extract_content_object_and_string() {
        let v: serde_json::Value =
            serde_json::json!({"content": {"markdown": "# hi"}, "title": "t"});
        assert_eq!(extract_content(&v).unwrap(), "# hi");
        let v: serde_json::Value = serde_json::json!({"content": {"text": "plain"}});
        assert_eq!(extract_content(&v).unwrap(), "plain");
        let v: serde_json::Value = serde_json::json!({"content": "raw string"});
        assert_eq!(extract_content(&v).unwrap(), "raw string");
        let v: serde_json::Value = serde_json::json!({});
        assert!(extract_content(&v).is_none());
    }

    #[test]
    fn truncate_chars_is_char_safe() {
        let s = "中文内容abc";
        let (out, truncated) = truncate_chars(s, 4);
        assert!(truncated);
        assert_eq!(out, "中文内容");
        let (out, truncated) = truncate_chars(s, 100);
        assert!(!truncated);
        assert_eq!(out, s);
    }

    #[test]
    fn missing_url_is_error() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let err = run_fetch(&serde_json::json!({}), &FetchConfig::default())
                .await
                .unwrap_err();
            assert!(err.to_string().contains("missing 'url'"));
        });
    }

    #[tokio::test]
    async fn rejects_non_http_scheme() {
        let err = run_fetch(
            &serde_json::json!({"url": "file:///etc/passwd"}),
            &FetchConfig::default(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("only http"), "got: {err}");
    }

    #[tokio::test]
    async fn url_whitelist_denies() {
        let cfg = FetchConfig {
            allow_urls: vec!["https://*.example.com/*".to_string()],
            ..Default::default()
        };
        let err = run_fetch(&serde_json::json!({"url": "https://evil.com/x"}), &cfg)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not allowed"), "got: {err}");
    }

    #[tokio::test]
    async fn missing_binary_reports_install_guidance() {
        // 仅当环境确实解析不到任何 browser 二进制时才断言安装指引
        // （PATH 里装了 browser 的机器上会走成功路径，跳过）
        let saved = std::env::var("ION_BROWSER_PATH").ok();
        unsafe { std::env::remove_var("ION_BROWSER_PATH") };
        let resolvable = resolve_browser_binary().is_none();
        let cfg = FetchConfig {
            path: Some("/nonexistent/ion_test_browser_path".into()),
            ..Default::default()
        };
        let result = run_fetch(&serde_json::json!({"url": "https://example.com/"}), &cfg).await;
        match saved {
            Some(v) => unsafe { std::env::set_var("ION_BROWSER_PATH", v) },
            None => unsafe { std::env::remove_var("ION_BROWSER_PATH") },
        }
        if resolvable {
            let err = result.unwrap_err();
            assert!(
                err.to_string().contains("cargo install --git"),
                "got: {err}"
            );
        }
    }
}
