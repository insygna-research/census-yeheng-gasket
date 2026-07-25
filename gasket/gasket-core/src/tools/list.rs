//! `list` tool — list directory entries, optionally recursive with a glob.

use std::sync::Arc;

use crate::types::tool::{ToolCallCtx, ToolDefinition, ToolResult};
use crate::ContentBlock;

/// Cap the number of entries returned so tool output stays bounded.
const MAX_ENTRIES: usize = 2000;

pub fn tool() -> ToolDefinition {
    ToolDefinition {
        name: "list".into(),
        label: "List".into(),
        description: "List directory entries. Optional recursive + glob pattern."
            .into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory relative to cwd (default .)" },
                "recursive": { "type": "boolean", "description": "Recurse into subdirs (default false)" },
                "pattern": { "type": "string", "description": "Glob to filter, e.g. **/*.rs" }
            }
        }),
        execute: Arc::new(|ctx| Box::pin(execute(ctx))),
    }
}

async fn execute(ctx: ToolCallCtx) -> Result<ToolResult, crate::error::ToolError> {
    let path = ctx.args["path"].as_str().unwrap_or(".");
    let recursive = ctx.args["recursive"].as_bool().unwrap_or(false);
    let pattern = ctx.args["pattern"].as_str();

    let base = ctx.ctx.cwd.join(path);
    if !base.exists() {
        return Ok(ToolResult::error(format!("path not found: {}", path)));
    }

    let mut entries: Vec<String> = Vec::new();
    let walker = if recursive {
        walkdir::WalkDir::new(&base)
    } else {
        walkdir::WalkDir::new(&base).max_depth(1)
    };

    let glob_pat = pattern.and_then(|p| glob::Pattern::new(p).ok());
    for entry in walker.into_iter().filter_map(|e| e.ok()) {
        if entries.len() >= MAX_ENTRIES {
            break;
        }
        let rel = entry
            .path()
            .strip_prefix(&base)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .to_string();
        if rel.is_empty() {
            continue;
        }
        if let Some(pat) = &glob_pat {
            if !pat.matches(&rel) {
                continue;
            }
        }
        let suffix = if entry.file_type().is_dir() { "/" } else { "" };
        entries.push(format!("{}{}", rel, suffix));
    }

    entries.sort();
    Ok(ToolResult {
        content: vec![ContentBlock::text(entries.join("\n"))],
        details: serde_json::json!({"count": entries.len()}),
        is_error: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::tool::ToolContext;
    use std::sync::atomic::AtomicBool;

    async fn run(args: serde_json::Value, cwd: &std::path::Path) -> ToolResult {
        let t = tool();
        (t.execute)(ToolCallCtx {
            tool_call_id: "x".into(),
            args,
            signal: Arc::new(AtomicBool::new(false)),
            ctx: ToolContext {
                cwd: cwd.to_path_buf(),
                env: Default::default(),
                session_id: "s".into(),
                state_dir: cwd.to_path_buf(),
            },
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn lists_top_level() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("a.rs"), "").await.unwrap();
        tokio::fs::create_dir(tmp.path().join("sub"))
            .await
            .unwrap();
        let r = run(serde_json::json!({}), tmp.path()).await;
        let text = match &r.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!(),
        };
        assert!(text.contains("a.rs"));
        assert!(text.contains("sub/"));
    }

    #[tokio::test]
    async fn recursive_with_pattern() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("a.rs"), "").await.unwrap();
        tokio::fs::create_dir(tmp.path().join("d")).await.unwrap();
        tokio::fs::write(tmp.path().join("d/b.txt"), "")
            .await
            .unwrap();
        let r = run(
            serde_json::json!({"recursive": true, "pattern": "*.txt"}),
            tmp.path(),
        )
        .await;
        let text = match &r.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!(),
        };
        assert!(text.contains("b.txt"));
        assert!(!text.contains("a.rs"));
    }
}
