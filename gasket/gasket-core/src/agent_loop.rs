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

    // Single outer loop.
    for _turn in 0..config.max_turns {
        emit(AgentEvent::TurnStart);

        // Cooperative abort before each expensive step.
        if is_aborted(&config) {
            break;
        }

        // 1. Call the LLM.
        let assistant =
            stream_assistant_response(&context, &config, &mut emit).await?;
        let stop_reason = assistant.stop_reason.clone();
        context.messages.push(AgentMessage::Assistant(assistant.clone()));
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
        let tool_results =
            execute_tool_calls(&context, &assistant, &config, &mut emit).await?;
        for r in &tool_results {
            context.messages.push(AgentMessage::ToolResult(r.clone()));
            new_messages.push(AgentMessage::ToolResult(r.clone()));
        }

        emit(AgentEvent::TurnEnd {
            message: assistant,
            tool_results,
        });
    }

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
                    text: "Error: assistant output was truncated (max_tokens); tool call discarded.".into(),
                }],
                is_error: true,
                timestamp: crate::now(),
            }),
            _ => None,
        })
        .collect()
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

        let args: serde_json::Value = serde_json::from_str(&tc.function.arguments)
            .unwrap_or(serde_json::Value::Null);

        // 1. before_tool_call hook (no-op stub until stage 3g).
        match before_tool_call(&tc.id, &tc.function.name, &args) {
            ToolCallVerdict::Block(reason) => {
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
            ToolCallVerdict::Modify(new_args) => {
                let _ = new_args; // applied below; kept simple in V0.1
            }
            ToolCallVerdict::Allow => {}
        }

        // 2. Locate the tool.
        let tool = context
            .tools
            .iter()
            .find(|t| t.name == tc.function.name)
            .ok_or_else(|| AgentError::ToolNotFound(tc.function.name.clone()))?;

        emit(AgentEvent::ToolExecutionStart {
            tool_call_id: tc.id.clone(),
            tool_name: tc.function.name.clone(),
            args: args.clone(),
        });

        // 3. Execute.
        let raw = (tool.execute)(ToolCallCtx {
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
        .map_err(AgentError::from)?;

        let result = ToolResultMessage {
            tool_call_id: tc.id.clone(),
            tool_name: tc.function.name.clone(),
            content: raw.content,
            is_error: raw.is_error,
            timestamp: crate::now(),
        };

        // 4. after_tool_call hook (no-op stub until stage 3g).
        let result = after_tool_call(&tc.id, &result).unwrap_or(result);

        emit(AgentEvent::ToolExecutionEnd {
            tool_call_id: tc.id.clone(),
            result: result.clone(),
            is_error: result.is_error,
        });

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

// ── hook stubs (replaced by real handler chains in stage 3g) ──────────────

fn before_tool_call(
    _id: &str,
    _name: &str,
    _args: &serde_json::Value,
) -> ToolCallVerdict {
    ToolCallVerdict::Allow
}

fn after_tool_call(
    _id: &str,
    result: &ToolResultMessage,
) -> Option<ToolResultMessage> {
    let _ = result;
    None
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
                    delta: ContentDelta::ToolCallDelta { id, name, args_delta },
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
    use crate::ThinkingLevel;
    use crate::StreamFn;
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
        }
    }

    #[tokio::test]
    async fn loop_emits_text_and_ends() {
        // Model streams "Hello" then " world" and ends naturally.
        let config = test_config(vec![
            StreamChunk::TextDelta("Hello".into()),
            StreamChunk::TextDelta(" world".into()),
            StreamChunk::Usage { input: 3, output: 2 },
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
        let any_assistant = msgs
            .iter()
            .any(|m| matches!(m, AgentMessage::Assistant(a) if a
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::Text { text } if text == "Hello world"))));
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
                Box::pin(async move {
                    Ok(crate::types::tool::ToolResult::text(c.args.to_string()))
                })
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

        let msgs = run_agent_loop(vec![], context, config, |_| {}).await.unwrap();

        // Expect: Assistant(tool_call) + ToolResult(echo output).
        let has_tool_result = msgs.iter().any(|m| {
            matches!(m, AgentMessage::ToolResult(tr) if tr.tool_name == "echo"
                && tr.content.iter().any(|b| matches!(b, ContentBlock::Text { text } if text == "{\"x\":1}")))
        });
        assert!(has_tool_result, "expected echo tool result");
    }
}
