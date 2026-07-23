use std::collections::HashMap;

/// Stores provider authentication configuration.
///
/// Holds the base URLs for each known provider so that callers can
/// quickly check whether a given provider is configured.
pub struct AuthStorage {
    /// Map of provider name -> base URL.
    pub provider_base_urls: HashMap<String, String>,
}

impl AuthStorage {
    /// Create an empty `AuthStorage`.
    pub fn new() -> Self {
        Self {
            provider_base_urls: HashMap::new(),
        }
    }

    /// Return `true` if `provider_base_urls` contains the given provider key.
    pub fn has_provider(&self, provider: &str) -> bool {
        self.provider_base_urls.contains_key(provider)
    }
}

impl Default for AuthStorage {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_has_provider() {
        // Build an AuthStorage with a known set of providers.
        let mut storage = AuthStorage::new();
        storage.provider_base_urls.insert(
            "openai".to_string(),
            "https://api.openai.com/v1".to_string(),
        );
        storage.provider_base_urls.insert(
            "anthropic".to_string(),
            "https://api.anthropic.com".to_string(),
        );

        // Known provider should be found.
        assert!(storage.has_provider("openai"));
        // Unknown provider should not be found.
        assert!(!storage.has_provider("unknown"));
    }
}
