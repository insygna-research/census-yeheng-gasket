//! `ExtensionApi` trait + `ExtensionApiImpl` — how extension crates register.
//!
//! Host/cli composition root calls `ext::register(&mut api)` for each linked
//! extension crate. See `docs/plugin-tutorial.md`.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use crate::types::event::AgentEvent;
use crate::types::message::{AgentMessage, ToolResultMessage};
use crate::types::tool::{ToolCallVerdict, ToolDefinition};

/// Per-invocation context handed to event/hook handlers.
#[derive(Debug, Clone)]
pub struct ExtensionContext {
    pub session_id: String,
    pub cwd: PathBuf,
    pub signal: Arc<AtomicBool>,
}

/// A `before_tool_call` hook handler. Returns a verdict controlling flow.
pub trait BeforeToolCallHandler: Send + Sync {
    fn call(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        args: &serde_json::Value,
        ctx: &ExtensionContext,
    ) -> ToolCallVerdict;
}

/// An `after_tool_call` hook handler. Returns a replacement result, or None to
/// leave it unchanged.
pub trait AfterToolCallHandler: Send + Sync {
    fn call(
        &self,
        tool_call_id: &str,
        result: &ToolResultMessage,
        ctx: &ExtensionContext,
    ) -> Option<ToolResultMessage>;
}

/// A single-direction event handler.
pub type EventHandler = Arc<dyn Fn(&AgentEvent, &ExtensionContext) + Send + Sync>;

/// The surface an extension crate uses to register capabilities with the agent.
///
/// **Events vs hooks are type-separated**: `register_event_handler` handlers
/// return nothing (pure observation); `register_before_tool_call` /
/// `register_after_tool_call` handlers return a verdict / replacement that
/// controls agent flow. The two cannot be confused at the type level.
pub trait ExtensionApi: Send + Sync {
    /// Register a tool the LLM may call.
    fn register_tool(&mut self, tool: ToolDefinition);

    /// Register a `before_tool_call` hook (block / modify args).
    fn register_before_tool_call(&mut self, handler: Box<dyn BeforeToolCallHandler>);

    /// Register an `after_tool_call` hook (replace result).
    fn register_after_tool_call(&mut self, handler: Box<dyn AfterToolCallHandler>);

    /// Subscribe to single-direction events (observation only).
    fn register_event_handler(&mut self, handler: EventHandler);

    /// Queue a message for the host to inject into the session.
    fn send_message(&mut self, msg: AgentMessage);

    /// Read-only snapshot of current session messages (host may fill this).
    fn current_messages(&self) -> &[AgentMessage];
}

/// Concrete registry holding everything extension crates have registered.
#[derive(Default)]
pub struct ExtensionApiImpl {
    pub tools: Vec<ToolDefinition>,
    pub before_hooks: Vec<Box<dyn BeforeToolCallHandler>>,
    pub after_hooks: Vec<Box<dyn AfterToolCallHandler>>,
    pub event_handlers: Vec<EventHandler>,
    pub outbound: Vec<AgentMessage>,
    pub messages: Vec<AgentMessage>,
}

impl ExtensionApiImpl {
    pub fn new() -> Self {
        Self::default()
    }

    /// Run all `before_tool_call` hooks in registration order, combining their
    /// verdicts: the first `Block` wins; otherwise the last `Modify` wins;
    /// `Allow` is the default.
    pub fn before_tool_call(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        args: &serde_json::Value,
        ctx: &ExtensionContext,
    ) -> ToolCallVerdict {
        let mut verdict = ToolCallVerdict::Allow;
        for h in &self.before_hooks {
            match h.call(tool_call_id, tool_name, args, ctx) {
                v @ ToolCallVerdict::Block(_) => return v,
                v @ ToolCallVerdict::Modify(_) => verdict = v,
                ToolCallVerdict::Allow => {}
            }
        }
        verdict
    }

    /// Run all `after_tool_call` hooks in order, each may replace the result.
    pub fn after_tool_call(
        &self,
        tool_call_id: &str,
        result: &ToolResultMessage,
        ctx: &ExtensionContext,
    ) -> ToolResultMessage {
        let mut current = result.clone();
        for h in &self.after_hooks {
            if let Some(replaced) = h.call(tool_call_id, &current, ctx) {
                current = replaced;
            }
        }
        current
    }

    /// Dispatch an event to all subscribers.
    pub fn emit(&self, event: &AgentEvent, ctx: &ExtensionContext) {
        for h in &self.event_handlers {
            h(event, ctx);
        }
    }

    /// Drain messages extensions asked to send.
    pub fn drain_outbound(&mut self) -> Vec<AgentMessage> {
        std::mem::take(&mut self.outbound)
    }

    /// Build a placeholder `ExtensionContext` for hook invocation. Real
    /// session id/cwd are owned by the host's `AgentContext`; if a hook needs
    /// them, wire them through here in a later iteration.
    fn placeholder_ctx() -> ExtensionContext {
        ExtensionContext {
            session_id: String::new(),
            cwd: PathBuf::new(),
            signal: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl crate::types::tool::HookChain for ExtensionApiImpl {
    fn before_tool_call(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        args: &serde_json::Value,
    ) -> ToolCallVerdict {
        ExtensionApiImpl::before_tool_call(
            self,
            tool_call_id,
            tool_name,
            args,
            &Self::placeholder_ctx(),
        )
    }

    fn after_tool_call(&self, tool_call_id: &str, result: &ToolResultMessage) -> ToolResultMessage {
        ExtensionApiImpl::after_tool_call(self, tool_call_id, result, &Self::placeholder_ctx())
    }
}

impl ExtensionApi for ExtensionApiImpl {
    fn register_tool(&mut self, tool: ToolDefinition) {
        self.tools.push(tool);
    }

    fn register_before_tool_call(&mut self, handler: Box<dyn BeforeToolCallHandler>) {
        self.before_hooks.push(handler);
    }

    fn register_after_tool_call(&mut self, handler: Box<dyn AfterToolCallHandler>) {
        self.after_hooks.push(handler);
    }

    fn register_event_handler(&mut self, handler: EventHandler) {
        self.event_handlers.push(handler);
    }

    fn send_message(&mut self, msg: AgentMessage) {
        self.outbound.push(msg);
    }

    fn current_messages(&self) -> &[AgentMessage] {
        &self.messages
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::message::{ContentBlock, ToolResultMessage};

    fn ctx() -> ExtensionContext {
        ExtensionContext {
            session_id: "s".into(),
            cwd: ".".into(),
            signal: Arc::new(AtomicBool::new(false)),
        }
    }

    struct Blocker;
    impl BeforeToolCallHandler for Blocker {
        fn call(
            &self,
            _: &str,
            _: &str,
            _: &serde_json::Value,
            _: &ExtensionContext,
        ) -> ToolCallVerdict {
            ToolCallVerdict::Block("no".into())
        }
    }

    #[test]
    fn before_hook_block_wins() {
        let mut api = ExtensionApiImpl::new();
        api.register_before_tool_call(Box::new(Blocker));
        let v = api.before_tool_call("id", "bash", &serde_json::json!({}), &ctx());
        assert!(matches!(v, ToolCallVerdict::Block(_)));
    }

    #[test]
    fn after_hook_replaces_result() {
        struct Redactor;
        impl AfterToolCallHandler for Redactor {
            fn call(
                &self,
                _: &str,
                _: &ToolResultMessage,
                _: &ExtensionContext,
            ) -> Option<ToolResultMessage> {
                Some(ToolResultMessage {
                    tool_call_id: "id".into(),
                    tool_name: "x".into(),
                    content: vec![ContentBlock::text("[REDACTED]")],
                    is_error: false,
                    timestamp: 0,
                })
            }
        }
        let mut api = ExtensionApiImpl::new();
        api.register_after_tool_call(Box::new(Redactor));
        let orig = ToolResultMessage {
            tool_call_id: "id".into(),
            tool_name: "x".into(),
            content: vec![ContentBlock::text("secret")],
            is_error: false,
            timestamp: 0,
        };
        let out = api.after_tool_call("id", &orig, &ctx());
        match &out.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "[REDACTED]"),
            _ => panic!(),
        }
    }
}
