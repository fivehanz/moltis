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
        prompt::build_system_prompt,
        providers::ProviderRegistry,
        runner::{RunnerEvent, run_agent_loop},
        tool_registry::ToolRegistry,
    },
    moltis_sessions::{metadata::SessionMetadata, store::SessionStore},
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
    session_metadata: Arc<RwLock<SessionMetadata>>,
}

impl LiveChatService {
    pub fn new(
        providers: Arc<RwLock<ProviderRegistry>>,
        state: Arc<GatewayState>,
        session_store: Arc<SessionStore>,
        session_metadata: Arc<RwLock<SessionMetadata>>,
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
        let model_id = params.get("model").and_then(|v| v.as_str());
        // Use streaming-only mode if explicitly requested or if no tools are registered.
        let stream_only = params
            .get("stream_only")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
            || !self.has_tools();

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

        // Resolve session key: explicit override (used by cron callbacks) or
        // connection-scoped lookup.
        let session_key = if let Some(sk) = params.get("_session_key").and_then(|v| v.as_str()) {
            sk.to_string()
        } else {
            self.session_key_for(conn_id.as_deref()).await
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
            let project_id = project_id.or_else(|| {
                // If the session itself is bound to a project, use that.
                let meta = self.session_metadata.try_read().ok()?;
                let entry = meta.get(&session_key)?;
                entry.project_id.clone()
            });
            if let Some(pid) = project_id {
                match self.state.services.project.get(serde_json::json!({"id": pid})).await {
                    Ok(val) => {
                        if let Some(dir) = val.get("directory").and_then(|v| v.as_str()) {
                            match moltis_projects::context::load_context_files(std::path::Path::new(dir)) {
                                Ok(files) => {
                                    let project: Option<moltis_projects::Project> =
                                        serde_json::from_value(val.clone()).ok();
                                    if let Some(p) = project {
                                        let ctx = moltis_projects::ProjectContext {
                                            project: p,
                                            context_files: files,
                                            worktree_dir: None,
                                        };
                                        Some(ctx.to_prompt_section())
                                    } else {
                                        None
                                    }
                                }
                                Err(e) => {
                                    warn!("failed to load project context: {e}");
                                    None
                                }
                            }
                        } else {
                            None
                        }
                    }
                    Err(_) => None,
                }
            } else {
                None
            }
        };

        // Persist the user message.
        let user_msg = serde_json::json!({"role": "user", "content": &text});
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
        {
            let mut meta = self.session_metadata.write().await;
            meta.upsert(&session_key, None);
            meta.touch(&session_key, history.len() as u32);
            let _ = meta.save();
        }

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

        let provider_name = provider.name().to_string();
        let model_id = provider.id().to_string();
        let session_store = Arc::clone(&self.session_store);
        let session_metadata = Arc::clone(&self.session_metadata);
        let session_key_clone = session_key.clone();
        let handle = tokio::spawn(async move {
            let ctx_ref = project_context.as_deref();
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
                )
                .await
            };

            // Persist assistant response.
            if let Some(response_text) = assistant_text {
                let assistant_msg =
                    serde_json::json!({"role": "assistant", "content": response_text, "model": model_id, "provider": provider_name});
                if let Err(e) = session_store
                    .append(&session_key_clone, &assistant_msg)
                    .await
                {
                    warn!("failed to persist assistant message: {e}");
                }
                // Update metadata counts.
                if let Ok(count) = session_store.count(&session_key_clone).await {
                    let mut meta = session_metadata.write().await;
                    meta.touch(&session_key_clone, count);
                    let _ = meta.save();
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
) -> Option<String> {
    let native_tools = provider.supports_tools();
    let system_prompt = build_system_prompt(tool_registry, native_tools, project_context);

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
                            if let Some(s) = capped.get(*field).and_then(|v| v.as_str()) {
                                if s.len() > 10_000 {
                                    let truncated = format!(
                                        "{}\n\n... [truncated — {} bytes total]",
                                        &s[..10_000],
                                        s.len()
                                    );
                                    capped[*field] = serde_json::Value::String(truncated);
                                }
                            }
                        }
                        payload["result"] = capped;
                    }
                    payload
                },
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

    let provider_ref = provider.clone();
    match run_agent_loop(
        provider,
        tool_registry,
        &system_prompt,
        text,
        Some(&on_event),
        hist,
    )
    .await
    {
        Ok(result) => {
            info!(
                run_id,
                iterations = result.iterations,
                tool_calls = result.tool_calls_made,
                "agent run complete"
            );
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
                }),
                BroadcastOpts::default(),
            )
            .await;
            Some(result.text)
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
) -> Option<String> {
    let mut messages: Vec<serde_json::Value> = Vec::new();
    // Prepend project context as a system message if available.
    if let Some(ctx) = project_context {
        messages.push(serde_json::json!({
            "role": "system",
            "content": ctx,
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
                    }),
                    BroadcastOpts::default(),
                )
                .await;
                return Some(accumulated);
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
