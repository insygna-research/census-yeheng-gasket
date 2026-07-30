//! External tools over stdio JSONL (one process, many tools).
//!
//! Protocol (one JSON object per line):
//! ```text
//! → {"op":"list"}
//! ← {"tools":[{"name":"...","description":"...","parameters":{...}}]}
//! → {"op":"call","id":"...","name":"...","args":{...}}
//! ← {"id":"...","content":[{"type":"text","text":"..."}],"is_error":false}
//! ```
//!
//! Host owns the child; builds `ToolDefinition`s whose `execute` talks to it.
//! No ExtensionApi over the wire. Reload = drop + re-spawn.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use gasket_core::{ContentBlock, ToolDefinition, ToolError, ToolResult};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, thiserror::Error)]
pub enum ExternalToolError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("timeout")]
    Timeout,
    #[error("empty GASKET_EXTERNAL_TOOLS")]
    Empty,
}

#[derive(Debug, Clone, Deserialize)]
struct ListedTool {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default = "empty_object")]
    parameters: serde_json::Value,
    #[serde(default)]
    label: Option<String>,
}

fn empty_object() -> serde_json::Value {
    serde_json::json!({"type": "object", "properties": {}})
}

#[derive(Debug, Deserialize)]
struct ListResponse {
    tools: Vec<ListedTool>,
}

#[derive(Debug, Serialize)]
struct CallRequest<'a> {
    op: &'static str,
    id: &'a str,
    name: &'a str,
    args: &'a serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct CallResponse {
    #[allow(dead_code)]
    id: String,
    #[serde(default)]
    content: Vec<ContentLine>,
    #[serde(default)]
    is_error: bool,
}

#[derive(Debug, Deserialize)]
struct ContentLine {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    text: String,
}

struct BridgeInner {
    /// Held so `kill_on_drop` reaps the process when the bridge is dropped.
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

/// One long-lived external tool process.
pub struct ExternalToolBridge {
    inner: Mutex<BridgeInner>,
    timeout: Duration,
}

impl ExternalToolBridge {
    /// Spawn `program` with `args`, run `list`, return bridge + tool defs.
    pub async fn spawn(
        program: &str,
        args: &[&str],
    ) -> Result<(Arc<Self>, Vec<ToolDefinition>), ExternalToolError> {
        Self::spawn_with_timeout(program, args, DEFAULT_TIMEOUT).await
    }

    pub async fn spawn_with_timeout(
        program: &str,
        args: &[&str],
        timeout: Duration,
    ) -> Result<(Arc<Self>, Vec<ToolDefinition>), ExternalToolError> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| ExternalToolError::Protocol("no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ExternalToolError::Protocol("no stdout".into()))?;

        let bridge = Arc::new(Self {
            inner: Mutex::new(BridgeInner {
                _child: child,
                stdin,
                stdout: BufReader::new(stdout),
            }),
            timeout,
        });

        let listed = bridge.list().await?;
        let tools = listed
            .into_iter()
            .map(|t| bridge.tool_definition(t))
            .collect();
        Ok((bridge, tools))
    }

    async fn list(&self) -> Result<Vec<ListedTool>, ExternalToolError> {
        let line = self.roundtrip(r#"{"op":"list"}"#).await?;
        let resp: ListResponse = serde_json::from_str(&line)?;
        Ok(resp.tools)
    }

    async fn call(
        &self,
        id: &str,
        name: &str,
        args: &serde_json::Value,
    ) -> Result<CallResponse, ExternalToolError> {
        let req = CallRequest {
            op: "call",
            id,
            name,
            args,
        };
        let payload = serde_json::to_string(&req)?;
        let line = self.roundtrip(&payload).await?;
        Ok(serde_json::from_str(&line)?)
    }

    async fn roundtrip(&self, request_line: &str) -> Result<String, ExternalToolError> {
        let mut guard = self.inner.lock().await;
        guard.stdin.write_all(request_line.as_bytes()).await?;
        guard.stdin.write_all(b"\n").await?;
        guard.stdin.flush().await?;

        let mut line = String::new();
        let read = guard.stdout.read_line(&mut line);
        match tokio::time::timeout(self.timeout, read).await {
            Ok(Ok(0)) => Err(ExternalToolError::Protocol(
                "external tool closed stdout".into(),
            )),
            Ok(Ok(_)) => {
                while line.ends_with('\n') || line.ends_with('\r') {
                    line.pop();
                }
                if line.is_empty() {
                    return Err(ExternalToolError::Protocol("empty response line".into()));
                }
                Ok(line)
            }
            Ok(Err(e)) => Err(ExternalToolError::Io(e)),
            Err(_) => Err(ExternalToolError::Timeout),
        }
    }

    fn tool_definition(self: &Arc<Self>, t: ListedTool) -> ToolDefinition {
        let bridge = Arc::clone(self);
        let name = t.name.clone();
        let label = t.label.unwrap_or_else(|| t.name.clone());
        ToolDefinition {
            name: t.name,
            label,
            description: t.description,
            parameters: t.parameters,
            execute: Arc::new(move |ctx| {
                let bridge = Arc::clone(&bridge);
                let name = name.clone();
                Box::pin(async move {
                    if ctx.aborted() {
                        return Ok(ToolResult::error("aborted"));
                    }
                    match bridge.call(&ctx.tool_call_id, &name, &ctx.args).await {
                        Ok(resp) => {
                            let content: Vec<ContentBlock> = resp
                                .content
                                .into_iter()
                                .filter(|c| c.kind == "text" || c.kind.is_empty())
                                .map(|c| ContentBlock::text(c.text))
                                .collect();
                            let content = if content.is_empty() {
                                vec![ContentBlock::text(String::new())]
                            } else {
                                content
                            };
                            Ok(ToolResult {
                                content,
                                details: serde_json::Value::Null,
                                is_error: resp.is_error,
                            })
                        }
                        Err(e) => Err(ToolError::Message(e.to_string())),
                    }
                })
            }),
        }
    }
}

/// Parse `GASKET_EXTERNAL_TOOLS`: comma-separated commands.
/// Each entry is split on whitespace into program + args
/// (e.g. `python3 /path/echo.py,./bin/tool`).
pub fn commands_from_env() -> Vec<Vec<String>> {
    commands_from(&|k| std::env::var(k))
}

pub fn commands_from(lookup: &dyn Fn(&str) -> Result<String, std::env::VarError>) -> Vec<Vec<String>> {
    let Ok(raw) = lookup("GASKET_EXTERNAL_TOOLS") else {
        return Vec::new();
    };
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|entry| {
            shell_words(entry)
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .filter(|v| !v.is_empty())
        .collect()
}

/// Minimal whitespace split (no quotes gymnastics). Good enough for env paths.
fn shell_words(s: &str) -> Vec<&str> {
    s.split_whitespace().collect()
}

/// Spawn every command from env/list; collect tools. Failures are returned per command.
pub async fn load_all(
    commands: &[Vec<String>],
) -> Result<Vec<ToolDefinition>, ExternalToolError> {
    let mut tools = Vec::new();
    for cmd in commands {
        let (program, args) = cmd
            .split_first()
            .ok_or(ExternalToolError::Empty)?;
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let (_bridge, defs) = ExternalToolBridge::spawn(program, &arg_refs).await?;
        // Bridge lives inside each tool's execute Arc; drop tools → kill child.
        tools.extend(defs);
    }
    Ok(tools)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gasket_core::ToolCallCtx;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicBool;

    fn fixture_script() -> String {
        // Resolve relative to this crate's tests — write a temp python helper.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("echo_tool.py");
        std::fs::write(
            &path,
            r#"#!/usr/bin/env python3
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    if msg.get("op") == "list":
        print(json.dumps({
            "tools": [{
                "name": "echo",
                "description": "echo args",
                "parameters": {
                    "type": "object",
                    "properties": {"text": {"type": "string"}},
                    "required": ["text"]
                }
            }]
        }), flush=True)
    elif msg.get("op") == "call":
        text = (msg.get("args") or {}).get("text", "")
        print(json.dumps({
            "id": msg.get("id", ""),
            "content": [{"type": "text", "text": text}],
            "is_error": False
        }), flush=True)
"#,
        )
        .unwrap();
        // Keep dir alive by leaking path string; on unix python reads file by path.
        // Use into_path to persist for process lifetime of test.
        let kept = dir.keep();
        kept.join("echo_tool.py").to_string_lossy().into_owned()
    }

    #[tokio::test]
    async fn list_and_call_echo() {
        let script = fixture_script();
        let (bridge, tools) = ExternalToolBridge::spawn("python3", &[&script])
            .await
            .expect("spawn");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");

        let result = (tools[0].execute)(ToolCallCtx {
            tool_call_id: "c1".into(),
            args: serde_json::json!({"text": "hi"}),
            signal: Arc::new(AtomicBool::new(false)),
            ctx: gasket_core::ToolContext {
                cwd: ".".into(),
                env: HashMap::new(),
                session_id: "t".into(),
                state_dir: ".".into(),
            },
        })
        .await
        .unwrap();
        assert!(!result.is_error);
        match &result.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "hi"),
            _ => panic!(),
        }
        drop(bridge);
    }

    #[test]
    fn parse_env_commands() {
        let cmds = commands_from(&|k| {
            if k == "GASKET_EXTERNAL_TOOLS" {
                Ok("python3 /tmp/a.py, ./bin/x".into())
            } else {
                Err(std::env::VarError::NotPresent)
            }
        });
        assert_eq!(cmds.len(), 2);
        assert_eq!(cmds[0], vec!["python3".to_string(), "/tmp/a.py".to_string()]);
        assert_eq!(cmds[1], vec!["./bin/x".to_string()]);
    }
}
