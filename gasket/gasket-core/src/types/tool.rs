//! `ToolDefinition` / `ToolFn` / `ToolContext` / `ToolResult` + hook verdicts.
//!
//! See `gasket-refactor-plan.md` §3.4 / §3.5.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use crate::error::ToolError;
use crate::types::message::{ContentBlock, ToolResultMessage};

/// A tool registered with the agent. `parameters` is a JSON Schema; the host
/// validates args before calling `execute`.
#[derive(Clone)]
pub struct ToolDefinition {
    pub name: String,
    pub label: String,
    pub description: String,
    pub parameters: serde_json::Value,
    pub execute: ToolFn,
}

impl std::fmt::Debug for ToolDefinition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolDefinition")
            .field("name", &self.name)
            .field("label", &self.label)
            .finish_non_exhaustive()
    }
}

/// The signature every tool's `execute` closure must match.
///
/// No `on_update` callback (V0.1 omits streaming tool progress — no consumer
/// exists among the 5 built-in tools).
pub type ToolFn = Arc<
    dyn Fn(ToolCallCtx) -> Pin<Box<dyn Future<Output = Result<ToolResult, ToolError>> + Send>>
        + Send
        + Sync,
>;

/// Arguments handed to a tool invocation.
#[derive(Debug, Clone)]
pub struct ToolCallCtx {
    pub tool_call_id: String,
    pub args: serde_json::Value,
    pub signal: Arc<AtomicBool>,
    pub ctx: ToolContext,
}

impl ToolCallCtx {
    /// True if the caller has requested this invocation be cancelled. Tools
    /// should check this at entry and inside long loops, returning promptly
    /// (e.g. an "aborted" error) when set.
    pub fn aborted(&self) -> bool {
        self.signal.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Context passed into a tool. `state_dir` is this plugin's **private** state
/// directory (`~/.gasket/tool_state/{plugin}/`); the tool reads/writes its own
/// files there.
#[derive(Debug, Clone)]
pub struct ToolContext {
    pub cwd: PathBuf,
    pub env: HashMap<String, String>,
    pub session_id: String,
    pub state_dir: PathBuf,
}

/// A tool's result. `details` is plugin-private (the agent never reads it).
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub content: Vec<ContentBlock>,
    pub details: serde_json::Value,
    pub is_error: bool,
}

impl ToolResult {
    pub fn text(s: impl Into<String>) -> Self {
        Self {
            content: vec![ContentBlock::text(s)],
            details: serde_json::Value::Null,
            is_error: false,
        }
    }

    pub fn error(s: impl Into<String>) -> Self {
        Self {
            content: vec![ContentBlock::text(s)],
            details: serde_json::Value::Null,
            is_error: true,
        }
    }
}

/// Verdict returned by a `before_tool_call` hook — controls whether/how a tool
/// call proceeds.
#[derive(Debug, Clone)]
pub enum ToolCallVerdict {
    /// Let the call through unchanged.
    Allow,
    /// Refuse the call; `reason` becomes the ToolResult sent back to the LLM.
    Block(String),
    /// Replace the args, then execute.
    Modify(serde_json::Value),
}

/// Object-safe hook chain the agent loop consults around each tool call.
///
/// Defined in `types` (not `extension`) so `AgentLoopConfig` can hold an
/// `Option<Arc<dyn HookChain>>` without a circular dependency. The concrete
/// implementation is `ExtensionApiImpl`; `None` means "no hooks installed"
/// (the default — used by tests and the bare `agent_loop` helper).
pub trait HookChain: Send + Sync {
    /// Consult all `before_tool_call` handlers. First `Block` wins; otherwise
    /// the last `Modify` wins; default `Allow`.
    fn before_tool_call(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        args: &serde_json::Value,
    ) -> ToolCallVerdict;

    /// Consult all `after_tool_call` handlers, each may replace the result.
    fn after_tool_call(&self, tool_call_id: &str, result: &ToolResultMessage) -> ToolResultMessage;
}
