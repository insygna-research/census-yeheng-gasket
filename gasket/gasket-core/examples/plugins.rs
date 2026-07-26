//! Example host that loads the 3 example plugins in-process and runs a turn.
//!
//! This is the runnable demonstration of the plugins in `examples/plugins/`.
//! It does NOT load cdylibs (that path is covered by `loader` unit tests);
//! instead it calls each plugin's `register` directly into an
//! `ExtensionApiImpl`, wires the hook chain into `AgentLoopConfig`, and runs
//! the agent loop with a mock provider.
//!
//! Real hosts would use `gasket_core::extension::discover_plugins` +
//! `load_plugin` to load `.so`/`.dylib` files instead — same effect, different
//! loading mechanism.

#[path = "plugins/hello.rs"]
mod hello;
#[path = "plugins/permission_gate.rs"]
mod permission_gate;
#[path = "plugins/todo_list.rs"]
mod todo_list;

use std::sync::Arc;

use futures_util::Stream;
use gasket_core::extension::ExtensionApiImpl;
use gasket_core::{
    AgentContext, AgentLoopConfig, AgentMessage, ContentBlock, ModelSpec, ProviderApi, StreamChunk,
    StreamFn, ThinkingLevel, UserMessage,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let mut api = ExtensionApiImpl::new();

    // Plugins populate the api: tools + hook handlers.
    hello::register(&mut api);
    todo_list::register(&mut api);
    permission_gate::register(&mut api);

    // The tools plugins registered go into the context; the hook chain goes
    // into the config.
    let context = AgentContext {
        system_prompt: "You are a helpful assistant.".into(),
        messages: vec![],
        tools: std::mem::take(&mut api.tools),
        cwd: std::env::current_dir()?,
        env: std::env::vars().collect(),
        session_id: "demo".into(),
    };

    let user_msg = AgentMessage::User(UserMessage {
        content: vec![ContentBlock::text("Say hello to Ada.")],
        timestamp: gasket_core::now(),
    });

    let config = AgentLoopConfig {
        model: ModelSpec {
            id: "mock".into(),
            api: ProviderApi::OpenAiCompat,
            max_tokens: 256,
            supports_thinking: false,
        },
        thinking_level: ThinkingLevel::Off,
        max_turns: 5,
        max_tool_calls_per_turn: 5,
        signal: None,
        stream_fn: Arc::new(MockThatCallsHello),
        // The before/after hooks plugins registered are now live.
        hooks: Some(Arc::new(api)),
        retry: gasket_core::RetryPolicy::default(),
    };

    let msgs = gasket_core::agent_loop(vec![user_msg], context, config).await?;

    // Print whatever the (mocked) assistant produced.
    for m in &msgs {
        if let AgentMessage::Assistant(a) = m {
            for b in &a.content {
                if let ContentBlock::Text { text } = b {
                    println!("{text}");
                }
            }
        }
        if let AgentMessage::ToolResult(tr) = m {
            for b in &tr.content {
                if let ContentBlock::Text { text } = b {
                    println!("[tool {}] {text}", tr.tool_name);
                }
            }
        }
    }
    Ok(())
}

/// A mock provider that emits a single `hello` tool call, then a summary.
struct MockThatCallsHello;
impl StreamFn for MockThatCallsHello {
    fn stream(
        &self,
        _model: &ModelSpec,
        _messages: &[AgentMessage],
        _system: &str,
        _tools: &[gasket_core::ToolDefinition],
        _signal: Option<Arc<std::sync::atomic::AtomicBool>>,
    ) -> std::pin::Pin<Box<dyn Stream<Item = StreamChunk> + Send>> {
        Box::pin(futures_util::stream::iter(vec![
            StreamChunk::ToolCallDelta {
                id: "call_1".into(),
                name: Some("hello".into()),
                args_delta: r#"{"name":"Ada"}"#.into(),
            },
            StreamChunk::Done,
        ]))
    }
}
