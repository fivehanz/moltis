use std::{collections::HashMap, sync::Arc};

use {
    async_trait::async_trait,
    serde_json::Value,
    tokio::{sync::RwLock, task::AbortHandle},
    tokio_stream::StreamExt,
    tracing::{debug, info, warn},
};

use moltis_agents::{
    model::StreamEvent,
    prompt::build_system_prompt,
    providers::ProviderRegistry,
    runner::{RunnerEvent, run_agent_loop},
    tool_registry::ToolRegistry,
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
}

impl LiveChatService {
    pub fn new(providers: Arc<RwLock<ProviderRegistry>>, state: Arc<GatewayState>) -> Self {
        Self {
            providers,
            state,
            active_runs: Arc::new(RwLock::new(HashMap::new())),
            tool_registry: Arc::new(ToolRegistry::new()),
        }
    }

    pub fn with_tools(mut self, registry: ToolRegistry) -> Self {
        self.tool_registry = Arc::new(registry);
        self
    }

    fn has_tools(&self) -> bool {
        !self.tool_registry.list_schemas().is_empty()
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
            "chat.send"
        );

        let provider_name = provider.name().to_string();
        let handle = tokio::spawn(async move {
            if stream_only {
                // Streaming mode (no tools) — plain LLM text generation.
                run_streaming(&state, &run_id_clone, provider, &text, &provider_name).await;
            } else {
                // Agent loop mode: LLM + tool call execution loop.
                run_with_tools(
                    &state,
                    &run_id_clone,
                    provider,
                    &tool_registry,
                    &text,
                    &provider_name,
                )
                .await;
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

// ── Agent loop mode ─────────────────────────────────────────────────────────

async fn run_with_tools(
    state: &Arc<GatewayState>,
    run_id: &str,
    provider: Arc<dyn moltis_agents::model::LlmProvider>,
    tool_registry: &Arc<ToolRegistry>,
    text: &str,
    provider_name: &str,
) {
    let native_tools = provider.supports_tools();
    let system_prompt = build_system_prompt(tool_registry, native_tools);

    // Broadcast tool events to the UI as they happen.
    let state_for_events = Arc::clone(state);
    let run_id_for_events = run_id.to_string();
    let on_event: Box<dyn Fn(RunnerEvent) + Send + Sync> = Box::new(move |event| {
        let state = Arc::clone(&state_for_events);
        let run_id = run_id_for_events.clone();
        tokio::spawn(async move {
            let payload = match &event {
                RunnerEvent::Thinking => serde_json::json!({
                    "runId": run_id,
                    "state": "thinking",
                }),
                RunnerEvent::ThinkingDone => serde_json::json!({
                    "runId": run_id,
                    "state": "thinking_done",
                }),
                RunnerEvent::ToolCallStart {
                    id,
                    name,
                    arguments,
                } => serde_json::json!({
                    "runId": run_id,
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
                    "state": "delta",
                    "text": text,
                }),
                RunnerEvent::Iteration(n) => serde_json::json!({
                    "runId": run_id,
                    "state": "iteration",
                    "iteration": n,
                }),
            };
            broadcast(&state, "chat", payload, BroadcastOpts::default()).await;
        });
    });

    match run_agent_loop(
        provider,
        tool_registry,
        &system_prompt,
        text,
        Some(&on_event),
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
                    "state": "final",
                    "text": result.text,
                    "iterations": result.iterations,
                    "toolCallsMade": result.tool_calls_made,
                }),
                BroadcastOpts::default(),
            )
            .await;
        },
        Err(e) => {
            warn!(run_id, error = %e, "agent run error");
            let error_obj = parse_chat_error(&e.to_string(), Some(provider_name));
            broadcast(
                state,
                "chat",
                serde_json::json!({
                    "runId": run_id,
                    "state": "error",
                    "error": error_obj,
                }),
                BroadcastOpts::default(),
            )
            .await;
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
) {
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
                    state,
                    "chat",
                    serde_json::json!({
                        "runId": run_id,
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
                        "state": "final",
                        "text": accumulated,
                    }),
                    BroadcastOpts::default(),
                )
                .await;
                break;
            },
            StreamEvent::Error(msg) => {
                warn!(run_id, error = %msg, "chat stream error");
                let error_obj = parse_chat_error(&msg, Some(provider_name));
                broadcast(
                    state,
                    "chat",
                    serde_json::json!({
                        "runId": run_id,
                        "state": "error",
                        "error": error_obj,
                    }),
                    BroadcastOpts::default(),
                )
                .await;
                break;
            },
        }
    }
}
