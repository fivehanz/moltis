//! Model registry for local GGUF models.
//!
//! Defines available models with HuggingFace URLs, memory requirements,
//! and chat template hints.

use std::path::PathBuf;

use {anyhow::Context, futures::StreamExt, tracing::info};

use super::{chat_templates::ChatTemplateHint, system_info::MemoryTier};

/// Progress info for model downloads.
#[derive(Debug, Clone)]
pub struct DownloadProgress {
    /// Bytes downloaded so far.
    pub downloaded: u64,
    /// Total bytes (if known from Content-Length).
    pub total: Option<u64>,
}

/// Definition of a GGUF model in the registry.
#[derive(Debug, Clone)]
pub struct GgufModelDef {
    /// Model identifier (e.g., "qwen2.5-coder-7b-q4_k_m").
    pub id: &'static str,
    /// Human-readable display name.
    pub display_name: &'static str,
    /// HuggingFace repository (e.g., "Qwen/Qwen2.5-Coder-7B-Instruct-GGUF").
    pub hf_repo: &'static str,
    /// Filename in the repository.
    pub hf_filename: &'static str,
    /// Minimum RAM required in GB.
    pub min_ram_gb: u32,
    /// Context window size in tokens.
    pub context_window: u32,
    /// Chat template hint for formatting messages.
    pub chat_template: ChatTemplateHint,
}

impl GgufModelDef {
    /// HuggingFace download URL for this model.
    #[must_use]
    pub fn hf_url(&self) -> String {
        format!(
            "https://huggingface.co/{}/resolve/main/{}",
            self.hf_repo, self.hf_filename
        )
    }
}

/// Model registry — all known GGUF models organized by memory tier.
///
/// Models are listed in recommended order within each tier.
pub static MODEL_REGISTRY: &[GgufModelDef] = &[
    // ── 4GB tier (Tiny) ────────────────────────────────────────────────────
    GgufModelDef {
        id: "qwen2.5-coder-1.5b-q4_k_m",
        display_name: "Qwen 2.5 Coder 1.5B (Q4_K_M)",
        hf_repo: "Qwen/Qwen2.5-Coder-1.5B-Instruct-GGUF",
        hf_filename: "qwen2.5-coder-1.5b-instruct-q4_k_m.gguf",
        min_ram_gb: 4,
        context_window: 32_768,
        chat_template: ChatTemplateHint::ChatML,
    },
    GgufModelDef {
        id: "llama-3.2-1b-q4_k_m",
        display_name: "Llama 3.2 1B (Q4_K_M)",
        hf_repo: "bartowski/Llama-3.2-1B-Instruct-GGUF",
        hf_filename: "Llama-3.2-1B-Instruct-Q4_K_M.gguf",
        min_ram_gb: 4,
        context_window: 128_000,
        chat_template: ChatTemplateHint::Llama3,
    },
    // ── 8GB tier (Small) ───────────────────────────────────────────────────
    GgufModelDef {
        id: "qwen2.5-coder-7b-q4_k_m",
        display_name: "Qwen 2.5 Coder 7B (Q4_K_M)",
        hf_repo: "Qwen/Qwen2.5-Coder-7B-Instruct-GGUF",
        hf_filename: "qwen2.5-coder-7b-instruct-q4_k_m.gguf",
        min_ram_gb: 8,
        context_window: 32_768,
        chat_template: ChatTemplateHint::ChatML,
    },
    GgufModelDef {
        id: "llama-3.2-3b-q4_k_m",
        display_name: "Llama 3.2 3B (Q4_K_M)",
        hf_repo: "bartowski/Llama-3.2-3B-Instruct-GGUF",
        hf_filename: "Llama-3.2-3B-Instruct-Q4_K_M.gguf",
        min_ram_gb: 8,
        context_window: 128_000,
        chat_template: ChatTemplateHint::Llama3,
    },
    GgufModelDef {
        id: "deepseek-coder-6.7b-q4_k_m",
        display_name: "DeepSeek Coder 6.7B (Q4_K_M)",
        hf_repo: "TheBloke/deepseek-coder-6.7B-instruct-GGUF",
        hf_filename: "deepseek-coder-6.7b-instruct.Q4_K_M.gguf",
        min_ram_gb: 8,
        context_window: 16_384,
        chat_template: ChatTemplateHint::DeepSeek,
    },
    // ── 16GB tier (Medium) ─────────────────────────────────────────────────
    GgufModelDef {
        id: "qwen2.5-coder-14b-q4_k_m",
        display_name: "Qwen 2.5 Coder 14B (Q4_K_M)",
        hf_repo: "Qwen/Qwen2.5-Coder-14B-Instruct-GGUF",
        hf_filename: "qwen2.5-coder-14b-instruct-q4_k_m.gguf",
        min_ram_gb: 16,
        context_window: 32_768,
        chat_template: ChatTemplateHint::ChatML,
    },
    GgufModelDef {
        id: "codestral-22b-q4_k_m",
        display_name: "Codestral 22B (Q4_K_M)",
        hf_repo: "bartowski/Codestral-22B-v0.1-GGUF",
        hf_filename: "Codestral-22B-v0.1-Q4_K_M.gguf",
        min_ram_gb: 16,
        context_window: 32_768,
        chat_template: ChatTemplateHint::Mistral,
    },
    GgufModelDef {
        id: "mistral-7b-q5_k_m",
        display_name: "Mistral 7B Instruct (Q5_K_M)",
        hf_repo: "TheBloke/Mistral-7B-Instruct-v0.2-GGUF",
        hf_filename: "mistral-7b-instruct-v0.2.Q5_K_M.gguf",
        min_ram_gb: 12,
        context_window: 32_768,
        chat_template: ChatTemplateHint::Mistral,
    },
    GgufModelDef {
        id: "llama-3.1-8b-q4_k_m",
        display_name: "Llama 3.1 8B (Q4_K_M)",
        hf_repo: "bartowski/Meta-Llama-3.1-8B-Instruct-GGUF",
        hf_filename: "Meta-Llama-3.1-8B-Instruct-Q4_K_M.gguf",
        min_ram_gb: 12,
        context_window: 128_000,
        chat_template: ChatTemplateHint::Llama3,
    },
    // ── 32GB tier (Large) ──────────────────────────────────────────────────
    GgufModelDef {
        id: "qwen2.5-coder-32b-q4_k_m",
        display_name: "Qwen 2.5 Coder 32B (Q4_K_M)",
        hf_repo: "Qwen/Qwen2.5-Coder-32B-Instruct-GGUF",
        hf_filename: "qwen2.5-coder-32b-instruct-q4_k_m.gguf",
        min_ram_gb: 32,
        context_window: 32_768,
        chat_template: ChatTemplateHint::ChatML,
    },
    GgufModelDef {
        id: "deepseek-coder-33b-q4_k_m",
        display_name: "DeepSeek Coder 33B (Q4_K_M)",
        hf_repo: "TheBloke/deepseek-coder-33B-instruct-GGUF",
        hf_filename: "deepseek-coder-33b-instruct.Q4_K_M.gguf",
        min_ram_gb: 32,
        context_window: 16_384,
        chat_template: ChatTemplateHint::DeepSeek,
    },
    GgufModelDef {
        id: "llama-3.1-70b-q2_k",
        display_name: "Llama 3.1 70B (Q2_K)",
        hf_repo: "bartowski/Meta-Llama-3.1-70B-Instruct-GGUF",
        hf_filename: "Meta-Llama-3.1-70B-Instruct-Q2_K.gguf",
        min_ram_gb: 48,
        context_window: 128_000,
        chat_template: ChatTemplateHint::Llama3,
    },
];

/// Find a model definition by ID.
#[must_use]
pub fn find_model(id: &str) -> Option<&'static GgufModelDef> {
    MODEL_REGISTRY.iter().find(|m| m.id == id)
}

/// Get models suitable for a given memory tier.
#[must_use]
pub fn models_for_tier(tier: MemoryTier) -> Vec<&'static GgufModelDef> {
    let max_ram = match tier {
        MemoryTier::Tiny => 4,
        MemoryTier::Small => 8,
        MemoryTier::Medium => 16,
        MemoryTier::Large => u32::MAX,
    };
    MODEL_REGISTRY
        .iter()
        .filter(|m| m.min_ram_gb <= max_ram)
        .collect()
}

/// Suggest the best model for a memory tier.
#[must_use]
pub fn suggest_model(tier: MemoryTier) -> Option<&'static GgufModelDef> {
    let models = models_for_tier(tier);
    // Return the last model that fits (usually the largest that works)
    models.iter().copied().max_by_key(|m| m.min_ram_gb)
}

/// Default cache directory for downloaded models.
#[must_use]
pub fn default_models_dir() -> PathBuf {
    directories::ProjectDirs::from("", "", "moltis")
        .map(|d: directories::ProjectDirs| d.data_dir().join("models"))
        .unwrap_or_else(|| PathBuf::from(".moltis/models"))
}

/// Ensure a model is downloaded, returning the path to the GGUF file.
///
/// Downloads from HuggingFace if not present in the cache.
pub async fn ensure_model(model: &GgufModelDef, cache_dir: &PathBuf) -> anyhow::Result<PathBuf> {
    ensure_model_with_progress(model, cache_dir, |_| {}).await
}

/// Ensure a model is downloaded with progress reporting.
///
/// The progress callback is called periodically during download with the current progress.
pub async fn ensure_model_with_progress<F>(
    model: &GgufModelDef,
    cache_dir: &PathBuf,
    mut on_progress: F,
) -> anyhow::Result<PathBuf>
where
    F: FnMut(DownloadProgress),
{
    let model_path = cache_dir.join(model.hf_filename);
    if model_path.exists() {
        info!(path = %model_path.display(), model = model.id, "model found in cache");
        return Ok(model_path);
    }

    tokio::fs::create_dir_all(cache_dir)
        .await
        .context("creating models cache dir")?;

    let url = model.hf_url();
    info!(url = %url, model = model.id, "downloading model");

    let response = reqwest::get(&url)
        .await
        .context("downloading GGUF model")?
        .error_for_status()
        .context("GGUF model download failed")?;

    let total = response.content_length();
    let mut downloaded: u64 = 0;

    // Report initial progress
    on_progress(DownloadProgress { downloaded, total });

    // Stream the download to a temp file
    let tmp_path = model_path.with_extension("tmp");
    let mut file = tokio::fs::File::create(&tmp_path)
        .await
        .context("creating temp file")?;

    let mut stream = response.bytes_stream();
    let mut last_report = std::time::Instant::now();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading chunk")?;
        downloaded += chunk.len() as u64;

        tokio::io::AsyncWriteExt::write_all(&mut file, &chunk)
            .await
            .context("writing chunk")?;

        // Report progress at most every 100ms to avoid flooding
        if last_report.elapsed() >= std::time::Duration::from_millis(100) {
            on_progress(DownloadProgress { downloaded, total });
            last_report = std::time::Instant::now();
        }
    }

    // Final progress report
    on_progress(DownloadProgress { downloaded, total });

    // Flush and close the file before renaming
    tokio::io::AsyncWriteExt::flush(&mut file)
        .await
        .context("flushing file")?;
    drop(file);

    tokio::fs::rename(&tmp_path, &model_path)
        .await
        .context("renaming model file")?;

    info!(
        path = %model_path.display(),
        size_mb = downloaded / (1024 * 1024),
        model = model.id,
        "model downloaded"
    );

    Ok(model_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_model() {
        assert!(find_model("qwen2.5-coder-7b-q4_k_m").is_some());
        assert!(find_model("nonexistent-model").is_none());
    }

    #[test]
    fn test_hf_url() {
        let model = find_model("qwen2.5-coder-7b-q4_k_m").unwrap();
        let url = model.hf_url();
        assert!(url.starts_with("https://huggingface.co/"));
        assert!(url.contains("Qwen"));
        assert!(url.ends_with(".gguf"));
    }

    #[test]
    fn test_models_for_tier() {
        let tiny = models_for_tier(MemoryTier::Tiny);
        assert!(!tiny.is_empty());
        for m in &tiny {
            assert!(m.min_ram_gb <= 4);
        }

        let small = models_for_tier(MemoryTier::Small);
        assert!(small.len() >= tiny.len());

        let medium = models_for_tier(MemoryTier::Medium);
        assert!(medium.len() >= small.len());

        let large = models_for_tier(MemoryTier::Large);
        assert_eq!(large.len(), MODEL_REGISTRY.len());
    }

    #[test]
    fn test_suggest_model() {
        // Each tier should have a suggestion
        assert!(suggest_model(MemoryTier::Tiny).is_some());
        assert!(suggest_model(MemoryTier::Small).is_some());
        assert!(suggest_model(MemoryTier::Medium).is_some());
        assert!(suggest_model(MemoryTier::Large).is_some());
    }

    #[test]
    fn test_default_models_dir() {
        let dir = default_models_dir();
        assert!(dir.to_string_lossy().contains("models"));
    }

    #[test]
    fn test_model_registry_unique_ids() {
        let mut ids: Vec<&str> = MODEL_REGISTRY.iter().map(|m| m.id).collect();
        ids.sort();
        let len_before = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), len_before, "duplicate model IDs found");
    }

    #[test]
    fn test_model_registry_valid_urls() {
        for model in MODEL_REGISTRY {
            let url = model.hf_url();
            assert!(
                url.starts_with("https://huggingface.co/"),
                "invalid URL for {}: {}",
                model.id,
                url
            );
            assert!(url.ends_with(".gguf"), "URL should end with .gguf: {}", url);
        }
    }

    #[test]
    fn test_model_registry_context_windows() {
        for model in MODEL_REGISTRY {
            assert!(
                model.context_window > 0,
                "model {} has zero context window",
                model.id
            );
        }
    }
}
