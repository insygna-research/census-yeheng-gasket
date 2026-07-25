//! The agent loop — single outer loop: LLM call → tool calls → repeat.
//!
//! See `gasket-refactor-plan.md` §4.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::Ordering;

use futures_util::StreamExt;

use crate::error::AgentError;
use crate::types::context::{AgentContext, AgentLoopConfig, StreamChunk};
use crate::types::event::{AgentEvent, ContentDelta};
use crate::types::message::{
    AgentMessage, AssistantMessage, ContentBlock, StopReason, ToolResultMessage,
};
use crate::types::tool::{ToolCallCtx, ToolCallVerdict, ToolContext};

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

    // Seed context with the initial prompts.
    for msg in &initial_prompts {
        context.messages.push(msg.clone());
        new_messages.push(msg.clone());
        emit(AgentEvent::MessageStart);
        emit(AgentEvent::MessageEnd {
            message: AssistantMessage::new(&config.model.id),
        });
    }

    emit(AgentEvent::AgentStart);
    tracing::info!(model = %config.model.id, session = %context.session_id, "agent loop start");

    // Single outer loop.
    for turn in 0..config.max_turns {
        emit(AgentEvent::TurnStart);
        tracing::info!("agent turn {} start", turn);

        // Cooperative abort before each expensive step.
        if is_aborted(&config) {
            break;
        }

        // 1. Call the LLM.
        let assistant = stream_assistant_response(&context, &config, &mut emit).await?;
        let stop_reason = assistant.stop_reason.clone();
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
        let tool_results = execute_tool_calls(&context, &assistant, &config, &mut emit).await?;
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
fn error_tool_result(tool_call_id: &str, tool_name: &str, message: impl Into<String>) -> ToolResultMessage {
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
                emit(AgentEvent::ToolExecutionEnd {
                    tool_call_id: tc.id.clone(),
                    result: result.clone(),
                    is_error: true,
                });
                results.push(result);
                continue;
            }
        };

        // 1. before_tool_call hook: consult the hook chain if installed.
        //    Block → refuse; Modify → replace args; Allow → proceed.
        let verdict = match &config.hooks {
            Some(h) => h.before_tool_call(&tc.id, &tc.function.name, &args),
            None => ToolCallVerdict::Allow,
        };
        match verdict {
            ToolCallVerdict::Block(reason) => {
                tracing::warn!(tool = %tc.function.name, "tool blocked by before_tool_call hook");
                let result = ToolResultMessage {
                    tool_call_id: tc.id.clone(),
                    tool_name: tc.function.name.clone(),
                    content: vec![ContentBlock::Text { text: reason }],
                    is_error: true,
                    timestamp: crate::now(),
                };
                emit(AgentEvent::ToolExecutionEnd {
                    tool_call_id: tc.id.clone(),
                    result: result.clone(),
                    is_error: true,
                });
                results.push(result);
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
                emit(AgentEvent::ToolExecutionEnd {
                    tool_call_id: tc.id.clone(),
                    result: result.clone(),
                    is_error: true,
                });
                results.push(result);
                continue;
            }
        };

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

        emit(AgentEvent::ToolExecutionEnd {
            tool_call_id: tc.id.clone(),
            result: result.clone(),
            is_error: result.is_error,
        });

        tracing::info!(tool = %tc.function.name, is_error = result.is_error, "tool done");
        results.push(result);
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
    emit(AgentEvent::BeforeProviderRequest {
        model: config.model.id.clone(),
    });
    tracing::debug!(model = %config.model.id, "provider request");

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

    while let Some(chunk) = stream.next().await {
        match chunk {
            StreamChunk::TextDelta(t) => {
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
                accumulated.append_thinking(&t);
                emit(AgentEvent::MessageUpdate {
                    delta: ContentDelta::ThinkingDelta(t),
                });
            }
            StreamChunk::Usage { input, output } => {
                usage = Some(crate::types::message::Usage {
                    input_tokens: input,
                    output_tokens: output,
                });
            }
            StreamChunk::Done => break,
            StreamChunk::Error(e) => {
                tracing::error!(error = %e, "provider stream error");
                accumulated.stop_reason = StopReason::Error(e);
                break;
            }
        }
    }

    // If the model emitted tool calls, the turn continues; otherwise it ended.
    if accumulated.stop_reason != StopReason::Error(String::new()) {
        accumulated.stop_reason = if accumulated
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
    tracing::debug!(stop_reason = ?accumulated.stop_reason, "provider response");
    emit(AgentEvent::MessageEnd {
        message: accumulated.clone(),
    });
    emit(AgentEvent::AfterProviderResponse {
        model: config.model.id.clone(),
        response: accumulated.clone(),
    });

    Ok(accumulated)
}

// Keep the boxed-future type name available for clarity in signatures above.
#[allow(dead_code)]
type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::context::{ModelSpec, ProviderApi};
    use crate::ExtensionApi;
    use crate::StreamFn;
    use crate::ThinkingLevel;
    use futures_util::stream;
    use std::sync::atomic::AtomicBool;

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
            api_key: None,
            signal: None,
            stream_fn: std::sync::Arc::new(MockStream(chunks)),
            hooks: None,
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
            execute: std::sync::Arc::new(|c: ToolCallCtx| {
                Box::pin(async move { Ok(crate::types::tool::ToolResult::text(c.args.to_string())) })
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
        assert!(!echo_results.is_empty(), "expected at least one echo tool result");
        for tr in &echo_results {
            let text = match &tr.content[0] {
                ContentBlock::Text { text } => text.clone(),
                _ => panic!("expected text content"),
            };
            assert!(text.contains("\"x\":1"), "expected assembled args, got: {text}");
        }
        // The bug would leak the args fragment into a split empty-name call.
        let split = msgs
            .iter()
            .any(|m| matches!(m, AgentMessage::ToolResult(tr) if tr.tool_name.is_empty()));
        assert!(!split, "chunked args leaked into a split empty-name tool call");
    }

    /// A `before_tool_call` handler that blocks the `bash` tool.
    struct BlockBash;
    impl crate::extension::BeforeToolCallHandler for BlockBash {
        fn call(
            &self,
            _id: &str,
            tool_name: &str,
            _args: &serde_json::Value,
            _ctx: &crate::ExtensionContext,
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
            _ctx: &crate::ExtensionContext,
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
            api_key: None,
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
        };
        let context = AgentContext {
            system_prompt: "sys".into(),
            messages: vec![],
            tools: vec![echo],
            cwd: ".".into(),
            env: Default::default(),
            session_id: "s3".into(),
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
            api_key: None,
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
        };
        let context = AgentContext {
            system_prompt: "sys".into(),
            messages: vec![],
            tools: vec![echo],
            cwd: ".".into(),
            env: Default::default(),
            session_id: "s4".into(),
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
            execute: std::sync::Arc::new(|_c: ToolCallCtx| {
                Box::pin(async move {
                    Err(crate::error::ToolError::Message("boom".into()))
                })
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
        };

        let msgs = run_agent_loop(vec![], context, config, |_| {})
            .await
            .unwrap();

        let has_error_result = msgs.iter().any(|m| {
            matches!(m, AgentMessage::ToolResult(tr)
                if tr.tool_name == "boom" && tr.is_error
                && tr.content.iter().any(|b| matches!(b, ContentBlock::Text { text } if text.contains("boom"))))
        });
        assert!(has_error_result, "expected an error tool_result, not a crash");
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
            execute: std::sync::Arc::new(|c: ToolCallCtx| {
                Box::pin(async move { Ok(crate::types::tool::ToolResult::text(c.args.to_string())) })
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
        };

        let msgs = run_agent_loop(vec![], context, config, |_| {})
            .await
            .unwrap();

        let has_error_result = msgs.iter().any(|m| {
            matches!(m, AgentMessage::ToolResult(tr)
                if tr.tool_name == "echo" && tr.is_error
                && tr.content.iter().any(|b| matches!(b, ContentBlock::Text { text } if text.contains("failed to parse tool arguments"))))
        });
        assert!(has_error_result, "expected a parse-error tool_result, not a crash");
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
        };

        let msgs = run_agent_loop(vec![], context, config, |_| {})
            .await
            .unwrap();

        let has_error_result = msgs.iter().any(|m| {
            matches!(m, AgentMessage::ToolResult(tr)
                if tr.tool_name == "ghost" && tr.is_error
                && tr.content.iter().any(|b| matches!(b, ContentBlock::Text { text } if text.contains("tool not found"))))
        });
        assert!(has_error_result, "expected a not-found tool_result, not a crash");
    }
}
