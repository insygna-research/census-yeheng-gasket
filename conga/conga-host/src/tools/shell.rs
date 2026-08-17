//! Persistent per-session shell: `cd`, exported env vars, and activated
//! virtualenvs survive across `bash` tool calls within a session.
//!
//! One `sh` child process per session id (process-global registry; `ToolContext`
//! is per-call and cannot carry handles). Commands are fed over stdin with a
//! sentinel protocol — the wrapper prints a marker + exit status when a
//! command finishes, so we know exactly where output ends. The stdout reader
//! lives IN the session (`BufReader<ChildStdout>`), so partial reads never
//! lose bytes between calls. A per-session [`tokio::sync::Mutex`] serializes
//! commands (a shell has one stdin).
//!
//! On timeout the shell is killed and evicted: a timed-out command may have
//! left the shell wedged (blocked read, unbalanced quote), so the next call
//! starts a fresh shell and says so in its output.

use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Marker printed after each command's output. The wrapper is evaluated as
/// one unit by the shell, so the user command cannot swallow it.
const SENTINEL: &str = "__CONGA_DONE__";

/// One live shell and its I/O, owned by the registry entry.
struct ShellSession {
    /// Kept (never read) so `kill_on_drop` reaps the process tree when the
    /// session is evicted.
    #[allow(dead_code)]
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: BufReader<tokio::process::ChildStdout>,
}

impl ShellSession {
    fn spawn(cwd: &std::path::Path, env: &HashMap<String, String>) -> std::io::Result<Self> {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.current_dir(cwd)
            .env_clear()
            // Don't leak conga's own config/secrets (e.g. CONGA_LLM_KEY).
            .envs(env.iter().filter(|(k, _)| !k.starts_with("CONGA_")))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
        Ok(Self {
            child,
            stdin,
            stdout,
        })
    }
}

type Shared = std::sync::Arc<tokio::sync::Mutex<ShellSession>>;

/// Registry of live shells, keyed by session id. An evicted entry is simply
/// respawned on the next call — a shell is cheap, correctness is not.
fn registry() -> &'static tokio::sync::Mutex<HashMap<String, Shared>> {
    static REG: std::sync::OnceLock<tokio::sync::Mutex<HashMap<String, Shared>>> =
        std::sync::OnceLock::new();
    REG.get_or_init(|| tokio::sync::Mutex::new(HashMap::new()))
}

/// Combined stdout+stderr of one command run through the persistent shell.
pub struct ShellOutcome {
    pub output: String,
    /// Exit status parsed from the sentinel line. `None` when the run did
    /// not complete (timeout, shell death, spawn failure).
    pub exit_code: Option<i32>,
}

impl ShellOutcome {
    fn incomplete(output: String) -> Self {
        Self {
            output,
            exit_code: None,
        }
    }
}
#[allow(clippy::too_many_arguments)]
pub async fn run(
    session_id: &str,
    command: &str,
    timeout: Duration,
    cwd: &std::path::Path,
    env: &HashMap<String, String>,
    background_log_dir: Option<&std::path::Path>,
) -> ShellOutcome {
    // Background mode: redirect to a log file, return immediately after fork.
    let mut log_path: Option<std::path::PathBuf> = None;
    let wrapped: String = match background_log_dir {
        Some(dir) => {
            let _ = std::fs::create_dir_all(dir);
            let log = dir.join(format!("bg-{}.log", millis()));
            log_path = Some(log.clone());
            format!(
                "{{ {command} ; }} > '{}' 2>&1 & printf '%s 0\\n' {SENTINEL}\n",
                log.display()
            )
        }
        None => format!("{{ {command} ; }} 2>&1\nprintf \"%s $?\\n\" {SENTINEL}\n"),
    };

    // Fetch (or respawn) the session; serial commands per session.
    let session = match get_or_spawn(session_id, cwd, env).await {
        Some(s) => s,
        None => {
            return ShellOutcome::incomplete("[shell unavailable: could not spawn sh]".into());
        }
    };
    let mut guard = session.lock().await;

    if guard.stdin.write_all(wrapped.as_bytes()).await.is_err() {
        // Shell died under us: drop, respawn once, retry.
        drop(guard);
        evict(session_id).await;
        let session = match get_or_spawn(session_id, cwd, env).await {
            Some(s) => s,
            None => {
                return ShellOutcome::incomplete("[shell unavailable: could not spawn sh]".into());
            }
        };
        let mut guard = session.lock().await;
        if guard.stdin.write_all(wrapped.as_bytes()).await.is_err() {
            return ShellOutcome::incomplete(
                "[shell unavailable: write failed after respawn]".into(),
            );
        }
        return finish(collect(&mut guard, timeout, session_id).await, log_path);
    }
    finish(collect(&mut guard, timeout, session_id).await, log_path)
}

/// Post-process one outcome: background runs name their log file so the
/// model knows where to poll.
fn finish(mut outcome: ShellOutcome, log_path: Option<std::path::PathBuf>) -> ShellOutcome {
    if let Some(log) = log_path {
        outcome
            .output
            .push_str(&format!("\n[background output -> {}]", log.display()));
    }
    outcome
}

/// Read output until the sentinel line; enforce the timeout. On timeout or
/// shell death the caller's registry entry is evicted (kill_on_drop reaps
/// the process tree once the Arc refs drop).
async fn collect(
    guard: &mut tokio::sync::MutexGuard<'_, ShellSession>,
    timeout: Duration,
    session_id: &str,
) -> ShellOutcome {
    let mut out = String::new();
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let read_one = guard.stdout.read_until(b'\n', &mut buf);
        match tokio::time::timeout(timeout, read_one).await {
            Err(_) => {
                evict(session_id).await;
                return ShellOutcome::incomplete(format!(
                    "{out}\n[exit timeout] command exceeded {}s; shell session was reset",
                    timeout.as_secs()
                ));
            }
            Ok(Ok(0)) => {
                evict(session_id).await;
                return ShellOutcome::incomplete(format!("{out}\n[shell died; session reset]"));
            }
            Ok(Ok(_)) => {
                let line = String::from_utf8_lossy(&buf);
                if let Some(rest) = line.strip_prefix(SENTINEL) {
                    let code = rest.trim();
                    out.push_str(&format!("[exit {code}]"));
                    return ShellOutcome {
                        output: out,
                        exit_code: code.parse().ok(),
                    };
                }
                out.push_str(&line);
            }
            Ok(Err(e)) => {
                evict(session_id).await;
                return ShellOutcome::incomplete(format!(
                    "{out}\n[shell read error: {e}; session reset]"
                ));
            }
        }
    }
}

async fn get_or_spawn(
    session_id: &str,
    cwd: &std::path::Path,
    env: &HashMap<String, String>,
) -> Option<Shared> {
    let mut reg = registry().lock().await;
    if let Some(existing) = reg.get(session_id) {
        return Some(std::sync::Arc::clone(existing));
    }
    let session = ShellSession::spawn(cwd, env).ok()?;
    let shared = std::sync::Arc::new(tokio::sync::Mutex::new(session));
    reg.insert(session_id.to_string(), std::sync::Arc::clone(&shared));
    Some(shared)
}

async fn evict(session_id: &str) {
    // Dropping the registry's Arc lets kill_on_drop reap the tree once the
    // in-flight caller's guard releases.
    registry().lock().await.remove(session_id);
}

/// Kill and forget the session's persistent shell (if any). Public: called
/// on session delete / last-connection close (`session_cleanup`) so a
/// long-running host doesn't accumulate idle shells for dead sessions. A
/// later call for the same id simply respawns.
pub async fn evict_session(session_id: &str) {
    evict(session_id).await;
}

/// Milliseconds since epoch, for background log file names.
fn millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn run_in(session: &str, cmd: &str) -> String {
        run(
            session,
            cmd,
            Duration::from_secs(10),
            std::path::Path::new("/tmp"),
            &HashMap::new(),
            None,
        )
        .await
        .output
    }

    #[tokio::test]
    async fn cwd_persists_across_calls() {
        let out = run_in("t1-cwd", "cd /tmp && pwd").await;
        assert!(out.contains("/tmp"), "{out}");
        let b = run_in("t1-cwd", "pwd").await;
        assert!(b.contains("/tmp"), "cwd must persist: {b}");
    }

    #[tokio::test]
    async fn env_export_persists() {
        run_in("t2-env", "export MYVAR=hello42").await;
        let b = run_in("t2-env", "printf %s $MYVAR").await;
        assert!(b.contains("hello42"), "env must persist: {b}");
    }

    #[tokio::test]
    async fn timeout_resets_session() {
        let out = run(
            "t3-timeout",
            "sleep 30",
            Duration::from_secs(1),
            std::path::Path::new("/tmp"),
            &HashMap::new(),
            None,
        )
        .await
        .output;
        assert!(out.contains("shell session was reset"), "{out}");
        let b = run_in("t3-timeout", "echo alive").await;
        assert!(
            b.contains("alive"),
            "session must be usable after reset: {b}"
        );
    }

    #[tokio::test]
    async fn background_returns_and_logs() {
        let dir = std::env::temp_dir().join("conga-bg-test-unique");
        let _ = std::fs::remove_dir_all(&dir);
        let out = run(
            "t4-bg",
            "echo delayed-output",
            Duration::from_secs(5),
            std::path::Path::new("/tmp"),
            &HashMap::new(),
            Some(&dir),
        )
        .await
        .output;
        assert!(out.contains("bg-"), "must reference the log name: {out}");
        assert!(out.contains("[exit 0]"), "{out}");
        tokio::time::sleep(Duration::from_millis(300)).await;
        let logged = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .any(|e| {
                std::fs::read_to_string(e.path())
                    .map(|c| c.contains("delayed-output"))
                    .unwrap_or(false)
            });
        assert!(logged, "background log must capture output");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn exit_status_reported() {
        // `exit N` in the top-level command would terminate the persistent
        // shell itself; an inner sh reports the code and the shell survives.
        let out = run_in("t5-exit", "sh -c 'exit 3'").await;
        assert!(out.contains("[exit 3]"), "{out}");
    }

    #[tokio::test]
    async fn mirror_of_bash_tool_parameters() {
        // Exact parameters the bash tool passes: tempdir cwd, full process
        // env, uuid session, default timeout. Isolates shell.rs from the
        // bash.rs glue.
        let tmp = tempfile::tempdir().unwrap();
        let session = format!("bash-mirror-{}", uuid::Uuid::new_v4());
        let out = run(
            &session,
            "echo hello",
            Duration::from_secs(120),
            tmp.path(),
            &std::env::vars().collect(),
            None,
        )
        .await
        .output;
        assert!(out.contains("hello"), "{out:?}");
        assert!(out.contains("[exit 0]"), "{out:?}");
    }

    #[tokio::test]
    async fn evict_session_removes_entry_and_respawns_clean() {
        run_in("t7-evict", "cd /").await;
        assert!(registry().lock().await.contains_key("t7-evict"));
        evict_session("t7-evict").await;
        assert!(
            !registry().lock().await.contains_key("t7-evict"),
            "evict must remove the registry entry"
        );
        // Fresh shell: the `cd /` must NOT have survived the eviction
        // (spawn cwd is back; on macOS /tmp may print as /private/tmp,
        // so assert the property that actually matters: not "/").
        let b = run_in("t7-evict", "pwd").await;
        assert!(b.contains("[exit 0]"), "respawn must work: {b}");
        assert_ne!(
            b.lines().next().unwrap().trim(),
            "/",
            "cwd must reset after eviction: {b}"
        );
    }

    #[tokio::test]
    async fn secrets_stripped_from_env() {
        let mut env = HashMap::new();
        env.insert("CONGA_LLM_KEY".to_string(), "sk-secret".to_string());
        env.insert("NORMAL_VAR".to_string(), "ok".to_string());
        let out = run(
            "t6-secrets",
            "printf %s \"$CONGA_LLM_KEY|$NORMAL_VAR\"",
            Duration::from_secs(5),
            std::path::Path::new("/tmp"),
            &env,
            None,
        )
        .await
        .output;
        assert!(out.contains("|ok"), "normal var must survive: {out}");
        assert!(
            !out.contains("sk-secret"),
            "CONGA_* must be stripped: {out}"
        );
    }
}
