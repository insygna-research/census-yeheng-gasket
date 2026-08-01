//! gasket-host - 可复用的 host 层（配置/session/权限/事件渲染/压缩/外部工具）。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use gasket_core::{AgentMessage, StreamFn, ToolDefinition};

pub mod compact;
pub mod config;
pub mod external_tool;
pub mod hooks;
pub mod permission;
pub mod printer;
pub mod session;

pub use compact::{compact_by_count, max_messages_from_env, ContextBudget, DEFAULT_MAX_MESSAGES};
pub use config::{ConfigLoader, HostConfig, TurnInputs};
pub use external_tool::{commands_from_env, load_all as load_external_tools, ExternalToolBridge};
pub use hooks::HookStack;
pub use permission::{Mode, PermissionPolicy, RiskLevel};
pub use printer::EventPrinter;
pub use session::{SessionInfo, SessionManager};

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
/// `Host` does **not** own a printer/writer — rendering goes through the
/// `on_event` callback of `run_turn`, so non-terminal frontends can drive
/// the same code.
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
            cfg,
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

    /// One user message through the whole pipeline:
    /// build context → build loop config → run the agent loop → persist.
    ///
    /// `history` is the caller-owned transcript: it is cloned into the
    /// context for this run, and the caller extends it with the returned
    /// messages afterwards. The run's new messages are appended to the
    /// session store **only on success** — a failed run writes no partial
    /// transcript.
    ///
    /// `on_event` receives every [`AgentEvent`](gasket_core::AgentEvent)
    /// as it happens (streaming text, tool calls, usage, errors).
    pub async fn run_turn<E>(
        &mut self,
        user_msg: AgentMessage,
        history: &[AgentMessage],
        on_event: E,
    ) -> Result<Vec<AgentMessage>, HostError>
    where
        E: FnMut(gasket_core::AgentEvent),
    {
        let (context, config) = self.cfg.prepare_turn(
            TurnInputs {
                system_prompt: &self.system_prompt,
                history,
                tools: &self.tools,
                cwd: &self.cwd,
                session_id: self.session.current_id(),
            },
            &self.signal,
            self.hooks.clone(),
            self.stream_fn.clone(),
            self.max_turns,
        );
        let new_msgs =
            gasket_core::run_agent_loop(vec![user_msg], context, config, on_event).await?;
        self.session.append(&new_msgs).await?;
        Ok(new_msgs)
    }
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
        assert_eq!(PermissionPolicy::risk_of("bash"), RiskLevel::High);
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
}
