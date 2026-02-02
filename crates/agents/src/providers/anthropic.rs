use std::pin::Pin;

use {async_trait::async_trait, futures::StreamExt, secrecy::ExposeSecret, tokio_stream::Stream};

use tracing::{debug, trace, warn};

use crate::model::{CompletionResponse, LlmProvider, StreamEvent, ToolCall, Usage};

pub struct AnthropicProvider {
    api_key: secrecy::Secret<String>,
    model: String,
    base_url: String,
    client: reqwest::Client,
}

impl AnthropicProvider {
    pub fn new(api_key: String, model: String, base_url: String) -> Self {
        Self {
            api_key: secrecy::Secret::new(api_key),
            model,
            base_url,
            client: reqwest::Client::new(),
        }
    }
}

/// Convert tool schemas from the generic format to Anthropic's tool format.
fn to_anthropic_tools(tools: &[serde_json::Value]) -> Vec<serde_json::Value> {
    tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "name": t["name"],
                "description": t["description"],
                "input_schema": t["parameters"],
            })
        })
        .collect()
}

/// Parse tool_use blocks from an Anthropic response.
fn parse_tool_calls(content: &[serde_json::Value]) -> Vec<ToolCall> {
    content
        .iter()
        .filter_map(|block| {
            if block["type"].as_str() == Some("tool_use") {
                Some(ToolCall {
                    id: block["id"].as_str().unwrap_or("").to_string(),
                    name: block["name"].as_str().unwrap_or("").to_string(),
                    arguments: block["input"].clone(),
                })
            } else {
                None
            }
        })
        .collect()
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn id(&self) -> &str {
        &self.model
    }

    fn supports_tools(&self) -> bool {
        true
    }

    fn context_window(&self) -> u32 {
        super::context_window_for_model(&self.model)
    }

    async fn complete(
        &self,
        messages: &[serde_json::Value],
        tools: &[serde_json::Value],
    ) -> anyhow::Result<CompletionResponse> {
        // Separate system message from conversation messages.
        let (system_text, conv_messages): (Option<String>, Vec<&serde_json::Value>) = {
            let mut sys = None;
            let mut msgs = Vec::new();
            for m in messages {
                if m["role"].as_str() == Some("system") {
                    sys = m["content"].as_str().map(|s| s.to_string());
                } else {
                    msgs.push(m);
                }
            }
            (sys, msgs)
        };

        // Convert tool-result messages to Anthropic format.
        let anthropic_messages: Vec<serde_json::Value> = conv_messages
            .iter()
            .map(|m| {
                if m["role"].as_str() == Some("tool") {
                    // Anthropic expects tool results as user messages with tool_result content blocks.
                    serde_json::json!({
                        "role": "user",
                        "content": [{
                            "type": "tool_result",
                            "tool_use_id": m["tool_call_id"],
                            "content": m["content"],
                        }]
                    })
                } else if m["role"].as_str() == Some("assistant") && m.get("tool_calls").is_some() {
                    // Convert assistant tool_calls to Anthropic content blocks.
                    let mut content = Vec::new();
                    if let Some(text) = m["content"].as_str()
                        && !text.is_empty()
                    {
                        content.push(serde_json::json!({ "type": "text", "text": text }));
                    }
                    if let Some(tcs) = m["tool_calls"].as_array() {
                        for tc in tcs {
                            let args_str = tc["function"]["arguments"].as_str().unwrap_or("{}");
                            let args: serde_json::Value =
                                serde_json::from_str(args_str).unwrap_or(serde_json::json!({}));
                            content.push(serde_json::json!({
                                "type": "tool_use",
                                "id": tc["id"],
                                "name": tc["function"]["name"],
                                "input": args,
                            }));
                        }
                    }
                    serde_json::json!({ "role": "assistant", "content": content })
                } else {
                    (*m).clone()
                }
            })
            .collect();

        let mut body = serde_json::json!({
            "model": self.model,
            "max_tokens": 4096,
            "messages": anthropic_messages,
        });

        if let Some(ref sys) = system_text {
            body["system"] = serde_json::Value::String(sys.clone());
        }

        if !tools.is_empty() {
            body["tools"] = serde_json::Value::Array(to_anthropic_tools(tools));
        }

        debug!(
            model = %self.model,
            messages_count = anthropic_messages.len(),
            tools_count = tools.len(),
            has_system = system_text.is_some(),
            "anthropic complete request"
        );
        trace!(body = %serde_json::to_string(&body).unwrap_or_default(), "anthropic request body");

        let http_resp = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", self.api_key.expose_secret())
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await?;

        let status = http_resp.status();
        if !status.is_success() {
            let body_text = http_resp.text().await.unwrap_or_default();
            warn!(status = %status, body = %body_text, "anthropic API error");
            anyhow::bail!("Anthropic API error HTTP {status}: {body_text}");
        }

        let resp = http_resp.json::<serde_json::Value>().await?;
        trace!(response = %resp, "anthropic raw response");

        let content = resp["content"].as_array().cloned().unwrap_or_default();

        let text = content
            .iter()
            .filter_map(|b| {
                if b["type"].as_str() == Some("text") {
                    b["text"].as_str().map(|s| s.to_string())
                } else {
                    None
                }
            })
            .reduce(|a, b| a + &b);

        let tool_calls = parse_tool_calls(&content);

        let usage = Usage {
            input_tokens: resp["usage"]["input_tokens"].as_u64().unwrap_or(0) as u32,
            output_tokens: resp["usage"]["output_tokens"].as_u64().unwrap_or(0) as u32,
        };

        Ok(CompletionResponse {
            text,
            tool_calls,
            usage,
        })
    }

    #[allow(clippy::collapsible_if)]
    fn stream(
        &self,
        messages: Vec<serde_json::Value>,
    ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + '_>> {
        Box::pin(async_stream::stream! {
            let body = serde_json::json!({
                "model": self.model,
                "max_tokens": 4096,
                "messages": messages,
                "stream": true,
            });

            let resp = match self
                .client
                .post(format!("{}/v1/messages", self.base_url))
                .header("x-api-key", self.api_key.expose_secret())
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json")
                .json(&body)
                .send()
                .await
            {
                Ok(r) => {
                    if let Err(e) = r.error_for_status_ref() {
                        let status = e.status().map(|s| s.as_u16()).unwrap_or(0);
                        let body_text = r.text().await.unwrap_or_default();
                        yield StreamEvent::Error(format!("HTTP {status}: {body_text}"));
                        return;
                    }
                    r
                }
                Err(e) => {
                    yield StreamEvent::Error(e.to_string());
                    return;
                }
            };

            let mut byte_stream = resp.bytes_stream();
            let mut buf = String::new();
            let mut input_tokens: u32 = 0;
            let mut output_tokens: u32 = 0;

            while let Some(chunk) = byte_stream.next().await {
                let chunk = match chunk {
                    Ok(c) => c,
                    Err(e) => {
                        yield StreamEvent::Error(e.to_string());
                        return;
                    }
                };
                buf.push_str(&String::from_utf8_lossy(&chunk));

                while let Some(pos) = buf.find("\n\n") {
                    let block = buf[..pos].to_string();
                    buf = buf[pos + 2..].to_string();

                    for line in block.lines() {
                        if let Some(data) = line.strip_prefix("data: ") {
                            if let Ok(evt) = serde_json::from_str::<serde_json::Value>(data) {
                                let evt_type = evt["type"].as_str().unwrap_or("");
                                match evt_type {
                                    "content_block_delta" => {
                                        if let Some(text) = evt["delta"]["text"].as_str() {
                                            if !text.is_empty() {
                                                yield StreamEvent::Delta(text.to_string());
                                            }
                                        }
                                    }
                                    "message_delta" => {
                                        if let Some(u) = evt["usage"]["output_tokens"].as_u64() {
                                            output_tokens = u as u32;
                                        }
                                    }
                                    "message_start" => {
                                        if let Some(u) = evt["message"]["usage"]["input_tokens"].as_u64() {
                                            input_tokens = u as u32;
                                        }
                                    }
                                    "message_stop" => {
                                        yield StreamEvent::Done(Usage { input_tokens, output_tokens });
                                        return;
                                    }
                                    "error" => {
                                        let msg = evt["error"]["message"]
                                            .as_str()
                                            .unwrap_or("unknown error");
                                        yield StreamEvent::Error(msg.to_string());
                                        return;
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                }
            }
        })
    }
}
