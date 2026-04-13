//! Agent loop support: model flagging, shell commands, channel streaming, and compaction.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use async_trait::async_trait;

use {
    moltis_agents::tool_registry::AgentTool,
    moltis_config::schema::{AgentMemoryWriteMode, MemoryStyle, ToolMode},
    serde_json::Value,
    tokio::sync::{Mutex, RwLock, mpsc},
    tracing::{debug, info, warn},
};

use {
    moltis_agents::{runner::RunnerEvent, tool_registry::ToolRegistry},
    moltis_sessions::{PersistedMessage, store::SessionStore},
};

use crate::{
    channels::{deliver_channel_replies, send_tool_status_to_channels},
    chat_error::parse_chat_error,
    compaction_run, error,
    models::DisabledModelsStore,
    runtime::ChatRuntime,
    service::{build_tool_call_assistant_message, persist_tool_history_pair},
    types::*,
};

pub(crate) async fn mark_unsupported_model(
    state: &Arc<dyn ChatRuntime>,
    model_store: &Arc<RwLock<DisabledModelsStore>>,
    model_id: &str,
    provider_name: &str,
    error_obj: &Value,
) {
    if error_obj.get("type").and_then(|v| v.as_str()) != Some("unsupported_model") {
        return;
    }

    let detail = error_obj
        .get("detail")
        .and_then(|v| v.as_str())
        .unwrap_or("Model is not supported for this account/provider");
    let provider = error_obj
        .get("provider")
        .and_then(|v| v.as_str())
        .unwrap_or(provider_name);

    let mut store = model_store.write().await;
    if store.mark_unsupported(model_id, detail, Some(provider)) {
        let unsupported = store.unsupported_info(model_id).cloned();
        if let Err(err) = store.save() {
            warn!(
                model = model_id,
                provider = provider,
                error = %err,
                "failed to persist unsupported model flag"
            );
        } else {
            info!(
                model = model_id,
                provider = provider,
                "flagged model as unsupported"
            );
        }
        drop(store);
        broadcast(
            state,
            "models.updated",
            serde_json::json!({
                "modelId": model_id,
                "unsupported": true,
                "unsupportedReason": unsupported.as_ref().map(|u| u.detail.as_str()).unwrap_or(detail),
                "unsupportedProvider": unsupported
                    .as_ref()
                    .and_then(|u| u.provider.as_deref())
                    .unwrap_or(provider),
                "unsupportedUpdatedAt": unsupported.map(|u| u.updated_at_ms).unwrap_or_else(now_ms),
            }),
            BroadcastOpts::default(),
        )
        .await;
    }
}

pub(crate) async fn clear_unsupported_model(
    state: &Arc<dyn ChatRuntime>,
    model_store: &Arc<RwLock<DisabledModelsStore>>,
    model_id: &str,
) {
    let mut store = model_store.write().await;
    if store.clear_unsupported(model_id) {
        if let Err(err) = store.save() {
            warn!(
                model = model_id,
                error = %err,
                "failed to persist unsupported model clear"
            );
        } else {
            info!(model = model_id, "cleared unsupported model flag");
        }
        drop(store);
        broadcast(
            state,
            "models.updated",
            serde_json::json!({
                "modelId": model_id,
                "unsupported": false,
            }),
            BroadcastOpts::default(),
        )
        .await;
    }
}

pub(crate) fn ordered_runner_event_callback() -> (
    Box<dyn Fn(RunnerEvent) + Send + Sync>,
    mpsc::UnboundedReceiver<RunnerEvent>,
) {
    let (tx, rx) = mpsc::unbounded_channel::<RunnerEvent>();
    let callback: Box<dyn Fn(RunnerEvent) + Send + Sync> = Box::new(move |event| {
        if tx.send(event).is_err() {
            debug!("runner event dropped because event processor is closed");
        }
    });
    (callback, rx)
}

const CHANNEL_STREAM_BUFFER_SIZE: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ChannelReplyTargetKey {
    channel_type: moltis_channels::ChannelType,
    account_id: String,
    chat_id: String,
    message_id: Option<String>,
    thread_id: Option<String>,
}

impl From<&moltis_channels::ChannelReplyTarget> for ChannelReplyTargetKey {
    fn from(target: &moltis_channels::ChannelReplyTarget) -> Self {
        Self {
            channel_type: target.channel_type,
            account_id: target.account_id.clone(),
            chat_id: target.chat_id.clone(),
            message_id: target.message_id.clone(),
            thread_id: target.thread_id.clone(),
        }
    }
}

struct ChannelStreamWorker {
    sender: moltis_channels::StreamSender,
}

/// Fan out model deltas to channel stream workers (Telegram/Discord edit-in-place).
///
/// Workers are started eagerly so channel typing indicators remain active
/// during long-running tool execution before the first text delta arrives.
/// Stream-dedup only applies after at least one delta has been sent.
pub(crate) struct ChannelStreamDispatcher {
    outbound: Arc<dyn moltis_channels::plugin::ChannelStreamOutbound>,
    targets: Vec<moltis_channels::ChannelReplyTarget>,
    workers: Vec<ChannelStreamWorker>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    completed: Arc<Mutex<HashSet<ChannelReplyTargetKey>>>,
    started: bool,
    sent_delta: bool,
}

impl ChannelStreamDispatcher {
    pub(crate) async fn for_session(
        state: &Arc<dyn ChatRuntime>,
        session_key: &str,
    ) -> Option<Self> {
        let outbound = state.channel_stream_outbound()?;
        let targets: Vec<moltis_channels::ChannelReplyTarget> = state
            .peek_channel_replies(session_key)
            .await
            .into_iter()
            .collect();
        if targets.is_empty() {
            return None;
        }
        let mut dispatcher = Self {
            outbound,
            targets,
            workers: Vec::new(),
            tasks: Vec::new(),
            completed: Arc::new(Mutex::new(HashSet::new())),
            started: false,
            sent_delta: false,
        };
        dispatcher.ensure_started().await;
        Some(dispatcher)
    }

    async fn ensure_started(&mut self) {
        if self.started {
            return;
        }
        self.started = true;

        for target in self.targets.iter().cloned() {
            if !self.outbound.is_stream_enabled(&target.account_id).await {
                debug!(
                    account_id = target.account_id.as_str(),
                    chat_id = target.chat_id.as_str(),
                    "channel streaming disabled for target account"
                );
                continue;
            }

            let key = ChannelReplyTargetKey::from(&target);
            let (tx, rx) = mpsc::channel(CHANNEL_STREAM_BUFFER_SIZE);
            let outbound = Arc::clone(&self.outbound);
            let completed = Arc::clone(&self.completed);
            let account_id = target.account_id.clone();
            let to = target.outbound_to().into_owned();
            let reply_to = target.message_id.clone();
            let key_for_insert = key.clone();
            let account_for_log = account_id.clone();
            let chat_for_log = target.chat_id.clone();
            let thread_for_log = target.thread_id.clone();

            self.workers.push(ChannelStreamWorker { sender: tx });
            self.tasks.push(tokio::spawn(async move {
                match outbound
                    .send_stream(&account_id, &to, reply_to.as_deref(), rx)
                    .await
                {
                    Ok(()) => {
                        completed.lock().await.insert(key_for_insert);
                    },
                    Err(e) => {
                        warn!(
                            account_id = account_for_log,
                            chat_id = chat_for_log,
                            thread_id = thread_for_log.as_deref().unwrap_or("-"),
                            "channel stream outbound failed: {e}"
                        );
                    },
                }
            }));
        }
    }

    pub(crate) async fn send_delta(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        self.sent_delta = true;
        self.ensure_started().await;
        let event = moltis_channels::StreamEvent::Delta(delta.to_string());
        for worker in &self.workers {
            if worker.sender.send(event.clone()).await.is_err() {
                debug!("channel stream delta dropped: worker closed");
            }
        }
    }

    pub(crate) async fn finish(&mut self) {
        self.send_terminal(moltis_channels::StreamEvent::Done).await;
        self.join_workers().await;
    }

    async fn send_terminal(&mut self, event: moltis_channels::StreamEvent) {
        if self.workers.is_empty() {
            return;
        }
        let workers = std::mem::take(&mut self.workers);
        for worker in &workers {
            if worker.sender.send(event.clone()).await.is_err() {
                debug!("channel stream terminal event dropped: worker closed");
            }
        }
    }

    async fn join_workers(&mut self) {
        let tasks = std::mem::take(&mut self.tasks);
        for task in tasks {
            if let Err(e) = task.await {
                warn!(error = %e, "channel stream worker task join failed");
            }
        }
    }

    pub(crate) async fn completed_target_keys(&self) -> HashSet<ChannelReplyTargetKey> {
        if !self.sent_delta {
            return HashSet::new();
        }
        self.completed.lock().await.clone()
    }
}

pub(crate) async fn run_explicit_shell_command(
    state: &Arc<dyn ChatRuntime>,
    run_id: &str,
    tool_registry: &Arc<RwLock<ToolRegistry>>,
    session_store: &Arc<SessionStore>,
    terminal_runs: &Arc<RwLock<HashSet<String>>>,
    session_key: &str,
    command: &str,
    user_message_index: usize,
    accept_language: Option<String>,
    conn_id: Option<String>,
    client_seq: Option<u64>,
) -> AssistantTurnOutput {
    let started = Instant::now();
    let tool_call_id = format!("sh_{}", uuid::Uuid::new_v4().simple());
    let tool_args = serde_json::json!({ "command": command });

    send_tool_status_to_channels(state, session_key, "exec", &tool_args).await;

    broadcast(
        state,
        "chat",
        serde_json::json!({
            "runId": run_id,
            "sessionKey": session_key,
            "state": "tool_call_start",
            "toolCallId": tool_call_id,
            "toolName": "exec",
            "arguments": tool_args,
            "seq": client_seq,
        }),
        BroadcastOpts::default(),
    )
    .await;

    let mut exec_params = serde_json::json!({
        "command": command,
        "_session_key": session_key,
    });
    if let Some(lang) = accept_language.as_deref() {
        exec_params["_accept_language"] = serde_json::json!(lang);
    }
    if let Some(cid) = conn_id.as_deref() {
        exec_params["_conn_id"] = serde_json::json!(cid);
    }

    let exec_tool = {
        let registry = tool_registry.read().await;
        registry.get("exec")
    };

    let exec_result = match exec_tool {
        Some(tool) => tool.execute(exec_params).await,
        None => Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "exec tool is not registered",
        )
        .into()),
    };

    let has_channel_targets = !state.peek_channel_replies(session_key).await.is_empty();
    let mut final_text = String::new();

    match exec_result {
        Ok(result) => {
            let capped = capped_tool_result_payload(&result, 10_000);
            let assistant_tool_call_msg = build_tool_call_assistant_message(
                tool_call_id.clone(),
                "exec",
                Some(tool_args.clone()),
                client_seq,
                Some(run_id),
            );
            let tool_result_msg = PersistedMessage::tool_result(
                tool_call_id.clone(),
                "exec",
                Some(serde_json::json!({ "command": command })),
                true,
                Some(capped.clone()),
                None,
            );
            persist_tool_history_pair(
                session_store,
                session_key,
                assistant_tool_call_msg,
                tool_result_msg,
                "failed to persist direct /sh assistant tool call",
                "failed to persist direct /sh tool result",
            )
            .await;

            broadcast(
                state,
                "chat",
                serde_json::json!({
                    "runId": run_id,
                    "sessionKey": session_key,
                    "state": "tool_call_end",
                    "toolCallId": tool_call_id,
                    "toolName": "exec",
                    "success": true,
                    "result": capped,
                    "seq": client_seq,
                }),
                BroadcastOpts::default(),
            )
            .await;

            if has_channel_targets {
                final_text = shell_reply_text_from_exec_result(&result);
                if final_text.is_empty() {
                    final_text = "Command completed.".to_string();
                }
            }
        },
        Err(err) => {
            let error_text = err.to_string();
            let parsed_error = parse_chat_error(&error_text, None);
            let assistant_tool_call_msg = build_tool_call_assistant_message(
                tool_call_id.clone(),
                "exec",
                Some(tool_args.clone()),
                client_seq,
                Some(run_id),
            );
            let tool_result_msg = PersistedMessage::tool_result(
                tool_call_id.clone(),
                "exec",
                Some(serde_json::json!({ "command": command })),
                false,
                None,
                Some(error_text.clone()),
            );
            persist_tool_history_pair(
                session_store,
                session_key,
                assistant_tool_call_msg,
                tool_result_msg,
                "failed to persist direct /sh assistant tool call",
                "failed to persist direct /sh tool error",
            )
            .await;

            broadcast(
                state,
                "chat",
                serde_json::json!({
                    "runId": run_id,
                    "sessionKey": session_key,
                    "state": "tool_call_end",
                    "toolCallId": tool_call_id,
                    "toolName": "exec",
                    "success": false,
                    "error": parsed_error,
                    "seq": client_seq,
                }),
                BroadcastOpts::default(),
            )
            .await;

            if has_channel_targets {
                final_text = error_text;
            }
        },
    }

    if !final_text.trim().is_empty() {
        let streamed_target_keys = HashSet::new();
        deliver_channel_replies(
            state,
            session_key,
            &final_text,
            ReplyMedium::Text,
            &streamed_target_keys,
        )
        .await;
    }

    let final_payload = ChatFinalBroadcast {
        run_id: run_id.to_string(),
        session_key: session_key.to_string(),
        state: "final",
        text: final_text.clone(),
        model: String::new(),
        provider: String::new(),
        input_tokens: 0,
        output_tokens: 0,
        duration_ms: started.elapsed().as_millis() as u64,
        request_input_tokens: Some(0),
        request_output_tokens: Some(0),
        message_index: user_message_index + 3, /* +1 tool call assistant, +1 tool result, +1 final assistant */
        reply_medium: ReplyMedium::Text,
        iterations: Some(1),
        tool_calls_made: Some(1),
        audio: None,
        audio_warning: None,
        reasoning: None,
        seq: client_seq,
    };
    #[allow(clippy::unwrap_used)] // serializing known-valid struct
    let payload = serde_json::to_value(&final_payload).unwrap();
    terminal_runs.write().await.insert(run_id.to_string());
    broadcast(state, "chat", payload, BroadcastOpts::default()).await;

    AssistantTurnOutput {
        text: final_text,
        input_tokens: 0,
        output_tokens: 0,
        duration_ms: started.elapsed().as_millis() as u64,
        request_input_tokens: 0,
        request_output_tokens: 0,
        audio_path: None,
        reasoning: None,
        llm_api_response: None,
    }
}

const MAX_AGENT_MEMORY_WRITE_BYTES: usize = 50 * 1024;
const MEMORY_SEARCH_FETCH_MULTIPLIER: usize = 8;
const MEMORY_SEARCH_MIN_FETCH: usize = 25;

fn is_valid_agent_memory_leaf_name(name: &str) -> bool {
    if name.is_empty() || name.contains('/') || !name.ends_with(".md") {
        return false;
    }
    if name.chars().any(char::is_whitespace) {
        return false;
    }
    let stem = &name[..name.len() - 3];
    !(stem.is_empty() || stem.starts_with('.'))
}

fn resolve_agent_memory_target_path(agent_id: &str, file: &str) -> anyhow::Result<PathBuf> {
    let trimmed = file.trim();
    if trimmed.is_empty() {
        anyhow::bail!("memory path cannot be empty");
    }

    let workspace = moltis_config::agent_workspace_dir(agent_id);
    if trimmed == "MEMORY.md" || trimmed == "memory.md" {
        return Ok(workspace.join("MEMORY.md"));
    }

    let Some(name) = trimmed.strip_prefix("memory/") else {
        anyhow::bail!(
            "invalid memory path '{trimmed}': allowed targets are MEMORY.md, memory.md, or memory/<name>.md"
        );
    };
    if !is_valid_agent_memory_leaf_name(name) {
        anyhow::bail!(
            "invalid memory path '{trimmed}': allowed targets are MEMORY.md, memory.md, or memory/<name>.md"
        );
    }
    Ok(workspace.join("memory").join(name))
}

fn is_path_in_agent_memory_scope(path: &Path, agent_id: &str) -> bool {
    let workspace = moltis_config::agent_workspace_dir(agent_id);
    let workspace_memory_dir = workspace.join("memory");
    if path == workspace.join("MEMORY.md")
        || path == workspace.join("memory.md")
        || path.starts_with(&workspace_memory_dir)
    {
        return true;
    }

    if agent_id != "main" {
        return false;
    }

    let data_dir = moltis_config::data_dir();
    let root_memory_dir = data_dir.join("memory");
    path == data_dir.join("MEMORY.md")
        || path == data_dir.join("memory.md")
        || path.starts_with(&root_memory_dir)
}

struct AgentScopedMemoryWriter {
    manager: moltis_memory::runtime::DynMemoryRuntime,
    agent_id: String,
    write_mode: AgentMemoryWriteMode,
    checkpoints: moltis_tools::checkpoints::CheckpointManager,
}

impl AgentScopedMemoryWriter {
    fn new(
        manager: moltis_memory::runtime::DynMemoryRuntime,
        agent_id: String,
        write_mode: AgentMemoryWriteMode,
    ) -> Self {
        Self {
            manager,
            agent_id,
            write_mode,
            checkpoints: moltis_tools::checkpoints::CheckpointManager::new(
                moltis_config::data_dir(),
            ),
        }
    }
}

#[async_trait]
impl moltis_agents::memory_writer::MemoryWriter for AgentScopedMemoryWriter {
    async fn write_memory(
        &self,
        file: &str,
        content: &str,
        append: bool,
    ) -> anyhow::Result<moltis_agents::memory_writer::MemoryWriteResult> {
        if content.len() > MAX_AGENT_MEMORY_WRITE_BYTES {
            anyhow::bail!(
                "content exceeds maximum size of {} bytes ({} bytes provided)",
                MAX_AGENT_MEMORY_WRITE_BYTES,
                content.len()
            );
        }

        validate_agent_memory_target_for_mode(self.write_mode, file)?;
        let path = resolve_agent_memory_target_path(&self.agent_id, file)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let checkpoint = self
            .checkpoints
            .checkpoint_path(&path, "memory_write")
            .await?;
        let final_content = if append && tokio::fs::try_exists(&path).await? {
            let existing = tokio::fs::read_to_string(&path).await?;
            format!("{existing}\n\n{content}")
        } else {
            content.to_string()
        };
        let bytes_written = final_content.len();

        tokio::fs::write(&path, &final_content).await?;
        if let Err(error) = self.manager.sync_path(&path).await {
            warn!(path = %path.display(), %error, "agent memory write re-index failed");
        }

        Ok(moltis_agents::memory_writer::MemoryWriteResult {
            location: path.to_string_lossy().into_owned(),
            bytes_written,
            checkpoint_id: Some(checkpoint.id),
        })
    }
}

struct AgentScopedMemorySearchTool {
    manager: moltis_memory::runtime::DynMemoryRuntime,
    agent_id: String,
}

impl AgentScopedMemorySearchTool {
    fn new(manager: moltis_memory::runtime::DynMemoryRuntime, agent_id: String) -> Self {
        Self { manager, agent_id }
    }
}

#[async_trait]
impl AgentTool for AgentScopedMemorySearchTool {
    fn name(&self) -> &str {
        "memory_search"
    }

    fn description(&self) -> &str {
        "Search agent memory using hybrid vector + keyword search. Returns relevant chunks from daily logs and long-term memory files."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "The search query"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of results to return",
                    "default": 5
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, params: Value) -> anyhow::Result<Value> {
        let query = params
            .get("query")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("missing 'query' parameter"))?;
        let requested_limit = params.get("limit").and_then(Value::as_u64).unwrap_or(5) as usize;
        let limit = requested_limit.clamp(1, 50);
        let search_limit = limit
            .saturating_mul(MEMORY_SEARCH_FETCH_MULTIPLIER)
            .max(MEMORY_SEARCH_MIN_FETCH)
            .max(limit);

        let mut results: Vec<moltis_memory::search::SearchResult> = self
            .manager
            .search(query, search_limit)
            .await?
            .into_iter()
            .filter(|result| is_path_in_agent_memory_scope(Path::new(&result.path), &self.agent_id))
            .collect();
        results.truncate(limit);

        let include_citations = moltis_memory::search::SearchResult::should_include_citations(
            &results,
            self.manager.citation_mode(),
        );
        let items: Vec<Value> = results
            .iter()
            .map(|result| {
                let text = if include_citations {
                    result.text_with_citation()
                } else {
                    result.text.clone()
                };
                serde_json::json!({
                    "chunk_id": result.chunk_id,
                    "path": result.path,
                    "source": result.source,
                    "start_line": result.start_line,
                    "end_line": result.end_line,
                    "score": result.score,
                    "text": text,
                    "citation": format!("{}#{}", result.path, result.start_line),
                })
            })
            .collect();

        Ok(serde_json::json!({
            "results": items,
            "citations_enabled": include_citations
        }))
    }
}

struct AgentScopedMemoryGetTool {
    manager: moltis_memory::runtime::DynMemoryRuntime,
    agent_id: String,
}

impl AgentScopedMemoryGetTool {
    fn new(manager: moltis_memory::runtime::DynMemoryRuntime, agent_id: String) -> Self {
        Self { manager, agent_id }
    }
}

#[async_trait]
impl AgentTool for AgentScopedMemoryGetTool {
    fn name(&self) -> &str {
        "memory_get"
    }

    fn description(&self) -> &str {
        "Retrieve a specific memory chunk by its ID. Use this to get the full text of a chunk found via memory_search."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "chunk_id": {
                    "type": "string",
                    "description": "The chunk ID to retrieve"
                }
            },
            "required": ["chunk_id"]
        })
    }

    async fn execute(&self, params: Value) -> anyhow::Result<Value> {
        let chunk_id = params
            .get("chunk_id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("missing 'chunk_id' parameter"))?;

        match self.manager.get_chunk(chunk_id).await? {
            Some(chunk)
                if is_path_in_agent_memory_scope(Path::new(&chunk.path), &self.agent_id) =>
            {
                Ok(serde_json::json!({
                    "chunk_id": chunk.id,
                    "path": chunk.path,
                    "source": chunk.source,
                    "start_line": chunk.start_line,
                    "end_line": chunk.end_line,
                    "text": chunk.text,
                }))
            },
            _ => Ok(serde_json::json!({
                "error": "chunk not found",
                "chunk_id": chunk_id,
            })),
        }
    }
}

struct AgentScopedMemorySaveTool {
    writer: AgentScopedMemoryWriter,
    write_mode: AgentMemoryWriteMode,
}

impl AgentScopedMemorySaveTool {
    fn new(
        manager: moltis_memory::runtime::DynMemoryRuntime,
        agent_id: String,
        write_mode: AgentMemoryWriteMode,
    ) -> Self {
        Self {
            writer: AgentScopedMemoryWriter::new(manager, agent_id, write_mode),
            write_mode,
        }
    }
}

#[async_trait]
impl AgentTool for AgentScopedMemorySaveTool {
    fn name(&self) -> &str {
        "memory_save"
    }

    fn description(&self) -> &str {
        "Save content to long-term memory. Writes to MEMORY.md or memory/<name>.md. Content persists across sessions and is searchable via memory_search."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "The content to save to memory"
                },
                "file": {
                    "type": "string",
                    "description": "Target file: MEMORY.md, memory.md, or memory/<name>.md",
                    "default": "MEMORY.md"
                },
                "append": {
                    "type": "boolean",
                    "description": "Append to existing file (true) or overwrite (false)",
                    "default": true
                }
            },
            "required": ["content"]
        })
    }

    async fn execute(&self, params: Value) -> anyhow::Result<Value> {
        let content = params
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("missing 'content' parameter"))?;
        let file = params
            .get("file")
            .and_then(Value::as_str)
            .unwrap_or_else(|| default_agent_memory_file_for_mode(self.write_mode));
        let append = params
            .get("append")
            .and_then(Value::as_bool)
            .unwrap_or(true);

        use moltis_agents::memory_writer::MemoryWriter;
        let result = self.writer.write_memory(file, content, append).await?;

        Ok(serde_json::json!({
            "saved": true,
            "path": file,
            "bytes_written": result.bytes_written,
            "checkpointId": result.checkpoint_id,
        }))
    }
}

fn install_agent_scoped_memory_tools(
    registry: &mut ToolRegistry,
    manager: &moltis_memory::runtime::DynMemoryRuntime,
    agent_id: &str,
    style: MemoryStyle,
    write_mode: AgentMemoryWriteMode,
) {
    let had_search = registry.unregister("memory_search");
    let had_get = registry.unregister("memory_get");
    let had_save = registry.unregister("memory_save");

    if !memory_style_allows_tools(style) {
        return;
    }

    let agent_id_owned = agent_id.to_string();
    if had_search {
        registry.register(Box::new(AgentScopedMemorySearchTool::new(
            Arc::clone(manager),
            agent_id_owned.clone(),
        )));
    }
    if had_get {
        registry.register(Box::new(AgentScopedMemoryGetTool::new(
            Arc::clone(manager),
            agent_id_owned.clone(),
        )));
    }
    if had_save && memory_write_mode_allows_save(write_mode) {
        registry.register(Box::new(AgentScopedMemorySaveTool::new(
            Arc::clone(manager),
            agent_id_owned,
            write_mode,
        )));
    }
}

/// Resolve the effective tool mode for a provider.
///
/// Combines the provider's `tool_mode()` override with its `supports_tools()`
/// capability to determine how tools should be dispatched:
/// - `Native` — provider handles tool schemas via API (OpenAI function calling, etc.)
/// - `Text` — tools are described in the prompt; the runner parses tool calls from text
/// - `Off` — no tools at all
pub(crate) fn effective_tool_mode(provider: &dyn moltis_agents::model::LlmProvider) -> ToolMode {
    match provider.tool_mode() {
        Some(ToolMode::Native) => ToolMode::Native,
        Some(ToolMode::Text) => ToolMode::Text,
        Some(ToolMode::Off) => ToolMode::Off,
        Some(ToolMode::Auto) | None => {
            if provider.supports_tools() {
                ToolMode::Native
            } else {
                ToolMode::Text
            }
        },
    }
}

pub(crate) async fn compact_session(
    store: &Arc<SessionStore>,
    session_key: &str,
    config: &moltis_config::CompactionConfig,
    provider: Option<&dyn moltis_agents::model::LlmProvider>,
) -> error::Result<compaction_run::CompactionOutcome> {
    let history = store
        .read(session_key)
        .await
        .map_err(|source| error::Error::external("failed to read session history", source))?;

    let mut outcome = compaction_run::run_compaction(&history, config, provider)
        .await
        .map_err(|e| error::Error::message(e.to_string()))?;

    // Enforce summary budget discipline on the compacted history.
    outcome.history = compress_summary_in_history(outcome.history);

    store
        .replace_history(session_key, outcome.history.clone())
        .await
        .map_err(|source| error::Error::external("failed to replace compacted history", source))?;

    Ok(outcome)
}
