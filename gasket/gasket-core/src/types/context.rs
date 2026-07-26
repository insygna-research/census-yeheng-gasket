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
    /// Cooperative abort: when set to true, the loop exits at the next safe point.
    pub signal: Option<Arc<AtomicBool>>,
    /// The LLM call entry point. Injected by the host so the loop is
    /// provider-agnostic and testable with a mock.
    pub stream_fn: Arc<dyn StreamFn>,
    /// Optional hook chain consulted around each tool call (block / modify /
    /// redact). `None` = no hooks (default — used by tests and `agent_loop`).
    pub hooks: Option<Arc<dyn crate::types::tool::HookChain>>,
    /// Retry policy for LLM calls that fail before any content is streamed
    /// (connection errors, non-2xx). Mid-stream failures are surfaced, not
    /// retried, to avoid duplicating partial output already emitted.
    pub retry: RetryPolicy,
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

/// Retry policy for a single LLM call. Backoff is exponential:
/// `initial_delay_ms * 2^(attempt-1)`, capped at `max_delay_ms`. No jitter.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Max retries after the initial attempt (0 = no retry).
    pub max_retries: usize,
    /// Delay before the first retry, in milliseconds.
    pub initial_delay_ms: u64,
    /// Upper bound on backoff delay, in milliseconds.
    pub max_delay_ms: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 2,
            initial_delay_ms: 500,
            max_delay_ms: 8_000,
        }
    }
}

impl RetryPolicy {
    /// No retries - for tests that want deterministic, fast failure.
    pub fn off() -> Self {
        Self {
            max_retries: 0,
            initial_delay_ms: 0,
            max_delay_ms: 0,
        }
    }
}

/// Loop tunables loadable from the environment (all optional, with defaults).
///
/// Recognized env vars:
/// - `GASKET_MAX_TURNS`        - outer-loop turn ceiling (default 50)
/// - `GASKET_MAX_TOOL_CALLS`   - per-turn tool-call ceiling (default 20)
/// - `GASKET_MAX_TOKENS`       - model output token cap (default 4096)
/// - `GASKET_THINKING`         - `off`|`low`|`medium`|`high` (default off)
/// - `GASKET_RETRY_MAX`        - max LLM-call retries (default 2)
/// - `GASKET_RETRY_INITIAL_MS` - first retry backoff ms (default 500)
/// - `GASKET_RETRY_MAX_MS`     - backoff cap ms (default 8000)
///
/// Malformed values fall back to the default silently.
#[derive(Debug, Clone)]
pub struct AgentTunables {
    pub max_turns: usize,
    pub max_tool_calls_per_turn: usize,
    pub max_tokens: usize,
    pub thinking_level: ThinkingLevel,
    pub retry: RetryPolicy,
}

impl AgentTunables {
    /// Read tunables from the process environment.
    pub fn from_env() -> Self {
        Self::from_env_with(&|k: &str| std::env::var(k))
    }

    /// Same as [`from_env`](Self::from_env) but with an injectable lookup -
    /// used by tests to avoid mutating process env.
    pub fn from_env_with(lookup: &dyn Fn(&str) -> Result<String, std::env::VarError>) -> Self {
        let default_retry = RetryPolicy::default();
        Self {
            max_turns: env_parse(lookup, "GASKET_MAX_TURNS", 50),
            max_tool_calls_per_turn: env_parse(lookup, "GASKET_MAX_TOOL_CALLS", 20),
            max_tokens: env_parse(lookup, "GASKET_MAX_TOKENS", 4096),
            thinking_level: match lookup("GASKET_THINKING").ok().as_deref() {
                Some("low") => ThinkingLevel::Low,
                Some("medium") => ThinkingLevel::Medium,
                Some("high") => ThinkingLevel::High,
                _ => ThinkingLevel::Off,
            },
            retry: RetryPolicy {
                max_retries: env_parse(lookup, "GASKET_RETRY_MAX", default_retry.max_retries),
                initial_delay_ms: env_parse(
                    lookup,
                    "GASKET_RETRY_INITIAL_MS",
                    default_retry.initial_delay_ms,
                ),
                max_delay_ms: env_parse(lookup, "GASKET_RETRY_MAX_MS", default_retry.max_delay_ms),
            },
        }
    }
}

/// Parse `key` from the lookup as `T`, falling back to `default` on miss or
/// parse failure.
fn env_parse<T: std::str::FromStr>(
    lookup: &dyn Fn(&str) -> Result<String, std::env::VarError>,
    key: &str,
    default: T,
) -> T {
    lookup(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
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
    ) -> std::pin::Pin<Box<dyn futures_util::Stream<Item = StreamChunk> + Send>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn fake_env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Result<String, std::env::VarError> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| map.get(k).cloned().ok_or(std::env::VarError::NotPresent)
    }

    #[test]
    fn tunables_default_when_unset() {
        let t = AgentTunables::from_env_with(&fake_env(&[]));
        assert_eq!(t.max_turns, 50);
        assert_eq!(t.max_tool_calls_per_turn, 20);
        assert_eq!(t.max_tokens, 4096);
        assert_eq!(t.thinking_level, ThinkingLevel::Off);
        assert_eq!(t.retry.max_retries, 2);
    }

    #[test]
    fn tunables_parse_values() {
        let t = AgentTunables::from_env_with(&fake_env(&[
            ("GASKET_MAX_TURNS", "5"),
            ("GASKET_MAX_TOOL_CALLS", "8"),
            ("GASKET_MAX_TOKENS", "123"),
            ("GASKET_THINKING", "medium"),
            ("GASKET_RETRY_MAX", "3"),
            ("GASKET_RETRY_INITIAL_MS", "100"),
            ("GASKET_RETRY_MAX_MS", "2000"),
        ]));
        assert_eq!(t.max_turns, 5);
        assert_eq!(t.max_tool_calls_per_turn, 8);
        assert_eq!(t.max_tokens, 123);
        assert_eq!(t.thinking_level, ThinkingLevel::Medium);
        assert_eq!(t.retry.max_retries, 3);
        assert_eq!(t.retry.initial_delay_ms, 100);
        assert_eq!(t.retry.max_delay_ms, 2000);
    }

    #[test]
    fn tunables_malformed_falls_back() {
        let t = AgentTunables::from_env_with(&fake_env(&[
            ("GASKET_MAX_TURNS", "not-a-number"),
            ("GASKET_THINKING", "bogus"),
        ]));
        assert_eq!(t.max_turns, 50);
        assert_eq!(t.thinking_level, ThinkingLevel::Off);
    }
}
