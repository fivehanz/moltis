use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::RwLock;

use moltis_sessions::{metadata::SessionMetadata, store::SessionStore};

use crate::services::{ServiceResult, SessionService};

/// Live session service backed by JSONL store + metadata index.
pub struct LiveSessionService {
    store: Arc<SessionStore>,
    metadata: Arc<RwLock<SessionMetadata>>,
}

impl LiveSessionService {
    pub fn new(store: Arc<SessionStore>, metadata: Arc<RwLock<SessionMetadata>>) -> Self {
        Self { store, metadata }
    }
}

#[async_trait]
impl SessionService for LiveSessionService {
    async fn list(&self) -> ServiceResult {
        let meta = self.metadata.read().await;
        let entries: Vec<Value> = meta
            .list()
            .into_iter()
            .map(|e| {
                serde_json::json!({
                    "id": e.id,
                    "key": e.key,
                    "label": e.label,
                    "createdAt": e.created_at,
                    "updatedAt": e.updated_at,
                    "messageCount": e.message_count,
                })
            })
            .collect();
        Ok(serde_json::json!(entries))
    }

    async fn preview(&self, params: Value) -> ServiceResult {
        let key = params
            .get("key")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing 'key' parameter".to_string())?;
        let limit = params
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(5) as usize;

        let messages = self
            .store
            .read_last_n(key, limit)
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::json!({ "messages": messages }))
    }

    async fn resolve(&self, params: Value) -> ServiceResult {
        let key = params
            .get("key")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing 'key' parameter".to_string())?;

        let entry = {
            let meta = self.metadata.read().await;
            meta.get(key).cloned()
        };

        let entry = entry.ok_or_else(|| format!("session '{key}' not found"))?;
        let history = self.store.read(key).await.map_err(|e| e.to_string())?;

        Ok(serde_json::json!({
            "entry": {
                "id": entry.id,
                "key": entry.key,
                "label": entry.label,
                "createdAt": entry.created_at,
                "updatedAt": entry.updated_at,
                "messageCount": entry.message_count,
            },
            "history": history,
        }))
    }

    async fn patch(&self, params: Value) -> ServiceResult {
        let key = params
            .get("key")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing 'key' parameter".to_string())?;
        let label = params.get("label").and_then(|v| v.as_str()).map(String::from);

        let mut meta = self.metadata.write().await;
        if meta.get(key).is_none() {
            return Err(format!("session '{key}' not found"));
        }
        meta.upsert(key, label);
        meta.save().map_err(|e| e.to_string())?;

        let entry = meta.get(key).unwrap();
        Ok(serde_json::json!({
            "id": entry.id,
            "key": entry.key,
            "label": entry.label,
        }))
    }

    async fn reset(&self, params: Value) -> ServiceResult {
        let key = params
            .get("key")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing 'key' parameter".to_string())?;

        self.store.clear(key).await.map_err(|e| e.to_string())?;

        let mut meta = self.metadata.write().await;
        meta.touch(key, 0);
        meta.save().map_err(|e| e.to_string())?;

        Ok(serde_json::json!({}))
    }

    async fn delete(&self, params: Value) -> ServiceResult {
        let key = params
            .get("key")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing 'key' parameter".to_string())?;

        if key == "main" {
            return Err("cannot delete the main session".to_string());
        }

        self.store.clear(key).await.map_err(|e| e.to_string())?;

        let mut meta = self.metadata.write().await;
        meta.remove(key);
        meta.save().map_err(|e| e.to_string())?;

        Ok(serde_json::json!({}))
    }

    async fn compact(&self, _params: Value) -> ServiceResult {
        // Stub — compaction not yet implemented.
        Ok(serde_json::json!({}))
    }
}
