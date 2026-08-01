//! Wire protocol types exchanged with the frontend over WebSocket/JSON.

use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
pub(crate) struct IncomingMessage {
    #[serde(rename = "type")]
    pub(crate) msg_type: String,
    pub(crate) content: Option<String>,
    pub(crate) trace_id: Option<String>,
}

/// Inbound `{"type":"approval_response","request_id":"ap1","approved":true,"remember":false}`
/// from the frontend. `remember` is optional (defaults false).
#[derive(Deserialize)]
pub(crate) struct ApprovalResponse {
    pub(crate) request_id: String,
    pub(crate) approved: bool,
    #[serde(default)]
    pub(crate) remember: bool,
}

#[derive(Serialize)]
pub(crate) struct OutgoingEvent {
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
    pub(crate) fn content(s: String) -> Self {
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
    pub(crate) fn thinking(s: String) -> Self {
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
    pub(crate) fn tool_start(name: String, args: String) -> Self {
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
    pub(crate) fn tool_end(name: String, output: String) -> Self {
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
    pub(crate) fn error(msg: String) -> Self {
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
    pub(crate) fn done() -> Self {
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
    pub(crate) fn approval_request(
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
