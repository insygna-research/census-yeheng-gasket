//! `spawn_subagents` tool — fan out parallel sub-agent loops.

use std::sync::Arc;

use crate::subagent::{SubagentSpawn, SubagentSpawner};
use crate::types::tool::{RiskLevel, ToolCallCtx, ToolDefinition, ToolResult};
use crate::ContentBlock;

pub fn tool() -> ToolDefinition {
    ToolDefinition {
        name: "spawn_subagents".into(),
        label: "Spawn Subagents".into(),
        description: "Spawn parallel sub-agents to work on independent tasks concurrently. Each sub-agent runs its own agent loop with the same tools. Use for divide-and-conquer: searching multiple areas, writing + testing + reviewing in parallel.".into(),
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

    let spawns: Vec<SubagentSpawn> = tasks
        .iter()
        .filter_map(|t| {
            t["task"]
                .as_str()
                .map(|s| SubagentSpawn { task: s.to_string() })
        })
        .collect();

    if spawns.is_empty() {
        return Ok(ToolResult::error(
            "no valid tasks provided (each needs a 'task' string)".to_string(),
        ));
    }

    // Enforce the schema's maxItems: 5, regardless of what the LLM sent.
    let mut spawns = spawns;
    spawns.truncate(5);

    let spawner: Arc<dyn SubagentSpawner> = match &ctx.ctx.spawner {
        Some(s) => Arc::clone(s),
        None => Arc::new(crate::subagent::NoopSubagentSpawner),
    };

    // The emit callback is a no-op here — the host's spawner implementation
    // owns the event channel to the gateway. This tool closure only collects
    // the final results. (Events flow through the spawner's internal emit,
    // not through this closure's return.)
    let emit: Arc<dyn Fn(crate::SubagentEvent) + Send + Sync> = Arc::new(|_| {});
    let results = spawner.spawn(spawns, emit).await;

    let summary = results
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

    Ok(ToolResult {
        content: vec![ContentBlock::text(summary)],
        details: serde_json::json!({
            "subagent_count": results.len(),
            "completed": results.iter().filter(|r| r.error.is_none()).count(),
            "errors": results.iter().filter(|r| r.error.is_some()).count(),
        }),
        is_error: false,
    })
}
