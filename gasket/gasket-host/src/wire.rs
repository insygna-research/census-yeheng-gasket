//! Wire DTO for the frontend's streaming chat protocol.
//!
//! This schema is owned by the host (single definition) and shared by every
//! transport: the gateway serializes it onto a WebSocket, the Tauri desktop
//! backend emits the same JSON as the payload of its `chat-event` IPC event.
//! Field-for-field compatibility with the frontend's `processWebSocketMessage`
//! is the contract - add fields, never rename.

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
    /// Cumulative input tokens for the whole session (only on `done`).
    #[serde(skip_serializing_if = "Option::is_none")]
    usage_in: Option<u64>,
    /// Cumulative output tokens for the whole session (only on `done`).
    #[serde(skip_serializing_if = "Option::is_none")]
    usage_out: Option<u64>,
    /// Wall-clock duration of this turn in milliseconds (only on `done`).
    #[serde(skip_serializing_if = "Option::is_none")]
    elapsed_ms: Option<u64>,
}

impl OutgoingEvent {
    fn base(event_type: &'static str) -> Self {
        Self {
            event_type,
            id: None,
            tool_name: None,
            description: None,
            content: None,
            name: None,
            arguments: None,
            output: None,
            message: None,
            usage_in: None,
            usage_out: None,
            elapsed_ms: None,
        }
    }

    pub fn content(s: String) -> Self {
        let mut ev = Self::base("content");
        ev.content = Some(s);
        ev
    }
    pub fn thinking(s: String) -> Self {
        let mut ev = Self::base("thinking");
        ev.content = Some(s);
        ev
    }
    pub fn tool_start(name: String, args: String) -> Self {
        let mut ev = Self::base("tool_start");
        ev.name = Some(name);
        ev.arguments = Some(args);
        ev
    }
    pub fn tool_end(name: String, output: String) -> Self {
        let mut ev = Self::base("tool_end");
        ev.name = Some(name);
        ev.output = Some(output);
        ev
    }
    pub fn error(msg: String) -> Self {
        let mut ev = Self::base("error");
        ev.content = Some(msg.clone());
        ev.message = Some(msg);
        ev
    }
    pub fn done() -> Self {
        Self::base("done")
    }
    /// Turn-boundary `done` carrying a usage summary. The frontend renders
    /// one line: elapsed time + cumulative input/output tokens.
    pub fn done_with_summary(usage_in: u64, usage_out: u64, elapsed_ms: u64) -> Self {
        let mut ev = Self::base("done");
        ev.usage_in = Some(usage_in);
        ev.usage_out = Some(usage_out);
        ev.elapsed_ms = Some(elapsed_ms);
        ev
    }
    /// Reply to a message received while a turn is already running. Kept
    /// distinct from `error` so the frontend can show a toast without
    /// clearing the in-flight conversation state.
    pub fn busy(msg: String) -> Self {
        let mut ev = Self::base("busy");
        ev.content = Some(msg.clone());
        ev.message = Some(msg);
        ev
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
        let mut ev = Self::base("approval_request");
        ev.id = Some(request_id);
        ev.tool_name = Some(tool_name);
        ev.description = Some(desc);
        ev.arguments = Some(args.to_string());
        ev
    }
}
