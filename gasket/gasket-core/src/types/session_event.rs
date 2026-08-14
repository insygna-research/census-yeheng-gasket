//! `SessionEvent` — the append-only vocabulary of the session event log.
//!
//! User/Assistant/ToolResult wrap the corresponding [`AgentMessage`]
//! variants (serde-compatible with legacy message rows: migration is just
//! wrapping by discriminant). [`derive_messages`] projects the log back into
//! the model-visible message list.

use serde::{Deserialize, Serialize};

use crate::types::message::{AgentMessage, Usage};

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
}
