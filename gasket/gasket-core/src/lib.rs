//! gasket-core — a pi-style pluggable agent core.
//!
//! A single `agent_loop` function plus an `ExtensionApi` trait. Everything else
//! (CLI / TUI / channels / sandbox / wiki) is a host or plugin concern.
//!
//! See `gasket-refactor-plan.md` §3-§9 for the design.
//!
//! Stage 3a: skeleton. Module-level re-exports are added as each module is
//! implemented in stages 3b-3g.

pub mod agent_loop;
pub mod error;
pub mod extension;
pub mod providers;
pub mod storage;
pub mod tools;
pub mod types;

pub use agent_loop::{agent_loop, run_agent_loop};
pub use error::{AgentError, ToolError};
pub use extension::{ExtensionApi, ExtensionApiImpl, ExtensionContext, Plugin};
pub use providers::{AnthropicProvider, ConfigError, OpenAiCompat, ProviderConfig};
pub use storage::JsonlStorage;
pub use tools::built_in_tools;
pub use types::context::{
    AgentContext, AgentLoopConfig, ModelSpec, ProviderApi, StreamChunk, StreamFn, ThinkingLevel,
};
pub use types::event::{AgentEvent, ContentDelta};
pub use types::message::{
    AgentMessage, AssistantMessage, ContentBlock, StopReason, ToolResultMessage, UserMessage,
};
pub use types::tool::{
    ToolCallCtx, ToolCallVerdict, ToolContext, ToolDefinition, ToolFn, ToolResult,
};

/// Current monotonically-increasing time in milliseconds since UNIX epoch.
///
/// Used for message timestamps.
pub fn now() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
