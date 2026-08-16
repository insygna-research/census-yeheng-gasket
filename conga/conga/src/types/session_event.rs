//! `SessionEvent` — the append-only vocabulary of the session event log.
//!
//! User/Assistant/ToolResult wrap the corresponding [`AgentMessage`]
//! variants (serde-compatible with legacy message rows: migration is just
//! wrapping by discriminant). [`derive_messages`] projects the log back into
//! the model-visible message list.

use serde::{Deserialize, Serialize};

use crate::types::message::{AgentMessage, ContentBlock, ToolResultMessage, Usage};

/// The append-only vocabulary of the session event log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    TurnStart,
    /// Wraps an [`AgentMessage::User`].
    User(AgentMessage),
    /// Wraps an [`AgentMessage::Assistant`], with the token accounting that
    /// traveled with it.
    Assistant {
        message: AgentMessage,
        usage: Option<Usage>,
    },
    /// Wraps an [`AgentMessage::ToolResult`].
    ToolResult(AgentMessage),
    TurnEnd {
        reason: TurnEndReason,
    },
}

/// Why a turn ended.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TurnEndReason {
    Completed,
    Aborted { cause: Option<CancelCause> },
    Error { message: String },
}

/// Who cancelled an in-flight turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CancelCause {
    User,
    Parent,
    Hook { reason: String },
}

impl SessionEvent {
    /// Message → event (only the three message-surface variants;
    /// TurnStart/TurnEnd are not constructed this way).
    pub fn from_message(msg: &AgentMessage, usage: Option<Usage>) -> Option<SessionEvent> {
        match msg {
            AgentMessage::User(_) => Some(SessionEvent::User(msg.clone())),
            AgentMessage::Assistant(_) => Some(SessionEvent::Assistant {
                message: msg.clone(),
                usage,
            }),
            AgentMessage::ToolResult(_) => Some(SessionEvent::ToolResult(msg.clone())),
            AgentMessage::Custom(_) => None,
        }
    }
}

/// Pure projection: event log → model-visible messages. TurnStart/TurnEnd
/// produce no messages. A torn tail left by a crash is projected as-is (the
/// partial facts are kept intact).
pub fn derive_messages(log: &[SessionEvent]) -> Vec<AgentMessage> {
    log.iter()
        .filter_map(|ev| match ev {
            SessionEvent::User(msg)
            | SessionEvent::Assistant { message: msg, .. }
            | SessionEvent::ToolResult(msg) => Some(msg.clone()),
            SessionEvent::TurnStart | SessionEvent::TurnEnd { .. } => None,
        })
        .collect()
}

/// Synthesize error `ToolResult`s for tool calls whose turn ended before a
/// result was recorded — a cooperative abort between calls in a batch, a
/// crash mid-execution, or a stream error after partial tool calls. The log
/// keeps those partial facts intact (that is the point); the provider
/// protocol does not tolerate them: OpenAI-compat and Anthropic both reject
/// an assistant `tool_calls` message that is not followed by one result per
/// `tool_call_id` (HTTP 400). Runs on the in-memory working copy only, right
/// after [`derive_messages`]; the on-disk log is untouched.
pub fn repair_unanswered_tool_calls(messages: &mut Vec<AgentMessage>) {
    let mut i = 0;
    while i < messages.len() {
        // Tool calls of this assistant message, in call order.
        let calls: Vec<(String, String)> = match &messages[i] {
            AgentMessage::Assistant(a) => a
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolCall { tool_call: tc } => {
                        Some((tc.id.clone(), tc.function.name.clone()))
                    }
                    _ => None,
                })
                .collect(),
            _ => {
                i += 1;
                continue;
            }
        };
        if calls.is_empty() {
            i += 1;
            continue;
        }
        // Consume the run of tool results that answers this assistant.
        let mut answered: std::collections::HashSet<String> = Default::default();
        let mut j = i + 1;
        while let Some(AgentMessage::ToolResult(tr)) = messages.get(j) {
            answered.insert(tr.tool_call_id.clone());
            j += 1;
        }
        let missing: Vec<(String, String)> = calls
            .into_iter()
            .filter(|(id, _)| !answered.contains(id))
            .collect();
        if missing.is_empty() {
            i = j;
            continue;
        }
        let synthesized: Vec<AgentMessage> = missing
            .into_iter()
            .map(|(id, name)| {
                AgentMessage::ToolResult(ToolResultMessage {
                    tool_call_id: id,
                    tool_name: name,
                    content: vec![ContentBlock::text(
                        "Error: no result was recorded for this tool call (the turn ended \
                         before it completed); treat it as failed.",
                    )],
                    is_error: true,
                    timestamp: crate::now(),
                })
            })
            .collect();
        let inserted = synthesized.len();
        messages.splice(j..j, synthesized);
        i = j + inserted;
    }
}

#[cfg(test)]
mod tests {
    use crate::types::message::AgentMessage;
    use crate::types::session_event::{
        derive_messages, CancelCause, SessionEvent, TurnEndReason, Usage,
    };

    #[test]
    fn derive_projects_only_surface_events() {
        let log = vec![
            SessionEvent::TurnStart,
            SessionEvent::User(AgentMessage::user("hi")),
            SessionEvent::Assistant {
                message: AgentMessage::assistant_text("hello"),
                usage: Some(Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                }),
            },
            SessionEvent::TurnEnd {
                reason: TurnEndReason::Completed,
            },
        ];
        assert_eq!(derive_messages(&log).len(), 2); // user + assistant
    }

    #[test]
    fn derive_tolerates_missing_turn_end() {
        // torn-tail 崩溃遗留
        let log = vec![
            SessionEvent::TurnStart,
            SessionEvent::User(AgentMessage::user("hi")),
            SessionEvent::Assistant {
                message: AgentMessage::assistant_text("partial"),
                usage: None,
            },
        ];
        assert_eq!(derive_messages(&log).len(), 2);
    }

    #[test]
    fn serde_tag_shape_is_snake_case() {
        let ev = SessionEvent::TurnEnd {
            reason: TurnEndReason::Aborted {
                cause: Some(CancelCause::User),
            },
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains(r#""type":"turn_end""#));
        assert!(s.contains(r#""kind":"aborted""#));
    }

    #[test]
    fn unknown_type_discriminant_fails_to_parse() {
        // fail closed
        assert!(serde_json::from_str::<SessionEvent>(r#"{"type":"wat","data":1}"#).is_err());
    }

    /// Assistant carrying two tool calls (the dangling-call shape an
    /// aborted batch leaves in the log).
    fn assistant_with_calls(ids: &[&str]) -> AgentMessage {
        use crate::types::message::{
            AssistantMessage, ContentBlock, FunctionCall, StopReason, ToolCall,
        };
        let mut a = AssistantMessage::new(&"m".into());
        a.stop_reason = StopReason::ToolUse;
        for id in ids {
            a.content.push(ContentBlock::ToolCall {
                tool_call: ToolCall {
                    id: (*id).to_string(),
                    function: FunctionCall {
                        name: "write".to_string(),
                        arguments: "{}".to_string(),
                    },
                },
            });
        }
        AgentMessage::Assistant(a)
    }

    fn tool_result(id: &str) -> AgentMessage {
        use crate::types::message::{ContentBlock, ToolResultMessage};
        AgentMessage::ToolResult(ToolResultMessage {
            tool_call_id: id.to_string(),
            tool_name: "write".to_string(),
            content: vec![ContentBlock::text("ok")],
            is_error: false,
            timestamp: 0,
        })
    }

    #[test]
    fn repair_synthesizes_missing_tool_results() {
        use super::repair_unanswered_tool_calls;
        // Abort between two calls in one batch: t1 answered, t2 dangling.
        let mut msgs = vec![
            AgentMessage::user("go"),
            assistant_with_calls(&["t1", "t2"]),
            tool_result("t1"),
            AgentMessage::user("again"),
        ];
        repair_unanswered_tool_calls(&mut msgs);
        assert_eq!(msgs.len(), 5, "one synthesized result inserted");
        let synth = match &msgs[3] {
            AgentMessage::ToolResult(tr) if tr.tool_call_id == "t2" => tr,
            other => panic!("expected synthesized t2 result, got {other:?}"),
        };
        assert!(synth.is_error);
        assert_eq!(synth.tool_name, "write");
        // Inserted after the real result, before the next user message.
        assert!(matches!(&msgs[2], AgentMessage::ToolResult(tr) if tr.tool_call_id == "t1"));
        assert!(matches!(&msgs[4], AgentMessage::User(_)));
    }

    #[test]
    fn repair_noop_when_all_answered() {
        use super::repair_unanswered_tool_calls;
        let mut msgs = vec![
            AgentMessage::user("go"),
            assistant_with_calls(&["t1", "t2"]),
            tool_result("t1"),
            tool_result("t2"),
        ];
        let before = msgs.clone();
        repair_unanswered_tool_calls(&mut msgs);
        assert_eq!(msgs, before);
    }

    #[test]
    fn repair_handles_dangling_tail() {
        use super::repair_unanswered_tool_calls;
        // Crash mid-execution: the assistant with tool calls is the last
        // message in the log.
        let mut msgs = vec![
            AgentMessage::user("go"),
            assistant_with_calls(&["t1", "t2"]),
            tool_result("t1"),
        ];
        repair_unanswered_tool_calls(&mut msgs);
        assert!(
            matches!(&msgs[3], AgentMessage::ToolResult(tr) if tr.tool_call_id == "t2" && tr.is_error)
        );
    }
}
