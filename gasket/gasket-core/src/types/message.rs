//! `AgentMessage` — the internal unified message model.
//!
//! All messages carry a `timestamp` (ms since epoch via [`crate::now`]).
//! `CustomMessage` is plugin-private and filtered out at the LLM boundary.

use serde::{Deserialize, Serialize};

/// The single internal message enum. Converted to provider protocol only at
/// the LLM boundary (`convert_to_llm`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum AgentMessage {
    User(UserMessage),
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
    /// Plugin-private message. Never sent to the LLM (filtered by
    /// `convert_to_llm`). The `custom_type` namespace is owned by the plugin.
    Custom(CustomMessage),
}

/// A user-authored message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserMessage {
    pub content: Vec<ContentBlock>,
    pub timestamp: u64,
}

/// An assistant (model) message. `content` may hold text, thinking, and tool
/// calls simultaneously.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub content: Vec<ContentBlock>,
    pub model: ModelId,
    pub stop_reason: StopReason,
    pub usage: Option<Usage>,
    pub timestamp: u64,
}

impl AssistantMessage {
    /// Empty assistant message tagged with the model in use, for streaming
    /// accumulation in `stream_assistant_response`.
    pub fn new(model: &ModelId) -> Self {
        Self {
            content: Vec::new(),
            model: model.clone(),
            stop_reason: StopReason::EndTurn,
            usage: None,
            timestamp: crate::now(),
        }
    }

    /// Append a text delta to the last Text block (or start a new one).
    pub fn append_text(&mut self, delta: &str) {
        if let Some(ContentBlock::Text { text }) = self.content.last_mut() {
            text.push_str(delta);
        } else {
            self.content.push(ContentBlock::text(delta));
        }
    }

    /// Append a thinking delta to the last Thinking block (or start a new one).
    pub fn append_thinking(&mut self, delta: &str) {
        if let Some(ContentBlock::Thinking { thinking }) = self.content.last_mut() {
            thinking.push_str(delta);
        } else {
            self.content.push(ContentBlock::Thinking {
                thinking: delta.to_string(),
            });
        }
    }

    /// Accumulate a tool-call delta: create the call on first sight of an id,
    /// then append argument fragments as they stream in.
    ///
    /// OpenAI-compat streams key continuation deltas by `index` and omit `id`
    /// (and `name`) on every delta after the first. When `id` is empty we
    /// therefore append to the most recent tool call instead of matching by id
    /// - this holds for sequential streaming (the common case). Truly
    ///   parallel/interleaved tool calls would need index-based tracking.
    pub fn append_tool_call(&mut self, id: String, name: Option<String>, args_delta: String) {
        let target = if id.is_empty() {
            self.content.iter_mut().rev().find_map(|b| match b {
                ContentBlock::ToolCall { tool_call: tc } => Some(tc),
                _ => None,
            })
        } else {
            self.content.iter_mut().rev().find_map(|b| match b {
                ContentBlock::ToolCall { tool_call: tc } if tc.id == id => Some(tc),
                _ => None,
            })
        };

        match target {
            Some(tc) => {
                if let Some(name) = name {
                    tc.function.name = name;
                }
                tc.function.arguments.push_str(&args_delta);
            }
            None => {
                self.content.push(ContentBlock::ToolCall {
                    tool_call: ToolCall {
                        id,
                        function: FunctionCall {
                            name: name.unwrap_or_default(),
                            arguments: args_delta,
                        },
                    },
                });
            }
        }
    }
}

/// A tool-result message, paired with the `tool_call_id` it answers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultMessage {
    pub tool_call_id: String,
    pub tool_name: String,
    pub content: Vec<ContentBlock>,
    pub is_error: bool,
    pub timestamp: u64,
}

/// A plugin-private message. `custom_type` is the plugin's namespace
/// (e.g. `"todo.list"`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomMessage {
    pub custom_type: String,
    pub content: serde_json::Value,
    pub timestamp: u64,
}

/// One block of message content. An assistant turn may hold several of these.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Image {
        image: ImageContent,
    },
    ToolCall {
        tool_call: ToolCall,
    },
    /// Model reasoning content (extended thinking).
    Thinking {
        thinking: String,
    },
}

impl ContentBlock {
    pub fn text(s: impl Into<String>) -> Self {
        ContentBlock::Text { text: s.into() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageContent {
    pub mime_type: String,
    pub data: String, // base64
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    /// Raw JSON arguments string from the model (validated at execution time).
    pub arguments: String,
}

/// Why the model stopped generating.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// Model finished naturally.
    EndTurn,
    /// Model emitted tool calls; loop should execute them and continue.
    ToolUse,
    /// Output hit the token cap.
    MaxTokens,
    /// Provider/stream returned an error.
    Error(String),
    /// Aborted via the abort signal.
    Aborted,
}

/// A model identifier (provider + model id). Lightweight string wrapper so
/// providers can format it as they see fit.
pub type ModelId = String;

/// Token usage reported by the provider.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}
