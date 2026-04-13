//! `ChatService` trait implementation for `LiveChatService`.

use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::Arc,
    time::Instant,
};

use {
    async_trait::async_trait,
    serde_json::Value,
    tokio::sync::RwLock,
    tracing::{debug, info, warn},
};

use {
    moltis_agents::{
        UserContent,
        model::values_to_chat_messages,
        prompt::{
            PromptRuntimeContext,
            build_system_prompt_with_session_runtime_details,
        },
        tool_registry::ToolRegistry,
    },
    moltis_config::{MessageQueueMode, ToolMode},
    moltis_sessions::{
        ContentBlock, MessageContent, PersistedMessage,
        message::ImageUrl,
        metadata::SessionEntry,
        store::SessionStore,
    },
    moltis_service_traits::{ChatService, ServiceResult},
    moltis_tools::policy::ToolPolicy,
};

use crate::{
    agent_loop::{run_explicit_shell_command, compact_session, mark_unsupported_model, clear_unsupported_model},
    channels::{deliver_channel_replies, deliver_channel_error, notify_channels_of_compaction, send_chat_push_notification, generate_tts_audio, send_retry_status_to_channels},
    chat_error::parse_chat_error,
    compaction_run,
    error,
    message::{
        apply_message_received_rewrite, apply_voice_reply_suffix, infer_reply_medium,
        to_user_content, user_audio_path_from_params, user_documents_for_persistence,
        user_documents_from_params,
    },
    prompt::{
        apply_request_runtime_context, build_policy_context, build_prompt_runtime_context,
        build_tool_context, clear_prompt_memory_snapshot, discover_skills_if_enabled,
        load_prompt_persona_for_agent, load_prompt_persona_for_session,
        prompt_build_limits_from_config, resolve_prompt_agent_id, apply_runtime_tool_filters,
    },
    run_with_tools::run_with_tools,
    runtime::ChatRuntime,
    streaming::run_streaming,
    types::*,
};

use super::*;

#[async_trait]
impl ChatService for LiveChatService {
    #[tracing::instrument(skip(self, params), fields(session_id))]
    async fn send(&self, mut params: Value) -> ServiceResult {
        // Support both text-only and multimodal content.
        // - "text": string → plain text message
        // - "content": array → multimodal content (text + images)
        //
        // Note: `text` and `message_content` are `mut` because a
        // `MessageReceived` hook may return `ModifyPayload` to rewrite the
        // inbound message before the turn begins (see GH #639).
        let (mut text, mut message_content) = if let Some(content) = params.get("content") {
            // Multimodal content - extract text for logging/hooks, parse into typed blocks
            let text_part = content
                .as_array()
                .and_then(|arr| {
                    arr.iter()
                        .find(|block| block.get("type").and_then(|t| t.as_str()) == Some("text"))
                        .and_then(|block| block.get("text").and_then(|t| t.as_str()))
                })
                .unwrap_or("[Image]")
                .to_string();

            // Parse JSON blocks into typed ContentBlock structs
            let blocks: Vec<ContentBlock> = content
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|block| {
                            let block_type = block.get("type")?.as_str()?;
                            match block_type {
                                "text" => {
                                    let text = block.get("text")?.as_str()?.to_string();
                                    Some(ContentBlock::text(text))
                                },
                                "image_url" => {
                                    let url = block.get("image_url")?.get("url")?.as_str()?;
                                    Some(ContentBlock::ImageUrl {
                                        image_url: moltis_sessions::message::ImageUrl {
                                            url: url.to_string(),
                                        },
                                    })
                                },
                                _ => None,
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();
            (text_part, MessageContent::Multimodal(blocks))
        } else {
            let text = params
                .get("text")
                .or_else(|| params.get("message"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| "missing 'text', 'message', or 'content' parameter".to_string())?
                .to_string();
            (text.clone(), MessageContent::Text(text))
        };
        let desired_reply_medium = infer_reply_medium(&params, &text);

        let conn_id = params
            .get("_conn_id")
            .and_then(|v| v.as_str())
            .map(String::from);
        let explicit_model = params.get("model").and_then(|v| v.as_str());
        // Use streaming-only mode if explicitly requested or if no tools are registered.
        let explicit_stream_only = params
            .get("stream_only")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let has_tools = self.has_tools_sync();
        let stream_only = explicit_stream_only || !has_tools;
        tracing::debug!(
            explicit_stream_only,
            has_tools,
            stream_only,
            "send() mode decision"
        );

        // Resolve session key from explicit overrides, public request params, or connection context.
        let session_key = self.resolve_session_key_from_params(&params).await;
        let queued_replay = params
            .get("_queued_replay")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // Track client-side sequence number for ordering diagnostics.
        // Note: seq resets to 1 on page reload, so a drop from a high value
        // back to 1 is normal (new browser session) — only flag issues within
        // a continuous ascending sequence.
        let client_seq = params.get("_seq").and_then(|v| v.as_u64());
        if let Some(seq) = client_seq {
            if queued_replay {
                debug!(
                    session = %session_key,
                    seq,
                    "client seq replayed from queue; skipping ordering diagnostics"
                );
            } else {
                let mut seq_map = self.last_client_seq.write().await;
                let last = seq_map.entry(session_key.clone()).or_insert(0);
                if *last == 0 {
                    // First observed sequence for this session in this process.
                    // We cannot infer a gap yet because earlier messages may have
                    // come from another tab/process before we started tracking.
                    debug!(session = %session_key, seq, "client seq initialized");
                } else if seq == 1 && *last > 1 {
                    // Page reload — reset tracking.
                    debug!(
                        session = %session_key,
                        prev_seq = *last,
                        "client seq reset (page reload)"
                    );
                } else if seq <= *last {
                    warn!(
                        session = %session_key,
                        seq,
                        last_seq = *last,
                        "client seq out of order (duplicate or reorder)"
                    );
                } else if seq > *last + 1 {
                    warn!(
                        session = %session_key,
                        seq,
                        last_seq = *last,
                        gap = seq - *last - 1,
                        "client seq gap detected (missing messages)"
                    );
                }
                *last = seq;
            }
        }

        let explicit_shell_command = match &message_content {
            MessageContent::Text(raw) => parse_explicit_shell_command(raw).map(str::to_string),
            MessageContent::Multimodal(_) => None,
        };

        if let Some(shell_command) = explicit_shell_command {
            // Generate run_id early so we can link the user message to this run.
            let run_id = uuid::Uuid::new_v4().to_string();
            let run_id_clone = run_id.clone();
            let channel_meta = params.get("channel").cloned();
            let user_audio = user_audio_path_from_params(&params, &session_key);
            let user_documents =
                user_documents_from_params(&params, &session_key, self.session_store.as_ref());
            let user_msg = PersistedMessage::User {
                content: message_content,
                created_at: Some(now_ms()),
                audio: user_audio,
                documents: user_documents
                    .as_deref()
                    .and_then(user_documents_for_persistence),
                channel: channel_meta,
                seq: client_seq,
                run_id: Some(run_id.clone()),
            };

            let history = self
                .session_store
                .read(&session_key)
                .await
                .unwrap_or_default();
            let user_message_index = history.len();

            // Ensure the session exists in metadata and counts are up to date.
            let _ = self.session_metadata.upsert(&session_key, None).await;
            self.session_metadata
                .touch(&session_key, history.len() as u32)
                .await;

            // If this is a web UI message on a channel-bound session, attach the
            // channel reply target so /sh output can be delivered back to the channel.
            let is_web_message = conn_id.is_some()
                && params.get("_session_key").is_none()
                && params.get("channel").is_none();

            if is_web_message
                && let Some(entry) = self.session_metadata.get(&session_key).await
                && let Some(ref binding_json) = entry.channel_binding
                && let Ok(target) =
                    serde_json::from_str::<moltis_channels::ChannelReplyTarget>(binding_json)
            {
                let is_active = self
                    .session_metadata
                    .get_active_session(
                        target.channel_type.as_str(),
                        &target.account_id,
                        &target.chat_id,
                        target.thread_id.as_deref(),
                    )
                    .await
                    .map(|k| k == session_key)
                    .unwrap_or(true);

                if is_active {
                    match serde_json::to_value(&target) {
                        Ok(target_val) => {
                            params["_channel_reply_target"] = target_val;
                        },
                        Err(e) => {
                            warn!(
                                session = %session_key,
                                error = %e,
                                "failed to serialize channel reply target for /sh"
                            );
                        },
                    }
                }
            }

            let deferred_channel_target =
                params
                    .get("_channel_reply_target")
                    .cloned()
                    .and_then(|value| {
                        match serde_json::from_value::<moltis_channels::ChannelReplyTarget>(value) {
                            Ok(target) => Some(target),
                            Err(e) => {
                                warn!(
                                    session = %session_key,
                                    error = %e,
                                    "ignoring invalid _channel_reply_target for /sh"
                                );
                                None
                            },
                        }
                    });

            info!(
                run_id = %run_id,
                user_message = %text,
                session = %session_key,
                command = %shell_command,
                client_seq = ?client_seq,
                mode = "explicit_shell",
                "chat.send"
            );

            // Try to acquire the per-session semaphore. If a run is already active,
            // queue the message according to MessageQueueMode.
            let session_sem = self.session_semaphore(&session_key).await;
            let permit: OwnedSemaphorePermit = match session_sem.clone().try_acquire_owned() {
                Ok(p) => p,
                Err(_) => {
                    let queue_mode = moltis_config::discover_and_load().chat.message_queue_mode;
                    info!(
                        session = %session_key,
                        mode = ?queue_mode,
                        client_seq = ?client_seq,
                        "queueing message (run active)"
                    );
                    let position = {
                        let mut q = self.message_queue.write().await;
                        let entry = q.entry(session_key.clone()).or_default();
                        entry.push(QueuedMessage {
                            params: params.clone(),
                        });
                        entry.len()
                    };
                    broadcast(
                        &self.state,
                        "chat",
                        serde_json::json!({
                            "sessionKey": session_key,
                            "state": "queued",
                            "mode": format!("{queue_mode:?}").to_lowercase(),
                            "position": position,
                        }),
                        BroadcastOpts::default(),
                    )
                    .await;
                    return Ok(serde_json::json!({
                        "ok": true,
                        "queued": true,
                        "mode": format!("{queue_mode:?}").to_lowercase(),
                    }));
                },
            };

            // Persist user message now that it will execute immediately.
            if let Err(e) = self
                .session_store
                .append(&session_key, &user_msg.to_value())
                .await
            {
                warn!("failed to persist /sh user message: {e}");
            }

            // Set preview from first user message if not already set.
            if let Some(entry) = self.session_metadata.get(&session_key).await
                && entry.preview.is_none()
            {
                let preview_text = extract_preview_from_value(&user_msg.to_value());
                if let Some(preview) = preview_text {
                    self.session_metadata
                        .set_preview(&session_key, Some(&preview))
                        .await;
                }
            }

            let state = Arc::clone(&self.state);
            let active_runs = Arc::clone(&self.active_runs);
            let active_runs_by_session = Arc::clone(&self.active_runs_by_session);
            let active_thinking_text = Arc::clone(&self.active_thinking_text);
            let active_tool_calls = Arc::clone(&self.active_tool_calls);
            let active_partial_assistant = Arc::clone(&self.active_partial_assistant);
            let active_reply_medium = Arc::clone(&self.active_reply_medium);
            let terminal_runs = Arc::clone(&self.terminal_runs);
            let session_store = Arc::clone(&self.session_store);
            let session_metadata = Arc::clone(&self.session_metadata);
            let tool_registry = Arc::clone(&self.tool_registry);
            let session_key_clone = session_key.clone();
            let message_queue = Arc::clone(&self.message_queue);
            let state_for_drain = Arc::clone(&self.state);
            let accept_language = params
                .get("_accept_language")
                .and_then(|v| v.as_str())
                .map(String::from);
            let conn_id_for_tool = conn_id.clone();

            let handle = tokio::spawn(async move {
                let permit = permit; // hold permit until command run completes
                if let Some(target) = deferred_channel_target {
                    state.push_channel_reply(&session_key_clone, target).await;
                }
                active_reply_medium
                    .write()
                    .await
                    .insert(session_key_clone.clone(), ReplyMedium::Text);

                let assistant_output = run_explicit_shell_command(
                    &state,
                    &run_id_clone,
                    &tool_registry,
                    &session_store,
                    &terminal_runs,
                    &session_key_clone,
                    &shell_command,
                    user_message_index,
                    accept_language,
                    conn_id_for_tool,
                    client_seq,
                )
                .await;

                let assistant_msg = PersistedMessage::Assistant {
                    content: assistant_output.text,
                    created_at: Some(now_ms()),
                    model: None,
                    provider: None,
                    input_tokens: Some(assistant_output.input_tokens),
                    output_tokens: Some(assistant_output.output_tokens),
                    duration_ms: Some(assistant_output.duration_ms),
                    request_input_tokens: Some(assistant_output.request_input_tokens),
                    request_output_tokens: Some(assistant_output.request_output_tokens),
                    tool_calls: None,
                    reasoning: assistant_output.reasoning,
                    llm_api_response: assistant_output.llm_api_response,
                    audio: assistant_output.audio_path,
                    seq: client_seq,
                    run_id: Some(run_id_clone.clone()),
                };
                if let Err(e) = session_store
                    .append(&session_key_clone, &assistant_msg.to_value())
                    .await
                {
                    warn!("failed to persist /sh assistant message: {e}");
                }
                if let Ok(count) = session_store.count(&session_key_clone).await {
                    session_metadata.touch(&session_key_clone, count).await;
                }

                active_runs.write().await.remove(&run_id_clone);
                let mut runs_by_session = active_runs_by_session.write().await;
                if runs_by_session.get(&session_key_clone) == Some(&run_id_clone) {
                    runs_by_session.remove(&session_key_clone);
                }
                drop(runs_by_session);
                active_thinking_text
                    .write()
                    .await
                    .remove(&session_key_clone);
                active_tool_calls.write().await.remove(&session_key_clone);
                terminal_runs.write().await.remove(&run_id_clone);
                active_partial_assistant
                    .write()
                    .await
                    .remove(&session_key_clone);
                active_reply_medium.write().await.remove(&session_key_clone);

                drop(permit);

                // Drain queued messages for this session.
                let queued = message_queue
                    .write()
                    .await
                    .remove(&session_key_clone)
                    .unwrap_or_default();
                if !queued.is_empty() {
                    let queue_mode = moltis_config::discover_and_load().chat.message_queue_mode;
                    let chat = state_for_drain.chat_service().await;
                    match queue_mode {
                        MessageQueueMode::Followup => {
                            let mut iter = queued.into_iter();
                            let Some(first) = iter.next() else {
                                return;
                            };
                            let rest: Vec<QueuedMessage> = iter.collect();
                            if !rest.is_empty() {
                                message_queue
                                    .write()
                                    .await
                                    .entry(session_key_clone.clone())
                                    .or_default()
                                    .extend(rest);
                            }
                            info!(session = %session_key_clone, "replaying queued message (followup)");
                            let mut replay_params = first.params;
                            replay_params["_queued_replay"] = serde_json::json!(true);
                            if let Err(e) = chat.send(replay_params).await {
                                warn!(session = %session_key_clone, error = %e, "failed to replay queued message");
                            }
                        },
                        MessageQueueMode::Collect => {
                            let combined: Vec<&str> = queued
                                .iter()
                                .filter_map(|m| m.params.get("text").and_then(|v| v.as_str()))
                                .collect();
                            if !combined.is_empty() {
                                info!(
                                    session = %session_key_clone,
                                    count = combined.len(),
                                    "replaying collected messages"
                                );
                                let Some(last) = queued.last() else {
                                    return;
                                };
                                let mut merged = last.params.clone();
                                merged["text"] = serde_json::json!(combined.join("\n\n"));
                                merged["_queued_replay"] = serde_json::json!(true);
                                if let Err(e) = chat.send(merged).await {
                                    warn!(session = %session_key_clone, error = %e, "failed to replay collected messages");
                                }
                            }
                        },
                    }
                }
            });

            self.active_runs
                .write()
                .await
                .insert(run_id.clone(), handle.abort_handle());
            self.active_runs_by_session
                .write()
                .await
                .insert(session_key.clone(), run_id.clone());

            return Ok(serde_json::json!({ "ok": true, "runId": run_id }));
        }

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

        let provider: Arc<dyn moltis_agents::model::LlmProvider> = {
            let reg = self.providers.read().await;
            let primary = if let Some(id) = model_id {
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
            };

            if self.failover_config.enabled {
                let fallbacks = if self.failover_config.fallback_models.is_empty() {
                    // Auto-build: same model on other providers first, then same
                    // provider's other models, then everything else.
                    reg.fallback_providers_for(primary.id(), primary.name())
                } else {
                    reg.providers_for_models(&self.failover_config.fallback_models)
                };
                if fallbacks.is_empty() {
                    primary
                } else {
                    let mut chain = vec![primary];
                    chain.extend(fallbacks);
                    Arc::new(moltis_agents::provider_chain::ProviderChain::new(chain))
                }
            } else {
                primary
            }
        };

        // Check if this is a local model that needs downloading.
        // Only do this check for local-llm providers.
        #[cfg(feature = "local-llm")]
        if provider.name() == "local-llm" {
            let model_to_check = model_id
                .map(raw_model_id)
                .unwrap_or_else(|| raw_model_id(provider.id()))
                .to_string();
            tracing::info!(
                provider_name = provider.name(),
                model_to_check,
                "checking local model cache"
            );
            if let Err(e) = self.state.ensure_local_model_cached(&model_to_check).await {
                return Err(format!("Failed to prepare local model: {}", e).into());
            }
        }

        // Resolve project context for this connection's active project.
        let project_context = self
            .resolve_project_context(&session_key, conn_id.as_deref())
            .await;

        // Generate run_id early so we can link the user message to its agent run.
        let run_id = uuid::Uuid::new_v4().to_string();

        // Load conversation history (the current user message is NOT yet
        // persisted — run_streaming / run_agent_loop add it themselves).
        let mut history = self
            .session_store
            .read(&session_key)
            .await
            .unwrap_or_default();

        // Update metadata.
        let _ = self.session_metadata.upsert(&session_key, None).await;
        self.session_metadata
            .touch(&session_key, history.len() as u32)
            .await;

        // If this is a web UI message on a channel-bound session, attach the
        // channel reply target so the run-start path can route the final
        // response back to the channel.
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
                .get_active_session(
                    target.channel_type.as_str(),
                    &target.account_id,
                    &target.chat_id,
                    target.thread_id.as_deref(),
                )
                .await
                .map(|k| k == session_key)
                .unwrap_or(true);

            if is_active {
                match serde_json::to_value(&target) {
                    Ok(target_val) => {
                        params["_channel_reply_target"] = target_val;
                    },
                    Err(e) => {
                        warn!(
                            session = %session_key,
                            error = %e,
                            "failed to serialize channel reply target"
                        );
                    },
                }
            }
        }

        let deferred_channel_target =
            params
                .get("_channel_reply_target")
                .cloned()
                .and_then(|value| {
                    match serde_json::from_value::<moltis_channels::ChannelReplyTarget>(value) {
                        Ok(target) => Some(target),
                        Err(e) => {
                            warn!(
                                session = %session_key,
                                error = %e,
                                "ignoring invalid _channel_reply_target"
                            );
                            None
                        },
                    }
                });

        // Dispatch the `MessageReceived` hook before the turn starts. The
        // hook can:
        //   - return `Continue` → proceed normally;
        //   - return `ModifyPayload({"content": "..."})` → rewrite the
        //     inbound text before it is persisted or sent to the model;
        //   - return `Block(reason)` → abort this turn entirely. The user
        //     message is NOT persisted, no run is started, and the reason
        //     is surfaced to the channel/web sender.
        //
        // Hook errors are treated as fail-open: a broken hook must not be
        // able to wedge every inbound message. See GH #639.
        if let Some(ref hooks) = self.hook_registry {
            let session_entry = self.session_metadata.get(&session_key).await;
            let channel = params
                .get("channel")
                .and_then(|v| v.as_str())
                .map(String::from);
            let channel_binding = Some(resolve_channel_runtime_context(
                &session_key,
                session_entry.as_ref(),
            ))
            .filter(|binding| !binding.is_empty());
            let payload = moltis_common::hooks::HookPayload::MessageReceived {
                session_key: session_key.clone(),
                content: text.clone(),
                channel,
                channel_binding,
            };
            match hooks.dispatch(&payload).await {
                Ok(moltis_common::hooks::HookAction::Continue) => {},
                Ok(moltis_common::hooks::HookAction::ModifyPayload(new_payload)) => {
                    match new_payload.get("content").and_then(|v| v.as_str()) {
                        Some(new_text) => {
                            info!(
                                session = %session_key,
                                "MessageReceived hook rewrote inbound content"
                            );
                            text = new_text.to_string();
                            apply_message_received_rewrite(
                                &mut message_content,
                                &mut params,
                                new_text,
                            );
                        },
                        None => {
                            warn!(
                                session = %session_key,
                                "MessageReceived hook ModifyPayload ignored: expected object with `content` string"
                            );
                        },
                    }
                },
                Ok(moltis_common::hooks::HookAction::Block(reason)) => {
                    info!(
                        session = %session_key,
                        reason = %reason,
                        "MessageReceived hook blocked inbound message"
                    );

                    // Surface the rejection to channel senders via the
                    // existing channel-error delivery path. If the caller
                    // attached a reply target (web-UI-on-bound-session or an
                    // inbound channel message), re-register it so
                    // `deliver_channel_error` has a destination to drain.
                    if let Some(target) = deferred_channel_target.clone() {
                        self.state.push_channel_reply(&session_key, target).await;
                        let error_obj = serde_json::json!({
                            "type": "message_rejected",
                            "message": reason,
                        });
                        deliver_channel_error(&self.state, &session_key, &error_obj).await;
                    }

                    // Broadcast a rejection event so web UI clients see it.
                    broadcast(
                        &self.state,
                        "chat",
                        serde_json::json!({
                            "state": "rejected",
                            "sessionKey": session_key,
                            "reason": reason,
                        }),
                        BroadcastOpts::default(),
                    )
                    .await;

                    return Ok(serde_json::json!({
                        "ok": false,
                        "rejected": true,
                        "reason": reason,
                    }));
                },
                Err(e) => {
                    warn!(
                        session = %session_key,
                        error = %e,
                        "MessageReceived hook failed; proceeding fail-open"
                    );
                },
            }
        }

        // Convert session-crate content to agents-crate content for the LLM.
        // Must happen before `message_content` is moved into `user_msg`, and
        // must happen AFTER the MessageReceived hook dispatch so a
        // `ModifyPayload` rewrite is reflected in both `user_content` (what
        // the LLM sees) and `user_msg` (what gets persisted).
        let user_documents =
            user_documents_from_params(&params, &session_key, self.session_store.as_ref())
                .unwrap_or_default();
        let user_content = to_user_content(&message_content, &user_documents);

        // Build the user message for later persistence (deferred until we
        // know the message won't be queued — avoids double-persist when a
        // queued message is replayed via send()).
        let channel_meta = params.get("channel").cloned();
        let user_audio = user_audio_path_from_params(&params, &session_key);
        let user_msg = PersistedMessage::User {
            content: message_content,
            created_at: Some(now_ms()),
            audio: user_audio,
            documents: user_documents_for_persistence(&user_documents),
            channel: channel_meta,
            seq: client_seq,
            run_id: Some(run_id.clone()),
        };

        // Discover enabled skills/plugins for prompt injection (gated on
        // `[skills] enabled` — see #655).
        let discovered_skills =
            discover_skills_if_enabled(&moltis_config::discover_and_load()).await;

        // Check if MCP tools are disabled for this session and capture
        // per-session sandbox override details for prompt runtime context.
        let session_entry = self.session_metadata.get(&session_key).await;
        let session_agent_id = resolve_prompt_agent_id(session_entry.as_ref());
        let persona = load_prompt_persona_for_session(
            &session_key,
            session_entry.as_ref(),
            self.session_state_store.as_deref(),
        )
        .await;
        let mcp_disabled = session_entry
            .as_ref()
            .and_then(|entry| entry.mcp_disabled)
            .unwrap_or(false);
        let mut runtime_context = build_prompt_runtime_context(
            &self.state,
            &provider,
            &session_key,
            session_entry.as_ref(),
        )
        .await;
        apply_request_runtime_context(&mut runtime_context.host, &params);

        let state = Arc::clone(&self.state);
        let active_runs = Arc::clone(&self.active_runs);
        let active_runs_by_session = Arc::clone(&self.active_runs_by_session);
        let active_thinking_text = Arc::clone(&self.active_thinking_text);
        let active_tool_calls = Arc::clone(&self.active_tool_calls);
        let active_partial_assistant = Arc::clone(&self.active_partial_assistant);
        let active_reply_medium = Arc::clone(&self.active_reply_medium);
        let run_id_clone = run_id.clone();
        let tool_registry = Arc::clone(&self.tool_registry);
        let hook_registry = self.hook_registry.clone();

        // Log if tool mode is active but the provider doesn't support tools.
        // Note: We don't broadcast to the user here - they chose the model knowing
        // its limitations. The UI should show capabilities when selecting a model.
        if !stream_only && !provider.supports_tools() {
            debug!(
                provider = provider.name(),
                model = provider.id(),
                "selected provider does not support tool calling"
            );
        }

        info!(
            run_id = %run_id,
            user_message = %text,
            model = provider.id(),
            stream_only,
            session = %session_key,
            reply_medium = ?desired_reply_medium,
            client_seq = ?client_seq,
            "chat.send"
        );

        // Capture user message index (0-based) so we can include assistant
        // message index in the "final" broadcast for client-side deduplication.
        let user_message_index = history.len(); // user msg is at this index in the JSONL

        let provider_name = provider.name().to_string();
        let model_id = provider.id().to_string();
        let model_store = Arc::clone(&self.model_store);
        let session_store = Arc::clone(&self.session_store);
        let session_metadata = Arc::clone(&self.session_metadata);
        let session_agent_id_clone = session_agent_id.clone();
        let session_key_clone = session_key.clone();
        let accept_language = params
            .get("_accept_language")
            .and_then(|v| v.as_str())
            .map(String::from);
        // Auto-compact when the next request is likely to exceed
        // `chat.compaction.threshold_percent` of the model context window.
        // The value is clamped to the 0.1–0.95 range in case config
        // validation missed a typo; the default (0.95) is loaded via
        // load_prompt_persona_for_agent for the session's agent and
        // matches the pre-PR-#653 hardcoded trigger.
        let compaction_cfg = &load_prompt_persona_for_agent(&session_agent_id)
            .config
            .chat
            .compaction;
        let context_window = provider.context_window() as u64;
        let token_usage = session_token_usage_from_messages(&history);
        let estimated_next_input = token_usage
            .current_request_input_tokens
            .saturating_add(estimate_text_tokens(&text));
        let compact_threshold =
            compute_auto_compact_threshold(context_window, compaction_cfg.threshold_percent);

        if estimated_next_input >= compact_threshold {
            let pre_compact_msg_count = history.len();
            let pre_compact_total = token_usage
                .current_request_input_tokens
                .saturating_add(token_usage.current_request_output_tokens);

            info!(
                session = %session_key,
                estimated_next_input,
                context_window,
                threshold_percent = compaction_cfg.threshold_percent,
                compact_threshold,
                "auto-compact triggered (estimated next request over chat.compaction.threshold_percent)"
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
                    "inputTokens": token_usage.current_request_input_tokens,
                    "outputTokens": token_usage.current_request_output_tokens,
                    "estimatedNextInputTokens": estimated_next_input,
                    "sessionInputTokens": token_usage.session_input_tokens,
                    "sessionOutputTokens": token_usage.session_output_tokens,
                    "contextWindow": context_window,
                }),
                BroadcastOpts::default(),
            )
            .await;

            let compact_params = serde_json::json!({ "_session_key": &session_key });
            match self.compact(compact_params).await {
                Ok(_) => {
                    // Reload history after compaction.
                    history = self
                        .session_store
                        .read(&session_key)
                        .await
                        .unwrap_or_default();
                    // This `auto_compact done` event is a lifecycle
                    // signal for subscribers that pre-emptive
                    // auto-compact finished. The mode/token metadata
                    // lives on the `chat.compact done` event that
                    // `self.compact()` broadcasts from the inside —
                    // the `compactBroadcastPath: "inner"` marker below
                    // lets hook / webhook consumers detect that and
                    // subscribe to that event instead. The parallel
                    // `run_with_tools` context-overflow path emits a
                    // self-contained `auto_compact done` (with
                    // `compactBroadcastPath: "wrapper"`) that carries
                    // the metadata directly.
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
                            "compactBroadcastPath": "inner",
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

        // Try to acquire the per-session semaphore.  If a run is already active,
        // queue the message according to the configured MessageQueueMode instead
        // of blocking the caller.
        let session_sem = self.session_semaphore(&session_key).await;
        let permit: OwnedSemaphorePermit = match session_sem.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                // Active run — enqueue and return immediately.
                let queue_mode = moltis_config::discover_and_load().chat.message_queue_mode;
                info!(
                    session = %session_key,
                    mode = ?queue_mode,
                    client_seq = ?client_seq,
                    "queueing message (run active)"
                );
                let position = {
                    let mut q = self.message_queue.write().await;
                    let entry = q.entry(session_key.clone()).or_default();
                    entry.push(QueuedMessage {
                        params: params.clone(),
                    });
                    entry.len()
                };
                broadcast(
                    &self.state,
                    "chat",
                    serde_json::json!({
                        "sessionKey": session_key,
                        "state": "queued",
                        "mode": format!("{queue_mode:?}").to_lowercase(),
                        "position": position,
                    }),
                    BroadcastOpts::default(),
                )
                .await;
                return Ok(serde_json::json!({
                    "ok": true,
                    "queued": true,
                    "mode": format!("{queue_mode:?}").to_lowercase(),
                }));
            },
        };

        // Persist the user message now that we know it won't be queued.
        // (Queued messages skip this; they are persisted when replayed.)
        if let Err(e) = self
            .session_store
            .append(&session_key, &user_msg.to_value())
            .await
        {
            warn!("failed to persist user message: {e}");
        }

        // Set preview from the first user message if not already set.
        if let Some(entry) = self.session_metadata.get(&session_key).await
            && entry.preview.is_none()
        {
            let preview_text = extract_preview_from_value(&user_msg.to_value());
            if let Some(preview) = preview_text {
                self.session_metadata
                    .set_preview(&session_key, Some(&preview))
                    .await;
            }
        }

        let agent_timeout_secs = moltis_config::discover_and_load().tools.agent_timeout_secs;

        let message_queue = Arc::clone(&self.message_queue);
        let state_for_drain = Arc::clone(&self.state);
        let active_event_forwarders = Arc::clone(&self.active_event_forwarders);
        let terminal_runs = Arc::clone(&self.terminal_runs);
        let deferred_channel_target = deferred_channel_target.clone();

        let handle = tokio::spawn(async move {
            let permit = permit; // hold permit until agent run completes
            let ctx_ref = project_context.as_deref();
            if let Some(target) = deferred_channel_target {
                // Register the channel reply target only after we own the
                // session permit, so queued messages keep per-message routing.
                state.push_channel_reply(&session_key_clone, target).await;
            }
            active_reply_medium
                .write()
                .await
                .insert(session_key_clone.clone(), desired_reply_medium);
            active_partial_assistant.write().await.insert(
                session_key_clone.clone(),
                ActiveAssistantDraft::new(&run_id_clone, &model_id, &provider_name, client_seq),
            );
            if desired_reply_medium == ReplyMedium::Voice {
                broadcast(
                    &state,
                    "chat",
                    serde_json::json!({
                        "runId": run_id_clone,
                        "sessionKey": session_key_clone,
                        "state": "voice_pending",
                    }),
                    BroadcastOpts::default(),
                )
                .await;
            }
            let agent_fut = async {
                if stream_only {
                    run_streaming(
                        persona,
                        &state,
                        &model_store,
                        &run_id_clone,
                        provider,
                        &model_id,
                        &user_content,
                        &provider_name,
                        &history,
                        &session_key_clone,
                        &session_agent_id_clone,
                        desired_reply_medium,
                        ctx_ref,
                        user_message_index,
                        &discovered_skills,
                        Some(&runtime_context),
                        Some(&session_store),
                        client_seq,
                        Some(Arc::clone(&active_partial_assistant)),
                        &terminal_runs,
                    )
                    .await
                } else {
                    run_with_tools(
                        persona,
                        &state,
                        &model_store,
                        &run_id_clone,
                        provider,
                        &model_id,
                        &tool_registry,
                        &user_content,
                        &provider_name,
                        &history,
                        &session_key_clone,
                        &session_agent_id_clone,
                        desired_reply_medium,
                        ctx_ref,
                        Some(&runtime_context),
                        user_message_index,
                        &discovered_skills,
                        hook_registry,
                        accept_language.clone(),
                        conn_id.clone(),
                        Some(&session_store),
                        mcp_disabled,
                        client_seq,
                        Some(Arc::clone(&active_thinking_text)),
                        Some(Arc::clone(&active_tool_calls)),
                        Some(Arc::clone(&active_partial_assistant)),
                        &active_event_forwarders,
                        &terminal_runs,
                    )
                    .await
                }
            };

            let assistant_text = if agent_timeout_secs > 0 {
                match tokio::time::timeout(Duration::from_secs(agent_timeout_secs), agent_fut).await
                {
                    Ok(result) => result,
                    Err(_) => {
                        warn!(
                            run_id = %run_id_clone,
                            session = %session_key_clone,
                            timeout_secs = agent_timeout_secs,
                            "agent run timed out"
                        );
                        let detail = format!("Agent run timed out after {agent_timeout_secs}s");
                        let error_obj = serde_json::json!({
                            "type": "timeout",
                            "title": "Timed out",
                            "detail": detail,
                        });
                        state.set_run_error(&run_id_clone, detail.clone()).await;
                        deliver_channel_error(&state, &session_key_clone, &error_obj).await;
                        terminal_runs.write().await.insert(run_id_clone.clone());
                        broadcast(
                            &state,
                            "chat",
                            serde_json::json!({
                                "runId": run_id_clone,
                                "sessionKey": session_key_clone,
                                "state": "error",
                                "error": error_obj,
                            }),
                            BroadcastOpts::default(),
                        )
                        .await;
                        None
                    },
                }
            } else {
                agent_fut.await
            };

            // Persist assistant response (even empty ones — needed for LLM history coherence).
            if let Some(assistant_output) = assistant_text {
                let assistant_msg = PersistedMessage::Assistant {
                    content: assistant_output.text,
                    created_at: Some(now_ms()),
                    model: Some(model_id.clone()),
                    provider: Some(provider_name.clone()),
                    input_tokens: Some(assistant_output.input_tokens),
                    output_tokens: Some(assistant_output.output_tokens),
                    duration_ms: Some(assistant_output.duration_ms),
                    request_input_tokens: Some(assistant_output.request_input_tokens),
                    request_output_tokens: Some(assistant_output.request_output_tokens),
                    tool_calls: None,
                    reasoning: assistant_output.reasoning,
                    llm_api_response: assistant_output.llm_api_response,
                    audio: assistant_output.audio_path,
                    seq: client_seq,
                    run_id: Some(run_id_clone.clone()),
                };
                if let Err(e) = session_store
                    .append(&session_key_clone, &assistant_msg.to_value())
                    .await
                {
                    warn!("failed to persist assistant message: {e}");
                }
                // Update metadata counts.
                if let Ok(count) = session_store.count(&session_key_clone).await {
                    session_metadata.touch(&session_key_clone, count).await;
                }
            }

            let _ = LiveChatService::wait_for_event_forwarder(
                &active_event_forwarders,
                &session_key_clone,
            )
            .await;

            active_runs.write().await.remove(&run_id_clone);
            let mut runs_by_session = active_runs_by_session.write().await;
            if runs_by_session.get(&session_key_clone) == Some(&run_id_clone) {
                runs_by_session.remove(&session_key_clone);
            }
            drop(runs_by_session);
            active_thinking_text
                .write()
                .await
                .remove(&session_key_clone);
            active_tool_calls.write().await.remove(&session_key_clone);
            terminal_runs.write().await.remove(&run_id_clone);
            active_partial_assistant
                .write()
                .await
                .remove(&session_key_clone);
            active_reply_medium.write().await.remove(&session_key_clone);

            // Release the semaphore *before* draining so replayed sends can
            // acquire it. Without this, every replayed `chat.send()` would
            // fail `try_acquire_owned()` and re-queue the message forever.
            drop(permit);

            // Drain queued messages for this session.
            let queued = message_queue
                .write()
                .await
                .remove(&session_key_clone)
                .unwrap_or_default();
            if !queued.is_empty() {
                let queue_mode = moltis_config::discover_and_load().chat.message_queue_mode;
                let chat = state_for_drain.chat_service().await;
                match queue_mode {
                    MessageQueueMode::Followup => {
                        let mut iter = queued.into_iter();
                        let Some(first) = iter.next() else {
                            return;
                        };
                        // Put remaining messages back so the replayed run's
                        // own drain loop picks them up after it completes.
                        let rest: Vec<QueuedMessage> = iter.collect();
                        if !rest.is_empty() {
                            message_queue
                                .write()
                                .await
                                .entry(session_key_clone.clone())
                                .or_default()
                                .extend(rest);
                        }
                        info!(session = %session_key_clone, "replaying queued message (followup)");
                        let mut replay_params = first.params;
                        replay_params["_queued_replay"] = serde_json::json!(true);
                        if let Err(e) = chat.send(replay_params).await {
                            warn!(session = %session_key_clone, error = %e, "failed to replay queued message");
                        }
                    },
                    MessageQueueMode::Collect => {
                        let combined: Vec<&str> = queued
                            .iter()
                            .filter_map(|m| m.params.get("text").and_then(|v| v.as_str()))
                            .collect();
                        if !combined.is_empty() {
                            info!(
                                session = %session_key_clone,
                                count = combined.len(),
                                "replaying collected messages"
                            );
                            // Use the last queued message as the base params, override text.
                            let Some(last) = queued.last() else {
                                return;
                            };
                            let mut merged = last.params.clone();
                            merged["text"] = serde_json::json!(combined.join("\n\n"));
                            merged["_queued_replay"] = serde_json::json!(true);
                            if let Err(e) = chat.send(merged).await {
                                warn!(session = %session_key_clone, error = %e, "failed to replay collected messages");
                            }
                        }
                    },
                }
            }
        });

        self.active_runs
            .write()
            .await
            .insert(run_id.clone(), handle.abort_handle());
        self.active_runs_by_session
            .write()
            .await
            .insert(session_key.clone(), run_id.clone());

        Ok(serde_json::json!({ "ok": true, "runId": run_id }))
    }

    async fn send_sync(&self, params: Value) -> ServiceResult {
        let text = params
            .get("text")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing 'text' parameter".to_string())?
            .to_string();
        let desired_reply_medium = infer_reply_medium(&params, &text);
        let requested_agent_id = params
            .get("agent_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let request_tool_policy = params
            .get("_tool_policy")
            .cloned()
            .map(serde_json::from_value::<ToolPolicy>)
            .transpose()
            .map_err(|e| format!("invalid '_tool_policy' parameter: {e}"))?;

        let explicit_model = params.get("model").and_then(|v| v.as_str());
        let stream_only = !self.has_tools_sync();

        // Resolve session key from explicit override.
        let session_key = match params.get("_session_key").and_then(|v| v.as_str()) {
            Some(sk) => sk.to_string(),
            None => "main".to_string(),
        };

        // Resolve provider.
        let provider: Arc<dyn moltis_agents::model::LlmProvider> = {
            let reg = self.providers.read().await;
            if let Some(id) = explicit_model {
                reg.get(id)
                    .ok_or_else(|| format!("model '{id}' not found"))?
            } else if !stream_only {
                reg.first_with_tools()
                    .ok_or_else(|| "no LLM providers configured".to_string())?
            } else {
                reg.first()
                    .ok_or_else(|| "no LLM providers configured".to_string())?
            }
        };

        let user_audio = user_audio_path_from_params(&params, &session_key);
        let user_documents =
            user_documents_from_params(&params, &session_key, self.session_store.as_ref());
        // Persist the user message.
        let user_msg = PersistedMessage::User {
            content: MessageContent::Text(text.clone()),
            created_at: Some(now_ms()),
            audio: user_audio,
            documents: user_documents
                .as_deref()
                .and_then(user_documents_for_persistence),
            channel: None,
            seq: None,
            run_id: None,
        };
        if let Err(e) = self
            .session_store
            .append(&session_key, &user_msg.to_value())
            .await
        {
            warn!("send_sync: failed to persist user message: {e}");
        }

        // Ensure this session appears in the sessions list.
        let _ = self.session_metadata.upsert(&session_key, None).await;
        if let Some(agent_id) = requested_agent_id.as_deref()
            && let Err(error) = self
                .session_metadata
                .set_agent_id(&session_key, Some(agent_id))
                .await
        {
            warn!(
                session = %session_key,
                agent_id,
                error = %error,
                "send_sync: failed to assign requested agent to session"
            );
        }
        self.session_metadata.touch(&session_key, 1).await;

        let session_entry = self.session_metadata.get(&session_key).await;
        let session_agent_id = resolve_prompt_agent_id(session_entry.as_ref());
        let persona = load_prompt_persona_for_session(
            &session_key,
            session_entry.as_ref(),
            self.session_state_store.as_deref(),
        )
        .await;
        let mut runtime_context = build_prompt_runtime_context(
            &self.state,
            &provider,
            &session_key,
            session_entry.as_ref(),
        )
        .await;
        apply_request_runtime_context(&mut runtime_context.host, &params);

        // Load conversation history (excluding the message we just appended).
        let mut history = self
            .session_store
            .read(&session_key)
            .await
            .unwrap_or_default();
        if !history.is_empty() {
            history.pop();
        }

        let run_id = uuid::Uuid::new_v4().to_string();
        let state = Arc::clone(&self.state);
        let tool_registry = if let Some(policy) = request_tool_policy.as_ref() {
            let registry_guard = self.tool_registry.read().await;
            Arc::new(RwLock::new(
                registry_guard.clone_allowed_by(|name| policy.is_allowed(name)),
            ))
        } else {
            Arc::clone(&self.tool_registry)
        };
        let hook_registry = self.hook_registry.clone();
        let provider_name = provider.name().to_string();
        let model_id = provider.id().to_string();
        let model_store = Arc::clone(&self.model_store);
        let user_message_index = history.len();

        info!(
            run_id = %run_id,
            user_message = %text,
            model = %model_id,
            stream_only,
            session = %session_key,
            reply_medium = ?desired_reply_medium,
            "chat.send_sync"
        );

        if desired_reply_medium == ReplyMedium::Voice {
            broadcast(
                &state,
                "chat",
                serde_json::json!({
                    "runId": run_id,
                    "sessionKey": session_key,
                    "state": "voice_pending",
                }),
                BroadcastOpts::default(),
            )
            .await;
        }

        // send_sync is text-only (used by API calls and channels).
        let user_content = UserContent::text(&text);
        let active_event_forwarders = Arc::new(RwLock::new(HashMap::new()));
        let terminal_runs = Arc::new(RwLock::new(HashSet::new()));
        let result = if stream_only {
            run_streaming(
                persona,
                &state,
                &model_store,
                &run_id,
                provider,
                &model_id,
                &user_content,
                &provider_name,
                &history,
                &session_key,
                &session_agent_id,
                desired_reply_medium,
                None,
                user_message_index,
                &[],
                Some(&runtime_context),
                Some(&self.session_store),
                None, // send_sync: no client seq
                None, // send_sync: no partial assistant tracking
                &terminal_runs,
            )
            .await
        } else {
            run_with_tools(
                persona,
                &state,
                &model_store,
                &run_id,
                provider,
                &model_id,
                &tool_registry,
                &user_content,
                &provider_name,
                &history,
                &session_key,
                &session_agent_id,
                desired_reply_medium,
                None,
                Some(&runtime_context),
                user_message_index,
                &[],
                hook_registry,
                None,
                None, // send_sync: no conn_id
                Some(&self.session_store),
                false, // send_sync: MCP tools always enabled for API calls
                None,  // send_sync: no client seq
                None,  // send_sync: no thinking text tracking
                None,  // send_sync: no tool call tracking
                None,  // send_sync: no partial assistant tracking
                &active_event_forwarders,
                &terminal_runs,
            )
            .await
        };

        // Persist assistant response (even empty ones — needed for LLM history coherence).
        if let Some(ref assistant_output) = result {
            let assistant_msg = PersistedMessage::Assistant {
                content: assistant_output.text.clone(),
                created_at: Some(now_ms()),
                model: Some(model_id.clone()),
                provider: Some(provider_name.clone()),
                input_tokens: Some(assistant_output.input_tokens),
                output_tokens: Some(assistant_output.output_tokens),
                duration_ms: Some(assistant_output.duration_ms),
                request_input_tokens: Some(assistant_output.request_input_tokens),
                request_output_tokens: Some(assistant_output.request_output_tokens),
                tool_calls: None,
                reasoning: assistant_output.reasoning.clone(),
                llm_api_response: assistant_output.llm_api_response.clone(),
                audio: assistant_output.audio_path.clone(),
                seq: None,
                run_id: Some(run_id.clone()),
            };
            if let Err(e) = self
                .session_store
                .append(&session_key, &assistant_msg.to_value())
                .await
            {
                warn!("send_sync: failed to persist assistant message: {e}");
            }
            // Update metadata message count.
            if let Ok(count) = self.session_store.count(&session_key).await {
                self.session_metadata.touch(&session_key, count).await;
            }
        }

        match result {
            Some(assistant_output) => Ok(serde_json::json!({
                "text": assistant_output.text,
                "inputTokens": assistant_output.input_tokens,
                "outputTokens": assistant_output.output_tokens,
                "durationMs": assistant_output.duration_ms,
                "requestInputTokens": assistant_output.request_input_tokens,
                "requestOutputTokens": assistant_output.request_output_tokens,
            })),
            None => {
                // Check the last broadcast for this run to get the actual error message.
                let error_msg = state
                    .last_run_error(&run_id)
                    .await
                    .unwrap_or_else(|| "agent run failed (check server logs)".to_string());

                // Persist the error in the session so it's visible in session history.
                let error_entry = PersistedMessage::system(format!("[error] {error_msg}"));
                let _ = self
                    .session_store
                    .append(&session_key, &error_entry.to_value())
                    .await;
                // Update metadata so the session shows in the UI.
                if let Ok(count) = self.session_store.count(&session_key).await {
                    self.session_metadata.touch(&session_key, count).await;
                }

                Err(error_msg.into())
            },
        }
    }

    async fn abort(&self, params: Value) -> ServiceResult {
        let run_id = params.get("runId").and_then(|v| v.as_str());
        let session_key = params.get("sessionKey").and_then(|v| v.as_str());
        if run_id.is_none() && session_key.is_none() {
            return Err("missing 'runId' or 'sessionKey'".into());
        }

        let resolved_session_key =
            Self::resolve_session_key_for_run(&self.active_runs_by_session, run_id, session_key)
                .await;

        let (resolved_run_id, aborted) = Self::abort_run_handle(
            &self.active_runs,
            &self.active_runs_by_session,
            &self.terminal_runs,
            run_id,
            session_key,
        )
        .await;
        info!(
            requested_run_id = ?run_id,
            session_key = ?session_key,
            resolved_run_id = ?resolved_run_id,
            aborted,
            "chat.abort"
        );

        if aborted && let Some(key) = resolved_session_key.as_deref() {
            let _ = Self::wait_for_event_forwarder(&self.active_event_forwarders, key).await;
            let partial = self.persist_partial_assistant_on_abort(key).await;
            self.active_thinking_text.write().await.remove(key);
            self.active_tool_calls.write().await.remove(key);
            self.active_reply_medium.write().await.remove(key);
            let mut payload = serde_json::json!({
                "state": "aborted",
                "runId": resolved_run_id,
                "sessionKey": key,
            });
            if let Some((partial_message, message_index)) = partial {
                payload["partialMessage"] = partial_message;
                if let Some(index) = message_index {
                    payload["messageIndex"] = serde_json::json!(index);
                }
            }
            broadcast(&self.state, "chat", payload, BroadcastOpts::default()).await;
        }

        Ok(serde_json::json!({
            "aborted": aborted,
            "runId": resolved_run_id,
            "sessionKey": resolved_session_key,
        }))
    }

    async fn cancel_queued(&self, params: Value) -> ServiceResult {
        let session_key = params
            .get("sessionKey")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing 'sessionKey'".to_string())?;

        let removed = self
            .message_queue
            .write()
            .await
            .remove(session_key)
            .unwrap_or_default();
        let count = removed.len();
        info!(session = %session_key, count, "cancel_queued: cleared message queue");

        broadcast(
            &self.state,
            "chat",
            serde_json::json!({
                "sessionKey": session_key,
                "state": "queue_cleared",
                "count": count,
            }),
            BroadcastOpts::default(),
        )
        .await;

        Ok(serde_json::json!({ "cleared": count }))
    }

    async fn history(&self, params: Value) -> ServiceResult {
        let session_key = self.resolve_session_key_from_params(&params).await;
        let messages = self
            .session_store
            .read(&session_key)
            .await
            .map_err(ServiceError::message)?;
        // Filter out empty assistant messages — they are kept in storage for LLM
        // history coherence but should not be shown in the UI.
        let visible: Vec<Value> = messages
            .into_iter()
            .filter(assistant_message_is_visible)
            .collect();
        Ok(serde_json::json!(visible))
    }

    async fn inject(&self, _params: Value) -> ServiceResult {
        Err("inject not yet implemented".into())
    }

    async fn clear(&self, params: Value) -> ServiceResult {
        let session_key = self.resolve_session_key_from_params(&params).await;

        self.session_store
            .clear(&session_key)
            .await
            .map_err(ServiceError::message)?;

        // Reset client sequence tracking for this session. A cleared chat starts
        // a fresh sequence from the web UI.
        {
            let mut seq_map = self.last_client_seq.write().await;
            seq_map.remove(&session_key);
        }

        // Reset metadata message count and preview.
        self.session_metadata.touch(&session_key, 0).await;
        self.session_metadata.set_preview(&session_key, None).await;

        // Notify all WebSocket clients so the web UI clears the session
        // even when /clear is issued from a channel (e.g. Telegram).
        broadcast(
            &self.state,
            "chat",
            serde_json::json!({
                "sessionKey": session_key,
                "state": "session_cleared",
            }),
            BroadcastOpts::default(),
        )
        .await;

        info!(session = %session_key, "chat.clear");
        Ok(serde_json::json!({ "ok": true }))
    }

    async fn compact(&self, params: Value) -> ServiceResult {
        let session_key = self.resolve_session_key_from_params(&params).await;
        let session_entry = self.session_metadata.get(&session_key).await;
        let session_agent_id = resolve_prompt_agent_id(session_entry.as_ref());

        let history = self
            .session_store
            .read(&session_key)
            .await
            .map_err(ServiceError::message)?;

        if history.is_empty() {
            return Err("nothing to compact".into());
        }

        // Dispatch BeforeCompaction hook.
        if let Some(ref hooks) = self.hook_registry {
            let payload = moltis_common::hooks::HookPayload::BeforeCompaction {
                session_key: session_key.clone(),
                message_count: history.len(),
            };
            if let Err(e) = hooks.dispatch(&payload).await {
                warn!(session = %session_key, error = %e, "BeforeCompaction hook failed");
            }
        }

        // Run silent memory turn before summarization — saves important memories to disk.
        // The manager implements MemoryWriter directly (with path validation, size limits,
        // and automatic re-indexing), so no manual sync_path is needed after the turn.
        if let Some(mm) = self.state.memory_manager()
            && let Ok(provider) = self.resolve_provider(&session_key, &history).await
        {
            let write_mode = moltis_config::discover_and_load().memory.agent_write_mode;
            if !memory_write_mode_allows_save(write_mode) {
                debug!(
                    "compact: agent-authored memory writes disabled, skipping silent memory turn"
                );
            } else {
                let chat_history_for_memory = values_to_chat_messages(&history);
                let writer: Arc<dyn moltis_agents::memory_writer::MemoryWriter> =
                    Arc::new(AgentScopedMemoryWriter::new(
                        Arc::clone(mm),
                        session_agent_id.clone(),
                        write_mode,
                    ));
                match moltis_agents::silent_turn::run_silent_memory_turn(
                    provider,
                    &chat_history_for_memory,
                    writer,
                )
                .await
                {
                    Ok(paths) => {
                        if !paths.is_empty() {
                            info!(
                                files = paths.len(),
                                "compact: silent memory turn wrote files"
                            );
                        }
                    },
                    Err(e) => warn!(error = %e, "compact: silent memory turn failed"),
                }
            }
        }

        // Resolve the session persona so we can pick up the compaction config
        // and provide a provider to LLM-backed compaction modes. Agent-scoped
        // config falls back through `load_prompt_persona_for_agent`'s default
        // path, so this is safe even when the session has no custom preset.
        let persona = load_prompt_persona_for_agent(&session_agent_id);
        let compaction_config = &persona.config.chat.compaction;

        // LLM-backed modes need a resolved provider. Deterministic mode
        // ignores it, so resolution failures are only fatal for the other
        // modes — and `run_compaction` returns a clear ProviderRequired
        // error in that case.
        let provider_arc = self.resolve_provider(&session_key, &history).await.ok();

        let outcome =
            compaction_run::run_compaction(&history, compaction_config, provider_arc.as_deref())
                .await
                .map_err(|e| ServiceError::message(e.to_string()))?;

        let compacted = outcome.history.clone();

        // Keep a plain-text copy of the summary so the memory-file snapshot
        // below can still record what we compacted to. The helper walks the
        // compacted history because recency_preserving / structured modes
        // splice head and tail messages around the summary — it isn't
        // necessarily compacted[0].
        let summary_for_memory = compaction_run::extract_summary_body(&compacted);

        info!(
            session = %session_key,
            requested_mode = ?compaction_config.mode,
            effective_mode = ?outcome.effective_mode,
            input_tokens = outcome.input_tokens,
            output_tokens = outcome.output_tokens,
            messages = history.len(),
            "chat.compact: strategy dispatched"
        );

        // Enforce summary budget discipline: max 1,200 chars, 24 lines,
        // 160 chars/line.  Mutate the compacted history in place so the
        // compressed text is what gets persisted and broadcast.
        let compacted = compress_summary_in_history(compacted);

        // Replace the session history BEFORE broadcasting or notifying
        // channels. If we did it the other way around, a concurrent
        // `send()` RPC that landed between the broadcast and the store
        // update would see the stale history and the client UI would
        // already believe compaction had finished — a narrow but real
        // race window flagged by Greptile on commit 0714de07.
        self.session_store
            .replace_history(&session_key, compacted.clone())
            .await
            .map_err(ServiceError::message)?;

        self.session_metadata.touch(&session_key, 1).await;

        // Broadcast a chat.compact-scoped "done" event so UI consumers see
        // the effective mode and token usage even when compaction is
        // triggered manually via the RPC (the auto-compact path broadcasts
        // separately around `send()`). The settings hint is included only
        // when the user hasn't opted out via chat.compaction.show_settings_hint.
        //
        // Include `totalTokens` / `contextWindow` on this payload so the
        // web UI's compact card can render a full "Before compact"
        // section even when this event fires first in `send()`'s
        // pre-emptive auto-compact path. Without these fields the card
        // was rendering without the "Total tokens" and "Context usage"
        // rows on that path.
        let show_hint = compaction_config.show_settings_hint;
        let pre_compact_total_tokens: u32 = history
            .iter()
            .filter_map(|m| m.get("content").and_then(Value::as_str))
            .map(|text| u32::try_from(estimate_text_tokens(text)).unwrap_or(u32::MAX))
            .sum();
        let context_window = provider_arc.as_deref().map(|p| p.context_window());
        let mut compact_payload = serde_json::json!({
            "sessionKey": session_key,
            "state": "compact",
            "phase": "done",
            "messageCount": history.len(),
            "totalTokens": pre_compact_total_tokens,
        });
        if let Some(window) = context_window
            && let Some(obj) = compact_payload.as_object_mut()
        {
            obj.insert("contextWindow".to_string(), serde_json::json!(window));
        }
        if let (Some(obj), Some(meta)) = (
            compact_payload.as_object_mut(),
            outcome.broadcast_metadata(show_hint).as_object().cloned(),
        ) {
            obj.extend(meta);
        }
        broadcast(
            &self.state,
            "chat",
            compact_payload,
            BroadcastOpts::default(),
        )
        .await;

        // Notify any channel (Telegram, Discord, Matrix, WhatsApp, etc.)
        // that has pending reply targets on this session, so channel
        // users see "Conversation compacted (mode, tokens, hint)"
        // alongside the web UI's compact card.
        notify_channels_of_compaction(&self.state, &session_key, &outcome, show_hint).await;

        // Save compaction summary to memory file and trigger sync.
        if let Some(mm) = self.state.memory_manager() {
            let memory_dir = moltis_config::agent_workspace_dir(&session_agent_id).join("memory");
            if let Err(e) = tokio::fs::create_dir_all(&memory_dir).await {
                warn!(error = %e, "compact: failed to create memory dir");
            } else {
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let filename = format!("compaction-{}-{ts}.md", session_key);
                let path = memory_dir.join(&filename);
                let content = format!(
                    "# Compaction Summary\n\n- **Session**: {session_key}\n- **Timestamp**: {ts}\n\n{summary_for_memory}"
                );
                if let Err(e) = tokio::fs::write(&path, &content).await {
                    warn!(error = %e, "compact: failed to write memory file");
                } else {
                    let mm = Arc::clone(mm);
                    tokio::spawn(async move {
                        if let Err(e) = mm.sync().await {
                            tracing::warn!("compact: memory sync failed: {e}");
                        }
                    });
                }
            }
        }

        // Dispatch AfterCompaction hook.
        if let Some(ref hooks) = self.hook_registry {
            let payload = moltis_common::hooks::HookPayload::AfterCompaction {
                session_key: session_key.clone(),
                summary_len: summary_for_memory.len(),
            };
            if let Err(e) = hooks.dispatch(&payload).await {
                warn!(session = %session_key, error = %e, "AfterCompaction hook failed");
            }
        }

        info!(session = %session_key, "chat.compact: done");
        Ok(serde_json::json!(compacted))
    }

    async fn context(&self, params: Value) -> ServiceResult {
        let session_key = self.resolve_session_key_from_params(&params).await;

        // Session info
        let message_count = self.session_store.count(&session_key).await.unwrap_or(0);
        let session_entry = self.session_metadata.get(&session_key).await;
        let prompt_persona = load_prompt_persona_for_session(
            &session_key,
            session_entry.as_ref(),
            self.session_state_store.as_deref(),
        )
        .await;
        let (provider_name, supports_tools) = {
            let reg = self.providers.read().await;
            let session_model = session_entry.as_ref().and_then(|e| e.model.as_deref());
            if let Some(id) = session_model {
                let p = reg.get(id);
                (
                    p.as_ref().map(|p| p.name().to_string()),
                    p.as_ref().map(|p| p.supports_tools()).unwrap_or(true),
                )
            } else {
                let p = reg.first();
                (
                    p.as_ref().map(|p| p.name().to_string()),
                    p.as_ref().map(|p| p.supports_tools()).unwrap_or(true),
                )
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
            self.state.active_project_id(cid).await
        } else {
            None
        };
        let project_id =
            project_id.or_else(|| session_entry.as_ref().and_then(|e| e.project_id.clone()));

        let project_info = if let Some(pid) = project_id {
            match self
                .state
                .project_service()
                .get(serde_json::json!({"id": pid}))
                .await
            {
                Ok(val) => {
                    let dir = val.get("directory").and_then(|v| v.as_str());
                    let context_files = if let Some(d) = dir {
                        match moltis_projects::context::load_context_files(Path::new(d)) {
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

        // Tools (only include if the provider supports tool calling)
        let mcp_disabled = session_entry
            .as_ref()
            .and_then(|e| e.mcp_disabled)
            .unwrap_or(false);
        let config = moltis_config::discover_and_load();
        let tools: Vec<Value> = if supports_tools {
            let registry_guard = self.tool_registry.read().await;
            let list_ctx = PolicyContext {
                agent_id: "main".into(),
                ..Default::default()
            };
            let effective_registry =
                apply_runtime_tool_filters(&registry_guard, &config, &[], mcp_disabled, &list_ctx);
            effective_registry
                .list_schemas()
                .iter()
                .map(|s| {
                    serde_json::json!({
                        "name": s.get("name").and_then(|v| v.as_str()).unwrap_or("unknown"),
                        "description": s.get("description").and_then(|v| v.as_str()).unwrap_or(""),
                    })
                })
                .collect()
        } else {
            vec![]
        };

        // Token usage from API-reported counts stored in messages.
        let messages = self
            .session_store
            .read(&session_key)
            .await
            .unwrap_or_default();
        let usage = session_token_usage_from_messages(&messages);
        let total_tokens = usage.session_input_tokens + usage.session_output_tokens;
        let current_total_tokens =
            usage.current_request_input_tokens + usage.current_request_output_tokens;

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
        let sandbox_info = if let Some(router) = self.state.sandbox_router() {
            let is_sandboxed = router.is_sandboxed(&session_key).await;
            let config = router.config();
            let session_image = session_entry.as_ref().and_then(|e| e.sandbox_image.clone());
            let effective_image = match session_image {
                Some(img) if !img.is_empty() => img,
                _ => router.default_image().await,
            };
            let container_name = {
                let id = router.sandbox_id_for(&session_key);
                format!(
                    "{}-{}",
                    config
                        .container_prefix
                        .as_deref()
                        .unwrap_or("moltis-sandbox"),
                    id.key
                )
            };
            serde_json::json!({
                "enabled": is_sandboxed,
                "backend": router.backend_name(),
                "mode": config.mode,
                "scope": config.scope,
                "workspaceMount": config.workspace_mount,
                "image": effective_image,
                "containerName": container_name,
            })
        } else {
            serde_json::json!({
                "enabled": false,
                "backend": null,
            })
        };
        let sandbox_enabled = sandbox_info
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let host_is_root = detect_host_root_user().await;
        // Sandbox containers currently run as root by default.
        let exec_is_root = if sandbox_enabled {
            Some(true)
        } else {
            host_is_root
        };
        let exec_prompt_symbol = exec_is_root.map(|is_root| {
            if is_root {
                "#"
            } else {
                "$"
            }
        });
        let execution_info = serde_json::json!({
            "mode": if sandbox_enabled { "sandbox" } else { "host" },
            "hostIsRoot": host_is_root,
            "isRoot": exec_is_root,
            "promptSymbol": exec_prompt_symbol,
        });

        // Discover enabled skills/plugins (only if provider supports tools and
        // `[skills] enabled` is true — see #655).
        let skills_list: Vec<Value> = if supports_tools {
            discover_skills_if_enabled(&config)
                .await
                .iter()
                .map(|s| {
                    serde_json::json!({
                        "name": s.name,
                        "description": s.description,
                        "source": s.source,
                    })
                })
                .collect()
        } else {
            vec![]
        };

        // MCP servers (only if provider supports tools)
        let mcp_servers = if supports_tools {
            self.state
                .mcp_service()
                .list()
                .await
                .unwrap_or(serde_json::json!([]))
        } else {
            serde_json::json!([])
        };

        Ok(serde_json::json!({
            "session": session_info,
            "project": project_info,
            "tools": tools,
            "skills": skills_list,
            "mcpServers": mcp_servers,
            "mcpDisabled": mcp_disabled,
            "sandbox": sandbox_info,
            "execution": execution_info,
            "promptMemory": prompt_persona.memory_status,
            "supportsTools": supports_tools,
            "tokenUsage": {
                "inputTokens": usage.session_input_tokens,
                "outputTokens": usage.session_output_tokens,
                "total": total_tokens,
                "currentInputTokens": usage.current_request_input_tokens,
                "currentOutputTokens": usage.current_request_output_tokens,
                "currentTotal": current_total_tokens,
                "estimatedNextInputTokens": usage.current_request_input_tokens,
                "contextWindow": context_window,
            },
        }))
    }

    async fn raw_prompt(&self, params: Value) -> ServiceResult {
        let session_key = self.resolve_session_key_from_params(&params).await;

        let conn_id = params
            .get("_conn_id")
            .and_then(|v| v.as_str())
            .map(String::from);

        // Resolve provider.
        let history = self
            .session_store
            .read(&session_key)
            .await
            .unwrap_or_default();
        let provider = self
            .resolve_provider(&session_key, &history)
            .await
            .map_err(ServiceError::message)?;
        let tool_mode = effective_tool_mode(&*provider);
        let native_tools = matches!(tool_mode, ToolMode::Native);
        let tools_enabled = !matches!(tool_mode, ToolMode::Off);

        // Build runtime context.
        let session_entry = self.session_metadata.get(&session_key).await;
        let persona = load_prompt_persona_for_session(
            &session_key,
            session_entry.as_ref(),
            self.session_state_store.as_deref(),
        )
        .await;
        let mut runtime_context = build_prompt_runtime_context(
            &self.state,
            &provider,
            &session_key,
            session_entry.as_ref(),
        )
        .await;
        apply_request_runtime_context(&mut runtime_context.host, &params);

        // Resolve project context.
        let project_context = self
            .resolve_project_context(&session_key, conn_id.as_deref())
            .await;

        // Discover skills (gated on `[skills] enabled` — see #655).
        let discovered_skills = discover_skills_if_enabled(&persona.config).await;

        // Check MCP disabled.
        let mcp_disabled = session_entry
            .as_ref()
            .and_then(|entry| entry.mcp_disabled)
            .unwrap_or(false);

        // Build filtered tool registry.
        let policy_ctx = build_policy_context("main", Some(&runtime_context), Some(&params));
        let filtered_registry = {
            let registry_guard = self.tool_registry.read().await;
            if tools_enabled {
                apply_runtime_tool_filters(
                    &registry_guard,
                    &persona.config,
                    &discovered_skills,
                    mcp_disabled,
                    &policy_ctx,
                )
            } else {
                registry_guard.clone_without(&[])
            }
        };

        let tool_count = filtered_registry.list_schemas().len();

        // Build the system prompt.
        let prompt_limits = prompt_build_limits_from_config(&persona.config);
        let prompt_build = if tools_enabled {
            build_system_prompt_with_session_runtime_details(
                &filtered_registry,
                native_tools,
                project_context.as_deref(),
                &discovered_skills,
                Some(&persona.identity),
                Some(&persona.user),
                persona.soul_text.as_deref(),
                persona.boot_text.as_deref(),
                persona.agents_text.as_deref(),
                persona.tools_text.as_deref(),
                Some(&runtime_context),
                persona.memory_text.as_deref(),
                prompt_limits,
            )
        } else {
            build_system_prompt_minimal_runtime_details(
                project_context.as_deref(),
                Some(&persona.identity),
                Some(&persona.user),
                persona.soul_text.as_deref(),
                persona.boot_text.as_deref(),
                persona.agents_text.as_deref(),
                persona.tools_text.as_deref(),
                Some(&runtime_context),
                persona.memory_text.as_deref(),
                prompt_limits,
            )
        };

        let truncated = prompt_build.metadata.truncated();
        let workspace_files = prompt_build.metadata.workspace_files.clone();
        let system_prompt = prompt_build.prompt;
        let char_count = system_prompt.len();

        Ok(serde_json::json!({
            "prompt": system_prompt,
            "charCount": char_count,
            "truncated": truncated,
            "workspaceFiles": workspace_files,
            "promptMemory": persona.memory_status,
            "native_tools": native_tools,
            "tools_enabled": tools_enabled,
            "tool_mode": format!("{:?}", tool_mode),
            "toolCount": tool_count,
        }))
    }

    /// Return the **full messages array** that would be sent to the LLM on the
    /// next call — system prompt + conversation history — in OpenAI format.
    async fn full_context(&self, params: Value) -> ServiceResult {
        let session_key = self.resolve_session_key_from_params(&params).await;

        let conn_id = params
            .get("_conn_id")
            .and_then(|v| v.as_str())
            .map(String::from);

        // Resolve provider.
        let history = self
            .session_store
            .read(&session_key)
            .await
            .unwrap_or_default();
        let provider = self
            .resolve_provider(&session_key, &history)
            .await
            .map_err(ServiceError::message)?;
        let tool_mode = effective_tool_mode(&*provider);
        let native_tools = matches!(tool_mode, ToolMode::Native);
        let tools_enabled = !matches!(tool_mode, ToolMode::Off);

        // Build runtime context.
        let session_entry = self.session_metadata.get(&session_key).await;
        let persona = load_prompt_persona_for_session(
            &session_key,
            session_entry.as_ref(),
            self.session_state_store.as_deref(),
        )
        .await;
        let mut runtime_context = build_prompt_runtime_context(
            &self.state,
            &provider,
            &session_key,
            session_entry.as_ref(),
        )
        .await;
        apply_request_runtime_context(&mut runtime_context.host, &params);

        // Resolve project context.
        let project_context = self
            .resolve_project_context(&session_key, conn_id.as_deref())
            .await;

        // Discover skills (gated on `[skills] enabled` — see #655).
        let discovered_skills = discover_skills_if_enabled(&persona.config).await;

        // Check MCP disabled.
        let mcp_disabled = session_entry
            .as_ref()
            .and_then(|entry| entry.mcp_disabled)
            .unwrap_or(false);

        // Build filtered tool registry.
        let policy_ctx = build_policy_context("main", Some(&runtime_context), Some(&params));
        let filtered_registry = {
            let registry_guard = self.tool_registry.read().await;
            if tools_enabled {
                apply_runtime_tool_filters(
                    &registry_guard,
                    &persona.config,
                    &discovered_skills,
                    mcp_disabled,
                    &policy_ctx,
                )
            } else {
                registry_guard.clone_without(&[])
            }
        };

        // Build the system prompt.
        let prompt_limits = prompt_build_limits_from_config(&persona.config);
        let prompt_build = if tools_enabled {
            build_system_prompt_with_session_runtime_details(
                &filtered_registry,
                native_tools,
                project_context.as_deref(),
                &discovered_skills,
                Some(&persona.identity),
                Some(&persona.user),
                persona.soul_text.as_deref(),
                persona.boot_text.as_deref(),
                persona.agents_text.as_deref(),
                persona.tools_text.as_deref(),
                Some(&runtime_context),
                persona.memory_text.as_deref(),
                prompt_limits,
            )
        } else {
            build_system_prompt_minimal_runtime_details(
                project_context.as_deref(),
                Some(&persona.identity),
                Some(&persona.user),
                persona.soul_text.as_deref(),
                persona.boot_text.as_deref(),
                persona.agents_text.as_deref(),
                persona.tools_text.as_deref(),
                Some(&runtime_context),
                persona.memory_text.as_deref(),
                prompt_limits,
            )
        };

        let truncated = prompt_build.metadata.truncated();
        let workspace_files = prompt_build.metadata.workspace_files.clone();
        let system_prompt = prompt_build.prompt;
        let system_prompt_chars = system_prompt.len();

        // Keep raw assistant outputs (including provider/model/token metadata)
        // so the UI can show a debug view of what the LLM actually returned.
        let llm_outputs: Vec<Value> = history
            .iter()
            .filter(|entry| entry.get("role").and_then(|r| r.as_str()) == Some("assistant"))
            .cloned()
            .collect();

        // Build the full messages array: system prompt + conversation history.
        // `values_to_chat_messages` handles `tool_result` → `tool` conversion.
        let mut messages = Vec::with_capacity(1 + history.len());
        messages.push(ChatMessage::system(system_prompt));
        messages.extend(values_to_chat_messages(&history));

        let openai_messages: Vec<Value> = messages.iter().map(|m| m.to_openai_value()).collect();
        let message_count = openai_messages.len();
        let total_chars: usize = openai_messages
            .iter()
            .map(|v| serde_json::to_string(v).unwrap_or_default().len())
            .sum();

        Ok(serde_json::json!({
            "messages": openai_messages,
            "llmOutputs": llm_outputs,
            "messageCount": message_count,
            "systemPromptChars": system_prompt_chars,
            "totalChars": total_chars,
            "truncated": truncated,
            "workspaceFiles": workspace_files,
            "promptMemory": persona.memory_status,
        }))
    }

    async fn refresh_prompt_memory(&self, params: Value) -> ServiceResult {
        let session_key = self.resolve_session_key_from_params(&params).await;
        let session_entry = self.session_metadata.get(&session_key).await;
        let agent_id = resolve_prompt_agent_id(session_entry.as_ref());
        let snapshot_cleared = clear_prompt_memory_snapshot(
            &session_key,
            &agent_id,
            self.session_state_store.as_deref(),
        )
        .await;
        let persona = load_prompt_persona_for_session(
            &session_key,
            session_entry.as_ref(),
            self.session_state_store.as_deref(),
        )
        .await;

        Ok(serde_json::json!({
            "ok": true,
            "sessionKey": session_key,
            "agentId": agent_id,
            "snapshotCleared": snapshot_cleared,
            "promptMemory": persona.memory_status,
        }))
    }

    async fn active(&self, params: Value) -> ServiceResult {
        let session_key = params
            .get("sessionKey")
            .or_else(|| params.get("session_key"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing 'sessionKey' parameter".to_string())?;
        let active = self
            .active_runs_by_session
            .read()
            .await
            .contains_key(session_key);
        Ok(serde_json::json!({ "active": active }))
    }

    async fn active_session_keys(&self) -> Vec<String> {
        self.active_runs_by_session
            .read()
            .await
            .keys()
            .cloned()
            .collect()
    }

    async fn active_thinking_text(&self, session_key: &str) -> Option<String> {
        self.active_thinking_text
            .read()
            .await
            .get(session_key)
            .cloned()
    }

    async fn active_voice_pending(&self, session_key: &str) -> bool {
        self.active_reply_medium
            .read()
            .await
            .get(session_key)
            .is_some_and(|m| *m == ReplyMedium::Voice)
    }

    async fn peek(&self, params: Value) -> ServiceResult {
        let session_key = params
            .get("sessionKey")
            .and_then(|v| v.as_str())
            .unwrap_or("main");

        let active = self
            .active_runs_by_session
            .read()
            .await
            .contains_key(session_key);

        if !active {
            return Ok(serde_json::json!({ "active": false }));
        }

        let thinking_text = self
            .active_thinking_text
            .read()
            .await
            .get(session_key)
            .cloned();

        let tool_calls: Vec<ActiveToolCall> = self
            .active_tool_calls
            .read()
            .await
            .get(session_key)
            .cloned()
            .unwrap_or_default();

        Ok(serde_json::json!({
            "active": true,
            "sessionKey": session_key,
            "thinkingText": thinking_text,
            "toolCalls": tool_calls,
        }))
    }
}

