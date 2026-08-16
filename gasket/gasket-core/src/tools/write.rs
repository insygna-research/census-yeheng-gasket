//! `write` tool — write content to a file (creating parent dirs).

use std::sync::Arc;

use crate::types::tool::{RiskLevel, ToolCallCtx, ToolDefinition, ToolResult};
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
        risk: RiskLevel::Medium,
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

    let full = match super::resolve_within_cwd(&ctx.ctx.cwd, path) {
        Ok(p) => p,
        Err(msg) => return Ok(ToolResult::error(msg)),
    };
    if let Some(parent) = full.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    // Atomic write via temp file + rename — a crash mid-write must not leave
    // a half-written file behind (same policy as `edit`). Suffix rather than
    // `with_extension`, mirroring edit.rs's collision rationale.
    let mut tmp_os = full.clone().into_os_string();
    tmp_os.push(".gasket-tmp");
    let tmp = std::path::PathBuf::from(tmp_os);
    tokio::fs::write(&tmp, content).await?;
    tokio::fs::rename(&tmp, &full).await?;

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
                spawner: None,
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

    /// Atomicity proof: after a successful write there is no leftover tmp
    /// file, and a pre-existing tmp file from an earlier crashed attempt is
    /// overwritten cleanly (the next write must not resurrect stale content).
    #[tokio::test]
    async fn atomic_write_leaves_no_tmp_file() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("sub/f.txt");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        // Simulate a crashed previous attempt: orphaned tmp next to target.
        std::fs::write(tmp.path().join("sub/f.txt.gasket-tmp"), "STALE").unwrap();

        let t = tool();
        let r = (t.execute)(ToolCallCtx {
            tool_call_id: "x".into(),
            args: serde_json::json!({"path": "sub/f.txt", "content": "fresh"}),
            signal: Arc::new(AtomicBool::new(false)),
            ctx: ToolContext {
                cwd: tmp.path().to_path_buf(),
                env: Default::default(),
                session_id: "s".into(),
                state_dir: tmp.path().to_path_buf(),
                spawner: None,
            },
        })
        .await
        .unwrap();
        assert!(!r.is_error);
        assert_eq!(
            tokio::fs::read_to_string(&target).await.unwrap(),
            "fresh",
            "stale tmp content must not survive"
        );
        assert!(!tmp.path().join("sub/f.txt.gasket-tmp").exists());
    }
}
