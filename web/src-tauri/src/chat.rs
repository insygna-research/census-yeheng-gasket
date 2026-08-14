//! In-process chat transport for the desktop app.
//!
//! Mirrors the gateway's per-connection session loop (gasket-gateway ws.rs):
//! one `Host` per session, assembled from the same config/env knobs, with the
//! same cancellation and approval semantics. The only difference is the
//! transport — instead of WebSocket frames, turn events are emitted as Tauri
//! `chat-event` IPC events whose `event` payload is byte-for-byte the
//! gateway's wire JSON (host-owned schema, `gasket_host::wire`), so the
//! frontend's message handling is shared unchanged between both transports.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use dashmap::DashMap;
use log::{info, warn};
use tauri::{AppHandle, Emitter, State};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use gasket_core::{built_in_tools, AgentEvent};
use gasket_host::approval::{self, ApprovalRegistry, RegisterOutcome};
use gasket_host::event_map::{event_to_ws, subagent_event_to_ws};
use gasket_host::permission::Approver;
use gasket_host::wire::OutgoingEvent;
use gasket_host::{
  load_all_mcp, ConfigLoader, Host, HostSubagentSpawner, Mode, PermissionPolicy, SessionManager,
};

/// Tauri event channel carrying every chat event. Payload: [`ChatEventPayload`].
pub const CHAT_EVENT: &str = "chat-event";

/// IPC payload: the gateway-protocol event plus the session it belongs to, so
/// one broadcast channel can multiplex many sessions (WS had a socket per
/// session instead).
#[derive(Clone, serde::Serialize)]
struct ChatEventPayload {
  session_id: String,
  event: serde_json::Value,
}

/// Same internal queue as the gateway's `WireEvent`: everything the frontend
/// sees flows through ONE ordered channel with a single emitter task, so a
/// turn-boundary `done` can never overtake the last subagent event and an
/// approval request cannot overtake the tool_start it belongs to.
enum WireEvent {
  Agent(AgentEvent),
  Subagent(gasket_core::SubagentEvent),
  Approval {
    request_id: String,
    tool_name: String,
    args: serde_json::Value,
  },
  /// Reply to a message received while a turn is already running.
  Busy(String),
  Done,
  Error(String),
}

/// Per-session state: the Host (which owns the on-disk event log via its
/// SessionManager) plus the cross-command knobs (cancel, approvals). The
/// transcript itself is never mirrored in memory — the event log is the
/// single source of truth.
struct ChatSession {
  host: Host,
  /// Turn serialization: mirrors the gateway's one-turn-per-connection
  /// behavior (a second message while a turn runs gets a `busy` reply).
  /// Host::run_turn's own TurnInProgress rejection is the backstop.
  turn_active: AtomicBool,
  /// Unlocks pending approval waits on cancel. Fresh `subscribe()` receivers
  /// per approval avoid the cancel-latch poisoning (see approval.rs tests).
  cancel_tx: tokio::sync::watch::Sender<bool>,
  /// Shared with the approver closure baked into the Host's policy —
  /// `approval_response` must fill in decisions on THIS registry.
  registry: Arc<Mutex<ApprovalRegistry>>,
  wire_tx: UnboundedSender<WireEvent>,
}

#[derive(Default)]
pub struct ChatState {
  sessions: DashMap<String, Arc<ChatSession>>,
  /// Serializes session construction: two concurrent first messages for the
  /// same session must not race the on-disk resume/migration.
  create_lock: tokio::sync::Mutex<()>,
  store_root: PathBuf,
}

impl ChatState {
  pub fn new() -> Self {
    Self {
      store_root: gasket_core::JsonlStorage::default_root().base_dir_clone(),
      ..Default::default()
    }
  }

  async fn get_or_create(
    &self,
    app: &AppHandle,
    session_id: &str,
  ) -> Result<Arc<ChatSession>, String> {
    if let Some(s) = self.sessions.get(session_id) {
      return Ok(Arc::clone(&s));
    }
    let _guard = self.create_lock.lock().await;
    if let Some(s) = self.sessions.get(session_id) {
      return Ok(Arc::clone(&s));
    }
    let session = build_session(app, &self.store_root, session_id).await?;
    self.sessions.insert(session_id.to_string(), session.clone());
    Ok(session)
  }
}

/// Single emitter task per session: owns all `app.emit` calls for that
/// session, preserving cross-stream ordering exactly like the gateway's
/// single socket writer.
fn spawn_emitter(app: AppHandle, session_id: String, mut rx: UnboundedReceiver<WireEvent>) {
  tauri::async_runtime::spawn(async move {
    // tool_call_id → tool name, per turn (cleared on Done).
    let mut tool_names: HashMap<String, String> = HashMap::new();
    while let Some(ev) = rx.recv().await {
      let event: Option<serde_json::Value> = match ev {
        WireEvent::Agent(event) => {
          if let AgentEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            ..
          } = &event
          {
            tool_names.insert(tool_call_id.clone(), tool_name.clone());
          }
          // The gateway also accumulates provider usage here for its context
          // REST API; the desktop app has no such endpoint, so usage is not
          // tracked on this transport.
          event_to_ws(&event, &mut tool_names).and_then(|ev| serde_json::to_value(ev).ok())
        }
        WireEvent::Subagent(ev) => subagent_event_to_ws(&ev),
        WireEvent::Approval {
          request_id,
          tool_name,
          args,
        } => serde_json::to_value(OutgoingEvent::approval_request(request_id, tool_name, &args))
          .ok(),
        WireEvent::Busy(msg) => serde_json::to_value(OutgoingEvent::busy(msg)).ok(),
        WireEvent::Done => {
          // Turn boundary: the tool-name cache is per-turn.
          tool_names.clear();
          serde_json::to_value(OutgoingEvent::done()).ok()
        }
        WireEvent::Error(msg) => serde_json::to_value(OutgoingEvent::error(msg)).ok(),
      };
      if let Some(event) = event {
        let payload = ChatEventPayload {
          session_id: session_id.clone(),
          event,
        };
        if let Err(e) = app.emit(CHAT_EVENT, payload) {
          warn!("session {session_id}: emit failed: {e}");
        }
      }
    }
  });
}

/// Assemble a session's Host exactly as the gateway does per WS connection:
/// same config loader, same system prompt, same mode env knob, same tool set
/// (built-in + external + MCP), same sub-agent wiring. Do not invent new
/// config here — the desktop app reads the same `~/.gasket` setup.
async fn build_session(
  app: &AppHandle,
  store_root: &std::path::Path,
  session_id: &str,
) -> Result<Arc<ChatSession>, String> {
  let host_cfg = ConfigLoader::load().map_err(|e| format!("Config error: {e}"))?;

  // Fail-loud resume, identical to the gateway: corruption is an error,
  // never adopt-and-restart over a damaged transcript.
  let mut session_mgr = SessionManager::with_root(store_root.to_path_buf());
  session_mgr
    .resume(session_id)
    .await
    .map_err(|e| format!("Session error: {e}"))?;

  let system_prompt = "You are a helpful, concise assistant.".to_string();
  // Intentionally the gateway's env knob: one mode setting shared by both
  // transports.
  let mode = std::env::var("GASKET_GATEWAY_MODE")
    .ok()
    .and_then(|s| Mode::parse(&s))
    .unwrap_or(Mode::AutoEdit);

  let (cancel_tx, _cancel_rx) = tokio::sync::watch::channel(false);
  let registry = Arc::new(Mutex::new(ApprovalRegistry::new()));
  let (wire_tx, wire_rx) = tokio::sync::mpsc::unbounded_channel::<WireEvent>();
  spawn_emitter(app.clone(), session_id.to_string(), wire_rx);

  let approver: Approver = {
    let registry = registry.clone();
    let cancel_tx = cancel_tx.clone();
    let wire = wire_tx.clone();
    // 显式标注 Approver：闭包返回的 Box::pin(async …) 需要按
    // `Pin<Box<dyn Future + Send>>` 非大小化（同 gateway 的批注）。
    Arc::new(move |tool_name: &str, args: &serde_json::Value| {
      let registry = registry.clone();
      let cancel_tx = cancel_tx.clone();
      let wire = wire.clone();
      Box::pin(async move {
        let outcome = { registry.lock().unwrap().register(tool_name) };
        let (request_id, rx) = match outcome {
          RegisterOutcome::Remembered(v) => return v,
          RegisterOutcome::Pending { request_id, rx } => (request_id, rx),
        };
        // Approval requests go through the same ordered channel as every
        // other wire event, so a request can never overtake the tool_start
        // event of the call it belongs to.
        let _ = wire.send(WireEvent::Approval {
          request_id: request_id.clone(),
          tool_name: tool_name.to_string(),
          args: args.clone(),
        });
        let timeout_s = std::env::var("GASKET_APPROVAL_TIMEOUT_S")
          .ok()
          .and_then(|v| v.parse().ok())
          .unwrap_or(300u64);
        // subscribe() 把当前值标记为已见：只有将来的 send 才命中 changed()，
        // 本连接的第一次 cancel 不会毒化后续所有审批。
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

  // Tool assembly mirrors the gateway: built-in + external + MCP for the
  // parent; built-in minus `spawn_subagents` for sub-agents.
  let extra_tools = {
    let cmds = gasket_host::commands_from_env();
    if cmds.is_empty() {
      Vec::new()
    } else {
      match gasket_host::load_external_tools(&cmds).await {
        Ok(t) => t,
        Err(e) => {
          warn!("session {session_id}: external tools load failed: {e}");
          Vec::new()
        }
      }
    }
  };
  let mcp_tools = load_all_mcp().await;
  let built_in = built_in_tools();
  let subagent_tools: Vec<_> = built_in
    .iter()
    .filter(|t| t.name != "spawn_subagents")
    .cloned()
    .collect();
  let tools = {
    let mut t = built_in;
    t.extend(extra_tools.iter().cloned());
    t.extend(mcp_tools.iter().cloned());
    t
  };

  let spawner_cfg = host_cfg.clone();
  let spawner_policy = Arc::clone(&policy);
  let mut host = Host::new(host_cfg, session_mgr, policy, system_prompt, tools);
  {
    let spawner_signal = host.signal().clone();
    let emit: Arc<dyn Fn(gasket_core::SubagentEvent) + Send + Sync> = {
      let wire = wire_tx.clone();
      Arc::new(move |ev: gasket_core::SubagentEvent| {
        let _ = wire.send(WireEvent::Subagent(ev));
      })
    };
    let spawner_stream_fn = spawner_cfg.provider_stream_fn();
    let spawner_hooks: Arc<dyn gasket_core::HookChain> =
      Arc::new(gasket_host::HookStack::new(vec![spawner_policy]));
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
        std::env::current_dir().unwrap_or_default(),
        loop_config,
      )
      .with_ws_emit(emit),
    );
    host = host.with_spawner(spawner);
  }

  Ok(Arc::new(ChatSession {
    host,
    turn_active: AtomicBool::new(false),
    cancel_tx,
    registry,
    wire_tx,
  }))
}

/// Start a turn for `content` on the given session. Returns immediately; the
/// turn's events stream back via `chat-event`. A message while a turn is
/// running is answered with a `busy` event (never silently dropped).
#[tauri::command]
pub async fn send_message(
  app: AppHandle,
  state: State<'_, ChatState>,
  session_id: String,
  content: String,
  trace_id: Option<String>,
) -> Result<(), String> {
  if !gasket_core::is_valid_session_id(&session_id) {
    return Err("invalid session id".into());
  }
  if content.trim().is_empty() {
    return Ok(());
  }
  info!("session {session_id}: message (trace {trace_id:?})");
  let session = match state.get_or_create(&app, &session_id).await {
    Ok(s) => s,
    Err(e) => {
      // Mirror the gateway's config/session error frame: without it the UI
      // would hang in "sending" with no banner.
      let payload = ChatEventPayload {
        session_id: session_id.clone(),
        event: serde_json::to_value(OutgoingEvent::error(e.clone())).unwrap_or_default(),
      };
      let _ = app.emit(CHAT_EVENT, payload);
      return Err(e);
    }
  };

  if session.turn_active.swap(true, Ordering::AcqRel) {
    let _ = session.wire_tx.send(WireEvent::Busy(
      "The agent is busy processing your previous request; this message was not accepted.".into(),
    ));
    return Ok(());
  }

  tauri::async_runtime::spawn(async move {
    // Drop guard: a failed/aborted turn must still free the slot, or the
    // session would stay "busy" forever.
    struct TurnGuard<'a>(&'a AtomicBool);
    impl Drop for TurnGuard<'_> {
      fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
      }
    }
    let _guard = TurnGuard(&session.turn_active);

    let wire = session.wire_tx.clone();
    let outcome = session
      .host
      .run_turn(&content, move |ev| {
        let _ = wire.send(WireEvent::Agent(ev));
      })
      .await;

    // Turn boundary: clear in-flight approvals regardless of outcome.
    session.registry.lock().unwrap().clear_pending();

    // done/error are queued AFTER every event the turn emitted, so the
    // frontend sees a complete picture before the turn-boundary markers.
    let _ = session.wire_tx.send(WireEvent::Done);
    if let Err(e) = outcome {
      warn!("session {session_id}: agent error: {e}");
      let _ = session.wire_tx.send(WireEvent::Error(format!("{e}")));
    }
  });
  Ok(())
}

/// Cooperative abort: set the Host's shared cancel flag (read at safe points
/// by the agent loop) and unlock any pending approval wait.
#[tauri::command]
pub fn cancel_turn(state: State<'_, ChatState>, session_id: String) -> Result<(), String> {
  if let Some(session) = state.sessions.get(&session_id) {
    session.host.signal().store(true, Ordering::Relaxed);
    let _ = session.cancel_tx.send(true);
    info!("session {session_id}: cancel");
  }
  // No session yet (cancel before the first turn): nothing to abort.
  Ok(())
}

/// Fill in an approval decision; `remember` caches it by tool name. Unknown
/// or late request ids are ignored silently, same as the gateway.
#[tauri::command]
pub fn approval_response(
  state: State<'_, ChatState>,
  session_id: String,
  request_id: String,
  approved: bool,
  remember: bool,
) -> Result<(), String> {
  if let Some(session) = state.sessions.get(&session_id) {
    session
      .registry
      .lock()
      .unwrap()
      .respond(&request_id, approved, remember);
  }
  Ok(())
}
