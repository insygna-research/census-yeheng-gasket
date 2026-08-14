//! In-memory history compaction.
//!
//! Pure host policy: shrinks the working transcript before `run_agent_loop`.
//! Does not rewrite JSONL, no LLM summary. One algorithm ([`compact_by_count`]:
//! drop oldest whole atomic groups, prepend a notice); two triggers via
//! [`ContextBudget`]: token-aware (provider-reported `usage.input_tokens`
//! over `threshold_pct` of `window`) or a message-count fallback.

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

/// Partition `messages` into atomic `[start, end)` groups that must never be
/// split across a compaction boundary. An `Assistant` opens a group and
/// absorbs any immediately-following `ToolResult` messages into the same
/// group; every other message forms its own singleton group.
fn atomic_groups(messages: &[AgentMessage]) -> Vec<(usize, usize)> {
    let mut groups = Vec::new();
    let mut i = 0;
    while i < messages.len() {
        let start = i;
        if matches!(messages[i], AgentMessage::Assistant(_)) {
            i += 1;
            while i < messages.len() && matches!(messages[i], AgentMessage::ToolResult(_)) {
                i += 1;
            }
        } else {
            i += 1;
        }
        groups.push((start, i));
    }
    groups
}

/// If `messages.len() > max_messages`, keep the newest groups whose total
/// message count fits in `max_messages - 1` (one slot reserved for the
/// summary notice) and prepend one user notice naming how many were dropped.
/// Groups are kept whole, so an `Assistant(tool_call)` is never separated from
/// its trailing `ToolResult`s.
///
/// Under budget (or `max_messages == 0`): clone unchanged.
pub fn compact_by_count(messages: &[AgentMessage], max_messages: usize) -> Vec<AgentMessage> {
    if max_messages == 0 || messages.len() <= max_messages {
        return messages.to_vec();
    }

    let groups = atomic_groups(messages);
    let budget = max_messages.saturating_sub(1);

    // Walk groups from newest backwards, accumulating whole groups until the
    // running message count would exceed the budget. Always keep at least the
    // final group.
    let mut kept_msg_count = 0;
    let mut first_kept_group = groups.len();
    for (idx, &(start, end)) in groups.iter().enumerate().rev() {
        let group_len = end - start;
        if idx < groups.len() - 1 && kept_msg_count + group_len > budget {
            break;
        }
        kept_msg_count += group_len;
        first_kept_group = idx;
    }

    let start = groups[first_kept_group].0;
    let dropped = start;
    if dropped == 0 {
        return messages.to_vec();
    }

    let mut out = Vec::with_capacity(kept_msg_count + 1);
    out.push(AgentMessage::User(UserMessage {
        content: vec![ContentBlock::text(format!(
            "[compacted {dropped} earlier messages]"
        ))],
        timestamp: gasket_core::now(),
    }));
    out.extend_from_slice(&messages[start..]);
    out
}

/// Parse `key` from the lookup as `T`, falling back to `default` on miss or
/// parse failure. Mirrors the helper in `gasket-core/src/types/context.rs` so
/// `compact.rs` stays self-contained.
fn env_parse<T: std::str::FromStr>(
    lookup: &dyn Fn(&str) -> Result<String, std::env::VarError>,
    key: &str,
    default: T,
) -> T {
    lookup(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

/// Token-aware compaction trigger with hysteresis.
///
/// Uses the provider-reported `usage.input_tokens` (fed in via
/// [`record_input_tokens`](Self::record_input_tokens)) to decide when the
/// working transcript has grown past `threshold_pct` of the context `window`.
/// When over threshold, [`compact`](Self::compact) retains `target_pct`% of
/// the current messages — a proportional reduction. Core ships no tokenizer,
/// so it does not pretend to model per-message token cost; it reuses
/// [`compact_by_count`]'s greedy group-walking algorithm for both paths.
///
/// When no usage has been recorded (`last_input_tokens == 0`), compaction
/// falls back to a fixed message-count cap (`fallback_max_messages`).
#[derive(Clone)]
pub struct ContextBudget {
    /// Model context window (`GASKET_CONTEXT_WINDOW`, default 128_000).
    window: u64,
    /// Compaction trigger as a percentage of `window`
    /// (`GASKET_COMPACT_THRESHOLD_PCT`, default 80).
    threshold_pct: u8,
    /// Post-compaction retention as a percentage of the current message count
    /// (`GASKET_COMPACT_TARGET_PCT`, default 50).
    target_pct: u8,
    /// Message-count fallback when no usage is available
    /// (`GASKET_COMPACT_MAX_MESSAGES`, default 80).
    fallback_max_messages: usize,
    /// Most recent `usage.input_tokens` reported by the provider.
    last_input_tokens: u64,
}

impl ContextBudget {
    /// Read all knobs from the process environment.
    pub fn from_env() -> Self {
        Self::from_env_with(&|k| std::env::var(k))
    }

    /// Same as [`from_env`](Self::from_env) but with an injectable lookup -
    /// used by tests to avoid mutating process env.
    pub fn from_env_with(lookup: &dyn Fn(&str) -> Result<String, std::env::VarError>) -> Self {
        Self {
            window: env_parse(lookup, "GASKET_CONTEXT_WINDOW", 128_000),
            threshold_pct: env_parse(lookup, "GASKET_COMPACT_THRESHOLD_PCT", 80),
            target_pct: env_parse(lookup, "GASKET_COMPACT_TARGET_PCT", 50),
            fallback_max_messages: max_messages_from(lookup),
            last_input_tokens: 0,
        }
    }

    /// Feed the provider-reported input-token count for this turn.
    pub fn record_input_tokens(&mut self, n: u64) {
        self.last_input_tokens = n;
    }

    /// Current input-token occupancy (most recent provider report).
    pub fn current_tokens(&self) -> u64 {
        self.last_input_tokens
    }

    /// True when `last_input_tokens` exceeds `threshold_pct` of `window`.
    pub fn needs_compaction(&self) -> bool {
        self.last_input_tokens > self.window * self.threshold_pct as u64 / 100
    }

    /// Compact `messages` according to the budget.
    ///
    /// - No usage recorded: fall back to [`compact_by_count`] with
    ///   `fallback_max_messages`.
    /// - Under threshold: return unchanged.
    /// - Over threshold: [`compact_by_count`] retaining `target_pct`% of the
    ///   current message count. One algorithm, two triggers.
    pub fn compact(&self, messages: &[AgentMessage]) -> Vec<AgentMessage> {
        if self.last_input_tokens == 0 {
            return compact_by_count(messages, self.fallback_max_messages);
        }
        if !self.needs_compaction() {
            return messages.to_vec();
        }
        // Token pressure triggered. Core has no tokenizer, so a proportional
        // message-count reduction is the honest target — it reuses the same
        // greedy group-walker as the count-based path instead of a second
        // compaction strategy that would pretend to know per-group token cost.
        let target = (messages.len() * self.target_pct as usize / 100).max(1);
        compact_by_count(messages, target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gasket_core::types::message::{FunctionCall, ToolCall};
    use gasket_core::{AssistantMessage, StopReason, ToolResultMessage};

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

    /// Assistant carrying a single tool call with the given id.
    fn assistant_with_tool(id: &str) -> AgentMessage {
        AgentMessage::Assistant(AssistantMessage {
            content: vec![ContentBlock::ToolCall {
                tool_call: ToolCall {
                    id: id.into(),
                    function: FunctionCall {
                        name: "f".into(),
                        arguments: "{}".into(),
                    },
                },
            }],
            model: "m".into(),
            stop_reason: StopReason::ToolUse,
            usage: None,
            timestamp: 1,
        })
    }

    /// ToolResult answering `tool_call_id`.
    fn result(id: &str) -> AgentMessage {
        AgentMessage::ToolResult(ToolResultMessage {
            tool_call_id: id.into(),
            tool_name: "f".into(),
            content: vec![ContentBlock::text("ok")],
            is_error: false,
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
    fn max_one_keeps_last_group_plus_notice() {
        // max=1 leaves zero budget for real messages, but the last atomic
        // group is always kept (never dropped entirely), so the result is the
        // summary notice followed by the final message.
        let msgs = vec![user("a"), user("b")];
        let out = compact_by_count(&msgs, 1);
        assert_eq!(out.len(), 2);
        assert!(matches!(out[0], AgentMessage::User(_)));
        match &out[1] {
            AgentMessage::User(u) => match &u.content[0] {
                ContentBlock::Text { text } => assert_eq!(text, "b"),
                _ => panic!(),
            },
            _ => panic!("expected last message preserved"),
        }
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

    #[test]
    fn never_splits_tool_call_from_result() {
        // [user, asst(tc=t1), result(t1), user, asst(tc=t2), result(t2), user]
        let msgs = vec![
            user("u1"),
            assistant_with_tool("t1"),
            result("t1"),
            user("u2"),
            assistant_with_tool("t2"),
            result("t2"),
            user("u3"),
        ];
        let out = compact_by_count(&msgs, 4);

        // Collect tool_call ids and result ids from the kept tail (skip the
        // leading summary notice).
        let mut call_ids: Vec<String> = Vec::new();
        let mut result_ids: Vec<String> = Vec::new();
        for m in out.iter().skip(1) {
            match m {
                AgentMessage::Assistant(a) => {
                    for b in &a.content {
                        if let ContentBlock::ToolCall { tool_call } = b {
                            call_ids.push(tool_call.id.clone());
                        }
                    }
                }
                AgentMessage::ToolResult(r) => result_ids.push(r.tool_call_id.clone()),
                _ => {}
            }
        }
        // Every tool call has a matching result and vice versa: no orphans.
        for id in &call_ids {
            assert!(
                result_ids.contains(id),
                "orphan tool_call {id} has no result"
            );
        }
        for id in &result_ids {
            assert!(call_ids.contains(id), "orphan tool_result {id} has no call");
        }
    }

    #[test]
    fn needs_compaction_uses_real_tokens() {
        let mut budget = ContextBudget {
            window: 100_000,
            threshold_pct: 80,
            target_pct: 50,
            fallback_max_messages: 80,
            last_input_tokens: 0,
        };
        budget.record_input_tokens(70_000);
        assert!(!budget.needs_compaction());
        budget.record_input_tokens(85_000);
        assert!(budget.needs_compaction());
    }

    #[test]
    fn compact_drops_groups_under_target() {
        let msgs: Vec<_> = (0..10).map(|i| user(&format!("m{i}"))).collect();
        let budget = ContextBudget {
            window: 100_000,
            threshold_pct: 80,
            target_pct: 50,
            fallback_max_messages: 80,
            last_input_tokens: 100_000,
        };
        let out = budget.compact(&msgs);
        // Token-triggered → retain target_pct% (50%) of 10 messages as the
        // count budget = 5; compact_by_count reserves 1 for the notice, so it
        // keeps 4 (m6..m9) + 1 notice = 5 total, dropping 6.
        assert_eq!(out.len(), 5);
        match &out[0] {
            AgentMessage::User(u) => match &u.content[0] {
                ContentBlock::Text { text } => assert!(text.contains("compacted 6")),
                _ => panic!(),
            },
            _ => panic!("expected summary user message"),
        }
    }

    #[test]
    fn compact_falls_back_when_no_usage() {
        let msgs: Vec<_> = (0..10).map(|i| user(&format!("m{i}"))).collect();
        let budget = ContextBudget {
            window: 100_000,
            threshold_pct: 80,
            target_pct: 50,
            fallback_max_messages: 4,
            last_input_tokens: 0,
        };
        let out = budget.compact(&msgs);
        // fallback: compact_by_count(msgs, 4) = 1 notice + 3 kept = 4
        assert_eq!(out.len(), 4);
    }

    #[test]
    fn compact_under_threshold_is_noop() {
        let msgs: Vec<_> = (0..5).map(|i| user(&format!("m{i}"))).collect();
        let budget = ContextBudget {
            window: 100_000,
            threshold_pct: 80,
            target_pct: 50,
            fallback_max_messages: 80,
            last_input_tokens: 50_000,
        };
        let out = budget.compact(&msgs);
        assert_eq!(out.len(), 5);
        // Clone, not prepended with a notice: first message is still m0.
        match &out[0] {
            AgentMessage::User(u) => match &u.content[0] {
                ContentBlock::Text { text } => assert_eq!(text, "m0"),
                _ => panic!(),
            },
            _ => panic!("expected original first message"),
        }
    }

    #[test]
    fn compact_never_splits_tool_pair() {
        // [user, asst(tc=t1), result(t1), user, asst(tc=t2), result(t2), user]
        let msgs = vec![
            user("u1"),
            assistant_with_tool("t1"),
            result("t1"),
            user("u2"),
            assistant_with_tool("t2"),
            result("t2"),
            user("u3"),
        ];
        let budget = ContextBudget {
            window: 100_000,
            threshold_pct: 80,
            target_pct: 50,
            fallback_max_messages: 80,
            last_input_tokens: 100_000,
        };
        let out = budget.compact(&msgs);

        // Collect tool_call ids and result ids from the kept tail (skip the
        // leading summary notice).
        let mut call_ids: Vec<String> = Vec::new();
        let mut result_ids: Vec<String> = Vec::new();
        for m in out.iter().skip(1) {
            match m {
                AgentMessage::Assistant(a) => {
                    for b in &a.content {
                        if let ContentBlock::ToolCall { tool_call } = b {
                            call_ids.push(tool_call.id.clone());
                        }
                    }
                }
                AgentMessage::ToolResult(r) => result_ids.push(r.tool_call_id.clone()),
                _ => {}
            }
        }
        // Every tool call has a matching result and vice versa: no orphans.
        for id in &call_ids {
            assert!(
                result_ids.contains(id),
                "orphan tool_call {id} has no result"
            );
        }
        for id in &result_ids {
            assert!(call_ids.contains(id), "orphan tool_result {id} has no call");
        }
    }
}
