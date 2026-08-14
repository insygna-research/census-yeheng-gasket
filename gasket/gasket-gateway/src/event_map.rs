//! Map core [`AgentEvent`]s to the frontend's WebSocket JSON protocol.

use std::collections::HashMap;

use gasket_core::{AgentEvent, ContentDelta};

use crate::wire::OutgoingEvent;

/// Convert an [`AgentEvent`] to the frontend's JSON protocol, looking up
/// tool names from `tool_names` (populated by [`ToolExecutionStart`]). For
/// `ToolExecutionEnd` without a preceding start (denied/timed-out/cancelled
/// tool calls) the name falls back to the one carried by the result message.
pub(crate) fn event_to_ws(
    event: &AgentEvent,
    tool_names: &mut HashMap<String, String>,
) -> Option<OutgoingEvent> {
    match event {
        AgentEvent::MessageUpdate { delta } => match delta {
            ContentDelta::TextDelta(t) => Some(OutgoingEvent::content(t.clone())),
            ContentDelta::ThinkingDelta(t) => Some(OutgoingEvent::thinking(t.clone())),
            ContentDelta::ToolCallDelta { .. } => {
                // Accumulated server-side; sent as tool_start at execution.
                None
            }
        },
        AgentEvent::ToolExecutionStart {
            tool_name, args, ..
        } => {
            let args_str = serde_json::to_string(args).unwrap_or_default();
            Some(OutgoingEvent::tool_start(tool_name.clone(), args_str))
        }
        AgentEvent::ToolExecutionEnd {
            tool_call_id,
            result,
            ..
        } => {
            let name = tool_names
                .get(tool_call_id)
                .cloned()
                .unwrap_or_else(|| result.tool_name.clone());
            let summary = result
                .content
                .iter()
                .find_map(|b| match b {
                    gasket_core::ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            Some(OutgoingEvent::tool_end(name, summary))
        }
        AgentEvent::Error { message } => Some(OutgoingEvent::error(message.clone())),
        _ => None,
    }
}

/// Convert a [`SubagentEvent`] to a raw JSON string for the frontend's
/// `subagent_*` protocol. Returns `None` for events that have no WS
/// representation (the internal `Usage` accounting event).
pub(crate) fn subagent_event_to_ws(event: &gasket_core::SubagentEvent) -> Option<String> {
    use gasket_core::SubagentEvent;
    let json = match event {
        SubagentEvent::AllStarted { count } => serde_json::json!({
            "type": "subagent_all_started", "count": count
        }),
        SubagentEvent::Synthesizing => serde_json::json!({
            "type": "subagent_synthesizing"
        }),
        SubagentEvent::Started { id, task, index } => serde_json::json!({
            "type": "subagent_started", "id": id, "task": task, "index": index
        }),
        SubagentEvent::Thinking { id, content } => serde_json::json!({
            "type": "subagent_thinking", "id": id, "content": content
        }),
        SubagentEvent::Content { id, content } => serde_json::json!({
            "type": "subagent_content", "id": id, "content": content
        }),
        SubagentEvent::ToolStart {
            id,
            name,
            arguments,
        } => serde_json::json!({
            "type": "subagent_tool_start", "id": id, "name": name,
            "arguments": arguments
        }),
        SubagentEvent::ToolEnd { id, name, output } => serde_json::json!({
            "type": "subagent_tool_end", "id": id, "name": name,
            "output": output
        }),
        SubagentEvent::Completed {
            id,
            index,
            summary,
            tool_count,
        } => serde_json::json!({
            "type": "subagent_completed", "id": id, "index": index,
            "summary": summary, "tool_count": tool_count
        }),
        SubagentEvent::Error { id, index, error } => serde_json::json!({
            "type": "subagent_error", "id": id, "index": index, "error": error
        }),
        // Internal accounting only — folded into the session usage counters
        // by the WS forwarder; never serialized to the frontend.
        SubagentEvent::Usage { .. } => return None,
    };
    Some(serde_json::to_string(&json).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gasket_core::SubagentEvent;

    #[test]
    fn usage_has_no_wire_representation() {
        assert!(
            subagent_event_to_ws(&SubagentEvent::Usage {
                input_tokens: 1,
                output_tokens: 2,
            })
            .is_none(),
            "Usage is internal accounting and must never reach the socket"
        );
    }

    #[test]
    fn tool_end_json_has_no_tool_id() {
        // The frontend generates its own tool ids and matches by name;
        // the server's tool_call_id is deliberately not serialized.
        let json = subagent_event_to_ws(&SubagentEvent::ToolEnd {
            id: "s1".into(),
            name: "bash".into(),
            output: Some("out".into()),
        })
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "subagent_tool_end");
        assert_eq!(v["name"], "bash");
        assert_eq!(v["id"], "s1");
        assert!(v.get("tool_id").is_none(), "no dead tool_id on the wire");
    }
}
