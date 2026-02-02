use std::sync::Arc;

use {
    anyhow::Result,
    tracing::{debug, info, trace, warn},
};

use moltis_common::hooks::{HookAction, HookPayload, HookRegistry};

use crate::{
    model::{CompletionResponse, LlmProvider, ToolCall, Usage},
    tool_registry::ToolRegistry,
};

/// Maximum number of tool-call loop iterations before giving up.
const MAX_ITERATIONS: usize = 25;

/// Error patterns that indicate the context window has been exceeded.
const CONTEXT_WINDOW_PATTERNS: &[&str] = &[
    "context_length_exceeded",
    "max_tokens",
    "too many tokens",
    "request too large",
    "maximum context length",
    "context window",
    "token limit",
    "content_too_large",
    "request_too_large",
];

/// Typed error returned by the agent loop.
#[derive(Debug, thiserror::Error)]
pub enum AgentRunError {
    /// The provider reported that the context window was exceeded.
    #[error("context window exceeded: {0}")]
    ContextWindowExceeded(String),
    /// Any other error.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Check whether an error message indicates a context-window overflow.
fn is_context_window_error(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    CONTEXT_WINDOW_PATTERNS
        .iter()
        .any(|pattern| lower.contains(pattern))
        || lower.contains("status 413")
        || lower.contains("http 413")
}

/// Result of running the agent loop.
#[derive(Debug)]
pub struct AgentRunResult {
    pub text: String,
    pub iterations: usize,
    pub tool_calls_made: usize,
    pub usage: Usage,
}

/// Callback for streaming events out of the runner.
pub type OnEvent = Box<dyn Fn(RunnerEvent) + Send + Sync>;

/// Events emitted during the agent run.
#[derive(Debug, Clone)]
pub enum RunnerEvent {
    /// LLM is processing (show a "thinking" indicator).
    Thinking,
    /// LLM finished thinking (hide the indicator).
    ThinkingDone,
    ToolCallStart {
        id: String,
        name: String,
        arguments: serde_json::Value,
    },
    ToolCallEnd {
        id: String,
        name: String,
        success: bool,
        error: Option<String>,
        result: Option<serde_json::Value>,
    },
    /// LLM returned reasoning/status text alongside tool calls.
    ThinkingText(String),
    TextDelta(String),
    Iteration(usize),
    /// A sub-agent has started executing.
    SubAgentStart {
        task: String,
        model: String,
        depth: u64,
    },
    /// A sub-agent has finished executing.
    SubAgentEnd {
        task: String,
        model: String,
        depth: u64,
        iterations: usize,
        tool_calls_made: usize,
    },
}

/// Try to parse a tool call from the LLM's text response.
///
/// Providers without native tool-calling support are instructed (via the system
/// prompt) to emit a fenced block like:
///
/// ```tool_call
/// {"tool": "exec", "arguments": {"command": "ls"}}
/// ```
///
/// This function extracts that JSON and returns a synthetic `ToolCall` plus the
/// remaining text (if any) outside the fence.
fn parse_tool_call_from_text(text: &str) -> Option<(ToolCall, Option<String>)> {
    // Look for ```tool_call ... ``` blocks.
    let start_marker = "```tool_call";
    let start = text.find(start_marker)?;
    let after_marker = start + start_marker.len();
    let rest = &text[after_marker..];
    let end = rest.find("```")?;
    let json_str = rest[..end].trim();

    let parsed: serde_json::Value = serde_json::from_str(json_str).ok()?;
    let tool_name = parsed["tool"].as_str()?.to_string();
    let arguments = parsed
        .get("arguments")
        .cloned()
        .unwrap_or(serde_json::json!({}));

    let id = format!("text-{}", uuid::Uuid::new_v4());

    // Collect any text outside the tool_call block.
    let before = text[..start].trim();
    let after_end = after_marker + end + 3; // skip closing ```
    let after = text.get(after_end..).unwrap_or("").trim();

    let remaining = if before.is_empty() && after.is_empty() {
        None
    } else if before.is_empty() {
        Some(after.to_string())
    } else if after.is_empty() {
        Some(before.to_string())
    } else {
        Some(format!("{before}\n{after}"))
    };

    Some((
        ToolCall {
            id,
            name: tool_name,
            arguments,
        },
        remaining,
    ))
}

/// Run the agent loop: send messages to the LLM, execute tool calls, repeat.
///
/// If `history` is provided, those messages are inserted between the system
/// prompt and the current user message, giving the LLM conversational context.
pub async fn run_agent_loop(
    provider: Arc<dyn LlmProvider>,
    tools: &ToolRegistry,
    system_prompt: &str,
    user_message: &str,
    on_event: Option<&OnEvent>,
    history: Option<Vec<serde_json::Value>>,
) -> Result<AgentRunResult, AgentRunError> {
    run_agent_loop_with_context(
        provider,
        tools,
        system_prompt,
        user_message,
        on_event,
        history,
        None,
        None,
    )
    .await
}

/// Like `run_agent_loop` but accepts optional context values that are injected
/// into every tool call's parameters (e.g. `_session_key`).
pub async fn run_agent_loop_with_context(
    provider: Arc<dyn LlmProvider>,
    tools: &ToolRegistry,
    system_prompt: &str,
    user_message: &str,
    on_event: Option<&OnEvent>,
    history: Option<Vec<serde_json::Value>>,
    tool_context: Option<serde_json::Value>,
    hook_registry: Option<Arc<HookRegistry>>,
) -> Result<AgentRunResult, AgentRunError> {
    let native_tools = provider.supports_tools();
    let tool_schemas = tools.list_schemas();

    info!(
        provider = provider.name(),
        model = provider.id(),
        native_tools,
        tools_count = tool_schemas.len(),
        "starting agent loop"
    );

    // Extract session key early for hook payloads.
    let session_key_for_hooks = tool_context
        .as_ref()
        .and_then(|ctx| ctx.get("_session_key"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Dispatch BeforeAgentStart hook — may block or modify model.
    if let Some(ref hooks) = hook_registry {
        let payload = HookPayload::BeforeAgentStart {
            session_key: session_key_for_hooks.clone(),
            model: provider.id().to_string(),
        };
        match hooks.dispatch(&payload).await {
            Ok(HookAction::Block(reason)) => {
                warn!(reason = %reason, "agent start blocked by hook");
                return Err(anyhow::anyhow!("agent start blocked by hook: {reason}").into());
            },
            Ok(_) => {},
            Err(e) => {
                warn!(error = %e, "BeforeAgentStart hook dispatch failed");
            },
        }
    }

    let mut messages: Vec<serde_json::Value> = vec![serde_json::json!({
        "role": "system",
        "content": system_prompt,
    })];

    // Insert conversation history before the current user message.
    if let Some(hist) = history {
        messages.extend(hist);
    }

    messages.push(serde_json::json!({
        "role": "user",
        "content": user_message,
    }));

    // Only send tool schemas to providers that support them natively.
    let schemas_for_api = if native_tools {
        &tool_schemas
    } else {
        &vec![]
    };

    let mut iterations = 0;
    let mut total_tool_calls = 0;
    let mut total_input_tokens: u32 = 0;
    let mut total_output_tokens: u32 = 0;

    loop {
        iterations += 1;
        if iterations > MAX_ITERATIONS {
            warn!("agent loop exceeded max iterations ({})", MAX_ITERATIONS);
            return Err(anyhow::anyhow!("agent loop exceeded max iterations").into());
        }

        if let Some(cb) = on_event {
            cb(RunnerEvent::Iteration(iterations));
        }

        info!(
            iteration = iterations,
            messages_count = messages.len(),
            "calling LLM"
        );
        trace!(iteration = iterations, messages = %serde_json::to_string(&messages).unwrap_or_default(), "LLM request messages");

        if let Some(cb) = on_event {
            cb(RunnerEvent::Thinking);
        }

        // Dispatch MessageSending hook — can modify the user content being sent.
        if let Some(ref hooks) = hook_registry {
            let last_user_content = messages
                .iter()
                .rev()
                .find(|m| m.get("role").and_then(|v| v.as_str()) == Some("user"))
                .and_then(|m| m.get("content").and_then(|v| v.as_str()))
                .unwrap_or("")
                .to_string();
            let payload = HookPayload::MessageSending {
                session_key: session_key_for_hooks.clone(),
                content: last_user_content,
            };
            match hooks.dispatch(&payload).await {
                Ok(HookAction::Block(reason)) => {
                    warn!(reason = %reason, "message sending blocked by hook");
                    return Err(anyhow::anyhow!("message sending blocked by hook: {reason}").into());
                },
                Ok(_) => {},
                Err(e) => {
                    warn!(error = %e, "MessageSending hook dispatch failed");
                },
            }
        }

        let mut response: CompletionResponse =
            match provider.complete(&messages, schemas_for_api).await {
                Ok(r) => r,
                Err(e) => {
                    let msg = e.to_string();
                    if is_context_window_error(&msg) {
                        return Err(AgentRunError::ContextWindowExceeded(msg));
                    }
                    return Err(AgentRunError::Other(e));
                },
            };

        if let Some(cb) = on_event {
            cb(RunnerEvent::ThinkingDone);
        }

        // Dispatch MessageSent hook (read-only) with the LLM response.
        if let Some(ref hooks) = hook_registry {
            let content = response.text.clone().unwrap_or_default();
            let payload = HookPayload::MessageSent {
                session_key: session_key_for_hooks.clone(),
                content,
            };
            if let Err(e) = hooks.dispatch(&payload).await {
                warn!(error = %e, "MessageSent hook dispatch failed");
            }
        }

        total_input_tokens = total_input_tokens.saturating_add(response.usage.input_tokens);
        total_output_tokens = total_output_tokens.saturating_add(response.usage.output_tokens);

        info!(
            iteration = iterations,
            has_text = response.text.is_some(),
            tool_calls_count = response.tool_calls.len(),
            input_tokens = response.usage.input_tokens,
            output_tokens = response.usage.output_tokens,
            "LLM response received"
        );
        if let Some(ref text) = response.text {
            trace!(iteration = iterations, text = %text, "LLM response text");
        }

        // For providers without native tool calling, try parsing tool calls from text.
        if !native_tools
            && response.tool_calls.is_empty()
            && let Some(ref text) = response.text
            && let Some((tc, remaining_text)) = parse_tool_call_from_text(text)
        {
            info!(
                tool = %tc.name,
                "parsed tool call from text (non-native provider)"
            );
            response.text = remaining_text;
            response.tool_calls = vec![tc];
        }

        for tc in &response.tool_calls {
            info!(
                iteration = iterations,
                tool_name = %tc.name,
                arguments = %tc.arguments,
                "LLM requested tool call"
            );
        }

        // If no tool calls, return the text response.
        if response.tool_calls.is_empty() {
            let text = response.text.unwrap_or_default();

            info!(
                iterations,
                tool_calls = total_tool_calls,
                "agent loop complete — returning text"
            );

            // Dispatch AgentEnd hook (read-only).
            if let Some(ref hooks) = hook_registry {
                let payload = HookPayload::AgentEnd {
                    session_key: session_key_for_hooks.clone(),
                    text: text.clone(),
                    iterations,
                    tool_calls: total_tool_calls,
                };
                if let Err(e) = hooks.dispatch(&payload).await {
                    warn!(error = %e, "AgentEnd hook dispatch failed");
                }
            }

            return Ok(AgentRunResult {
                text,
                iterations,
                tool_calls_made: total_tool_calls,
                usage: Usage {
                    input_tokens: total_input_tokens,
                    output_tokens: total_output_tokens,
                },
            });
        }

        // Append assistant message with tool calls.
        let tool_calls_json: Vec<serde_json::Value> = response
            .tool_calls
            .iter()
            .map(|tc| {
                serde_json::json!({
                    "id": tc.id,
                    "type": "function",
                    "function": {
                        "name": tc.name,
                        "arguments": tc.arguments.to_string(),
                    }
                })
            })
            .collect();

        let mut assistant_msg = serde_json::json!({
            "role": "assistant",
            "tool_calls": tool_calls_json,
        });
        if let Some(ref text) = response.text {
            assistant_msg["content"] = serde_json::Value::String(text.clone());
            if let Some(cb) = on_event {
                cb(RunnerEvent::ThinkingText(text.clone()));
            }
        }
        messages.push(assistant_msg);

        // Execute each tool call.
        for tc in &response.tool_calls {
            total_tool_calls += 1;

            if let Some(cb) = on_event {
                cb(RunnerEvent::ToolCallStart {
                    id: tc.id.clone(),
                    name: tc.name.clone(),
                    arguments: tc.arguments.clone(),
                });
            }

            // Dispatch BeforeToolCall hook — may block or modify arguments.
            let mut effective_args = tc.arguments.clone();
            if let Some(ref hooks) = hook_registry {
                let payload = HookPayload::BeforeToolCall {
                    session_key: session_key_for_hooks.clone(),
                    tool_name: tc.name.clone(),
                    arguments: tc.arguments.clone(),
                };
                match hooks.dispatch(&payload).await {
                    Ok(HookAction::Block(reason)) => {
                        warn!(tool = %tc.name, reason = %reason, "tool call blocked by hook");
                        if let Some(cb) = on_event {
                            cb(RunnerEvent::ToolCallEnd {
                                id: tc.id.clone(),
                                name: tc.name.clone(),
                                success: false,
                                error: Some(reason.clone()),
                                result: None,
                            });
                        }
                        let err_str = format!("blocked by hook: {reason}");
                        messages.push(serde_json::json!({
                            "role": "tool",
                            "tool_call_id": tc.id,
                            "content": serde_json::json!({ "error": err_str }).to_string(),
                        }));
                        continue;
                    },
                    Ok(HookAction::ModifyPayload(v)) => {
                        effective_args = v;
                    },
                    Ok(HookAction::Continue) => {},
                    Err(e) => {
                        warn!(tool = %tc.name, error = %e, "BeforeToolCall hook dispatch failed");
                    },
                }
            }

            info!(tool = %tc.name, id = %tc.id, args = %effective_args, "executing tool");

            let result = if let Some(tool) = tools.get(&tc.name) {
                // Merge tool_context (e.g. _session_key) into the tool call params.
                let mut args = effective_args;
                if let Some(ref ctx) = tool_context
                    && let (Some(args_obj), Some(ctx_obj)) = (args.as_object_mut(), ctx.as_object())
                {
                    for (k, v) in ctx_obj {
                        args_obj.insert(k.clone(), v.clone());
                    }
                }
                match tool.execute(args).await {
                    Ok(val) => {
                        info!(tool = %tc.name, id = %tc.id, "tool execution succeeded");
                        trace!(tool = %tc.name, result = %val, "tool result");
                        if let Some(cb) = on_event {
                            cb(RunnerEvent::ToolCallEnd {
                                id: tc.id.clone(),
                                name: tc.name.clone(),
                                success: true,
                                error: None,
                                result: Some(val.clone()),
                            });
                        }
                        // Dispatch AfterToolCall hook.
                        if let Some(ref hooks) = hook_registry {
                            let payload = HookPayload::AfterToolCall {
                                session_key: session_key_for_hooks.clone(),
                                tool_name: tc.name.clone(),
                                success: true,
                                result: Some(val.clone()),
                            };
                            if let Err(e) = hooks.dispatch(&payload).await {
                                warn!(tool = %tc.name, error = %e, "AfterToolCall hook dispatch failed");
                            }
                        }
                        serde_json::json!({ "result": val })
                    },
                    Err(e) => {
                        let err_str = e.to_string();
                        warn!(tool = %tc.name, id = %tc.id, error = %err_str, "tool execution failed");
                        if let Some(cb) = on_event {
                            cb(RunnerEvent::ToolCallEnd {
                                id: tc.id.clone(),
                                name: tc.name.clone(),
                                success: false,
                                error: Some(err_str.clone()),
                                result: None,
                            });
                        }
                        // Dispatch AfterToolCall hook on failure too.
                        if let Some(ref hooks) = hook_registry {
                            let payload = HookPayload::AfterToolCall {
                                session_key: session_key_for_hooks.clone(),
                                tool_name: tc.name.clone(),
                                success: false,
                                result: None,
                            };
                            if let Err(e) = hooks.dispatch(&payload).await {
                                warn!(tool = %tc.name, error = %e, "AfterToolCall hook dispatch failed");
                            }
                        }
                        serde_json::json!({ "error": err_str })
                    },
                }
            } else {
                let err_str = format!("unknown tool: {}", tc.name);
                warn!(tool = %tc.name, id = %tc.id, "unknown tool requested by LLM");
                if let Some(cb) = on_event {
                    cb(RunnerEvent::ToolCallEnd {
                        id: tc.id.clone(),
                        name: tc.name.clone(),
                        success: false,
                        error: Some(err_str.clone()),
                        result: None,
                    });
                }
                serde_json::json!({ "error": err_str })
            };

            // Dispatch ToolResultPersist hook — can modify the result before it enters history.
            let result = if let Some(ref hooks) = hook_registry {
                let payload = HookPayload::ToolResultPersist {
                    session_key: session_key_for_hooks.clone(),
                    tool_name: tc.name.clone(),
                    result: result.clone(),
                };
                match hooks.dispatch_sync(&payload) {
                    Ok(HookAction::ModifyPayload(v)) => v,
                    Ok(HookAction::Block(reason)) => {
                        warn!(tool = %tc.name, reason = %reason, "tool result persist blocked by hook");
                        serde_json::json!({ "error": format!("result blocked: {reason}") })
                    },
                    Ok(HookAction::Continue) => result,
                    Err(e) => {
                        warn!(tool = %tc.name, error = %e, "ToolResultPersist hook failed");
                        result
                    },
                }
            } else {
                result
            };

            let tool_result_str = result.to_string();
            debug!(
                tool = %tc.name,
                id = %tc.id,
                result_len = tool_result_str.len(),
                "appending tool result to messages"
            );
            trace!(tool = %tc.name, content = %tool_result_str, "tool result message content");

            messages.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": tc.id,
                "content": tool_result_str,
            }));
        }
    }
}

/// Convenience wrapper matching the old stub signature.
pub async fn run_agent(_agent_id: &str, _session_key: &str, _message: &str) -> Result<String> {
    anyhow::bail!(
        "run_agent requires a configured provider and tool registry; use run_agent_loop instead"
    )
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::model::{CompletionResponse, LlmProvider, StreamEvent, ToolCall, Usage},
        async_trait::async_trait,
        std::pin::Pin,
        tokio_stream::Stream,
    };

    // ── parse_tool_call_from_text tests ──────────────────────────────

    #[test]
    fn test_parse_tool_call_basic() {
        let text = "```tool_call\n{\"tool\": \"exec\", \"arguments\": {\"command\": \"ls\"}}\n```";
        let (tc, remaining) = parse_tool_call_from_text(text).unwrap();
        assert_eq!(tc.name, "exec");
        assert_eq!(tc.arguments["command"], "ls");
        assert!(remaining.is_none());
    }

    #[test]
    fn test_parse_tool_call_with_surrounding_text() {
        let text = "I'll run ls for you.\n```tool_call\n{\"tool\": \"exec\", \"arguments\": {\"command\": \"ls\"}}\n```\nHere you go.";
        let (tc, remaining) = parse_tool_call_from_text(text).unwrap();
        assert_eq!(tc.name, "exec");
        let remaining = remaining.unwrap();
        assert!(remaining.contains("I'll run ls"));
        assert!(remaining.contains("Here you go"));
    }

    #[test]
    fn test_parse_tool_call_no_block() {
        let text = "I would run ls but I can't.";
        assert!(parse_tool_call_from_text(text).is_none());
    }

    #[test]
    fn test_parse_tool_call_invalid_json() {
        let text = "```tool_call\nnot json\n```";
        assert!(parse_tool_call_from_text(text).is_none());
    }

    // ── Mock helpers ─────────────────────────────────────────────────

    /// A mock provider that returns text on the first call.
    struct MockProvider {
        response_text: String,
    }

    #[async_trait]
    impl LlmProvider for MockProvider {
        fn name(&self) -> &str {
            "mock"
        }

        fn id(&self) -> &str {
            "mock-model"
        }

        async fn complete(
            &self,
            _messages: &[serde_json::Value],
            _tools: &[serde_json::Value],
        ) -> Result<CompletionResponse> {
            Ok(CompletionResponse {
                text: Some(self.response_text.clone()),
                tool_calls: vec![],
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                },
            })
        }

        fn stream(
            &self,
            _messages: Vec<serde_json::Value>,
        ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + '_>> {
            Box::pin(tokio_stream::empty())
        }
    }

    /// Mock provider that makes one tool call then returns text (native tool support).
    struct ToolCallingProvider {
        call_count: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl LlmProvider for ToolCallingProvider {
        fn name(&self) -> &str {
            "mock"
        }

        fn id(&self) -> &str {
            "mock-model"
        }

        fn supports_tools(&self) -> bool {
            true
        }

        async fn complete(
            &self,
            _messages: &[serde_json::Value],
            _tools: &[serde_json::Value],
        ) -> Result<CompletionResponse> {
            let count = self
                .call_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if count == 0 {
                Ok(CompletionResponse {
                    text: None,
                    tool_calls: vec![ToolCall {
                        id: "call_1".into(),
                        name: "echo_tool".into(),
                        arguments: serde_json::json!({"text": "hi"}),
                    }],
                    usage: Usage {
                        input_tokens: 10,
                        output_tokens: 5,
                    },
                })
            } else {
                Ok(CompletionResponse {
                    text: Some("Done!".into()),
                    tool_calls: vec![],
                    usage: Usage {
                        input_tokens: 20,
                        output_tokens: 10,
                    },
                })
            }
        }

        fn stream(
            &self,
            _messages: Vec<serde_json::Value>,
        ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + '_>> {
            Box::pin(tokio_stream::empty())
        }
    }

    /// Non-native provider that returns tool calls as text blocks.
    struct TextToolCallingProvider {
        call_count: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl LlmProvider for TextToolCallingProvider {
        fn name(&self) -> &str {
            "mock-no-native"
        }

        fn id(&self) -> &str {
            "mock-no-native"
        }

        fn supports_tools(&self) -> bool {
            false
        }

        async fn complete(
            &self,
            messages: &[serde_json::Value],
            _tools: &[serde_json::Value],
        ) -> Result<CompletionResponse> {
            let count = self
                .call_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if count == 0 {
                // Simulate an LLM emitting a tool_call block in text.
                Ok(CompletionResponse {
                    text: Some("```tool_call\n{\"tool\": \"exec\", \"arguments\": {\"command\": \"echo hello\"}}\n```".into()),
                    tool_calls: vec![],
                    usage: Usage { input_tokens: 10, output_tokens: 20 },
                })
            } else {
                // Verify tool result was fed back.
                let tool_msg = messages.iter().find(|m| m["role"].as_str() == Some("tool"));
                let tool_content = tool_msg.and_then(|m| m["content"].as_str()).unwrap_or("");
                assert!(
                    tool_content.contains("hello"),
                    "tool result should contain 'hello', got: {tool_content}"
                );
                Ok(CompletionResponse {
                    text: Some("The command output: hello".into()),
                    tool_calls: vec![],
                    usage: Usage {
                        input_tokens: 30,
                        output_tokens: 10,
                    },
                })
            }
        }

        fn stream(
            &self,
            _messages: Vec<serde_json::Value>,
        ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + '_>> {
            Box::pin(tokio_stream::empty())
        }
    }

    /// Simple echo tool for testing.
    struct EchoTool;

    #[async_trait]
    impl crate::tool_registry::AgentTool for EchoTool {
        fn name(&self) -> &str {
            "echo_tool"
        }

        fn description(&self) -> &str {
            "Echoes input"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {"text": {"type": "string"}}})
        }

        async fn execute(&self, params: serde_json::Value) -> Result<serde_json::Value> {
            Ok(params)
        }
    }

    /// A tool that actually runs shell commands (test-only, mirrors ExecTool).
    struct TestExecTool;

    #[async_trait]
    impl crate::tool_registry::AgentTool for TestExecTool {
        fn name(&self) -> &str {
            "exec"
        }

        fn description(&self) -> &str {
            "Execute a shell command"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Shell command to execute" }
                },
                "required": ["command"]
            })
        }

        async fn execute(&self, params: serde_json::Value) -> Result<serde_json::Value> {
            let command = params["command"].as_str().unwrap_or("echo noop");
            let output = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(command)
                .output()
                .await?;
            Ok(serde_json::json!({
                "stdout": String::from_utf8_lossy(&output.stdout).to_string(),
                "stderr": String::from_utf8_lossy(&output.stderr).to_string(),
                "exit_code": output.status.code().unwrap_or(-1),
            }))
        }
    }

    // ── Tests ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_simple_text_response() {
        let provider = Arc::new(MockProvider {
            response_text: "Hello!".into(),
        });
        let tools = ToolRegistry::new();
        let result = run_agent_loop(provider, &tools, "You are a test bot.", "Hi", None, None)
            .await
            .unwrap();
        assert_eq!(result.text, "Hello!");
        assert_eq!(result.iterations, 1);
        assert_eq!(result.tool_calls_made, 0);
    }

    #[tokio::test]
    async fn test_tool_call_loop() {
        let provider = Arc::new(ToolCallingProvider {
            call_count: std::sync::atomic::AtomicUsize::new(0),
        });
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(EchoTool));

        let result = run_agent_loop(
            provider,
            &tools,
            "You are a test bot.",
            "Use the tool",
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(result.text, "Done!");
        assert_eq!(result.iterations, 2);
        assert_eq!(result.tool_calls_made, 1);
    }

    /// Mock provider that calls the "exec" tool (native) and verifies result fed back.
    struct ExecSimulatingProvider {
        call_count: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl LlmProvider for ExecSimulatingProvider {
        fn name(&self) -> &str {
            "mock"
        }

        fn id(&self) -> &str {
            "mock-model"
        }

        fn supports_tools(&self) -> bool {
            true
        }

        async fn complete(
            &self,
            messages: &[serde_json::Value],
            _tools: &[serde_json::Value],
        ) -> Result<CompletionResponse> {
            let count = self
                .call_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if count == 0 {
                Ok(CompletionResponse {
                    text: None,
                    tool_calls: vec![ToolCall {
                        id: "call_exec_1".into(),
                        name: "exec".into(),
                        arguments: serde_json::json!({"command": "echo hello"}),
                    }],
                    usage: Usage {
                        input_tokens: 10,
                        output_tokens: 5,
                    },
                })
            } else {
                let tool_msg = messages.iter().find(|m| m["role"].as_str() == Some("tool"));
                let tool_content = tool_msg.and_then(|m| m["content"].as_str()).unwrap_or("");
                let parsed: serde_json::Value = serde_json::from_str(tool_content).unwrap();
                let stdout = parsed["result"]["stdout"].as_str().unwrap_or("");
                assert!(stdout.contains("hello"));
                assert_eq!(parsed["result"]["exit_code"].as_i64().unwrap(), 0);
                Ok(CompletionResponse {
                    text: Some(format!("The output was: {}", stdout.trim())),
                    tool_calls: vec![],
                    usage: Usage {
                        input_tokens: 20,
                        output_tokens: 10,
                    },
                })
            }
        }

        fn stream(
            &self,
            _messages: Vec<serde_json::Value>,
        ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + '_>> {
            Box::pin(tokio_stream::empty())
        }
    }

    #[tokio::test]
    async fn test_exec_tool_end_to_end() {
        let provider = Arc::new(ExecSimulatingProvider {
            call_count: std::sync::atomic::AtomicUsize::new(0),
        });
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(TestExecTool));

        let events: Arc<std::sync::Mutex<Vec<RunnerEvent>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let events_clone = Arc::clone(&events);
        let on_event: OnEvent = Box::new(move |event| {
            events_clone.lock().unwrap().push(event);
        });

        let result = run_agent_loop(
            provider,
            &tools,
            "You are a test bot.",
            "Run echo hello",
            Some(&on_event),
            None,
        )
        .await
        .unwrap();

        assert!(result.text.contains("hello"), "got: {}", result.text);
        assert_eq!(result.iterations, 2);
        assert_eq!(result.tool_calls_made, 1);

        let evts = events.lock().unwrap();
        let has = |name: &str| {
            evts.iter().any(|e| {
                matches!(
                    (e, name),
                    (RunnerEvent::Thinking, "thinking")
                        | (RunnerEvent::ToolCallStart { .. }, "tool_call_start")
                        | (RunnerEvent::ToolCallEnd { .. }, "tool_call_end")
                )
            })
        };
        assert!(has("tool_call_start"));
        assert!(has("tool_call_end"));
        assert!(has("thinking"));

        let tool_end = evts
            .iter()
            .find(|e| matches!(e, RunnerEvent::ToolCallEnd { .. }));
        if let Some(RunnerEvent::ToolCallEnd { success, name, .. }) = tool_end {
            assert!(success, "exec tool should succeed");
            assert_eq!(name, "exec");
        }
    }

    /// Test that non-native providers can still execute tools via text parsing.
    #[tokio::test]
    async fn test_text_based_tool_calling() {
        let provider = Arc::new(TextToolCallingProvider {
            call_count: std::sync::atomic::AtomicUsize::new(0),
        });
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(TestExecTool));

        let events: Arc<std::sync::Mutex<Vec<RunnerEvent>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let events_clone = Arc::clone(&events);
        let on_event: OnEvent = Box::new(move |event| {
            events_clone.lock().unwrap().push(event);
        });

        let result = run_agent_loop(
            provider,
            &tools,
            "You are a test bot.",
            "Run echo hello",
            Some(&on_event),
            None,
        )
        .await
        .unwrap();

        assert!(result.text.contains("hello"), "got: {}", result.text);
        assert_eq!(result.iterations, 2, "should take 2 iterations");
        assert_eq!(result.tool_calls_made, 1, "should execute 1 tool call");

        // Verify tool events were emitted even for text-parsed calls.
        let evts = events.lock().unwrap();
        assert!(
            evts.iter()
                .any(|e| matches!(e, RunnerEvent::ToolCallStart { .. }))
        );
        assert!(
            evts.iter()
                .any(|e| matches!(e, RunnerEvent::ToolCallEnd { success: true, .. }))
        );
    }

    // ── Hook integration tests ──────────────────────────────────────

    use moltis_common::hooks::{HookAction, HookHandler, HookPayload, HookRegistry};

    /// Hook that blocks BeforeAgentStart.
    struct BlockAgentStartHook;

    #[async_trait]
    impl HookHandler for BlockAgentStartHook {
        fn name(&self) -> &str {
            "block-agent-start"
        }

        fn events(&self) -> &[moltis_common::hooks::HookEvent] {
            &[moltis_common::hooks::HookEvent::BeforeAgentStart]
        }

        async fn handle(
            &self,
            _event: moltis_common::hooks::HookEvent,
            _payload: &HookPayload,
        ) -> Result<HookAction> {
            Ok(HookAction::Block("test block".into()))
        }
    }

    #[tokio::test]
    async fn test_before_agent_start_hook_blocks() {
        let provider = Arc::new(MockProvider {
            response_text: "Hello!".into(),
        });
        let tools = ToolRegistry::new();
        let mut registry = HookRegistry::new();
        registry.register(Arc::new(BlockAgentStartHook));

        let result = run_agent_loop_with_context(
            provider,
            &tools,
            "system",
            "hi",
            None,
            None,
            Some(serde_json::json!({"_session_key": "test"})),
            Some(Arc::new(registry)),
        )
        .await;

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("agent start blocked by hook")
        );
    }

    /// Hook that records AgentEnd events.
    struct RecordAgentEndHook {
        called: Arc<std::sync::atomic::AtomicBool>,
    }

    #[async_trait]
    impl HookHandler for RecordAgentEndHook {
        fn name(&self) -> &str {
            "record-agent-end"
        }

        fn events(&self) -> &[moltis_common::hooks::HookEvent] {
            &[moltis_common::hooks::HookEvent::AgentEnd]
        }

        async fn handle(
            &self,
            _event: moltis_common::hooks::HookEvent,
            _payload: &HookPayload,
        ) -> Result<HookAction> {
            self.called.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(HookAction::Continue)
        }
    }

    #[tokio::test]
    async fn test_agent_end_hook_fires() {
        let provider = Arc::new(MockProvider {
            response_text: "Hello!".into(),
        });
        let tools = ToolRegistry::new();
        let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut registry = HookRegistry::new();
        registry.register(Arc::new(RecordAgentEndHook {
            called: Arc::clone(&called),
        }));

        let result = run_agent_loop_with_context(
            provider,
            &tools,
            "system",
            "hi",
            None,
            None,
            Some(serde_json::json!({"_session_key": "test"})),
            Some(Arc::new(registry)),
        )
        .await
        .unwrap();

        assert_eq!(result.text, "Hello!");
        assert!(called.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// Hook that blocks MessageSending.
    struct BlockMessageSendingHook;

    #[async_trait]
    impl HookHandler for BlockMessageSendingHook {
        fn name(&self) -> &str {
            "block-message-sending"
        }

        fn events(&self) -> &[moltis_common::hooks::HookEvent] {
            &[moltis_common::hooks::HookEvent::MessageSending]
        }

        async fn handle(
            &self,
            _event: moltis_common::hooks::HookEvent,
            _payload: &HookPayload,
        ) -> Result<HookAction> {
            Ok(HookAction::Block("content policy violation".into()))
        }
    }

    #[tokio::test]
    async fn test_message_sending_hook_blocks() {
        let provider = Arc::new(MockProvider {
            response_text: "Hello!".into(),
        });
        let tools = ToolRegistry::new();
        let mut registry = HookRegistry::new();
        registry.register(Arc::new(BlockMessageSendingHook));

        let result = run_agent_loop_with_context(
            provider,
            &tools,
            "system",
            "hi",
            None,
            None,
            Some(serde_json::json!({"_session_key": "test"})),
            Some(Arc::new(registry)),
        )
        .await;

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("message sending blocked")
        );
    }

    /// Hook that records MessageSent events.
    struct RecordMessageSentHook {
        content: Arc<std::sync::Mutex<String>>,
    }

    #[async_trait]
    impl HookHandler for RecordMessageSentHook {
        fn name(&self) -> &str {
            "record-message-sent"
        }

        fn events(&self) -> &[moltis_common::hooks::HookEvent] {
            &[moltis_common::hooks::HookEvent::MessageSent]
        }

        async fn handle(
            &self,
            _event: moltis_common::hooks::HookEvent,
            payload: &HookPayload,
        ) -> Result<HookAction> {
            if let HookPayload::MessageSent { content, .. } = payload {
                *self.content.lock().unwrap() = content.clone();
            }
            Ok(HookAction::Continue)
        }
    }

    #[tokio::test]
    async fn test_message_sent_hook_fires() {
        let provider = Arc::new(MockProvider {
            response_text: "Hello from LLM!".into(),
        });
        let tools = ToolRegistry::new();
        let content = Arc::new(std::sync::Mutex::new(String::new()));
        let mut registry = HookRegistry::new();
        registry.register(Arc::new(RecordMessageSentHook {
            content: Arc::clone(&content),
        }));

        let result = run_agent_loop_with_context(
            provider,
            &tools,
            "system",
            "hi",
            None,
            None,
            Some(serde_json::json!({"_session_key": "test"})),
            Some(Arc::new(registry)),
        )
        .await
        .unwrap();

        assert_eq!(result.text, "Hello from LLM!");
        assert_eq!(*content.lock().unwrap(), "Hello from LLM!");
    }

    /// Hook that redacts tool results via ToolResultPersist.
    struct RedactToolResultHook;

    #[async_trait]
    impl HookHandler for RedactToolResultHook {
        fn name(&self) -> &str {
            "redact-tool-result"
        }

        fn events(&self) -> &[moltis_common::hooks::HookEvent] {
            &[moltis_common::hooks::HookEvent::ToolResultPersist]
        }

        async fn handle(
            &self,
            _event: moltis_common::hooks::HookEvent,
            _payload: &HookPayload,
        ) -> Result<HookAction> {
            Ok(HookAction::ModifyPayload(
                serde_json::json!({"result": "[REDACTED]"}),
            ))
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_tool_result_persist_hook_modifies() {
        let provider = Arc::new(ToolCallingProvider {
            call_count: std::sync::atomic::AtomicUsize::new(0),
        });
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(EchoTool));

        let mut registry = HookRegistry::new();
        registry.register(Arc::new(RedactToolResultHook));

        let result = run_agent_loop_with_context(
            provider,
            &tools,
            "system",
            "use tool",
            None,
            None,
            Some(serde_json::json!({"_session_key": "test"})),
            Some(Arc::new(registry)),
        )
        .await
        .unwrap();

        // The agent still completes — the redacted result is fed to the LLM.
        assert_eq!(result.text, "Done!");
        assert_eq!(result.tool_calls_made, 1);
    }

    // ── Context window error detection tests ───────────────────────

    #[test]
    fn test_is_context_window_error_patterns() {
        assert!(is_context_window_error(
            "context_length_exceeded: too many tokens"
        ));
        assert!(is_context_window_error("request too large for model"));
        assert!(is_context_window_error(
            "This model's maximum context length is 128000 tokens"
        ));
        assert!(is_context_window_error("HTTP 413 Payload Too Large"));
        assert!(is_context_window_error("status 413"));
        assert!(is_context_window_error("content_too_large"));
        assert!(!is_context_window_error("rate limit exceeded"));
        assert!(!is_context_window_error("internal server error"));
    }

    /// Mock provider that returns a context-window error on the first call,
    /// then succeeds on the second.
    struct ContextWindowErrorProvider {
        call_count: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl LlmProvider for ContextWindowErrorProvider {
        fn name(&self) -> &str {
            "mock"
        }

        fn id(&self) -> &str {
            "mock-model"
        }

        async fn complete(
            &self,
            _messages: &[serde_json::Value],
            _tools: &[serde_json::Value],
        ) -> Result<CompletionResponse> {
            let count = self
                .call_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if count == 0 {
                anyhow::bail!(
                    "context_length_exceeded: This model's maximum context length is 128000 tokens"
                );
            }
            Ok(CompletionResponse {
                text: Some("Success after retry".into()),
                tool_calls: vec![],
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                },
            })
        }

        fn stream(
            &self,
            _messages: Vec<serde_json::Value>,
        ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + '_>> {
            Box::pin(tokio_stream::empty())
        }
    }

    #[tokio::test]
    async fn test_context_window_error_returned() {
        let provider = Arc::new(ContextWindowErrorProvider {
            call_count: std::sync::atomic::AtomicUsize::new(0),
        });
        let tools = ToolRegistry::new();
        let result =
            run_agent_loop(provider, &tools, "You are a test bot.", "Hi", None, None).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, AgentRunError::ContextWindowExceeded(_)),
            "expected ContextWindowExceeded, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn test_non_context_error_is_other() {
        struct FailProvider;

        #[async_trait]
        impl LlmProvider for FailProvider {
            fn name(&self) -> &str {
                "mock"
            }

            fn id(&self) -> &str {
                "mock"
            }

            async fn complete(
                &self,
                _messages: &[serde_json::Value],
                _tools: &[serde_json::Value],
            ) -> Result<CompletionResponse> {
                anyhow::bail!("internal server error")
            }

            fn stream(
                &self,
                _messages: Vec<serde_json::Value>,
            ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + '_>> {
                Box::pin(tokio_stream::empty())
            }
        }

        let provider = Arc::new(FailProvider);
        let tools = ToolRegistry::new();
        let result = run_agent_loop(provider, &tools, "system", "hi", None, None).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, AgentRunError::Other(_)),
            "expected Other, got: {err:?}"
        );
    }
}
