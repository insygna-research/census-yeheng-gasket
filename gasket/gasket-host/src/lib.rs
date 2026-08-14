//! gasket-host - 可复用的 host 层（配置/session/权限/事件渲染/压缩/外部工具）。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use gasket_core::{
    derive_messages, AgentError, AgentEvent, AgentMessage, CancelCause, ContentBlock, SessionEvent,
    StopReason, StreamFn, ToolDefinition, TurnEndReason, UserMessage,
};

pub mod approval;
pub mod compact;
pub mod config;
pub mod event_map;
pub mod external_tool;
pub mod hooks;
pub mod mcp;
pub mod permission;
pub mod printer;
pub mod session;
pub mod subagent;
pub mod wire;

pub use compact::{compact_by_count, max_messages_from_env, ContextBudget, DEFAULT_MAX_MESSAGES};
pub use config::{ConfigLoader, HostConfig, TurnInputs};
pub use external_tool::{commands_from_env, load_all as load_external_tools, ExternalToolBridge};
pub use gasket_core::RiskLevel;
pub use hooks::HookStack;
pub use mcp::{load_all_mcp, McpBridge, McpError, McpServerConfig};
pub use permission::{Mode, PermissionPolicy};
pub use printer::EventPrinter;
pub use session::{SessionInfo, SessionManager};
pub use subagent::HostSubagentSpawner;

#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("config error: {0}")]
    Config(#[from] gasket_core::ConfigError),
    #[error("session error: {0}")]
    Session(String),
    #[error("agent error: {0}")]
    Agent(#[from] gasket_core::AgentError),
}

/// Assemble config/session/policy/loop into one driver. Hosts hold one
/// `Host` and call [`run_turn`](Host::run_turn) per user message; CLI,
/// smoke tests, and future frontends (a2a, channels) reuse the same path.
///
pub struct Host {
    cfg: HostConfig,
    session: SessionManager,
    policy: Arc<PermissionPolicy>,
    hooks: Arc<dyn gasket_core::HookChain>,
    signal: Arc<AtomicBool>,
    stream_fn: Arc<dyn StreamFn>,
    system_prompt: String,
    tools: Vec<ToolDefinition>,
    cwd: PathBuf,
    max_turns: usize,
    /// Compaction knobs. The token count itself is NOT kept here: every turn
    /// re-derives history from the log and restores the last persisted
    /// assistant usage from its tail, so token-aware compaction survives
    /// restarts by construction.
    budget: ContextBudget,
    /// Subagent spawner — built lazily; injected into AgentContext so the
    /// `spawn_subagents` tool can use it.
    spawner: Option<Arc<dyn gasket_core::SubagentSpawner>>,
    /// Turn serialization slot: run_turn acquires it on entry and releases
    /// it on completion or drop. The event log's format contract assumes a
    /// single writer per session, and one Host drives one session — so a
    /// second concurrent turn is rejected outright instead of interleaving
    /// two event streams into one log.
    turn_in_flight: AtomicBool,
}

impl Host {
    /// Build a host from a loaded config. `stream_fn` starts as the
    /// provider's own; tests override it with [`with_stream_fn`](Self::with_stream_fn).
    ///
    /// `policy` is the shared instance: it is the default hook chain AND what
    /// [`policy`](Self::policy) exposes, so hosts that compose extra gates on
    /// top (via [`with_hooks`](Self::with_hooks)) still mutate the live policy
    /// through the accessor (e.g. the REPL's `/mode`).
    pub fn new(
        cfg: HostConfig,
        session: SessionManager,
        policy: Arc<PermissionPolicy>,
        system_prompt: String,
        tools: Vec<ToolDefinition>,
    ) -> Self {
        let hooks: Arc<dyn gasket_core::HookChain> = Arc::new(HookStack::new(vec![policy.clone()]));
        // cwd failure is practically impossible (the process runs from it);
        // falling back to `.` keeps tools resolving relative to the same place.
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self {
            cwd,
            signal: Arc::new(AtomicBool::new(false)),
            stream_fn: cfg.provider_stream_fn(),
            max_turns: cfg.tunables.max_turns,
            budget: ContextBudget::from_env(),
            spawner: None,
            cfg,
            turn_in_flight: AtomicBool::new(false),
            session,
            policy,
            hooks,
            system_prompt,
            tools,
        }
    }

    /// Replace the provider stream_fn with a fake (tests) or custom one.
    pub fn with_stream_fn(mut self, stream_fn: Arc<dyn StreamFn>) -> Self {
        self.stream_fn = stream_fn;
        self
    }

    /// Override the compaction budget (tests inject knobs without touching
    /// the process environment).
    pub fn with_budget(mut self, budget: ContextBudget) -> Self {
        self.budget = budget;
        self
    }

    /// Override the per-turn turn ceiling (default: the tunables value).
    pub fn with_max_turns(mut self, n: usize) -> Self {
        self.max_turns = n;
        self
    }

    /// Replace the hook chain (default: the permission policy alone). Hosts
    /// that stack extension gates before the policy pass the composed stack.
    pub fn with_hooks(mut self, hooks: Arc<dyn gasket_core::HookChain>) -> Self {
        self.hooks = hooks;
        self
    }

    /// Inject a subagent spawner. Without this, the `spawn_subagents` tool
    /// returns an "unavailable" error. CLI/gateway pass a `HostSubagentSpawner`
    /// built from the host's config.
    pub fn with_spawner(mut self, spawner: Arc<dyn gasket_core::SubagentSpawner>) -> Self {
        self.spawner = Some(spawner);
        self
    }

    /// Replace the tool list (used by `/reload-tools`).
    pub fn set_tools(&mut self, tools: Vec<ToolDefinition>) {
        self.tools = tools;
    }

    pub fn session(&self) -> &SessionManager {
        &self.session
    }

    pub fn session_mut(&mut self) -> &mut SessionManager {
        &mut self.session
    }

    pub fn policy(&self) -> &PermissionPolicy {
        &self.policy
    }

    /// The shared abort flag. `install_ctrl_c` wants the Arc; callers that
    /// only read/write the flag can deref it (`signal().load(…)`).
    pub fn signal(&self) -> &Arc<AtomicBool> {
        &self.signal
    }

    /// One user message through the whole event-sourced pipeline.
    ///
    /// The log is the only truth: history is *derived* from
    /// `events.jsonl` (never carried by the caller), the turn is framed by
    /// `TurnStart`/`User`/`TurnEnd` writes, and the agent loop persists every
    /// Assistant/ToolResult as it happens via the injected persist closure —
    /// so a crash or failed turn keeps every side effect that already
    /// happened, instead of rolling back to a pre-turn transcript.
    ///
    /// Flow: persist(TurnStart) → persist(User) → history =
    /// derive_messages(load_events) → budget.compact(&history) (in-memory
    /// only; the log stays append-only full) → prepare_turn →
    /// run_agent_loop → persist(TurnEnd{reason}) → [`TurnSummary`].
    ///
    /// `on_event` receives every [`AgentEvent`](gasket_core::AgentEvent)
    /// as it happens (streaming text, tool calls, usage, errors).
    pub async fn run_turn<E>(
        &self,
        user_msg: &str,
        mut on_event: E,
    ) -> Result<TurnSummary, AgentError>
    where
        E: FnMut(AgentEvent) + Send,
    {
        // Turns are serialized per Host (single-writer event log). The
        // guard is a Drop release: a dropped/cancelled turn still frees the
        // host, so a cancelled connection cannot deadlock the next one.
        if self.turn_in_flight.swap(true, Ordering::AcqRel) {
            return Err(AgentError::TurnInProgress);
        }
        let _turn_guard = TurnGuard(&self.turn_in_flight);

        let sid = self.session.current_id().to_string();
        // Open (migrating a legacy transcript once, if any), fail closed on
        // corruption, then frame the turn.
        let events = self.session.open_or_migrate(&sid).await?;
        self.session.append_event(&SessionEvent::TurnStart).await?;
        let user = AgentMessage::User(UserMessage {
            content: vec![ContentBlock::text(user_msg.to_string())],
            timestamp: gasket_core::now(),
        });
        self.session
            .append_event(&SessionEvent::User(user.clone()))
            .await?;

        // Restore the token budget from the log tail (the last persisted
        // assistant usage), then shrink the derived working copy in memory.
        let mut budget = self.budget.clone();
        if let Some(input_tokens) = events.iter().rev().find_map(|ev| match ev {
            SessionEvent::Assistant { usage, .. } => usage.as_ref().map(|u| u.input_tokens),
            _ => None,
        }) {
            budget.record_input_tokens(input_tokens);
        }
        let history = budget.compact(&derive_messages(&events));

        let (mut context, config) = self.cfg.prepare_turn(
            TurnInputs {
                system_prompt: &self.system_prompt,
                history: &history,
                tools: &self.tools,
                cwd: &self.cwd,
                session_id: &sid,
            },
            &self.signal,
            self.hooks.clone(),
            self.stream_fn.clone(),
            self.max_turns,
            Some(self.session.persist_fn()),
        );
        // Inject the subagent spawner if the host has one configured.
        if let Some(sp) = &self.spawner {
            context.spawner = Some(Arc::clone(sp));
        }

        let outcome = gasket_core::run_agent_loop(vec![user], context, config, |ev| {
            on_event(ev);
        })
        .await;

        // Ok + no signal = Completed; Ok + signal = Aborted; Err = Error.
        // A surfaced provider error (stop_reason::Error on the closing
        // assistant) counts as an errored turn.
        let reason = match &outcome {
            Err(e) => TurnEndReason::Error {
                message: e.to_string(),
            },
            Ok(msgs) => {
                if self.signal.load(Ordering::Relaxed) {
                    TurnEndReason::Aborted {
                        cause: Some(CancelCause::User),
                    }
                } else if let Some(AgentMessage::Assistant(a)) = msgs
                    .iter()
                    .rev()
                    .find(|m| matches!(m, AgentMessage::Assistant(_)))
                {
                    match &a.stop_reason {
                        StopReason::Error(message) => TurnEndReason::Error {
                            message: message.clone(),
                        },
                        _ => TurnEndReason::Completed,
                    }
                } else {
                    TurnEndReason::Completed
                }
            }
        };
        // The TurnEnd marker is best-effort: appending it must never shadow
        // the loop's own outcome. If the write fails (disk full, permission,
        // …) we log and carry on, returning the loop's result/error as-is —
        // derive_messages tolerates a missing trailing TurnEnd, so the next
        // open still reconstructs a coherent history.
        if let Err(e) = self
            .session
            .append_event(&SessionEvent::TurnEnd {
                reason: reason.clone(),
            })
            .await
        {
            tracing::warn!(error = %e, "failed to append TurnEnd marker; loop outcome preserved");
        }

        outcome.map(|new_messages| TurnSummary {
            reason,
            new_messages,
        })
    }
}

/// Releases the per-Host turn slot when the turn future completes *or* is
/// dropped (ws close, cancellation): run_turn is cancel-safe, so a dropped
/// turn must not leave the host permanently busy.
struct TurnGuard<'a>(&'a AtomicBool);

impl Drop for TurnGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// What one `run_turn` produced. The event log remains the source of truth;
/// `new_messages` is the loop's returned slice for UI/stats convenience.
#[derive(Debug)]
pub struct TurnSummary {
    pub reason: TurnEndReason,
    pub new_messages: Vec<AgentMessage>,
}

/// Install a SIGINT handler that sets `signal` true (cooperative abort).
/// Every press is honored; callers reset the flag via
/// [`run_turn`](Host::run_turn) or `signal().store(false, …)` before a turn.
pub fn install_ctrl_c(signal: Arc<AtomicBool>) {
    tokio::spawn(async move {
        loop {
            if tokio::signal::ctrl_c().await.is_err() {
                break; // handler could not be installed; give up quietly
            }
            signal.store(true, Ordering::Relaxed);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn fake_env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Result<String, std::env::VarError> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| map.get(k).cloned().ok_or(std::env::VarError::NotPresent)
    }

    fn test_cfg() -> HostConfig {
        ConfigLoader::load_with(&fake_env(&[
            ("GASKET_LLM_BASE_URL", "https://api.x.com/v1"),
            ("GASKET_LLM_KEY", "sk-test"),
            ("GASKET_LLM_MODEL", "m"),
        ]))
        .unwrap()
    }

    #[test]
    fn host_construction_and_accessors() {
        let tmp = tempfile::tempdir().unwrap();
        let session = SessionManager::with_root(tmp.path().to_path_buf());
        let mut host = Host::new(
            test_cfg(),
            session,
            Arc::new(PermissionPolicy::new(
                Mode::FullAuto,
                Arc::new(|_, _| Box::pin(async { false })),
            )),
            "sys".into(),
            vec![],
        );
        assert!(!host.session().current_id().is_empty());
        assert!(!host.signal().load(Ordering::Relaxed));

        host.set_tools(vec![]); // compile check: setter exists
        let _ = host
            .with_stream_fn(Arc::new(FakeProvider))
            .with_max_turns(1);
    }
    #[test]
    fn host_error_from_agent_error() {
        let e: HostError = gasket_core::AgentError::ToolNotFound("x".into()).into();
        assert!(e.to_string().contains("x"));
    }

    /// Minimal StreamFn for construction tests (never called).
    struct FakeProvider;
    impl StreamFn for FakeProvider {
        fn stream(
            &self,
            _model: &gasket_core::ModelSpec,
            _messages: &[AgentMessage],
            _system: &str,
            _tools: &[ToolDefinition],
            _signal: Option<Arc<AtomicBool>>,
        ) -> std::pin::Pin<Box<dyn futures_util::Stream<Item = gasket_core::StreamChunk> + Send>>
        {
            unreachable!("not called in construction tests")
        }
    }

    /// StreamFn that never yields: keeps a turn in flight on demand.
    struct PendingProvider;
    impl StreamFn for PendingProvider {
        fn stream(
            &self,
            _model: &gasket_core::ModelSpec,
            _messages: &[AgentMessage],
            _system: &str,
            _tools: &[ToolDefinition],
            _signal: Option<Arc<AtomicBool>>,
        ) -> std::pin::Pin<Box<dyn futures_util::Stream<Item = gasket_core::StreamChunk> + Send>>
        {
            Box::pin(futures_util::stream::pending())
        }
    }

    fn test_host() -> Host {
        let tmp = tempfile::tempdir().unwrap();
        let session = SessionManager::with_root(tmp.path().to_path_buf());
        Host::new(
            test_cfg(),
            session,
            Arc::new(PermissionPolicy::new(
                Mode::FullAuto,
                Arc::new(|_, _| Box::pin(async { true })),
            )),
            "sys".into(),
            vec![],
        )
        .with_stream_fn(Arc::new(PendingProvider))
    }

    /// StreamFn that emits one text delta then a stream error. Because content
    /// was already emitted, the loop does not retry and returns an assistant
    /// with `stop_reason::Error` (outcome == `Ok`).
    struct ErroringProvider;
    impl StreamFn for ErroringProvider {
        fn stream(
            &self,
            _: &gasket_core::ModelSpec,
            _: &[AgentMessage],
            _: &str,
            _: &[ToolDefinition],
            _: Option<Arc<AtomicBool>>,
        ) -> std::pin::Pin<Box<dyn futures_util::Stream<Item = gasket_core::StreamChunk> + Send>>
        {
            Box::pin(futures_util::stream::iter([
                gasket_core::StreamChunk::TextDelta("partial".into()),
                gasket_core::StreamChunk::Error("provider-boom".into()),
            ]))
        }
    }

    #[tokio::test]
    async fn run_turn_preserves_loop_outcome_when_turnend_append_fails() {
        // The agent loop emits its own AgentEvent::TurnEnd AFTER the
        // (errored) assistant is persisted but BEFORE run_turn appends the
        // SessionEvent::TurnEnd marker. Replacing the log with a directory
        // at that point makes ONLY that final append fail. The loop's outcome
        // (the provider error, surfaced as TurnEndReason::Error) must still
        // be returned — the storage error must not shadow it. Pre-fix, the
        // `.await?` on the TurnEnd append returned the storage error instead.
        let tmp = tempfile::tempdir().unwrap();
        let session = SessionManager::with_root(tmp.path().to_path_buf());
        let events_path = tmp.path().join(session.current_id()).join("events.jsonl");
        let host = Host::new(
            test_cfg(),
            session,
            Arc::new(PermissionPolicy::new(
                Mode::FullAuto,
                Arc::new(|_, _| Box::pin(async { true })),
            )),
            "sys".into(),
            vec![],
        )
        .with_stream_fn(Arc::new(ErroringProvider));

        let summary = host
            .run_turn("hi", move |ev| {
                if matches!(ev, AgentEvent::TurnEnd { .. }) {
                    // Only the trailing SessionEvent::TurnEnd append should
                    // fail; the assistant was already persisted above this.
                    let _ = std::fs::remove_file(&events_path);
                    let _ = std::fs::create_dir(&events_path);
                }
            })
            .await
            .expect("loop outcome must be returned, not the storage error");

        assert_eq!(
            summary.reason,
            TurnEndReason::Error {
                message: "provider-boom".into(),
            }
        );
    }

    #[tokio::test]
    async fn run_turn_is_serialized_per_host() {
        let host = test_host();
        // Poll the first turn once: the guard is acquired synchronously at
        // entry, before any await.
        let mut first = Box::pin(host.run_turn("hello", |_ev| {}));
        assert!(futures_util::poll!(first.as_mut()).is_pending());

        // A second concurrent turn on the same Host is rejected outright.
        match host.run_turn("again", |_ev| {}).await {
            Err(e) => assert!(e.to_string().contains("already running"), "{e}"),
            Ok(_) => panic!("second concurrent turn must be rejected"),
        }

        // Dropping the in-flight turn releases the slot (Drop guard): a
        // fresh turn starts — it just never finishes on the pending
        // provider, so the timeout elapsing is the pass condition.
        drop(first);
        let mut third = Box::pin(host.run_turn("third", |_ev| {}));
        let raced =
            tokio::time::timeout(std::time::Duration::from_millis(200), third.as_mut()).await;
        assert!(
            raced.is_err(),
            "third turn should be in flight, not rejected"
        );
    }
}
