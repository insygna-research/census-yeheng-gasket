//! Example host: link `gasket-ext` in-process and run one turn (mock provider).
//!
//! ```bash
//! cargo run -p gasket-core --example plugins
//! ```

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
    gasket_ext::register_all(&mut api);

    let context = AgentContext {
        system_prompt: "You are a helpful assistant.".into(),
        messages: vec![],
        tools: std::mem::take(&mut api.tools),
        cwd: std::env::current_dir()?,
        env: std::env::vars().collect(),
        session_id: "demo".into(),
        spawner: None,
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
        hooks: Some(Arc::new(api)),
        retry: gasket_core::RetryPolicy::default(),
    };

    let msgs = gasket_core::agent_loop(vec![user_msg], context, config).await?;

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
