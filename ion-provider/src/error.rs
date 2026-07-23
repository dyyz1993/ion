use std::sync::OnceLock;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ProviderError {
    #[error("Provider error: {0}")]
    Provider(String),

    #[error("Stream error: {0}")]
    Stream(String),

    #[error("HTTP error: {status} {body}")]
    HttpError { status: u16, body: String },

    #[error("API key not found for provider: {0}")]
    MissingApiKey(String),

    #[error("Provider not found: {0}")]
    ProviderNotFound(String),

    #[error("Model not found: {0}")]
    ModelNotFound(String),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error(transparent)]
    Reqwest(#[from] reqwest::Error),
}

pub type ProviderResult<T> = Result<T, ProviderError>;

// ---------------------------------------------------------------------------
// Context overflow detection (对齐 pi packages/ai/src/utils/overflow.ts)
// ---------------------------------------------------------------------------

/// 匹配各厂商"上下文溢出"错误文案的正则集合。
///
/// 移植自 pi 的 `OVERFLOW_PATTERNS`，覆盖 Anthropic / OpenAI / Google /
/// xAI / Groq / OpenRouter / Together / Mistral / Kimi / Ollama / Cerebras 等。
fn overflow_patterns() -> &'static [regex::Regex] {
    static PATTERNS: OnceLock<Vec<regex::Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            // Anthropic
            r"(?i)prompt is too long",
            r"(?i)request_too_large",
            // OpenAI
            r"(?i)exceeds the context window",
            r"(?i)exceeds (?:the )?(?:model'?s )?maximum context length of [\d,]+ tokens?",
            r"(?i)maximum context length",
            // Google
            r"(?i)input token count.*exceeds the maximum",
            // xAI / Groq / OpenRouter / Together / others
            r"(?i)maximum prompt length is \d+",
            r"(?i)reduce the length of the messages",
            r"(?i)maximum context length is \d+ tokens",
            // Mistral / Kimi / MiniMax / Ollama
            r"(?i)too many tokens",
            r"(?i)token limit exceeded",
            // 兜底
            r"(?i)context[_ ]length[_ ]exceeded",
            r"(?i)context length exceeded",
            // Cerebras 无 body 的 400/413
            r"(?i)^4(?:00|13)\s*(?:status code)?\s*\(no body\)",
        ]
        .iter()
        .map(|p| regex::Regex::new(p).unwrap())
        .collect()
    })
}

/// 需要先排除的"看似溢出但实际不是"的错误（限流 / 服务不可用）。
///
/// Bedrock 会把 throttling 格式化成 "Too many tokens, please wait..."，不排除会误命中。
fn non_overflow_patterns() -> &'static [regex::Regex] {
    static PATTERNS: OnceLock<Vec<regex::Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            r"(?i)^(Throttling error|Service unavailable):",
            r"(?i)rate limit",
            r"(?i)too many requests",
        ]
        .iter()
        .map(|p| regex::Regex::new(p).unwrap())
        .collect()
    })
}

/// 判断一段错误文案是否表示"上下文溢出"（排除限流/服务不可用后）。
///
/// 供 `retry::should_retry` 等只拿到 `&str` 的调用方使用。
pub fn is_overflow_message(msg: &str) -> bool {
    // 先排除：限流 / 服务不可用
    if non_overflow_patterns().iter().any(|re| re.is_match(msg)) {
        return false;
    }
    overflow_patterns().iter().any(|re| re.is_match(msg))
}

impl ProviderError {
    /// 判断该错误是否为"上下文溢出"。
    ///
    /// 检测来源：
    /// - `Provider(msg)` / `Stream(msg)`：按文案匹配
    /// - `HttpError { status, body }`：status 400/413 或 body 文案匹配
    pub fn is_context_overflow(&self) -> bool {
        match self {
            ProviderError::Provider(msg) | ProviderError::Stream(msg) => is_overflow_message(msg),
            ProviderError::HttpError { status, body } => {
                // 400/413 本身不一定是溢出（可能是参数错误），需 body 确认
                if matches!(status, 400 | 413) && is_overflow_message(body) {
                    return true;
                }
                is_overflow_message(body)
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_anthropic_overflow() {
        assert!(is_overflow_message("prompt is too long: 213462 tokens > 200000 maximum"));
        assert!(is_overflow_message(
            "Error: {\"type\":\"request_too_large\",\"message\":\"...\"}"
        ));
    }

    #[test]
    fn detect_openai_overflow() {
        assert!(is_overflow_message(
            "This model's maximum context length is 128000 tokens"
        ));
        assert!(is_overflow_message(
            "Request exceeds the context window"
        ));
    }

    #[test]
    fn detect_generic_overflow() {
        assert!(is_overflow_message("context_length_exceeded"));
        assert!(is_overflow_message("too many tokens"));
        assert!(is_overflow_message("token limit exceeded"));
    }

    #[test]
    fn non_overflow_not_matched() {
        // 限流不是溢出
        assert!(!is_overflow_message("Rate limit exceeded, please retry"));
        assert!(!is_overflow_message("too many requests"));
        assert!(!is_overflow_message("Throttling error: Too many tokens, please wait"));
        assert!(!is_overflow_message("Service unavailable"));
    }

    #[test]
    fn http_error_overflow() {
        let err = ProviderError::HttpError {
            status: 400,
            body: "prompt is too long".into(),
        };
        assert!(err.is_context_overflow());

        // 400 但不是溢出
        let err2 = ProviderError::HttpError {
            status: 400,
            body: "invalid model id".into(),
        };
        assert!(!err2.is_context_overflow());
    }

    #[test]
    fn provider_msg_overflow() {
        let err = ProviderError::Provider("context length exceeded".into());
        assert!(err.is_context_overflow());

        let err2 = ProviderError::Provider("internal server error".into());
        assert!(!err2.is_context_overflow());
    }
}
