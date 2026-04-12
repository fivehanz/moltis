//! Live approval service and broadcaster for the gateway.

use std::sync::Arc;

use {
    async_trait::async_trait,
    serde_json::Value,
    tracing::{info, warn},
};

use {
    moltis_channels::ChannelReplyTarget,
    moltis_sessions::metadata::SessionEntry,
    moltis_tools::{
        approval::{ApprovalDecision, ApprovalManager},
        exec::ApprovalBroadcaster,
    },
};

use crate::{
    broadcast::{BroadcastOpts, broadcast},
    services::{ExecApprovalService, ServiceResult},
    state::GatewayState,
};

/// Live approval service backed by an `ApprovalManager`.
pub struct LiveExecApprovalService {
    manager: Arc<ApprovalManager>,
}

impl LiveExecApprovalService {
    pub fn new(manager: Arc<ApprovalManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl ExecApprovalService for LiveExecApprovalService {
    async fn get(&self) -> ServiceResult {
        Ok(serde_json::json!({
            "mode": self.manager.mode,
            "securityLevel": self.manager.security_level,
        }))
    }

    async fn set(&self, _params: Value) -> ServiceResult {
        // Config mutation not yet implemented.
        Ok(serde_json::json!({}))
    }

    async fn node_get(&self, _params: Value) -> ServiceResult {
        Ok(serde_json::json!({ "mode": self.manager.mode }))
    }

    async fn node_set(&self, _params: Value) -> ServiceResult {
        Ok(serde_json::json!({}))
    }

    async fn request(&self, _params: Value) -> ServiceResult {
        let requests = if let Some(session_key) = _params.get("sessionKey").and_then(|v| v.as_str())
        {
            self.manager.pending_requests_for_session(session_key).await
        } else {
            self.manager.pending_requests().await
        };
        let pending = requests
            .iter()
            .map(|request| request.id.clone())
            .collect::<Vec<_>>();
        Ok(serde_json::json!({
            "pending": pending,
            "requests": requests,
        }))
    }

    async fn resolve(&self, params: Value) -> ServiceResult {
        let id = params
            .get("requestId")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing 'requestId'".to_string())?;

        let decision_str = params
            .get("decision")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing 'decision'".to_string())?;

        let decision = match decision_str {
            "approved" => ApprovalDecision::Approved,
            "denied" => ApprovalDecision::Denied,
            _ => return Err(format!("invalid decision: {decision_str}").into()),
        };

        let command = params.get("command").and_then(|v| v.as_str());

        info!(id, ?decision, "resolving approval request");
        self.manager.resolve(id, decision, command).await;

        Ok(serde_json::json!({ "ok": true }))
    }
}

/// Broadcasts approval requests to connected WebSocket clients.
pub struct GatewayApprovalBroadcaster {
    state: Arc<GatewayState>,
}

impl GatewayApprovalBroadcaster {
    pub fn new(state: Arc<GatewayState>) -> Self {
        Self { state }
    }

    async fn notify_origin_channel(
        &self,
        session_key: Option<&str>,
        command: &str,
    ) -> moltis_tools::Result<()> {
        let Some(session_key) = session_key else {
            return Ok(());
        };
        let Some(session_metadata) = self.state.services.session_metadata.as_ref() else {
            return Ok(());
        };
        let Some(outbound) = self.state.services.channel_outbound_arc() else {
            return Ok(());
        };

        let target = session_metadata
            .get(session_key)
            .await
            .and_then(|entry| channel_reply_target_for_entry(&entry));
        let Some(target) = target else {
            return Ok(());
        };

        let message = format!(
            "Approval needed for `{}`.\nUse /approvals to see the numbered list, then /approve N or /deny N. The web UI still works too.",
            command
        );
        outbound
            .send_text(&target.account_id, &target.outbound_to(), &message, None)
            .await
            .map_err(|error| moltis_tools::Error::external("send approval notification", error))
    }
}

#[async_trait]
impl ApprovalBroadcaster for GatewayApprovalBroadcaster {
    async fn broadcast_request(
        &self,
        request_id: &str,
        command: &str,
        session_key: Option<&str>,
    ) -> moltis_tools::Result<()> {
        broadcast(
            &self.state,
            "exec.approval.requested",
            serde_json::json!({
                "requestId": request_id,
                "command": command,
                "sessionKey": session_key,
            }),
            BroadcastOpts::default(),
        )
        .await;
        if let Err(error) = self.notify_origin_channel(session_key, command).await {
            warn!(%error, session_key, request_id, "failed to notify originating channel about approval");
        }
        Ok(())
    }
}

fn channel_reply_target_for_entry(entry: &SessionEntry) -> Option<ChannelReplyTarget> {
    entry
        .channel_binding
        .as_deref()
        .and_then(|binding| serde_json::from_str(binding).ok())
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_live_service_resolve() {
        let mgr = Arc::new(ApprovalManager::default());
        let svc = LiveExecApprovalService::new(Arc::clone(&mgr));

        // Create a pending request.
        let (id, mut rx) = mgr.create_request("rm -rf /", Some("session:test")).await;

        // Resolve via the service.
        let result = svc
            .resolve(serde_json::json!({
                "requestId": id,
                "decision": "denied",
            }))
            .await;
        assert!(result.is_ok());

        // The receiver should get Denied.
        let decision = rx.try_recv().unwrap();
        assert_eq!(decision, ApprovalDecision::Denied);
    }

    #[tokio::test]
    async fn test_live_service_get() {
        let mgr = Arc::new(ApprovalManager::default());
        let svc = LiveExecApprovalService::new(mgr);
        let result = svc.get().await.unwrap();
        // Default mode is on-miss.
        assert_eq!(result["mode"], "on-miss");
    }

    #[tokio::test]
    async fn test_live_service_request_filters_by_session() {
        let mgr = Arc::new(ApprovalManager::default());
        let svc = LiveExecApprovalService::new(Arc::clone(&mgr));
        let _ = mgr.create_request("echo one", Some("session:a")).await;
        let _ = mgr.create_request("echo two", Some("session:b")).await;

        let result = svc
            .request(serde_json::json!({ "sessionKey": "session:a" }))
            .await
            .unwrap();
        let requests = result["requests"]
            .as_array()
            .expect("requests should be an array");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["session_key"], "session:a");
        assert_eq!(requests[0]["command"], "echo one");
    }

    #[test]
    fn test_channel_reply_target_for_entry_parses_channel_binding() {
        let entry = SessionEntry {
            id: "1".into(),
            key: "telegram:bot-main:-100123".into(),
            label: None,
            model: None,
            created_at: 0,
            updated_at: 0,
            message_count: 0,
            last_seen_message_count: 0,
            project_id: None,
            archived: false,
            worktree_branch: None,
            sandbox_enabled: None,
            sandbox_image: None,
            channel_binding: Some(
                serde_json::json!({
                    "channel_type": "telegram",
                    "account_id": "bot-main",
                    "chat_id": "-100123",
                    "thread_id": "42"
                })
                .to_string(),
            ),
            parent_session_key: None,
            fork_point: None,
            mcp_disabled: None,
            preview: None,
            agent_id: None,
            node_id: None,
            version: 0,
        };

        let target = channel_reply_target_for_entry(&entry).expect("expected channel target");
        assert_eq!(target.account_id, "bot-main");
        assert_eq!(target.outbound_to(), "-100123:42");
    }

    #[test]
    fn test_channel_reply_target_for_entry_rejects_invalid_binding() {
        let entry = SessionEntry {
            id: "1".into(),
            key: "session:abc".into(),
            label: None,
            model: None,
            created_at: 0,
            updated_at: 0,
            message_count: 0,
            last_seen_message_count: 0,
            project_id: None,
            archived: false,
            worktree_branch: None,
            sandbox_enabled: None,
            sandbox_image: None,
            channel_binding: Some("{not-json".into()),
            parent_session_key: None,
            fork_point: None,
            mcp_disabled: None,
            preview: None,
            agent_id: None,
            node_id: None,
            version: 0,
        };

        assert!(channel_reply_target_for_entry(&entry).is_none());
    }
}
