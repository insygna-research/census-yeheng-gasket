//! Offline end-to-end integration tests driving the full host pipeline
//! (Host, ConfigLoader, SessionManager, PermissionPolicy, EventPrinter,
//! run_agent_loop) with a deterministic FakeStream: no network, no LLM keys,
//! CI-mandatory.
mod common;

use std::sync::Arc;

use common::FakeStream;
use gasket_core::{AgentEvent, AgentMessage, ContentBlock, StreamChunk, UserMessage};
use gasket_host::{
    ConfigLoader, EventPrinter, Host, HostConfig, Mode, PermissionPolicy, SessionManager,
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
