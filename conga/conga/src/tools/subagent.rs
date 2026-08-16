//! `spawn_subagents` tool — fan out parallel sub-agent loops.

use std::sync::Arc;

use crate::subagent::{SubagentSpawn, SubagentSpawner};
use crate::types::tool::{RiskLevel, ToolCallCtx, ToolDefinition, ToolResult};
use crate::ContentBlock;

pub fn tool() -> ToolDefinition {
    ToolDefinition {
        name: "spawn_subagents".into(),
        label: "Spawn Subagents".into(),
        description: "Spawn parallel sub-agents to work on independent tasks concurrently. Each sub-agent runs its own agent loop with the standard built-in tools. Use for divide-and-conquer: searching multiple areas, writing + testing + reviewing in parallel. Max 5 tasks.".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "tasks": {
                    "type": "array",
                    "items": { "type": "object", "properties": { "task": { "type": "string" } }, "required": ["task"] },
                    "minItems": 1,
                    "maxItems": 5,
                    "description": "Each task gets its own sub-agent. Max 5."
                }
            },
            "required": ["tasks"]
        }),
        risk: RiskLevel::Medium,
        execute: Arc::new(|ctx| Box::pin(execute(ctx))),
    }
}

async fn execute(ctx: ToolCallCtx) -> Result<ToolResult, crate::error::ToolError> {
    if ctx.aborted() {
        return Ok(ToolResult::error("aborted".to_string()));
    }

    let tasks = ctx.args["tasks"]
        .as_array()
        .ok_or_else(|| crate::error::ToolError::Message("tasks array is required".into()))?;

    let mut spawns: Vec<SubagentSpawn> = tasks
        .iter()
        .filter_map(|t| {
            t["task"].as_str().map(|s| SubagentSpawn {
                task: s.to_string(),
            })
        })
        .collect();

    if spawns.is_empty() {
        return Ok(ToolResult::error(
            "no valid tasks provided (each needs a 'task' string)".to_string(),
        ));
    }

    // Enforce the schema's maxItems: 5, regardless of what the LLM sent.
    // Dropped tasks are reported so the model isn't silently truncated.
    let dropped = spawns.len().saturating_sub(5);
    spawns.truncate(5);

    let spawner: Arc<dyn SubagentSpawner> = match &ctx.ctx.spawner {
        Some(s) => Arc::clone(s),
        None => Arc::new(crate::subagent::NoopSubagentSpawner),
    };

    let results = spawner.spawn(spawns).await;

    let mut summary = results
        .iter()
        .map(|r| {
            if let Some(err) = &r.error {
                format!("Subagent {} ({}): ERROR - {}", r.index, r.task, err)
            } else {
                format!("Subagent {} ({}): {}", r.index, r.task, r.summary)
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    if dropped > 0 {
        summary.push_str(&format!(
            "\n\n(Note: {dropped} additional task(s) beyond the max of 5 were dropped.)"
        ));
    }

    Ok(ToolResult {
        content: vec![ContentBlock::text(summary)],
        details: serde_json::json!({
            "subagent_count": results.len(),
            "completed": results.iter().filter(|r| r.error.is_none()).count(),
            "errors": results.iter().filter(|r| r.error.is_some()).count(),
            "dropped": dropped,
        }),
        is_error: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use crate::subagent::{SubagentResult, SubagentSpawner};
    use crate::types::tool::ToolContext;

    /// Records how many tasks it received; returns one canned result each.
    struct CountingSpawner(Arc<AtomicUsize>);

    impl SubagentSpawner for CountingSpawner {
        fn spawn(
            &self,
            tasks: Vec<SubagentSpawn>,
        ) -> Pin<Box<dyn Future<Output = Vec<SubagentResult>> + Send>> {
            let count = Arc::clone(&self.0);
            Box::pin(async move {
                count.fetch_add(tasks.len(), Ordering::SeqCst);
                tasks
                    .into_iter()
                    .enumerate()
                    .map(|(i, t)| SubagentResult {
                        id: format!("r-{i}"),
                        task: t.task,
                        index: i + 1,
                        summary: "ok".into(),
                        tool_count: 0,
                        error: None,
                    })
                    .collect()
            })
        }
    }

    fn ctx_with(
        spawner: Option<Arc<dyn SubagentSpawner>>,
        tasks: serde_json::Value,
    ) -> ToolCallCtx {
        ToolCallCtx {
            tool_call_id: "t1".into(),
            args: serde_json::json!({ "tasks": tasks }),
            signal: Arc::new(AtomicBool::new(false)),
            ctx: ToolContext {
                cwd: std::env::current_dir().unwrap(),
                env: HashMap::new(),
                session_id: "s1".into(),
                state_dir: std::env::temp_dir(),
                spawner,
            },
        }
    }

    fn seven_tasks() -> serde_json::Value {
        serde_json::json!([
            { "task": "a" },
            { "task": "b" },
            { "task": "c" },
            { "task": "d" },
            { "task": "e" },
            { "task": "f" },
            { "task": "g" },
        ])
    }

    /// The schema's maxItems (5) is enforced regardless of what the LLM
    /// sent, and the truncation is reported instead of being silent.
    #[tokio::test]
    async fn truncates_over_limit_tasks_and_reports() {
        let count = Arc::new(AtomicUsize::new(0));
        let spawner = Arc::new(CountingSpawner(Arc::clone(&count)));
        let r = execute(ctx_with(Some(spawner), seven_tasks()))
            .await
            .unwrap();

        assert_eq!(
            count.load(Ordering::SeqCst),
            5,
            "spawner must receive max 5"
        );
        assert_eq!(r.details["subagent_count"], 5);
        assert_eq!(r.details["dropped"], 2);
        let text = r.content.first().and_then(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        });
        assert!(
            text.unwrap_or_default().contains("dropped"),
            "dropped count must be visible to the model"
        );
    }

    /// No spawner wired (bare agent_loop / CLI without subagents): every
    /// task comes back as an explicit error, never a silent no-op.
    #[tokio::test]
    async fn no_spawner_reports_unavailable() {
        let r = execute(ctx_with(None, seven_tasks())).await.unwrap();

        assert_eq!(r.details["subagent_count"], 5);
        assert_eq!(r.details["errors"], 5);
        let text = r.content.first().and_then(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        });
        assert!(
            text.unwrap_or_default().contains("not available"),
            "unavailable spawner must be surfaced as errors"
        );
    }
}
