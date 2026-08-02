//! Host-side `SubagentSpawner`: fans out parallel sub-agent loops.
//!
//! Each sub-agent runs its own `run_agent_loop` with the same tools, stream_fn,
//! and hooks as the parent. Events are mapped from `AgentEvent` to
//! `SubagentEvent` and emitted through the callback. Results are collected
//! after all sub-agents finish.

use std::sync::Arc;

use gasket_core::{
    run_agent_loop, AgentContext, AgentEvent, AgentMessage, ContentBlock, ContentDelta,
    ModelSpec, StreamFn, SubagentEvent, SubagentResult, SubagentSpawn, SubagentSpawner,
    ToolDefinition, UserMessage,
};

/// Max turns for a sub-agent (lower than the parent's default 50).
const SUBAGENT_MAX_TURNS: usize = 10;

/// A spawner built from the host's config. Each `spawn` call creates fresh
/// per-task contexts and runs them concurrently.
pub struct HostSubagentSpawner {
    system_prompt: String,
    tools: Vec<ToolDefinition>,
    stream_fn: Arc<dyn StreamFn>,
    hooks: Arc<dyn gasket_core::HookChain>,
    signal: Arc<std::sync::atomic::AtomicBool>,
    cwd: std::path::PathBuf,
    max_turns: usize,
    model: ModelSpec,
    /// Optional event forwarder set by the gateway. When set, subagent events
    /// are forwarded here in addition to the per-spawn emit callback.
    ws_emit: Option<Arc<dyn Fn(SubagentEvent) + Send + Sync>>,
}

impl HostSubagentSpawner {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        system_prompt: String,
        tools: Vec<ToolDefinition>,
        stream_fn: Arc<dyn StreamFn>,
        hooks: Arc<dyn gasket_core::HookChain>,
        signal: Arc<std::sync::atomic::AtomicBool>,
        cwd: std::path::PathBuf,
        model: ModelSpec,
    ) -> Self {
        Self {
            system_prompt,
            tools,
            stream_fn,
            hooks,
            signal,
            cwd,
            max_turns: SUBAGENT_MAX_TURNS,
            model,
            ws_emit: None,
        }
    }

    /// Set a WS event forwarder (gateway). When set, all subagent events are
    /// forwarded to this callback in addition to the per-spawn emit.
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
        emit: Arc<dyn Fn(SubagentEvent) + Send + Sync>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<SubagentResult>> + Send>> {
        let count = tasks.len();
        // If the gateway set a ws_emit, merge it into the emit callback so
        // events flow to both the per-spawn emit and the WS forwarder.
        let emit: Arc<dyn Fn(SubagentEvent) + Send + Sync> = match &self.ws_emit {
            Some(ws) => {
                let emit = Arc::clone(&emit);
                let ws = Arc::clone(ws);
                Arc::new(move |ev| {
                    emit(ev.clone());
                    ws(ev);
                })
            }
            None => emit,
        };
        let spawner = Arc::new(HostSubagentSpawner {
            system_prompt: self.system_prompt.clone(),
            tools: self.tools.clone(),
            stream_fn: Arc::clone(&self.stream_fn),
            hooks: Arc::clone(&self.hooks),
            signal: Arc::clone(&self.signal),
            cwd: self.cwd.clone(),
            max_turns: self.max_turns,
            model: self.model.clone(),
            ws_emit: self.ws_emit.clone(),
        });

        Box::pin(async move {
            emit(SubagentEvent::AllStarted { count });

            let mut handles = Vec::with_capacity(count);

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
                    env: std::env::vars().collect(),
                    session_id: format!("subagent-{id}"),
                    spawner: None,
                };

                let sub_config = gasket_core::AgentLoopConfig {
                    model: spawner.model.clone(),
                    thinking_level: gasket_core::ThinkingLevel::default(),
                    max_turns: spawner.max_turns,
                    max_tool_calls_per_turn: 20,
                    signal: Some(Arc::clone(&spawner.signal)),
                    stream_fn: Arc::clone(&spawner.stream_fn),
                    hooks: Some(Arc::clone(&spawner.hooks)),
                    retry: gasket_core::RetryPolicy::default(),
                };

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

                handles.push(tokio::spawn(async move {
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
                            let (summary, tool_count) = extract_summary_and_tools(&msgs);
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
                }));
            }


            let mut results = Vec::with_capacity(handles.len());
            for handle in handles {
                match handle.await {
                    Ok(r) => results.push(r),
                    Err(e) => {
                        results.push(SubagentResult {
                            id: "panic".into(),
                            task: String::new(),
                            index: 0,
                            summary: String::new(),
                            tool_count: 0,
                            error: Some(format!("subagent task panicked: {e}")),
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
        AgentEvent::ToolExecutionEnd {
            tool_call_id,
            result,
            ..
        } => {
            let output = result
                .content
                .iter()
                .find_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            Some(SubagentEvent::ToolEnd {
                id: id.into(),
                tool_id: Some(tool_call_id.clone()),
                name: result.tool_name.clone(),
                output: Some(output),
            })
        }
        _ => None,
    }
}

/// Extract the last assistant text as summary + count tool results.
fn extract_summary_and_tools(msgs: &[AgentMessage]) -> (String, usize) {
    let summary = msgs
        .iter()
        .rev()
        .find_map(|m| match m {
            AgentMessage::Assistant(a) => {
                let text: String = a
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                if text.is_empty() {
                    None
                } else {
                    Some(text.chars().take(200).collect())
                }
            }
            _ => None,
        })
        .unwrap_or_default();

    let tool_count = msgs
        .iter()
        .filter(|m| matches!(m, AgentMessage::ToolResult(_)))
        .count();

    (summary, tool_count)
}
