//! `bash` tool — run a shell command with a timeout, output truncated.

use std::sync::Arc;
use std::time::Duration;

use crate::types::tool::{RiskLevel, ToolCallCtx, ToolDefinition, ToolResult};
use crate::ContentBlock;

/// Default command timeout.
const DEFAULT_TIMEOUT_SECS: u64 = 120;

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
        risk: RiskLevel::High,
        execute: Arc::new(|ctx| Box::pin(execute(ctx))),
    }
}

async fn execute(ctx: ToolCallCtx) -> Result<ToolResult, crate::error::ToolError> {
    if ctx.aborted() {
        return Ok(ToolResult::error("aborted".to_string()));
    }
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
    // Don't leak gasket's own config/secrets (e.g. GASKET_LLM_KEY) into
    // commands the model asks to run.
    cmd.envs(
        ctx.ctx
            .env
            .iter()
            .filter(|(k, _)| !k.starts_with("GASKET_")),
    );
    // A timeout drops the `output()` future mid-wait; without kill_on_drop the
    // spawned shell (and its children) would survive as orphans burning CPU.
    cmd.kill_on_drop(true);

    let output = match tokio::time::timeout(Duration::from_secs(timeout), cmd.output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            return Ok(ToolResult::error(format!("failed to spawn: {e}")));
        }
        Err(_) => {
            return Ok(ToolResult::error(format!("timed out after {timeout}s")));
        }
    };

    let stdout = super::truncate_output(&String::from_utf8_lossy(&output.stdout));
    let stderr = super::truncate_output(&String::from_utf8_lossy(&output.stderr));
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
                spawner: None,
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

    #[tokio::test]
    async fn does_not_leak_gasket_secrets() {
        let tmp = tempfile::tempdir().unwrap();
        let t = tool();
        let mut env = std::collections::HashMap::new();
        env.insert("GASKET_LLM_KEY".to_string(), "sk-secret".to_string());
        env.insert("KEEP_ME".to_string(), "visible".to_string());
        let cmd = if cfg!(target_os = "windows") {
            "echo %GASKET_LLM_KEY%%KEEP_ME%"
        } else {
            "echo $GASKET_LLM_KEY$KEEP_ME"
        };
        let r = (t.execute)(ToolCallCtx {
            tool_call_id: "x".into(),
            args: serde_json::json!({"command": cmd}),
            signal: Arc::new(AtomicBool::new(false)),
            ctx: ToolContext {
                cwd: tmp.path().to_path_buf(),
                env,
                session_id: "s".into(),
                state_dir: tmp.path().to_path_buf(),
                spawner: None,
            },
        })
        .await
        .unwrap();
        let text = match &r.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!(),
        };
        assert!(!text.contains("sk-secret"), "leaked secret, got: {text}");
        assert!(text.contains("visible"), "non-secret env dropped: {text}");
    }

    /// Timeout must not just fail fast — it must kill the child. The command
    /// records its shell PID, then sleeps well past the 1s timeout; after the
    /// tool returns, that PID must be gone (kill_on_drop fired when the
    /// `output()` future was dropped at the deadline).
    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_child_process() {
        let tmp = tempfile::tempdir().unwrap();
        let pidfile = tmp.path().join("pid");
        let start = std::time::Instant::now();
        let r = run(
            serde_json::json!({
                "command": format!("echo $$ > {}; sleep 30", pidfile.display()),
                "timeout": 1
            }),
            tmp.path(),
        )
        .await;
        assert!(
            start.elapsed().as_secs() < 10,
            "must return at the 1s deadline, not after sleep 30"
        );
        assert!(r.is_error);
        match &r.content[0] {
            ContentBlock::Text { text } => assert!(text.contains("timed out"), "got: {text}"),
            _ => panic!("expected text content"),
        }
        // Give the runtime a moment to reap the killed child before probing.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let pid = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .to_string();
        let alive = std::process::Command::new("kill")
            .arg("-0")
            .arg(&pid)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(
            !alive,
            "child {pid} survived the timeout — kill_on_drop not effective"
        );
    }
}
