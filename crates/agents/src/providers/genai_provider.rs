use std::pin::Pin;

use async_trait::async_trait;
use futures::StreamExt;
use tokio_stream::Stream;

use crate::model::{CompletionResponse, LlmProvider, StreamEvent, Usage};

/// Provider backed by the `genai` crate (supports Anthropic, OpenAI, Gemini,
/// Groq, Ollama, xAI, DeepSeek, Cohere, and more via a single client).
pub struct GenaiProvider {
    model: String,
    provider_name: String,
    client: genai::Client,
}

impl GenaiProvider {
    pub fn new(model: String, provider_name: String) -> Self {
        Self {
            model,
            provider_name,
            client: genai::Client::default(),
        }
    }
}

fn genai_usage_to_usage(u: &genai::chat::Usage) -> Usage {
    Usage {
        input_tokens: u.prompt_tokens.unwrap_or(0) as u32,
        output_tokens: u.completion_tokens.unwrap_or(0) as u32,
    }
}

fn build_genai_messages(messages: &[serde_json::Value]) -> Vec<genai::chat::ChatMessage> {
    messages
        .iter()
        .map(|msg| {
            let role = msg["role"].as_str().unwrap_or("user");
            let content = msg["content"].as_str().unwrap_or("");
            match role {
                "system" => genai::chat::ChatMessage::system(content),
                "assistant" => genai::chat::ChatMessage::assistant(content),
                _ => genai::chat::ChatMessage::user(content),
            }
        })
        .collect()
}

#[async_trait]
impl LlmProvider for GenaiProvider {
    fn name(&self) -> &str {
        &self.provider_name
    }

    fn id(&self) -> &str {
        &self.model
    }

    async fn complete(
        &self,
        messages: &[serde_json::Value],
        _tools: &[serde_json::Value],
    ) -> anyhow::Result<CompletionResponse> {
        let chat_req = genai::chat::ChatRequest::new(build_genai_messages(messages));
        let chat_res = self
            .client
            .exec_chat(&self.model, chat_req, None)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        let text = chat_res.first_text().map(|s| s.to_string());
        let usage = genai_usage_to_usage(&chat_res.usage);

        Ok(CompletionResponse {
            text,
            tool_calls: vec![],
            usage,
        })
    }

    fn stream(
        &self,
        messages: Vec<serde_json::Value>,
    ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + '_>> {
        Box::pin(async_stream::stream! {
            use genai::chat::ChatStreamEvent;

            let chat_req = genai::chat::ChatRequest::new(build_genai_messages(&messages));
            let mut chat_stream = match self
                .client
                .exec_chat_stream(&self.model, chat_req, None)
                .await
            {
                Ok(s) => s,
                Err(e) => {
                    yield StreamEvent::Error(format!("{e}"));
                    return;
                }
            };

            while let Some(result) = chat_stream.stream.next().await {
                match result {
                    Ok(ChatStreamEvent::Chunk(chunk)) => {
                        if !chunk.content.is_empty() {
                            yield StreamEvent::Delta(chunk.content);
                        }
                    }
                    Ok(ChatStreamEvent::ReasoningChunk(chunk)) => {
                        if !chunk.content.is_empty() {
                            yield StreamEvent::Delta(chunk.content);
                        }
                    }
                    Ok(ChatStreamEvent::End(end)) => {
                        let usage = end.captured_usage
                            .as_ref()
                            .map(genai_usage_to_usage)
                            .unwrap_or(Usage { input_tokens: 0, output_tokens: 0 });
                        yield StreamEvent::Done(usage);
                        return;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        yield StreamEvent::Error(format!("{e}"));
                        return;
                    }
                }
            }
        })
    }
}
