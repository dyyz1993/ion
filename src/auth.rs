use std::path::PathBuf;

/// Auth storage for API keys and credentials.
///
/// Stored in `~/.ion/auth.json` with permissions 0600 (owner read/write only).
/// This separates secrets from the main `config.json`.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AuthStorage {
    /// Default API key (used across providers)
    #[serde(default)]
    pub api_key: Option<String>,

    /// Per-provider API keys (provider_name → key)
    #[serde(default)]
    pub provider_api_keys: std::collections::HashMap<String, String>,

    /// Per-provider base URLs
    #[serde(default)]
    pub provider_base_urls: std::collections::HashMap<String, String>,
}

impl Default for AuthStorage {
    fn default() -> Self {
        Self {
            api_key: None,
            provider_api_keys: std::collections::HashMap::new(),
            provider_base_urls: std::collections::HashMap::new(),
        }
    }
}

impl AuthStorage {
    /// Path: ~/.ion/auth.json
    pub fn path() -> PathBuf {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_else(|_| ".".into());
        PathBuf::from(home).join(".ion").join("auth.json")
    }

    /// Load auth from file, or return defaults.
    pub fn load() -> Self {
        let path = Self::path();
        if !path.exists() {
            return AuthStorage::default();
        }
        match std::fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_else(|e| {
                eprintln!("Warning: failed to parse auth.json: {e}");
                AuthStorage::default()
            }),
            Err(_) => AuthStorage::default(),
        }
    }

    /// Save auth to file with permissions 0600.
    pub fn save(&self) -> Result<(), Box<dyn std::error::Error>> {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, &content)?;

        // Set permissions to 0600 (owner read/write only) on Unix
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }

        Ok(())
    }

    /// Resolve API key with full priority chain:
    /// 1. CLI --api-key
    /// 2. auth.json api_key
    /// 3. auth.json provider_api_keys[provider]
    /// 4. Config file (legacy)
    /// 5. Environment variable (PROVIDER_API_KEY)
    /// 6. Generic ION_API_KEY
    pub fn resolve_api_key(cli_key: Option<&str>, provider: &str) -> Option<String> {
        // 1. CLI
        if let Some(key) = cli_key {
            return Some(key.to_string());
        }

        let auth = Self::load();

        // 2. auth.json provider_api_keys (provider-specific first)
        if let Some(key) = auth.provider_api_keys.get(provider) {
            return Some(key.clone());
        }

        // 2b. 前缀匹配 fallback:zhipuai-2 → zhipuai,openai-3 → openai
        // 模型配置里 provider 可能带后缀(如 zhipuai-2),但 auth.json 里只有基础名
        if let Some(dash_pos) = provider.rfind('-') {
            let base = &provider[..dash_pos];
            if let Some(key) = auth.provider_api_keys.get(base) {
                return Some(key.clone());
            }
        }

        // 3. auth.json api_key (generic fallback)
        if let Some(ref key) = auth.api_key {
            return Some(key.clone());
        }

        // 4. Legacy config.json (loaded by IonConfig)
        let cfg = crate::config::IonConfig::load();
        if let Some(ref key) = cfg.api_key {
            return Some(key.clone());
        }
        if let Some(key) = cfg.provider_api_keys.get(provider) {
            return Some(key.clone());
        }
        // 前缀匹配 fallback(config.json)
        if let Some(dash_pos) = provider.rfind('-') {
            let base = &provider[..dash_pos];
            if let Some(key) = cfg.provider_api_keys.get(base) {
                return Some(key.clone());
            }
        }

        // 5. Environment var (PROVIDER_API_KEY)
        let env_var = format!("{}_API_KEY", provider.to_uppercase());
        if let Ok(key) = std::env::var(&env_var) {
            return Some(key);
        }

        // 6. Generic ION_API_KEY
        if let Ok(key) = std::env::var("ION_API_KEY") {
            return Some(key);
        }

        None
    }

    /// Return the list of provider names from `provider_base_urls`.
    pub fn list_providers(&self) -> Vec<String> {
        self.provider_base_urls.keys().cloned().collect()
    }

    /// Return the number of providers in `provider_base_urls`.
    pub fn provider_count(&self) -> usize {
        self.provider_base_urls.len()
    }

    /// Return total entries count (api keys + base urls).
    pub fn total_entries_count(&self) -> usize {
        self.provider_api_keys.len() + self.provider_base_urls.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_list_providers() {
        let mut base_urls = std::collections::HashMap::new();
        base_urls.insert("openai".to_string(), "https://api.openai.com".to_string());
        base_urls.insert("anthropic".to_string(), "https://api.anthropic.com".to_string());
        base_urls.insert("zhipuai".to_string(), "https://open.bigmodel.cn".to_string());

        let auth = AuthStorage {
            api_key: None,
            provider_api_keys: std::collections::HashMap::new(),
            provider_base_urls: base_urls,
        };

        let mut providers = auth.list_providers();
        providers.sort();
        assert_eq!(providers, vec!["anthropic", "openai", "zhipuai"]);
    }

    #[test]
    fn test_provider_count() {
        let mut base_urls = std::collections::HashMap::new();
        base_urls.insert("openai".to_string(), "https://api.openai.com".to_string());
        base_urls.insert("anthropic".to_string(), "https://api.anthropic.com".to_string());
        base_urls.insert("zhipuai".to_string(), "https://open.bigmodel.cn".to_string());

        let auth = AuthStorage {
            api_key: None,
            provider_api_keys: std::collections::HashMap::new(),
            provider_base_urls: base_urls,
        };

        assert_eq!(auth.provider_count(), 3);
    }

    #[test]
    fn test_total_entries_count() {
        let mut api_keys = std::collections::HashMap::new();
        api_keys.insert("openai".to_string(), "sk-xxx".to_string());
        api_keys.insert("anthropic".to_string(), "sk-yyy".to_string());

        let mut base_urls = std::collections::HashMap::new();
        base_urls.insert("openai".to_string(), "https://api.openai.com".to_string());

        let auth = AuthStorage {
            api_key: None,
            provider_api_keys: api_keys,
            provider_base_urls: base_urls,
        };

        assert_eq!(auth.total_entries_count(), 3);

        // Empty store
        let empty = AuthStorage {
            api_key: None,
            provider_api_keys: std::collections::HashMap::new(),
            provider_base_urls: std::collections::HashMap::new(),
        };
        assert_eq!(empty.total_entries_count(), 0);
    }
}
