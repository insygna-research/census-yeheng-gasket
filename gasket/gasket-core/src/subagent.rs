//! Subagent orchestration types: event protocol + spawner trait.
//!
//! The host injects a `SubagentSpawner` into `ToolContext`. The
//! `spawn_subagents` tool calls it to fan out parallel sub-agent loops.
//! Events are emitted as `SubagentEvent`, which the gateway maps 1:1 to the
//! frontend's `subagent_*` WS protocol.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// One task for a sub-agent to work on.
#[derive(Debug, Clone)]
pub struct SubagentSpawn {
    pub task: String,
}

/// Result of a completed sub-agent.
#[derive(Debug, Clone)]
pub struct SubagentResult {
    pub id: String,
    pub task: String,
    pub index: usize,
    pub summary: String,
    pub tool_count: usize,
    pub error: Option<String>,
}

/// Real-time events from sub-agent execution. Maps 1:1 to the frontend's
/// `subagent_*` WS messages (see `web/src/types/index.ts`).
#[derive(Debug, Clone)]
pub enum SubagentEvent {
    AllStarted { count: usize },
    Started { id: String, task: String, index: usize },
    Thinking { id: String, content: String },
    Content { id: String, content: String },
    ToolStart { id: String, name: String, arguments: Option<String> },
    ToolEnd { id: String, tool_id: Option<String>, name: String, output: Option<String> },
    Completed { id: String, index: usize, summary: String, tool_count: usize },
    Error { id: String, index: usize, error: String },
    Synthesizing,
}

/// Trait injected into `ToolContext` by the host. The `spawn_subagents` tool
/// calls this to fan out parallel sub-agent loops. The host implementation
/// builds per-task contexts (same tools/stream_fn/hooks, fewer max_turns) and
/// runs `run_agent_loop` concurrently.
pub trait SubagentSpawner: Send + Sync {
    fn spawn(
        &self,
        tasks: Vec<SubagentSpawn>,
        emit: Arc<dyn Fn(SubagentEvent) + Send + Sync>,
    ) -> Pin<Box<dyn Future<Output = Vec<SubagentResult>> + Send>>;
}

/// A no-op spawner used when no host is wired (tests, bare agent_loop). Always
/// returns an error result explaining subagents are unavailable.
pub struct NoopSubagentSpawner;

impl SubagentSpawner for NoopSubagentSpawner {
    fn spawn(
        &self,
        tasks: Vec<SubagentSpawn>,
        _emit: Arc<dyn Fn(SubagentEvent) + Send + Sync>,
    ) -> Pin<Box<dyn Future<Output = Vec<SubagentResult>> + Send>> {
        Box::pin(async move {
            tasks
                .into_iter()
                .enumerate()
                .map(|(i, t)| SubagentResult {
                    id: format!("noop-{i}"),
                    task: t.task,
                    index: i + 1,
                    summary: String::new(),
                    tool_count: 0,
                    error: Some("subagents not available in this context".into()),
                })
                .collect()
        })
    }
}
