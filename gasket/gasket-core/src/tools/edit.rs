//! `edit` tool — replace a unique string in a file, written atomically.

use std::sync::Arc;

use crate::types::tool::{ToolCallCtx, ToolDefinition, ToolResult};
use crate::ContentBlock;

pub fn tool() -> ToolDefinition {
    ToolDefinition {
        name: "edit".into(),
        label: "Edit".into(),
        description: "Replace old_text with new_text in a file. old_text must be unique."
            .into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "old_text": { "type": "string" },
                "new_text": { "type": "string" }
            },
            "required": ["path", "old_text", "new_text"]
        }),
        execute: Arc::new(|ctx| Box::pin(execute(ctx))),
    }
}

async fn execute(ctx: ToolCallCtx) -> Result<ToolResult, crate::error::ToolError> {
    let path = ctx.args["path"]
        .as_str()
        .ok_or_else(|| crate::error::ToolError::Message("path is required".into()))?;
    let old_text = ctx.args["old_text"]
        .as_str()
        .ok_or_else(|| crate::error::ToolError::Message("old_text is required".into()))?;
    let new_text = ctx.args["new_text"]
        .as_str()
        .ok_or_else(|| crate::error::ToolError::Message("new_text is required".into()))?;

    let full = ctx.ctx.cwd.join(path);
    let original = tokio::fs::read_to_string(&full).await?;

    let count = original.matches(old_text).count();
    if count == 0 {
        return Ok(ToolResult::error(format!(
            "old_text not found in {}",
            path
        )));
    }
    if count > 1 {
        return Ok(ToolResult::error(format!(
            "old_text appears {} times in {}; it must be unique",
            count, path
        )));
    }

    let updated = original.replacen(old_text, new_text, 1);

    // Atomic write via temp file + rename.
    let tmp = full.with_extension("gasket-tmp");
    tokio::fs::write(&tmp, &updated).await?;
    tokio::fs::rename(&tmp, &full).await?;

    Ok(ToolResult {
        content: vec![ContentBlock::text(format!("edited {}", path))],
        details: serde_json::json!({"path": path}),
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
    async fn replaces_unique_match() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("f.txt"), "foo bar baz")
            .await
            .unwrap();
        let r = run(
            serde_json::json!({"path": "f.txt", "old_text": "bar", "new_text": "QUX"}),
            tmp.path(),
        )
        .await;
        assert!(!r.is_error);
        assert_eq!(
            tokio::fs::read_to_string(tmp.path().join("f.txt"))
                .await
                .unwrap(),
            "foo QUX baz"
        );
    }

    #[tokio::test]
    async fn errors_on_multiple_matches() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("f.txt"), "x x x")
            .await
            .unwrap();
        let r = run(
            serde_json::json!({"path": "f.txt", "old_text": "x", "new_text": "y"}),
            tmp.path(),
        )
        .await;
        assert!(r.is_error);
    }

    #[tokio::test]
    async fn errors_on_missing_match() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("f.txt"), "abc")
            .await
            .unwrap();
        let r = run(
            serde_json::json!({"path": "f.txt", "old_text": "zzz", "new_text": "y"}),
            tmp.path(),
        )
        .await;
        assert!(r.is_error);
    }
}
