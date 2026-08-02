//! Host-side `SubagentSpawner`: fans out parallel sub-agent loops.
//!
//! Each sub-agent runs its own `run_agent_loop` with the same built-in tool
//! set, stream_fn, and policy hooks as the parent (minus `spawn_subagents`
//! itself — sub-agent contexts carry no spawner, so nesting is disabled).
//! Events are mapped from `AgentEvent` to `SubagentEvent` and emitted through
//! the constructor-injected forwarder (`with_ws_emit`). Results are collected
//! after all sub-agents finish.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use gasket_core::{
    run_agent_loop, AgentContext, AgentEvent, AgentLoopConfig, AgentMessage, ContentBlock,
    ContentDelta, SubagentEvent, SubagentResult, SubagentSpawn, SubagentSpawner,
    ToolDefinition, UserMessage,
};

/// Max turns for a sub-agent (lower than the parent's default 50).
const SUBAGENT_MAX_TURNS: usize = 10;

/// A spawner built from the host's config. Each `spawn` call creates fresh
/// per-task contexts and runs them concurrently.
pub struct HostSubagentSpawner {
    system_prompt: String,
    tools: Vec<ToolDefinition>,
    hooks: Arc<dyn gasket_core::HookChain>,
    signal: Arc<std::sync::atomic::AtomicBool>,
    cwd: std::path::PathBuf,
    /// Process environment captured once at construction; cloned (cheap Arc)
    /// into each sub-agent context instead of re-querying the OS per task.
    env: Arc<HashMap<String, String>>,
    /// Loop-config template derived from the parent's provider/tunables
    /// (`build_loop_config`). Each sub-agent clones it and overrides
    /// `max_turns` (capped) plus signal/hooks. Sub-agents therefore inherit
    /// the parent's configured thinking level, per-turn tool-call ceiling,
    /// and retry policy — no hardcoded drift.
    loop_config: AgentLoopConfig,
    /// Optional event forwarder set by the gateway. All subagent events are
    /// delivered through this callback (the trait has no per-call emit).
    ws_emit: Option<Arc<dyn Fn(SubagentEvent) + Send + Sync>>,
}

impl HostSubagentSpawner {
    pub fn new(
        system_prompt: String,
        tools: Vec<ToolDefinition>,
        hooks: Arc<dyn gasket_core::HookChain>,
        signal: Arc<std::sync::atomic::AtomicBool>,
        cwd: std::path::PathBuf,
        loop_config: AgentLoopConfig,
    ) -> Self {
        Self {
            system_prompt,
            tools,
            hooks,
            signal,
            cwd,
            env: Arc::new(std::env::vars().collect()),
            loop_config,
            ws_emit: None,
        }
    }

    /// Set an event forwarder (gateway). All subagent events are delivered to
    /// this callback.
    pub fn with_ws_emit(
        mut self,
        ws_emit: Arc<dyn Fn(SubagentEvent) + Send + Sync>,
    ) -> Self {
        self.ws_emit = Some(ws_emit);
        self
    }
}

impl SubagentSpawner for HostSubagentSpawner {
    fn spawn(
        &self,
        tasks: Vec<SubagentSpawn>,
    ) -> Pin<Box<dyn Future<Output = Vec<SubagentResult>> + Send>> {
        let count = tasks.len();
        let emit: Arc<dyn Fn(SubagentEvent) + Send + Sync> = match &self.ws_emit {
            Some(ws) => Arc::clone(ws),
            None => Arc::new(|_| {}),
        };
        let spawner = Arc::new(HostSubagentSpawner {
            system_prompt: self.system_prompt.clone(),
            tools: self.tools.clone(),
            hooks: Arc::clone(&self.hooks),
            signal: Arc::clone(&self.signal),
            cwd: self.cwd.clone(),
            env: Arc::clone(&self.env),
            loop_config: self.loop_config.clone(),
            ws_emit: self.ws_emit.clone(),
        });

        Box::pin(async move {
            emit(SubagentEvent::AllStarted { count });

            // Sub-agent tasks live behind a drop guard: if this future is
            // dropped mid-flight (turn cancelled, connection closed), every
            // still-running task is aborted instead of executing tools and
            // emitting events into a dead session. abort() on a completed
            // task is a no-op, so the guard is harmless on the happy path.
            // AbortHandles stay tracked for the WHOLE function (including
            // the collection loop below), so a drop at any await point stops
            // everything — including the handle currently being awaited.
            let mut guard = AbortOnDrop::default();
            let mut pending: Vec<SubagentTask> = Vec::with_capacity(count);

            for (i, task) in tasks.into_iter().enumerate() {
                let id = uuid::Uuid::new_v4().to_string();
                let index = i + 1;
                let task_clone = task.task.clone();

                let (event_tx, mut event_rx) =
                    tokio::sync::mpsc::unbounded_channel::<AgentEvent>();

                // Forwarder: AgentEvent → SubagentEvent → emit.
                let emit_fwd = Arc::clone(&emit);
                let fwd_id = id.clone();
                let fwd_handle = tokio::spawn(async move {
                    while let Some(ev) = event_rx.recv().await {
                        if let Some(sub_ev) = map_agent_event(&fwd_id, &ev) {
                            emit_fwd(sub_ev);
                        }
                    }
                });

                let sub_context = AgentContext {
                    system_prompt: spawner.system_prompt.clone(),
                    messages: vec![],
                    tools: spawner.tools.clone(),
                    cwd: spawner.cwd.clone(),
                    env: (*spawner.env).clone(),
                    session_id: format!("subagent-{id}"),
                    spawner: None,
                };

                // Clone the parent-derived template, then pin this spawner's
                // signal/hooks and cap max_turns below the parent's.
                let mut sub_config = spawner.loop_config.clone();
                sub_config.max_turns = SUBAGENT_MAX_TURNS.min(spawner.loop_config.max_turns);
                sub_config.signal = Some(Arc::clone(&spawner.signal));
                sub_config.hooks = Some(Arc::clone(&spawner.hooks));

                let user_msg = AgentMessage::User(UserMessage {
                    content: vec![ContentBlock::text(task.task.clone())],
                    timestamp: gasket_core::now(),
                });

                emit(SubagentEvent::Started {
                    id: id.clone(),
                    task: task_clone.clone(),
                    index,
                });

                let run_id = id.clone();
                let run_task = task_clone.clone();
                let run_index = index;
                let emit = Arc::clone(&emit);

                let handle = tokio::spawn(async move {
                    let result = run_agent_loop(
                        vec![user_msg],
                        sub_context,
                        sub_config,
                        move |ev| {
                            let _ = event_tx.send(ev);
                        },
                    )
                    .await;

                    // Wait for forwarder to drain.
                    let _ = fwd_handle.await;

                    match result {
                        Ok(msgs) => {
                            // A run can end "successfully" with a failed
                            // stream: provider error mid-response, or abort via
                            // cancel. StopReason::Error/Aborted is surfaced as
                            // a sub-agent error, not a completion — otherwise
                            // the main agent and the frontend see a green
                            // checkmark on work that never finished.
                            let failed = msgs.iter().rev().find_map(|m| match m {
                                AgentMessage::Assistant(a) => match &a.stop_reason {
                                    gasket_core::StopReason::Error(e) => Some(format!(
                                        "sub-agent stream failed: {e}"
                                    )),
                                    gasket_core::StopReason::Aborted => {
                                        Some("sub-agent cancelled".into())
                                    }
                                    _ => None,
                                },
                                _ => None,
                            });
                            if let Some(err_msg) = failed {
                                emit(SubagentEvent::Error {
                                    id: run_id.clone(),
                                    index: run_index,
                                    error: err_msg.clone(),
                                });
                                SubagentResult {
                                    id: run_id,
                                    task: run_task,
                                    index: run_index,
                                    summary: String::new(),
                                    tool_count: 0,
                                    error: Some(err_msg),
                                }
                            } else {
                                let (summary, tool_count) =
                                    extract_summary_and_tools(&msgs);
                                emit(SubagentEvent::Completed {
                                    id: run_id.clone(),
                                    index: run_index,
                                    summary: summary.clone(),
                                    tool_count,
                                });
                                SubagentResult {
                                    id: run_id,
                                    task: run_task,
                                    index: run_index,
                                    summary,
                                    tool_count,
                                    error: None,
                                }
                            }
                        }
                        Err(e) => {
                            let err_msg = e.to_string();
                            emit(SubagentEvent::Error {
                                id: run_id.clone(),
                                index: run_index,
                                error: err_msg.clone(),
                            });
                            SubagentResult {
                                id: run_id,
                                task: run_task,
                                index: run_index,
                                summary: String::new(),
                                tool_count: 0,
                                error: Some(err_msg),
                            }
                        }
                    }
                });
                guard.push(&handle);
                pending.push(SubagentTask {
                    handle,
                    id,
                    index,
                    task: task_clone,
                });
            }

            let mut results = Vec::with_capacity(pending.len());
            for st in pending {
                match st.handle.await {
                    Ok(r) => results.push(r),
                    Err(e) => {
                        // Panicked sub-agent task: surface it exactly like a
                        // run error (event + result) so the frontend's
                        // completion count stays consistent.
                        let err_msg = format!("subagent task panicked: {e}");
                        emit(SubagentEvent::Error {
                            id: st.id.clone(),
                            index: st.index,
                            error: err_msg.clone(),
                        });
                        results.push(SubagentResult {
                            id: st.id,
                            task: st.task,
                            index: st.index,
                            summary: String::new(),
                            tool_count: 0,
                            error: Some(err_msg),
                        });
                    }
                }
            }

            // All sub-agents finished: signal the main agent is synthesizing
            // their results. Must come AFTER all handles complete, not before.
            emit(SubagentEvent::Synthesizing);
            results
        })
    }
}

/// Owns sub-agent abort handles; aborts every still-running task on drop.
/// The spawn future holds one of these, so dropping the future mid-flight
/// (turn cancelled, connection closed) stops sub-agents instead of leaving
/// them detached. Handles are never removed — a drop at ANY await point
/// (including the collection loop) aborts everything, even the task
/// currently being awaited. abort() on a completed task is a no-op.
#[derive(Default)]
struct AbortOnDrop(Vec<tokio::task::AbortHandle>);

impl AbortOnDrop {
    fn push(&mut self, handle: &tokio::task::JoinHandle<SubagentResult>) {
        self.0.push(handle.abort_handle());
    }
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        for abort in &self.0 {
            abort.abort();
        }
    }
}

/// A spawned sub-agent task awaiting collection: its JoinHandle plus the
/// metadata needed to report a panic (id / index / task description).
struct SubagentTask {
    handle: tokio::task::JoinHandle<SubagentResult>,
    id: String,
    index: usize,
    task: String,
}

/// Map a sub-agent's `AgentEvent` to a `SubagentEvent` tagged with `id`.
fn map_agent_event(id: &str, ev: &AgentEvent) -> Option<SubagentEvent> {
    match ev {
        AgentEvent::MessageUpdate { delta } => match delta {
            ContentDelta::TextDelta(t) => Some(SubagentEvent::Content {
                id: id.into(),
                content: t.clone(),
            }),
            ContentDelta::ThinkingDelta(t) => Some(SubagentEvent::Thinking {
                id: id.into(),
                content: t.clone(),
            }),
            ContentDelta::ToolCallDelta { .. } => None,
        },
        AgentEvent::ToolExecutionStart {
            tool_name, args, ..
        } => Some(SubagentEvent::ToolStart {
            id: id.into(),
            name: tool_name.clone(),
            arguments: Some(serde_json::to_string(args).unwrap_or_default()),
        }),
        AgentEvent::ToolExecutionEnd { result, .. } => {
            let output = result
                .content
                .iter()
                .find_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            // Note: the server's tool_call_id is deliberately NOT forwarded —
            // the frontend generates its own tool ids and matches by name
            // (sub-agents execute tools serially, so the name is unambiguous).
            Some(SubagentEvent::ToolEnd {
                id: id.into(),
                name: result.tool_name.clone(),
                output: Some(output),
            })
        }
        // Provider usage from a sub-agent's LLM calls: internal accounting
        // only — the gateway folds it into the session token counters.
        AgentEvent::AfterProviderResponse { response, .. } => {
            response.usage.as_ref().map(|u| SubagentEvent::Usage {
                input_tokens: u.input_tokens,
                output_tokens: u.output_tokens,
            })
        }
        _ => None,
    }
}

/// Extract the sub-agent's summary (≤200 chars) + tool-result count.
///
/// The LAST assistant message is the sub-agent's final output — we never
/// search backward past it. When `max_turns` runs out mid-tool-call, the
/// last assistant ended on `ToolUse` (it wanted to keep going but the loop
/// exhausted its turn budget). In that case the summary is marked incomplete
/// so the main agent doesn't mistake an intermediate turn's text for the
/// sub-agent's conclusion.
fn extract_summary_and_tools(msgs: &[AgentMessage]) -> (String, usize) {
    let tool_count = msgs
        .iter()
        .filter(|m| matches!(m, AgentMessage::ToolResult(_)))
        .count();

    let summary = match msgs.iter().rev().find_map(|m| match m {
        AgentMessage::Assistant(a) => Some(a),
        _ => None,
    }) {
        Some(a) => {
            let text: String = a
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            if matches!(a.stop_reason, gasket_core::StopReason::ToolUse) {
                // The loop exhausted max_turns mid-tool-call.
                if text.is_empty() {
                    "(reached turn limit without a final answer)".into()
                } else {
                    let truncated: String = text.chars().take(200).collect();
                    format!("{truncated} (note: reached turn limit, result may be incomplete)")
                }
            } else {
                text.chars().take(200).collect()
            }
        }
        None => String::new(),
    };

    (summary, tool_count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::sync::atomic::AtomicBool;

    use gasket_core::{
        AgentLoopConfig, ModelSpec, ProviderApi, RetryPolicy, StreamChunk, StreamFn,
        ThinkingLevel, ToolDefinition,
    };

    use crate::hooks::HookStack;
    use crate::permission::{Approver, Mode, PermissionPolicy};

    /// A mock StreamFn that replays a fixed chunk sequence on every call.
    struct MockStream(Vec<StreamChunk>);

    impl StreamFn for MockStream {
        fn stream(
            &self,
            _model: &ModelSpec,
            _messages: &[AgentMessage],
            _system_prompt: &str,
            _tools: &[ToolDefinition],
            _signal: Option<Arc<AtomicBool>>,
        ) -> Pin<Box<dyn futures_util::Stream<Item = StreamChunk> + Send>> {
            Box::pin(futures_util::stream::iter(self.0.clone()))
        }
    }

    fn test_spawner(
        stream: Arc<dyn StreamFn>,
        hooks: Arc<dyn gasket_core::HookChain>,
        ev_tx: tokio::sync::mpsc::UnboundedSender<SubagentEvent>,
        signal: Arc<AtomicBool>,
    ) -> HostSubagentSpawner {
        HostSubagentSpawner::new(
            "sys".into(),
            gasket_core::built_in_tools(),
            hooks,
            signal,
            std::env::current_dir().unwrap(),
            AgentLoopConfig {
                model: ModelSpec {
                    id: "test".into(),
                    api: ProviderApi::OpenAiCompat,
                    max_tokens: 1024,
                    supports_thinking: false,
                },
                thinking_level: ThinkingLevel::Off,
                max_turns: 2,
                max_tool_calls_per_turn: 20,
                signal: None,
                stream_fn: stream,
                hooks: None,
                retry: RetryPolicy::off(),
            },
        )
        .with_ws_emit(Arc::new(move |ev| {
            let _ = ev_tx.send(ev);
        }))
    }

    /// Security regression: a sub-agent calling `bash` must go through the
    /// SAME `PermissionPolicy` as the parent — the approver is consulted and
    /// the call is blocked before any command runs.
    #[tokio::test]
    async fn subagent_tool_calls_go_through_shared_policy() {
        let calls = Arc::new(Mutex::new(Vec::<String>::new()));
        let calls_c = Arc::clone(&calls);
        let approver: Approver = Arc::new(move |name: &str, _args: &serde_json::Value| {
            let calls = Arc::clone(&calls_c);
            Box::pin(async move {
                calls.lock().push(name.to_string());
                false // deny: AutoEdit consults the approver for High-risk tools
            })
        });
        let policy = Arc::new(PermissionPolicy::new(Mode::AutoEdit, approver));
        let hooks: Arc<dyn gasket_core::HookChain> = Arc::new(HookStack::new(vec![policy]));

        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel::<SubagentEvent>();
        let spawner = test_spawner(
            Arc::new(MockStream(vec![
                StreamChunk::ToolCallDelta {
                    id: "t1".into(),
                    name: Some("bash".into()),
                    args_delta: "{\"command\":\"echo hi\"}".into(),
                },
                StreamChunk::Done,
            ])),
            hooks,
            ev_tx,
            Arc::new(AtomicBool::new(false)),
        );

        let results = spawner
            .spawn(vec![SubagentSpawn {
                task: "run a command".into(),
            }])
            .await;

        assert_eq!(results.len(), 1);
        assert!(
            results[0].error.is_none(),
            "sub-agent should complete: {:?}",
            results[0].error
        );

        // The sub-agent's bash call hit the shared policy: the approver was
        // consulted with the tool name...
        let names = calls.lock().clone();
        assert!(
            names.iter().any(|n| n == "bash"),
            "approver must be consulted for sub-agent bash: {names:?}"
        );
        // ...and the block surfaced on the wire as a tool result.
        let mut tool_ends = Vec::new();
        while let Ok(ev) = ev_rx.try_recv() {
            if let SubagentEvent::ToolEnd { name, output, .. } = ev {
                tool_ends.push((name, output.unwrap_or_default()));
            }
        }
        assert!(
            tool_ends.iter().any(|(n, o)| n == "bash" && o.contains("denied")),
            "blocked bash must be visible in events: {tool_ends:?}"
        );
    }

    /// Protocol ordering: `Synthesizing` must arrive after every sub-agent
    /// terminal event; `AllStarted` leads; usage is forwarded internally.
    #[tokio::test]
    async fn synthesizing_arrives_last() {
        let policy = Arc::new(PermissionPolicy::new(
            Mode::FullAuto,
            Arc::new(|_, _| Box::pin(async { true })),
        ));
        let hooks: Arc<dyn gasket_core::HookChain> = Arc::new(HookStack::new(vec![policy]));

        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel::<SubagentEvent>();
        let spawner = test_spawner(
            Arc::new(MockStream(vec![
                StreamChunk::TextDelta("done".into()),
                StreamChunk::Usage {
                    input: 7,
                    output: 3,
                },
                StreamChunk::Done,
            ])),
            hooks,
            ev_tx,
            Arc::new(AtomicBool::new(false)),
        );

        let results = spawner
            .spawn(vec![SubagentSpawn {
                task: "say done".into(),
            }])
            .await;
        assert_eq!(results.len(), 1);
        assert!(results[0].error.is_none());

        let events: Vec<SubagentEvent> = std::iter::from_fn(|| ev_rx.try_recv().ok()).collect();
        assert!(matches!(events.first(), Some(SubagentEvent::AllStarted { .. })));
        assert!(matches!(events.last(), Some(SubagentEvent::Synthesizing)));
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, SubagentEvent::Completed { .. }))
                .count(),
            1,
            "events: {events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(
                e,
                SubagentEvent::Usage {
                    input_tokens: 7,
                    output_tokens: 3
                }
            )),
            "usage must be emitted: {events:?}"
        );
    }

    /// Summary honesty: when a sub-agent exhausts max_turns mid-tool-call,
    /// the summary must NOT report an earlier turn's text as if it were the
    /// final answer — it must indicate the task is incomplete.
    #[tokio::test]
    async fn max_turns_exhaustion_marks_summary_incomplete() {
        let policy = Arc::new(PermissionPolicy::new(
            Mode::FullAuto,
            Arc::new(|_, _| Box::pin(async { true })),
        ));
        let hooks: Arc<dyn gasket_core::HookChain> = Arc::new(HookStack::new(vec![policy]));
        let (ev_tx, _ev_rx) = tokio::sync::mpsc::unbounded_channel::<SubagentEvent>();
        // Mock replays the same chunks every turn: text + tool call → the
        // loop keeps going until max_turns (2) runs out.
        let spawner = test_spawner(
            Arc::new(MockStream(vec![
                StreamChunk::TextDelta("Searching the codebase".into()),
                StreamChunk::ToolCallDelta {
                    id: "tc1".into(),
                    name: Some("list".into()),
                    args_delta: "{}".into(),
                },
                StreamChunk::Done,
            ])),
            hooks,
            ev_tx,
            Arc::new(AtomicBool::new(false)),
        );

        let results = spawner
            .spawn(vec![SubagentSpawn {
                task: "search".into(),
            }])
            .await;
        assert_eq!(results.len(), 1);
        // It's Completed (not Error) — the sub-agent did work, just didn't finish.
        assert!(results[0].error.is_none(), "max_turns is not an error");
        // But the summary must warn about incompleteness, not report the
        // stale "Searching the codebase" text as the final answer.
        let summary = &results[0].summary;
        assert!(
            summary.contains("turn limit"),
            "summary must indicate incomplete: {summary}"
        );
    }

    /// Error reporting: a sub-agent whose provider stream dies mid-response
    /// must surface as an Error, not a hollow Completed.
    #[tokio::test]
    async fn mid_stream_failure_reports_error_not_completed() {
        let policy = Arc::new(PermissionPolicy::new(
            Mode::FullAuto,
            Arc::new(|_, _| Box::pin(async { true })),
        ));
        let hooks: Arc<dyn gasket_core::HookChain> = Arc::new(HookStack::new(vec![policy]));
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel::<SubagentEvent>();
        let spawner = test_spawner(
            Arc::new(MockStream(vec![
                StreamChunk::TextDelta("partial".into()),
                StreamChunk::Error("provider boom".into()),
            ])),
            hooks,
            ev_tx,
            Arc::new(AtomicBool::new(false)),
        );

        let results = spawner
            .spawn(vec![SubagentSpawn {
                task: "t".into(),
            }])
            .await;
        assert_eq!(results.len(), 1);
        assert!(
            results[0].error.is_some(),
            "mid-stream failure must be an error result: {:?}",
            results[0].error
        );

        let events: Vec<SubagentEvent> = std::iter::from_fn(|| ev_rx.try_recv().ok()).collect();
        assert!(
            events.iter().any(|e| matches!(e, SubagentEvent::Error { .. })),
            "a failed stream must emit Error: {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, SubagentEvent::Completed { .. })),
            "a failed stream must NOT emit Completed: {events:?}"
        );
    }

    /// Cancellation: a pre-set abort signal must surface as a sub-agent
    /// error ("cancelled"), not a hollow Completed.
    #[tokio::test]
    async fn aborted_subagent_reports_cancelled() {
        let policy = Arc::new(PermissionPolicy::new(
            Mode::FullAuto,
            Arc::new(|_, _| Box::pin(async { true })),
        ));
        let hooks: Arc<dyn gasket_core::HookChain> = Arc::new(HookStack::new(vec![policy]));
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel::<SubagentEvent>();
        let spawner = test_spawner(
            Arc::new(MockStream(vec![
                StreamChunk::TextDelta("hi".into()),
                StreamChunk::Done,
            ])),
            hooks,
            ev_tx,
            Arc::new(AtomicBool::new(true)), // pre-set: loop aborts before any provider call
        );

        let results = spawner
            .spawn(vec![SubagentSpawn {
                task: "t".into(),
            }])
            .await;
        let err = results[0].error.clone();
        assert!(
            err.is_some(),
            "pre-set signal must produce an error result: {:?}",
            err
        );
        assert!(
            err.as_deref().unwrap_or_default().contains("cancelled"),
            "abort must read as cancelled: {err:?}"
        );

        let events: Vec<SubagentEvent> = std::iter::from_fn(|| ev_rx.try_recv().ok()).collect();
        assert!(events.iter().any(|e| matches!(e, SubagentEvent::Error { .. })));
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, SubagentEvent::Completed { .. }))
        );
    }

    /// Lifecycle: dropping the spawn future mid-flight must abort every
    /// sub-agent task (no detached tasks executing after the turn is gone).
    #[tokio::test]
    async fn dropping_spawn_future_aborts_subagents() {
        let policy = Arc::new(PermissionPolicy::new(
            Mode::FullAuto,
            Arc::new(|_, _| Box::pin(async { true })),
        ));
        let hooks: Arc<dyn gasket_core::HookChain> = Arc::new(HookStack::new(vec![policy]));
        // The stream parks on a oneshot; if the sub-agent task survives the
        // spawn-future drop, the sender below stays alive and this test hangs
        // (it fails via the assertion instead).
        let (block_tx, block_rx) = tokio::sync::oneshot::channel::<()>();
        let (ev_tx, _ev_rx) = tokio::sync::mpsc::unbounded_channel::<SubagentEvent>();
        let stream: Arc<dyn StreamFn> =
            Arc::new(BlockingStream(parking_lot::Mutex::new(Some(block_rx))));
        let spawner = test_spawner(stream, hooks, ev_tx, Arc::new(AtomicBool::new(false)));

        // `spawn()` already returns `Pin<Box<dyn Future>>` — no tokio::pin!
        // (it would shadow the variable, making `drop(fut)` drop only the
        // pin wrapper and leaving the future — and the abort guard — alive).
        let mut fut = spawner.spawn(vec![SubagentSpawn {
            task: "hang".into(),
        }]);
        // One poll: the async block spawns the sub-agent task synchronously,
        // then parks on the first handle.await.
        let waker = futures_util::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        assert!(fut.as_mut().poll(&mut cx).is_pending());

        // Dropping the spawn future must abort the sub-agent task, which
        // drops its pending stream future — and with it the oneshot receiver.
        // (The local `spawner` also holds the stream Arc; drop it so the
        // receiver's fate depends only on the aborted task.)
        drop(fut);
        drop(spawner);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            block_tx.send(()).is_err(),
            "sub-agent task must be aborted when the spawn future is dropped"
        );
    }

    /// A StreamFn that parks on a oneshot receiver (used by the abort test).
    struct BlockingStream(parking_lot::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>);

    impl StreamFn for BlockingStream {
        fn stream(
            &self,
            _model: &ModelSpec,
            _messages: &[AgentMessage],
            _system_prompt: &str,
            _tools: &[ToolDefinition],
            _signal: Option<Arc<AtomicBool>>,
        ) -> Pin<Box<dyn futures_util::Stream<Item = StreamChunk> + Send>> {
            let rx = self.0.lock().take();
            Box::pin(futures_util::stream::once(async move {
                if let Some(rx) = rx {
                    let _ = rx.await; // parks until the task is aborted
                }
                StreamChunk::Done
            }))
        }
    }
}
