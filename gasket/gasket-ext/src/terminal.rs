//! `terminal` tool — run commands on a PTY, with run/read/send actions and a
//! per-session output ring buffer. Lives in gasket-ext behind Cargo feature
//! `terminal`; the session registry is process-global within this crate.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::sync::{Arc, LazyLock, Mutex, RwLock};

use gasket_core::{
    ContentBlock, ExtensionApi, RiskLevel, ToolCallCtx, ToolDefinition, ToolError, ToolResult,
};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};

/// Rolling output buffer for one PTY session, capped at MAX_BYTES: pushing
/// past the cap evicts whole oldest chunks until back under it.
#[derive(Default)]
struct OutputRing {
    chunks: VecDeque<String>,
    bytes: usize,
}

impl OutputRing {
    const MAX_BYTES: usize = 64 * 1024;

    fn push_str(&mut self, s: &str) {
        let mut s = s.to_string();
        if s.len() > Self::MAX_BYTES {
            // Char-safe tail: never slice through a multi-byte char.
            let mut cut = Self::MAX_BYTES;
            while !s.is_char_boundary(cut) {
                cut -= 1;
            }
            s = s[cut..].to_string();
        }
        self.bytes += s.len();
        self.chunks.push_back(s);
        while self.bytes > Self::MAX_BYTES {
            let Some(front) = self.chunks.pop_front() else {
                break;
            };
            self.bytes -= front.len();
        }
    }

    /// Take everything buffered (empty string when nothing new).
    fn drain(&mut self) -> String {
        let out: String = self.chunks.drain(..).collect();
        self.bytes = 0;
        out
    }
}

struct PtySession {
    child: Box<dyn portable_pty::Child + Send>,
    /// Consumed by the `send` action; must be taken at spawn time because
    /// `take_writer` succeeds only once per master.
    writer: Box<dyn Write + Send>,
    ring: Arc<Mutex<OutputRing>>,
}

/// Sessions keyed by `<tool session_id>/<name>`; same global-state pattern
/// as gasket-core's `proxy.rs` override. Known limitation: no reaper — a
/// session is killed only when a new `run` reuses its key.
static REGISTRY: LazyLock<RwLock<HashMap<String, Arc<Mutex<PtySession>>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Same registration shape as `search.rs::register`.
pub fn register(api: &mut dyn ExtensionApi) {
    api.register_tool(ToolDefinition {
        name: "terminal".into(),
        label: "Terminal".into(),
        description: "Run commands on a PTY. action: run (spawn), read (drain new \
                      output + exit status), send (write to stdin)."
            .into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["run", "read", "send"] },
                "command": { "type": "string", "description": "command (run) or input line (send)" },
                "session": { "type": "string", "description": "session name (default \"default\")" }
            },
            "required": ["action"]
        }),
        risk: RiskLevel::Medium,
        execute: Arc::new(|ctx| Box::pin(execute(ctx))),
    });
}

async fn execute(ctx: ToolCallCtx) -> Result<ToolResult, ToolError> {
    let action = ctx.args["action"]
        .as_str()
        .ok_or_else(|| ToolError::Message("action is required".into()))?;
    let session_name = ctx.args["session"].as_str().unwrap_or("default");
    let key = format!("{}/{}", ctx.ctx.session_id, session_name);
    match action {
        "run" => run(&ctx, &key),
        "read" => read(&key).await,
        "send" => send(&ctx, &key),
        other => Ok(ToolResult::error(format!("unknown action: {other}"))),
    }
}

fn run(ctx: &ToolCallCtx, key: &str) -> Result<ToolResult, ToolError> {
    let command = ctx.args["command"]
        .as_str()
        .ok_or_else(|| ToolError::Message("command is required for run".into()))?;

    // Replace any live session under this key (kill + drop).
    if let Some(old) = REGISTRY.write().unwrap().remove(key) {
        let mut s = old.lock().unwrap();
        let _ = s.child.kill();
    }

    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            ..Default::default()
        })
        .map_err(|e| ToolError::Message(format!("openpty failed: {e}")))?;
    let mut cmd = CommandBuilder::new(if cfg!(target_os = "windows") {
        "cmd"
    } else {
        "sh"
    });
    if cfg!(target_os = "windows") {
        cmd.arg("/C");
    } else {
        cmd.arg("-c");
    }
    cmd.arg(command);
    cmd.cwd(&ctx.ctx.cwd);
    // Host env is already scrubbed of GASKET_* secrets by the host; CommandBuilder
    // inherits the process env by default, so nothing to do here.
    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| ToolError::Message(format!("spawn failed: {e}")))?;
    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| ToolError::Message(format!("pty reader failed: {e}")))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|e| ToolError::Message(format!("pty writer failed: {e}")))?;

    let ring = Arc::new(Mutex::new(OutputRing::default()));
    let pump_ring = Arc::clone(&ring);
    // Blocking reads live on a plain thread — portable-pty is sync IO. The
    // reader holds its own dup of the master fd, so dropping `pair` below is
    // safe; the thread exits on EOF when the child closes its end.
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => pump_ring
                    .lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&buf[..n])),
            }
        }
    });

    REGISTRY.write().unwrap().insert(
        key.to_string(),
        Arc::new(Mutex::new(PtySession {
            child,
            writer,
            ring,
        })),
    );
    Ok(ToolResult {
        content: vec![ContentBlock::text(format!("session `{key}` started"))],
        details: serde_json::json!({"session": key}),
        is_error: false,
    })
}

async fn read(key: &str) -> Result<ToolResult, ToolError> {
    let Some(sess) = REGISTRY.read().unwrap().get(key).cloned() else {
        return Ok(ToolResult {
            content: vec![ContentBlock::text("no active session")],
            details: serde_json::json!({"exited": true}),
            is_error: false,
        });
    };
    // Lock scope ends before any await; child poll is non-blocking. The ring
    // itself bounds output at 64KiB — core's spill_or_truncate is pub(crate)
    // and not exported, so ext returns the drained text directly (consistent
    // with ext's search/web tools, which don't spill either).
    let (mut text, status) = {
        let mut s = sess.lock().unwrap();
        let out = s.ring.lock().unwrap().drain();
        // try_wait: Ok(Some(status)) = exited, Ok(None) = still running.
        let status = s
            .child
            .try_wait()
            .map_err(|e| ToolError::Message(format!("wait failed: {e}")))?;
        (out, status)
    };
    // `ExitStatus` is not Copy: derive `exited`/`exit_code` from one Option.
    let exit_code = status.map(|s| s.exit_code() as i32);
    if let Some(code) = exit_code {
        text.push_str(&format!("\n[exited code {code}]"));
    }
    Ok(ToolResult {
        content: vec![ContentBlock::text(text.trim())],
        details: serde_json::json!({
            "exited": exit_code.is_some(),
            "exit_code": exit_code,
        }),
        is_error: false,
    })
}

fn send(ctx: &ToolCallCtx, key: &str) -> Result<ToolResult, ToolError> {
    let input = ctx.args["command"]
        .as_str()
        .ok_or_else(|| ToolError::Message("command is required for send".into()))?;
    let Some(sess) = REGISTRY.read().unwrap().get(key).cloned() else {
        return Ok(ToolResult::error(format!("no session `{key}`")));
    };
    let mut s = sess.lock().unwrap();
    // PTY stdin is line-oriented: always terminate with a newline.
    s.writer
        .write_all(input.as_bytes())
        .and_then(|_| s.writer.write_all(b"\n"))
        .and_then(|_| s.writer.flush())
        .map_err(|e| ToolError::Message(format!("stdin write failed: {e}")))?;
    Ok(ToolResult {
        content: vec![ContentBlock::text("sent")],
        details: serde_json::json!({"session": key}),
        is_error: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_returns_and_clears() {
        let mut r = OutputRing::default();
        r.push_str("hello\n");
        r.push_str("world\n");
        assert_eq!(r.drain(), "hello\nworld\n");
        assert_eq!(r.drain(), "", "second drain is empty");
    }

    #[test]
    fn cap_evicts_oldest_first() {
        let mut r = OutputRing::default();
        // 2 chunks of half the cap (plus 1) -> over cap after the second push.
        r.push_str(&"a".repeat(OutputRing::MAX_BYTES / 2));
        r.push_str(&"b".repeat(OutputRing::MAX_BYTES / 2 + 1));
        let out = r.drain();
        assert!(out.starts_with('b'), "oldest chunk evicted first");
        assert!(out.len() <= OutputRing::MAX_BYTES);
    }

    #[test]
    fn oversized_single_chunk_is_truncated_to_cap() {
        let mut r = OutputRing::default();
        r.push_str(&"x".repeat(OutputRing::MAX_BYTES * 2));
        assert!(r.drain().len() <= OutputRing::MAX_BYTES);
    }

    #[test]
    fn oversized_chunk_keeps_tail_on_char_boundary() {
        let mut r = OutputRing::default();
        // MAX_BYTES - 1 x's: cut lands mid-é, so the walk-back loop must run.
        r.push_str(&format!("{}é", "x".repeat(OutputRing::MAX_BYTES - 1)));
        let out = r.drain();
        assert!(
            out.ends_with('é'),
            "tail must keep the trailing multi-byte char"
        );
        assert!(out.len() <= OutputRing::MAX_BYTES);
    }

    // ── Tool-level integration (mirrors search.rs's proxy wiring test) ──

    use gasket_core::{ContentBlock, ExtensionApiImpl, ToolCallCtx, ToolContext, ToolResult};
    use std::sync::Arc;

    fn registered_tool() -> gasket_core::ToolDefinition {
        let mut api = ExtensionApiImpl::new();
        super::register(&mut api);
        assert_eq!(api.tools.len(), 1);
        assert_eq!(api.tools[0].name, "terminal");
        api.tools.remove(0)
    }

    async fn exec(args: serde_json::Value, cwd: &std::path::Path, session: &str) -> ToolResult {
        let t = registered_tool();
        (t.execute)(ToolCallCtx {
            tool_call_id: "x".into(),
            args,
            signal: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            ctx: ToolContext {
                cwd: cwd.to_path_buf(),
                env: std::env::vars().collect(),
                session_id: session.into(),
                state_dir: cwd.to_path_buf(),
                spawner: None,
            },
        })
        .await
        .unwrap()
    }

    fn text(r: &ToolResult) -> String {
        match &r.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text"),
        }
    }

    #[tokio::test]
    async fn run_then_read_returns_output_and_exit() {
        let tmp = tempfile::tempdir().unwrap();
        // unique session key so parallel tests never share a registry slot
        let s = format!("run-read-{}", std::process::id());
        let r = exec(
            serde_json::json!({"action": "run", "command": "echo hello"}),
            tmp.path(),
            &s,
        )
        .await;
        assert!(!r.is_error, "spawn failed");
        // Poll read until the child exits and output shows up (pump thread is async).
        let mut got = String::new();
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let r = exec(serde_json::json!({"action": "read"}), tmp.path(), &s).await;
            got = text(&r);
            if got.contains("[exited") {
                break;
            }
        }
        assert!(got.contains("hello"), "got: {got}");
        assert!(got.contains("[exited code 0]"), "got: {got}");
    }

    #[tokio::test]
    async fn read_with_no_session_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let r = exec(
            serde_json::json!({"action": "read"}),
            tmp.path(),
            "never-spawned",
        )
        .await;
        assert!(!r.is_error);
        assert_eq!(text(&r), "no active session");
    }

    #[tokio::test]
    async fn send_writes_to_stdin_of_running_child() {
        let tmp = tempfile::tempdir().unwrap();
        let s = format!("send-{}", std::process::id());
        // `read` a line then echo it back — proves stdin round-trip through the PTY.
        let r = exec(
            serde_json::json!({"action": "run", "command": "read line; echo got:$line"}),
            tmp.path(),
            &s,
        )
        .await;
        assert!(!r.is_error);
        let r = exec(
            serde_json::json!({"action": "send", "command": "ping"}),
            tmp.path(),
            &s,
        )
        .await;
        assert!(!r.is_error, "send failed");
        let mut got = String::new();
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let r = exec(serde_json::json!({"action": "read"}), tmp.path(), &s).await;
            got = text(&r);
            if got.contains("got:ping") {
                break;
            }
        }
        assert!(got.contains("got:ping"), "got: {got}");
    }
}
