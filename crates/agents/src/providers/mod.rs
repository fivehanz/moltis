pub mod anthropic;
pub mod openai;

#[cfg(feature = "provider-genai")]
pub mod genai_provider;

#[cfg(feature = "provider-async-openai")]
pub mod async_openai_provider;

#[cfg(feature = "provider-openai-codex")]
pub mod openai_codex;

use std::{collections::HashMap, sync::Arc};

use moltis_config::schema::ProvidersConfig;

use crate::model::LlmProvider;

/// Info about an available model.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ModelInfo {
    pub id: String,
    pub provider: String,
    pub display_name: String,
}

/// Known Anthropic Claude models (model_id, display_name).
/// Current models listed first, then legacy models.
const ANTHROPIC_MODELS: &[(&str, &str)] = &[
    ("claude-opus-4-5-20251101", "Claude Opus 4.5"),
    ("claude-sonnet-4-5-20250929", "Claude Sonnet 4.5"),
    ("claude-haiku-4-5-20251001", "Claude Haiku 4.5"),
    ("claude-opus-4-1-20250805", "Claude Opus 4.1"),
    ("claude-sonnet-4-20250514", "Claude Sonnet 4"),
    ("claude-opus-4-20250514", "Claude Opus 4"),
    ("claude-3-7-sonnet-20250219", "Claude 3.7 Sonnet"),
    ("claude-3-haiku-20240307", "Claude 3 Haiku"),
];

/// Known OpenAI models (model_id, display_name).
const OPENAI_MODELS: &[(&str, &str)] = &[
    ("gpt-4o", "GPT-4o"),
    ("gpt-4o-mini", "GPT-4o Mini"),
    ("gpt-4-turbo", "GPT-4 Turbo"),
    ("o3", "o3"),
    ("o3-mini", "o3-mini"),
    ("o4-mini", "o4-mini"),
];

/// Registry of available LLM providers, keyed by model ID.
pub struct ProviderRegistry {
    providers: HashMap<String, Arc<dyn LlmProvider>>,
    models: Vec<ModelInfo>,
}

impl ProviderRegistry {
    /// Register a provider manually.
    pub fn register(&mut self, info: ModelInfo, provider: Arc<dyn LlmProvider>) {
        self.providers.insert(info.id.clone(), provider);
        self.models.push(info);
    }

    /// Auto-discover providers from environment variables.
    /// Uses default config (all providers enabled).
    pub fn from_env() -> Self {
        Self::from_env_with_config(&ProvidersConfig::default())
    }

    /// Auto-discover providers from environment variables,
    /// respecting the given config for enable/disable and overrides.
    ///
    /// Provider priority (first registered wins for a given model ID):
    /// 1. Built-in raw reqwest providers (always available, support tool calling)
    /// 2. async-openai-backed providers (if `provider-async-openai` feature enabled)
    /// 3. genai-backed providers (if `provider-genai` feature enabled, no tool support)
    /// 4. OpenAI Codex OAuth providers (if `provider-openai-codex` feature enabled)
    pub fn from_env_with_config(config: &ProvidersConfig) -> Self {
        let mut reg = Self {
            providers: HashMap::new(),
            models: Vec::new(),
        };

        // Built-in providers first: they support tool calling.
        reg.register_builtin_providers(config);

        #[cfg(feature = "provider-async-openai")]
        {
            reg.register_async_openai_providers(config);
        }

        // GenAI providers last: they don't support tool calling,
        // so they only fill in models not already covered above.
        #[cfg(feature = "provider-genai")]
        {
            reg.register_genai_providers(config);
        }

        #[cfg(feature = "provider-openai-codex")]
        {
            reg.register_openai_codex_providers(config);
        }

        reg
    }

    #[cfg(feature = "provider-genai")]
    fn register_genai_providers(&mut self, config: &ProvidersConfig) {
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
                "GEMINI_API_KEY",
                "gemini",
                "gemini-2.0-flash",
                "Gemini 2.0 Flash (genai)",
            ),
            (
                "GROQ_API_KEY",
                "groq",
                "llama-3.1-8b-instant",
                "Llama 3.1 8B (genai/groq)",
            ),
            ("XAI_API_KEY", "xai", "grok-3-mini", "Grok 3 Mini (genai)"),
            (
                "DEEPSEEK_API_KEY",
                "deepseek",
                "deepseek-chat",
                "DeepSeek Chat (genai)",
            ),
        ];

        for &(env_key, provider_name, default_model_id, display_name) in genai_models {
            if !config.is_enabled(provider_name) {
                continue;
            }

            // Use config api_key or fall back to env var.
            let has_key = config
                .get(provider_name)
                .and_then(|e| e.api_key.as_ref())
                .map(|k| !k.is_empty())
                .unwrap_or(false)
                || std::env::var(env_key).is_ok();

            if !has_key {
                continue;
            }

            // If config provides an api_key, set the env var so genai picks it up.
            if let Some(key) = config.get(provider_name).and_then(|e| e.api_key.as_ref())
                && !key.is_empty()
            {
                // Safety: only called during single-threaded startup.
                unsafe { std::env::set_var(env_key, key) };
            }

            let model_id = config
                .get(provider_name)
                .and_then(|e| e.model.as_deref())
                .unwrap_or(default_model_id);

            if self.providers.contains_key(model_id) {
                continue;
            }

            let genai_provider_name = format!("genai/{provider_name}");
            let provider = Arc::new(genai_provider::GenaiProvider::new(
                model_id.into(),
                genai_provider_name.clone(),
            ));
            self.register(
                ModelInfo {
                    id: model_id.into(),
                    provider: genai_provider_name,
                    display_name: display_name.into(),
                },
                provider,
            );
        }
    }

    #[cfg(feature = "provider-async-openai")]
    fn register_async_openai_providers(&mut self, config: &ProvidersConfig) {
        if !config.is_enabled("openai") {
            return;
        }

        let key = config
            .get("openai")
            .and_then(|e| e.api_key.clone())
            .or_else(|| std::env::var("OPENAI_API_KEY").ok());

        let Some(key) = key.filter(|k| !k.is_empty()) else {
            return;
        };

        let base_url = config
            .get("openai")
            .and_then(|e| e.base_url.clone())
            .or_else(|| std::env::var("OPENAI_BASE_URL").ok())
            .unwrap_or_else(|| "https://api.openai.com/v1".into());

        let model_id = config
            .get("openai")
            .and_then(|e| e.model.as_deref())
            .unwrap_or("gpt-4o");

        if self.providers.contains_key(model_id) {
            return;
        }

        let provider = Arc::new(async_openai_provider::AsyncOpenAiProvider::new(
            key,
            model_id.into(),
            base_url,
        ));
        self.register(
            ModelInfo {
                id: model_id.into(),
                provider: "async-openai".into(),
                display_name: "GPT-4o (async-openai)".into(),
            },
            provider,
        );
    }

    #[cfg(feature = "provider-openai-codex")]
    fn register_openai_codex_providers(&mut self, config: &ProvidersConfig) {
        if !config.is_enabled("openai-codex") {
            return;
        }

        if !openai_codex::has_stored_tokens() {
            return;
        }

        // All available Codex models, matching the Codex CLI model picker.
        let codex_models: &[(&str, &str)] = &[
            ("gpt-5.2-codex", "GPT-5.2 Codex"),
            ("gpt-5.2", "GPT-5.2"),
            ("gpt-5.1-codex-max", "GPT-5.1 Codex Max"),
            ("gpt-5.1-codex-mini", "GPT-5.1 Codex Mini"),
        ];

        // If user configured a specific model, register only that one.
        if let Some(model_id) = config.get("openai-codex").and_then(|e| e.model.as_deref()) {
            if !self.providers.contains_key(model_id) {
                let display_name = codex_models
                    .iter()
                    .find(|(id, _)| *id == model_id)
                    .map(|(_, name)| format!("{name} (Codex/OAuth)"))
                    .unwrap_or_else(|| format!("{model_id} (Codex/OAuth)"));
                let provider = Arc::new(openai_codex::OpenAiCodexProvider::new(model_id.into()));
                self.register(
                    ModelInfo {
                        id: model_id.into(),
                        provider: "openai-codex".into(),
                        display_name,
                    },
                    provider,
                );
            }
            return;
        }

        // No specific model configured — register all available Codex models.
        for &(model_id, display_name) in codex_models {
            if self.providers.contains_key(model_id) {
                continue;
            }
            let provider = Arc::new(openai_codex::OpenAiCodexProvider::new(model_id.into()));
            self.register(
                ModelInfo {
                    id: model_id.into(),
                    provider: "openai-codex".into(),
                    display_name: format!("{display_name} (Codex/OAuth)"),
                },
                provider,
            );
        }
    }

    fn register_builtin_providers(&mut self, config: &ProvidersConfig) {
        // Anthropic — register all known Claude models when API key is available.
        if config.is_enabled("anthropic") {
            let key = config
                .get("anthropic")
                .and_then(|e| e.api_key.clone())
                .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok());

            if let Some(key) = key.filter(|k| !k.is_empty()) {
                let base_url = config
                    .get("anthropic")
                    .and_then(|e| e.base_url.clone())
                    .or_else(|| std::env::var("ANTHROPIC_BASE_URL").ok())
                    .unwrap_or_else(|| "https://api.anthropic.com".into());

                // If user configured a specific model, register only that one.
                if let Some(model_id) = config.get("anthropic").and_then(|e| e.model.as_deref()) {
                    if !self.providers.contains_key(model_id) {
                        let display = ANTHROPIC_MODELS
                            .iter()
                            .find(|(id, _)| *id == model_id)
                            .map(|(_, name)| name.to_string())
                            .unwrap_or_else(|| model_id.to_string());
                        let provider = Arc::new(anthropic::AnthropicProvider::new(
                            key.clone(),
                            model_id.into(),
                            base_url.clone(),
                        ));
                        self.register(
                            ModelInfo {
                                id: model_id.into(),
                                provider: "anthropic".into(),
                                display_name: display,
                            },
                            provider,
                        );
                    }
                } else {
                    // No specific model — register all known Anthropic models.
                    for &(model_id, display_name) in ANTHROPIC_MODELS {
                        if self.providers.contains_key(model_id) {
                            continue;
                        }
                        let provider = Arc::new(anthropic::AnthropicProvider::new(
                            key.clone(),
                            model_id.into(),
                            base_url.clone(),
                        ));
                        self.register(
                            ModelInfo {
                                id: model_id.into(),
                                provider: "anthropic".into(),
                                display_name: display_name.into(),
                            },
                            provider,
                        );
                    }
                }
            }
        }

        // OpenAI — register all known OpenAI models when API key is available.
        if config.is_enabled("openai") {
            let key = config
                .get("openai")
                .and_then(|e| e.api_key.clone())
                .or_else(|| std::env::var("OPENAI_API_KEY").ok());

            if let Some(key) = key.filter(|k| !k.is_empty()) {
                let base_url = config
                    .get("openai")
                    .and_then(|e| e.base_url.clone())
                    .or_else(|| std::env::var("OPENAI_BASE_URL").ok())
                    .unwrap_or_else(|| "https://api.openai.com/v1".into());

                if let Some(model_id) = config.get("openai").and_then(|e| e.model.as_deref()) {
                    if !self.providers.contains_key(model_id) {
                        let display = OPENAI_MODELS
                            .iter()
                            .find(|(id, _)| *id == model_id)
                            .map(|(_, name)| name.to_string())
                            .unwrap_or_else(|| model_id.to_string());
                        let provider = Arc::new(openai::OpenAiProvider::new(
                            key.clone(),
                            model_id.into(),
                            base_url.clone(),
                        ));
                        self.register(
                            ModelInfo {
                                id: model_id.into(),
                                provider: "openai".into(),
                                display_name: display,
                            },
                            provider,
                        );
                    }
                } else {
                    for &(model_id, display_name) in OPENAI_MODELS {
                        if self.providers.contains_key(model_id) {
                            continue;
                        }
                        let provider = Arc::new(openai::OpenAiProvider::new(
                            key.clone(),
                            model_id.into(),
                            base_url.clone(),
                        ));
                        self.register(
                            ModelInfo {
                                id: model_id.into(),
                                provider: "openai".into(),
                                display_name: display_name.into(),
                            },
                            provider,
                        );
                    }
                }
            }
        }
    }

    pub fn get(&self, model_id: &str) -> Option<Arc<dyn LlmProvider>> {
        self.providers.get(model_id).cloned()
    }

    pub fn first(&self) -> Option<Arc<dyn LlmProvider>> {
        self.models
            .first()
            .and_then(|m| self.providers.get(&m.id))
            .cloned()
    }

    /// Return the first provider that supports tool calling,
    /// falling back to the first provider overall.
    pub fn first_with_tools(&self) -> Option<Arc<dyn LlmProvider>> {
        self.models
            .iter()
            .filter_map(|m| self.providers.get(&m.id))
            .find(|p| p.supports_tools())
            .cloned()
            .or_else(|| self.first())
    }

    pub fn list_models(&self) -> &[ModelInfo] {
        &self.models
    }

    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    pub fn provider_summary(&self) -> String {
        if self.models.is_empty() {
            return "no LLM providers configured".into();
        }
        self.models
            .iter()
            .map(|m| format!("{}: {}", m.provider, m.id))
            .collect::<Vec<_>>()
            .join(", ")
    }
}
