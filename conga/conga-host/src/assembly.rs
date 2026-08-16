//! Shared Host assembly for server-style transports (the gateway's
//! WebSocket, the desktop app's IPC).
//!
//! ONE place wires: fail-loud session resume → system prompt + skills →
//! permission mode → approver (registry + cancel watch + transport emit) →
//! tool set (built-in + external + MCP + transport extras) → sub-agent
//! spawner → `Host`. Transports keep only their channel/emitter plumbing.
//! Before this module existed, the gateway and the desktop backend each
//! hand-copied this wiring and the copies drifted (the desktop `/clear`
//! stopped rotating the log; the desktop missed `policy.set_signal`).

use std::path::Path;
use std::sync::{Arc, Mutex as StdMutex};

use conga::ToolDefinition;

use crate::approval::{self, ApprovalRegistry, RegisterOutcome};
use crate::permission::{Approver, Mode, PermissionPolicy};
use crate::subagent::HostSubagentSpawner;
use crate::subagent_types::SubagentEvent;
use crate::{ConfigLoader, HookStack, Host, SessionManager};

/// A session-API failure for transport mapping: `Config` (provider/env
/// setup broken — the user must fix `~/.conga` or env) vs `Session` (this
/// session's log refuses to load — corruption fails closed). Transports
/// render `to_string()`; the variants exist so future callers can act on
/// the class instead of parsing prose.
#[derive(Debug)]
pub enum AssemblyError {
    Config(String),
    Session(String),
}

impl std::fmt::Display for AssemblyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AssemblyError::Config(m) => write!(f, "Config error: {m}"),
            AssemblyError::Session(m) => write!(f, "Session error: {m}"),
        }
    }
}

/// Where an approval request goes when a tool needs human consent:
/// `(request_id, tool_name, args)`. Transports forward it onto their
/// ordered event channel so a request can never overtake the
/// `tool_start` event of the call it belongs to.
pub type ApprovalEmit = Arc<dyn Fn(String, String, serde_json::Value) + Send + Sync>;

/// Where sub-agent events go. Transports forward onto their ordered event
/// channel (the same one as approvals and main-agent stream events).
pub type SubagentEmit = Arc<dyn Fn(SubagentEvent) + Send + Sync>;

/// A fully wired session: the transport drives `host.run_turn` per user
/// message, fills in approval decisions on `registry`, and cancels via the
/// Host's abort flag plus `cancel_tx`.
pub struct SessionAssembly {
    pub host: Host,
    /// In-flight approvals for this session. `approval_response` from the
    /// transport fills in decisions; the turn boundary calls
    /// `clear_pending`.
    pub registry: Arc<StdMutex<ApprovalRegistry>>,
    /// Cancel broadcast: sending `true` unlocks any approval still waiting
    /// on a decision (the Host's abort flag stops the agent loop itself).
    pub cancel_tx: tokio::sync::watch::Sender<bool>,
}

/// Fail-loud resume of a session's event log (the config-independent step,
/// so tests can exercise corruption refusal without provider config).
/// Corruption is an `Err`, never adopt-and-restart over a damaged log.
pub async fn resume_session(
    store_root: &Path,
    session_id: &str,
) -> Result<SessionManager, AssemblyError> {
    let mgr = SessionManager::with_root(store_root.to_path_buf());
    match mgr.resume(session_id).await {
        Ok(history) => {
            if !history.is_empty() {
                tracing::info!(
                    "session {session_id}: resumed {} msgs (event log)",
                    history.len()
                );
            }
            Ok(mgr)
        }
        Err(e) => Err(AssemblyError::Session(e.to_string())),
    }
}

impl SessionAssembly {
    /// Assemble one session's `Host` exactly as every server transport
    /// does. `extra_tools` are appended after built-in + external + MCP
    /// (the desktop app adds its in-process extension tools there).
    /// `Err` class is [`AssemblyError`] (Config vs Session); transports
    /// render `to_string()` directly for the user.
    pub async fn build(
        store_root: &Path,
        session_id: &str,
        extra_tools: Vec<ToolDefinition>,
        approval_emit: ApprovalEmit,
        subagent_emit: SubagentEmit,
    ) -> Result<Self, AssemblyError> {
        let cfg = match ConfigLoader::load() {
            Ok(c) => c,
            Err(e) => return Err(AssemblyError::Config(e.to_string())),
        };
        let session_mgr = resume_session(store_root, session_id).await?;

        let cwd = crate::project_dir();
        let system_prompt = crate::append_skills("You are a helpful, concise assistant.", &cwd);
        // Intentionally one env knob shared by every transport.
        let mode = std::env::var("CONGA_GATEWAY_MODE")
            .ok()
            .and_then(|s| Mode::parse(&s))
            .unwrap_or(Mode::AutoEdit);

        // Cancel 双通道：Host 的 AtomicBool 驱动 loop 中止，watch 解锁挂起的审批。
        let (cancel_tx, _cancel_rx) = tokio::sync::watch::channel(false);
        let registry = Arc::new(StdMutex::new(ApprovalRegistry::new()));
        let approver: Approver = {
            let registry = Arc::clone(&registry);
            let cancel_tx = cancel_tx.clone();
            let emit = approval_emit.clone();
            Arc::new(move |tool_name: &str, args: &serde_json::Value| {
                let registry = Arc::clone(&registry);
                let cancel_tx = cancel_tx.clone();
                let emit = Arc::clone(&emit);
                Box::pin(async move {
                    let outcome = { registry.lock().unwrap().register(tool_name) };
                    let (request_id, rx) = match outcome {
                        RegisterOutcome::Remembered(v) => return v,
                        RegisterOutcome::Pending { request_id, rx } => (request_id, rx),
                    };
                    emit(request_id.clone(), tool_name.to_string(), args.clone());
                    let timeout_s = std::env::var("CONGA_APPROVAL_TIMEOUT_S")
                        .ok()
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(300u64);
                    // subscribe() 把当前值标记为已见：只有将来的 send 才命中
                    // changed()，本连接的第一次 cancel 不会毒化后续所有审批
                    // （见 approval.rs 的 wait_for_decision 测试）。
                    approval::wait_for_decision(
                        rx,
                        cancel_tx.subscribe(),
                        std::time::Duration::from_secs(timeout_s),
                    )
                    .await
                })
            })
        };
        let policy = Arc::new(PermissionPolicy::new(mode, approver));

        // Tool assembly: built-in + external + MCP (+ transport extras) for
        // the parent; sub-agents get the built-in set minus
        // `spawn_subagents` (nesting disabled; shared MCP/external servers
        // are not built for N parallel loops). The shared permission policy
        // still gates every call the sub-agents do get.
        let external = {
            let cmds = crate::commands_from_env();
            if cmds.is_empty() {
                Vec::new()
            } else {
                match crate::load_external_tools(&cmds).await {
                    Ok(t) => {
                        tracing::info!("loaded {} external tool(s)", t.len());
                        t
                    }
                    Err(e) => {
                        tracing::warn!("external tools load failed: {e}");
                        Vec::new()
                    }
                }
            }
        };
        let mcp_tools = crate::mcp::load_all_mcp().await;
        let built_in = crate::built_in_tools();
        let subagent_tools: Vec<_> = built_in
            .iter()
            .filter(|t| t.name != "spawn_subagents")
            .cloned()
            .collect();
        let mut tools = built_in;
        tools.extend(external);
        tools.extend(mcp_tools);
        tools.extend(extra_tools);

        let spawner_cfg = cfg;
        let spawner_policy = Arc::clone(&policy);
        let mut host =
            Host::new(spawner_cfg.clone(), session_mgr, policy.clone(), system_prompt, tools);
        // The approver may wait on a client that never answers; give it the
        // Host's abort signal so cancel unwinds the wait. (The desktop
        // backend used to miss this line — it lived only in the gateway's
        // copy of this wiring.)
        policy.set_signal(host.signal().clone());
        {
            let spawner_signal = host.signal().clone();
            let spawner_stream_fn = spawner_cfg.provider_stream_fn();
            let spawner_hooks: Arc<dyn conga::HookChain> =
                Arc::new(HookStack::new(vec![spawner_policy]));
            let loop_config = spawner_cfg.build_loop_config(
                spawner_cfg.tunables.max_turns,
                Some(spawner_signal.clone()),
                None,
                spawner_stream_fn,
            );
            let spawner = Arc::new(
                HostSubagentSpawner::new(
                    "You are a focused sub-agent. Complete your assigned task concisely.".into(),
                    subagent_tools,
                    spawner_hooks,
                    spawner_signal,
                    crate::project_dir(),
                    loop_config,
                )
                .with_ws_emit(subagent_emit),
            );
            host = host.with_spawner(spawner);
        }
        Ok(Self {
            host,
            registry,
            cancel_tx,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mid-file row this reader does not understand (a `Data` error, not
    /// a torn tail) must refuse the session — never a silent adopt.
    /// (Ported from the gateway's ws.rs so the contract lives with the
    /// implementation.)
    #[tokio::test]
    async fn resume_session_fails_loud_on_corrupt_log() {
        let tmp = tempfile::tempdir().unwrap();
        let id = "corrupt-sess";
        let dir = tmp.path().join(id);
        std::fs::create_dir_all(&dir).unwrap();
        let good = serde_json::to_string(&conga::SessionEvent::TurnStart).unwrap();
        let body = format!("{good}\n{{\"type\":\"from_the_future\"}}\n{good}\n");
        std::fs::write(dir.join("events.jsonl"), body).unwrap();

        let err = resume_session(tmp.path(), id)
            .await
            .err()
            .expect("corrupt log must error");
        let msg = err.to_string();
        assert!(
            msg.contains("from_the_future") || msg.contains("invalid"),
            "{msg}"
        );
    }

    #[tokio::test]
    async fn resume_session_adopts_fresh_and_existing_ids() {
        let tmp = tempfile::tempdir().unwrap();
        // Fresh: no log, no legacy file — a brand-new session, not an error.
        let mgr = resume_session(tmp.path(), "fresh-sess").await.unwrap();
        assert_eq!(mgr.current_id(), "fresh-sess");

        // Existing: log on disk is loaded and the id adopted.
        conga::EventStorage::new(tmp.path().to_path_buf())
            .append_event("has-log", &conga::SessionEvent::TurnStart)
            .await
            .unwrap();
        let mgr = resume_session(tmp.path(), "has-log").await.unwrap();
        assert_eq!(mgr.current_id(), "has-log");
    }
}
