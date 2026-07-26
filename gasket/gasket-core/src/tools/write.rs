//! `write` tool — write content to a file (creating parent dirs).

use std::sync::Arc;

use crate::types::tool::{ToolCallCtx, ToolDefinition, ToolResult};
use crate::ContentBlock;

pub fn tool() -> ToolDefinition {
    ToolDefinition {
        name: "write".into(),
        label: "Write".into(),
        description: "Write text content to a file, creating parent directories.".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "content": { "type": "string" }
            },
            "required": ["path", "content"]
        }),
        execute: Arc::new(|ctx| Box::pin(execute(ctx))),
    }
}

async fn execute(ctx: ToolCallCtx) -> Result<ToolResult, crate::error::ToolError> {
    if ctx.aborted() {
        return Ok(ToolResult::error("aborted".to_string()));
    }
    let path = ctx.args["path"]
        .as_str()
        .ok_or_else(|| crate::error::ToolError::Message("path is required".into()))?;
    let content = ctx.args["content"]
        .as_str()
        .ok_or_else(|| crate::error::ToolError::Message("content is required".into()))?;

    let full = ctx.ctx.cwd.join(path);
    if let Some(parent) = full.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(&full, content).await?;

    Ok(ToolResult {
        content: vec![ContentBlock::text(format!(
            "wrote {} bytes to {}",
            content.len(),
            path
        ))],
        details: serde_json::json!({"path": path, "bytes": content.len()}),
        is_error: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::tool::ToolContext;
    use std::sync::atomic::AtomicBool;

    #[tokio::test]
    async fn writes_file_and_creates_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let t = tool();
        let r = (t.execute)(ToolCallCtx {
            tool_call_id: "x".into(),
            args: serde_json::json!({"path": "sub/f.txt", "content": "hello"}),
            signal: Arc::new(AtomicBool::new(false)),
            ctx: ToolContext {
                cwd: tmp.path().to_path_buf(),
                env: Default::default(),
                session_id: "s".into(),
                state_dir: tmp.path().to_path_buf(),
            },
        })
        .await
        .unwrap();
        assert!(!r.is_error);
        let written = tokio::fs::read_to_string(tmp.path().join("sub/f.txt"))
            .await
            .unwrap();
        assert_eq!(written, "hello");
    }
}
