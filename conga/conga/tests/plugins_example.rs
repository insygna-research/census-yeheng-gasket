//! Integration: `conga-ext` register shapes against the agent loop (mock).

use std::sync::Arc;

use conga::{
    AgentContext, AgentEvent, AgentLoopConfig, AgentMessage, ContentBlock, ExtensionApiImpl,
    ModelSpec, ProviderApi, StreamChunk, StreamFn, ThinkingLevel, ToolDefinition,
};
use futures_util::{stream, Stream};

struct CallToolOnce {
    tool: String,
    args: serde_json::Value,
}
impl StreamFn for CallToolOnce {
    fn stream(
        &self,
        _model: &ModelSpec,
        _messages: &[AgentMessage],
        _system: &str,
        _tools: &[ToolDefinition],
        _signal: Option<Arc<std::sync::atomic::AtomicBool>>,
    ) -> std::pin::Pin<Box<dyn Stream<Item = StreamChunk> + Send>> {
        let tool = self.tool.clone();
        let args_str = self.args.to_string();
        Box::pin(stream::iter(vec![
            StreamChunk::ToolCallDelta {
                index: None,
                id: "call_1".into(),
                name: Some(tool),
                args_delta: args_str,
            },
            StreamChunk::Done,
        ]))
    }
}

#[tokio::test]
async fn hello_extension_greets() {
    let mut api = ExtensionApiImpl::new();
    conga_ext::hello::register(&mut api);
    let tools = std::mem::take(&mut api.tools);

    let ctx = AgentContext {
        system_prompt: "".into(),
        messages: vec![],
        tools,
        cwd: ".".into(),
        env: Default::default(),
        session_id: "t".into(),
        spawner: None,
    };
    let cfg = AgentLoopConfig {
        model: ModelSpec {
            id: "m".into(),
            api: ProviderApi::OpenAiCompat,
            max_tokens: 64,
            supports_thinking: false,
        },
        thinking_level: ThinkingLevel::Off,
        max_turns: 1,
        max_tool_calls_per_turn: 5,
        signal: None,
        stream_fn: Arc::new(CallToolOnce {
            tool: "hello".into(),
            args: serde_json::json!({"name": "Ada"}),
        }),
        hooks: None,
        retry: conga::RetryPolicy::default(),
        persist: None,
        transform_context: None,
    };

    let msgs = conga::agent_loop(vec![], ctx, cfg).await.unwrap();
    let greeted = msgs.iter().any(|m| {
        matches!(m, AgentMessage::ToolResult(tr) if tr.content.iter()
            .any(|b| matches!(b, ContentBlock::Text { text } if text == "Hello, Ada!")))
    });
    assert!(greeted, "hello tool should have greeted Ada");
}

#[tokio::test]
async fn permission_gate_blocks_bash() {
    let mut api = ExtensionApiImpl::new();
    conga_ext::permission_gate::register(&mut api);

    let bash = ToolDefinition {
        name: "bash".into(),
        label: "Bash".into(),
        description: "shell".into(),
        parameters: serde_json::json!({"type": "object"}),
        risk: conga::RiskLevel::High,
        execute: Arc::new(|_| Box::pin(async { Ok(conga::ToolResult::text("RAN")) })),
    };

    let agent_ctx = AgentContext {
        system_prompt: "".into(),
        messages: vec![],
        tools: vec![bash],
        cwd: ".".into(),
        env: Default::default(),
        session_id: "t".into(),
        spawner: None,
    };
    let cfg = AgentLoopConfig {
        model: ModelSpec {
            id: "m".into(),
            api: ProviderApi::OpenAiCompat,
            max_tokens: 64,
            supports_thinking: false,
        },
        thinking_level: ThinkingLevel::Off,
        max_turns: 1,
        max_tool_calls_per_turn: 5,
        signal: None,
        stream_fn: Arc::new(CallToolOnce {
            tool: "bash".into(),
            args: serde_json::json!({"command": "rm -rf /tmp/x"}),
        }),
        hooks: Some(Arc::new(api) as Arc<dyn conga::types::tool::HookChain>),
        retry: conga::RetryPolicy::default(),
        persist: None,
        transform_context: None,
    };

    let mut saw_block = false;
    let msgs = conga::run_agent_loop(vec![], agent_ctx, cfg, |ev| {
        if let AgentEvent::ToolExecutionEnd {
            result, is_error, ..
        } = ev
        {
            if is_error && result.tool_name == "bash" {
                saw_block = true;
            }
        }
    })
    .await
    .unwrap();

    assert!(saw_block, "bash should have been blocked");
    let ran = msgs.iter().any(|m| {
        matches!(m, AgentMessage::ToolResult(tr) if tr.content.iter()
            .any(|b| matches!(b, ContentBlock::Text { text } if text == "RAN")))
    });
    assert!(!ran, "the bash tool must not have executed");
}
