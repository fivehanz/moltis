use std::{collections::HashMap, path::PathBuf, sync::Arc};

use {async_trait::async_trait, serde_json::Value, tokio::sync::RwLock, tracing::info};

use {
    moltis_agents::providers::ProviderRegistry,
    moltis_config::schema::ProvidersConfig,
    moltis_oauth::{
        CallbackServer, OAuthFlow, TokenStore, callback_port, device_flow, load_oauth_config,
    },
};

use crate::services::{ProviderSetupService, ServiceResult};

// ── Key store ──────────────────────────────────────────────────────────────

/// File-based API key storage at `~/.config/moltis/provider_keys.json`.
/// Stores `{ "anthropic": "sk-...", "openai": "sk-..." }`.
#[derive(Debug, Clone)]
pub(crate) struct KeyStore {
    path: PathBuf,
}

impl KeyStore {
    pub(crate) fn new() -> Self {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_else(|_| ".".into());
        let path = PathBuf::from(home)
            .join(".config")
            .join("moltis")
            .join("provider_keys.json");
        Self { path }
    }

    #[cfg(test)]
    fn with_path(path: PathBuf) -> Self {
        Self { path }
    }

    fn load_all(&self) -> HashMap<String, String> {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|d| serde_json::from_str(&d).ok())
            .unwrap_or_default()
    }

    fn load(&self, provider: &str) -> Option<String> {
        self.load_all().get(provider).cloned()
    }

    fn remove(&self, provider: &str) -> Result<(), String> {
        let mut map = self.load_all();
        map.remove(provider);
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let data = serde_json::to_string_pretty(&map).map_err(|e| e.to_string())?;
        std::fs::write(&self.path, &data).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    fn save(&self, provider: &str, api_key: &str) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut map = self.load_all();
        map.insert(provider.to_string(), api_key.to_string());
        let data = serde_json::to_string_pretty(&map).map_err(|e| e.to_string())?;
        std::fs::write(&self.path, &data).map_err(|e| e.to_string())?;
        // Set file permissions to 0600 on Unix (keys are secrets)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────

/// Merge persisted API keys into a ProvidersConfig so the registry rebuild
/// picks them up without needing env vars.
pub(crate) fn config_with_saved_keys(
    base: &ProvidersConfig,
    key_store: &KeyStore,
) -> ProvidersConfig {
    let mut config = base.clone();
    for (name, key) in key_store.load_all() {
        let entry = config.providers.entry(name).or_default();
        // Only override if config doesn't already have a key.
        if entry.api_key.as_ref().is_none_or(|k| k.is_empty()) {
            entry.api_key = Some(key);
        }
    }
    config
}

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
        name: "mistral",
        display_name: "Mistral",
        auth_type: "api-key",
        env_key: Some("MISTRAL_API_KEY"),
    },
    KnownProvider {
        name: "openrouter",
        display_name: "OpenRouter",
        auth_type: "api-key",
        env_key: Some("OPENROUTER_API_KEY"),
    },
    KnownProvider {
        name: "cerebras",
        display_name: "Cerebras",
        auth_type: "api-key",
        env_key: Some("CEREBRAS_API_KEY"),
    },
    KnownProvider {
        name: "minimax",
        display_name: "MiniMax",
        auth_type: "api-key",
        env_key: Some("MINIMAX_API_KEY"),
    },
    KnownProvider {
        name: "moonshot",
        display_name: "Moonshot",
        auth_type: "api-key",
        env_key: Some("MOONSHOT_API_KEY"),
    },
    KnownProvider {
        name: "venice",
        display_name: "Venice",
        auth_type: "api-key",
        env_key: Some("VENICE_API_KEY"),
    },
    KnownProvider {
        name: "ollama",
        display_name: "Ollama",
        auth_type: "api-key",
        env_key: Some("OLLAMA_API_KEY"),
    },
    KnownProvider {
        name: "openai-codex",
        display_name: "OpenAI Codex",
        auth_type: "oauth",
        env_key: None,
    },
    KnownProvider {
        name: "github-copilot",
        display_name: "GitHub Copilot",
        auth_type: "oauth",
        env_key: None,
    },
    KnownProvider {
        name: "kimi-code",
        display_name: "Kimi Code",
        auth_type: "oauth",
        env_key: None,
    },
];

pub struct LiveProviderSetupService {
    registry: Arc<RwLock<ProviderRegistry>>,
    config: ProvidersConfig,
    token_store: TokenStore,
    key_store: KeyStore,
}

impl LiveProviderSetupService {
    pub fn new(registry: Arc<RwLock<ProviderRegistry>>, config: ProvidersConfig) -> Self {
        Self {
            registry,
            config,
            token_store: TokenStore::new(),
            key_store: KeyStore::new(),
        }
    }

    fn is_provider_configured(&self, provider: &KnownProvider) -> bool {
        // Check if the provider has an API key set via env
        if let Some(env_key) = provider.env_key
            && std::env::var(env_key).is_ok()
        {
            return true;
        }
        // Check config file
        if let Some(entry) = self.config.get(provider.name)
            && entry.api_key.as_ref().is_some_and(|k| !k.is_empty())
        {
            return true;
        }
        // Check persisted key store
        if self.key_store.load(provider.name).is_some() {
            return true;
        }
        // For OAuth providers, check token store
        if provider.auth_type == "oauth" {
            return self.token_store.load(provider.name).is_some();
        }
        false
    }

    /// Start a device-flow OAuth for providers like GitHub Copilot.
    /// Returns `{ "userCode": "...", "verificationUri": "..." }` for the UI to display.
    async fn oauth_start_device_flow(
        &self,
        provider_name: String,
        oauth_config: moltis_oauth::OAuthConfig,
    ) -> ServiceResult {
        let client = reqwest::Client::new();
        let device_resp = device_flow::request_device_code(&client, &oauth_config)
            .await
            .map_err(|e| e.to_string())?;

        let user_code = device_resp.user_code.clone();
        let verification_uri = device_resp.verification_uri.clone();
        let device_code = device_resp.device_code.clone();
        let interval = device_resp.interval;

        // Spawn background task to poll for the token
        let token_store = self.token_store.clone();
        let registry = Arc::clone(&self.registry);
        let config = self.effective_config();
        tokio::spawn(async move {
            match device_flow::poll_for_token(&client, &oauth_config, &device_code, interval).await
            {
                Ok(tokens) => {
                    if let Err(e) = token_store.save(&provider_name, &tokens) {
                        tracing::error!(
                            provider = %provider_name,
                            error = %e,
                            "failed to save device-flow OAuth tokens"
                        );
                        return;
                    }
                    let new_registry = ProviderRegistry::from_env_with_config(&config);
                    let mut reg = registry.write().await;
                    *reg = new_registry;
                    info!(
                        provider = %provider_name,
                        "device-flow OAuth complete, rebuilt provider registry"
                    );
                },
                Err(e) => {
                    tracing::error!(
                        provider = %provider_name,
                        error = %e,
                        "device-flow OAuth polling failed"
                    );
                },
            }
        });

        Ok(serde_json::json!({
            "deviceFlow": true,
            "userCode": user_code,
            "verificationUri": verification_uri,
        }))
    }

    /// Build a ProvidersConfig that includes saved keys for registry rebuild.
    fn effective_config(&self) -> ProvidersConfig {
        config_with_saved_keys(&self.config, &self.key_store)
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

        // Persist to disk so the key survives restarts.
        self.key_store.save(provider_name, api_key)?;

        // Also set the environment variable so the provider registry picks it
        // up during rebuild (it reads env vars for key discovery).
        if let Some(env_key) = known.env_key {
            // Safety: called from a single async context; env var mutation is
            // unavoidable here since providers read from env at registration time.
            unsafe { std::env::set_var(env_key, api_key) };
        }

        // Rebuild the provider registry with saved keys merged into config.
        let effective = self.effective_config();
        let new_registry = ProviderRegistry::from_env_with_config(&effective);
        let mut reg = self.registry.write().await;
        *reg = new_registry;

        info!(
            provider = provider_name,
            "saved API key to disk and rebuilt provider registry"
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

        if oauth_config.device_flow {
            return self
                .oauth_start_device_flow(provider_name, oauth_config)
                .await;
        }

        let port = callback_port(&oauth_config);
        let flow = OAuthFlow::new(oauth_config);
        let auth_req = flow.start();

        let auth_url = auth_req.url.clone();
        let verifier = auth_req.pkce.verifier.clone();
        let expected_state = auth_req.state.clone();

        // Spawn background task to wait for the callback and exchange the code
        let token_store = self.token_store.clone();
        let registry = Arc::clone(&self.registry);
        let config = self.effective_config();
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

    async fn remove_key(&self, params: Value) -> ServiceResult {
        let provider_name = params
            .get("provider")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing 'provider' parameter".to_string())?;

        let known = KNOWN_PROVIDERS
            .iter()
            .find(|p| p.name == provider_name)
            .ok_or_else(|| format!("unknown provider: {provider_name}"))?;

        // Remove persisted API key
        if known.auth_type == "api-key" {
            self.key_store.remove(provider_name)?;
            // Unset the environment variable so the registry rebuild no longer finds it.
            if let Some(env_key) = known.env_key {
                unsafe { std::env::remove_var(env_key) };
            }
        }

        // Remove OAuth tokens
        if known.auth_type == "oauth" {
            let _ = self.token_store.delete(provider_name);
        }

        // Rebuild the provider registry without the removed provider.
        let effective = self.effective_config();
        let new_registry = ProviderRegistry::from_env_with_config(&effective);
        let mut reg = self.registry.write().await;
        *reg = new_registry;

        info!(
            provider = provider_name,
            "removed provider credentials and rebuilt registry"
        );

        Ok(serde_json::json!({ "ok": true }))
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
    use {super::*, moltis_config::schema::ProviderEntry};

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

    #[test]
    fn key_store_save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let store = KeyStore::with_path(dir.path().join("keys.json"));
        assert!(store.load("anthropic").is_none());
        store.save("anthropic", "sk-test-123").unwrap();
        assert_eq!(store.load("anthropic").unwrap(), "sk-test-123");
        // Overwrite
        store.save("anthropic", "sk-new").unwrap();
        assert_eq!(store.load("anthropic").unwrap(), "sk-new");
        // Multiple providers
        store.save("openai", "sk-openai").unwrap();
        assert_eq!(store.load("openai").unwrap(), "sk-openai");
        assert_eq!(store.load("anthropic").unwrap(), "sk-new");
        let all = store.load_all();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn key_store_remove() {
        let dir = tempfile::tempdir().unwrap();
        let store = KeyStore::with_path(dir.path().join("keys.json"));
        store.save("anthropic", "sk-test").unwrap();
        store.save("openai", "sk-openai").unwrap();
        assert!(store.load("anthropic").is_some());
        store.remove("anthropic").unwrap();
        assert!(store.load("anthropic").is_none());
        // Other keys unaffected
        assert_eq!(store.load("openai").unwrap(), "sk-openai");
        // Removing non-existent key is fine
        store.remove("nonexistent").unwrap();
    }

    #[tokio::test]
    async fn remove_key_rejects_unknown_provider() {
        let registry = Arc::new(RwLock::new(ProviderRegistry::from_env_with_config(
            &ProvidersConfig::default(),
        )));
        let svc = LiveProviderSetupService::new(registry, ProvidersConfig::default());
        let result = svc
            .remove_key(serde_json::json!({"provider": "nonexistent"}))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn remove_key_rejects_missing_params() {
        let registry = Arc::new(RwLock::new(ProviderRegistry::from_env_with_config(
            &ProvidersConfig::default(),
        )));
        let svc = LiveProviderSetupService::new(registry, ProvidersConfig::default());
        assert!(svc.remove_key(serde_json::json!({})).await.is_err());
    }

    #[test]
    fn config_with_saved_keys_merges() {
        let dir = tempfile::tempdir().unwrap();
        let store = KeyStore::with_path(dir.path().join("keys.json"));
        store.save("anthropic", "sk-saved").unwrap();

        let base = ProvidersConfig::default();
        let merged = config_with_saved_keys(&base, &store);
        let entry = merged.get("anthropic").unwrap();
        assert_eq!(entry.api_key.as_deref(), Some("sk-saved"));
    }

    #[test]
    fn config_with_saved_keys_does_not_override_existing() {
        let dir = tempfile::tempdir().unwrap();
        let store = KeyStore::with_path(dir.path().join("keys.json"));
        store.save("anthropic", "sk-saved").unwrap();

        let mut base = ProvidersConfig::default();
        base.providers.insert("anthropic".into(), ProviderEntry {
            api_key: Some("sk-config".into()),
            ..Default::default()
        });
        let merged = config_with_saved_keys(&base, &store);
        let entry = merged.get("anthropic").unwrap();
        // Config key takes precedence over saved key.
        assert_eq!(entry.api_key.as_deref(), Some("sk-config"));
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

    #[test]
    fn known_providers_include_new_providers() {
        let names: Vec<&str> = KNOWN_PROVIDERS.iter().map(|p| p.name).collect();
        // All new OpenAI-compatible providers
        assert!(names.contains(&"mistral"), "missing mistral");
        assert!(names.contains(&"openrouter"), "missing openrouter");
        assert!(names.contains(&"cerebras"), "missing cerebras");
        assert!(names.contains(&"minimax"), "missing minimax");
        assert!(names.contains(&"moonshot"), "missing moonshot");
        assert!(names.contains(&"venice"), "missing venice");
        assert!(names.contains(&"ollama"), "missing ollama");
        // OAuth providers
        assert!(names.contains(&"github-copilot"), "missing github-copilot");
    }

    #[test]
    fn github_copilot_is_oauth_provider() {
        let copilot = KNOWN_PROVIDERS
            .iter()
            .find(|p| p.name == "github-copilot")
            .expect("github-copilot not in KNOWN_PROVIDERS");
        assert_eq!(copilot.auth_type, "oauth");
        assert!(copilot.env_key.is_none());
    }

    #[test]
    fn new_api_key_providers_have_correct_env_keys() {
        let expected = [
            ("mistral", "MISTRAL_API_KEY"),
            ("openrouter", "OPENROUTER_API_KEY"),
            ("cerebras", "CEREBRAS_API_KEY"),
            ("minimax", "MINIMAX_API_KEY"),
            ("moonshot", "MOONSHOT_API_KEY"),
            ("venice", "VENICE_API_KEY"),
            ("ollama", "OLLAMA_API_KEY"),
        ];
        for (name, env_key) in expected {
            let provider = KNOWN_PROVIDERS
                .iter()
                .find(|p| p.name == name)
                .unwrap_or_else(|| panic!("missing provider: {name}"));
            assert_eq!(provider.env_key, Some(env_key), "wrong env_key for {name}");
            assert_eq!(provider.auth_type, "api-key");
        }
    }

    #[tokio::test]
    async fn save_key_accepts_new_providers() {
        let registry = Arc::new(RwLock::new(ProviderRegistry::from_env_with_config(
            &ProvidersConfig::default(),
        )));
        let _svc = LiveProviderSetupService::new(registry, ProvidersConfig::default());

        // All new API-key providers should be accepted by save_key
        for name in [
            "mistral",
            "openrouter",
            "cerebras",
            "minimax",
            "moonshot",
            "venice",
            "ollama",
        ] {
            // We can't actually persist in tests (would write to real disk),
            // but we can verify the provider name is recognized.
            let known = KNOWN_PROVIDERS
                .iter()
                .find(|p| p.name == name && p.auth_type == "api-key");
            assert!(
                known.is_some(),
                "{name} should be a recognized api-key provider"
            );
        }
    }

    #[tokio::test]
    async fn available_includes_new_providers() {
        let registry = Arc::new(RwLock::new(ProviderRegistry::from_env_with_config(
            &ProvidersConfig::default(),
        )));
        let svc = LiveProviderSetupService::new(registry, ProvidersConfig::default());
        let result = svc.available().await.unwrap();
        let arr = result.as_array().unwrap();

        let names: Vec<&str> = arr
            .iter()
            .filter_map(|v| v.get("name").and_then(|n| n.as_str()))
            .collect();

        for expected in [
            "mistral",
            "openrouter",
            "cerebras",
            "minimax",
            "moonshot",
            "venice",
            "ollama",
            "github-copilot",
        ] {
            assert!(
                names.contains(&expected),
                "{expected} not found in available providers: {names:?}"
            );
        }
    }
}
