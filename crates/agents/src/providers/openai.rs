use std::pin::Pin;

use async_trait::async_trait;
use futures::StreamExt;
use tokio_stream::Stream;

use crate::model::{CompletionResponse, LlmProvider, StreamEvent, Usage};

pub struct OpenAiProvider {
    api_key: String,
    model: String,
    base_url: String,
    client: reqwest::Client,
}

impl OpenAiProvider {
    pub fn new(api_key: String, model: String, base_url: String) -> Self {
        Self {
            api_key,
            model,
            base_url,
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    fn name(&self) -> &str {
        "openai"
    }

    fn id(&self) -> &str {
        &self.model
    }

    async fn complete(
        &self,
        messages: &[serde_json::Value],
        _tools: &[serde_json::Value],
    ) -> anyhow::Result<CompletionResponse> {
        let body = serde_json::json!({
            "model": self.model,
            "messages": messages,
        });

        let resp = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json::<serde_json::Value>()
            .await?;

        let text = resp["choices"][0]["message"]["content"]
            .as_str()
            .map(|s| s.to_string());

        let usage = Usage {
            input_tokens: resp["usage"]["prompt_tokens"].as_u64().unwrap_or(0) as u32,
            output_tokens: resp["usage"]["completion_tokens"].as_u64().unwrap_or(0) as u32,
        };

        Ok(CompletionResponse {
            text,
            tool_calls: vec![],
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
                "messages": messages,
                "stream": true,
                "stream_options": { "include_usage": true },
            });

            let resp = match self
                .client
                .post(format!("{}/chat/completions", self.base_url))
                .header("Authorization", format!("Bearer {}", self.api_key))
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

                while let Some(pos) = buf.find('\n') {
                    let line = buf[..pos].trim().to_string();
                    buf = buf[pos + 1..].to_string();

                    if line.is_empty() {
                        continue;
                    }

                    let Some(data) = line.strip_prefix("data: ") else {
                        continue;
                    };

                    if data == "[DONE]" {
                        yield StreamEvent::Done(Usage { input_tokens, output_tokens });
                        return;
                    }

                    if let Ok(evt) = serde_json::from_str::<serde_json::Value>(data) {
                        // Usage chunk (sent with stream_options.include_usage)
                        if let Some(u) = evt.get("usage").filter(|u| !u.is_null()) {
                            input_tokens = u["prompt_tokens"].as_u64().unwrap_or(0) as u32;
                            output_tokens = u["completion_tokens"].as_u64().unwrap_or(0) as u32;
                        }

                        if let Some(delta) = evt["choices"][0]["delta"]["content"].as_str() {
                            if !delta.is_empty() {
                                yield StreamEvent::Delta(delta.to_string());
                            }
                        }
                    }
                }
            }
        })
    }
}
