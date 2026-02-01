use std::{collections::HashMap, sync::Arc};

use {
    async_trait::async_trait,
    serde_json::Value,
    tokio::{sync::RwLock, task::AbortHandle},
    tokio_stream::StreamExt,
    tracing::{debug, info, warn},
};

use {
    moltis_agents::{
        model::StreamEvent,
        prompt::build_system_prompt_with_session,
        providers::ProviderRegistry,
        runner::{RunnerEvent, run_agent_loop_with_context},
        tool_registry::ToolRegistry,
    },
    moltis_sessions::{metadata::SqliteSessionMetadata, store::SessionStore},
    moltis_skills::discover::SkillDiscoverer,
};

use crate::{
    broadcast::{BroadcastOpts, broadcast},
    chat_error::parse_chat_error,
    services::{ChatService, ModelService, ServiceResult},
    state::GatewayState,
};

// ── LiveModelService ────────────────────────────────────────────────────────

pub struct LiveModelService {
    providers: Arc<RwLock<ProviderRegistry>>,
}

impl LiveModelService {
    pub fn new(providers: Arc<RwLock<ProviderRegistry>>) -> Self {
        Self { providers }
    }
}

#[async_trait]
impl ModelService for LiveModelService {
    async fn list(&self) -> ServiceResult {
        let reg = self.providers.read().await;
        let models: Vec<_> = reg
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
    providers: Arc<RwLock<ProviderRegistry>>,
    state: Arc<GatewayState>,
    active_runs: Arc<RwLock<HashMap<String, AbortHandle>>>,
    tool_registry: Arc<ToolRegistry>,
    session_store: Arc<SessionStore>,
    session_metadata: Arc<SqliteSessionMetadata>,
}

impl LiveChatService {
    pub fn new(
        providers: Arc<RwLock<ProviderRegistry>>,
        state: Arc<GatewayState>,
        session_store: Arc<SessionStore>,
        session_metadata: Arc<SqliteSessionMetadata>,
    ) -> Self {
        Self {
            providers,
            state,
            active_runs: Arc::new(RwLock::new(HashMap::new())),
            tool_registry: Arc::new(ToolRegistry::new()),
            session_store,
            session_metadata,
        }
    }

    pub fn with_tools(mut self, registry: ToolRegistry) -> Self {
        self.tool_registry = Arc::new(registry);
        self
    }

    fn has_tools(&self) -> bool {
        !self.tool_registry.list_schemas().is_empty()
    }

    /// Resolve the active session key for a connection.
    async fn session_key_for(&self, conn_id: Option<&str>) -> String {
        if let Some(cid) = conn_id {
            let sessions = self.state.active_sessions.read().await;
            if let Some(key) = sessions.get(cid) {
                return key.clone();
            }
        }
        "main".to_string()
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

        let conn_id = params
            .get("_conn_id")
            .and_then(|v| v.as_str())
            .map(String::from);
        let explicit_model = params.get("model").and_then(|v| v.as_str());
        // Use streaming-only mode if explicitly requested or if no tools are registered.
        let stream_only = params
            .get("stream_only")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
            || !self.has_tools();

        // Resolve session key: explicit override (used by cron callbacks) or
        // connection-scoped lookup.
        let session_key = if let Some(sk) = params.get("_session_key").and_then(|v| v.as_str()) {
            sk.to_string()
        } else {
            self.session_key_for(conn_id.as_deref()).await
        };

        // Resolve model: explicit param → session metadata → first registered.
        let session_model = if explicit_model.is_none() {
            self.session_metadata
                .get(&session_key)
                .await
                .and_then(|e| e.model)
        } else {
            None
        };
        let model_id = explicit_model.or(session_model.as_deref());

        let provider = {
            let reg = self.providers.read().await;
            if let Some(id) = model_id {
                reg.get(id).ok_or_else(|| {
                    let available: Vec<_> =
                        reg.list_models().iter().map(|m| m.id.clone()).collect();
                    format!("model '{}' not found. available: {:?}", id, available)
                })?
            } else if !stream_only {
                reg.first_with_tools()
                    .ok_or_else(|| "no LLM providers configured".to_string())?
            } else {
                reg.first()
                    .ok_or_else(|| "no LLM providers configured".to_string())?
            }
        };

        // Resolve project context for this connection's active project.
        let project_context = {
            let project_id = if let Some(cid) = conn_id.as_deref() {
                let projects = self.state.active_projects.read().await;
                projects.get(cid).cloned()
            } else {
                None
            };
            // Also check session metadata for project binding.
            let project_id = if project_id.is_some() {
                project_id
            } else {
                self.session_metadata
                    .get(&session_key)
                    .await
                    .and_then(|e| e.project_id)
            };
            if let Some(pid) = project_id {
                match self
                    .state
                    .services
                    .project
                    .get(serde_json::json!({"id": pid}))
                    .await
                {
                    Ok(val) => {
                        if let Some(dir) = val.get("directory").and_then(|v| v.as_str()) {
                            match moltis_projects::context::load_context_files(
                                std::path::Path::new(dir),
                            ) {
                                Ok(files) => {
                                    let project: Option<moltis_projects::Project> =
                                        serde_json::from_value(val.clone()).ok();
                                    if let Some(p) = project {
                                        // Resolve worktree dir from session metadata.
                                        let worktree_dir = self
                                            .session_metadata
                                            .get(&session_key)
                                            .await
                                            .and_then(|e| e.worktree_branch)
                                            .and_then(|_| {
                                                let wt_path = std::path::Path::new(dir)
                                                    .join(".moltis-worktrees")
                                                    .join(&session_key);
                                                if wt_path.exists() {
                                                    Some(wt_path)
                                                } else {
                                                    None
                                                }
                                            });
                                        let ctx = moltis_projects::ProjectContext {
                                            project: p,
                                            context_files: files,
                                            worktree_dir,
                                        };
                                        Some(ctx.to_prompt_section())
                                    } else {
                                        None
                                    }
                                },
                                Err(e) => {
                                    warn!("failed to load project context: {e}");
                                    None
                                },
                            }
                        } else {
                            None
                        }
                    },
                    Err(_) => None,
                }
            } else {
                None
            }
        };

        // Persist the user message (with optional channel metadata for UI display).
        let channel_meta = params.get("channel").cloned();
        let mut user_msg = serde_json::json!({"role": "user", "content": &text});
        if let Some(ch) = &channel_meta {
            user_msg["channel"] = ch.clone();
        }
        if let Err(e) = self.session_store.append(&session_key, &user_msg).await {
            warn!("failed to persist user message: {e}");
        }

        // Load conversation history excluding the user message we just appended
        // (both run_streaming and run_agent_loop add the current user message themselves).
        let mut history = self
            .session_store
            .read(&session_key)
            .await
            .unwrap_or_default();
        // Pop the last message (the one we just appended).
        if !history.is_empty() {
            history.pop();
        }

        // Update metadata.
        let _ = self.session_metadata.upsert(&session_key, None).await;
        self.session_metadata
            .touch(&session_key, history.len() as u32)
            .await;

        // If this is a web UI message on a channel-bound session, echo the
        // user message to the channel and register a reply target so the LLM
        // response is also delivered there.
        let is_web_message = conn_id.is_some()
            && params.get("_session_key").is_none()
            && params.get("channel").is_none();
        if is_web_message
            && let Some(entry) = self.session_metadata.get(&session_key).await
            && let Some(ref binding_json) = entry.channel_binding
            && let Ok(target) =
                serde_json::from_str::<moltis_channels::ChannelReplyTarget>(binding_json)
        {
            // Only echo to channel if this is the active session for this chat.
            let is_active = self
                .session_metadata
                .get_active_session(&target.channel_type, &target.account_id, &target.chat_id)
                .await
                .map(|k| k == session_key)
                .unwrap_or(true); // no override → deterministic key is active

            if is_active {
                // Push reply target so deliver_channel_replies sends the LLM
                // response to the channel.
                self.state
                    .push_channel_reply(&session_key, target.clone())
                    .await;

                // Echo user message to the channel prefixed with [Web].
                if let Some(outbound) = self.state.services.channel_outbound_arc() {
                    let echo_text = format!("[Web] {text}");
                    let account_id = target.account_id.clone();
                    let chat_id = target.chat_id.clone();
                    tokio::spawn(async move {
                        if let Err(e) = outbound.send_text(&account_id, &chat_id, &echo_text).await
                        {
                            warn!("failed to echo web message to channel: {e}");
                        }
                    });
                }
            }
        }

        // Discover enabled skills/plugins for prompt injection.
        let cwd = std::env::current_dir().unwrap_or_default();
        let search_paths = moltis_skills::discover::FsSkillDiscoverer::default_paths(&cwd);
        let discoverer = moltis_skills::discover::FsSkillDiscoverer::new(search_paths);
        let discovered_skills = match discoverer.discover().await {
            Ok(s) => s,
            Err(e) => {
                warn!("failed to discover skills: {e}");
                Vec::new()
            },
        };

        let run_id = uuid::Uuid::new_v4().to_string();
        let state = Arc::clone(&self.state);
        let active_runs = Arc::clone(&self.active_runs);
        let run_id_clone = run_id.clone();
        let tool_registry = Arc::clone(&self.tool_registry);

        // Warn if tool mode is active but the provider doesn't support tools.
        if !stream_only && !provider.supports_tools() {
            warn!(
                provider = provider.name(),
                model = provider.id(),
                "selected provider does not support tool calling; \
                 LLM will not be able to use tools"
            );
        }

        info!(
            run_id = %run_id,
            user_message = %text,
            model = provider.id(),
            stream_only,
            session = %session_key,
            "chat.send"
        );

        // Capture user message index (0-based) so we can include assistant
        // message index in the "final" broadcast for client-side deduplication.
        let user_message_index = history.len(); // user msg is at this index in the JSONL

        let provider_name = provider.name().to_string();
        let model_id = provider.id().to_string();
        let session_store = Arc::clone(&self.session_store);
        let session_metadata = Arc::clone(&self.session_metadata);
        let session_key_clone = session_key.clone();
        // Compute session context stats for the system prompt.
        let session_stats = {
            let msg_count = history.len() + 1; // +1 for the current user message
            let mut total_input: u64 = 0;
            let mut total_output: u64 = 0;
            for msg in &history {
                if let Some(t) = msg.get("inputTokens").and_then(|v| v.as_u64()) {
                    total_input += t;
                }
                if let Some(t) = msg.get("outputTokens").and_then(|v| v.as_u64()) {
                    total_output += t;
                }
            }
            let total_tokens = total_input + total_output;
            format!(
                "Session \"{session_key}\": {msg_count} messages, {total_tokens} tokens used ({total_input} input / {total_output} output)."
            )
        };

        // Auto-compact: if conversation input tokens exceed 95% of context window, compact first.
        let context_window = provider.context_window() as u64;
        let total_input: u64 = history
            .iter()
            .filter_map(|m| m.get("inputTokens").and_then(|v| v.as_u64()))
            .sum();
        if total_input >= context_window * 95 / 100 {
            let pre_compact_msg_count = history.len();
            let total_output: u64 = history
                .iter()
                .filter_map(|m| m.get("outputTokens").and_then(|v| v.as_u64()))
                .sum();
            let pre_compact_total = total_input + total_output;

            info!(
                session = %session_key,
                total_input,
                context_window,
                "auto-compact triggered (95% threshold reached)"
            );
            broadcast(
                &self.state,
                "chat",
                serde_json::json!({
                    "sessionKey": session_key,
                    "state": "auto_compact",
                    "phase": "start",
                    "messageCount": pre_compact_msg_count,
                    "totalTokens": pre_compact_total,
                    "inputTokens": total_input,
                    "outputTokens": total_output,
                    "contextWindow": context_window,
                }),
                BroadcastOpts::default(),
            )
            .await;

            let compact_params = serde_json::json!({ "_conn_id": conn_id });
            match self.compact(compact_params).await {
                Ok(_) => {
                    // Reload history after compaction.
                    history = self
                        .session_store
                        .read(&session_key)
                        .await
                        .unwrap_or_default();
                    broadcast(
                        &self.state,
                        "chat",
                        serde_json::json!({
                            "sessionKey": session_key,
                            "state": "auto_compact",
                            "phase": "done",
                            "messageCount": pre_compact_msg_count,
                            "totalTokens": pre_compact_total,
                            "contextWindow": context_window,
                        }),
                        BroadcastOpts::default(),
                    )
                    .await;
                },
                Err(e) => {
                    warn!(session = %session_key, error = %e, "auto-compact failed, proceeding with full history");
                    broadcast(
                        &self.state,
                        "chat",
                        serde_json::json!({
                            "sessionKey": session_key,
                            "state": "auto_compact",
                            "phase": "error",
                            "error": e.to_string(),
                        }),
                        BroadcastOpts::default(),
                    )
                    .await;
                },
            }
        }

        let handle = tokio::spawn(async move {
            let ctx_ref = project_context.as_deref();
            let stats_ref = Some(session_stats.as_str());
            let assistant_text = if stream_only {
                run_streaming(
                    &state,
                    &run_id_clone,
                    provider,
                    &text,
                    &provider_name,
                    &history,
                    &session_key_clone,
                    ctx_ref,
                    stats_ref,
                    user_message_index,
                    &discovered_skills,
                )
                .await
            } else {
                run_with_tools(
                    &state,
                    &run_id_clone,
                    provider,
                    &tool_registry,
                    &text,
                    &provider_name,
                    &history,
                    &session_key_clone,
                    ctx_ref,
                    stats_ref,
                    user_message_index,
                    &discovered_skills,
                )
                .await
            };

            // Persist assistant response.
            if let Some((response_text, input_tokens, output_tokens)) = assistant_text {
                let assistant_msg = serde_json::json!({"role": "assistant", "content": response_text, "model": model_id, "provider": provider_name, "inputTokens": input_tokens, "outputTokens": output_tokens});
                if let Err(e) = session_store
                    .append(&session_key_clone, &assistant_msg)
                    .await
                {
                    warn!("failed to persist assistant message: {e}");
                }
                // Update metadata counts.
                if let Ok(count) = session_store.count(&session_key_clone).await {
                    session_metadata.touch(&session_key_clone, count).await;
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

    async fn history(&self, params: Value) -> ServiceResult {
        let conn_id = params
            .get("_conn_id")
            .and_then(|v| v.as_str())
            .map(String::from);
        let session_key = self.session_key_for(conn_id.as_deref()).await;
        let messages = self
            .session_store
            .read(&session_key)
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::json!(messages))
    }

    async fn inject(&self, _params: Value) -> ServiceResult {
        Err("inject not yet implemented".into())
    }

    async fn clear(&self, params: Value) -> ServiceResult {
        let session_key = if let Some(sk) = params.get("_session_key").and_then(|v| v.as_str()) {
            sk.to_string()
        } else {
            let conn_id = params
                .get("_conn_id")
                .and_then(|v| v.as_str())
                .map(String::from);
            self.session_key_for(conn_id.as_deref()).await
        };

        self.session_store
            .clear(&session_key)
            .await
            .map_err(|e| e.to_string())?;

        // Reset metadata message count.
        self.session_metadata.touch(&session_key, 0).await;

        info!(session = %session_key, "chat.clear");
        Ok(serde_json::json!({ "ok": true }))
    }

    async fn compact(&self, params: Value) -> ServiceResult {
        let session_key = if let Some(sk) = params.get("_session_key").and_then(|v| v.as_str()) {
            sk.to_string()
        } else {
            let conn_id = params
                .get("_conn_id")
                .and_then(|v| v.as_str())
                .map(String::from);
            self.session_key_for(conn_id.as_deref()).await
        };

        let history = self
            .session_store
            .read(&session_key)
            .await
            .map_err(|e| e.to_string())?;

        if history.is_empty() {
            return Err("nothing to compact".into());
        }

        // Build a summary prompt from the conversation.
        let mut summary_messages: Vec<serde_json::Value> = Vec::new();
        summary_messages.push(serde_json::json!({
            "role": "system",
            "content": "You are a conversation summarizer. Summarize the following conversation into a concise form that preserves all key facts, decisions, and context. Output only the summary, no preamble."
        }));

        let mut conversation_text = String::new();
        for msg in &history {
            let role = msg
                .get("role")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let content = msg.get("content").and_then(|v| v.as_str()).unwrap_or("");
            conversation_text.push_str(&format!("{role}: {content}\n\n"));
        }
        summary_messages.push(serde_json::json!({
            "role": "user",
            "content": conversation_text,
        }));

        // Use the session's model if available, otherwise fall back to the model
        // from the last assistant message, then to the first registered provider.
        let provider = {
            let reg = self.providers.read().await;
            let session_model = self
                .session_metadata
                .get(&session_key)
                .await
                .and_then(|e| e.model.clone());
            let history_model = history
                .iter()
                .rev()
                .find_map(|m| m.get("model").and_then(|v| v.as_str()).map(String::from));
            let model_id = session_model.or(history_model);
            if let Some(ref id) = model_id {
                reg.get(id)
            } else {
                None
            }
            .or_else(|| reg.first())
            .ok_or_else(|| "no LLM providers configured".to_string())?
        };

        info!(session = %session_key, messages = history.len(), "chat.compact: summarizing");

        let mut stream = provider.stream(summary_messages);
        let mut summary = String::new();
        while let Some(event) = stream.next().await {
            match event {
                StreamEvent::Delta(delta) => summary.push_str(&delta),
                StreamEvent::Done(_) => break,
                StreamEvent::Error(e) => return Err(format!("compact summarization failed: {e}")),
            }
        }

        if summary.is_empty() {
            return Err("compact produced empty summary".into());
        }

        // Replace history with a single assistant message containing the summary.
        let compacted = vec![serde_json::json!({
            "role": "assistant",
            "content": format!("[Conversation Summary]\n\n{summary}"),
        })];

        self.session_store
            .replace_history(&session_key, compacted.clone())
            .await
            .map_err(|e| e.to_string())?;

        self.session_metadata.touch(&session_key, 1).await;

        info!(session = %session_key, "chat.compact: done");
        Ok(serde_json::json!(compacted))
    }

    async fn context(&self, params: Value) -> ServiceResult {
        let session_key = if let Some(sk) = params.get("_session_key").and_then(|v| v.as_str()) {
            sk.to_string()
        } else {
            let conn_id = params
                .get("_conn_id")
                .and_then(|v| v.as_str())
                .map(String::from);
            self.session_key_for(conn_id.as_deref()).await
        };

        // Session info
        let message_count = self.session_store.count(&session_key).await.unwrap_or(0);
        let session_entry = self.session_metadata.get(&session_key).await;
        let provider_name = {
            let reg = self.providers.read().await;
            let session_model = session_entry.as_ref().and_then(|e| e.model.as_deref());
            if let Some(id) = session_model {
                reg.get(id).map(|p| p.name().to_string())
            } else {
                reg.first().map(|p| p.name().to_string())
            }
        };
        let session_info = serde_json::json!({
            "key": session_key,
            "messageCount": message_count,
            "model": session_entry.as_ref().and_then(|e| e.model.as_deref()),
            "provider": provider_name,
            "label": session_entry.as_ref().and_then(|e| e.label.as_deref()),
            "projectId": session_entry.as_ref().and_then(|e| e.project_id.as_deref()),
        });

        // Project info & context files
        let conn_id = params
            .get("_conn_id")
            .and_then(|v| v.as_str())
            .map(String::from);
        let project_id = if let Some(cid) = conn_id.as_deref() {
            let projects = self.state.active_projects.read().await;
            projects.get(cid).cloned()
        } else {
            None
        };
        let project_id =
            project_id.or_else(|| session_entry.as_ref().and_then(|e| e.project_id.clone()));

        let project_info = if let Some(pid) = project_id {
            match self
                .state
                .services
                .project
                .get(serde_json::json!({"id": pid}))
                .await
            {
                Ok(val) => {
                    let dir = val.get("directory").and_then(|v| v.as_str());
                    let context_files = if let Some(d) = dir {
                        match moltis_projects::context::load_context_files(std::path::Path::new(d))
                        {
                            Ok(files) => files
                                .iter()
                                .map(|f| {
                                    serde_json::json!({
                                        "path": f.path.display().to_string(),
                                        "size": f.content.len(),
                                    })
                                })
                                .collect::<Vec<_>>(),
                            Err(_) => vec![],
                        }
                    } else {
                        vec![]
                    };
                    serde_json::json!({
                        "id": val.get("id"),
                        "label": val.get("label"),
                        "directory": dir,
                        "systemPrompt": val.get("system_prompt").or(val.get("systemPrompt")),
                        "contextFiles": context_files,
                    })
                },
                Err(_) => serde_json::json!(null),
            }
        } else {
            serde_json::json!(null)
        };

        // Tools
        let tool_schemas = self.tool_registry.list_schemas();
        let tools: Vec<_> = tool_schemas
            .iter()
            .map(|s| {
                serde_json::json!({
                    "name": s.get("name").and_then(|v| v.as_str()).unwrap_or("unknown"),
                    "description": s.get("description").and_then(|v| v.as_str()).unwrap_or(""),
                })
            })
            .collect();

        // Token usage from actual API-reported counts stored in messages.
        let messages = self
            .session_store
            .read(&session_key)
            .await
            .unwrap_or_default();
        let mut total_input: u64 = 0;
        let mut total_output: u64 = 0;
        for msg in &messages {
            if let Some(t) = msg.get("inputTokens").and_then(|v| v.as_u64()) {
                total_input += t;
            }
            if let Some(t) = msg.get("outputTokens").and_then(|v| v.as_u64()) {
                total_output += t;
            }
        }
        let total_tokens = total_input + total_output;

        // Context window from the session's provider
        let context_window = {
            let reg = self.providers.read().await;
            let session_model = session_entry.as_ref().and_then(|e| e.model.as_deref());
            if let Some(id) = session_model {
                reg.get(id).map(|p| p.context_window()).unwrap_or(200_000)
            } else {
                reg.first().map(|p| p.context_window()).unwrap_or(200_000)
            }
        };

        // Sandbox info
        let sandbox_info = if let Some(ref router) = self.state.sandbox_router {
            let is_sandboxed = router.is_sandboxed(&session_key).await;
            let config = router.config();
            let session_image = session_entry.as_ref().and_then(|e| e.sandbox_image.clone());
            let effective_image = match session_image {
                Some(img) if !img.is_empty() => img,
                _ => router.default_image().await,
            };
            serde_json::json!({
                "enabled": is_sandboxed,
                "backend": router.backend_name(),
                "mode": config.mode,
                "scope": config.scope,
                "workspaceMount": config.workspace_mount,
                "image": effective_image,
            })
        } else {
            serde_json::json!({
                "enabled": false,
                "backend": null,
            })
        };

        // Discover enabled skills/plugins
        let cwd = std::env::current_dir().unwrap_or_default();
        let search_paths = moltis_skills::discover::FsSkillDiscoverer::default_paths(&cwd);
        let discoverer = moltis_skills::discover::FsSkillDiscoverer::new(search_paths);
        let skills_list: Vec<_> = match discoverer.discover().await {
            Ok(s) => s
                .iter()
                .map(|s| {
                    serde_json::json!({
                        "name": s.name,
                        "description": s.description,
                        "source": s.source,
                    })
                })
                .collect(),
            Err(_) => vec![],
        };

        Ok(serde_json::json!({
            "session": session_info,
            "project": project_info,
            "tools": tools,
            "skills": skills_list,
            "sandbox": sandbox_info,
            "tokenUsage": {
                "inputTokens": total_input,
                "outputTokens": total_output,
                "total": total_tokens,
                "contextWindow": context_window,
            },
        }))
    }
}

// ── Agent loop mode ─────────────────────────────────────────────────────────

async fn run_with_tools(
    state: &Arc<GatewayState>,
    run_id: &str,
    provider: Arc<dyn moltis_agents::model::LlmProvider>,
    tool_registry: &Arc<ToolRegistry>,
    text: &str,
    provider_name: &str,
    history: &[serde_json::Value],
    session_key: &str,
    project_context: Option<&str>,
    session_context: Option<&str>,
    user_message_index: usize,
    skills: &[moltis_skills::types::SkillMetadata],
) -> Option<(String, u32, u32)> {
    // Load identity and user profile from config so the LLM knows who it is.
    let config = moltis_config::discover_and_load();

    let native_tools = provider.supports_tools();
    let system_prompt = build_system_prompt_with_session(
        tool_registry,
        native_tools,
        project_context,
        session_context,
        skills,
        Some(&config.identity),
        Some(&config.user),
    );

    // Broadcast tool events to the UI as they happen.
    let state_for_events = Arc::clone(state);
    let run_id_for_events = run_id.to_string();
    let session_key_for_events = session_key.to_string();
    let on_event: Box<dyn Fn(RunnerEvent) + Send + Sync> = Box::new(move |event| {
        let state = Arc::clone(&state_for_events);
        let run_id = run_id_for_events.clone();
        let sk = session_key_for_events.clone();
        tokio::spawn(async move {
            let payload = match &event {
                RunnerEvent::Thinking => serde_json::json!({
                    "runId": run_id,
                    "sessionKey": sk,
                    "state": "thinking",
                }),
                RunnerEvent::ThinkingDone => serde_json::json!({
                    "runId": run_id,
                    "sessionKey": sk,
                    "state": "thinking_done",
                }),
                RunnerEvent::ToolCallStart {
                    id,
                    name,
                    arguments,
                } => serde_json::json!({
                    "runId": run_id,
                    "sessionKey": sk,
                    "state": "tool_call_start",
                    "toolCallId": id,
                    "toolName": name,
                    "arguments": arguments,
                }),
                RunnerEvent::ToolCallEnd {
                    id,
                    name,
                    success,
                    error,
                    result,
                } => {
                    let mut payload = serde_json::json!({
                        "runId": run_id,
                        "sessionKey": sk,
                        "state": "tool_call_end",
                        "toolCallId": id,
                        "toolName": name,
                        "success": success,
                    });
                    if let Some(err) = error {
                        payload["error"] = serde_json::json!(parse_chat_error(err, None));
                    }
                    if let Some(res) = result {
                        // Cap output sent to the UI to avoid huge WS frames.
                        let mut capped = res.clone();
                        for field in &["stdout", "stderr"] {
                            if let Some(s) = capped.get(*field).and_then(|v| v.as_str())
                                && s.len() > 10_000
                            {
                                let truncated = format!(
                                    "{}\n\n... [truncated — {} bytes total]",
                                    &s[..10_000],
                                    s.len()
                                );
                                capped[*field] = serde_json::Value::String(truncated);
                            }
                        }
                        payload["result"] = capped;
                    }
                    payload
                },
                RunnerEvent::ThinkingText(text) => serde_json::json!({
                    "runId": run_id,
                    "sessionKey": sk,
                    "state": "thinking_text",
                    "text": text,
                }),
                RunnerEvent::TextDelta(text) => serde_json::json!({
                    "runId": run_id,
                    "sessionKey": sk,
                    "state": "delta",
                    "text": text,
                }),
                RunnerEvent::Iteration(n) => serde_json::json!({
                    "runId": run_id,
                    "sessionKey": sk,
                    "state": "iteration",
                    "iteration": n,
                }),
            };
            broadcast(&state, "chat", payload, BroadcastOpts::default()).await;
        });
    });

    // Pass history (excluding the current user message, which run_agent_loop adds).
    let hist = if history.is_empty() {
        None
    } else {
        Some(history.to_vec())
    };

    // Inject session key into tool call params so tools can resolve per-session state.
    let tool_context = serde_json::json!({ "_session_key": session_key });

    let provider_ref = provider.clone();
    match run_agent_loop_with_context(
        provider,
        tool_registry,
        &system_prompt,
        text,
        Some(&on_event),
        hist,
        Some(tool_context),
    )
    .await
    {
        Ok(result) => {
            info!(
                run_id,
                iterations = result.iterations,
                tool_calls = result.tool_calls_made,
                response = %result.text,
                "agent run complete"
            );
            // Assistant message index = user message index + 1.
            let assistant_message_index = user_message_index + 1;
            broadcast(
                state,
                "chat",
                serde_json::json!({
                    "runId": run_id,
                    "sessionKey": session_key,
                    "state": "final",
                    "text": result.text,
                    "iterations": result.iterations,
                    "toolCallsMade": result.tool_calls_made,
                    "model": provider_ref.id(),
                    "provider": provider_name,
                    "inputTokens": result.usage.input_tokens,
                    "outputTokens": result.usage.output_tokens,
                    "messageIndex": assistant_message_index,
                }),
                BroadcastOpts::default(),
            )
            .await;
            deliver_channel_replies(state, session_key, &result.text).await;
            Some((
                result.text,
                result.usage.input_tokens,
                result.usage.output_tokens,
            ))
        },
        Err(e) => {
            warn!(run_id, error = %e, "agent run error");
            let error_obj = parse_chat_error(&e.to_string(), Some(provider_name));
            broadcast(
                state,
                "chat",
                serde_json::json!({
                    "runId": run_id,
                    "sessionKey": session_key,
                    "state": "error",
                    "error": error_obj,
                }),
                BroadcastOpts::default(),
            )
            .await;
            None
        },
    }
}

// ── Streaming mode (no tools) ───────────────────────────────────────────────

async fn run_streaming(
    state: &Arc<GatewayState>,
    run_id: &str,
    provider: Arc<dyn moltis_agents::model::LlmProvider>,
    text: &str,
    provider_name: &str,
    history: &[serde_json::Value],
    session_key: &str,
    project_context: Option<&str>,
    session_context: Option<&str>,
    user_message_index: usize,
    skills: &[moltis_skills::types::SkillMetadata],
) -> Option<(String, u32, u32)> {
    let mut messages: Vec<serde_json::Value> = Vec::new();
    // Prepend session + project context as system messages.
    if let Some(ctx) = session_context {
        messages.push(serde_json::json!({
            "role": "system",
            "content": format!("## Current Session\n\n{ctx}"),
        }));
    }
    if let Some(ctx) = project_context {
        messages.push(serde_json::json!({
            "role": "system",
            "content": ctx,
        }));
    }
    // Inject skills into the system prompt for streaming mode too.
    if !skills.is_empty() {
        let skills_block = moltis_skills::prompt_gen::generate_skills_prompt(skills);
        messages.push(serde_json::json!({
            "role": "system",
            "content": skills_block,
        }));
    }
    messages.extend_from_slice(history);
    messages.push(serde_json::json!({
        "role": "user",
        "content": text,
    }));

    let mut stream = provider.stream(messages);
    let mut accumulated = String::new();

    while let Some(event) = stream.next().await {
        match event {
            StreamEvent::Delta(delta) => {
                accumulated.push_str(&delta);
                broadcast(
                    state,
                    "chat",
                    serde_json::json!({
                        "runId": run_id,
                        "sessionKey": session_key,
                        "state": "delta",
                        "text": delta,
                    }),
                    BroadcastOpts::default(),
                )
                .await;
            },
            StreamEvent::Done(usage) => {
                debug!(
                    run_id,
                    input_tokens = usage.input_tokens,
                    output_tokens = usage.output_tokens,
                    "chat stream done"
                );
                let assistant_message_index = user_message_index + 1;
                broadcast(
                    state,
                    "chat",
                    serde_json::json!({
                        "runId": run_id,
                        "sessionKey": session_key,
                        "state": "final",
                        "text": accumulated,
                        "model": provider.id(),
                        "provider": provider_name,
                        "inputTokens": usage.input_tokens,
                        "outputTokens": usage.output_tokens,
                        "messageIndex": assistant_message_index,
                    }),
                    BroadcastOpts::default(),
                )
                .await;
                deliver_channel_replies(state, session_key, &accumulated).await;
                return Some((accumulated, usage.input_tokens, usage.output_tokens));
            },
            StreamEvent::Error(msg) => {
                warn!(run_id, error = %msg, "chat stream error");
                let error_obj = parse_chat_error(&msg, Some(provider_name));
                broadcast(
                    state,
                    "chat",
                    serde_json::json!({
                        "runId": run_id,
                        "sessionKey": session_key,
                        "state": "error",
                        "error": error_obj,
                    }),
                    BroadcastOpts::default(),
                )
                .await;
                return None;
            },
        }
    }
    None
}

/// Drain any pending channel reply targets for a session and send the
/// response text back to each originating channel via outbound.
/// Each delivery runs in its own spawned task so slow network calls
/// don't block each other or the chat pipeline.
async fn deliver_channel_replies(state: &Arc<GatewayState>, session_key: &str, text: &str) {
    let targets = state.drain_channel_replies(session_key).await;
    if targets.is_empty() || text.is_empty() {
        return;
    }
    let outbound = match state.services.channel_outbound_arc() {
        Some(o) => o,
        None => return,
    };
    let text = text.to_string();
    for target in targets {
        let outbound = Arc::clone(&outbound);
        let text = text.clone();
        tokio::spawn(async move {
            match target.channel_type.as_str() {
                "telegram" => {
                    if let Err(e) = outbound
                        .send_text(&target.account_id, &target.chat_id, &text)
                        .await
                    {
                        warn!(
                            account_id = target.account_id,
                            chat_id = target.chat_id,
                            "failed to send channel reply: {e}"
                        );
                    }
                },
                other => {
                    warn!(channel_type = other, "unsupported channel type for reply");
                },
            }
        });
    }
}
