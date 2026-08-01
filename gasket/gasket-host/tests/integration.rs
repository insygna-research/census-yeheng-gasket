//! Offline end-to-end integration tests driving the full host pipeline
//! (Host, ConfigLoader, SessionManager, PermissionPolicy, EventPrinter,
//! run_agent_loop) with a deterministic FakeStream: no network, no LLM keys,
//! CI-mandatory.
mod common;

use std::sync::Arc;

use common::FakeStream;
use gasket_core::{AgentEvent, AgentMessage, ContentBlock, StreamChunk, UserMessage};
use gasket_host::{
    ConfigLoader, ContextBudget, EventPrinter, Host, HostConfig, Mode, PermissionPolicy,
    SessionManager,
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
        ("GASKET_LLM_BASE_URL", "https://api.test/v1"),
        ("GASKET_LLM_KEY", "sk-test"),
        ("GASKET_LLM_MODEL", "m"),
    ];
    if retry_off {
        pairs.push(("GASKET_RETRY_MAX", "0"));
    }
    ConfigLoader::load_with(&fake_env(&pairs)).unwrap()
}

fn user_msg(text: &str) -> AgentMessage {
    AgentMessage::User(UserMessage {
        content: vec![ContentBlock::text(text.to_string())],
        timestamp: gasket_core::now(),
    })
}

fn full_auto_policy() -> PermissionPolicy {
    PermissionPolicy::new(Mode::FullAuto, Arc::new(|_, _| Box::pin(async { false })))
}

/// Basic chat: one text script. Asserts the assistant message is produced,
/// the printer renders it, and the transcript is persisted to JSONL.
#[tokio::test]
async fn host_basic_chat() {
    let tmp = tempfile::tempdir().unwrap();
    let session = SessionManager::with_root(tmp.path().to_path_buf());
    let fake = FakeStream::new(vec![vec![
        StreamChunk::TextDelta("pong".into()),
        StreamChunk::Usage {
            input: 1,
            output: 1,
        },
        StreamChunk::Done,
    ]]);
    let mut host = Host::new(
        test_cfg(false),
        session,
        Arc::new(full_auto_policy()),
        "You are a helpful assistant.".into(),
        vec![],
    )
    .with_stream_fn(Arc::new(fake));

    let history: Vec<AgentMessage> = Vec::new();
    let mut buf: Vec<u8> = Vec::new();
    let new_msgs = host
        .run_turn(user_msg("hello"), &history, |ev| {
            EventPrinter::new(&mut buf).on_event(&ev);
        })
        .await
        .expect("basic chat turn should succeed");

    assert!(
        new_msgs.iter().any(|m| matches!(
            m,
            AgentMessage::Assistant(a) if a.content.iter().any(|b| matches!(b, ContentBlock::Text { text } if text == "pong"))
        )),
        "expected an assistant message with the streamed text"
    );
    let out = String::from_utf8_lossy(&buf);
    assert!(
        out.contains("pong"),
        "printer must render the streamed text, got: {out}"
    );

    // Persisted: messages.jsonl exists under the current session dir.
    let sid = host.session().current_id().to_string();
    let path = tmp.path().join(&sid).join("messages.jsonl");
    let raw = std::fs::read_to_string(&path).expect("session transcript must be persisted");
    assert!(
        raw.contains("pong"),
        "persisted transcript missing assistant text"
    );
}

/// Tool call: script 1 asks for `list`, script 2 closes with text. Asserts a
/// ToolResult message is produced and executed under FullAuto, plus both
/// assistant and tool result end up persisted.
#[tokio::test]
async fn host_tool_call() {
    let tmp = tempfile::tempdir().unwrap();
    let session = SessionManager::with_root(tmp.path().to_path_buf());
    let fake = FakeStream::new(vec![
        vec![
            StreamChunk::ToolCallDelta {
                id: "t1".into(),
                name: Some("list".into()),
                args_delta: "{}".into(),
            },
            StreamChunk::Done,
        ],
        vec![StreamChunk::TextDelta("done".into()), StreamChunk::Done],
    ]);
    let mut host = Host::new(
        test_cfg(false),
        session,
        Arc::new(full_auto_policy()),
        "You are a helpful assistant.".into(),
        gasket_core::built_in_tools(),
    )
    .with_stream_fn(Arc::new(fake));

    let history: Vec<AgentMessage> = Vec::new();
    let new_msgs = host
        .run_turn(user_msg("list the cwd"), &history, |_| {})
        .await
        .expect("tool-call turn should succeed");

    assert!(
        new_msgs
            .iter()
            .any(|m| matches!(m, AgentMessage::ToolResult { .. })),
        "expected a ToolResult message after the list call"
    );
    assert!(
        new_msgs
            .iter()
            .any(|m| matches!(m, AgentMessage::Assistant(_))),
        "expected the closing assistant message"
    );

    let sid = host.session().current_id().to_string();
    let raw = std::fs::read_to_string(tmp.path().join(&sid).join("messages.jsonl"))
        .expect("session transcript must be persisted");
    assert!(
        raw.contains("\"tool_result\""),
        "tool result missing from transcript"
    );
}

/// An errored stream with retry off must surface as `stop_reason::Error` on
/// the assistant message (the loop feeds it back as a message event — it does
/// NOT fail the run; that is deliberate core behavior). The errored turn is a
/// complete transcript record, so it IS persisted; nothing is lost.
#[tokio::test]
async fn host_error_surfaces_and_persists() {
    let tmp = tempfile::tempdir().unwrap();
    let session = SessionManager::with_root(tmp.path().to_path_buf());
    let fake = FakeStream::new(vec![vec![StreamChunk::Error("boom".into())]]);
    let mut host = Host::new(
        test_cfg(true),
        session,
        Arc::new(full_auto_policy()),
        "sys".into(),
        vec![],
    )
    .with_stream_fn(Arc::new(fake));

    let history: Vec<AgentMessage> = Vec::new();
    let mut surfaced = false;
    let new_msgs = host
        .run_turn(user_msg("hi"), &history, |ev| {
            // The loop surfaces pre-content errors as an AfterProviderResponse
            // whose message carries stop_reason::Error (AgentEvent::Error is
            // reserved for hosts that emit their own errors).
            if let AgentEvent::AfterProviderResponse { response, .. } = ev {
                surfaced = matches!(response.stop_reason, gasket_core::StopReason::Error(_));
            }
        })
        .await
        .expect("error stream must not fail the run — it surfaces as a message");

    assert!(
        new_msgs.iter().any(|m| matches!(
            m,
            AgentMessage::Assistant(a)
                if matches!(a.stop_reason, gasket_core::StopReason::Error(_))
        )),
        "expected assistant message with stop_reason::Error"
    );
    assert!(
        surfaced,
        "the errored response must reach the on_event callback"
    );

    // The errored turn is a complete transcript record and must persist.
    let sid = host.session().current_id();
    let path = tmp.path().join(sid).join("messages.jsonl");
    let raw = std::fs::read_to_string(&path).expect("errored turn must still be persisted");
    assert!(
        raw.contains("assistant"),
        "transcript must contain the errored turn"
    );
}

/// Multi-turn `run_turn` with token-driven compaction between turns (mirroring
/// the CLI loop). The working history is compacted via `ContextBudget` before
/// each turn once the provider-reported `input_tokens` trips the threshold, and
/// the result must never contain an orphan `ToolCall` (no matching `ToolResult`)
/// or orphan `ToolResult` (no matching `ToolCall`). A tiny context window makes
/// a normal `Usage` report force compaction on the next turn, deterministically.
#[tokio::test]
async fn compaction_keeps_history_valid() {
    let tmp = tempfile::tempdir().unwrap();
    let session = SessionManager::with_root(tmp.path().to_path_buf());

    // 4 tool-call turns = 8 scripts (2 per turn: ToolCallDelta+Done triggers
    // execution, then TextDelta+Usage+Done closes with EndTurn). input=90 trips
    // the 80% threshold of window=100 on the following turn. Distinct tool_call
    // ids per turn make an orphan split actually detectable.
    let mut scripts: Vec<Vec<StreamChunk>> = Vec::new();
    for id in ["t1", "t2", "t3", "t4"] {
        scripts.push(vec![
            StreamChunk::ToolCallDelta {
                id: id.into(),
                name: Some("list".into()),
                args_delta: "{}".into(),
            },
            StreamChunk::Done,
        ]);
        scripts.push(vec![
            StreamChunk::TextDelta("done".into()),
            StreamChunk::Usage {
                input: 90,
                output: 1,
            },
            StreamChunk::Done,
        ]);
    }
    let fake = FakeStream::new(scripts);
    let mut host = Host::new(
        test_cfg(false),
        session,
        Arc::new(full_auto_policy()),
        "You are a helpful assistant.".into(),
        gasket_core::built_in_tools(),
    )
    .with_stream_fn(Arc::new(fake));

    let mut budget = ContextBudget::from_env_with(&fake_env(&[
        ("GASKET_LLM_BASE_URL", "https://api.test/v1"),
        ("GASKET_LLM_KEY", "sk-test"),
        ("GASKET_LLM_MODEL", "m"),
        ("GASKET_CONTEXT_WINDOW", "100"),
        ("GASKET_COMPACT_THRESHOLD_PCT", "80"),
        ("GASKET_COMPACT_TARGET_PCT", "50"),
    ]));
    let mut history: Vec<AgentMessage> = Vec::new();

    for _ in 0..4 {
        // Mirror the CLI: compact before the turn when over threshold.
        if budget.needs_compaction() {
            history = budget.compact(&history);
        }
        let new = host
            .run_turn(user_msg("go"), &history, |ev| {
                if let AgentEvent::AfterProviderResponse { response, .. } = ev {
                    if let Some(u) = response.usage {
                        budget.record_input_tokens(u.input_tokens);
                    }
                }
            })
            .await
            .expect("turn should succeed");
        history.extend(new);
    }

    // Compaction must have fired at least once: turn 1 reports input=90 (>80%
    // of 100), so turns 2..4 enter the loop over threshold and compact.
    let compacted = history.iter().any(|m| {
        matches!(
            m,
            AgentMessage::User(u) if u.content.iter().any(|b| matches!(
                b,
                ContentBlock::Text { text } if text.contains("[compacted")
            ))
        )
    });
    assert!(
        compacted,
        "expected at least one [compacted ...] notice in history"
    );

    // No orphan tool_call / tool_result pairs across the whole history.
    assert_no_orphan_tool_pairs(&history);

    // History non-empty and the tail is not a dangling ToolResult.
    assert!(!history.is_empty(), "history must be non-empty");
    assert!(
        !matches!(history.last(), Some(AgentMessage::ToolResult(_))),
        "history must not end on a bare ToolResult"
    );
}

/// Every `ContentBlock::ToolCall` id must have a matching `ToolResult`
/// (`tool_call_id`), and vice versa. Scans the whole history; an orphan on
/// either side fails with the offending id.
fn assert_no_orphan_tool_pairs(history: &[AgentMessage]) {
    let mut call_ids: Vec<String> = Vec::new();
    let mut result_ids: Vec<String> = Vec::new();
    for m in history {
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
    for id in &call_ids {
        assert!(
            result_ids.contains(id),
            "orphan tool_call {id} has no matching result"
        );
    }
    for id in &result_ids {
        assert!(
            call_ids.contains(id),
            "orphan tool_result {id} has no matching call"
        );
    }
}
