//! Minimal host: read user input, run the agent loop, print streamed tokens.
//!
//! Demonstrates "use gasket to do something" — the full extent of wiring a
//! host needs. See `gasket-refactor-plan.md` §9.
//!
//! Config via env:
//!   GASKET_API_KEY   — provider key
//!   GASKET_BASE_URL  — e.g. https://api.deepseek.com/v1 (OpenAI-compatible)
//!                      or https://api.anthropic.com/v1 (Anthropic)
//!   GASKET_API       — "openai" (default) or "anthropic"
//!   GASKET_MODEL     — model id (default gpt-4o-mini)
//!
//! Without GASKET_API_KEY this prints a canned reply (smoke test of plumbing).

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use gasket_core::{
    agent_loop, AgentContext, AgentMessage, ContentBlock, OpenAiCompat,
    ProviderApi, StreamChunk, StreamFn, ThinkingLevel, UserMessage,
};
use futures_util::Stream;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let prompt = std::env::args().nth(1).unwrap_or_else(|| "Hello!".into());

    let cwd = std::env::current_dir()?;
    let context = AgentContext {
        system_prompt: "You are a helpful, concise assistant.".into(),
        messages: vec![],
        tools: gasket_core::built_in_tools(),
        cwd,
        env: std::env::vars().collect(),
        session_id: uuid::Uuid::new_v4().to_string(),
    };

    let user_msg = AgentMessage::User(UserMessage {
        content: vec![ContentBlock::text(prompt)],
        timestamp: gasket_core::now(),
    });

    // Provider config from env: GASKET_LLM_BASE_URL / KEY / MODEL / *_PROXY.
    // See `ProviderConfig::from_env`. Falls back to a mock if not configured.
    let (model_id, provider): (String, Option<Arc<dyn StreamFn>>) =
        match gasket_core::ProviderConfig::from_env() {
            Ok(cfg) => {
                let model = cfg.model.clone();
                let stream: Arc<dyn StreamFn> = Arc::new(OpenAiCompat::from_config(&cfg));
                (model, Some(stream))
            }
            Err(e) => {
                println!("(no LLM config: {e}; using mock reply)\n");
                ("mock".to_string(), None)
            }
        };
    let stream_fn: Arc<dyn StreamFn> = provider.unwrap_or_else(|| Arc::new(MockStream));

    let config = gasket_core::AgentLoopConfig {
        model: gasket_core::ModelSpec {
            id: model_id.clone(),
            api: ProviderApi::OpenAiCompat,
            max_tokens: 1024,
            supports_thinking: false,
        },
        thinking_level: ThinkingLevel::Off,
        max_turns: 20,
        max_tool_calls_per_turn: 20,
        api_key: None,
        signal: Some(Arc::new(AtomicBool::new(false))),
        stream_fn,
        hooks: None,
    };

    let msgs = agent_loop(vec![user_msg], context, config).await?;

    // Print the final assistant text (streaming already printed deltas in a
    // real host; here we just surface the assembled message).
    for m in &msgs {
        if let AgentMessage::Assistant(a) = m {
            for b in &a.content {
                if let ContentBlock::Text { text } = b {
                    print!("{text}");
                }
            }
        }
    }
    println!();
    Ok(())
}

/// A no-network mock that emits a single canned line — proves the loop runs.
struct MockStream;
impl StreamFn for MockStream {
    fn stream(
        &self,
        _model: &gasket_core::ModelSpec,
        _messages: &[AgentMessage],
        _system: &str,
        _tools: &[gasket_core::ToolDefinition],
        _signal: Option<Arc<AtomicBool>>,
    ) -> std::pin::Pin<Box<dyn Stream<Item = StreamChunk> + Send>> {
        Box::pin(futures_util::stream::iter(vec![
            StreamChunk::TextDelta("(mock) gasket-core loop ran successfully.".into()),
            StreamChunk::Done,
        ]))
    }
}
