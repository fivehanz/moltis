use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio::task::AbortHandle;
use tokio::sync::RwLock;
use tokio_stream::StreamExt;
use tracing::{debug, warn};

use moltis_agents::model::StreamEvent;
use moltis_agents::providers::ProviderRegistry;

use crate::broadcast::{broadcast, BroadcastOpts};
use crate::services::{ChatService, ModelService, ServiceResult};
use crate::state::GatewayState;

// ── LiveModelService ────────────────────────────────────────────────────────

pub struct LiveModelService {
    providers: Arc<ProviderRegistry>,
}

impl LiveModelService {
    pub fn new(providers: Arc<ProviderRegistry>) -> Self {
        Self { providers }
    }
}

#[async_trait]
impl ModelService for LiveModelService {
    async fn list(&self) -> ServiceResult {
        let models: Vec<_> = self
            .providers
            .list_models()
            .iter()
            .map(|m| {
                serde_json::json!({
                    "id": m.id,
                    "provider": m.provider,
                    "displayName": m.display_name,
                })
            })
            .collect();
        Ok(serde_json::json!(models))
    }
}

// ── LiveChatService ─────────────────────────────────────────────────────────

pub struct LiveChatService {
    providers: Arc<ProviderRegistry>,
    state: Arc<GatewayState>,
    active_runs: Arc<RwLock<HashMap<String, AbortHandle>>>,
}

impl LiveChatService {
    pub fn new(providers: Arc<ProviderRegistry>, state: Arc<GatewayState>) -> Self {
        Self {
            providers,
            state,
            active_runs: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

#[async_trait]
impl ChatService for LiveChatService {
    async fn send(&self, params: Value) -> ServiceResult {
        let text = params
            .get("text")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing 'text' parameter".to_string())?
            .to_string();

        let model_id = params.get("model").and_then(|v| v.as_str());

        let provider = if let Some(id) = model_id {
            self.providers.get(id).ok_or_else(|| {
                let available: Vec<_> = self
                    .providers
                    .list_models()
                    .iter()
                    .map(|m| m.id.clone())
                    .collect();
                format!("model '{}' not found. available: {:?}", id, available)
            })?
        } else {
            self.providers
                .first()
                .ok_or_else(|| "no LLM providers configured".to_string())?
        };

        let run_id = uuid::Uuid::new_v4().to_string();
        let state = Arc::clone(&self.state);
        let active_runs = Arc::clone(&self.active_runs);
        let run_id_clone = run_id.clone();

        let handle = tokio::spawn(async move {
            let messages = vec![serde_json::json!({
                "role": "user",
                "content": text,
            })];

            let mut stream = provider.stream(messages);
            let mut accumulated = String::new();

            while let Some(event) = stream.next().await {
                match event {
                    StreamEvent::Delta(delta) => {
                        accumulated.push_str(&delta);
                        broadcast(
                            &state,
                            "chat",
                            serde_json::json!({
                                "runId": run_id_clone,
                                "state": "delta",
                                "text": delta,
                            }),
                            BroadcastOpts::default(),
                        )
                        .await;
                    }
                    StreamEvent::Done(usage) => {
                        debug!(
                            run_id = %run_id_clone,
                            input_tokens = usage.input_tokens,
                            output_tokens = usage.output_tokens,
                            "chat stream done"
                        );
                        broadcast(
                            &state,
                            "chat",
                            serde_json::json!({
                                "runId": run_id_clone,
                                "state": "final",
                                "text": accumulated,
                            }),
                            BroadcastOpts::default(),
                        )
                        .await;
                        break;
                    }
                    StreamEvent::Error(msg) => {
                        warn!(run_id = %run_id_clone, error = %msg, "chat stream error");
                        broadcast(
                            &state,
                            "chat",
                            serde_json::json!({
                                "runId": run_id_clone,
                                "state": "error",
                                "message": msg,
                            }),
                            BroadcastOpts::default(),
                        )
                        .await;
                        break;
                    }
                }
            }

            active_runs.write().await.remove(&run_id_clone);
        });

        self.active_runs
            .write()
            .await
            .insert(run_id.clone(), handle.abort_handle());

        Ok(serde_json::json!({ "runId": run_id }))
    }

    async fn abort(&self, params: Value) -> ServiceResult {
        let run_id = params
            .get("runId")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing 'runId'".to_string())?;

        if let Some(handle) = self.active_runs.write().await.remove(run_id) {
            handle.abort();
        }
        Ok(serde_json::json!({}))
    }

    async fn history(&self, _params: Value) -> ServiceResult {
        Ok(serde_json::json!([]))
    }

    async fn inject(&self, _params: Value) -> ServiceResult {
        Err("inject not yet implemented".into())
    }
}
