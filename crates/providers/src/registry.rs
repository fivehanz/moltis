//! Provider registry: model registration, lookup, discovery, and lifecycle.

use std::{
    collections::{HashMap, HashSet},
    future::Future,
    pin::Pin,
    sync::Arc,
};

use {
    moltis_config::schema::{ProviderStreamTransport, ProvidersConfig},
    secrecy::ExposeSecret,
    tokio_stream::Stream,
};

use moltis_agents::model::{ChatMessage, LlmProvider, StreamEvent};

use crate::{
    anthropic,
    config_helpers::{
        configured_models_for_provider, env_value, normalize_unique_models,
        oauth_discovery_enabled, resolve_api_key, should_fetch_models,
        subscription_preference_rank,
    },
    discovered_model::{
        catalog_to_discovered, merge_discovered_with_fallback_catalog,
        merge_preferred_and_discovered_models, DiscoveredModel,
    },
    model_capabilities::{ModelCapabilities, ModelInfo},
    model_catalogs::{ANTHROPIC_MODELS, OPENAI_COMPAT_PROVIDERS},
    model_id::{
        namespaced_model_id, raw_model_id, split_reasoning_suffix, REASONING_SUFFIXES,
        REASONING_SUFFIX_SEP,
    },
    ollama::{
        self, OllamaShowResponse, probe_ollama_models_batch, probe_ollama_models_batch_async,
        resolve_ollama_tool_mode,
    },
    openai,
};

struct RegistryModelProvider {
    model_id: String,
    inner: Arc<dyn LlmProvider>,
}

#[async_trait::async_trait]
impl LlmProvider for RegistryModelProvider {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn id(&self) -> &str {
        &self.model_id
    }

    async fn complete(
        &self,
        messages: &[ChatMessage],
        tools: &[serde_json::Value],
    ) -> anyhow::Result<moltis_agents::model::CompletionResponse> {
        self.inner.complete(messages, tools).await
    }

    fn supports_tools(&self) -> bool {
        self.inner.supports_tools()
    }

    fn tool_mode(&self) -> Option<moltis_config::ToolMode> {
        self.inner.tool_mode()
    }

    fn context_window(&self) -> u32 {
        self.inner.context_window()
    }

    fn supports_vision(&self) -> bool {
        self.inner.supports_vision()
    }

    fn stream(
        &self,
        messages: Vec<ChatMessage>,
    ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + '_>> {
        self.inner.stream(messages)
    }

    fn stream_with_tools(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<serde_json::Value>,
    ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + '_>> {
        self.inner.stream_with_tools(messages, tools)
    }

    fn reasoning_effort(&self) -> Option<moltis_agents::model::ReasoningEffort> {
        self.inner.reasoning_effort()
    }

    fn with_reasoning_effort(
        self: Arc<Self>,
        effort: moltis_agents::model::ReasoningEffort,
    ) -> Option<Arc<dyn LlmProvider>> {
        let new_inner = Arc::clone(&self.inner).with_reasoning_effort(effort)?;
        Some(Arc::new(RegistryModelProvider {
            model_id: self.model_id.clone(),
            inner: new_inner,
        }))
    }
}

fn anthropic_fallback_catalog() -> Vec<DiscoveredModel> {
    catalog_to_discovered(ANTHROPIC_MODELS, 3)
}

/// Result of a runtime model rediscovery pass.
///
/// Bundles the discovered model lists together with any Ollama `/api/show`
/// probe results so that registration can proceed without further I/O.
/// Ollama probe data is opaque — callers pass this struct directly to
/// [`ProviderRegistry::register_rediscovered_models`].
pub struct RediscoveryResult {
    /// Models discovered per provider (keyed by config name).
    pub(crate) models: HashMap<String, Vec<DiscoveredModel>>,
    /// Ollama `/api/show` probe metadata (keyed by model ID).
    pub(crate) ollama_probes: HashMap<String, OllamaShowResponse>,
}

impl RediscoveryResult {
    /// Returns `true` when no models were discovered across all providers.
    pub fn is_empty(&self) -> bool {
        self.models.values().all(|v| v.is_empty())
    }
}

/// Asynchronously fetch models from all discoverable provider APIs.
///
/// Runs `/v1/models` (or Ollama `/api/tags`) for each eligible provider
/// concurrently, then batch-probes any discovered Ollama models via
/// `/api/show` for tool-mode metadata. Returns everything needed for
/// lock-free registration.
///
/// `provider_filter` narrows the scope to a single provider name (case-
/// insensitive comparison against config name or alias).
pub async fn fetch_discoverable_models(
    config: &ProvidersConfig,
    env_overrides: &HashMap<String, String>,
    provider_filter: Option<&str>,
) -> RediscoveryResult {
    use futures::future::join_all;

    let filter_matches =
        |name: &str| -> bool { provider_filter.is_none_or(|f| f.eq_ignore_ascii_case(name)) };

    let mut tasks: Vec<(
        String,
        Pin<Box<dyn Future<Output = anyhow::Result<Vec<DiscoveredModel>>> + Send>>,
    )> = Vec::new();

    // ── OpenAI builtin ────────────────────────────────────────────────
    if filter_matches("openai")
        && config.is_enabled("openai")
        && !cfg!(test)
        && let Some(key) = resolve_api_key(config, "openai", "OPENAI_API_KEY", env_overrides)
        && should_fetch_models(config, "openai")
    {
        let base_url = config
            .get("openai")
            .and_then(|e| e.base_url.clone())
            .or_else(|| env_value(env_overrides, "OPENAI_BASE_URL"))
            .unwrap_or_else(|| "https://api.openai.com/v1".into());
        tasks.push((
            "openai".into(),
            Box::pin(openai::fetch_models_from_api(key, base_url)),
        ));
    }

    // ── Anthropic builtin ─────────────────────────────────────────────
    if filter_matches("anthropic")
        && config.is_enabled("anthropic")
        && !cfg!(test)
        && let Some(key) = resolve_api_key(config, "anthropic", "ANTHROPIC_API_KEY", env_overrides)
        && should_fetch_models(config, "anthropic")
    {
        let base_url = config
            .get("anthropic")
            .and_then(|e| e.base_url.clone())
            .or_else(|| env_value(env_overrides, "ANTHROPIC_BASE_URL"))
            .unwrap_or_else(|| "https://api.anthropic.com".into());
        tasks.push((
            "anthropic".into(),
            Box::pin(anthropic::fetch_models_from_api(key, base_url)),
        ));
    }

    // ── OpenAI-compatible providers ───────────────────────────────────
    for def in OPENAI_COMPAT_PROVIDERS {
        if !filter_matches(def.config_name) || !config.is_enabled(def.config_name) {
            continue;
        }

        let key = resolve_api_key(config, def.config_name, def.env_key, env_overrides);
        let key = if !def.requires_api_key {
            key.or_else(|| Some(secrecy::Secret::new(def.config_name.into())))
        } else if def.config_name == "gemini" {
            key.or_else(|| env_value(env_overrides, "GOOGLE_API_KEY").map(secrecy::Secret::new))
        } else {
            key
        };
        let Some(key) = key else {
            continue;
        };

        let base_url = config
            .get(def.config_name)
            .and_then(|e| e.base_url.clone())
            .or_else(|| env_value(env_overrides, def.env_base_url_key))
            .unwrap_or_else(|| def.default_base_url.into());

        if def.local_only {
            let has_explicit_entry = config.get(def.config_name).is_some();
            let has_env_base_url = env_value(env_overrides, def.env_base_url_key).is_some();
            let preferred = configured_models_for_provider(config, def.config_name);
            if !has_explicit_entry && !has_env_base_url && preferred.is_empty() {
                continue;
            }
        }

        let user_opted_in = config
            .get(def.config_name)
            .is_some_and(|entry| entry.fetch_models);
        let try_fetch = def.supports_model_discovery || user_opted_in;
        if !try_fetch || !should_fetch_models(config, def.config_name) {
            continue;
        }

        if def.config_name == "ollama" {
            tasks.push((
                def.config_name.into(),
                Box::pin(ollama::discover_ollama_models_from_api(base_url)),
            ));
        } else {
            tasks.push((
                def.config_name.into(),
                Box::pin(openai::fetch_models_from_api(key, base_url)),
            ));
        }
    }

    // ── Custom providers ──────────────────────────────────────────────
    for (name, entry) in &config.providers {
        if !name.starts_with("custom-") || !entry.enabled {
            continue;
        }
        if !filter_matches(name) {
            continue;
        }
        let Some(api_key) = entry
            .api_key
            .as_ref()
            .filter(|k| !k.expose_secret().is_empty())
        else {
            continue;
        };
        let Some(base_url) = entry.base_url.as_ref().filter(|u| !u.trim().is_empty()) else {
            continue;
        };
        if should_fetch_models(config, name) {
            tasks.push((
                name.clone(),
                Box::pin(openai::fetch_models_from_api(
                    api_key.clone(),
                    base_url.clone(),
                )),
            ));
        }
    }

    // Run all fetches concurrently.
    let names: Vec<String> = tasks.iter().map(|(n, _)| n.clone()).collect();
    let futures: Vec<_> = tasks.into_iter().map(|(_, fut)| fut).collect();
    let results = join_all(futures).await;

    let mut map = HashMap::new();
    for (name, result) in names.into_iter().zip(results) {
        match result {
            Ok(models) => {
                tracing::debug!(
                    provider = %name,
                    model_count = models.len(),
                    "runtime model rediscovery succeeded"
                );
                map.insert(name, models);
            },
            Err(err) => {
                tracing::debug!(
                    provider = %name,
                    error = %err,
                    "runtime model rediscovery failed"
                );
            },
        }
    }

    // Batch-probe any newly discovered Ollama models for `/api/show` metadata
    // (tool capabilities, family info). Runs outside any registry lock.
    let ollama_probes = if let Some(ollama_models) = map.get("ollama") {
        let ollama_base_url = config
            .get("ollama")
            .and_then(|e| e.base_url.clone())
            .or_else(|| env_value(env_overrides, "OLLAMA_BASE_URL"))
            .unwrap_or_else(|| "http://localhost:11434".into());
        probe_ollama_models_batch_async(&ollama_base_url, ollama_models).await
    } else {
        HashMap::new()
    };

    RediscoveryResult {
        models: map,
        ollama_probes,
    }
}

#[cfg(any(feature = "provider-openai-codex", feature = "provider-github-copilot"))]
trait DynamicModelDiscovery {
    fn provider_name(&self) -> &'static str;
    fn is_enabled_and_authenticated(&self, config: &ProvidersConfig) -> bool;
    fn configured_models(&self, config: &ProvidersConfig) -> Vec<String>;
    fn should_fetch_models(&self, config: &ProvidersConfig) -> bool;
    fn live_models(&self) -> anyhow::Result<Vec<DiscoveredModel>>;
    fn build_provider(&self, model_id: String, config: &ProvidersConfig) -> Arc<dyn LlmProvider>;
    fn display_name(&self, model_id: &str, discovered: &str) -> String;
}

#[cfg(feature = "provider-openai-codex")]
struct OpenAiCodexDiscovery;

#[cfg(feature = "provider-openai-codex")]
impl DynamicModelDiscovery for OpenAiCodexDiscovery {
    fn provider_name(&self) -> &'static str {
        "openai-codex"
    }

    fn is_enabled_and_authenticated(&self, config: &ProvidersConfig) -> bool {
        use crate::openai_codex;
        oauth_discovery_enabled(config, self.provider_name()) && openai_codex::has_stored_tokens()
    }

    fn configured_models(&self, config: &ProvidersConfig) -> Vec<String> {
        configured_models_for_provider(config, self.provider_name())
    }

    fn should_fetch_models(&self, config: &ProvidersConfig) -> bool {
        should_fetch_models(config, self.provider_name())
    }

    fn live_models(&self) -> anyhow::Result<Vec<DiscoveredModel>> {
        use crate::openai_codex;
        openai_codex::live_models()
    }

    fn build_provider(&self, model_id: String, config: &ProvidersConfig) -> Arc<dyn LlmProvider> {
        use crate::openai_codex;
        let stream_transport = config
            .get(self.provider_name())
            .map(|entry| entry.stream_transport)
            .unwrap_or(ProviderStreamTransport::Sse);
        Arc::new(openai_codex::OpenAiCodexProvider::new_with_transport(
            model_id,
            stream_transport,
        ))
    }

    fn display_name(&self, _model_id: &str, discovered: &str) -> String {
        format!("{discovered} (Codex/OAuth)")
    }
}

#[cfg(feature = "provider-github-copilot")]
struct GitHubCopilotDiscovery;

#[cfg(feature = "provider-github-copilot")]
impl DynamicModelDiscovery for GitHubCopilotDiscovery {
    fn provider_name(&self) -> &'static str {
        "github-copilot"
    }

    fn is_enabled_and_authenticated(&self, config: &ProvidersConfig) -> bool {
        use crate::github_copilot;
        oauth_discovery_enabled(config, self.provider_name()) && github_copilot::has_stored_tokens()
    }

    fn configured_models(&self, config: &ProvidersConfig) -> Vec<String> {
        configured_models_for_provider(config, self.provider_name())
    }

    fn should_fetch_models(&self, config: &ProvidersConfig) -> bool {
        should_fetch_models(config, self.provider_name())
    }

    fn live_models(&self) -> anyhow::Result<Vec<DiscoveredModel>> {
        use crate::github_copilot;
        github_copilot::live_models()
    }

    fn build_provider(&self, model_id: String, _config: &ProvidersConfig) -> Arc<dyn LlmProvider> {
        use crate::github_copilot;
        Arc::new(github_copilot::GitHubCopilotProvider::new(model_id))
    }

    fn display_name(&self, _model_id: &str, discovered: &str) -> String {
        if discovered.to_ascii_lowercase().contains("copilot") {
            discovered.to_string()
        } else {
            format!("{discovered} (Copilot)")
        }
    }
}

/// Registry of available LLM providers, keyed by namespaced model ID.
pub struct ProviderRegistry {
    providers: HashMap<String, Arc<dyn LlmProvider>>,
    models: Vec<ModelInfo>,
}

/// Pending model discovery handles returned by [`ProviderRegistry::fire_discoveries`].
pub type PendingDiscoveries = Vec<(
    String,
    std::sync::mpsc::Receiver<anyhow::Result<Vec<DiscoveredModel>>>,
)>;

impl ProviderRegistry {
    #[must_use]
    pub fn empty() -> Self {
        Self {
            providers: HashMap::new(),
            models: Vec::new(),
        }
    }

    fn has_provider_model(&self, provider: &str, model_id: &str) -> bool {
        self.providers
            .contains_key(&namespaced_model_id(provider, model_id))
    }

    /// Check if the raw (un-namespaced) model ID is registered under any provider.
    fn has_model_any_provider(&self, model_id: &str) -> bool {
        let raw = raw_model_id(model_id);
        self.models.iter().any(|m| raw_model_id(&m.id) == raw)
    }

    fn resolve_registry_model_id(
        &self,
        model_id: &str,
        provider_hint: Option<&str>,
    ) -> Option<String> {
        if self.providers.contains_key(model_id) {
            return Some(model_id.to_string());
        }

        let raw = raw_model_id(model_id);
        self.models
            .iter()
            .enumerate()
            .filter(|(_, m)| raw_model_id(&m.id) == raw)
            .filter(|(_, m)| provider_hint.is_none_or(|hint| m.provider == hint))
            .min_by_key(|(idx, m)| (subscription_preference_rank(&m.provider), *idx))
            .map(|(_, m)| m.id.clone())
    }

    #[cfg(any(feature = "provider-openai-codex", feature = "provider-github-copilot"))]
    #[allow(clippy::vec_init_then_push)]
    fn dynamic_discovery_sources() -> Vec<Box<dyn DynamicModelDiscovery>> {
        let mut sources: Vec<Box<dyn DynamicModelDiscovery>> = Vec::new();
        #[cfg(feature = "provider-openai-codex")]
        sources.push(Box::new(OpenAiCodexDiscovery));
        #[cfg(feature = "provider-github-copilot")]
        sources.push(Box::new(GitHubCopilotDiscovery));
        sources
    }

    #[cfg(any(feature = "provider-openai-codex", feature = "provider-github-copilot"))]
    fn desired_models_for_dynamic_source(
        source: &dyn DynamicModelDiscovery,
        config: &ProvidersConfig,
        catalog: Vec<DiscoveredModel>,
    ) -> Option<Vec<DiscoveredModel>> {
        if !source.is_enabled_and_authenticated(config) {
            return None;
        }

        let preferred = source.configured_models(config);
        Some(merge_preferred_and_discovered_models(preferred, catalog))
    }

    #[cfg(any(feature = "provider-openai-codex", feature = "provider-github-copilot"))]
    fn register_dynamic_source_models(
        &mut self,
        source: &dyn DynamicModelDiscovery,
        config: &ProvidersConfig,
        catalog: Vec<DiscoveredModel>,
    ) {
        let Some(models) = Self::desired_models_for_dynamic_source(source, config, catalog) else {
            return;
        };

        for model in models {
            if self.has_provider_model(source.provider_name(), &model.id) {
                continue;
            }
            let provider = source.build_provider(model.id.clone(), config);
            self.register(
                ModelInfo {
                    id: model.id.clone(),
                    provider: source.provider_name().to_string(),
                    display_name: source.display_name(&model.id, &model.display_name),
                    created_at: model.created_at,
                    recommended: model.recommended,
                    capabilities: model
                        .capabilities
                        .unwrap_or_else(|| ModelCapabilities::infer(&model.id)),
                },
                provider,
            );
        }
    }

    #[cfg(any(feature = "provider-openai-codex", feature = "provider-github-copilot"))]
    fn refresh_dynamic_source_models(
        &mut self,
        source: &dyn DynamicModelDiscovery,
        config: &ProvidersConfig,
    ) -> bool {
        if !source.is_enabled_and_authenticated(config) {
            return false;
        }
        if !source.should_fetch_models(config) {
            return false;
        }

        let live_catalog = match source.live_models() {
            Ok(models) => models,
            Err(err) => {
                tracing::warn!(
                    provider = source.provider_name(),
                    error = %err,
                    "skipping dynamic model refresh because live fetch failed"
                );
                return false;
            },
        };

        let Some(next_models) =
            Self::desired_models_for_dynamic_source(source, config, live_catalog)
        else {
            return false;
        };

        let new_entries: Vec<(ModelInfo, Arc<dyn LlmProvider>)> = next_models
            .into_iter()
            .map(|model| {
                let caps = model
                    .capabilities
                    .unwrap_or_else(|| ModelCapabilities::infer(&model.id));
                (
                    ModelInfo {
                        id: model.id.clone(),
                        provider: source.provider_name().to_string(),
                        display_name: source.display_name(&model.id, &model.display_name),
                        created_at: model.created_at,
                        recommended: model.recommended,
                        capabilities: caps,
                    },
                    source.build_provider(model.id, config),
                )
            })
            .collect();

        // Replace stale provider entries atomically only after successful fetch.
        let stale_ids: Vec<String> = self
            .models
            .iter()
            .filter(|m| m.provider == source.provider_name())
            .map(|m| m.id.clone())
            .collect();
        for model_id in &stale_ids {
            self.providers.remove(model_id);
        }
        self.models.retain(|m| m.provider != source.provider_name());
        for (info, provider) in new_entries {
            self.register(info, provider);
        }

        true
    }

    fn desired_anthropic_models(
        config: &ProvidersConfig,
        prefetched: &HashMap<String, Vec<DiscoveredModel>>,
    ) -> Vec<DiscoveredModel> {
        let preferred = configured_models_for_provider(config, "anthropic");
        let discovered = if should_fetch_models(config, "anthropic") {
            match prefetched.get("anthropic") {
                Some(live) => live.clone(),
                None => anthropic_fallback_catalog(),
            }
        } else {
            Vec::new()
        };
        merge_preferred_and_discovered_models(preferred, discovered)
    }

    fn register_anthropic_catalog(
        &mut self,
        models: Vec<DiscoveredModel>,
        key: &secrecy::Secret<String>,
        base_url: &str,
        provider_label: &str,
        alias: Option<String>,
        cache_retention: moltis_config::CacheRetention,
    ) -> usize {
        let mut added = 0usize;

        for model in models {
            let caps = model
                .capabilities
                .unwrap_or_else(|| ModelCapabilities::infer(&model.id));
            let (model_id, display_name, created_at, recommended) = (
                model.id,
                model.display_name,
                model.created_at,
                model.recommended,
            );
            if self.has_provider_model(provider_label, &model_id) {
                continue;
            }
            let provider = Arc::new(
                anthropic::AnthropicProvider::with_alias(
                    key.clone(),
                    model_id.clone(),
                    base_url.to_string(),
                    alias.clone(),
                )
                .with_cache_retention(cache_retention),
            );
            self.register(
                ModelInfo {
                    id: model_id,
                    provider: provider_label.to_string(),
                    display_name,
                    created_at,
                    recommended,
                    capabilities: caps,
                },
                provider,
            );
            added += 1;
        }

        added
    }

    fn replace_anthropic_catalog(
        &mut self,
        models: Vec<DiscoveredModel>,
        key: &secrecy::Secret<String>,
        base_url: &str,
        provider_label: &str,
        alias: Option<String>,
        cache_retention: moltis_config::CacheRetention,
    ) -> usize {
        let new_entries: Vec<(ModelInfo, Arc<dyn LlmProvider>)> = models
            .into_iter()
            .map(|model| {
                let caps = model
                    .capabilities
                    .unwrap_or_else(|| ModelCapabilities::infer(&model.id));
                let provider = Arc::new(
                    anthropic::AnthropicProvider::with_alias(
                        key.clone(),
                        model.id.clone(),
                        base_url.to_string(),
                        alias.clone(),
                    )
                    .with_cache_retention(cache_retention),
                );
                (
                    ModelInfo {
                        id: model.id,
                        provider: provider_label.to_string(),
                        display_name: model.display_name,
                        created_at: model.created_at,
                        recommended: model.recommended,
                        capabilities: caps,
                    },
                    provider as Arc<dyn LlmProvider>,
                )
            })
            .collect();

        let previous_ids: HashSet<String> = self
            .models
            .iter()
            .filter(|m| m.provider == provider_label)
            .map(|m| m.id.clone())
            .collect();

        self.models.retain(|m| m.provider != provider_label);
        self.providers.retain(|id, _| !previous_ids.contains(id));

        let next_ids: HashSet<String> = new_entries
            .iter()
            .map(|(info, _)| namespaced_model_id(provider_label, raw_model_id(&info.id)))
            .collect();

        for (info, provider) in new_entries {
            self.register(info, provider);
        }

        next_ids.difference(&previous_ids).count()
    }

    /// Register a provider manually.
    pub fn register(&mut self, mut info: ModelInfo, provider: Arc<dyn LlmProvider>) {
        let model_id = raw_model_id(&info.id).to_string();
        let registry_model_id = namespaced_model_id(&info.provider, &model_id);
        info.id = registry_model_id.clone();
        let wrapped: Arc<dyn LlmProvider> = Arc::new(RegistryModelProvider {
            model_id: registry_model_id.clone(),
            inner: provider,
        });
        self.providers.insert(registry_model_id, wrapped);
        self.models.push(info);
    }

    /// Unregister a provider by model ID. Returns true if it was removed.
    pub fn unregister(&mut self, model_id: &str) -> bool {
        let resolved_id = self.resolve_registry_model_id(model_id, None);
        let removed = resolved_id
            .as_deref()
            .and_then(|id| self.providers.remove(id))
            .is_some();
        if removed && let Some(id) = resolved_id {
            self.models.retain(|m| m.id != id);
        }
        removed
    }

    /// Auto-discover providers from environment variables.
    /// Uses default config (all providers enabled).
    pub fn from_env() -> Self {
        Self::from_env_with_config(&ProvidersConfig::default())
    }

    /// Auto-discover providers from environment variables,
    /// respecting the given config for enable/disable and overrides.
    ///
    /// Provider registration order:
    /// 1. Built-in raw reqwest providers (always available, support tool calling)
    /// 2. async-openai-backed providers (if `provider-async-openai` feature enabled)
    /// 3. genai-backed providers (if `provider-genai` feature enabled, no tool support)
    /// 4. OpenAI Codex OAuth providers (if `provider-openai-codex` feature enabled)
    ///
    /// Model/provider auto-selection preference:
    /// 1. Subscription providers (`openai-codex`, `github-copilot`)
    /// 2. Everything else
    ///
    /// Within the same preference tier, registration order wins.
    pub fn from_env_with_config(config: &ProvidersConfig) -> Self {
        let env_overrides = HashMap::new();
        Self::from_env_with_config_and_overrides(config, &env_overrides)
    }

    /// Auto-discover providers from config, process env, and optional env
    /// overrides. Process env always wins when both are present.
    ///
    /// Model discovery HTTP requests are fired concurrently in Phase 1,
    /// collected in Phase 2, and the results are used to register providers
    /// in Phase 3. This reduces startup time from `sum(latencies)` to
    /// `max(latencies)`.
    pub fn from_env_with_config_and_overrides(
        config: &ProvidersConfig,
        env_overrides: &HashMap<String, String>,
    ) -> Self {
        let pending = Self::fire_discoveries(config, env_overrides);
        let prefetched = Self::collect_discoveries(pending);
        Self::from_config_with_prefetched(config, env_overrides, &prefetched)
    }

    /// Register providers without making any discovery HTTP requests.
    ///
    /// This uses static model catalogs plus any explicit/pinned models from
    /// config and env overrides.
    pub fn from_config_with_static_catalogs(
        config: &ProvidersConfig,
        env_overrides: &HashMap<String, String>,
    ) -> Self {
        let prefetched = HashMap::new();
        Self::from_config_with_prefetched(config, env_overrides, &prefetched)
    }

    /// Register providers using already-collected discovery results.
    ///
    /// `prefetched` should come from [`collect_discoveries`], but callers may
    /// also pass an empty map to register only static catalogs.
    pub fn from_config_with_prefetched(
        config: &ProvidersConfig,
        env_overrides: &HashMap<String, String>,
        prefetched: &HashMap<String, Vec<DiscoveredModel>>,
    ) -> Self {
        let mut reg = Self::empty();

        // Built-in providers first: they support tool calling.
        reg.register_builtin_providers(config, env_overrides, prefetched);
        reg.register_openai_compatible_providers(config, env_overrides, prefetched);
        reg.register_custom_providers(config, prefetched);

        #[cfg(feature = "provider-async-openai")]
        {
            reg.register_async_openai_providers(config, env_overrides);
        }

        // GenAI providers last: they don't support tool calling,
        // so they only fill in models not already covered above.
        #[cfg(feature = "provider-genai")]
        {
            reg.register_genai_providers(config, env_overrides);
        }

        #[cfg(feature = "provider-openai-codex")]
        {
            reg.register_openai_codex_providers(config, prefetched);
        }

        #[cfg(feature = "provider-github-copilot")]
        {
            reg.register_github_copilot_providers(config, prefetched);
        }

        #[cfg(feature = "provider-kimi-code")]
        {
            reg.register_kimi_code_providers(config, env_overrides);
        }

        // Local GGUF providers (no API key needed, model runs locally)
        #[cfg(feature = "local-llm")]
        {
            reg.register_local_gguf_providers(config);
        }

        reg
    }

    /// Fire all provider model discovery HTTP requests concurrently.
    ///
    /// Returns a vec of `(provider_key, Receiver)` handles. Each receiver
    /// will eventually yield the discovered model list. Call
    /// [`collect_discoveries`] to drain them (blocking).
    #[allow(unused_mut)] // `pending` may be unused when features are disabled
    pub fn fire_discoveries(
        config: &ProvidersConfig,
        env_overrides: &HashMap<String, String>,
    ) -> PendingDiscoveries {
        let mut pending: PendingDiscoveries = Vec::new();

        // ── OpenAI builtin ───────────────────────────────────────────────
        if config.is_enabled("openai")
            && !cfg!(test)
            && let Some(key) = resolve_api_key(config, "openai", "OPENAI_API_KEY", env_overrides)
            && should_fetch_models(config, "openai")
        {
            let base_url = config
                .get("openai")
                .and_then(|e| e.base_url.clone())
                .or_else(|| env_value(env_overrides, "OPENAI_BASE_URL"))
                .unwrap_or_else(|| "https://api.openai.com/v1".into());
            pending.push((
                "openai".into(),
                openai::start_model_discovery(key.clone(), base_url),
            ));
        }

        // ── Anthropic builtin ───────────────────────────────────────────
        if config.is_enabled("anthropic")
            && !cfg!(test)
            && let Some(key) =
                resolve_api_key(config, "anthropic", "ANTHROPIC_API_KEY", env_overrides)
            && should_fetch_models(config, "anthropic")
        {
            let base_url = config
                .get("anthropic")
                .and_then(|e| e.base_url.clone())
                .or_else(|| env_value(env_overrides, "ANTHROPIC_BASE_URL"))
                .unwrap_or_else(|| "https://api.anthropic.com".into());
            pending.push((
                "anthropic".into(),
                anthropic::start_model_discovery(key.clone(), base_url),
            ));
        }

        // ── OpenAI-compatible providers ──────────────────────────────────
        for def in OPENAI_COMPAT_PROVIDERS {
            if !config.is_enabled(def.config_name) {
                continue;
            }

            let key = resolve_api_key(config, def.config_name, def.env_key, env_overrides);
            let key = if !def.requires_api_key {
                key.or_else(|| Some(secrecy::Secret::new(def.config_name.into())))
            } else if def.config_name == "gemini" {
                key.or_else(|| env_value(env_overrides, "GOOGLE_API_KEY").map(secrecy::Secret::new))
            } else {
                key
            };

            let Some(key) = key else {
                continue;
            };

            let base_url = config
                .get(def.config_name)
                .and_then(|e| e.base_url.clone())
                .or_else(|| env_value(env_overrides, def.env_base_url_key))
                .unwrap_or_else(|| def.default_base_url.into());

            let preferred = configured_models_for_provider(config, def.config_name);

            if def.local_only {
                let has_explicit_entry = config.get(def.config_name).is_some();
                let has_env_base_url = env_value(env_overrides, def.env_base_url_key).is_some();
                if !has_explicit_entry && !has_env_base_url && preferred.is_empty() {
                    continue;
                }
            }

            let skip_discovery = def.models.is_empty()
                && preferred.is_empty()
                && !def.local_only
                && (def.config_name == "venice" || cfg!(test));
            let user_opted_in = config
                .get(def.config_name)
                .is_some_and(|entry| entry.fetch_models);
            let try_fetch = def.supports_model_discovery || user_opted_in;

            if !skip_discovery && try_fetch && should_fetch_models(config, def.config_name) {
                if def.config_name == "ollama" {
                    pending.push((
                        def.config_name.into(),
                        ollama::start_ollama_discovery(&base_url),
                    ));
                } else {
                    pending.push((
                        def.config_name.into(),
                        openai::start_model_discovery(key.clone(), base_url),
                    ));
                }
            }
        }

        // ── Custom providers ─────────────────────────────────────────────
        for (name, entry) in &config.providers {
            if !name.starts_with("custom-") || !entry.enabled {
                continue;
            }
            let Some(api_key) = entry
                .api_key
                .as_ref()
                .filter(|k| !k.expose_secret().is_empty())
            else {
                continue;
            };
            let Some(base_url) = entry.base_url.as_ref().filter(|u| !u.trim().is_empty()) else {
                continue;
            };
            let has_explicit_models = !configured_models_for_provider(config, name).is_empty();
            if !has_explicit_models && should_fetch_models(config, name) {
                pending.push((
                    name.clone(),
                    openai::start_model_discovery(api_key.clone(), base_url.clone()),
                ));
            }
        }

        // ── OpenAI Codex ─────────────────────────────────────────────────
        #[cfg(feature = "provider-openai-codex")]
        if oauth_discovery_enabled(config, "openai-codex")
            && crate::openai_codex::has_stored_tokens()
            && should_fetch_models(config, "openai-codex")
            && let Some(rx) = crate::openai_codex::start_model_discovery()
        {
            pending.push(("openai-codex".into(), rx));
        }

        // ── GitHub Copilot ───────────────────────────────────────────────
        #[cfg(feature = "provider-github-copilot")]
        if oauth_discovery_enabled(config, "github-copilot")
            && crate::github_copilot::has_stored_tokens()
            && should_fetch_models(config, "github-copilot")
        {
            pending.push((
                "github-copilot".into(),
                crate::github_copilot::start_model_discovery(),
            ));
        }

        pending
    }

    /// Drain all pending discovery receivers (blocking on each `recv()`).
    ///
    /// Returns a map from provider name to discovered models.
    pub fn collect_discoveries(
        pending: PendingDiscoveries,
    ) -> HashMap<String, Vec<DiscoveredModel>> {
        let mut results: HashMap<String, Vec<DiscoveredModel>> = HashMap::new();
        for (key, rx) in pending {
            match rx.recv() {
                Ok(Ok(models)) => {
                    tracing::debug!(
                        provider = %key,
                        model_count = models.len(),
                        "parallel model discovery succeeded"
                    );
                    results.insert(key, models);
                },
                Ok(Err(err)) => {
                    let msg = err.to_string();
                    if msg.contains("not logged in")
                        || msg.contains("tokens not found")
                        || msg.contains("not configured")
                    {
                        tracing::debug!(
                            provider = %key,
                            error = %err,
                            "provider not configured, skipping model discovery"
                        );
                    } else {
                        tracing::warn!(
                            provider = %key,
                            error = %err,
                            "parallel model discovery failed"
                        );
                    }
                },
                Err(err) => {
                    tracing::warn!(
                        provider = %key,
                        error = %err,
                        "parallel model discovery worker crashed"
                    );
                },
            }
        }
        results
    }

    /// Register models from a [`RediscoveryResult`], skipping those already
    /// present. All I/O (model list fetches, Ollama probes) must be completed
    /// before calling this — it only does fast in-memory work.
    ///
    /// Returns the number of newly registered models.
    pub fn register_rediscovered_models(
        &mut self,
        config: &ProvidersConfig,
        env_overrides: &HashMap<String, String>,
        result: &RediscoveryResult,
    ) -> usize {
        let fetched = &result.models;
        let mut added = 0usize;

        // ── Anthropic builtin ─────────────────────────────────────────
        if fetched.contains_key("anthropic")
            && config.is_enabled("anthropic")
            && let Some(key) =
                resolve_api_key(config, "anthropic", "ANTHROPIC_API_KEY", env_overrides)
        {
            let base_url = config
                .get("anthropic")
                .and_then(|e| e.base_url.clone())
                .or_else(|| env_value(env_overrides, "ANTHROPIC_BASE_URL"))
                .unwrap_or_else(|| "https://api.anthropic.com".into());
            let alias = config.get("anthropic").and_then(|e| e.alias.clone());
            let provider_label = alias.clone().unwrap_or_else(|| "anthropic".into());
            let cache_retention = config
                .get("anthropic")
                .map(|e| e.cache_retention)
                .unwrap_or(moltis_config::CacheRetention::Short);
            let models = Self::desired_anthropic_models(config, fetched);

            added += self.replace_anthropic_catalog(
                models,
                &key,
                &base_url,
                &provider_label,
                alias,
                cache_retention,
            );
        }

        // ── OpenAI builtin ────────────────────────────────────────────
        if let Some(models) = fetched.get("openai")
            && config.is_enabled("openai")
            && let Some(key) = resolve_api_key(config, "openai", "OPENAI_API_KEY", env_overrides)
        {
            let base_url = config
                .get("openai")
                .and_then(|e| e.base_url.clone())
                .or_else(|| env_value(env_overrides, "OPENAI_BASE_URL"))
                .unwrap_or_else(|| "https://api.openai.com/v1".into());
            let alias = config.get("openai").and_then(|e| e.alias.clone());
            let provider_label = alias.unwrap_or_else(|| "openai".into());
            let stream_transport = config
                .get("openai")
                .map(|entry| entry.stream_transport)
                .unwrap_or(ProviderStreamTransport::Sse);

            for model in models {
                if self.has_provider_model(&provider_label, &model.id) {
                    continue;
                }
                let provider = Arc::new(
                    openai::OpenAiProvider::new_with_name(
                        key.clone(),
                        model.id.clone(),
                        base_url.clone(),
                        provider_label.clone(),
                    )
                    .with_stream_transport(stream_transport),
                );
                self.register(
                    ModelInfo {
                        id: model.id.clone(),
                        provider: provider_label.clone(),
                        display_name: model.display_name.clone(),
                        created_at: model.created_at,
                        recommended: model.recommended,
                        capabilities: model
                            .capabilities
                            .unwrap_or_else(|| ModelCapabilities::infer(&model.id)),
                    },
                    provider,
                );
                added += 1;
            }
        }

        // ── OpenAI-compatible providers ───────────────────────────────
        for def in OPENAI_COMPAT_PROVIDERS {
            let Some(models) = fetched.get(def.config_name) else {
                continue;
            };
            if !config.is_enabled(def.config_name) {
                continue;
            }

            let key = resolve_api_key(config, def.config_name, def.env_key, env_overrides);
            let key = if !def.requires_api_key {
                key.or_else(|| Some(secrecy::Secret::new(def.config_name.into())))
            } else if def.config_name == "gemini" {
                key.or_else(|| env_value(env_overrides, "GOOGLE_API_KEY").map(secrecy::Secret::new))
            } else {
                key
            };
            let Some(key) = key else {
                continue;
            };

            let base_url = config
                .get(def.config_name)
                .and_then(|e| e.base_url.clone())
                .or_else(|| env_value(env_overrides, def.env_base_url_key))
                .unwrap_or_else(|| def.default_base_url.into());
            let alias = config.get(def.config_name).and_then(|e| e.alias.clone());
            let provider_label = alias.unwrap_or_else(|| def.config_name.into());
            let stream_transport = config
                .get(def.config_name)
                .map(|entry| entry.stream_transport)
                .unwrap_or(ProviderStreamTransport::Sse);
            let cache_retention = config
                .get(def.config_name)
                .map(|e| e.cache_retention)
                .unwrap_or(moltis_config::CacheRetention::Short);
            let config_tool_mode = config
                .get(def.config_name)
                .map(|e| e.tool_mode)
                .unwrap_or_default();
            let is_ollama = def.config_name == "ollama";

            // Use pre-fetched Ollama `/api/show` probes (already collected
            // outside the registry lock by `fetch_discoverable_models`).
            let empty_probes = HashMap::new();
            let ollama_probes: &HashMap<String, OllamaShowResponse> = if is_ollama {
                &result.ollama_probes
            } else {
                &empty_probes
            };

            for model in models {
                if self.has_provider_model(&provider_label, &model.id) {
                    continue;
                }
                let effective_tool_mode = if is_ollama {
                    resolve_ollama_tool_mode(
                        config_tool_mode,
                        &model.id,
                        ollama_probes.get(&model.id),
                    )
                } else if !matches!(config_tool_mode, moltis_config::ToolMode::Auto) {
                    config_tool_mode
                } else {
                    moltis_config::ToolMode::Auto
                };

                let mut oai = openai::OpenAiProvider::new_with_name(
                    key.clone(),
                    model.id.clone(),
                    base_url.clone(),
                    provider_label.clone(),
                )
                .with_stream_transport(stream_transport)
                .with_cache_retention(cache_retention);

                if !matches!(effective_tool_mode, moltis_config::ToolMode::Auto) {
                    oai = oai.with_tool_mode(effective_tool_mode);
                }

                self.register(
                    ModelInfo {
                        id: model.id.clone(),
                        provider: provider_label.clone(),
                        display_name: model.display_name.clone(),
                        created_at: model.created_at,
                        recommended: model.recommended,
                        capabilities: model
                            .capabilities
                            .unwrap_or_else(|| ModelCapabilities::infer(&model.id)),
                    },
                    Arc::new(oai),
                );
                added += 1;
            }
        }

        // ── Custom providers ──────────────────────────────────────────
        for (name, entry) in &config.providers {
            if !name.starts_with("custom-") || !entry.enabled {
                continue;
            }
            let Some(models) = fetched.get(name.as_str()) else {
                continue;
            };
            let Some(api_key) = entry
                .api_key
                .as_ref()
                .filter(|k| !k.expose_secret().is_empty())
            else {
                continue;
            };
            let Some(base_url) = entry.base_url.as_ref().filter(|u| !u.trim().is_empty()) else {
                continue;
            };
            let custom_tool_mode = entry.tool_mode;

            for model in models {
                if self.has_provider_model(name, &model.id) {
                    continue;
                }
                let mut oai = openai::OpenAiProvider::new_with_name(
                    api_key.clone(),
                    model.id.clone(),
                    base_url.clone(),
                    name.clone(),
                )
                .with_stream_transport(entry.stream_transport);
                if !matches!(entry.wire_api, moltis_config::WireApi::ChatCompletions) {
                    oai = oai.with_wire_api(entry.wire_api);
                }
                if !matches!(custom_tool_mode, moltis_config::ToolMode::Auto) {
                    oai = oai.with_tool_mode(custom_tool_mode);
                }
                self.register(
                    ModelInfo {
                        id: model.id.clone(),
                        provider: name.clone(),
                        display_name: model.display_name.clone(),
                        created_at: model.created_at,
                        recommended: model.recommended,
                        capabilities: model
                            .capabilities
                            .unwrap_or_else(|| ModelCapabilities::infer(&model.id)),
                    },
                    Arc::new(oai),
                );
                added += 1;
            }
        }

        added
    }

    #[cfg(feature = "provider-genai")]
    fn register_genai_providers(
        &mut self,
        config: &ProvidersConfig,
        env_overrides: &HashMap<String, String>,
    ) {
        use crate::genai_provider;

        // (env_key, provider_config_name, model_id, display_name)
        let genai_models: &[(&str, &str, &str, &str)] = &[
            (
                "ANTHROPIC_API_KEY",
                "anthropic",
                "claude-sonnet-4-20250514",
                "Claude Sonnet 4 (genai)",
            ),
            ("OPENAI_API_KEY", "openai", "gpt-4o", "GPT-4o (genai)"),
            (
                "GROQ_API_KEY",
                "groq",
                "llama-3.1-8b-instant",
                "Llama 3.1 8B (genai/groq)",
            ),
            ("XAI_API_KEY", "xai", "grok-3-mini", "Grok 3 Mini (genai)"),
        ];

        for &(env_key, provider_name, default_model_id, display_name) in genai_models {
            if !config.is_enabled(provider_name) {
                continue;
            }

            // Use config api_key or fall back to env var.
            let Some(resolved_key) = resolve_api_key(config, provider_name, env_key, env_overrides)
            else {
                continue;
            };

            let model_id = configured_models_for_provider(config, provider_name)
                .into_iter()
                .next()
                .unwrap_or_else(|| default_model_id.to_string());

            // Get alias if configured (for metrics differentiation).
            let alias = config.get(provider_name).and_then(|e| e.alias.clone());
            let genai_provider_name = alias.unwrap_or_else(|| format!("genai/{provider_name}"));
            if self.has_model_any_provider(&model_id) {
                continue;
            }

            let provider = Arc::new(genai_provider::GenaiProvider::new(
                model_id.clone(),
                genai_provider_name.clone(),
                resolved_key,
            ));
            self.register(
                ModelInfo {
                    id: model_id.clone(),
                    provider: genai_provider_name,
                    display_name: display_name.into(),
                    created_at: None,
                    recommended: false,
                    capabilities: ModelCapabilities::infer(&model_id),
                },
                provider,
            );
        }
    }

    #[cfg(feature = "provider-async-openai")]
    fn register_async_openai_providers(
        &mut self,
        config: &ProvidersConfig,
        env_overrides: &HashMap<String, String>,
    ) {
        use crate::async_openai_provider;

        if !config.is_enabled("openai") {
            return;
        }

        let Some(key) = resolve_api_key(config, "openai", "OPENAI_API_KEY", env_overrides) else {
            return;
        };

        let base_url = config
            .get("openai")
            .and_then(|e| e.base_url.clone())
            .or_else(|| env_value(env_overrides, "OPENAI_BASE_URL"))
            .unwrap_or_else(|| "https://api.openai.com/v1".into());

        let model_id = configured_models_for_provider(config, "openai")
            .into_iter()
            .next()
            .unwrap_or_else(|| "gpt-4o".to_string());

        // Get alias if configured (for metrics differentiation).
        let alias = config.get("openai").and_then(|e| e.alias.clone());
        let provider_label = alias.clone().unwrap_or_else(|| "async-openai".into());
        if self.has_model_any_provider(&model_id) {
            return;
        }

        let provider = Arc::new(async_openai_provider::AsyncOpenAiProvider::with_alias(
            key,
            model_id.clone(),
            base_url,
            alias,
        ));
        self.register(
            ModelInfo {
                id: model_id.clone(),
                provider: provider_label,
                display_name: "GPT-4o (async-openai)".into(),
                created_at: None,
                recommended: false,
                capabilities: ModelCapabilities::infer(&model_id),
            },
            provider,
        );
    }

    #[cfg(feature = "provider-openai-codex")]
    fn register_openai_codex_providers(
        &mut self,
        config: &ProvidersConfig,
        prefetched: &HashMap<String, Vec<DiscoveredModel>>,
    ) {
        use crate::openai_codex;
        let source = OpenAiCodexDiscovery;
        let catalog = if source.should_fetch_models(config) {
            // Use pre-fetched live models from parallel discovery.
            let fallback = openai_codex::default_model_catalog();
            match prefetched.get("openai-codex") {
                Some(live) => {
                    let merged = merge_discovered_with_fallback_catalog(live.clone(), fallback);
                    tracing::info!(
                        model_count = merged.len(),
                        "loaded openai-codex models catalog"
                    );
                    merged
                },
                None => fallback,
            }
        } else {
            Vec::new()
        };
        self.register_dynamic_source_models(&source, config, catalog);
    }

    pub fn refresh_openai_codex_models(&mut self, config: &ProvidersConfig) -> bool {
        #[cfg(feature = "provider-openai-codex")]
        {
            let source = OpenAiCodexDiscovery;
            self.refresh_dynamic_source_models(&source, config)
        }

        #[cfg(not(feature = "provider-openai-codex"))]
        {
            let _ = config;
            false
        }
    }

    #[cfg(feature = "provider-github-copilot")]
    fn register_github_copilot_providers(
        &mut self,
        config: &ProvidersConfig,
        prefetched: &HashMap<String, Vec<DiscoveredModel>>,
    ) {
        let source = GitHubCopilotDiscovery;
        let catalog = if source.should_fetch_models(config) {
            // Use pre-fetched live models from parallel discovery.
            let fallback = crate::github_copilot::default_model_catalog();
            match prefetched.get("github-copilot") {
                Some(live) => {
                    let merged = merge_discovered_with_fallback_catalog(live.clone(), fallback);
                    tracing::debug!(
                        model_count = merged.len(),
                        "loaded github-copilot models catalog"
                    );
                    merged
                },
                None => fallback,
            }
        } else {
            Vec::new()
        };
        self.register_dynamic_source_models(&source, config, catalog);
    }

    pub fn refresh_github_copilot_models(&mut self, config: &ProvidersConfig) -> bool {
        #[cfg(feature = "provider-github-copilot")]
        {
            let source = GitHubCopilotDiscovery;
            self.refresh_dynamic_source_models(&source, config)
        }

        #[cfg(not(feature = "provider-github-copilot"))]
        {
            let _ = config;
            false
        }
    }

    pub fn refresh_dynamic_models(&mut self, config: &ProvidersConfig) -> Vec<(String, bool)> {
        #[cfg(any(feature = "provider-openai-codex", feature = "provider-github-copilot"))]
        {
            let mut results = Vec::new();
            for source in Self::dynamic_discovery_sources() {
                let refreshed = self.refresh_dynamic_source_models(source.as_ref(), config);
                results.push((source.provider_name().to_string(), refreshed));
            }
            results
        }

        #[cfg(not(any(feature = "provider-openai-codex", feature = "provider-github-copilot")))]
        {
            let _ = config;
            Vec::new()
        }
    }

    #[cfg(feature = "provider-kimi-code")]
    fn register_kimi_code_providers(
        &mut self,
        config: &ProvidersConfig,
        env_overrides: &HashMap<String, String>,
    ) {
        use crate::kimi_code;

        if !config.is_enabled("kimi-code") {
            return;
        }

        let api_key = resolve_api_key(config, "kimi-code", "KIMI_API_KEY", env_overrides);
        let has_oauth_tokens = kimi_code::has_stored_tokens();
        if api_key.is_none() && !has_oauth_tokens {
            return;
        }

        let base_url = config
            .get("kimi-code")
            .and_then(|e| e.base_url.clone())
            .or_else(|| env_value(env_overrides, "KIMI_BASE_URL"))
            .unwrap_or_else(|| "https://api.kimi.com/coding/v1".into());

        let build_provider = |model_id: &str| -> Arc<dyn LlmProvider> {
            if let Some(api_key) = api_key.as_ref() {
                Arc::new(kimi_code::KimiCodeProvider::new_with_api_key(
                    api_key.clone(),
                    model_id.into(),
                    base_url.clone(),
                ))
            } else {
                Arc::new(kimi_code::KimiCodeProvider::new(model_id.into()))
            }
        };

        let preferred = configured_models_for_provider(config, "kimi-code");
        let discovered = if should_fetch_models(config, "kimi-code") {
            catalog_to_discovered(kimi_code::KIMI_CODE_MODELS, 1)
        } else {
            Vec::new()
        };
        let models = merge_preferred_and_discovered_models(preferred, discovered);
        for model in models {
            let caps = model
                .capabilities
                .unwrap_or_else(|| ModelCapabilities::infer(&model.id));
            let (model_id, display_name, created_at, recommended) = (
                model.id,
                model.display_name,
                model.created_at,
                model.recommended,
            );
            if self.has_provider_model("kimi-code", &model_id) {
                continue;
            }
            let provider = build_provider(&model_id);
            self.register(
                ModelInfo {
                    id: model_id,
                    provider: "kimi-code".into(),
                    display_name,
                    created_at,
                    recommended,
                    capabilities: caps,
                },
                provider,
            );
        }
    }

    #[cfg(feature = "local-llm")]
    fn register_local_gguf_providers(&mut self, config: &ProvidersConfig) {
        use std::path::PathBuf;

        use crate::{local_gguf, local_llm};

        if !config.is_enabled("local") {
            return;
        }

        // Collect all model IDs to register:
        // 1. From local_models (multi-model config from local-llm.json)
        // 2. From provider models in config (preferred pins)
        let mut model_ids: Vec<String> = config.local_models.clone();
        model_ids.extend(configured_models_for_provider(config, "local"));
        model_ids = normalize_unique_models(model_ids);

        if model_ids.is_empty() {
            tracing::info!(
                "local-llm enabled but no models configured. Add [providers.local] models = [\"...\"] to config."
            );
            return;
        }

        // Only probe local hardware/backends when at least one local model is
        // configured. On macOS this avoids loading Metal/MLX runtime state
        // during startup when local inference is not in use.
        local_gguf::log_system_info_and_suggestions();

        // Build config from provider entry for user overrides
        let entry = config.get("local");
        let user_model_path = entry
            .and_then(|e| e.base_url.as_deref()) // Reuse base_url for model_path
            .map(PathBuf::from);

        // Register each model
        for model_id in model_ids {
            if self.has_provider_model("local-llm", &model_id) {
                continue;
            }

            // Look up model in registries to get display name
            let display_name = if let Some(def) = local_llm::models::find_model(&model_id) {
                def.display_name.to_string()
            } else if let Some(def) = local_gguf::models::find_model(&model_id) {
                def.display_name.to_string()
            } else {
                format!("{} (local)", model_id)
            };

            // Use LocalLlmProvider which auto-detects backend based on model type
            let llm_config = local_llm::LocalLlmConfig {
                model_id: model_id.clone(),
                model_path: user_model_path.clone(),
                backend: None, // Auto-detect based on model type
                context_size: None,
                gpu_layers: 0,
                temperature: 0.7,
                cache_dir: local_llm::models::default_models_dir(),
            };

            tracing::info!(
                model = %model_id,
                display_name = %display_name,
                "local-llm model configured (will load on first use)"
            );

            // Use LocalLlmProvider which properly routes to GGUF or MLX backend
            let provider = Arc::new(local_llm::LocalLlmProvider::new(llm_config));
            self.register(
                ModelInfo {
                    id: model_id.clone(),
                    provider: "local-llm".into(),
                    display_name,
                    created_at: None,
                    recommended: false,
                    capabilities: ModelCapabilities::infer(&model_id),
                },
                provider,
            );
        }
    }

    fn register_builtin_providers(
        &mut self,
        config: &ProvidersConfig,
        env_overrides: &HashMap<String, String>,
        prefetched: &HashMap<String, Vec<DiscoveredModel>>,
    ) {
        // Anthropic — register all known Claude models when API key is available.
        if config.is_enabled("anthropic")
            && let Some(key) =
                resolve_api_key(config, "anthropic", "ANTHROPIC_API_KEY", env_overrides)
        {
            let base_url = config
                .get("anthropic")
                .and_then(|e| e.base_url.clone())
                .or_else(|| env_value(env_overrides, "ANTHROPIC_BASE_URL"))
                .unwrap_or_else(|| "https://api.anthropic.com".into());

            // Get alias if configured (for metrics differentiation).
            let alias = config.get("anthropic").and_then(|e| e.alias.clone());
            let provider_label = alias.clone().unwrap_or_else(|| "anthropic".into());
            let cache_retention = config
                .get("anthropic")
                .map(|e| e.cache_retention)
                .unwrap_or(moltis_config::CacheRetention::Short);
            let models = Self::desired_anthropic_models(config, prefetched);
            self.register_anthropic_catalog(
                models,
                &key,
                &base_url,
                &provider_label,
                alias,
                cache_retention,
            );
        }

        // OpenAI — register all known OpenAI models when API key is available.
        if config.is_enabled("openai")
            && let Some(key) = resolve_api_key(config, "openai", "OPENAI_API_KEY", env_overrides)
        {
            let base_url = config
                .get("openai")
                .and_then(|e| e.base_url.clone())
                .or_else(|| env_value(env_overrides, "OPENAI_BASE_URL"))
                .unwrap_or_else(|| "https://api.openai.com/v1".into());

            // Get alias if configured (for metrics differentiation).
            let alias = config.get("openai").and_then(|e| e.alias.clone());
            let provider_label = alias.clone().unwrap_or_else(|| "openai".into());
            let stream_transport = config
                .get("openai")
                .map(|entry| entry.stream_transport)
                .unwrap_or(ProviderStreamTransport::Sse);
            let preferred = configured_models_for_provider(config, "openai");
            let discovered = if should_fetch_models(config, "openai") {
                // Use pre-fetched live models from parallel discovery.
                let fallback = openai::default_model_catalog();
                match prefetched.get("openai") {
                    Some(live) => {
                        let merged = merge_discovered_with_fallback_catalog(live.clone(), fallback);
                        tracing::debug!(model_count = merged.len(), "loaded openai models catalog");
                        merged
                    },
                    None => fallback,
                }
            } else {
                Vec::new()
            };
            let models = merge_preferred_and_discovered_models(preferred, discovered);

            for model in models {
                let caps = model
                    .capabilities
                    .unwrap_or_else(|| ModelCapabilities::infer(&model.id));
                let (model_id, display_name, created_at, recommended) = (
                    model.id,
                    model.display_name,
                    model.created_at,
                    model.recommended,
                );
                if self.has_provider_model(&provider_label, &model_id) {
                    continue;
                }
                let provider = Arc::new(
                    openai::OpenAiProvider::new_with_name(
                        key.clone(),
                        model_id.clone(),
                        base_url.clone(),
                        provider_label.clone(),
                    )
                    .with_stream_transport(stream_transport),
                );
                self.register(
                    ModelInfo {
                        id: model_id,
                        provider: provider_label.clone(),
                        display_name,
                        created_at,
                        recommended,
                        capabilities: caps,
                    },
                    provider,
                );
            }
        }
    }

    fn register_openai_compatible_providers(
        &mut self,
        config: &ProvidersConfig,
        env_overrides: &HashMap<String, String>,
        prefetched: &HashMap<String, Vec<DiscoveredModel>>,
    ) {
        for def in OPENAI_COMPAT_PROVIDERS {
            if !config.is_enabled(def.config_name) {
                continue;
            }

            let key = resolve_api_key(config, def.config_name, def.env_key, env_overrides);

            // Local providers don't require an API key — use a dummy value.
            // Gemini accepts both GEMINI_API_KEY and GOOGLE_API_KEY.
            let key = if !def.requires_api_key {
                key.or_else(|| Some(secrecy::Secret::new(def.config_name.into())))
            } else if def.config_name == "gemini" {
                key.or_else(|| env_value(env_overrides, "GOOGLE_API_KEY").map(secrecy::Secret::new))
            } else {
                key
            };

            let Some(key) = key else {
                continue;
            };

            let base_url = config
                .get(def.config_name)
                .and_then(|e| e.base_url.clone())
                .or_else(|| env_value(env_overrides, def.env_base_url_key))
                .unwrap_or_else(|| def.default_base_url.into());

            // Get alias if configured (for metrics differentiation).
            let alias = config.get(def.config_name).and_then(|e| e.alias.clone());
            let provider_label = alias.unwrap_or_else(|| def.config_name.into());
            let cache_retention = config
                .get(def.config_name)
                .map(|e| e.cache_retention)
                .unwrap_or(moltis_config::CacheRetention::Short);
            let stream_transport = config
                .get(def.config_name)
                .map(|entry| entry.stream_transport)
                .unwrap_or(ProviderStreamTransport::Sse);
            let preferred = configured_models_for_provider(config, def.config_name);
            if def.local_only {
                let has_explicit_entry = config.get(def.config_name).is_some();
                let has_env_base_url = env_value(env_overrides, def.env_base_url_key).is_some();
                if !has_explicit_entry && !has_env_base_url && preferred.is_empty() {
                    continue;
                }
            }
            // Some providers need an explicit model before they can answer;
            // keep discovery off there when no model is configured.
            // OpenRouter supports `/models`, so we discover dynamically.
            let skip_discovery = def.models.is_empty()
                && preferred.is_empty()
                && !def.local_only
                && (def.config_name == "venice" || cfg!(test));
            // Respect `supports_model_discovery`: providers whose API lacks a
            // /models endpoint (e.g. MiniMax) skip live fetch unless the user
            // explicitly opted in via `fetch_models = true` in config.
            let user_opted_in = config
                .get(def.config_name)
                .is_some_and(|entry| entry.fetch_models);
            let try_fetch = def.supports_model_discovery || user_opted_in;
            let static_catalog =
                || -> Vec<DiscoveredModel> { catalog_to_discovered(def.models, 2) };
            let discovered =
                if !skip_discovery && try_fetch && should_fetch_models(config, def.config_name) {
                    // Use pre-fetched results from parallel discovery.
                    match prefetched.get(def.config_name) {
                        Some(models) => models.clone(),
                        None => static_catalog(),
                    }
                } else if !def.supports_model_discovery && !def.models.is_empty() {
                    // Provider has no /models endpoint — use the static catalog.
                    static_catalog()
                } else {
                    Vec::new()
                };
            let models = merge_preferred_and_discovered_models(preferred, discovered);

            // Resolve per-provider tool_mode from config (defaults to Auto).
            let config_tool_mode = config
                .get(def.config_name)
                .map(|e| e.tool_mode)
                .unwrap_or_default();

            // For Ollama, probe each model's family info to decide native vs text
            // tool calling. For non-Ollama, just pass through the config tool mode.
            let is_ollama = def.config_name == "ollama";

            // Batch-probe Ollama models for family metadata (best-effort, 3s timeout).
            let ollama_probes: HashMap<String, OllamaShowResponse> = if is_ollama {
                probe_ollama_models_batch(&base_url, &models)
            } else {
                HashMap::new()
            };

            for model in models {
                let caps = model
                    .capabilities
                    .unwrap_or_else(|| ModelCapabilities::infer(&model.id));
                let (model_id, display_name, created_at, recommended) = (
                    model.id,
                    model.display_name,
                    model.created_at,
                    model.recommended,
                );
                if self.has_provider_model(&provider_label, &model_id) {
                    continue;
                }

                // Determine effective tool mode for this model.
                let effective_tool_mode = if is_ollama {
                    resolve_ollama_tool_mode(
                        config_tool_mode,
                        &model_id,
                        ollama_probes.get(&model_id),
                    )
                } else if !matches!(config_tool_mode, moltis_config::ToolMode::Auto) {
                    config_tool_mode
                } else {
                    // Non-Ollama providers: let OpenAiProvider use its default logic.
                    moltis_config::ToolMode::Auto
                };

                let mut oai = openai::OpenAiProvider::new_with_name(
                    key.clone(),
                    model_id.clone(),
                    base_url.clone(),
                    provider_label.clone(),
                )
                .with_stream_transport(stream_transport)
                .with_cache_retention(cache_retention);

                if !matches!(effective_tool_mode, moltis_config::ToolMode::Auto) {
                    oai = oai.with_tool_mode(effective_tool_mode);
                }

                let provider = Arc::new(oai);
                self.register(
                    ModelInfo {
                        id: model_id,
                        provider: provider_label.clone(),
                        display_name,
                        created_at,
                        recommended,
                        capabilities: caps,
                    },
                    provider,
                );
            }
        }
    }

    /// Register custom OpenAI-compatible providers (names starting with `custom-`).
    /// These are user-added endpoints that may support model discovery via `/v1/models`.
    fn register_custom_providers(
        &mut self,
        config: &ProvidersConfig,
        prefetched: &HashMap<String, Vec<DiscoveredModel>>,
    ) {
        for (name, entry) in &config.providers {
            if !name.starts_with("custom-") || !entry.enabled {
                continue;
            }

            let Some(api_key) = entry
                .api_key
                .as_ref()
                .filter(|k| !k.expose_secret().is_empty())
            else {
                continue;
            };

            let Some(base_url) = entry.base_url.as_ref().filter(|u| !u.trim().is_empty()) else {
                continue;
            };

            let preferred = configured_models_for_provider(config, name);

            // Use pre-fetched results from parallel discovery.
            let discovered = if should_fetch_models(config, name) {
                match prefetched.get(name.as_str()) {
                    Some(models) => models.clone(),
                    None => Vec::new(),
                }
            } else {
                Vec::new()
            };

            let models = merge_preferred_and_discovered_models(preferred, discovered);
            if models.is_empty() {
                tracing::debug!(
                    provider = %name,
                    "custom provider has no models — skipping registration"
                );
                continue;
            }

            let custom_tool_mode = entry.tool_mode;
            for model in models {
                let caps = model
                    .capabilities
                    .unwrap_or_else(|| ModelCapabilities::infer(&model.id));
                let (model_id, display_name, created_at, recommended) = (
                    model.id,
                    model.display_name,
                    model.created_at,
                    model.recommended,
                );
                if self.has_provider_model(name, &model_id) {
                    continue;
                }
                let mut oai = openai::OpenAiProvider::new_with_name(
                    api_key.clone(),
                    model_id.clone(),
                    base_url.clone(),
                    name.clone(),
                )
                .with_stream_transport(entry.stream_transport)
                .with_cache_retention(entry.cache_retention);
                if !matches!(entry.wire_api, moltis_config::WireApi::ChatCompletions) {
                    oai = oai.with_wire_api(entry.wire_api);
                }
                if !matches!(custom_tool_mode, moltis_config::ToolMode::Auto) {
                    oai = oai.with_tool_mode(custom_tool_mode);
                }
                let provider = Arc::new(oai);
                self.register(
                    ModelInfo {
                        id: model_id,
                        provider: name.clone(),
                        display_name,
                        created_at,
                        recommended,
                        capabilities: caps,
                    },
                    provider,
                );
            }

            tracing::info!(
                provider = %name,
                "registered custom OpenAI-compatible provider"
            );
        }
    }

    pub fn get(&self, model_id: &str) -> Option<Arc<dyn LlmProvider>> {
        let (base_id, reasoning) = split_reasoning_suffix(model_id);
        let provider = self
            .resolve_registry_model_id(base_id, None)
            .as_deref()
            .and_then(|id| self.providers.get(id))
            .cloned()?;
        if let Some(effort) = reasoning {
            let new_provider = Arc::clone(&provider).with_reasoning_effort(effort);
            if new_provider.is_none() {
                tracing::warn!(
                    model_id,
                    ?effort,
                    "provider does not support reasoning effort; ignoring suffix"
                );
            }
            Some(new_provider.unwrap_or(provider))
        } else {
            Some(provider)
        }
    }

    pub fn first(&self) -> Option<Arc<dyn LlmProvider>> {
        self.models
            .iter()
            .enumerate()
            .min_by_key(|(idx, m)| (subscription_preference_rank(&m.provider), *idx))
            .map(|(_, m)| m)
            .and_then(|m| self.providers.get(&m.id))
            .cloned()
    }

    /// Return the first provider that supports tool calling,
    /// falling back to the first provider overall.
    pub fn first_with_tools(&self) -> Option<Arc<dyn LlmProvider>> {
        self.models
            .iter()
            .enumerate()
            .filter_map(|(idx, m)| self.providers.get(&m.id).map(|p| (idx, m, p)))
            .filter(|(_, _, p)| p.supports_tools())
            .min_by_key(|(idx, m, _)| (subscription_preference_rank(&m.provider), *idx))
            .map(|(_, _, p)| Arc::clone(p))
            .or_else(|| self.first())
    }

    pub fn list_models(&self) -> &[ModelInfo] {
        &self.models
    }

    /// Return the base model list plus reasoning-effort variants for supported models.
    ///
    /// For each model that supports extended thinking, three additional entries
    /// are appended: `<id>@reasoning-low`, `<id>@reasoning-medium`, `<id>@reasoning-high`.
    /// These virtual IDs are resolved by `get()` back to the base provider with
    /// the corresponding reasoning effort applied.
    #[must_use]
    pub fn list_models_with_reasoning_variants(&self) -> Vec<ModelInfo> {
        let mut result = Vec::with_capacity(self.models.len() * 4);
        for m in &self.models {
            result.push(m.clone());
            if m.capabilities.reasoning {
                for &(suffix, _) in REASONING_SUFFIXES {
                    let label = suffix.strip_prefix("reasoning-").unwrap_or(suffix);
                    result.push(ModelInfo {
                        id: format!("{}{REASONING_SUFFIX_SEP}{suffix}", m.id),
                        provider: m.provider.clone(),
                        display_name: format!("{} ({label} reasoning)", m.display_name),
                        created_at: m.created_at,
                        recommended: false,
                        capabilities: ModelCapabilities {
                            reasoning: true,
                            ..m.capabilities
                        },
                    });
                }
            }
        }
        result
    }

    /// Return all registered providers in registration order.
    pub fn all_providers(&self) -> Vec<Arc<dyn LlmProvider>> {
        self.models
            .iter()
            .filter_map(|m| self.providers.get(&m.id).cloned())
            .collect()
    }

    /// Return providers for the given model IDs (in order), skipping unknown IDs.
    pub fn providers_for_models(&self, model_ids: &[String]) -> Vec<Arc<dyn LlmProvider>> {
        model_ids
            .iter()
            .filter_map(|id| {
                self.resolve_registry_model_id(id, None)
                    .as_deref()
                    .and_then(|rid| self.providers.get(rid))
                    .cloned()
            })
            .collect()
    }

    /// Return fallback providers ordered by affinity to the given primary:
    ///
    /// 1. Same model ID on a different provider backend (e.g. `gpt-4o` via openrouter)
    /// 2. Subscription providers (`openai-codex`, `github-copilot`)
    /// 3. Other models from the same provider (e.g. `claude-opus-4` when primary is `claude-sonnet-4`)
    /// 4. Models from other providers
    ///
    /// The primary itself is excluded from the result.
    pub fn fallback_providers_for(
        &self,
        primary_model_id: &str,
        primary_provider_name: &str,
    ) -> Vec<Arc<dyn LlmProvider>> {
        let primary_raw_model_id = raw_model_id(primary_model_id);
        let mut ranked: Vec<(u8, usize, usize, Arc<dyn LlmProvider>)> = Vec::new();

        for (idx, info) in self.models.iter().enumerate() {
            if info.id == primary_model_id && info.provider == primary_provider_name {
                continue; // skip the primary itself
            }
            let Some(p) = self.providers.get(&info.id).cloned() else {
                continue;
            };
            let provider_rank = subscription_preference_rank(&info.provider);
            let bucket = if raw_model_id(&info.id) == primary_raw_model_id {
                0
            } else if provider_rank == 0 {
                1
            } else if info.provider == primary_provider_name {
                2
            } else {
                3
            };
            ranked.push((bucket, provider_rank, idx, p));
        }

        ranked.sort_by_key(|(bucket, provider_rank, idx, _)| (*bucket, *provider_rank, *idx));
        ranked.into_iter().map(|(_, _, _, p)| p).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    pub fn provider_summary(&self) -> String {
        if self.providers.is_empty() {
            return "no LLM providers configured".into();
        }
        let provider_count = self
            .models
            .iter()
            .map(|m| m.provider.as_str())
            .collect::<HashSet<_>>()
            .len();
        let model_count = self.models.len();
        format!(
            "{} provider{}, {} model{}",
            provider_count,
            if provider_count == 1 {
                ""
            } else {
                "s"
            },
            model_count,
            if model_count == 1 {
                ""
            } else {
                "s"
            },
        )
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn secret(s: &str) -> secrecy::Secret<String> {
        secrecy::Secret::new(s.into())
    }

    #[test]
    fn provider_context_window_uses_lookup() {
        let provider = openai::OpenAiProvider::new(secret("k"), "gpt-4o".into(), "u".into());
        assert_eq!(provider.context_window(), 128_000);

        let anthropic = anthropic::AnthropicProvider::new(
            secret("k"),
            "claude-sonnet-4-20250514".into(),
            "u".into(),
        );
        assert_eq!(anthropic.context_window(), 200_000);
    }

    #[test]
    fn provider_supports_vision_uses_lookup() {
        let provider = openai::OpenAiProvider::new(secret("k"), "gpt-4o".into(), "u".into());
        assert!(provider.supports_vision());

        let anthropic = anthropic::AnthropicProvider::new(
            secret("k"),
            "claude-sonnet-4-20250514".into(),
            "u".into(),
        );
        assert!(anthropic.supports_vision());

        // Non-vision model
        let mistral = openai::OpenAiProvider::new_with_name(
            secret("k"),
            "codestral-latest".into(),
            "u".into(),
            "mistral".into(),
        );
        assert!(!mistral.supports_vision());
    }

    #[test]
    fn provider_supports_tools_uses_model_lookup() {
        let gpt = openai::OpenAiProvider::new(secret("k"), "gpt-5.2".into(), "u".into());
        assert!(gpt.supports_tools());

        let babbage = openai::OpenAiProvider::new(secret("k"), "babbage-002".into(), "u".into());
        assert!(!babbage.supports_tools());
    }

    #[test]
    fn default_context_window_trait() {
        let provider =
            openai::OpenAiProvider::new(secret("k"), "unknown-model-xyz".into(), "u".into());
        assert_eq!(provider.context_window(), 200_000);
    }

    #[test]
    fn registry_from_env_does_not_panic() {
        let reg = ProviderRegistry::from_env();
        let _ = reg.provider_summary();
    }

    #[test]
    fn registry_register_and_get() {
        let mut reg = ProviderRegistry::from_env_with_config(&ProvidersConfig::default());
        let initial_count = reg.list_models().len();

        let provider = Arc::new(openai::OpenAiProvider::new(
            secret("test-key"),
            "test-model".into(),
            "https://example.com".into(),
        ));
        reg.register(
            ModelInfo {
                id: "test-model".into(),
                provider: "test".into(),
                display_name: "Test Model".into(),
                created_at: None,
                recommended: false,
                capabilities: ModelCapabilities::default(),
            },
            provider,
        );

        assert_eq!(reg.list_models().len(), initial_count + 1);
        assert!(reg.get("test-model").is_some());
        assert!(reg.get("nonexistent").is_none());
    }

    #[cfg(feature = "provider-openai-codex")]
    #[test]
    fn refresh_openai_codex_models_is_noop_when_disabled() {
        let mut reg = ProviderRegistry {
            providers: HashMap::new(),
            models: Vec::new(),
        };
        let provider = Arc::new(openai::OpenAiProvider::new_with_name(
            secret("k"),
            "gpt-5.2-codex".into(),
            "https://example.com/v1".into(),
            "openai-codex".into(),
        ));
        reg.register(
            ModelInfo {
                id: "gpt-5.2-codex".into(),
                provider: "openai-codex".into(),
                display_name: "GPT-5.2 Codex (Codex/OAuth)".into(),
                created_at: None,
                recommended: false,
                capabilities: ModelCapabilities::infer("gpt-5.2-codex"),
            },
            provider,
        );

        let mut config = ProvidersConfig::default();
        config.providers.insert(
            "openai-codex".into(),
            moltis_config::schema::ProviderEntry {
                enabled: false,
                ..Default::default()
            },
        );

        let refreshed = reg.refresh_openai_codex_models(&config);
        assert!(!refreshed);
        assert!(
            reg.list_models()
                .iter()
                .any(|m| m.provider == "openai-codex")
        );
    }

    #[test]
    fn mistral_registers_with_api_key() {
        let mut config = ProvidersConfig::default();
        config
            .providers
            .insert("mistral".into(), moltis_config::schema::ProviderEntry {
                api_key: Some(secrecy::Secret::new("sk-test-mistral".into())),
                ..Default::default()
            });

        let reg = ProviderRegistry::from_env_with_config(&config);
        let mistral_models: Vec<_> = reg
            .list_models()
            .iter()
            .filter(|m| m.provider == "mistral")
            .collect();
        assert!(
            !mistral_models.is_empty(),
            "expected Mistral models to be registered"
        );
        for m in &mistral_models {
            assert!(reg.get(&m.id).is_some());
            assert_eq!(reg.get(&m.id).unwrap().name(), "mistral");
        }
    }

    #[test]
    fn cerebras_registers_with_api_key() {
        let mut config = ProvidersConfig::default();
        config
            .providers
            .insert("cerebras".into(), moltis_config::schema::ProviderEntry {
                api_key: Some(secrecy::Secret::new("sk-test-cerebras".into())),
                ..Default::default()
            });

        let reg = ProviderRegistry::from_env_with_config(&config);
        let cerebras_models: Vec<_> = reg
            .list_models()
            .iter()
            .filter(|m| m.provider == "cerebras")
            .collect();
        assert!(!cerebras_models.is_empty());
    }

    #[test]
    fn minimax_registers_with_api_key() {
        let mut config = ProvidersConfig::default();
        config
            .providers
            .insert("minimax".into(), moltis_config::schema::ProviderEntry {
                api_key: Some(secrecy::Secret::new("sk-test-minimax".into())),
                ..Default::default()
            });

        let reg = ProviderRegistry::from_env_with_config(&config);
        assert!(reg.list_models().iter().any(|m| m.provider == "minimax"));
    }

    #[test]
    fn minimax_registers_with_env_override_api_key() {
        let config = ProvidersConfig::default();
        let env_overrides = HashMap::from([(
            "MINIMAX_API_KEY".to_string(),
            "sk-test-minimax-override".to_string(),
        )]);

        let reg = ProviderRegistry::from_env_with_config_and_overrides(&config, &env_overrides);
        assert!(reg.list_models().iter().any(|m| m.provider == "minimax"));
    }

    #[test]
    fn openai_registers_with_generic_provider_env_override() {
        let config = ProvidersConfig::default();
        let env_overrides = HashMap::from([
            ("MOLTIS_PROVIDER".to_string(), "openai".to_string()),
            (
                "MOLTIS_API_KEY".to_string(),
                "sk-test-openai-generic".to_string(),
            ),
        ]);

        let reg = ProviderRegistry::from_env_with_config_and_overrides(&config, &env_overrides);
        assert!(reg.list_models().iter().any(|m| m.provider == "openai"));
    }

    #[test]
    fn zai_registers_with_api_key() {
        let mut config = ProvidersConfig::default();
        config
            .providers
            .insert("zai".into(), moltis_config::schema::ProviderEntry {
                api_key: Some(secrecy::Secret::new("sk-test-zai".into())),
                ..Default::default()
            });

        let reg = ProviderRegistry::from_env_with_config(&config);
        assert!(reg.list_models().iter().any(|m| m.provider == "zai"));
    }

    #[test]
    fn zai_code_registers_with_api_key() {
        let mut config = ProvidersConfig::default();
        config
            .providers
            .insert("zai-code".into(), moltis_config::schema::ProviderEntry {
                api_key: Some(secrecy::Secret::new("sk-test-zai-code".into())),
                ..Default::default()
            });

        let reg = ProviderRegistry::from_env_with_config(&config);
        assert!(reg.list_models().iter().any(|m| m.provider == "zai-code"));
    }

    #[test]
    fn moonshot_registers_with_api_key() {
        let mut config = ProvidersConfig::default();
        config
            .providers
            .insert("moonshot".into(), moltis_config::schema::ProviderEntry {
                api_key: Some(secrecy::Secret::new("sk-test-moonshot".into())),
                ..Default::default()
            });

        let reg = ProviderRegistry::from_env_with_config(&config);
        assert!(reg.list_models().iter().any(|m| m.provider == "moonshot"));
    }

    #[test]
    fn deepseek_registers_with_api_key() {
        let mut config = ProvidersConfig::default();
        config
            .providers
            .insert("deepseek".into(), moltis_config::schema::ProviderEntry {
                api_key: Some(secrecy::Secret::new("sk-test-deepseek".into())),
                ..Default::default()
            });

        let reg = ProviderRegistry::from_env_with_config(&config);
        let ds_models: Vec<_> = reg
            .list_models()
            .iter()
            .filter(|m| m.provider == "deepseek")
            .collect();
        assert!(!ds_models.is_empty());
        let provider = reg
            .get(&format!(
                "deepseek::{}",
                ds_models[0].id.split("::").last().unwrap_or_default()
            ))
            .expect("deepseek model should be in registry");
        assert!(
            provider.supports_tools(),
            "deepseek models must support tool calling"
        );
    }

    #[test]
    fn fireworks_registers_with_api_key() {
        let mut config = ProvidersConfig::default();
        config
            .providers
            .insert("fireworks".into(), moltis_config::schema::ProviderEntry {
                api_key: Some(secrecy::Secret::new("sk-test-fireworks".into())),
                ..Default::default()
            });

        let reg = ProviderRegistry::from_env_with_config(&config);
        let fw_models: Vec<_> = reg
            .list_models()
            .iter()
            .filter(|m| m.provider == "fireworks")
            .collect();
        assert!(
            !fw_models.is_empty(),
            "expected Fireworks models to be registered"
        );
        let provider = reg
            .get(&format!(
                "fireworks::{}",
                fw_models[0].id.split("::").last().unwrap_or_default()
            ))
            .expect("fireworks model should be in registry");
        assert!(
            provider.supports_tools(),
            "fireworks models must support tool calling"
        );
    }

    #[test]
    fn openrouter_requires_model_in_config() {
        let mut config = ProvidersConfig::default();
        config
            .providers
            .insert("openrouter".into(), moltis_config::schema::ProviderEntry {
                api_key: Some(secrecy::Secret::new("sk-test-or".into())),
                ..Default::default()
            });

        let reg = ProviderRegistry::from_env_with_config(&config);
        assert!(!reg.list_models().iter().any(|m| m.provider == "openrouter"));
    }

    #[test]
    fn openrouter_registers_with_model_in_config() {
        let mut config = ProvidersConfig::default();
        config
            .providers
            .insert("openrouter".into(), moltis_config::schema::ProviderEntry {
                api_key: Some(secrecy::Secret::new("sk-test-or".into())),
                models: vec!["anthropic/claude-3-haiku".into()],
                ..Default::default()
            });

        let reg = ProviderRegistry::from_env_with_config(&config);
        let or_models: Vec<_> = reg
            .list_models()
            .iter()
            .filter(|m| m.provider == "openrouter")
            .collect();
        assert!(
            or_models
                .iter()
                .any(|m| m.id == "openrouter::anthropic/claude-3-haiku")
        );
    }

    #[test]
    fn openrouter_strips_foreign_namespace_in_config_model_ids() {
        let mut config = ProvidersConfig::default();
        config
            .providers
            .insert("openrouter".into(), moltis_config::schema::ProviderEntry {
                api_key: Some(secrecy::Secret::new("sk-test-or".into())),
                models: vec!["openai::gpt-5.2".into()],
                ..Default::default()
            });

        let reg = ProviderRegistry::from_env_with_config(&config);
        assert!(
            reg.list_models()
                .iter()
                .any(|m| m.id == "openrouter::gpt-5.2")
        );
        assert!(
            !reg.list_models()
                .iter()
                .any(|m| m.id == "openrouter::openai::gpt-5.2")
        );
    }

    #[test]
    fn ollama_registers_without_api_key_env() {
        let mut config = ProvidersConfig::default();
        config
            .providers
            .insert("ollama".into(), moltis_config::schema::ProviderEntry {
                models: vec!["llama3".into()],
                ..Default::default()
            });

        let reg = ProviderRegistry::from_env_with_config(&config);
        assert!(reg.list_models().iter().any(|m| m.provider == "ollama"));
        assert!(reg.get("llama3").is_some());
    }

    #[test]
    fn venice_requires_model_in_config() {
        let mut config = ProvidersConfig::default();
        config
            .providers
            .insert("venice".into(), moltis_config::schema::ProviderEntry {
                api_key: Some(secrecy::Secret::new("sk-test-venice".into())),
                ..Default::default()
            });

        let reg = ProviderRegistry::from_env_with_config(&config);
        assert!(!reg.list_models().iter().any(|m| m.provider == "venice"));
    }

    #[test]
    fn disabled_provider_not_registered() {
        let mut config = ProvidersConfig::default();
        config
            .providers
            .insert("mistral".into(), moltis_config::schema::ProviderEntry {
                api_key: Some(secrecy::Secret::new("sk-test".into())),
                enabled: false,
                ..Default::default()
            });

        let reg = ProviderRegistry::from_env_with_config(&config);
        assert!(!reg.list_models().iter().any(|m| m.provider == "mistral"));
    }

    #[test]
    fn provider_name_returned_by_openai_provider() {
        let provider = openai::OpenAiProvider::new_with_name(
            secret("k"),
            "m".into(),
            "u".into(),
            "mistral".into(),
        );
        assert_eq!(provider.name(), "mistral");
    }

    #[test]
    fn custom_base_url_from_config() {
        let mut config = ProvidersConfig::default();
        config
            .providers
            .insert("mistral".into(), moltis_config::schema::ProviderEntry {
                api_key: Some(secrecy::Secret::new("sk-test".into())),
                base_url: Some("https://custom.mistral.example.com/v1".into()),
                ..Default::default()
            });

        let reg = ProviderRegistry::from_env_with_config(&config);
        assert!(reg.list_models().iter().any(|m| m.provider == "mistral"));
    }

    #[test]
    fn provider_models_can_disable_fetch_and_pin_single_model() {
        let mut config = ProvidersConfig::default();
        config
            .providers
            .insert("mistral".into(), moltis_config::schema::ProviderEntry {
                api_key: Some(secrecy::Secret::new("sk-test".into())),
                models: vec!["mistral-small-latest".into()],
                fetch_models: false,
                ..Default::default()
            });

        let reg = ProviderRegistry::from_env_with_config(&config);
        let mistral_models: Vec<_> = reg
            .list_models()
            .iter()
            .filter(|m| m.provider == "mistral")
            .collect();
        assert_eq!(mistral_models.len(), 1);
        assert_eq!(mistral_models[0].id, "mistral::mistral-small-latest");
    }

    #[test]
    fn provider_models_are_ordered_before_discovered_catalog() {
        let mut config = ProvidersConfig::default();
        config
            .providers
            .insert("mistral".into(), moltis_config::schema::ProviderEntry {
                api_key: Some(secrecy::Secret::new("sk-test".into())),
                models: vec!["codestral-latest".into()],
                ..Default::default()
            });

        let reg = ProviderRegistry::from_env_with_config(&config);
        let mistral_models: Vec<&str> = reg
            .list_models()
            .iter()
            .filter(|m| m.provider == "mistral")
            .map(|m| m.id.as_str())
            .collect();
        assert!(!mistral_models.is_empty());
        assert_eq!(mistral_models[0], "mistral::codestral-latest");
    }

    #[test]
    fn fallback_providers_ordering() {
        let mut reg = ProviderRegistry {
            providers: HashMap::new(),
            models: Vec::new(),
        };

        let mk = |id: &str, prov: &str| {
            (
                ModelInfo {
                    id: id.into(),
                    provider: prov.into(),
                    display_name: id.into(),
                    created_at: None,
                    recommended: false,
                    capabilities: ModelCapabilities::infer(id),
                },
                Arc::new(openai::OpenAiProvider::new_with_name(
                    secret("k"),
                    id.into(),
                    "u".into(),
                    prov.into(),
                )) as Arc<dyn LlmProvider>,
            )
        };

        let (info, prov) = mk("gpt-4o", "openai");
        reg.register(info, prov);
        let (info, prov) = mk("gpt-4o-mini", "openai");
        reg.register(info, prov);
        let (info, prov) = mk("claude-sonnet", "anthropic");
        reg.register(info, prov);
        let provider_or = Arc::new(openai::OpenAiProvider::new_with_name(
            secret("k"),
            "gpt-4o".into(),
            "u".into(),
            "openrouter".into(),
        ));
        let fallbacks = reg.fallback_providers_for("openai::gpt-4o", "openai");
        let ids: Vec<&str> = fallbacks.iter().map(|p| p.id()).collect();

        assert_eq!(ids, vec!["openai::gpt-4o-mini", "anthropic::claude-sonnet"]);

        let fallbacks = reg.fallback_providers_for("anthropic::claude-sonnet", "anthropic");
        let ids: Vec<&str> = fallbacks.iter().map(|p| p.id()).collect();
        assert_eq!(ids, vec!["openai::gpt-4o", "openai::gpt-4o-mini"]);

        drop(provider_or);
    }

    #[test]
    fn raw_model_lookup_prefers_subscription_provider() {
        let mut reg = ProviderRegistry::empty();

        let mk = |id: &str, prov: &str| {
            (
                ModelInfo {
                    id: id.into(),
                    provider: prov.into(),
                    display_name: id.into(),
                    created_at: None,
                    recommended: false,
                    capabilities: ModelCapabilities::infer(id),
                },
                Arc::new(openai::OpenAiProvider::new_with_name(
                    secret("k"),
                    id.into(),
                    "u".into(),
                    prov.into(),
                )) as Arc<dyn LlmProvider>,
            )
        };

        let (info, prov) = mk("gpt-5.2", "openai");
        reg.register(info, prov);
        let (info, prov) = mk("gpt-5.2", "openai-codex");
        reg.register(info, prov);

        let selected = reg.get("gpt-5.2").expect("model should resolve");
        assert_eq!(selected.name(), "openai-codex");
    }

    #[test]
    fn first_with_tools_prefers_subscription_provider() {
        let mut reg = ProviderRegistry::empty();

        let mk = |id: &str, prov: &str| {
            (
                ModelInfo {
                    id: id.into(),
                    provider: prov.into(),
                    display_name: id.into(),
                    created_at: None,
                    recommended: false,
                    capabilities: ModelCapabilities::infer(id),
                },
                Arc::new(openai::OpenAiProvider::new_with_name(
                    secret("k"),
                    id.into(),
                    "u".into(),
                    prov.into(),
                )) as Arc<dyn LlmProvider>,
            )
        };

        let (info, prov) = mk("gpt-5-mini", "openai");
        reg.register(info, prov);
        let (info, prov) = mk("gpt-5.2-codex", "openai-codex");
        reg.register(info, prov);

        let selected = reg.first_with_tools().expect("provider should be selected");
        assert_eq!(selected.name(), "openai-codex");
    }

    #[test]
    fn fallback_prefers_subscription_before_same_provider_non_subscription_models() {
        let mut reg = ProviderRegistry::empty();

        let mk = |id: &str, prov: &str| {
            (
                ModelInfo {
                    id: id.into(),
                    provider: prov.into(),
                    display_name: id.into(),
                    created_at: None,
                    recommended: false,
                    capabilities: ModelCapabilities::infer(id),
                },
                Arc::new(openai::OpenAiProvider::new_with_name(
                    secret("k"),
                    id.into(),
                    "u".into(),
                    prov.into(),
                )) as Arc<dyn LlmProvider>,
            )
        };

        let (info, prov) = mk("gpt-5.2", "openai");
        reg.register(info, prov);
        let (info, prov) = mk("gpt-5-mini", "openai");
        reg.register(info, prov);
        let (info, prov) = mk("gpt-5.3-codex", "openai-codex");
        reg.register(info, prov);
        let (info, prov) = mk("claude-sonnet", "anthropic");
        reg.register(info, prov);

        let fallbacks = reg.fallback_providers_for("openai::gpt-5.2", "openai");
        let ids: Vec<&str> = fallbacks.iter().map(|p| p.id()).collect();

        assert_eq!(ids, vec![
            "openai-codex::gpt-5.3-codex",
            "openai::gpt-5-mini",
            "anthropic::claude-sonnet",
        ]);
    }

    #[cfg(feature = "local-llm")]
    #[test]
    fn local_llm_requires_model_in_config() {
        let mut config = ProvidersConfig::default();
        config
            .providers
            .insert("local".into(), moltis_config::schema::ProviderEntry {
                ..Default::default()
            });

        let reg = ProviderRegistry::from_env_with_config(&config);
        assert!(!reg.list_models().iter().any(|m| m.provider == "local-llm"));
    }

    #[cfg(feature = "local-llm")]
    #[test]
    fn local_llm_registers_with_model_in_config() {
        let mut config = ProvidersConfig::default();
        config
            .providers
            .insert("local".into(), moltis_config::schema::ProviderEntry {
                models: vec!["qwen2.5-coder-7b-q4_k_m".into()],
                ..Default::default()
            });

        let reg = ProviderRegistry::from_env_with_config(&config);
        let local_models: Vec<_> = reg
            .list_models()
            .iter()
            .filter(|m| m.provider == "local-llm")
            .collect();
        assert_eq!(local_models.len(), 1);
        assert_eq!(local_models[0].id, "local-llm::qwen2.5-coder-7b-q4_k_m");
    }

    #[cfg(feature = "local-llm")]
    #[test]
    fn local_llm_disabled_not_registered() {
        let mut config = ProvidersConfig::default();
        config
            .providers
            .insert("local".into(), moltis_config::schema::ProviderEntry {
                enabled: false,
                models: vec!["qwen2.5-coder-7b-q4_k_m".into()],
                ..Default::default()
            });

        let reg = ProviderRegistry::from_env_with_config(&config);
        assert!(!reg.list_models().iter().any(|m| m.provider == "local-llm"));
    }

    #[cfg(feature = "local-llm")]
    #[test]
    fn local_llm_alias_key_registers_model() {
        let mut config = ProvidersConfig::default();
        config
            .providers
            .insert("local-llm".into(), moltis_config::schema::ProviderEntry {
                models: vec!["qwen2.5-coder-7b-q4_k_m".into()],
                ..Default::default()
            });

        let reg = ProviderRegistry::from_env_with_config(&config);
        assert!(
            reg.list_models().iter().any(|m| m.provider == "local-llm"),
            "local-llm alias config key should register local models"
        );
    }

    #[cfg(feature = "local-llm")]
    #[test]
    fn local_llm_alias_key_respects_disabled_flag() {
        let mut config = ProvidersConfig::default();
        config
            .providers
            .insert("local-llm".into(), moltis_config::schema::ProviderEntry {
                enabled: false,
                models: vec!["qwen2.5-coder-7b-q4_k_m".into()],
                ..Default::default()
            });

        let reg = ProviderRegistry::from_env_with_config(&config);
        assert!(
            !reg.list_models().iter().any(|m| m.provider == "local-llm"),
            "disabled local-llm alias config should suppress local model registration"
        );
    }

    #[test]
    fn openai_provider_supports_tools_respects_override() {
        use moltis_config::ToolMode;
        let make = |mode: ToolMode| {
            openai::OpenAiProvider::new(secret("key"), "gpt-4o".into(), "http://x".into())
                .with_tool_mode(mode)
        };
        assert!(make(ToolMode::Native).supports_tools());
        assert!(!make(ToolMode::Text).supports_tools());
        assert!(!make(ToolMode::Off).supports_tools());
        assert!(make(ToolMode::Auto).supports_tools());
    }

    #[test]
    fn openai_provider_tool_mode_returns_override() {
        use moltis_config::ToolMode;
        let p = openai::OpenAiProvider::new(secret("key"), "gpt-4o".into(), "http://x".into())
            .with_tool_mode(ToolMode::Text);
        assert_eq!(p.tool_mode(), Some(ToolMode::Text));
    }

    #[test]
    fn openai_provider_tool_mode_default_is_none() {
        let p = openai::OpenAiProvider::new(secret("key"), "gpt-4o".into(), "http://x".into());
        assert_eq!(p.tool_mode(), None);
    }

    #[test]
    fn registry_get_resolves_reasoning_suffix() {
        let mut reg = ProviderRegistry::empty();
        reg.register(
            ModelInfo {
                id: "claude-opus-4-5-20251101".into(),
                provider: "anthropic".into(),
                display_name: "Claude Opus 4.5".into(),
                created_at: None,
                recommended: false,
                capabilities: ModelCapabilities::infer("claude-opus-4-5-20251101"),
            },
            Arc::new(anthropic::AnthropicProvider::new(
                secret("key"),
                "claude-opus-4-5-20251101".into(),
                "https://api.anthropic.com".into(),
            )),
        );

        let p = reg.get("anthropic::claude-opus-4-5-20251101");
        assert!(p.is_some());
        assert!(p.unwrap().reasoning_effort().is_none());

        let p = reg.get("anthropic::claude-opus-4-5-20251101@reasoning-high");
        assert!(p.is_some());
        assert_eq!(
            p.unwrap().reasoning_effort(),
            Some(moltis_agents::model::ReasoningEffort::High)
        );
    }

    #[test]
    fn list_models_with_reasoning_variants_generates_entries() {
        let mut reg = ProviderRegistry::empty();
        reg.register(
            ModelInfo {
                id: "claude-opus-4-5-20251101".into(),
                provider: "anthropic".into(),
                display_name: "Claude Opus 4.5".into(),
                created_at: None,
                recommended: false,
                capabilities: ModelCapabilities::infer("claude-opus-4-5-20251101"),
            },
            Arc::new(anthropic::AnthropicProvider::new(
                secret("key"),
                "claude-opus-4-5-20251101".into(),
                "https://api.anthropic.com".into(),
            )),
        );
        reg.register(
            ModelInfo {
                id: "gpt-4o".into(),
                provider: "openai".into(),
                display_name: "GPT-4o".into(),
                created_at: None,
                recommended: false,
                capabilities: ModelCapabilities::infer("gpt-4o"),
            },
            Arc::new(openai::OpenAiProvider::new(
                secret("key"),
                "gpt-4o".into(),
                "https://api.openai.com/v1".into(),
            )),
        );

        let base_count = reg.list_models().len();
        assert_eq!(base_count, 2);

        let with_variants = reg.list_models_with_reasoning_variants();
        assert_eq!(with_variants.len(), 5);

        let variant_ids: Vec<&str> = with_variants.iter().map(|m| m.id.as_str()).collect();
        assert!(variant_ids.contains(&"anthropic::claude-opus-4-5-20251101@reasoning-low"));
        assert!(variant_ids.contains(&"anthropic::claude-opus-4-5-20251101@reasoning-medium"));
        assert!(variant_ids.contains(&"anthropic::claude-opus-4-5-20251101@reasoning-high"));
        assert!(!variant_ids.iter().any(|id| id.contains("gpt-4o@")));

        assert_eq!(variant_ids[0], "anthropic::claude-opus-4-5-20251101");
        assert_eq!(
            variant_ids[1],
            "anthropic::claude-opus-4-5-20251101@reasoning-low"
        );
        assert_eq!(
            variant_ids[2],
            "anthropic::claude-opus-4-5-20251101@reasoning-medium"
        );
        assert_eq!(
            variant_ids[3],
            "anthropic::claude-opus-4-5-20251101@reasoning-high"
        );
        assert_eq!(variant_ids[4], "openai::gpt-4o");
    }

    #[test]
    fn custom_provider_with_explicit_models_skips_discovery() {
        let mut config = ProvidersConfig::default();
        config.providers.insert(
            "custom-mylocal".into(),
            moltis_config::schema::ProviderEntry {
                enabled: true,
                api_key: Some(secret("sk-test")),
                base_url: Some("http://localhost:8080/v1".into()),
                models: vec!["my-model".into()],
                fetch_models: true,
                ..Default::default()
            },
        );
        let pending = ProviderRegistry::fire_discoveries(&config, &HashMap::new());
        let names: Vec<&str> = pending.iter().map(|(n, _)| n.as_str()).collect();
        assert!(
            !names.contains(&"custom-mylocal"),
            "should not fire discovery for custom provider with explicit models, got: {names:?}"
        );
    }

    #[test]
    fn custom_provider_without_explicit_models_fires_discovery() {
        let mut config = ProvidersConfig::default();
        config.providers.insert(
            "custom-mylocal".into(),
            moltis_config::schema::ProviderEntry {
                enabled: true,
                api_key: Some(secret("sk-test")),
                base_url: Some("http://localhost:8080/v1".into()),
                models: vec![],
                fetch_models: true,
                ..Default::default()
            },
        );
        let pending = ProviderRegistry::fire_discoveries(&config, &HashMap::new());
        let names: Vec<&str> = pending.iter().map(|(n, _)| n.as_str()).collect();
        assert!(
            names.contains(&"custom-mylocal"),
            "should fire discovery for custom provider without explicit models, got: {names:?}"
        );
    }

    #[test]
    fn custom_provider_with_explicit_models_registers_from_empty_prefetch() {
        let mut config = ProvidersConfig::default();
        config.providers.insert(
            "custom-mylocal".into(),
            moltis_config::schema::ProviderEntry {
                enabled: true,
                api_key: Some(secret("sk-test")),
                base_url: Some("http://localhost:8080/v1".into()),
                models: vec!["my-model".into()],
                ..Default::default()
            },
        );
        let registry = ProviderRegistry::from_config_with_prefetched(
            &config,
            &HashMap::new(),
            &HashMap::new(),
        );
        let models = registry.list_models();
        assert!(
            models
                .iter()
                .any(|m| m.id == "custom-mylocal::my-model" && m.provider == "custom-mylocal"),
            "explicit model should be registered even with empty prefetch, got: {models:?}"
        );
    }

    #[test]
    fn alibaba_coding_registers_with_api_key() {
        let mut config = ProvidersConfig::default();
        config.providers.insert(
            "alibaba-coding".into(),
            moltis_config::schema::ProviderEntry {
                api_key: Some(secrecy::Secret::new("sk-sp-test".into())),
                ..Default::default()
            },
        );

        let reg = ProviderRegistry::from_env_with_config(&config);
        assert!(
            reg.list_models()
                .iter()
                .any(|m| m.provider == "alibaba-coding")
        );
    }

    #[test]
    fn anthropic_prefetched_models_replace_static_fallback_at_startup() {
        let mut config = ProvidersConfig::default();
        config
            .providers
            .insert("anthropic".into(), moltis_config::schema::ProviderEntry {
                enabled: true,
                api_key: Some(secret("sk-ant-test")),
                fetch_models: true,
                ..Default::default()
            });

        let mut prefetched = HashMap::new();
        prefetched.insert("anthropic".into(), vec![DiscoveredModel::new(
            "claude-future-1",
            "Claude Future 1",
        )]);

        let registry =
            ProviderRegistry::from_config_with_prefetched(&config, &HashMap::new(), &prefetched);
        let ids: Vec<&str> = registry
            .list_models()
            .iter()
            .map(|m| m.id.as_str())
            .collect();

        assert!(ids.contains(&"anthropic::claude-future-1"));
        assert!(
            !ids.contains(&"anthropic::claude-sonnet-4-6"),
            "live Anthropic discovery should be authoritative, got: {ids:?}"
        );
    }

    #[test]
    fn anthropic_rediscovery_replaces_stale_static_models() {
        let mut config = ProvidersConfig::default();
        config
            .providers
            .insert("anthropic".into(), moltis_config::schema::ProviderEntry {
                enabled: true,
                api_key: Some(secret("sk-ant-test")),
                fetch_models: true,
                ..Default::default()
            });

        let env_overrides = HashMap::new();
        let mut registry =
            ProviderRegistry::from_config_with_static_catalogs(&config, &env_overrides);
        assert!(
            registry
                .list_models()
                .iter()
                .any(|m| m.id == "anthropic::claude-sonnet-4-6"),
            "expected startup fallback Anthropic model to be present"
        );

        let mut models = HashMap::new();
        models.insert("anthropic".into(), vec![DiscoveredModel::new(
            "claude-future-1",
            "Claude Future 1",
        )]);
        let result = RediscoveryResult {
            models,
            ollama_probes: HashMap::new(),
        };

        let added = registry.register_rediscovered_models(&config, &env_overrides, &result);
        assert_eq!(added, 1);

        let ids: Vec<&str> = registry
            .list_models()
            .iter()
            .map(|m| m.id.as_str())
            .collect();
        assert!(ids.contains(&"anthropic::claude-future-1"));
        assert!(
            !ids.contains(&"anthropic::claude-sonnet-4-6"),
            "runtime rediscovery should replace stale Anthropic catalog entries, got: {ids:?}"
        );
    }

    #[test]
    fn register_rediscovered_models_adds_new_models() {
        let mut config = ProvidersConfig::default();
        config.providers.insert(
            "custom-test".to_string(),
            moltis_config::schema::ProviderEntry {
                enabled: true,
                api_key: Some(secrecy::Secret::new("test-key".into())),
                base_url: Some("http://localhost:1234/v1".into()),
                fetch_models: true,
                ..Default::default()
            },
        );

        let env_overrides = HashMap::new();

        let mut reg = ProviderRegistry::from_config_with_static_catalogs(&config, &env_overrides);
        let before = reg.list_models().len();

        let mut models = HashMap::new();
        models.insert("custom-test".to_string(), vec![
            DiscoveredModel::new("new-model-a", "New Model A"),
            DiscoveredModel::new("new-model-b", "New Model B"),
        ]);
        let result = RediscoveryResult {
            models,
            ollama_probes: HashMap::new(),
        };

        let added = reg.register_rediscovered_models(&config, &env_overrides, &result);
        assert_eq!(added, 2, "should register 2 new models");
        assert_eq!(
            reg.list_models().len(),
            before + 2,
            "model list should grow by 2"
        );

        let added_again = reg.register_rediscovered_models(&config, &env_overrides, &result);
        assert_eq!(added_again, 0, "should not re-register existing models");
    }

    #[test]
    fn register_rediscovered_models_skips_existing() {
        let mut config = ProvidersConfig::default();
        config.providers.insert(
            "custom-test".to_string(),
            moltis_config::schema::ProviderEntry {
                enabled: true,
                api_key: Some(secrecy::Secret::new("test-key".into())),
                base_url: Some("http://localhost:1234/v1".into()),
                fetch_models: true,
                models: vec!["existing-model".to_string()],
                ..Default::default()
            },
        );

        let env_overrides = HashMap::new();
        let mut reg = ProviderRegistry::from_config_with_static_catalogs(&config, &env_overrides);
        let before = reg.list_models().len();

        let mut models = HashMap::new();
        models.insert("custom-test".to_string(), vec![
            DiscoveredModel::new("existing-model", "Existing Model"),
            DiscoveredModel::new("brand-new-model", "Brand New Model"),
        ]);
        let result = RediscoveryResult {
            models,
            ollama_probes: HashMap::new(),
        };

        let added = reg.register_rediscovered_models(&config, &env_overrides, &result);
        assert_eq!(added, 1, "should only add the brand-new model");
        assert_eq!(reg.list_models().len(), before + 1);
    }
}
