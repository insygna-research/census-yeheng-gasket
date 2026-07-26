//! Anthropic native messages API provider.
//!
//! See `gasket-refactor-plan.md` §8.2. Uses the same SSE transport as
//! [`crate::providers::openai_compat`] but a different body/event shape:
//! `system` is a top-level field, tools use `input_schema`, and stream deltas
//! arrive as `content_block_delta` events with `text_delta` / `input_json_delta`.

use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use futures_util::Stream;
use serde_json::json;

use crate::types::context::{ModelSpec, StreamChunk, StreamFn};
use crate::types::message::{AgentMessage, ContentBlock};
use crate::types::tool::ToolDefinition;

/// Anthropic native messages provider.
#[derive(Clone)]
pub struct AnthropicProvider {
    base_url: String,
    api_key: String,
    client: reqwest::Client,
}

impl AnthropicProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            base_url: "https://api.anthropic.com/v1".into(),
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

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }
}

impl std::fmt::Debug for AnthropicProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicProvider")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl StreamFn for AnthropicProvider {
    fn stream(
        &self,
        model: &ModelSpec,
        messages: &[AgentMessage],
        system_prompt: &str,
        tools: &[ToolDefinition],
        _signal: Option<Arc<AtomicBool>>,
    ) -> Pin<Box<dyn Stream<Item = StreamChunk> + Send>> {
        let body = build_request_body(model, messages, system_prompt, tools);
        let url = format!("{}/messages", self.base_url.trim_end_matches('/'));
        let client = self.client.clone();
        let api_key = self.api_key.clone();

        tracing::debug!(url = %url, model = %model.id, "anthropic request");
        tracing::debug!(
            request_body = %serde_json::to_string(&body).unwrap_or_default(),
            "anthropic request body"
        );

        Box::pin(async_stream::stream! {
            let resp = match client
                .post(&url)
                .header("x-api-key", &api_key)
                .header("anthropic-version", "2023-06-01")
                .json(&body)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!(error = %e, "anthropic request failed");
                    yield StreamChunk::Error(e.to_string());
                    return;
                }
            };

            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                tracing::warn!(status = %status, "anthropic non-2xx response");
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
                        for chunk in parse_anthropic_chunk(&json_str) {
                            yield chunk;
                        }
                    }
                }
            }
            yield StreamChunk::Done;
        })
    }
}

fn build_request_body(
    model: &ModelSpec,
    messages: &[AgentMessage],
    system_prompt: &str,
    tools: &[ToolDefinition],
) -> serde_json::Value {
    let msgs: Vec<_> = messages.iter().filter_map(convert_message).collect();

    let mut body = json!({
        "model": model.id,
        "max_tokens": model.max_tokens,
        "messages": msgs,
        "stream": true,
    });
    if !system_prompt.is_empty() {
        body["system"] = json!(system_prompt);
    }
    if !tools.is_empty() {
        body["tools"] = json!(tools.iter().map(tool_to_anthropic).collect::<Vec<_>>());
    }
    body
}

/// Convert one message to an Anthropic message. Anthropic requires user/assistant
/// alternation; tool results go in user messages as `tool_result` blocks.
fn convert_message(msg: &AgentMessage) -> Option<serde_json::Value> {
    match msg {
        AgentMessage::User(u) => {
            let blocks: Vec<_> = u
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(json!({"type": "text", "text": text})),
                    _ => None,
                })
                .collect();
            if blocks.is_empty() {
                None
            } else {
                Some(json!({"role": "user", "content": blocks}))
            }
        }
        AgentMessage::Assistant(a) => {
            let blocks: Vec<_> = a
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(json!({"type": "text", "text": text})),
                    ContentBlock::ToolCall { tool_call: tc } => Some(json!({
                        "type": "tool_use",
                        "id": tc.id,
                        "name": tc.function.name,
                        "input": serde_json::from_str::<serde_json::Value>(&tc.function.arguments)
                            .unwrap_or(json!({})),
                    })),
                    _ => None,
                })
                .collect();
            if blocks.is_empty() {
                None
            } else {
                Some(json!({"role": "assistant", "content": blocks}))
            }
        }
        AgentMessage::ToolResult(tr) => Some(json!({
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": tr.tool_call_id,
                "content": collect_text(&tr.content),
                "is_error": tr.is_error,
            }]
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

fn tool_to_anthropic(t: &ToolDefinition) -> serde_json::Value {
    json!({
        "name": t.name,
        "description": t.description,
        "input_schema": t.parameters,
    })
}

/// Parse one Anthropic SSE event JSON into zero or more chunks.
pub(crate) fn parse_anthropic_chunk(json_str: &str) -> Vec<StreamChunk> {
    let v: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(_) => return vec![],
    };

    let event_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");

    match event_type {
        "content_block_delta" => {
            let delta = match v.get("delta") {
                Some(d) => d,
                None => return vec![],
            };
            let delta_type = delta.get("type").and_then(|t| t.as_str()).unwrap_or("");
            match delta_type {
                "text_delta" => {
                    let text = delta
                        .get("text")
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .to_string();
                    if text.is_empty() {
                        vec![]
                    } else {
                        vec![StreamChunk::TextDelta(text)]
                    }
                }
                "input_json_delta" => {
                    let partial = delta
                        .get("partial_json")
                        .and_then(|p| p.as_str())
                        .unwrap_or("")
                        .to_string();
                    vec![StreamChunk::ToolCallDelta {
                        id: String::new(),
                        name: None,
                        args_delta: partial,
                    }]
                }
                "thinking_delta" => {
                    let thinking = delta
                        .get("thinking")
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .to_string();
                    vec![StreamChunk::ThinkingDelta(thinking)]
                }
                _ => vec![],
            }
        }
        "message_delta" => {
            if let Some(usage) = v.pointer("/usage/output_tokens") {
                let output = usage.as_u64().unwrap_or(0);
                vec![StreamChunk::Usage { input: 0, output }]
            } else {
                vec![]
            }
        }
        "message_start" => {
            if let Some(input) = v.pointer("/message/usage/input_tokens") {
                let input = input.as_u64().unwrap_or(0);
                vec![StreamChunk::Usage { input, output: 0 }]
            } else {
                vec![]
            }
        }
        _ => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_text_delta() {
        let json = r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hi"}}"#;
        let chunks = parse_anthropic_chunk(json);
        assert_eq!(chunks, vec![StreamChunk::TextDelta("Hi".into())]);
    }

    #[test]
    fn parses_input_json_delta() {
        let json = r#"{"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"{\"x\":"}}"#;
        let chunks = parse_anthropic_chunk(json);
        assert_eq!(chunks.len(), 1);
        match &chunks[0] {
            StreamChunk::ToolCallDelta { args_delta, .. } => {
                assert_eq!(args_delta, "{\"x\":");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parses_message_start_usage() {
        let json = r#"{"type":"message_start","message":{"usage":{"input_tokens":42}}}"#;
        let chunks = parse_anthropic_chunk(json);
        assert_eq!(
            chunks,
            vec![StreamChunk::Usage {
                input: 42,
                output: 0
            }]
        );
    }

    #[test]
    fn ignores_ping_events() {
        let json = r#"{"type":"ping"}"#;
        assert!(parse_anthropic_chunk(json).is_empty());
    }

    #[test]
    fn ignores_malformed_json() {
        assert!(parse_anthropic_chunk("garbage").is_empty());
    }
}
