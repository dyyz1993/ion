
/// Map provider name → environment variable name.
///
/// Follows the pi-ai reference: each provider maps to a specific env var.
/// When the user specifies `--provider opencode`, we look up `OPOCODE_API_KEY`.
fn provider_env_var(provider: &str) -> String {
    let key = match provider {
        "anthropic" => "ANTHROPIC_API_KEY",
        "openai" => "OPENAI_API_KEY",
        "google" => "GOOGLE_API_KEY",
        "deepseek" => "DEEPSEEK_API_KEY",
        "opencode" => "OPOCODE_API_KEY",
        "openrouter" => "OPENROUTER_API_KEY",
        "xai" => "XAI_API_KEY",
        "groq" => "GROQ_API_KEY",
        "mistral" => "MISTRAL_API_KEY",
        "github-copilot" => "GITHUB_COPILOT_API_KEY",
        "cloudflare-workers-ai" => "CLOUDFLARE_API_KEY",
        other => &format!("{other}_API_KEY").to_uppercase(),
    };
    key.to_string()
}

/// Look up API key for a given provider from environment variables.
pub fn get_env_api_key(provider: &str) -> Option<String> {
    let var_name = provider_env_var(provider);
    std::env::var(&var_name).ok().or_else(|| {
        // Also check the generic ION_API_KEY
        std::env::var("ION_API_KEY").ok()
    })
}

/// Resolve API key: explicit > env var > error.
pub fn resolve_api_key(provider: &str, explicit: Option<String>) -> crate::ProviderResult<String> {
    if let Some(key) = explicit {
        return Ok(key);
    }
    get_env_api_key(provider)
        .ok_or_else(|| crate::ProviderError::MissingApiKey(provider.to_string()))
}
