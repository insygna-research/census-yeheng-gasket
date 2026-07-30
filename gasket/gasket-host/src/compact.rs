//! In-memory history compaction by message count.
//!
//! Pure host policy: shrinks the working transcript before `run_agent_loop`.
//! Does not rewrite JSONL. No token counting, no LLM summary.

use gasket_core::{AgentMessage, ContentBlock, UserMessage};

/// Default cap on working history size (message count, not tokens).
pub const DEFAULT_MAX_MESSAGES: usize = 80;

/// Read `GASKET_COMPACT_MAX_MESSAGES` or [`DEFAULT_MAX_MESSAGES`].
/// `0` means "do not compact".
pub fn max_messages_from_env() -> usize {
    max_messages_from(&|k| std::env::var(k))
}

pub fn max_messages_from(lookup: &dyn Fn(&str) -> Result<String, std::env::VarError>) -> usize {
    lookup("GASKET_COMPACT_MAX_MESSAGES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_MAX_MESSAGES)
}

/// If `messages.len() > max_messages`, keep the newest `max_messages - 1`
/// entries and prepend one user notice naming how many were dropped.
///
/// Under budget (or `max_messages == 0`): clone unchanged.
/// Best-effort by count only — no turn-boundary repair.
pub fn compact_by_count(messages: &[AgentMessage], max_messages: usize) -> Vec<AgentMessage> {
    if max_messages == 0 || messages.len() <= max_messages {
        return messages.to_vec();
    }

    let keep = max_messages.saturating_sub(1);
    let start = messages.len().saturating_sub(keep);
    let dropped = start;

    let mut out = Vec::with_capacity(keep + 1);
    out.push(AgentMessage::User(UserMessage {
        content: vec![ContentBlock::text(format!(
            "[compacted {dropped} earlier messages]"
        ))],
        timestamp: gasket_core::now(),
    }));
    out.extend_from_slice(&messages[start..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use gasket_core::{AssistantMessage, StopReason};

    fn user(s: &str) -> AgentMessage {
        AgentMessage::User(UserMessage {
            content: vec![ContentBlock::text(s)],
            timestamp: 1,
        })
    }

    fn assistant(s: &str) -> AgentMessage {
        AgentMessage::Assistant(AssistantMessage {
            content: vec![ContentBlock::text(s)],
            model: "m".into(),
            stop_reason: StopReason::EndTurn,
            usage: None,
            timestamp: 1,
        })
    }

    #[test]
    fn under_budget_unchanged() {
        let msgs = vec![user("a"), assistant("b")];
        let out = compact_by_count(&msgs, 10);
        assert_eq!(out.len(), 2);
        assert!(matches!(&out[0], AgentMessage::User(_)));
    }

    #[test]
    fn exactly_at_limit_unchanged() {
        let msgs: Vec<_> = (0..5).map(|i| user(&format!("{i}"))).collect();
        let out = compact_by_count(&msgs, 5);
        assert_eq!(out.len(), 5);
    }

    #[test]
    fn over_budget_shrinks_and_notices() {
        let msgs: Vec<_> = (0..10).map(|i| user(&format!("m{i}"))).collect();
        let out = compact_by_count(&msgs, 4);
        // 1 summary + 3 kept = 4
        assert_eq!(out.len(), 4);
        match &out[0] {
            AgentMessage::User(u) => match &u.content[0] {
                ContentBlock::Text { text } => {
                    assert!(text.contains("compacted 7 earlier messages"));
                }
                _ => panic!(),
            },
            _ => panic!("expected summary user message"),
        }
        // Tail preserved: m7, m8, m9
        match &out[3] {
            AgentMessage::User(u) => match &u.content[0] {
                ContentBlock::Text { text } => assert_eq!(text, "m9"),
                _ => panic!(),
            },
            _ => panic!(),
        }
    }

    #[test]
    fn max_zero_is_noop() {
        let msgs = vec![user("a"), user("b"), user("c")];
        assert_eq!(compact_by_count(&msgs, 0).len(), 3);
    }

    #[test]
    fn max_one_is_summary_only() {
        let msgs = vec![user("a"), user("b")];
        let out = compact_by_count(&msgs, 1);
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0], AgentMessage::User(_)));
    }

    #[test]
    fn env_default() {
        assert_eq!(
            max_messages_from(&|_| Err(std::env::VarError::NotPresent)),
            DEFAULT_MAX_MESSAGES
        );
        assert_eq!(
            max_messages_from(&|k| {
                if k == "GASKET_COMPACT_MAX_MESSAGES" {
                    Ok("12".into())
                } else {
                    Err(std::env::VarError::NotPresent)
                }
            }),
            12
        );
    }
}
