//! `bash` tool — run a shell command with a timeout, output truncated.

use std::sync::Arc;
use std::time::Duration;

use crate::types::tool::{ToolCallCtx, ToolDefinition, ToolResult};
use crate::ContentBlock;

const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MAX_OUTPUT_BYTES: usize = 200_000;

pub fn tool() -> ToolDefinition {
    ToolDefinition {
        name: "bash".into(),
        label: "Bash".into(),
        description: "Run a shell command. Optional timeout in seconds.".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string" },
                "timeout": { "type": "integer", "description": "seconds (default 120)" }
            },
            "required": ["command"]
        }),
        execute: Arc::new(|ctx| Box::pin(execute(ctx))),
    }
}

async fn execute(ctx: ToolCallCtx) -> Result<ToolResult, crate::error::ToolError> {
    let command = ctx.args["command"]
        .as_str()
        .ok_or_else(|| crate::error::ToolError::Message("command is required".into()))?;
    let timeout = ctx.args["timeout"].as_u64().unwrap_or(DEFAULT_TIMEOUT_SECS);

    let mut cmd = if cfg!(target_os = "windows") {
        let mut c = tokio::process::Command::new("cmd");
        c.arg("/C").arg(command);
        c
    } else {
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg(command);
        c
    };
    cmd.current_dir(&ctx.ctx.cwd);
    cmd.env_clear();
    cmd.envs(&ctx.ctx.env);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let output = match tokio::time::timeout(Duration::from_secs(timeout), cmd.output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            return Ok(ToolResult::error(format!("failed to spawn: {e}")));
        }
        Err(_) => {
            return Ok(ToolResult::error(format!("timed out after {timeout}s")));
        }
    };

    let stdout = truncate(String::from_utf8_lossy(&output.stdout).into_owned());
    let stderr = truncate(String::from_utf8_lossy(&output.stderr).into_owned());
    let code = output.status.code().unwrap_or(-1);

    let mut text = String::new();
    if !stdout.is_empty() {
        text.push_str(&stdout);
    }
    if !stderr.is_empty() {
        if !text.is_empty() {
            text.push_str("\n--- stderr ---\n");
        }
        text.push_str(&stderr);
    }
    text.push_str(&format!("\n[exit {}]", code));

    let is_error = !output.status.success();
    Ok(ToolResult {
        content: vec![ContentBlock::text(text.trim())],
        details: serde_json::json!({"exit_code": code}),
        is_error,
    })
}

fn truncate(mut s: String) -> String {
    if s.len() > MAX_OUTPUT_BYTES {
        s.truncate(MAX_OUTPUT_BYTES);
        s.push_str("\n... (truncated)");
    }
    s
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
                env: std::env::vars().collect(),
                session_id: "s".into(),
                state_dir: cwd.to_path_buf(),
            },
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn runs_echo() {
        let tmp = tempfile::tempdir().unwrap();
        let r = run(serde_json::json!({"command": "echo hello"}), tmp.path()).await;
        assert!(!r.is_error, "stderr was captured");
        let text = match &r.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!(),
        };
        assert!(text.contains("hello"), "got: {text}");
    }

    #[tokio::test]
    async fn reports_nonzero_exit() {
        let tmp = tempfile::tempdir().unwrap();
        let cmd = if cfg!(target_os = "windows") {
            "cmd /C exit 3"
        } else {
            "exit 3"
        };
        let r = run(serde_json::json!({"command": cmd}), tmp.path()).await;
        assert!(r.is_error);
        let text = match &r.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!(),
        };
        assert!(text.contains("exit 3") || text.contains("[exit"));
    }
}
