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
