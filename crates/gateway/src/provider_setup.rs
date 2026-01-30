use std::sync::Arc;

use {async_trait::async_trait, serde_json::Value, tokio::sync::RwLock, tracing::info};

use {
    moltis_agents::providers::ProviderRegistry,
    moltis_config::schema::ProvidersConfig,
    moltis_oauth::{CallbackServer, OAuthFlow, TokenStore, callback_port, load_oauth_config},
};

use crate::services::{ProviderSetupService, ServiceResult};

/// Known provider definitions used to populate the "available providers" list.
struct KnownProvider {
    name: &'static str,
    display_name: &'static str,
    auth_type: &'static str,
    env_key: Option<&'static str>,
}

const KNOWN_PROVIDERS: &[KnownProvider] = &[
    KnownProvider {
        name: "anthropic",
        display_name: "Anthropic",
        auth_type: "api-key",
        env_key: Some("ANTHROPIC_API_KEY"),
    },
    KnownProvider {
        name: "openai",
        display_name: "OpenAI",
        auth_type: "api-key",
        env_key: Some("OPENAI_API_KEY"),
    },
    KnownProvider {
        name: "gemini",
        display_name: "Google Gemini",
        auth_type: "api-key",
        env_key: Some("GEMINI_API_KEY"),
    },
    KnownProvider {
        name: "groq",
        display_name: "Groq",
        auth_type: "api-key",
        env_key: Some("GROQ_API_KEY"),
    },
    KnownProvider {
        name: "xai",
        display_name: "xAI (Grok)",
        auth_type: "api-key",
        env_key: Some("XAI_API_KEY"),
    },
    KnownProvider {
        name: "deepseek",
        display_name: "DeepSeek",
        auth_type: "api-key",
        env_key: Some("DEEPSEEK_API_KEY"),
    },
    KnownProvider {
        name: "openai-codex",
        display_name: "OpenAI Codex",
        auth_type: "oauth",
        env_key: None,
    },
];

pub struct LiveProviderSetupService {
    registry: Arc<RwLock<ProviderRegistry>>,
    config: ProvidersConfig,
    token_store: TokenStore,
}

impl LiveProviderSetupService {
    pub fn new(registry: Arc<RwLock<ProviderRegistry>>, config: ProvidersConfig) -> Self {
        Self {
            registry,
            config,
            token_store: TokenStore::new(),
        }
    }

    fn is_provider_configured(&self, provider: &KnownProvider) -> bool {
        // Check if the provider has an API key set via config or env
        if let Some(env_key) = provider.env_key
            && std::env::var(env_key).is_ok()
        {
            return true;
        }
        if let Some(entry) = self.config.get(provider.name)
            && entry.api_key.as_ref().is_some_and(|k| !k.is_empty())
        {
            return true;
        }
        // For OAuth providers, check token store
        if provider.auth_type == "oauth" {
            return self.token_store.load(provider.name).is_some();
        }
        false
    }
}

#[async_trait]
impl ProviderSetupService for LiveProviderSetupService {
    async fn available(&self) -> ServiceResult {
        let providers: Vec<Value> = KNOWN_PROVIDERS
            .iter()
            .map(|p| {
                serde_json::json!({
                    "name": p.name,
                    "displayName": p.display_name,
                    "authType": p.auth_type,
                    "configured": self.is_provider_configured(p),
                })
            })
            .collect();
        Ok(Value::Array(providers))
    }

    async fn save_key(&self, params: Value) -> ServiceResult {
        let provider_name = params
            .get("provider")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing 'provider' parameter".to_string())?;
        let api_key = params
            .get("apiKey")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing 'apiKey' parameter".to_string())?;

        // Validate provider name
        let known = KNOWN_PROVIDERS
            .iter()
            .find(|p| p.name == provider_name && p.auth_type == "api-key")
            .ok_or_else(|| format!("unknown api-key provider: {provider_name}"))?;

        // Set the environment variable so the provider registry picks it up
        if let Some(env_key) = known.env_key {
            // Safety: called from a single async context; env var mutation is
            // unavoidable here since providers read from env at registration time.
            unsafe { std::env::set_var(env_key, api_key) };
        }

        // Rebuild the provider registry with the new key
        let new_registry = ProviderRegistry::from_env_with_config(&self.config);
        let mut reg = self.registry.write().await;
        *reg = new_registry;

        info!(
            provider = provider_name,
            "saved API key and rebuilt provider registry"
        );

        Ok(serde_json::json!({ "ok": true }))
    }

    async fn oauth_start(&self, params: Value) -> ServiceResult {
        let provider_name = params
            .get("provider")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing 'provider' parameter".to_string())?
            .to_string();

        let oauth_config = load_oauth_config(&provider_name)
            .ok_or_else(|| format!("no OAuth config for provider: {provider_name}"))?;

        let port = callback_port(&oauth_config);
        let flow = OAuthFlow::new(oauth_config);
        let auth_req = flow.start();

        let auth_url = auth_req.url.clone();
        let verifier = auth_req.pkce.verifier.clone();
        let expected_state = auth_req.state.clone();

        // Spawn background task to wait for the callback and exchange the code
        let token_store = self.token_store.clone();
        let registry = Arc::clone(&self.registry);
        let config = self.config.clone();
        tokio::spawn(async move {
            match CallbackServer::wait_for_code(port, expected_state).await {
                Ok(code) => {
                    match flow.exchange(&code, &verifier).await {
                        Ok(tokens) => {
                            if let Err(e) = token_store.save(&provider_name, &tokens) {
                                tracing::error!(
                                    provider = %provider_name,
                                    error = %e,
                                    "failed to save OAuth tokens"
                                );
                                return;
                            }
                            // Rebuild registry with new tokens
                            let new_registry = ProviderRegistry::from_env_with_config(&config);
                            let mut reg = registry.write().await;
                            *reg = new_registry;
                            info!(
                                provider = %provider_name,
                                "OAuth flow complete, rebuilt provider registry"
                            );
                        },
                        Err(e) => {
                            tracing::error!(
                                provider = %provider_name,
                                error = %e,
                                "OAuth token exchange failed"
                            );
                        },
                    }
                },
                Err(e) => {
                    tracing::error!(
                        provider = %provider_name,
                        error = %e,
                        "OAuth callback failed"
                    );
                },
            }
        });

        Ok(serde_json::json!({
            "authUrl": auth_url,
        }))
    }

    async fn oauth_status(&self, params: Value) -> ServiceResult {
        let provider_name = params
            .get("provider")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing 'provider' parameter".to_string())?;

        let has_tokens = self.token_store.load(provider_name).is_some();
        Ok(serde_json::json!({
            "provider": provider_name,
            "authenticated": has_tokens,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_providers_have_valid_auth_types() {
        for p in KNOWN_PROVIDERS {
            assert!(
                p.auth_type == "api-key" || p.auth_type == "oauth",
                "invalid auth type for {}: {}",
                p.name,
                p.auth_type
            );
        }
    }

    #[test]
    fn api_key_providers_have_env_key() {
        for p in KNOWN_PROVIDERS {
            if p.auth_type == "api-key" {
                assert!(
                    p.env_key.is_some(),
                    "api-key provider {} missing env_key",
                    p.name
                );
            }
        }
    }

    #[test]
    fn oauth_providers_have_no_env_key() {
        for p in KNOWN_PROVIDERS {
            if p.auth_type == "oauth" {
                assert!(
                    p.env_key.is_none(),
                    "oauth provider {} should not have env_key",
                    p.name
                );
            }
        }
    }

    #[test]
    fn known_provider_names_unique() {
        let mut names: Vec<&str> = KNOWN_PROVIDERS.iter().map(|p| p.name).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), KNOWN_PROVIDERS.len());
    }

    #[tokio::test]
    async fn noop_service_returns_empty() {
        use crate::services::NoopProviderSetupService;
        let svc = NoopProviderSetupService;
        let result = svc.available().await.unwrap();
        assert_eq!(result, serde_json::json!([]));
    }

    #[tokio::test]
    async fn live_service_lists_providers() {
        let registry = Arc::new(RwLock::new(ProviderRegistry::from_env_with_config(
            &ProvidersConfig::default(),
        )));
        let svc = LiveProviderSetupService::new(registry, ProvidersConfig::default());
        let result = svc.available().await.unwrap();
        let arr = result.as_array().unwrap();
        assert!(!arr.is_empty());
        // Check that we have expected fields
        let first = &arr[0];
        assert!(first.get("name").is_some());
        assert!(first.get("displayName").is_some());
        assert!(first.get("authType").is_some());
        assert!(first.get("configured").is_some());
    }

    #[tokio::test]
    async fn save_key_rejects_unknown_provider() {
        let registry = Arc::new(RwLock::new(ProviderRegistry::from_env_with_config(
            &ProvidersConfig::default(),
        )));
        let svc = LiveProviderSetupService::new(registry, ProvidersConfig::default());
        let result = svc
            .save_key(serde_json::json!({"provider": "nonexistent", "apiKey": "test"}))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn save_key_rejects_missing_params() {
        let registry = Arc::new(RwLock::new(ProviderRegistry::from_env_with_config(
            &ProvidersConfig::default(),
        )));
        let svc = LiveProviderSetupService::new(registry, ProvidersConfig::default());
        assert!(svc.save_key(serde_json::json!({})).await.is_err());
        assert!(
            svc.save_key(serde_json::json!({"provider": "anthropic"}))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn oauth_start_rejects_unknown_provider() {
        let registry = Arc::new(RwLock::new(ProviderRegistry::from_env_with_config(
            &ProvidersConfig::default(),
        )));
        let svc = LiveProviderSetupService::new(registry, ProvidersConfig::default());
        let result = svc
            .oauth_start(serde_json::json!({"provider": "nonexistent"}))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn oauth_status_returns_not_authenticated() {
        let registry = Arc::new(RwLock::new(ProviderRegistry::from_env_with_config(
            &ProvidersConfig::default(),
        )));
        let svc = LiveProviderSetupService::new(registry, ProvidersConfig::default());
        let result = svc
            .oauth_status(serde_json::json!({"provider": "openai-codex"}))
            .await
            .unwrap();
        // Might or might not have tokens depending on environment
        assert!(result.get("authenticated").is_some());
    }
}
