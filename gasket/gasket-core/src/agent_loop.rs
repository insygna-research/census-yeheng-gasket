//! The agent loop — single outer loop: LLM call → tool calls → repeat.

use std::pin::Pin;
use std::sync::atomic::Ordering;

use futures_util::StreamExt;

use crate::error::AgentError;
use crate::types::context::{AgentContext, AgentLoopConfig, StreamChunk};
use crate::types::event::{AgentEvent, ContentDelta};
use crate::types::message::{
    AgentMessage, AssistantMessage, ContentBlock, StopReason, ToolResultMessage,
};
use crate::types::session_event::SessionEvent;
use crate::types::tool::{RiskLevel, ToolCallCtx, ToolCallVerdict, ToolContext};

/// Run the agent loop to completion.
///
/// `emit` is called for every [`AgentEvent`] as it happens. Returns the full
/// list of messages produced (assistant turns + tool results).
///
/// Host usage:
/// ```ignore
/// run_agent_loop(prompts, context, config, |ev| match ev { ... }).await?;
/// ```
pub async fn run_agent_loop<E>(
    initial_prompts: Vec<AgentMessage>,
    mut context: AgentContext,
    config: AgentLoopConfig,
    mut emit: E,
) -> Result<Vec<AgentMessage>, AgentError>
where
    E: FnMut(AgentEvent),
{
    let mut new_messages: Vec<AgentMessage> = Vec::new();

    // Seed context with the initial prompts. These are inputs, not assistant
    // output: no MessageStart/MessageEnd is emitted for them (an empty
    // AssistantMessage per user prompt would be a lie both frontends would
    // have to filter out).
    for msg in &initial_prompts {
        context.messages.push(msg.clone());
        new_messages.push(msg.clone());
    }
    emit(AgentEvent::AgentStart);
    tracing::info!(model = %config.model.id, session = %context.session_id, "agent loop start");

    let mut guard = crate::guard::RepeatGuard::new();

    // Single outer loop.
    for turn in 0..config.max_turns {
        emit(AgentEvent::TurnStart);
        tracing::info!("agent turn {} start", turn);

        // 1. Call the LLM. (An abort signal is handled inside
        //    `stream_assistant_response`, before any provider request is made.)
        let assistant = stream_assistant_response(&context, &config, &mut emit).await?;
        let stop_reason = assistant.stop_reason.clone();
        // Crash-safety invariant: the assembled assistant message (with this
        // step's usage) hits the log BEFORE any tool in it executes, so a
        // crash mid-tool leaves an honest "assistant asked, tool never
        // answered" tail instead of a phantom.
        persist_event(
            &config,
            &SessionEvent::Assistant {
                message: AgentMessage::Assistant(assistant.clone()),
                usage: assistant.usage,
            },
        )?;
        context
            .messages
            .push(AgentMessage::Assistant(assistant.clone()));
        new_messages.push(AgentMessage::Assistant(assistant.clone()));

        // 2. Check termination.
        match stop_reason {
            StopReason::EndTurn | StopReason::Error(_) | StopReason::Aborted => {
                emit(AgentEvent::TurnEnd {
                    message: assistant,
                    tool_results: vec![],
                });
                break;
            }
            StopReason::MaxTokens => {
                // Output was truncated: fail every tool call in this turn.
                tracing::warn!("assistant output truncated (max_tokens); discarding tool calls");
                let error_results = fail_all_tool_calls(&assistant);
                for r in &error_results {
                    persist_event(
                        &config,
                        &SessionEvent::ToolResult(AgentMessage::ToolResult(r.clone())),
                    )?;
                    context.messages.push(AgentMessage::ToolResult(r.clone()));
                    new_messages.push(AgentMessage::ToolResult(r.clone()));
                }
                emit(AgentEvent::TurnEnd {
                    message: assistant,
                    tool_results: error_results,
                });
                continue;
            }
            StopReason::ToolUse => {} // fall through to execution
        }

        // 3. Execute tool calls (serial in V0.1).
        let tool_results =
            execute_tool_calls(&context, &assistant, &config, &mut guard, &mut emit).await?;
        for r in &tool_results {
            context.messages.push(AgentMessage::ToolResult(r.clone()));
            new_messages.push(AgentMessage::ToolResult(r.clone()));
        }

        emit(AgentEvent::TurnEnd {
            message: assistant,
            tool_results,
        });
    }

    tracing::info!("agent loop end");
    emit(AgentEvent::AgentEnd);
    Ok(new_messages)
}

/// Convenience: run the loop with a no-op emitter (for hosts that only want
/// the final message list).
pub async fn agent_loop(
    initial_prompts: Vec<AgentMessage>,
    context: AgentContext,
    config: AgentLoopConfig,
) -> Result<Vec<AgentMessage>, AgentError> {
    run_agent_loop(initial_prompts, context, config, |_| {}).await
}

fn is_aborted(config: &AgentLoopConfig) -> bool {
    config
        .signal
        .as_ref()
        .is_some_and(|s| s.load(Ordering::Relaxed))
}

/// Hand one [`SessionEvent`] to the loop's `persist` callback (if installed).
/// A persist `Err` propagates: storage failures abort the run (fail loud),
/// never silently swallowed. `persist: None` is a no-op.
fn persist_event(config: &AgentLoopConfig, event: &SessionEvent) -> Result<(), AgentError> {
    match &config.persist {
        Some(persist) => persist(event),
        None => Ok(()),
    }
}

/// Emit `ToolExecutionEnd`, persist the completed tool result, and record it.
/// Every finished result flows through here — successes, tool errors, hook
/// blocks (a refusal is a fact), malformed args, unknown tools, over-limit
/// drops — so the on-disk order always matches execution order. Called only
/// after any `after_tool_call` rewriting, never before.
fn record_tool_result<E>(
    config: &AgentLoopConfig,
    emit: &mut E,
    results: &mut Vec<ToolResultMessage>,
    result: ToolResultMessage,
    is_error: bool,
) -> Result<(), AgentError>
where
    E: FnMut(AgentEvent),
{
    emit(AgentEvent::ToolExecutionEnd {
        tool_call_id: result.tool_call_id.clone(),
        result: result.clone(),
        is_error,
    });
    persist_event(
        config,
        &SessionEvent::ToolResult(AgentMessage::ToolResult(result.clone())),
    )?;
    results.push(result);
    Ok(())
}

/// Build an error ToolResult for every tool call in `assistant` (used on
/// MaxTokens truncation, where calls may be partial).
fn fail_all_tool_calls(assistant: &AssistantMessage) -> Vec<ToolResultMessage> {
    assistant
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolCall { tool_call: tc } => Some(ToolResultMessage {
                tool_call_id: tc.id.clone(),
                tool_name: tc.function.name.clone(),
                content: vec![ContentBlock::Text {
                    text:
                        "Error: assistant output was truncated (max_tokens); tool call discarded."
                            .into(),
                }],
                is_error: true,
                timestamp: crate::now(),
            }),
            _ => None,
        })
        .collect()
}

/// Build an error [`ToolResultMessage`] for a tool call that failed before or
/// during execution (missing/malformed args, unknown tool, tool-internal
/// error). The agent loop feeds this back to the LLM so the model can retry,
/// rather than aborting the whole run.
fn error_tool_result(
    tool_call_id: &str,
    tool_name: &str,
    message: impl Into<String>,
) -> ToolResultMessage {
    ToolResultMessage {
        tool_call_id: tool_call_id.into(),
        tool_name: tool_name.into(),
        content: vec![ContentBlock::text(message)],
        is_error: true,
        timestamp: crate::now(),
    }
}

/// Execute every tool call in `assistant`, running before/after hooks (V0.1:
/// hooks are no-ops; wired in stage 3g).
async fn execute_tool_calls<E>(
    context: &AgentContext,
    assistant: &AssistantMessage,
    config: &AgentLoopConfig,
    guard: &mut crate::guard::RepeatGuard,
    emit: &mut E,
) -> Result<Vec<ToolResultMessage>, AgentError>
where
    E: FnMut(AgentEvent),
{
    let tool_calls: Vec<_> = assistant
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolCall { tool_call: tc } => Some(tc.clone()),
            _ => None,
        })
        .collect();

    let mut results = Vec::with_capacity(tool_calls.len());
    for (i, tc) in tool_calls.into_iter().enumerate() {
        if i >= config.max_tool_calls_per_turn {
            // Limit reached: report the dropped call as an error tool_result so
            // the model sees one result per call instead of a silent gap.
            let limit = config.max_tool_calls_per_turn;
            let result = error_tool_result(
                &tc.id,
                &tc.function.name,
                format!("tool call limit reached ({limit} per turn); call dropped"),
            );
            record_tool_result(config, emit, &mut results, result, true)?;
            continue;
        }
        // Cooperative abort between tool calls in a batch.
        if is_aborted(config) {
            break;
        }

        // Parse the accumulated tool-call arguments. Empty -> `{}` (a tool may
        // take no params); malformed JSON -> feed the parse error back to the
        // LLM as a tool_result so it can retry, instead of silently degrading
        // to `Null` and crashing inside the tool.
        let mut args: serde_json::Value = match serde_json::from_str(&tc.function.arguments) {
            Ok(v) => v,
            Err(_) if tc.function.arguments.trim().is_empty() => {
                serde_json::Value::Object(Default::default())
            }
            Err(e) => {
                tracing::warn!(tool = %tc.function.name, error = %e, "malformed tool arguments");
                let result = error_tool_result(
                    &tc.id,
                    &tc.function.name,
                    format!(
                        "failed to parse tool arguments as JSON: {e}\nraw arguments: {:?}",
                        tc.function.arguments
                    ),
                );
                record_tool_result(config, emit, &mut results, result, true)?;
                continue;
            }
        };

        // 1. before_tool_call hook: consult the hook chain if installed.
        //    Risk is looked up from the tool definition (unknown tools default
        //    to High - the safe default, matching the old host-side table).
        let risk = context
            .tools
            .iter()
            .find(|t| t.name == tc.function.name)
            .map(|t| t.risk)
            .unwrap_or(RiskLevel::High);
        let verdict = match &config.hooks {
            Some(h) => {
                h.before_tool_call(&tc.id, &tc.function.name, &args, risk)
                    .await
            }
            None => ToolCallVerdict::Allow,
        };
        match verdict {
            ToolCallVerdict::Block(reason) => {
                tracing::warn!(tool = %tc.function.name, "tool blocked by before_tool_call hook");
                let result = error_tool_result(&tc.id, &tc.function.name, reason);
                record_tool_result(config, emit, &mut results, result, true)?;
                continue;
            }
            ToolCallVerdict::Modify(new_args) => args = new_args,
            ToolCallVerdict::Allow => {}
        }

        // 2. Locate the tool. Unknown tool -> error tool_result (the model may
        //    have hallucinated a name); continue the run.
        let tool = match context.tools.iter().find(|t| t.name == tc.function.name) {
            Some(t) => t,
            None => {
                tracing::warn!(tool = %tc.function.name, "tool not found");
                let result = error_tool_result(
                    &tc.id,
                    &tc.function.name,
                    format!("tool not found: {}", tc.function.name),
                );
                record_tool_result(config, emit, &mut results, result, true)?;
                continue;
            }
        };

        let args_key = args.to_string();

        emit(AgentEvent::ToolExecutionStart {
            tool_call_id: tc.id.clone(),
            tool_name: tc.function.name.clone(),
            args: args.clone(),
        });
        tracing::info!(tool = %tc.function.name, "tool execute");

        // 3. Execute. A tool-internal error becomes an error tool_result fed
        //    back to the LLM; the run continues instead of aborting.
        let raw = match (tool.execute)(ToolCallCtx {
            tool_call_id: tc.id.clone(),
            args,
            signal: config.signal.clone().unwrap_or_default(),
            ctx: ToolContext {
                cwd: context.cwd.clone(),
                env: context.env.clone(),
                session_id: context.session_id.clone(),
                state_dir: tool_state_dir(context, &tc.function.name),
                spawner: context.spawner.clone(),
            },
        })
        .await
        {
            Ok(r) => r,
            Err(e) => {
                let msg = e.to_string();
                tracing::warn!(tool = %tc.function.name, error = %msg, "tool execute error");
                crate::types::tool::ToolResult::error(msg)
            }
        };

        let mut result = ToolResultMessage {
            tool_call_id: tc.id.clone(),
            tool_name: tc.function.name.clone(),
            content: raw.content,
            is_error: raw.is_error,
            timestamp: crate::now(),
        };

        // 4. after_tool_call hook: chain may replace the result (redact, etc.).
        if let Some(h) = &config.hooks {
            result = h.after_tool_call(&tc.id, &result);
        }

        if let Some(note) =
            crate::guard::repeat_advisory(guard.observe(&tc.function.name, &args_key))
        {
            if let Some(crate::ContentBlock::Text { text }) = result.content.first_mut() {
                text.push_str("\n\n[");
                text.push_str(&note);
                text.push(']');
            }
        }

        let is_error = result.is_error;
        record_tool_result(config, emit, &mut results, result, is_error)?;

        tracing::info!(tool = %tc.function.name, is_error, "tool done");
    }

    Ok(results)
}

/// Default per-tool state directory:
/// `<config_dir>/tool_state/<session_id>/<tool_name>/`.
fn tool_state_dir(context: &AgentContext, tool_name: &str) -> std::path::PathBuf {
    crate::storage::config_dir()
        .join("tool_state")
        .join(&context.session_id)
        .join(tool_name)
}

/// Stream one assistant response from the LLM, accumulating into an
/// [`AssistantMessage`] and emitting `MessageUpdate` for each delta.
async fn stream_assistant_response<E>(
    context: &AgentContext,
    config: &AgentLoopConfig,
    emit: &mut E,
) -> Result<AssistantMessage, AgentError>
where
    E: FnMut(AgentEvent),
{
    let max_retries = config.retry.max_retries;
    let mut attempt: usize = 0;
    loop {
        if is_aborted(config) {
            // A cancel arrived while the host was waiting (e.g. an approval
            // prompt): exit before burning a provider request. Mirrors the
            // in-stream abort path's event shape so hosts see the Aborted
            // message the same way.
            tracing::info!("provider request skipped: aborted");
            let mut msg = AssistantMessage::new(&config.model.id);
            msg.stop_reason = StopReason::Aborted;
            emit(AgentEvent::MessageEnd {
                message: msg.clone(),
            });
            emit(AgentEvent::AfterProviderResponse {
                model: config.model.id.clone(),
                response: msg.clone(),
            });
            return Ok(msg);
        }
        attempt += 1;
        emit(AgentEvent::BeforeProviderRequest {
            model: config.model.id.clone(),
        });
        tracing::debug!(model = %config.model.id, attempt, "provider request");

        match attempt_stream_once(context, config, &mut *emit).await {
            StreamAttempt::Done(accumulated) => {
                tracing::debug!(stop_reason = ?accumulated.stop_reason, "provider response");
                emit(AgentEvent::MessageEnd {
                    message: accumulated.clone(),
                });
                emit(AgentEvent::AfterProviderResponse {
                    model: config.model.id.clone(),
                    response: accumulated.clone(),
                });
                return Ok(accumulated);
            }
            StreamAttempt::Errored {
                error,
                emitted_content,
            } => {
                // Only retry when nothing was emitted to the host yet (so the
                // retry is invisible) and the signal isn't already aborting.
                let can_retry = !emitted_content && attempt <= max_retries && !is_aborted(config);
                if can_retry {
                    let delay = backoff_ms(attempt, &config.retry);
                    tracing::warn!(
                        attempt,
                        max_retries,
                        delay_ms = delay,
                        error = %error,
                        "provider stream error, retrying"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                    continue;
                }
                let mut msg = AssistantMessage::new(&config.model.id);
                msg.stop_reason = StopReason::Error(error);
                tracing::debug!(stop_reason = ?msg.stop_reason, "provider response (errored)");
                emit(AgentEvent::MessageEnd {
                    message: msg.clone(),
                });
                emit(AgentEvent::AfterProviderResponse {
                    model: config.model.id.clone(),
                    response: msg.clone(),
                });
                return Ok(msg);
            }
        }
    }
}

/// Outcome of one streaming attempt.
enum StreamAttempt {
    /// Stream completed (normally or via abort). Carries the accumulated message.
    Done(AssistantMessage),
    /// Stream errored. `emitted_content` tells the caller whether any content
    /// delta was already sent to the host - if so, retrying would duplicate it.
    Errored {
        error: String,
        emitted_content: bool,
    },
}

/// Run one streaming attempt: accumulate chunks into an [`AssistantMessage`],
/// emitting `MessageUpdate` for each delta. Returns [`StreamAttempt::Done`] on
/// normal completion (or abort), or [`StreamAttempt::Errored`] on a stream error.
async fn attempt_stream_once<E>(
    context: &AgentContext,
    config: &AgentLoopConfig,
    emit: &mut E,
) -> StreamAttempt
where
    E: FnMut(AgentEvent),
{
    let mut stream: Pin<Box<dyn futures_util::Stream<Item = StreamChunk> + Send>> =
        (config.stream_fn).stream(
            &config.model,
            &context.messages,
            &context.system_prompt,
            &context.tools,
            config.signal.clone(),
        );

    let mut accumulated = AssistantMessage::new(&config.model.id);
    let mut usage = None;
    let mut emitted_content = false;

    while let Some(chunk) = stream.next().await {
        // Cooperative abort: stop accumulating as soon as the signal is set.
        if is_aborted(config) {
            tracing::info!("provider stream aborted");
            accumulated.stop_reason = StopReason::Aborted;
            break;
        }
        match chunk {
            StreamChunk::TextDelta(t) => {
                emitted_content = true;
                accumulated.append_text(&t);
                emit(AgentEvent::MessageUpdate {
                    delta: ContentDelta::TextDelta(t),
                });
            }
            StreamChunk::ToolCallDelta {
                id,
                name,
                args_delta,
            } => {
                emitted_content = true;
                accumulated.append_tool_call(id.clone(), name.clone(), args_delta.clone());
                emit(AgentEvent::MessageUpdate {
                    delta: ContentDelta::ToolCallDelta {
                        id,
                        name,
                        args_delta,
                    },
                });
            }
            StreamChunk::ThinkingDelta(t) => {
                emitted_content = true;
                accumulated.append_thinking(&t);
                emit(AgentEvent::MessageUpdate {
                    delta: ContentDelta::ThinkingDelta(t),
                });
            }
            StreamChunk::Usage { input, output } => {
                // Merge, don't overwrite: Anthropic sends input tokens in
                // `message_start` and output tokens in `message_delta` as two
                // separate Usage chunks. Overwriting would zero input on the
                // second. Both OpenAI (one combined chunk) and Anthropic
                // (complementary partials) sum correctly.
                let u = usage.get_or_insert(crate::types::message::Usage {
                    input_tokens: 0,
                    output_tokens: 0,
                });
                u.input_tokens += input;
                u.output_tokens += output;
            }
            StreamChunk::Done => break,
            StreamChunk::Error(e) => {
                tracing::error!(error = %e, "provider stream error");
                return StreamAttempt::Errored {
                    error: e,
                    emitted_content,
                };
            }
        }
    }

    // The provider may end its stream without a Done chunk when the abort
    // signal stopped the download mid-flight; preserve the Aborted marker so
    // the persisted partial transcript stays honest. If the model emitted
    // tool calls the turn would otherwise continue; otherwise it ended.
    if accumulated.stop_reason != StopReason::Aborted {
        accumulated.stop_reason = if is_aborted(config) {
            StopReason::Aborted
        } else if accumulated
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolCall { .. }))
        {
            StopReason::ToolUse
        } else {
            StopReason::EndTurn
        };
    }

    accumulated.usage = usage;
    StreamAttempt::Done(accumulated)
}

/// Exponential backoff for retry `attempt` (1-based): `initial * 2^(attempt-1)`,
/// capped at `max`. Returns 0 when `initial` is 0 (no delay).
fn backoff_ms(attempt: usize, policy: &crate::types::context::RetryPolicy) -> u64 {
    if policy.initial_delay_ms == 0 {
        return 0;
    }
    let shift = attempt.saturating_sub(1).min(10);
    let base = policy.initial_delay_ms.saturating_mul(1u64 << shift);
    base.min(policy.max_delay_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::context::{ModelSpec, ProviderApi};
    use crate::types::tool::ToolDefinition;
    use crate::ExtensionApi;
    use crate::StreamFn;
    use crate::ThinkingLevel;
    use futures_util::stream;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use std::sync::Mutex;

    use crate::types::session_event::SessionEvent;

    /// A mock StreamFn that replays a fixed chunk sequence.
    struct MockStream(Vec<StreamChunk>);
    impl StreamFn for MockStream {
        fn stream(
            &self,
            _model: &ModelSpec,
            _messages: &[AgentMessage],
            _system_prompt: &str,
            _tools: &[crate::types::tool::ToolDefinition],
            _signal: Option<std::sync::Arc<AtomicBool>>,
        ) -> Pin<Box<dyn futures_util::Stream<Item = StreamChunk> + Send>> {
            Box::pin(stream::iter(self.0.clone()))
        }
    }

    fn test_config(chunks: Vec<StreamChunk>) -> AgentLoopConfig {
        AgentLoopConfig {
            model: ModelSpec {
                id: "test".into(),
                api: ProviderApi::OpenAiCompat,
                max_tokens: 1024,
                supports_thinking: false,
            },
            thinking_level: ThinkingLevel::Off,
            max_turns: 5,
            max_tool_calls_per_turn: 5,
            signal: None,
            stream_fn: std::sync::Arc::new(MockStream(chunks)),
            hooks: None,
            retry: crate::RetryPolicy::off(),
            persist: None,
        }
    }

    #[tokio::test]
    async fn loop_emits_text_and_ends() {
        // Model streams "Hello" then " world" and ends naturally.
        let config = test_config(vec![
            StreamChunk::TextDelta("Hello".into()),
            StreamChunk::TextDelta(" world".into()),
            StreamChunk::Usage {
                input: 3,
                output: 2,
            },
            StreamChunk::Done,
        ]);
        let context = AgentContext {
            system_prompt: "sys".into(),
            messages: vec![],
            tools: vec![],
            cwd: ".".into(),
            env: Default::default(),
            session_id: "s1".into(),
            spawner: None,
        };

        let mut saw_start = false;
        let mut saw_end = false;
        let msgs = run_agent_loop(vec![], context, config, |ev| match ev {
            AgentEvent::AgentStart => saw_start = true,
            AgentEvent::AgentEnd => saw_end = true,
            _ => {}
        })
        .await
        .unwrap();

        assert!(saw_start && saw_end);
        // One assistant message with the full text.
        let any_assistant = msgs.iter().any(|m| {
            matches!(m, AgentMessage::Assistant(a) if a
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::Text { text } if text == "Hello world")))
        });
        assert!(any_assistant, "expected accumulated 'Hello world' text");
    }

    #[tokio::test]
    async fn loop_executes_tool_then_ends() {
        // A tool that echoes its args as text.
        let echo = crate::types::tool::ToolDefinition {
            name: "echo".into(),
            label: "Echo".into(),
            description: "echo args".into(),
            parameters: serde_json::json!({"type": "object"}),
            risk: RiskLevel::Low,
            execute: std::sync::Arc::new(|c: ToolCallCtx| {
                Box::pin(
                    async move { Ok(crate::types::tool::ToolResult::text(c.args.to_string())) },
                )
            }),
        };
        // Model: tool_call(echo, {"x":1}) -> then plain text "done".
        let config = test_config(vec![
            StreamChunk::ToolCallDelta {
                id: "t1".into(),
                name: Some("echo".into()),
                args_delta: "{\"x\":1}".into(),
            },
            StreamChunk::Done,
        ]);
        let context = AgentContext {
            system_prompt: "sys".into(),
            messages: vec![],
            tools: vec![echo],
            cwd: ".".into(),
            env: Default::default(),
            session_id: "s2".into(),
            spawner: None,
        };

        let msgs = run_agent_loop(vec![], context, config, |_| {})
            .await
            .unwrap();

        // Expect: Assistant(tool_call) + ToolResult(echo output).
        let has_tool_result = msgs.iter().any(|m| {
            matches!(m, AgentMessage::ToolResult(tr) if tr.tool_name == "echo"
                && tr.content.iter().any(|b| matches!(b, ContentBlock::Text { text } if text == "{\"x\":1}")))
        });
        assert!(has_tool_result, "expected echo tool result");
    }

    #[tokio::test]
    async fn loop_assembles_chunked_tool_call() {
        // Model streams ONE tool call across two deltas: the first carries
        // id+name, the second (continuation) carries only args with an empty
        // id - the OpenAI-compat streaming shape. Must assemble into ONE call,
        // not split into two.
        let echo = crate::types::tool::ToolDefinition {
            name: "echo".into(),
            label: "Echo".into(),
            description: "echo args".into(),
            parameters: serde_json::json!({"type": "object"}),
            risk: RiskLevel::Low,
            execute: std::sync::Arc::new(|c: ToolCallCtx| {
                Box::pin(
                    async move { Ok(crate::types::tool::ToolResult::text(c.args.to_string())) },
                )
            }),
        };
        let config = test_config(vec![
            StreamChunk::ToolCallDelta {
                id: "t1".into(),
                name: Some("echo".into()),
                args_delta: "{\"x\":".into(),
            },
            StreamChunk::ToolCallDelta {
                id: String::new(),
                name: None,
                args_delta: "1}".into(),
            },
            StreamChunk::Done,
        ]);
        let context = AgentContext {
            system_prompt: "sys".into(),
            messages: vec![],
            tools: vec![echo],
            cwd: ".".into(),
            env: Default::default(),
            session_id: "s".into(),
            spawner: None,
        };

        let msgs = run_agent_loop(vec![], context, config, |_| {})
            .await
            .unwrap();

        // Each turn replays the same chunked call, so the mock drives max_turns
        // turns. What matters is that the two deltas assemble into ONE call per
        // turn with full args - not a split into a named-no-args call plus an
        // empty-name-with-args call (the pre-fix behavior).
        let echo_results: Vec<_> = msgs
            .iter()
            .filter_map(|m| match m {
                AgentMessage::ToolResult(tr) if tr.tool_name == "echo" => Some(tr.clone()),
                _ => None,
            })
            .collect();
        assert!(
            !echo_results.is_empty(),
            "expected at least one echo tool result"
        );
        for tr in &echo_results {
            let text = match &tr.content[0] {
                ContentBlock::Text { text } => text.clone(),
                _ => panic!("expected text content"),
            };
            assert!(
                text.contains("\"x\":1"),
                "expected assembled args, got: {text}"
            );
        }
        // The bug would leak the args fragment into a split empty-name call.
        let split = msgs
            .iter()
            .any(|m| matches!(m, AgentMessage::ToolResult(tr) if tr.tool_name.is_empty()));
        assert!(
            !split,
            "chunked args leaked into a split empty-name tool call"
        );
    }

    /// A `before_tool_call` handler that blocks the `bash` tool.
    struct BlockBash;
    impl crate::extension::BeforeToolCallHandler for BlockBash {
        fn call(
            &self,
            _id: &str,
            tool_name: &str,
            _args: &serde_json::Value,
            _risk: RiskLevel,
        ) -> ToolCallVerdict {
            if tool_name == "bash" {
                ToolCallVerdict::Block("blocked by policy".into())
            } else {
                ToolCallVerdict::Allow
            }
        }
    }

    /// An `after_tool_call` handler that redacts text content.
    struct Redactor;
    impl crate::extension::AfterToolCallHandler for Redactor {
        fn call(
            &self,
            _id: &str,
            _result: &crate::ToolResultMessage,
        ) -> Option<crate::ToolResultMessage> {
            Some(crate::ToolResultMessage {
                tool_call_id: "t1".into(),
                tool_name: "echo".into(),
                content: vec![ContentBlock::text("[REDACTED]")],
                is_error: false,
                timestamp: 0,
            })
        }
    }

    #[tokio::test]
    async fn before_hook_blocks_tool() {
        // Register a BlockBash handler, then have the model call `bash`.
        let mut api = crate::extension::ExtensionApiImpl::new();
        api.register_before_tool_call(Box::new(BlockBash));

        let echo = crate::ToolDefinition {
            name: "bash".into(),
            label: "Bash".into(),
            description: "shell".into(),
            parameters: serde_json::json!({"type": "object"}),
            risk: RiskLevel::Low,
            execute: std::sync::Arc::new(|_c: ToolCallCtx| {
                Box::pin(async move { Ok(crate::ToolResult::text("ran")) })
            }),
        };
        let config = AgentLoopConfig {
            model: ModelSpec {
                id: "test".into(),
                api: ProviderApi::OpenAiCompat,
                max_tokens: 1024,
                supports_thinking: false,
            },
            thinking_level: ThinkingLevel::Off,
            max_turns: 5,
            max_tool_calls_per_turn: 5,
            signal: None,
            stream_fn: std::sync::Arc::new(MockStream(vec![
                StreamChunk::ToolCallDelta {
                    id: "t1".into(),
                    name: Some("bash".into()),
                    args_delta: "{\"command\":\"rm -rf /\"}".into(),
                },
                StreamChunk::Done,
            ])),
            hooks: Some(std::sync::Arc::new(api)),
            persist: None,
            retry: crate::RetryPolicy::off(),
        };
        let context = AgentContext {
            system_prompt: "sys".into(),
            messages: vec![],
            tools: vec![echo],
            cwd: ".".into(),
            env: Default::default(),
            session_id: "s3".into(),
            spawner: None,
        };

        let msgs = run_agent_loop(vec![], context, config, |_| {})
            .await
            .unwrap();

        // The tool was NOT executed — instead we got an error result with the
        // block reason.
        let blocked = msgs.iter().any(|m| {
            matches!(m, AgentMessage::ToolResult(tr)
                if tr.tool_name == "bash" && tr.is_error
                && tr.content.iter().any(|b| matches!(b, ContentBlock::Text { text } if text == "blocked by policy")))
        });
        assert!(blocked, "expected bash to be blocked, not executed");
    }

    #[tokio::test]
    async fn after_hook_redacts_result() {
        let mut api = crate::extension::ExtensionApiImpl::new();
        api.register_after_tool_call(Box::new(Redactor));

        let echo = crate::ToolDefinition {
            name: "echo".into(),
            label: "Echo".into(),
            description: "echo".into(),
            parameters: serde_json::json!({"type": "object"}),
            risk: RiskLevel::Low,
            execute: std::sync::Arc::new(|c: ToolCallCtx| {
                Box::pin(async move { Ok(crate::ToolResult::text(c.args.to_string())) })
            }),
        };
        let config = AgentLoopConfig {
            model: ModelSpec {
                id: "test".into(),
                api: ProviderApi::OpenAiCompat,
                max_tokens: 1024,
                supports_thinking: false,
            },
            thinking_level: ThinkingLevel::Off,
            max_turns: 5,
            max_tool_calls_per_turn: 5,
            signal: None,
            stream_fn: std::sync::Arc::new(MockStream(vec![
                StreamChunk::ToolCallDelta {
                    id: "t1".into(),
                    name: Some("echo".into()),
                    args_delta: "{\"secret\":1}".into(),
                },
                StreamChunk::Done,
            ])),
            hooks: Some(std::sync::Arc::new(api)),
            persist: None,
            retry: crate::RetryPolicy::off(),
        };
        let context = AgentContext {
            system_prompt: "sys".into(),
            messages: vec![],
            tools: vec![echo],
            cwd: ".".into(),
            env: Default::default(),
            session_id: "s4".into(),
            spawner: None,
        };

        let msgs = run_agent_loop(vec![], context, config, |_| {})
            .await
            .unwrap();

        // Original secret output was replaced by [REDACTED].
        let redacted = msgs.iter().any(|m| {
            matches!(m, AgentMessage::ToolResult(tr)
                if tr.content.iter().any(|b| matches!(b, ContentBlock::Text { text } if text == "[REDACTED]")))
        });
        let leaked = msgs.iter().any(|m| {
            matches!(m, AgentMessage::ToolResult(tr)
                if tr.content.iter().any(|b| matches!(b, ContentBlock::Text { text } if text.contains("secret"))))
        });
        assert!(redacted, "expected redacted result");
        assert!(!leaked, "secret must not appear in output");
    }

    #[tokio::test]
    async fn loop_recovers_from_tool_error() {
        // A tool whose execute returns Err. The loop must feed an error
        // tool_result back to the LLM and continue, not abort the run.
        let boom = crate::types::tool::ToolDefinition {
            name: "boom".into(),
            label: "Boom".into(),
            description: "always fails".into(),
            parameters: serde_json::json!({"type": "object"}),
            risk: RiskLevel::Low,
            execute: std::sync::Arc::new(|_c: ToolCallCtx| {
                Box::pin(async move { Err(crate::error::ToolError::Message("boom".into())) })
            }),
        };
        let config = test_config(vec![
            StreamChunk::ToolCallDelta {
                id: "t1".into(),
                name: Some("boom".into()),
                args_delta: "{}".into(),
            },
            StreamChunk::Done,
        ]);
        let context = AgentContext {
            system_prompt: "sys".into(),
            messages: vec![],
            tools: vec![boom],
            cwd: ".".into(),
            env: Default::default(),
            session_id: "s".into(),
            spawner: None,
        };

        let msgs = run_agent_loop(vec![], context, config, |_| {})
            .await
            .unwrap();

        let has_error_result = msgs.iter().any(|m| {
            matches!(m, AgentMessage::ToolResult(tr)
                if tr.tool_name == "boom" && tr.is_error
                && tr.content.iter().any(|b| matches!(b, ContentBlock::Text { text } if text.contains("boom"))))
        });
        assert!(
            has_error_result,
            "expected an error tool_result, not a crash"
        );
    }

    #[tokio::test]
    async fn loop_handles_malformed_tool_args() {
        // The model streams malformed argument JSON. The loop must report a
        // parse error as a tool_result and continue, not crash inside the tool.
        let echo = crate::types::tool::ToolDefinition {
            name: "echo".into(),
            label: "Echo".into(),
            description: "echo".into(),
            parameters: serde_json::json!({"type": "object"}),
            risk: RiskLevel::Low,
            execute: std::sync::Arc::new(|c: ToolCallCtx| {
                Box::pin(
                    async move { Ok(crate::types::tool::ToolResult::text(c.args.to_string())) },
                )
            }),
        };
        let config = test_config(vec![
            StreamChunk::ToolCallDelta {
                id: "t1".into(),
                name: Some("echo".into()),
                args_delta: "{\"command\":".into(),
            },
            StreamChunk::Done,
        ]);
        let context = AgentContext {
            system_prompt: "sys".into(),
            messages: vec![],
            tools: vec![echo],
            cwd: ".".into(),
            env: Default::default(),
            session_id: "s".into(),
            spawner: None,
        };

        let msgs = run_agent_loop(vec![], context, config, |_| {})
            .await
            .unwrap();

        let has_error_result = msgs.iter().any(|m| {
            matches!(m, AgentMessage::ToolResult(tr)
                if tr.tool_name == "echo" && tr.is_error
                && tr.content.iter().any(|b| matches!(b, ContentBlock::Text { text } if text.contains("failed to parse tool arguments"))))
        });
        assert!(
            has_error_result,
            "expected a parse-error tool_result, not a crash"
        );
    }

    #[tokio::test]
    async fn loop_handles_unknown_tool() {
        // The model calls a tool that was never registered. The loop must
        // report "tool not found" as a tool_result and continue.
        let config = test_config(vec![
            StreamChunk::ToolCallDelta {
                id: "t1".into(),
                name: Some("ghost".into()),
                args_delta: "{}".into(),
            },
            StreamChunk::Done,
        ]);
        let context = AgentContext {
            system_prompt: "sys".into(),
            messages: vec![],
            tools: vec![],
            cwd: ".".into(),
            env: Default::default(),
            session_id: "s".into(),
            spawner: None,
        };

        let msgs = run_agent_loop(vec![], context, config, |_| {})
            .await
            .unwrap();

        let has_error_result = msgs.iter().any(|m| {
            matches!(m, AgentMessage::ToolResult(tr)
                if tr.tool_name == "ghost" && tr.is_error
                && tr.content.iter().any(|b| matches!(b, ContentBlock::Text { text } if text.contains("tool not found"))))
        });
        assert!(
            has_error_result,
            "expected a not-found tool_result, not a crash"
        );
    }

    /// A mock that flips the abort signal on when streaming starts, then yields
    /// a text delta + Done. Exercises the in-stream abort check.
    struct FlipOnStream;
    impl StreamFn for FlipOnStream {
        fn stream(
            &self,
            _model: &ModelSpec,
            _messages: &[AgentMessage],
            _system_prompt: &str,
            _tools: &[crate::types::tool::ToolDefinition],
            signal: Option<std::sync::Arc<AtomicBool>>,
        ) -> Pin<Box<dyn futures_util::Stream<Item = StreamChunk> + Send>> {
            if let Some(s) = signal {
                s.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            Box::pin(stream::iter(vec![
                StreamChunk::TextDelta("partial".into()),
                StreamChunk::Done,
            ]))
        }
    }

    #[tokio::test]
    async fn loop_aborts_during_stream() {
        // The signal flips during streaming (the mock sets it when stream() is
        // called), so the top-of-turn guard doesn't fire first. The stream must
        // stop after the first chunk and carry StopReason::Aborted.
        let config = AgentLoopConfig {
            model: ModelSpec {
                id: "test".into(),
                api: ProviderApi::OpenAiCompat,
                max_tokens: 1024,
                supports_thinking: false,
            },
            thinking_level: ThinkingLevel::Off,
            max_turns: 5,
            max_tool_calls_per_turn: 5,
            signal: Some(std::sync::Arc::new(AtomicBool::new(false))),
            stream_fn: std::sync::Arc::new(FlipOnStream),
            hooks: None,
            persist: None,
            retry: crate::RetryPolicy::off(),
        };
        let context = AgentContext {
            system_prompt: "sys".into(),
            messages: vec![],
            tools: vec![],
            cwd: ".".into(),
            env: Default::default(),
            session_id: "s".into(),
            spawner: None,
        };

        let msgs = run_agent_loop(vec![], context, config, |_| {})
            .await
            .unwrap();

        let aborted = msgs.iter().any(
            |m| matches!(m, AgentMessage::Assistant(a) if a.stop_reason == StopReason::Aborted),
        );
        assert!(
            aborted,
            "expected an assistant message with stop_reason Aborted"
        );
    }

    #[tokio::test]
    async fn loop_aborts_mid_tool_batch() {
        // Two tool calls in one turn. The first (`set_abort`) flips the signal;
        // the second (`echo`) must NOT execute.
        let set_abort = crate::types::tool::ToolDefinition {
            name: "set_abort".into(),
            label: "SetAbort".into(),
            description: "sets abort".into(),
            parameters: serde_json::json!({"type": "object"}),
            risk: RiskLevel::Low,
            execute: std::sync::Arc::new(|c: ToolCallCtx| {
                Box::pin(async move {
                    c.signal.store(true, std::sync::atomic::Ordering::Relaxed);
                    Ok(crate::types::tool::ToolResult::text("flipped"))
                })
            }),
        };
        let echo = crate::types::tool::ToolDefinition {
            name: "echo".into(),
            label: "Echo".into(),
            description: "echo".into(),
            parameters: serde_json::json!({"type": "object"}),
            risk: RiskLevel::Low,
            execute: std::sync::Arc::new(|c: ToolCallCtx| {
                Box::pin(
                    async move { Ok(crate::types::tool::ToolResult::text(c.args.to_string())) },
                )
            }),
        };
        let mut config = test_config(vec![
            StreamChunk::ToolCallDelta {
                id: "t1".into(),
                name: Some("set_abort".into()),
                args_delta: "{}".into(),
            },
            StreamChunk::ToolCallDelta {
                id: "t2".into(),
                name: Some("echo".into()),
                args_delta: "{}".into(),
            },
            StreamChunk::Done,
        ]);
        config.signal = Some(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
            false,
        )));

        let context = AgentContext {
            system_prompt: "sys".into(),
            messages: vec![],
            tools: vec![set_abort, echo],
            cwd: ".".into(),
            env: Default::default(),
            session_id: "s".into(),
            spawner: None,
        };

        let msgs = run_agent_loop(vec![], context, config, |_| {})
            .await
            .unwrap();

        let ran_set_abort = msgs
            .iter()
            .any(|m| matches!(m, AgentMessage::ToolResult(tr) if tr.tool_name == "set_abort"));
        let ran_echo = msgs
            .iter()
            .any(|m| matches!(m, AgentMessage::ToolResult(tr) if tr.tool_name == "echo"));
        assert!(
            ran_set_abort,
            "first tool should have executed before abort"
        );
        assert!(!ran_echo, "second tool must not execute after abort");
    }

    /// A mock that errors `failures` times, then replays `success`. Shares a
    /// call counter across attempts (StreamFn is `&self`).
    struct FlakyStream {
        failures: usize,
        success: Vec<StreamChunk>,
        calls: std::sync::atomic::AtomicUsize,
    }
    impl StreamFn for FlakyStream {
        fn stream(
            &self,
            _model: &ModelSpec,
            _messages: &[AgentMessage],
            _system_prompt: &str,
            _tools: &[crate::types::tool::ToolDefinition],
            _signal: Option<std::sync::Arc<AtomicBool>>,
        ) -> Pin<Box<dyn futures_util::Stream<Item = StreamChunk> + Send>> {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n < self.failures {
                Box::pin(stream::iter(vec![StreamChunk::Error("transient".into())]))
            } else {
                Box::pin(stream::iter(self.success.clone()))
            }
        }
    }

    #[tokio::test]
    async fn loop_retries_transient_provider_error() {
        // First two attempts error before any content; third succeeds. With
        // max_retries=2 the run must recover and produce the success text.
        let mut config = test_config(vec![]);
        config.stream_fn = std::sync::Arc::new(FlakyStream {
            failures: 2,
            success: vec![StreamChunk::TextDelta("ok".into()), StreamChunk::Done],
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        config.retry = crate::RetryPolicy {
            max_retries: 2,
            initial_delay_ms: 1,
            max_delay_ms: 10,
        };
        let context = AgentContext {
            system_prompt: "sys".into(),
            messages: vec![],
            tools: vec![],
            cwd: ".".into(),
            env: Default::default(),
            session_id: "s".into(),
            spawner: None,
        };
        let msgs = run_agent_loop(vec![], context, config, |_| {})
            .await
            .unwrap();
        let ok = msgs.iter().any(|m| {
            matches!(m, AgentMessage::Assistant(a)
                if a.content.iter().any(|b| matches!(b, ContentBlock::Text { text } if text == "ok")))
        });
        assert!(ok, "expected retry to recover and produce 'ok'");
    }

    #[tokio::test]
    async fn loop_does_not_retry_after_content_emitted() {
        // Stream emits text then errors mid-stream. Retry must NOT fire (would
        // duplicate emitted content); the error surfaces as stop_reason::Error.
        let mut config = test_config(vec![
            StreamChunk::TextDelta("partial".into()),
            StreamChunk::Error("mid-stream boom".into()),
        ]);
        config.retry = crate::RetryPolicy {
            max_retries: 3,
            initial_delay_ms: 1,
            max_delay_ms: 10,
        };
        let context = AgentContext {
            system_prompt: "sys".into(),
            messages: vec![],
            tools: vec![],
            cwd: ".".into(),
            env: Default::default(),
            session_id: "s".into(),
            spawner: None,
        };
        let mut text_deltas = 0u32;
        let msgs = run_agent_loop(vec![], context, config, |ev| {
            if matches!(
                ev,
                AgentEvent::MessageUpdate {
                    delta: ContentDelta::TextDelta(_)
                }
            ) {
                text_deltas += 1;
            }
        })
        .await
        .unwrap();
        let errored = msgs.iter().any(|m| {
            matches!(m, AgentMessage::Assistant(a)
                if matches!(a.stop_reason, StopReason::Error(_)))
        });
        assert_eq!(
            text_deltas, 1,
            "mid-stream error must not retry (would re-emit content)"
        );
        assert!(
            errored,
            "mid-stream error should surface as stop_reason::Error"
        );
    }

    #[tokio::test]
    async fn over_limit_tool_calls_reported_not_silent() {
        // max_tool_calls_per_turn = 1, but the model emits 2 calls. The second
        // must come back as an error tool_result (not a silent gap).
        let echo = crate::types::tool::ToolDefinition {
            name: "echo".into(),
            label: "Echo".into(),
            description: "echo".into(),
            parameters: serde_json::json!({"type": "object"}),
            risk: RiskLevel::Low,
            execute: std::sync::Arc::new(|c: ToolCallCtx| {
                Box::pin(
                    async move { Ok(crate::types::tool::ToolResult::text(c.args.to_string())) },
                )
            }),
        };
        let mut config = test_config(vec![
            StreamChunk::ToolCallDelta {
                id: "t1".into(),
                name: Some("echo".into()),
                args_delta: "{}".into(),
            },
            StreamChunk::ToolCallDelta {
                id: "t2".into(),
                name: Some("echo".into()),
                args_delta: "{}".into(),
            },
            StreamChunk::Done,
        ]);
        config.max_tool_calls_per_turn = 1;
        let context = AgentContext {
            system_prompt: "sys".into(),
            messages: vec![],
            tools: vec![echo],
            cwd: ".".into(),
            env: Default::default(),
            session_id: "s".into(),
            spawner: None,
        };
        let msgs = run_agent_loop(vec![], context, config, |_| {})
            .await
            .unwrap();
        // t2 must surface as an error tool_result mentioning the limit.
        let dropped = msgs.iter().any(|m| {
            matches!(m, AgentMessage::ToolResult(tr)
                if tr.is_error && tr.content.iter().any(|b| matches!(b, ContentBlock::Text { text } if text.contains("tool call limit"))))
        });
        assert!(
            dropped,
            "over-limit call must be reported as an error, not dropped silently"
        );
    }

    #[tokio::test]
    async fn usage_merges_across_chunks() {
        // Two complementary Usage chunks (Anthropic shape: input then output).
        // Final usage must hold both, not just the last one.
        let config = test_config(vec![
            StreamChunk::Usage {
                input: 42,
                output: 0,
            },
            StreamChunk::Usage {
                input: 0,
                output: 7,
            },
            StreamChunk::Done,
        ]);
        let context = AgentContext {
            system_prompt: "sys".into(),
            messages: vec![],
            tools: vec![],
            cwd: ".".into(),
            env: Default::default(),
            session_id: "s".into(),
            spawner: None,
        };
        let msgs = run_agent_loop(vec![], context, config, |_| {})
            .await
            .unwrap();
        let merged = msgs.iter().any(|m| {
            matches!(m, AgentMessage::Assistant(a)
                if a.usage.as_ref().is_some_and(|u| u.input_tokens == 42 && u.output_tokens == 7))
        });
        assert!(merged, "usage must merge input+output across chunks");
    }

    /// Test stream that must never be polled: the pre-set abort has to stop
    /// the loop before the provider is touched.
    struct PollCountingStream {
        polls: std::sync::atomic::AtomicUsize,
    }
    impl StreamFn for PollCountingStream {
        fn stream(
            &self,
            _model: &ModelSpec,
            _messages: &[AgentMessage],
            _system: &str,
            _tools: &[ToolDefinition],
            _signal: Option<Arc<AtomicBool>>,
        ) -> Pin<Box<dyn futures_util::Stream<Item = StreamChunk> + Send>> {
            let n = self.polls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(
                n, 0,
                "provider stream must not be polled after a pre-set abort"
            );
            Box::pin(futures_util::stream::iter(vec![]))
        }
    }

    #[tokio::test]
    async fn pre_set_signal_aborts_before_provider_request() {
        let mut config = test_config(vec![]);
        config.signal = Some(Arc::new(AtomicBool::new(true)));
        config.stream_fn = Arc::new(PollCountingStream {
            polls: Default::default(),
        });
        let ctx = AgentContext {
            system_prompt: "sys".into(),
            messages: vec![],
            tools: vec![],
            cwd: std::env::current_dir().unwrap(),
            env: std::collections::HashMap::new(),
            session_id: "test".into(),
            spawner: None,
        };
        let msgs = crate::agent_loop(vec![], ctx, config).await.unwrap();
        assert!(
            msgs.iter().any(|m| matches!(
                m,
                AgentMessage::Assistant(a) if a.stop_reason == StopReason::Aborted
            )),
            "pre-set signal must produce an Aborted message"
        );
    }

    #[tokio::test]
    async fn persist_writes_assistant_before_tool_results() {
        // Scripted stream: text + tool_call + usage. The persist callback
        // must observe the assembled Assistant (with its tool_call block and
        // THIS step's usage) BEFORE any ToolResult - crash-safe ordering.
        let echo = crate::types::tool::ToolDefinition {
            name: "echo".into(),
            label: "Echo".into(),
            description: "echo args".into(),
            parameters: serde_json::json!({"type": "object"}),
            risk: RiskLevel::Low,
            execute: std::sync::Arc::new(|c: ToolCallCtx| {
                Box::pin(
                    async move { Ok(crate::types::tool::ToolResult::text(c.args.to_string())) },
                )
            }),
        };
        let mut config = test_config(vec![
            StreamChunk::TextDelta("checking".into()),
            StreamChunk::ToolCallDelta {
                id: "t1".into(),
                name: Some("echo".into()),
                args_delta: "{}".into(),
            },
            StreamChunk::Usage {
                input: 5,
                output: 3,
            },
            StreamChunk::Done,
        ]);
        let events: Arc<Mutex<Vec<SessionEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        config.persist = Some(Arc::new(move |ev: &SessionEvent| {
            sink.lock().unwrap().push(ev.clone());
            Ok(())
        }));
        let context = AgentContext {
            system_prompt: "sys".into(),
            messages: vec![],
            tools: vec![echo],
            cwd: ".".into(),
            env: Default::default(),
            session_id: "s".into(),
            spawner: None,
        };

        run_agent_loop(vec![], context, config, |_| {})
            .await
            .unwrap();

        let recorded = events.lock().unwrap().clone();
        assert!(
            recorded.len() >= 2,
            "expected at least Assistant + ToolResult"
        );
        match &recorded[0] {
            SessionEvent::Assistant { message, usage } => {
                assert!(
                    matches!(message, AgentMessage::Assistant(a)
                        if a.content.iter().any(|b| matches!(b, ContentBlock::ToolCall { .. }))),
                    "persisted Assistant must carry the tool_call block"
                );
                assert_eq!(
                    usage,
                    &Some(crate::types::message::Usage {
                        input_tokens: 5,
                        output_tokens: 3,
                    }),
                    "persisted Assistant must carry this step's usage"
                );
            }
            other => panic!("expected Assistant first, got {other:?}"),
        }
        assert!(
            matches!(&recorded[1], SessionEvent::ToolResult(AgentMessage::ToolResult(tr))
                if tr.tool_name == "echo"),
            "ToolResult must be persisted after the Assistant"
        );
    }

    #[tokio::test]
    async fn persist_error_aborts_run() {
        // The persist callback fails on its very first call: the run must
        // abort with that Err instead of silently continuing.
        let mut config = test_config(vec![StreamChunk::TextDelta("hi".into()), StreamChunk::Done]);
        config.persist = Some(Arc::new(|_ev: &SessionEvent| {
            Err(AgentError::Io(std::io::Error::other("disk full")))
        }));
        let context = AgentContext {
            system_prompt: "sys".into(),
            messages: vec![],
            tools: vec![],
            cwd: ".".into(),
            env: Default::default(),
            session_id: "s".into(),
            spawner: None,
        };

        let result = run_agent_loop(vec![], context, config, |_| {}).await;
        assert!(result.is_err(), "persist error must abort the run");
    }

    #[tokio::test]
    async fn blocked_tool_call_still_persists_result() {
        // A before_tool_call Block still produces an is_error ToolResult,
        // and that refusal is persisted like any other fact.
        let mut api = crate::extension::ExtensionApiImpl::new();
        api.register_before_tool_call(Box::new(BlockBash));
        let bash = crate::ToolDefinition {
            name: "bash".into(),
            label: "Bash".into(),
            description: "shell".into(),
            parameters: serde_json::json!({"type": "object"}),
            risk: RiskLevel::Low,
            execute: std::sync::Arc::new(|_c: ToolCallCtx| {
                Box::pin(async move { Ok(crate::ToolResult::text("ran")) })
            }),
        };
        let mut config = test_config(vec![
            StreamChunk::ToolCallDelta {
                id: "t1".into(),
                name: Some("bash".into()),
                args_delta: "{\"command\":\"rm -rf /\"}".into(),
            },
            StreamChunk::Done,
        ]);
        config.hooks = Some(std::sync::Arc::new(api));
        let events: Arc<Mutex<Vec<SessionEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        config.persist = Some(Arc::new(move |ev: &SessionEvent| {
            sink.lock().unwrap().push(ev.clone());
            Ok(())
        }));
        let context = AgentContext {
            system_prompt: "sys".into(),
            messages: vec![],
            tools: vec![bash],
            cwd: ".".into(),
            env: Default::default(),
            session_id: "s".into(),
            spawner: None,
        };

        run_agent_loop(vec![], context, config, |_| {})
            .await
            .unwrap();

        let recorded = events.lock().unwrap().clone();
        assert!(
            recorded.len() >= 2,
            "expected Assistant + blocked ToolResult on disk"
        );
        assert!(
            matches!(&recorded[1], SessionEvent::ToolResult(AgentMessage::ToolResult(tr))
                if tr.tool_name == "bash" && tr.is_error
                && tr.content.iter().any(|b| matches!(b, ContentBlock::Text { text } if text == "blocked by policy"))),
            "a blocked call must still persist its is_error ToolResult"
        );
    }
}
