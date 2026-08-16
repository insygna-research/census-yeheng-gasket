//! Offline end-to-end integration tests for the event-sourced host pipeline
//! (Host, ConfigLoader, SessionManager, PermissionPolicy, run_agent_loop +
//! persist callback, EventStorage) with a deterministic FakeStream: no
//! network, no LLM keys, CI-mandatory.
//!
//! The log under `<root>/<session>/events.jsonl` is the single source of
//! truth: `run_turn` derives history from it, the loop persists each
//! Assistant/ToolResult as it happens, and the host frames each turn with
//! TurnStart/User/TurnEnd. Legacy `messages.jsonl` migrates once, in full,
//! and is then deleted.
mod common;

use std::sync::atomic::Ordering;
use std::sync::Arc;

use common::FakeStream;
use conga::{
    derive_messages, AgentMessage, ContentBlock, EventStorage, SessionEvent, StreamChunk,
    TurnEndReason, UserMessage,
};
use conga_host::{
    ConfigLoader, EventPrinter, Host, HostConfig, Mode, PermissionPolicy, SessionManager,
    TurnSummary,
};

fn fake_env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Result<String, std::env::VarError> {
    let map: std::collections::HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |k: &str| map.get(k).cloned().ok_or(std::env::VarError::NotPresent)
}

fn test_cfg(retry_off: bool) -> HostConfig {
    let mut pairs = vec![
        ("CONGA_LLM_BASE_URL", "https://api.test/v1"),
        ("CONGA_LLM_KEY", "sk-test"),
        ("CONGA_LLM_MODEL", "m"),
    ];
    if retry_off {
        pairs.push(("CONGA_RETRY_MAX", "0"));
    }
    ConfigLoader::load_with(&fake_env(&pairs)).unwrap()
}

fn user_msg(text: &str) -> AgentMessage {
    AgentMessage::User(UserMessage {
        content: vec![ContentBlock::text(text.to_string())],
        timestamp: conga::now(),
    })
}

fn full_auto_policy() -> PermissionPolicy {
    PermissionPolicy::new(Mode::FullAuto, Arc::new(|_, _| Box::pin(async { false })))
}

/// A `write` tool-call script chunk (args streamed as one delta, like the
/// FakeStream's other scripts).
fn write_call(id: &str, path: &str, content: &str) -> StreamChunk {
    StreamChunk::ToolCallDelta {
        index: None,
        id: id.into(),
        name: Some("write".into()),
        args_delta: serde_json::json!({ "path": path, "content": content }).to_string(),
    }
}

/// The reason of the log's only TurnEnd event.
fn turn_end_reason(events: &[SessionEvent]) -> TurnEndReason {
    let ends: Vec<&TurnEndReason> = events
        .iter()
        .filter_map(|ev| match ev {
            SessionEvent::TurnEnd { reason } => Some(reason),
            _ => None,
        })
        .collect();
    assert_eq!(ends.len(), 1, "expected exactly one TurnEnd, got {ends:?}");
    ends[0].clone()
}

/// The reason this plan exists: a tool side effect that already happened must
/// survive a turn whose second stream errors mid-flight. The log keeps every
/// fact (user, assistant, tool result) and ends with TurnEnd=Error — never a
/// silently adopted "fresh" session.
#[tokio::test]
async fn mid_turn_failure_preserves_side_effect() {
    let tmp = tempfile::tempdir().unwrap();
    // The `write` tool resolves within the process cwd, so the side-effect
    // target must live in a dir under it.
    let work = tempfile::tempdir_in(".").unwrap();
    let rel = format!(
        "{}/side-effect.txt",
        work.path().file_name().unwrap().to_string_lossy()
    );

    let session = SessionManager::with_root(tmp.path().to_path_buf());
    let fake = FakeStream::new(vec![
        vec![write_call("t1", &rel, "precious"), StreamChunk::Done],
        // Mid-stream error AFTER content was emitted: not retried (would
        // duplicate partial output), surfaced as stop_reason::Error.
        vec![
            StreamChunk::TextDelta("partial".into()),
            StreamChunk::Error("boom".into()),
        ],
    ]);
    let host = Host::new(
        test_cfg(true),
        session,
        Arc::new(full_auto_policy()),
        "sys".into(),
        conga::built_in_tools(),
    )
    .with_stream_fn(Arc::new(fake));

    let summary = host
        .run_turn("write the file please", |_| {})
        .await
        .expect("a mid-stream error surfaces as a message; it must not fail the turn");

    // The side effect happened — nothing may erase that fact.
    assert!(
        std::path::Path::new(&rel).exists(),
        "tool side effect must exist on disk after the failed turn"
    );
    assert!(
        matches!(summary.reason, TurnEndReason::Error { .. }),
        "expected TurnEnd::Error, got {:?}",
        summary.reason
    );

    // Reopen: the derived history keeps user + assistant + tool result.
    let sid = host.session().current_id().to_string();
    let reopen = SessionManager::with_root(tmp.path().to_path_buf());
    let events = reopen.open_or_migrate(&sid).await.unwrap();
    let msgs = derive_messages(&events);
    assert!(
        msgs.iter().any(|m| matches!(m, AgentMessage::User(_))),
        "user message missing from derived history"
    );
    assert!(
        msgs.iter().any(|m| matches!(m, AgentMessage::Assistant(_))),
        "assistant message missing from derived history"
    );
    assert!(
        msgs.iter()
            .any(|m| matches!(m, AgentMessage::ToolResult(_))),
        "completed tool result missing from derived history"
    );
    assert!(matches!(
        turn_end_reason(&events),
        TurnEndReason::Error { .. }
    ));
}

/// A cooperative abort between tool executions persists the partial facts:
/// the completed ToolResult is in the log and TurnEnd{Aborted} is written.
#[tokio::test]
async fn aborted_turn_persists_partial_facts() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir_in(".").unwrap();
    let rel = format!(
        "{}/aborted.txt",
        work.path().file_name().unwrap().to_string_lossy()
    );

    let session = SessionManager::with_root(tmp.path().to_path_buf());
    // One tool-call script: after its ToolExecutionEnd fires we set the abort
    // signal, so the next provider request is skipped (StopReason::Aborted).
    let fake = FakeStream::new(vec![vec![
        write_call("t1", &rel, "done"),
        StreamChunk::Done,
    ]]);
    let host = Host::new(
        test_cfg(false),
        session,
        Arc::new(full_auto_policy()),
        "sys".into(),
        conga::built_in_tools(),
    )
    .with_stream_fn(Arc::new(fake));

    let signal = host.signal().clone();
    let summary = host
        .run_turn("go", move |ev| {
            if matches!(ev, conga::AgentEvent::ToolExecutionEnd { .. }) {
                signal.store(true, Ordering::Relaxed);
            }
        })
        .await
        .expect("an aborted turn returns Ok with partial facts");

    assert!(
        matches!(summary.reason, TurnEndReason::Aborted { .. }),
        "expected TurnEnd::Aborted, got {:?}",
        summary.reason
    );
    assert!(
        std::path::Path::new(&rel).exists(),
        "the tool that completed before the abort must have run"
    );

    let sid = host.session().current_id().to_string();
    let events = EventStorage::new(tmp.path())
        .load_events(&sid)
        .await
        .unwrap();
    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, SessionEvent::ToolResult(_))),
        "completed tool result must be persisted"
    );
    assert!(matches!(
        turn_end_reason(&events),
        TurnEndReason::Aborted { .. }
    ));
}

/// A full successful turn: the event log projects to exactly the messages the
/// loop returned (which is what the legacy history+new_msgs concatenation
/// used to be), framed by TurnStart/User/TurnEnd{Completed}.
#[tokio::test]
async fn success_path_log_equals_legacy_messages() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir_in(".").unwrap();
    let rel = format!(
        "{}/ok.txt",
        work.path().file_name().unwrap().to_string_lossy()
    );

    let session = SessionManager::with_root(tmp.path().to_path_buf());
    let fake = FakeStream::new(vec![
        vec![write_call("t1", &rel, "data"), StreamChunk::Done],
        vec![
            StreamChunk::TextDelta("done".into()),
            StreamChunk::Usage {
                input: 10,
                output: 2,
            },
            StreamChunk::Done,
        ],
    ]);
    let host = Host::new(
        test_cfg(false),
        session,
        Arc::new(full_auto_policy()),
        "You are a helpful assistant.".into(),
        conga::built_in_tools(),
    )
    .with_stream_fn(Arc::new(fake));

    let mut buf: Vec<u8> = Vec::new();
    let summary: TurnSummary = host
        .run_turn("write it", |ev| {
            EventPrinter::new(&mut buf).on_event(&ev);
        })
        .await
        .expect("success turn");
    assert!(matches!(summary.reason, TurnEndReason::Completed));
    assert!(
        String::from_utf8_lossy(&buf).contains("done"),
        "printer must render the streamed text"
    );

    let sid = host.session().current_id().to_string();
    let events = EventStorage::new(tmp.path())
        .load_events(&sid)
        .await
        .unwrap();

    // Framing: TurnStart first, User second, TurnEnd{Completed} last.
    assert!(matches!(events.first(), Some(SessionEvent::TurnStart)));
    assert!(matches!(events.get(1), Some(SessionEvent::User(_))));
    assert!(matches!(events.last(), Some(SessionEvent::TurnEnd { .. })));

    // The projection equals the loop's returned messages — i.e. the legacy
    // "history + new_msgs" list the old API assembled in memory.
    assert_eq!(
        derive_messages(&events),
        summary.new_messages,
        "derived history must equal the loop's new messages"
    );
    // And it is the full legacy shape: user, assistant(tool call), tool
    // result, closing assistant.
    let kinds: Vec<&str> = summary
        .new_messages
        .iter()
        .map(|m| match m {
            AgentMessage::User(_) => "user",
            AgentMessage::Assistant(_) => "assistant",
            AgentMessage::ToolResult(_) => "toolresult",
            AgentMessage::Custom(_) => "custom",
        })
        .collect();
    assert_eq!(kinds, vec!["user", "assistant", "toolresult", "assistant"]);

    // Usage travels with the persisted assistant event (restart-safe budget).
    let last_usage = events.iter().rev().find_map(|ev| match ev {
        SessionEvent::Assistant { usage, .. } => *usage,
        _ => None,
    });
    assert_eq!(
        last_usage.map(|u| u.input_tokens),
        Some(10),
        "assistant usage must be persisted for token-aware compaction restarts"
    );
}

/// The 400-repair regression: an abort between two calls of one batch
/// leaves the assistant's second tool call unanswered in the log. The next
/// turn's provider request must carry a synthesized error result for it —
/// OpenAI-compat APIs reject `tool_calls` without matching `tool` messages.
#[tokio::test]
async fn next_turn_request_answers_every_tool_call() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir_in(".").unwrap();
    let rel = |name: &str| {
        format!(
            "{}/{name}",
            work.path().file_name().unwrap().to_string_lossy()
        )
    };

    let session = SessionManager::with_root(tmp.path().to_path_buf());
    // Turn 1: one batch of two write calls; the test flips the abort signal
    // after the FIRST ToolExecutionEnd, so t2 never executes. The next
    // provider request is skipped by the abort (one stream() call total).
    // Turn 2: plain text answer (second stream() call) — its request is
    // what we assert on.
    let fake = Arc::new(FakeStream::new(vec![
        vec![
            write_call("t1", &rel("a.txt"), "one"),
            write_call("t2", &rel("b.txt"), "two"),
            StreamChunk::Done,
        ],
        vec![StreamChunk::TextDelta("ok".into()), StreamChunk::Done],
    ]));
    let host = Host::new(
        test_cfg(false),
        session,
        Arc::new(full_auto_policy()),
        "sys".into(),
        conga::built_in_tools(),
    )
    .with_stream_fn(fake.clone());

    let signal = host.signal().clone();
    let seen_ends = std::sync::atomic::AtomicU32::new(0);
    let summary = host
        .run_turn("write both", |ev| {
            if matches!(ev, conga::AgentEvent::ToolExecutionEnd { .. }) {
                let n = seen_ends.fetch_add(1, Ordering::Relaxed);
                if n == 0 {
                    signal.store(true, Ordering::Relaxed);
                }
            }
        })
        .await
        .expect("aborted turn returns Ok");
    assert!(
        matches!(summary.reason, TurnEndReason::Aborted { .. }),
        "expected abort, got {:?}",
        summary.reason
    );

    let summary2 = host
        .run_turn("continue", |_| {})
        .await
        .expect("second turn");
    assert!(matches!(summary2.reason, TurnEndReason::Completed));

    // The provider requests the host actually assembled, in call order.
    let requests = fake.seen();
    assert_eq!(requests.len(), 2, "abort skipped the follow-up request");

    // Every assistant tool_call in the turn-2 request is answered by a
    // tool result — the OpenAI-compat 400 contract.
    let mut pending: Vec<String> = Vec::new();
    for msg in &requests[1] {
        match msg {
            AgentMessage::Assistant(a) => {
                for b in &a.content {
                    if let ContentBlock::ToolCall { tool_call: tc } = b {
                        pending.push(tc.id.clone());
                    }
                }
            }
            AgentMessage::ToolResult(tr) => {
                let i = pending
                    .iter()
                    .position(|id| id == &tr.tool_call_id)
                    .expect("tool result without a pending tool call");
                pending.remove(i);
            }
            _ => {}
        }
    }
    assert!(
        pending.is_empty(),
        "turn-2 request still has unanswered tool calls: {pending:?}"
    );
    // And the synthesized t2 result is an error result placed after t1's.
    let turn2_results: Vec<&conga::ToolResultMessage> = requests[1]
        .iter()
        .filter_map(|m| match m {
            AgentMessage::ToolResult(tr) => Some(tr),
            _ => None,
        })
        .collect();
    let ids: Vec<&str> = turn2_results
        .iter()
        .map(|tr| tr.tool_call_id.as_str())
        .collect();
    assert_eq!(ids, vec!["t1", "t2"], "t1 real, t2 synthesized, in order");
    assert!(turn2_results[1].is_error, "synthesized result is an error");

    // The on-disk log keeps the partial facts — the repair is in-memory
    // only, never written back.
    let sid = host.session().current_id().to_string();
    let events = EventStorage::new(tmp.path())
        .load_events(&sid)
        .await
        .unwrap();
    let answered: Vec<&str> = events
        .iter()
        .filter_map(|ev| match ev {
            SessionEvent::ToolResult(AgentMessage::ToolResult(tr)) => {
                Some(tr.tool_call_id.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(answered, vec!["t1"], "t2 must stay unanswered on disk");
}

/// One-shot legacy migration: messages.jsonl is wrapped into events.jsonl in
/// full, deleted only after the full write, and a second open loads
/// events.jsonl directly (idempotent).
#[tokio::test]
async fn legacy_messages_migrate_once_and_delete_legacy() {
    let tmp = tempfile::tempdir().unwrap();
    let sid = "legacy1";
    let dir = tmp.path().join(sid);
    std::fs::create_dir_all(&dir).unwrap();

    // A legacy transcript: user + assistant rows.
    let legacy = vec![user_msg("hello"), user_msg("again")];
    let mut raw = String::new();
    for m in &legacy {
        raw.push_str(&serde_json::to_string(m).unwrap());
        raw.push('\n');
    }
    std::fs::write(dir.join("messages.jsonl"), raw).unwrap();

    let sm = SessionManager::with_root(tmp.path().to_path_buf());
    let events = sm.open_or_migrate(sid).await.unwrap();
    assert_eq!(
        derive_messages(&events),
        legacy,
        "migrated log must project to the legacy messages"
    );
    assert!(
        dir.join("events.jsonl").exists(),
        "events.jsonl must exist after migration"
    );
    assert!(
        !dir.join("events.jsonl.tmp").exists(),
        "the atomic-install staging file must not survive the migration"
    );
    assert!(
        !dir.join("messages.jsonl").exists(),
        "legacy messages.jsonl must be deleted after a successful migration (D1)"
    );

    // Second open: loads events.jsonl directly, no legacy to touch.
    let events2 = sm.open_or_migrate(sid).await.unwrap();
    assert_eq!(events, events2, "second open must be idempotent");
    assert!(!dir.join("messages.jsonl").exists());
}

/// Corruption fails closed: a mid-file bad row makes open_or_migrate return
/// Err — the old adopt-and-start-fresh behavior is gone, and a failed
/// migration leaves the legacy file untouched.
#[tokio::test]
async fn corrupted_session_errors_instead_of_adopting() {
    let tmp = tempfile::tempdir().unwrap();

    // Corrupt events.jsonl (bad middle line): has_events → load_events → Err.
    let sid = "broken-events";
    let dir = tmp.path().join(sid);
    std::fs::create_dir_all(&dir).unwrap();
    let good = serde_json::to_string(&SessionEvent::User(user_msg("a"))).unwrap();
    let mut raw = String::new();
    raw.push_str(&good);
    raw.push('\n');
    raw.push_str("{\"type\":\"???\"\n"); // torn/corrupt middle row
    raw.push_str(&good);
    raw.push('\n');
    std::fs::write(dir.join("events.jsonl"), raw).unwrap();
    let sm = SessionManager::with_root(tmp.path().to_path_buf());
    assert!(
        sm.open_or_migrate(sid).await.is_err(),
        "corrupt events.jsonl must fail closed, not adopt"
    );

    // Corrupt legacy messages.jsonl (bad middle line): migration input is
    // unreadable → Err, and the legacy file must be left untouched.
    let sid2 = "broken-legacy";
    let dir2 = tmp.path().join(sid2);
    std::fs::create_dir_all(&dir2).unwrap();
    let mut raw2 = String::new();
    raw2.push_str(&serde_json::to_string(&user_msg("a")).unwrap());
    raw2.push('\n');
    raw2.push_str("{\"role\":\"??\"\n");
    raw2.push_str(&serde_json::to_string(&user_msg("b")).unwrap());
    raw2.push('\n');
    std::fs::write(dir2.join("messages.jsonl"), raw2).unwrap();
    let sm2 = SessionManager::with_root(tmp.path().to_path_buf());
    assert!(
        sm2.open_or_migrate(sid2).await.is_err(),
        "corrupt legacy transcript must fail closed, not adopt"
    );
    assert!(
        dir2.join("messages.jsonl").exists(),
        "a failed migration must leave the legacy file untouched"
    );
    assert!(
        !dir2.join("events.jsonl").exists(),
        "no partial events.jsonl may be written from a corrupt source"
    );
}

/// Mid-turn compaction through the `transform_context` seam: within ONE
/// turn the working transcript grows with every assistant+tool-result
/// pair, and the wire view handed to the provider must be re-compacted
/// before EVERY call — not just once at turn start.
#[tokio::test]
async fn run_turn_compacts_before_every_llm_call() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir_in(".").unwrap();
    let rel = format!(
        "{}/c.txt",
        work.path().file_name().unwrap().to_string_lossy()
    );

    let session = SessionManager::with_root(tmp.path().to_path_buf());
    let fake = Arc::new(FakeStream::new(vec![
        vec![write_call("t1", &rel, "one"), StreamChunk::Done],
        vec![write_call("t2", &rel, "two"), StreamChunk::Done],
        vec![StreamChunk::TextDelta("done".into()), StreamChunk::Done],
    ]));
    let stream_fn: Arc<dyn conga::StreamFn> = fake.clone();
    // No recorded usage → count-fallback path with a tiny cap of 3.
    let budget =
        conga_host::ContextBudget::from_env_with(&fake_env(&[("CONGA_COMPACT_MAX_MESSAGES", "3")]));
    let host = Host::new(
        test_cfg(true),
        session,
        Arc::new(full_auto_policy()),
        "sys".into(),
        conga::built_in_tools(),
    )
    .with_stream_fn(stream_fn)
    .with_budget(budget);

    let summary = host.run_turn("go", |_| {}).await.unwrap();
    assert!(matches!(summary.reason, TurnEndReason::Completed));

    let seen = fake.seen();
    assert_eq!(seen.len(), 3, "three LLM calls expected");
    // Call 1: history is just the user prompt.
    assert_eq!(seen[0].len(), 1);
    // Call 2: user + assistant + tool_result = 3, still within cap.
    assert_eq!(seen[1].len(), 3);
    // Call 3: history is 5 (user + 2×(assistant+tool_result)); the wire
    // view must be compacted back to the cap, notice included — under the
    // old compact-once-at-turn-start behavior this call saw all 5.
    assert!(
        seen[2].len() <= 3,
        "third call must be compacted, got {}",
        seen[2].len()
    );
    let notice = match &seen[2][0] {
        AgentMessage::User(u) => match &u.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text"),
        },
        _ => panic!("expected compaction notice first, got {:?}", seen[2][0]),
    };
    assert!(
        notice.starts_with("[compacted"),
        "expected compaction notice, got: {notice}"
    );
    // The on-disk log keeps the FULL transcript: every assistant message,
    // uncompacted — the seam is a wire view only.
    let sid = host.session().current_id().to_string();
    let reopen = SessionManager::with_root(tmp.path().to_path_buf());
    let events = reopen.open_or_migrate(&sid).await.unwrap();
    let assistants = events
        .iter()
        .filter(|ev| matches!(ev, SessionEvent::Assistant { .. }))
        .count();
    assert_eq!(assistants, 3, "log keeps every assistant, uncompacted");
}
