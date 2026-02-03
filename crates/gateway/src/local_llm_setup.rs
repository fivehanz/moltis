//! Local LLM provider setup service.
//!
//! Provides RPC handlers for configuring the local GGUF LLM provider,
//! including system info detection, model listing, and model configuration.

use std::{path::PathBuf, sync::Arc};

use {
    async_trait::async_trait,
    serde::{Deserialize, Serialize},
    serde_json::Value,
    tokio::sync::{OnceCell, RwLock},
    tracing::info,
};

use moltis_agents::providers::{ProviderRegistry, local_gguf};

use crate::{
    broadcast::{BroadcastOpts, broadcast},
    services::{LocalLlmService, ServiceResult},
    state::GatewayState,
};

/// Check if the mlx-lm Python package is installed.
fn is_mlx_installed() -> bool {
    std::process::Command::new("python3")
        .args(["-c", "import mlx_lm"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Configuration file for local-llm stored in the config directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalLlmConfig {
    pub model_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_path: Option<PathBuf>,
    #[serde(default)]
    pub gpu_layers: u32,
    /// Backend to use: "GGUF" or "MLX"
    #[serde(default = "default_backend")]
    pub backend: String,
}

fn default_backend() -> String {
    "GGUF".to_string()
}

impl LocalLlmConfig {
    /// Load config from the config directory.
    pub fn load() -> Option<Self> {
        let config_dir = moltis_config::config_dir()?;
        let config_path = config_dir.join("local-llm.json");
        let content = std::fs::read_to_string(&config_path).ok()?;
        serde_json::from_str(&content).ok()
    }

    /// Save config to the config directory.
    pub fn save(&self) -> anyhow::Result<()> {
        let config_dir =
            moltis_config::config_dir().ok_or_else(|| anyhow::anyhow!("no config directory"))?;
        std::fs::create_dir_all(&config_dir)?;
        let config_path = config_dir.join("local-llm.json");
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(config_path, content)?;
        Ok(())
    }
}

/// Status of the local LLM provider.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum LocalLlmStatus {
    /// No model configured.
    Unconfigured,
    /// Model configured but not yet loaded.
    Ready { model_id: String },
    /// Model is being downloaded/loaded.
    Loading {
        model_id: String,
        progress: Option<f32>,
    },
    /// Model is loaded and ready.
    Loaded { model_id: String },
    /// Error loading model.
    Error { model_id: String, error: String },
    /// Feature not enabled.
    Unavailable,
}

/// Live implementation of LocalLlmService.
pub struct LiveLocalLlmService {
    registry: Arc<RwLock<ProviderRegistry>>,
    status: Arc<RwLock<LocalLlmStatus>>,
    /// State reference for broadcasting progress (set after state is created).
    state: Arc<OnceCell<Arc<GatewayState>>>,
}

impl LiveLocalLlmService {
    pub fn new(registry: Arc<RwLock<ProviderRegistry>>) -> Self {
        // Check if we have a saved config and set initial status
        let status = if let Some(config) = LocalLlmConfig::load() {
            // Check if the model is already in the registry
            let model_id = config.model_id.clone();
            LocalLlmStatus::Ready { model_id }
        } else {
            LocalLlmStatus::Unconfigured
        };

        Self {
            registry,
            status: Arc::new(RwLock::new(status)),
            state: Arc::new(OnceCell::new()),
        }
    }

    /// Set the gateway state reference for broadcasting progress updates.
    pub fn set_state(&self, state: Arc<GatewayState>) {
        // Ignore if already set (shouldn't happen in normal operation)
        let _ = self.state.set(state);
    }

    /// Get model display info for JSON response.
    fn model_to_json(model: &local_gguf::models::GgufModelDef, is_suggested: bool) -> Value {
        serde_json::json!({
            "id": model.id,
            "displayName": model.display_name,
            "minRamGb": model.min_ram_gb,
            "contextWindow": model.context_window,
            "hfRepo": model.hf_repo,
            "suggested": is_suggested,
        })
    }
}

#[async_trait]
impl LocalLlmService for LiveLocalLlmService {
    async fn system_info(&self) -> ServiceResult {
        let sys = local_gguf::system_info::SystemInfo::detect();
        let tier = sys.memory_tier();

        // Check MLX availability (requires mlx-lm Python package)
        let mlx_available = sys.is_apple_silicon && is_mlx_installed();

        // Determine the recommended backend
        let recommended_backend = if mlx_available {
            "MLX"
        } else {
            "GGUF"
        };

        // Build available backends list
        let mut available_backends = vec![serde_json::json!({
            "id": "GGUF",
            "name": "GGUF (llama.cpp)",
            "description": if sys.is_apple_silicon {
                "Cross-platform, Metal GPU acceleration"
            } else if sys.has_cuda {
                "Cross-platform, CUDA GPU acceleration"
            } else {
                "Cross-platform, CPU inference"
            },
            "available": true,
        })];

        if sys.is_apple_silicon {
            available_backends.push(serde_json::json!({
                "id": "MLX",
                "name": "MLX (Apple Native)",
                "description": if mlx_available {
                    "Optimized for Apple Silicon, fastest on Mac"
                } else {
                    "Requires: pip install mlx-lm"
                },
                "available": mlx_available,
            }));
        }

        // Build backend note for display
        let backend_note = if mlx_available {
            "MLX recommended (native Apple Silicon optimization)"
        } else if sys.is_apple_silicon {
            "GGUF with Metal (install mlx-lm for native MLX)"
        } else if sys.has_cuda {
            "GGUF with CUDA acceleration"
        } else {
            "GGUF (CPU inference)"
        };

        Ok(serde_json::json!({
            "totalRamGb": sys.total_ram_gb(),
            "availableRamGb": sys.available_ram_gb(),
            "hasMetal": sys.has_metal,
            "hasCuda": sys.has_cuda,
            "hasGpu": sys.has_gpu(),
            "isAppleSilicon": sys.is_apple_silicon,
            "memoryTier": tier.to_string(),
            "recommendedBackend": recommended_backend,
            "availableBackends": available_backends,
            "backendNote": backend_note,
            "mlxAvailable": mlx_available,
        }))
    }

    async fn models(&self) -> ServiceResult {
        let sys = local_gguf::system_info::SystemInfo::detect();
        let tier = sys.memory_tier();

        // Get suggested model for this tier
        let suggested = local_gguf::models::suggest_model(tier);
        let suggested_id = suggested.map(|m| m.id);

        // Get all models for this tier
        let available = local_gguf::models::models_for_tier(tier);

        let models: Vec<Value> = available
            .iter()
            .map(|m| Self::model_to_json(m, Some(m.id) == suggested_id))
            .collect();

        // Also include all models (not just for this tier) in a separate array
        let all_models: Vec<Value> = local_gguf::models::MODEL_REGISTRY
            .iter()
            .map(|m| Self::model_to_json(m, Some(m.id) == suggested_id))
            .collect();

        Ok(serde_json::json!({
            "recommended": models,
            "all": all_models,
            "memoryTier": tier.to_string(),
        }))
    }

    async fn configure(&self, params: Value) -> ServiceResult {
        let model_id = params
            .get("modelId")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing 'modelId' parameter".to_string())?
            .to_string();

        // Get backend choice (default to recommended)
        let sys = local_gguf::system_info::SystemInfo::detect();
        let mlx_available = sys.is_apple_silicon && is_mlx_installed();
        let default_backend = if mlx_available {
            "MLX"
        } else {
            "GGUF"
        };
        let backend = params
            .get("backend")
            .and_then(|v| v.as_str())
            .unwrap_or(default_backend)
            .to_string();

        // Validate backend choice
        if backend != "GGUF" && backend != "MLX" {
            return Err(format!("invalid backend: {backend}. Must be GGUF or MLX"));
        }
        if backend == "MLX" && !mlx_available {
            return Err(
                "MLX backend requires mlx-lm. Install with: pip install mlx-lm".to_string(),
            );
        }

        // Validate model exists in registry
        let model_def = local_gguf::models::find_model(&model_id)
            .ok_or_else(|| format!("unknown model: {model_id}"))?;

        info!(model = %model_id, backend = %backend, "configuring local-llm");

        // Update status to loading
        {
            let mut status = self.status.write().await;
            *status = LocalLlmStatus::Loading {
                model_id: model_id.clone(),
                progress: None,
            };
        }

        // Save configuration
        let config = LocalLlmConfig {
            model_id: model_id.clone(),
            model_path: None,
            gpu_layers: 0,
            backend: backend.clone(),
        };
        config
            .save()
            .map_err(|e| format!("failed to save config: {e}"))?;

        // Trigger model download in background with progress updates
        let model_id_clone = model_id.clone();
        let status = Arc::clone(&self.status);
        let registry = Arc::clone(&self.registry);
        let state_cell = Arc::clone(&self.state);
        let cache_dir = local_gguf::models::default_models_dir();
        let display_name = model_def.display_name.to_string();

        tokio::spawn(async move {
            // Get state if available (for broadcasting progress)
            let state = state_cell.get().cloned();

            // Use a channel to send progress updates to a broadcast task
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(u64, Option<u64>)>();
            let state_for_progress = state.clone();
            let model_id_for_broadcast = model_id_clone.clone();
            let display_name_for_broadcast = display_name.clone();

            // Spawn a task to broadcast progress updates (if state is available)
            let broadcast_task = tokio::spawn(async move {
                let Some(state) = state_for_progress else {
                    // No state available, just drain the channel
                    while rx.recv().await.is_some() {}
                    return;
                };

                while let Some((downloaded, total)) = rx.recv().await {
                    let progress = total.map(|t| {
                        if t > 0 {
                            (downloaded as f64 / t as f64 * 100.0).min(100.0)
                        } else {
                            0.0
                        }
                    });
                    broadcast(
                        &state,
                        "local-llm.download",
                        serde_json::json!({
                            "modelId": model_id_for_broadcast,
                            "displayName": display_name_for_broadcast,
                            "downloaded": downloaded,
                            "total": total,
                            "progress": progress,
                        }),
                        BroadcastOpts::default(),
                    )
                    .await;
                }
            });

            let result =
                local_gguf::models::ensure_model_with_progress(model_def, &cache_dir, |p| {
                    // Send progress to the broadcast task (ignore errors if channel closed)
                    let _ = tx.send((p.downloaded, p.total));
                })
                .await;

            // Drop the sender to signal the broadcast task to finish
            drop(tx);
            // Wait for final broadcasts to complete
            let _ = broadcast_task.await;

            match result {
                Ok(_path) => {
                    info!(model = %model_id_clone, "model downloaded successfully");

                    // Broadcast completion (if state is available)
                    if let Some(state) = &state {
                        broadcast(
                            state,
                            "local-llm.download",
                            serde_json::json!({
                                "modelId": model_id_clone,
                                "displayName": display_name,
                                "progress": 100.0,
                                "complete": true,
                            }),
                            BroadcastOpts::default(),
                        )
                        .await;
                    }

                    // Register the provider in the registry
                    let gguf_config = local_gguf::LocalGgufConfig {
                        model_id: model_id_clone.clone(),
                        model_path: None,
                        context_size: None,
                        gpu_layers: 0,
                        temperature: 0.7,
                        cache_dir,
                    };

                    let provider = Arc::new(local_gguf::LazyLocalGgufProvider::new(gguf_config));

                    let mut reg = registry.write().await;
                    reg.register(
                        moltis_agents::providers::ModelInfo {
                            id: model_id_clone.clone(),
                            provider: "local-llm".into(),
                            display_name,
                        },
                        provider,
                    );

                    let mut s = status.write().await;
                    *s = LocalLlmStatus::Ready {
                        model_id: model_id_clone,
                    };
                },
                Err(e) => {
                    tracing::error!(model = %model_id_clone, error = %e, "failed to download model");

                    // Broadcast error (if state is available)
                    if let Some(state) = &state {
                        broadcast(
                            state,
                            "local-llm.download",
                            serde_json::json!({
                                "modelId": model_id_clone,
                                "error": e.to_string(),
                            }),
                            BroadcastOpts::default(),
                        )
                        .await;
                    }

                    let mut s = status.write().await;
                    *s = LocalLlmStatus::Error {
                        model_id: model_id_clone,
                        error: e.to_string(),
                    };
                },
            }
        });

        Ok(serde_json::json!({
            "ok": true,
            "modelId": model_id,
            "displayName": model_def.display_name,
        }))
    }

    async fn status(&self) -> ServiceResult {
        let status = self.status.read().await;
        Ok(serde_json::to_value(&*status).unwrap_or_else(
            |_| serde_json::json!({ "status": "error", "error": "serialization failed" }),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_local_llm_config_serialization() {
        let config = LocalLlmConfig {
            model_id: "qwen2.5-coder-7b-q4_k_m".into(),
            model_path: None,
            gpu_layers: 0,
            backend: "GGUF".into(),
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("qwen2.5-coder-7b-q4_k_m"));

        let parsed: LocalLlmConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.model_id, "qwen2.5-coder-7b-q4_k_m");
    }

    #[test]
    fn test_status_serialization() {
        let status = LocalLlmStatus::Ready {
            model_id: "test-model".into(),
        };
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["status"], "ready");
        assert_eq!(json["model_id"], "test-model");
    }
}
