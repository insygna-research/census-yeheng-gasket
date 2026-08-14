//! Wire DTO for the frontend's streaming chat protocol.
//!
//! This schema is owned by the host (single definition) and shared by every
//! transport: the gateway serializes it onto a WebSocket, the Tauri desktop
//! backend emits the same JSON as the payload of its `chat-event` IPC event.
//! Field-for-field compatibility with the frontend's `processWebSocketMessage`
//! is the contract — add fields, never rename.

use serde::Serialize;

/// One server -> client event. Optional fields are omitted from the JSON when
/// absent, matching the original gateway wire shape exactly.
#[derive(Serialize)]
pub struct OutgoingEvent {
    #[serde(rename = "type")]
    event_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    arguments: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

impl OutgoingEvent {
    pub fn content(s: String) -> Self {
        Self {
            event_type: "content",
            id: None,
            tool_name: None,
            description: None,
            content: Some(s),
            name: None,
            arguments: None,
            output: None,
            message: None,
        }
    }
    pub fn thinking(s: String) -> Self {
        Self {
            event_type: "thinking",
            id: None,
            tool_name: None,
            description: None,
            content: Some(s),
            name: None,
            arguments: None,
            output: None,
            message: None,
        }
    }
    pub fn tool_start(name: String, args: String) -> Self {
        Self {
            event_type: "tool_start",
            id: None,
            tool_name: None,
            description: None,
            content: None,
            name: Some(name),
            arguments: Some(args),
            output: None,
            message: None,
        }
    }
    pub fn tool_end(name: String, output: String) -> Self {
        Self {
            event_type: "tool_end",
            id: None,
            tool_name: None,
            description: None,
            content: None,
            name: Some(name),
            arguments: None,
            output: Some(output),
            message: None,
        }
    }
    pub fn error(msg: String) -> Self {
        Self {
            event_type: "error",
            id: None,
            tool_name: None,
            description: None,
            content: Some(msg.clone()),
            name: None,
            arguments: None,
            output: None,
            message: Some(msg),
        }
    }
    pub fn done() -> Self {
        Self {
            event_type: "done",
            id: None,
            tool_name: None,
            description: None,
            content: None,
            name: None,
            arguments: None,
            output: None,
            message: None,
        }
    }
    /// Reply to a message received while a turn is already running. Kept
    /// distinct from `error` so the frontend can show a toast without
    /// clearing the in-flight conversation state.
    pub fn busy(msg: String) -> Self {
        Self {
            event_type: "busy",
            id: None,
            tool_name: None,
            description: None,
            content: Some(msg.clone()),
            name: None,
            arguments: None,
            output: None,
            message: Some(msg),
        }
    }
    pub fn approval_request(
        request_id: String,
        tool_name: String,
        args: &serde_json::Value,
    ) -> Self {
        // description 给前端展示；arguments 保留原始参数。截断防超长。
        let desc = serde_json::to_string(args).unwrap_or_default();
        let desc = if desc.chars().count() > 300 {
            format!("{}...", desc.chars().take(300).collect::<String>())
        } else {
            desc
        };
        Self {
            event_type: "approval_request",
            id: Some(request_id),
            tool_name: Some(tool_name),
            description: Some(desc),
            content: None,
            name: None,
            arguments: Some(args.to_string()),
            output: None,
            message: None,
        }
    }
}
