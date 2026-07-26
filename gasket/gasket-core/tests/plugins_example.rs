//! Integration test: the example plugins work end-to-end against the agent
//! loop (mock provider).
//!
//! This is the "proof the examples are correct" layer: it registers the
//! example plugins' tools/hooks into a real `ExtensionApiImpl`, wires the hook
//! chain into `AgentLoopConfig`, and asserts the agent loop produces the
//! expected tool results.
//!
//! The example plugin source lives in `examples/plugins/` (not in the lib
//! path), so the tool/handler definitions are reconstructed here to the same
//! spec. `agent_loop::tests::before_hook_blocks_tool` already covers the
//! permission_gate path against the real loop.

use std::sync::Arc;

use futures_util::{stream, Stream};
use gasket_core::extension::BeforeToolCallHandler;
use gasket_core::{
    AgentContext, AgentEvent, AgentLoopConfig, AgentMessage, ContentBlock, ExtensionApi,
    ExtensionApiImpl, ExtensionContext, ModelSpec, ProviderApi, StreamChunk, StreamFn,
    ThinkingLevel, ToolCallVerdict, ToolDefinition, ToolResult,
};

// ── a mock provider that calls a named tool once, then ends ───────────────
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
                id: "call_1".into(),
                name: Some(tool),
                args_delta: args_str,
            },
            StreamChunk::Done,
        ]))
    }
}

fn hello_config(tools: Vec<ToolDefinition>) -> (AgentContext, AgentLoopConfig) {
    let ctx = AgentContext {
        system_prompt: "".into(),
        messages: vec![],
        tools,
        cwd: ".".into(),
        env: Default::default(),
        session_id: "t".into(),
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
        retry: gasket_core::RetryPolicy::default(),
    };
    (ctx, cfg)
}

#[tokio::test]
async fn hello_plugin_greets() {
    let mut api = ExtensionApiImpl::new();
    // Same tool the hello example registers.
    api.register_tool(ToolDefinition {
        name: "hello".into(),
        label: "Hello".into(),
        description: "greet".into(),
        parameters: serde_json::json!({"type": "object"}),
        execute: Arc::new(|c| {
            Box::pin(async move {
                let name = c.args["name"].as_str().unwrap_or("world");
                Ok(ToolResult::text(format!("Hello, {}!", name)))
            })
        }),
    });
    let (ctx, cfg) = hello_config(std::mem::take(&mut api.tools));
    let msgs = gasket_core::agent_loop(vec![], ctx, cfg).await.unwrap();
    let greeted = msgs.iter().any(|m| {
        matches!(m, AgentMessage::ToolResult(tr) if tr.content.iter()
            .any(|b| matches!(b, ContentBlock::Text { text } if text == "Hello, Ada!")))
    });
    assert!(greeted, "hello tool should have greeted Ada");
}

#[tokio::test]
async fn permission_gate_blocks_bash() {
    // A gate identical to the permission_gate example.
    struct Gate;
    impl BeforeToolCallHandler for Gate {
        fn call(
            &self,
            _id: &str,
            tool: &str,
            args: &serde_json::Value,
            _ctx: &ExtensionContext,
        ) -> ToolCallVerdict {
            if tool == "bash" && args["command"].as_str().unwrap_or("").contains("rm -rf") {
                ToolCallVerdict::Block("refused".into())
            } else {
                ToolCallVerdict::Allow
            }
        }
    }

    let mut api = ExtensionApiImpl::new();
    api.register_before_tool_call(Box::new(Gate));

    // A bash tool that would run if not blocked.
    let bash = ToolDefinition {
        name: "bash".into(),
        label: "Bash".into(),
        description: "shell".into(),
        parameters: serde_json::json!({"type": "object"}),
        execute: Arc::new(|_| Box::pin(async { Ok(ToolResult::text("RAN")) })),
    };

    let agent_ctx = AgentContext {
        system_prompt: "".into(),
        messages: vec![],
        tools: vec![bash],
        cwd: ".".into(),
        env: Default::default(),
        session_id: "t".into(),
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
        hooks: Some(Arc::new(api) as Arc<dyn gasket_core::types::tool::HookChain>),
        retry: gasket_core::RetryPolicy::default(),
    };

    let mut saw_block = false;
    let msgs = gasket_core::run_agent_loop(vec![], agent_ctx, cfg, |ev| {
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
    // And the dangerous command must NOT have produced a "RAN" result.
    let ran = msgs.iter().any(|m| {
        matches!(m, AgentMessage::ToolResult(tr) if tr.content.iter()
            .any(|b| matches!(b, ContentBlock::Text { text } if text == "RAN")))
    });
    assert!(!ran, "the bash tool must not have executed");
}
