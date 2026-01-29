pub mod anthropic;
pub mod openai;

#[cfg(feature = "provider-genai")]
pub mod genai_provider;

#[cfg(feature = "provider-async-openai")]
pub mod async_openai_provider;

use std::collections::HashMap;
use std::sync::Arc;

use crate::model::LlmProvider;

/// Info about an available model.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ModelInfo {
    pub id: String,
    pub provider: String,
    pub display_name: String,
}

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
    ///
    /// Provider priority (first registered wins for a given model ID):
    /// 1. genai-backed providers (if `provider-genai` feature enabled)
    /// 2. async-openai-backed providers (if `provider-async-openai` feature enabled)
    /// 3. Built-in raw reqwest providers (always available)
    pub fn from_env() -> Self {
        let mut reg = Self {
            providers: HashMap::new(),
            models: Vec::new(),
        };

        // --- genai providers (multi-provider crate) ---
        #[cfg(feature = "provider-genai")]
        {
            reg.register_genai_providers();
        }

        // --- async-openai provider ---
        #[cfg(feature = "provider-async-openai")]
        {
            reg.register_async_openai_providers();
        }

        // --- Built-in raw reqwest providers (fallback) ---
        reg.register_builtin_providers();

        reg
    }

    #[cfg(feature = "provider-genai")]
    fn register_genai_providers(&mut self) {
        // genai picks up keys from standard env vars automatically
        // (OPENAI_API_KEY, ANTHROPIC_API_KEY, GEMINI_API_KEY, etc.)
        // We register known models if their env var is present.

        let genai_models: &[(&str, &str, &str, &str)] = &[
            (
                "ANTHROPIC_API_KEY",
                "claude-sonnet-4-20250514",
                "genai/anthropic",
                "Claude Sonnet 4 (genai)",
            ),
            (
                "OPENAI_API_KEY",
                "gpt-4o",
                "genai/openai",
                "GPT-4o (genai)",
            ),
            (
                "GEMINI_API_KEY",
                "gemini-2.0-flash",
                "genai/gemini",
                "Gemini 2.0 Flash (genai)",
            ),
            (
                "GROQ_API_KEY",
                "llama-3.1-8b-instant",
                "genai/groq",
                "Llama 3.1 8B (genai/groq)",
            ),
            (
                "XAI_API_KEY",
                "grok-3-mini",
                "genai/xai",
                "Grok 3 Mini (genai)",
            ),
            (
                "DEEPSEEK_API_KEY",
                "deepseek-chat",
                "genai/deepseek",
                "DeepSeek Chat (genai)",
            ),
        ];

        for &(env_key, model_id, provider_name, display_name) in genai_models {
            if std::env::var(env_key).is_ok() && !self.providers.contains_key(model_id) {
                let provider = Arc::new(genai_provider::GenaiProvider::new(
                    model_id.into(),
                    provider_name.into(),
                ));
                self.register(
                    ModelInfo {
                        id: model_id.into(),
                        provider: provider_name.into(),
                        display_name: display_name.into(),
                    },
                    provider,
                );
            }
        }
    }

    #[cfg(feature = "provider-async-openai")]
    fn register_async_openai_providers(&mut self) {
        if let Ok(key) = std::env::var("OPENAI_API_KEY") {
            let base_url = std::env::var("OPENAI_BASE_URL")
                .unwrap_or_else(|_| "https://api.openai.com/v1".into());

            // Only register if genai didn't already claim this model.
            let model_id = "gpt-4o";
            if !self.providers.contains_key(model_id) {
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
        }
    }

    fn register_builtin_providers(&mut self) {
        // Built-in raw reqwest providers as final fallback.
        if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
            let model_id = "claude-sonnet-4-20250514";
            if !self.providers.contains_key(model_id) {
                let base_url = std::env::var("ANTHROPIC_BASE_URL")
                    .unwrap_or_else(|_| "https://api.anthropic.com".into());
                let provider = Arc::new(anthropic::AnthropicProvider::new(
                    key,
                    model_id.into(),
                    base_url,
                ));
                self.register(
                    ModelInfo {
                        id: model_id.into(),
                        provider: "anthropic".into(),
                        display_name: "Claude Sonnet 4".into(),
                    },
                    provider,
                );
            }
        }

        if let Ok(key) = std::env::var("OPENAI_API_KEY") {
            let model_id = "gpt-4o";
            if !self.providers.contains_key(model_id) {
                let base_url = std::env::var("OPENAI_BASE_URL")
                    .unwrap_or_else(|_| "https://api.openai.com/v1".into());
                let provider = Arc::new(openai::OpenAiProvider::new(
                    key,
                    model_id.into(),
                    base_url,
                ));
                self.register(
                    ModelInfo {
                        id: model_id.into(),
                        provider: "openai".into(),
                        display_name: "GPT-4o".into(),
                    },
                    provider,
                );
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
