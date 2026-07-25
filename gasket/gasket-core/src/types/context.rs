//! `AgentContext` / `AgentLoopConfig` — what the agent sees and how it runs.
//!
//! See `gasket-refactor-plan.md` §3.3.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use crate::types::message::ModelId;
use crate::types::tool::ToolDefinition;

/// Everything the agent sees for one run. Deliberately has **no plugin-shared
/// state field** — plugin-private state lives in files under `ToolContext.state_dir`.
#[derive(Debug, Clone)]
pub struct AgentContext {
    pub system_prompt: String,
    pub messages: Vec<crate::types::message::AgentMessage>,
    pub tools: Vec<ToolDefinition>,
    pub cwd: PathBuf,
    pub env: HashMap<String, String>,
    pub session_id: String,
}

/// How a single agent loop invocation is configured.
#[derive(Clone)]
pub struct AgentLoopConfig {
    pub model: ModelSpec,
    pub thinking_level: ThinkingLevel,
    /// Hard ceiling on outer-loop turns. Default 50.
    pub max_turns: usize,
    /// Hard ceiling on tool calls executed within a single turn. Default 20.
    pub max_tool_calls_per_turn: usize,
    pub api_key: Option<String>,
    /// Cooperative abort: when set to true, the loop exits at the next safe point.
    pub signal: Option<Arc<AtomicBool>>,
    /// The LLM call entry point. Injected by the host so the loop is
    /// provider-agnostic and testable with a mock.
    pub stream_fn: Arc<dyn StreamFn>,
}

impl std::fmt::Debug for AgentLoopConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentLoopConfig")
            .field("model", &self.model.id)
            .field("thinking_level", &self.thinking_level)
            .field("max_turns", &self.max_turns)
            .field("max_tool_calls_per_turn", &self.max_tool_calls_per_turn)
            .finish_non_exhaustive()
    }
}

/// Specification of a model the host wants to talk to.
#[derive(Debug, Clone)]
pub struct ModelSpec {
    pub id: ModelId,
    pub api: ProviderApi,
    pub max_tokens: usize,
    pub supports_thinking: bool,
}

/// Which wire protocol the provider speaks. Determines `convert_to_llm` shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderApi {
    /// OpenAI / DeepSeek / 智谱 / xAI / Groq / Ollama / vLLM / etc.
    OpenAiCompat,
    /// Anthropic native messages API.
    Anthropic,
}

/// Extended-thinking level. Off for providers/models that don't support it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ThinkingLevel {
    #[default]
    Off,
    Low,
    Medium,
    High,
}

/// A provider-agnostic stream chunk produced by `StreamFn`.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamChunk {
    TextDelta(String),
    ToolCallDelta {
        id: String,
        name: Option<String>,
        args_delta: String,
    },
    ThinkingDelta(String),
    Usage {
        input: u64,
        output: u64,
    },
    Done,
    Error(String),
}

/// The LLM call entry point, injected into `AgentLoopConfig`.
///
/// Returns a boxed async iterator of [`StreamChunk`]. Providers implement this;
/// tests inject a mock that yields a canned chunk sequence.
pub trait StreamFn: Send + Sync {
    fn stream(
        &self,
        model: &ModelSpec,
        messages: &[crate::types::message::AgentMessage],
        system_prompt: &str,
        tools: &[ToolDefinition],
        signal: Option<Arc<AtomicBool>>,
    ) -> std::pin::Pin<
        Box<
            dyn futures_util::Stream<Item = StreamChunk> + Send,
        >,
    >;
}
