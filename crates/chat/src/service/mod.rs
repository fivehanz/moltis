//! `LiveChatService` struct, constructors, and helper methods.

mod chat_impl;

use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::Arc,
    time::Instant,
};

use {
    serde::Serialize,
    serde_json::Value,
    tokio::{
        sync::{RwLock, Semaphore},
        task::AbortHandle,
    },
    tracing::{debug, info, warn},
};

use {
    moltis_agents::{runner::RunnerEvent, tool_registry::ToolRegistry},
    moltis_providers::ProviderRegistry,
    moltis_sessions::{
        PersistedMessage,
        message::{PersistedFunction, PersistedToolCall},
        metadata::SqliteSessionMetadata,
        state_store::SessionStateStore,
        store::SessionStore,
    },
};

use crate::{error, models::DisabledModelsStore, runtime::ChatRuntime, types::*};

/// A message that arrived while an agent run was already active on the session.
#[derive(Debug, Clone)]
struct QueuedMessage {
    params: Value,
}

/// A tool call currently executing within an active agent run.
#[derive(Debug, Clone, Serialize)]
pub struct ActiveToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
    #[serde(rename = "startedAt")]
    pub started_at: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct ActiveAssistantDraft {
    content: String,
    reasoning: String,
    model: String,
    provider: String,
    seq: Option<u64>,
    run_id: String,
}

impl ActiveAssistantDraft {
    pub(crate) fn new(run_id: &str, model: &str, provider: &str, seq: Option<u64>) -> Self {
        Self {
            content: String::new(),
            reasoning: String::new(),
            model: model.to_string(),
            provider: provider.to_string(),
            seq,
            run_id: run_id.to_string(),
        }
    }

    pub(crate) fn append_text(&mut self, delta: &str) {
        if !delta.is_empty() {
            self.content.push_str(delta);
        }
    }

    pub(crate) fn set_reasoning(&mut self, reasoning: &str) {
        self.reasoning.clear();
        self.reasoning.push_str(reasoning);
    }

    pub(crate) fn has_visible_content(&self) -> bool {
        !self.content.trim().is_empty() || !self.reasoning.trim().is_empty()
    }

    pub(crate) fn to_persisted_message(&self) -> PersistedMessage {
        let reasoning = self.reasoning.trim();
        PersistedMessage::Assistant {
            content: self.content.clone(),
            created_at: Some(now_ms()),
            model: Some(self.model.clone()),
            provider: Some(self.provider.clone()),
            input_tokens: None,
            output_tokens: None,
            duration_ms: None,
            request_input_tokens: None,
            request_output_tokens: None,
            tool_calls: None,
            reasoning: (!reasoning.is_empty()).then(|| reasoning.to_string()),
            llm_api_response: None,
            audio: None,
            seq: self.seq,
            run_id: Some(self.run_id.clone()),
        }
    }
}

fn build_persisted_tool_call(
    tool_call_id: impl Into<String>,
    tool_name: impl Into<String>,
    arguments: Option<Value>,
) -> PersistedToolCall {
    PersistedToolCall {
        id: tool_call_id.into(),
        call_type: "function".to_string(),
        function: PersistedFunction {
            name: tool_name.into(),
            arguments: arguments
                .unwrap_or_else(|| serde_json::json!({}))
                .to_string(),
        },
    }
}

pub(crate) fn build_tool_call_assistant_message(
    tool_call_id: impl Into<String>,
    tool_name: impl Into<String>,
    arguments: Option<Value>,
    seq: Option<u64>,
    run_id: Option<&str>,
) -> PersistedMessage {
    PersistedMessage::Assistant {
        content: String::new(),
        created_at: Some(now_ms()),
        model: None,
        provider: None,
        input_tokens: None,
        output_tokens: None,
        duration_ms: None,
        request_input_tokens: None,
        request_output_tokens: None,
        tool_calls: Some(vec![build_persisted_tool_call(
            tool_call_id,
            tool_name,
            arguments,
        )]),
        reasoning: None,
        llm_api_response: None,
        audio: None,
        seq,
        run_id: run_id.map(str::to_string),
    }
}

pub(crate) async fn persist_tool_history_pair(
    session_store: &Arc<SessionStore>,
    session_key: &str,
    assistant_tool_call_msg: PersistedMessage,
    tool_result_msg: PersistedMessage,
    assistant_warn_context: &str,
    tool_result_warn_context: &str,
) {
    if let Err(e) = session_store
        .append(session_key, &assistant_tool_call_msg.to_value())
        .await
    {
        warn!("{assistant_warn_context}: {e}");
        warn!(
            session = %session_key,
            "skipping tool result persistence to avoid orphaned tool history"
        );
        return;
    }

    if let Err(e) = session_store
        .append(session_key, &tool_result_msg.to_value())
        .await
    {
        warn!("{tool_result_warn_context}: {e}");
    }
}

pub struct LiveChatService {
    providers: Arc<RwLock<ProviderRegistry>>,
    model_store: Arc<RwLock<DisabledModelsStore>>,
    state: Arc<dyn ChatRuntime>,
    active_runs: Arc<RwLock<HashMap<String, AbortHandle>>>,
    active_runs_by_session: Arc<RwLock<HashMap<String, String>>>,
    active_event_forwarders: Arc<RwLock<HashMap<String, tokio::task::JoinHandle<String>>>>,
    terminal_runs: Arc<RwLock<HashSet<String>>>,
    tool_registry: Arc<RwLock<ToolRegistry>>,
    session_store: Arc<SessionStore>,
    session_metadata: Arc<SqliteSessionMetadata>,
    session_state_store: Option<Arc<SessionStateStore>>,
    hook_registry: Option<Arc<moltis_common::hooks::HookRegistry>>,
    /// Per-session semaphore ensuring only one agent run executes per session at a time.
    session_locks: Arc<RwLock<HashMap<String, Arc<Semaphore>>>>,
    /// Per-session message queue for messages arriving during an active run.
    message_queue: Arc<RwLock<HashMap<String, Vec<QueuedMessage>>>>,
    /// Per-session last-seen client sequence number for ordering diagnostics.
    last_client_seq: Arc<RwLock<HashMap<String, u64>>>,
    /// Per-session accumulated thinking text for active runs, so it can be
    /// returned in `sessions.switch` after a page reload.
    active_thinking_text: Arc<RwLock<HashMap<String, String>>>,
    /// Per-session active tool calls for `chat.peek` snapshot.
    active_tool_calls: Arc<RwLock<HashMap<String, Vec<ActiveToolCall>>>>,
    /// Per-session streamed assistant content buffered so an abort can persist
    /// what the user already saw instead of dropping it on the floor.
    active_partial_assistant: Arc<RwLock<HashMap<String, ActiveAssistantDraft>>>,
    /// Per-session reply medium for active runs, so the frontend can restore
    /// `voicePending` state after a page reload.
    active_reply_medium: Arc<RwLock<HashMap<String, ReplyMedium>>>,
    /// Failover configuration for automatic model/provider failover.
    failover_config: moltis_config::schema::FailoverConfig,
}

impl LiveChatService {
    pub fn new(
        providers: Arc<RwLock<ProviderRegistry>>,
        model_store: Arc<RwLock<DisabledModelsStore>>,
        state: Arc<dyn ChatRuntime>,
        session_store: Arc<SessionStore>,
        session_metadata: Arc<SqliteSessionMetadata>,
    ) -> Self {
        Self {
            providers,
            model_store,
            state,
            active_runs: Arc::new(RwLock::new(HashMap::new())),
            active_runs_by_session: Arc::new(RwLock::new(HashMap::new())),
            active_event_forwarders: Arc::new(RwLock::new(HashMap::new())),
            terminal_runs: Arc::new(RwLock::new(HashSet::new())),
            tool_registry: Arc::new(RwLock::new(ToolRegistry::new())),
            session_store,
            session_metadata,
            session_state_store: None,
            hook_registry: None,
            session_locks: Arc::new(RwLock::new(HashMap::new())),
            message_queue: Arc::new(RwLock::new(HashMap::new())),
            last_client_seq: Arc::new(RwLock::new(HashMap::new())),
            active_thinking_text: Arc::new(RwLock::new(HashMap::new())),
            active_tool_calls: Arc::new(RwLock::new(HashMap::new())),
            active_partial_assistant: Arc::new(RwLock::new(HashMap::new())),
            active_reply_medium: Arc::new(RwLock::new(HashMap::new())),
            failover_config: moltis_config::schema::FailoverConfig::default(),
        }
    }

    pub fn with_failover(mut self, config: moltis_config::schema::FailoverConfig) -> Self {
        self.failover_config = config;
        self
    }

    pub fn with_tools(mut self, registry: Arc<RwLock<ToolRegistry>>) -> Self {
        self.tool_registry = registry;
        self
    }

    pub fn with_session_state_store(mut self, store: Arc<SessionStateStore>) -> Self {
        self.session_state_store = Some(store);
        self
    }

    pub fn with_hooks(mut self, registry: moltis_common::hooks::HookRegistry) -> Self {
        self.hook_registry = Some(Arc::new(registry));
        self
    }

    pub fn with_hooks_arc(mut self, registry: Arc<moltis_common::hooks::HookRegistry>) -> Self {
        self.hook_registry = Some(registry);
        self
    }

    fn has_tools_sync(&self) -> bool {
        // Best-effort check: try_read avoids blocking. If the lock is held,
        // assume tools are present (conservative — enables tool mode).
        self.tool_registry
            .try_read()
            .map(|r| {
                let schemas = r.list_schemas();
                let has = !schemas.is_empty();
                tracing::debug!(
                    tool_count = schemas.len(),
                    has_tools = has,
                    "has_tools_sync check"
                );
                has
            })
            .unwrap_or(true)
    }

    /// Return the per-session semaphore, creating one if absent.
    async fn session_semaphore(&self, key: &str) -> Arc<Semaphore> {
        // Fast path: read lock.
        {
            let locks = self.session_locks.read().await;
            if let Some(sem) = locks.get(key) {
                return Arc::clone(sem);
            }
        }
        // Slow path: write lock, insert.
        let mut locks = self.session_locks.write().await;
        Arc::clone(
            locks
                .entry(key.to_string())
                .or_insert_with(|| Arc::new(Semaphore::new(1))),
        )
    }

    async fn abort_run_handle(
        active_runs: &Arc<RwLock<HashMap<String, AbortHandle>>>,
        active_runs_by_session: &Arc<RwLock<HashMap<String, String>>>,
        terminal_runs: &Arc<RwLock<HashSet<String>>>,
        run_id: Option<&str>,
        session_key: Option<&str>,
    ) -> (Option<String>, bool) {
        let resolved_run_id = if let Some(id) = run_id {
            Some(id.to_string())
        } else if let Some(key) = session_key {
            active_runs_by_session.read().await.get(key).cloned()
        } else {
            None
        };

        let Some(target_run_id) = resolved_run_id.clone() else {
            return (None, false);
        };

        if terminal_runs.read().await.contains(&target_run_id) {
            return (resolved_run_id, false);
        }

        let aborted = if let Some(handle) = active_runs.write().await.remove(&target_run_id) {
            handle.abort();
            true
        } else {
            false
        };

        let mut by_session = active_runs_by_session.write().await;
        if let Some(key) = session_key
            && by_session.get(key).is_some_and(|id| id == &target_run_id)
        {
            by_session.remove(key);
        }
        by_session.retain(|_, id| id != &target_run_id);

        (resolved_run_id, aborted)
    }

    async fn resolve_session_key_for_run(
        active_runs_by_session: &Arc<RwLock<HashMap<String, String>>>,
        run_id: Option<&str>,
        session_key: Option<&str>,
    ) -> Option<String> {
        if let Some(key) = session_key {
            return Some(key.to_string());
        }
        let target_run_id = run_id?;
        active_runs_by_session
            .read()
            .await
            .iter()
            .find_map(|(key, active_run_id)| (active_run_id == target_run_id).then(|| key.clone()))
    }

    pub(crate) async fn wait_for_event_forwarder(
        active_event_forwarders: &Arc<RwLock<HashMap<String, tokio::task::JoinHandle<String>>>>,
        session_key: &str,
    ) -> String {
        let handle = active_event_forwarders.write().await.remove(session_key);
        let Some(handle) = handle else {
            return String::new();
        };

        match handle.await {
            Ok(reasoning) => reasoning,
            Err(e) => {
                warn!(
                    session = %session_key,
                    error = %e,
                    "runner event forwarder task failed"
                );
                String::new()
            },
        }
    }

    async fn persist_partial_assistant_on_abort(
        &self,
        session_key: &str,
    ) -> Option<(Value, Option<u32>)> {
        let partial = self
            .active_partial_assistant
            .write()
            .await
            .remove(session_key)?;
        if !partial.has_visible_content() {
            return None;
        }

        let partial_message = partial.to_persisted_message();
        let partial_value = partial_message.to_value();
        let mut message_index = None;

        if let Err(e) = self.session_store.append(session_key, &partial_value).await {
            warn!(session = %session_key, error = %e, "failed to persist aborted partial assistant message");
            return Some((partial_value, None));
        }

        match self.session_store.count(session_key).await {
            Ok(count) => {
                self.session_metadata.touch(session_key, count).await;
                message_index = Some(count.saturating_sub(1));
            },
            Err(e) => {
                warn!(session = %session_key, error = %e, "failed to count session after persisting aborted partial assistant message");
            },
        }

        Some((partial_value, message_index))
    }

    /// Resolve a provider from session metadata, history, or first registered.
    async fn resolve_provider(
        &self,
        session_key: &str,
        history: &[Value],
    ) -> error::Result<Arc<dyn moltis_agents::model::LlmProvider>> {
        let reg = self.providers.read().await;
        let session_model = self
            .session_metadata
            .get(session_key)
            .await
            .and_then(|e| e.model.clone());
        let history_model = history
            .iter()
            .rev()
            .find_map(|m| m.get("model").and_then(|v| v.as_str()).map(String::from));
        let model_id = session_model.or(history_model);

        model_id
            .and_then(|id| reg.get(&id))
            .or_else(|| reg.first())
            .ok_or_else(|| error::Error::message("no LLM providers configured"))
    }

    /// Resolve the active session key for a connection.
    async fn session_key_for(&self, conn_id: Option<&str>) -> String {
        if let Some(cid) = conn_id
            && let Some(key) = self.state.active_session_key(cid).await
        {
            return key;
        }
        "main".to_string()
    }

    /// Resolve the effective session key for chat operations.
    ///
    /// Precedence is:
    /// 1. Internal `_session_key` overrides used by runtime-owned callers.
    /// 2. Public `sessionKey` / `session_key` request parameters.
    /// 3. Connection-scoped active session derived from `_conn_id`.
    /// 4. The default `"main"` session.
    async fn resolve_session_key_from_params(&self, params: &Value) -> String {
        if let Some(session_key) = params
            .get("_session_key")
            .and_then(|v| v.as_str())
            .filter(|v| !v.is_empty())
        {
            return session_key.to_string();
        }
        if let Some(session_key) = params
            .get("sessionKey")
            .or_else(|| params.get("session_key"))
            .and_then(|v| v.as_str())
            .filter(|v| !v.is_empty())
        {
            return session_key.to_string();
        }
        let conn_id = params.get("_conn_id").and_then(|v| v.as_str());
        self.session_key_for(conn_id).await
    }

    /// Resolve the project context prompt section for a session.
    async fn resolve_project_context(
        &self,
        session_key: &str,
        conn_id: Option<&str>,
    ) -> Option<String> {
        let project_id = if let Some(cid) = conn_id {
            self.state.active_project_id(cid).await
        } else {
            None
        };
        // Also check session metadata for project binding (async path).
        let project_id = match project_id {
            Some(pid) => Some(pid),
            None => self
                .session_metadata
                .get(session_key)
                .await
                .and_then(|e| e.project_id),
        };

        let pid = project_id?;
        let val = self
            .state
            .project_service()
            .get(serde_json::json!({"id": pid}))
            .await
            .ok()?;
        let dir = val.get("directory").and_then(|v| v.as_str())?;
        let files = match moltis_projects::context::load_context_files(Path::new(dir)) {
            Ok(f) => f,
            Err(e) => {
                warn!("failed to load project context: {e}");
                return None;
            },
        };
        let project: moltis_projects::Project = serde_json::from_value(val.clone()).ok()?;
        let worktree_dir = self
            .session_metadata
            .get(session_key)
            .await
            .and_then(|e| e.worktree_branch)
            .and_then(|_| {
                let wt_path = Path::new(dir).join(".moltis-worktrees").join(session_key);
                if wt_path.exists() {
                    Some(wt_path)
                } else {
                    None
                }
            });
        let ctx = moltis_projects::ProjectContext {
            project,
            context_files: files,
            worktree_dir,
        };
        Some(ctx.to_prompt_section())
    }
}
