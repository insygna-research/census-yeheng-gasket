//! Minimal host: read user input, run the agent loop, print streamed tokens.
//!
//! Demonstrates "use gasket to do something" - the full extent of wiring a
//! host needs. See `gasket-refactor-plan.md` §9.
//!
//! Config via env (load a `.env` with `dotenvy`, or export these):
//!   GASKET_LLM_BASE_URL - provider base URL (e.g. https://api.deepseek.com/v1)
//!   GASKET_LLM_KEY      - API key
//!   GASKET_LLM_MODEL    - model id
//!   GASKET_LLM_API      - "openai" (default) or "anthropic"
//!   GASKET_MAX_TURNS / GASKET_MAX_TOOL_CALLS / GASKET_MAX_TOKENS - loop knobs
//!   GASKET_THINKING     - off|low|medium|high
//!   GASKET_RETRY_*      - retry policy (max / initial_ms / max_ms)
//!
//! Without GASKET_LLM_KEY this prints a canned reply (smoke test of plumbing).

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use futures_util::Stream;
use gasket_core::{
    agent_loop, AgentContext, AgentMessage, AgentTunables, AnthropicProvider, ContentBlock,
    ModelSpec, OpenAiCompat, ProviderApi, StreamChunk, StreamFn, ThinkingLevel, UserMessage,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Load .env if present (host responsibility; real env vars take precedence).
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
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

    // Loop tunables from env (max_turns / max_tokens / thinking / retry ...).
    let tunables = AgentTunables::from_env();

    // Provider connection from env: GASKET_LLM_BASE_URL / KEY / MODEL / API / *_PROXY.
    // cfg.api picks the provider impl. Falls back to a mock if not configured.
    let (model, stream_fn): (ModelSpec, Arc<dyn StreamFn>) =
        match gasket_core::ProviderConfig::from_env() {
            Ok(cfg) => {
                let stream: Arc<dyn StreamFn> = match cfg.api {
                    ProviderApi::OpenAiCompat => Arc::new(OpenAiCompat::from_config(&cfg)),
                    ProviderApi::Anthropic => Arc::new(AnthropicProvider::from_config(&cfg)),
                };
                let model = ModelSpec {
                    id: cfg.model.clone(),
                    api: cfg.api,
                    max_tokens: tunables.max_tokens,
                    supports_thinking: tunables.thinking_level != ThinkingLevel::Off,
                };
                (model, stream)
            }
            Err(e) => {
                println!("(no LLM config: {e}; using mock reply)\n");
                (
                    ModelSpec {
                        id: "mock".into(),
                        api: ProviderApi::OpenAiCompat,
                        max_tokens: tunables.max_tokens,
                        supports_thinking: false,
                    },
                    Arc::new(MockStream),
                )
            }
        };

    let config = gasket_core::AgentLoopConfig {
        model,
        thinking_level: tunables.thinking_level,
        max_turns: tunables.max_turns,
        max_tool_calls_per_turn: tunables.max_tool_calls_per_turn,
        signal: Some(Arc::new(AtomicBool::new(false))),
        stream_fn,
        hooks: None,
        retry: tunables.retry,
    };

    let session_id = context.session_id.clone();
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

    // Persist the transcript (append-only JSONL under ~/.gasket/sessions/).
    let store = gasket_core::JsonlStorage::default_root();
    match store.append_messages(&session_id, &msgs).await {
        Ok(()) => eprintln!(
            "(session {} saved: {})",
            session_id,
            store.messages_path(&session_id).display()
        ),
        Err(e) => eprintln!("(warn: failed to persist session: {e})"),
    }
    Ok(())
}

/// A no-network mock that emits a single canned line - proves the loop runs.
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
