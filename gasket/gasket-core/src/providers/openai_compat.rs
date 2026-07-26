//! OpenAI-compatible provider (OpenAI / DeepSeek / 智谱 / xAI / Groq / Ollama /
//! vLLM / etc.).
//!
//! See `gasket-refactor-plan.md` §8.1. One implementation covers ~80% of
//! providers; Anthropic native lives in [`crate::providers::anthropic`].

use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use futures_util::Stream;
use serde_json::json;

use crate::types::context::{ModelSpec, StreamChunk, StreamFn};
use crate::types::message::{AgentMessage, ContentBlock};
use crate::types::tool::ToolDefinition;

/// OpenAI-compatible chat completions provider.
#[derive(Clone)]
pub struct OpenAiCompat {
    base_url: String,
    api_key: String,
    client: reqwest::Client,
}

impl OpenAiCompat {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
            client: reqwest::Client::new(),
        }
    }

    /// Construct with a pre-built client (e.g. one carrying proxy config from
    /// [`crate::providers::ProviderConfig`]).
    pub fn with_client(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        client: reqwest::Client,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
            client,
        }
    }

    /// Build from a [`ProviderConfig`] (reads base_url/key/client).
    pub fn from_config(cfg: &crate::providers::ProviderConfig) -> Self {
        Self::with_client(&cfg.base_url, &cfg.api_key, cfg.client.clone())
    }
}

impl std::fmt::Debug for OpenAiCompat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiCompat")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl StreamFn for OpenAiCompat {
    fn stream(
        &self,
        model: &ModelSpec,
        messages: &[AgentMessage],
        system_prompt: &str,
        tools: &[ToolDefinition],
        _signal: Option<Arc<AtomicBool>>,
    ) -> Pin<Box<dyn Stream<Item = StreamChunk> + Send>> {
        let body = build_request_body(model, messages, system_prompt, tools);
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let client = self.client.clone();
        let api_key = self.api_key.clone();

        tracing::debug!(url = %url, model = %model.id, "openai-compat request");
        tracing::debug!(
            request_body = %&serde_json::to_string(&body).unwrap_or_default(),
            "openai-compat request body"
        );

        Box::pin(async_stream::stream! {
            let resp = match client
                .post(&url)
                .bearer_auth(&api_key)
                .json(&body)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!(error = %e, "openai-compat request failed");
                    yield StreamChunk::Error(e.to_string());
                    return;
                }
            };

            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                tracing::warn!(status = %status, "openai-compat non-2xx response");
                yield StreamChunk::Error(format!("HTTP {status}: {text}"));
                return;
            }

            let body_text = match resp.text().await {
                Ok(t) => t,
                Err(e) => {
                    yield StreamChunk::Error(e.to_string());
                    return;
                }
            };

            for payload in crate::providers::sse::parse_sse_body(&body_text) {
                match payload {
                    None => {
                        yield StreamChunk::Done;
                        return;
                    }
                    Some(json_str) => {
                        for chunk in parse_openai_chunk(&json_str) {
                            yield chunk;
                        }
                    }
                }
            }
            yield StreamChunk::Done;
        })
    }
}

/// Build the OpenAI chat/completions request body.
fn build_request_body(
    model: &ModelSpec,
    messages: &[AgentMessage],
    system_prompt: &str,
    tools: &[ToolDefinition],
) -> serde_json::Value {
    let mut msgs = Vec::new();
    if !system_prompt.is_empty() {
        msgs.push(json!({"role": "system", "content": system_prompt}));
    }
    for m in messages {
        if let Some(entry) = convert_message(m) {
            msgs.push(entry);
        }
    }

    let mut body = json!({
        "model": model.id,
        "messages": msgs,
        "stream": true,
        "max_tokens": model.max_tokens,
        "stream_options": {"include_usage": true},
    });

    if !tools.is_empty() {
        body["tools"] = json!(tools.iter().map(tool_to_openai).collect::<Vec<_>>());
    }

    body
}

/// Convert one internal message to an OpenAI message object. `Custom` is
/// dropped (never sent to the LLM).
fn convert_message(msg: &AgentMessage) -> Option<serde_json::Value> {
    match msg {
        AgentMessage::User(u) => {
            let text = collect_text(&u.content);
            Some(json!({"role": "user", "content": text}))
        }
        AgentMessage::Assistant(a) => {
            let mut entry = json!({"role": "assistant"});
            let text = collect_text(&a.content);
            if !text.is_empty() {
                entry["content"] = json!(text);
            }
            // DeepSeek-reasoner (and other reasoning models) require the
            // assistant's `reasoning_content` to be echoed back when continuing
            // a turn, or the API rejects the request with HTTP 400.
            let reasoning: String = a
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Thinking { thinking } => Some(thinking.as_str()),
                    _ => None,
                })
                .collect();
            if !reasoning.is_empty() {
                entry["reasoning_content"] = json!(reasoning);
            }
            let tool_calls: Vec<_> = a
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolCall { tool_call: tc } => Some(json!({
                        "id": tc.id,
                        "type": "function",
                        "function": {
                            "name": tc.function.name,
                            "arguments": tc.function.arguments,
                        }
                    })),
                    _ => None,
                })
                .collect();
            if !tool_calls.is_empty() {
                entry["tool_calls"] = json!(tool_calls);
            }
            Some(entry)
        }
        AgentMessage::ToolResult(tr) => Some(json!({
            "role": "tool",
            "tool_call_id": tr.tool_call_id,
            "content": collect_text(&tr.content),
        })),
        AgentMessage::Custom(_) => None,
    }
}

fn collect_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

fn tool_to_openai(t: &ToolDefinition) -> serde_json::Value {
    json!({
        "type": "function",
        "function": {
            "name": t.name,
            "description": t.description,
            "parameters": t.parameters,
        }
    })
}

/// Parse one OpenAI streaming JSON payload into zero or more chunks.
pub(crate) fn parse_openai_chunk(json_str: &str) -> Vec<StreamChunk> {
    let v: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(_) => return vec![],
    };

    let mut chunks = Vec::new();

    // Usage (final chunk when include_usage is set).
    if let Some(usage) = v.get("usage") {
        let input = usage
            .get("prompt_tokens")
            .and_then(|t| t.as_u64())
            .unwrap_or(0);
        let output = usage
            .get("completion_tokens")
            .and_then(|t| t.as_u64())
            .unwrap_or(0);
        chunks.push(StreamChunk::Usage { input, output });
    }

    let delta = match v.pointer("/choices/0/delta") {
        Some(d) => d,
        None => return chunks,
    };

    if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
        chunks.push(StreamChunk::TextDelta(content.to_string()));
    }

    // DeepSeek-reasoner and similar reasoning models stream `reasoning_content`
    // alongside `content`. Capture it as thinking so it can be echoed back on
    // the next turn (the API rejects requests that drop it with HTTP 400).
    if let Some(reasoning) = delta.get("reasoning_content").and_then(|c| c.as_str()) {
        chunks.push(StreamChunk::ThinkingDelta(reasoning.to_string()));
    }

    if let Some(tool_calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
        for tc in tool_calls {
            let id = tc
                .get("id")
                .and_then(|i| i.as_str())
                .unwrap_or("")
                .to_string();
            let function = tc.get("function");
            let name = function
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .map(String::from);
            let args_delta = function
                .and_then(|f| f.get("arguments"))
                .and_then(|a| a.as_str())
                .unwrap_or("")
                .to_string();
            if !id.is_empty() || name.is_some() || !args_delta.is_empty() {
                chunks.push(StreamChunk::ToolCallDelta {
                    id,
                    name,
                    args_delta,
                });
            }
        }
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_text_delta() {
        let json = r#"{"choices":[{"delta":{"content":"Hi"}}]}"#;
        let chunks = parse_openai_chunk(json);
        assert_eq!(chunks, vec![StreamChunk::TextDelta("Hi".into())]);
    }

    #[test]
    fn parses_tool_call_delta() {
        let json = r#"{"choices":[{"delta":{"tool_calls":[{"id":"t1","function":{"name":"echo","arguments":"{\"x\":"}}]}}]}"#;
        let chunks = parse_openai_chunk(json);
        assert_eq!(chunks.len(), 1);
        match &chunks[0] {
            StreamChunk::ToolCallDelta {
                id,
                name,
                args_delta,
            } => {
                assert_eq!(id, "t1");
                assert_eq!(name.as_deref(), Some("echo"));
                assert_eq!(args_delta, "{\"x\":");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parses_usage_chunk() {
        let json = r#"{"usage":{"prompt_tokens":10,"completion_tokens":5}}"#;
        let chunks = parse_openai_chunk(json);
        assert_eq!(
            chunks,
            vec![StreamChunk::Usage {
                input: 10,
                output: 5
            }]
        );
    }

    #[test]
    fn ignores_malformed_json() {
        assert!(parse_openai_chunk("not json").is_empty());
    }

    #[test]
    fn convert_message_drops_custom() {
        let custom = AgentMessage::Custom(crate::types::message::CustomMessage {
            custom_type: "x".into(),
            content: json!({}),
            timestamp: 0,
        });
        assert!(convert_message(&custom).is_none());
    }

    #[test]
    fn parses_reasoning_content() {
        let json = r#"{"choices":[{"delta":{"reasoning_content":"thinking..."}}]}"#;
        let chunks = parse_openai_chunk(json);
        assert_eq!(
            chunks,
            vec![StreamChunk::ThinkingDelta("thinking...".into())]
        );
    }
}
