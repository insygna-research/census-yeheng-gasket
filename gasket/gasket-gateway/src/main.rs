//! gasket WebSocket Gateway — bridges the existing Vue 3 frontend to the
//! gasket agent loop via WebSocket + JSON.
//!
//! ## Architecture
//!
//! Each WebSocket connection is one session. The main tokio task loops on
//! incoming messages.  When a `"message"` arrives it spawns the agent loop in
//! a background task and enters a secondary select loop that multiplexes agent
//! events (forwarded to the WebSocket) and incoming messages (cancel, etc.).
//!
//! ## Wire protocol (frontend ↔ gateway)
//!
//! ### Client → Server
//! ```json
//! {"type":"message","content":"...","trace_id":"..."}
//! {"type":"cancel"}
//! ```
//!
//! ### Server → Client (streamed per turn)
//! ```json
//! {"type":"thinking","content":"..."}
//! {"type":"tool_start","name":"...","arguments":"..."}
//! {"type":"tool_end","name":"...","output":"..."}
//! {"type":"content","content":"..."}
//! {"type":"error","content":"...","message":"..."}
//! {"type":"done"}
//! ```

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use dashmap::DashMap;
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use gasket_core::{
    built_in_tools, run_agent_loop, AgentEvent, AgentMessage, ContentBlock, ContentDelta,
    ToolDefinition, UserMessage,
};
use gasket_host::{ConfigLoader, Mode, PermissionPolicy};

// ── Shared state ──────────────────────────────────────────────

struct AppState {
    sessions: DashMap<String, Arc<Mutex<WsSession>>>,
}

struct WsSession {
    sender: SplitSink<WebSocket, Message>,
    history: Vec<AgentMessage>,
    /// Provider-reported token usage accumulated across turns (fed by
    /// `AfterProviderResponse` events in the forwarder).
    usage_in: u64,
    usage_out: u64,
}

// ── Wire protocol types ──────────────────────────────────────

#[derive(Deserialize)]
struct IncomingMessage {
    #[serde(rename = "type")]
    msg_type: String,
    content: Option<String>,
    trace_id: Option<String>,
}

#[derive(Serialize)]
struct OutgoingEvent {
    #[serde(rename = "type")]
    event_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    arguments: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

impl OutgoingEvent {
    fn content(s: String) -> Self {
        Self {
            event_type: "content",
            content: Some(s),
            name: None,
            arguments: None,
            output: None,
            message: None,
        }
    }
    fn thinking(s: String) -> Self {
        Self {
            event_type: "thinking",
            content: Some(s),
            name: None,
            arguments: None,
            output: None,
            message: None,
        }
    }
    fn tool_start(name: String, args: String) -> Self {
        Self {
            event_type: "tool_start",
            content: None,
            name: Some(name),
            arguments: Some(args),
            output: None,
            message: None,
        }
    }
    fn tool_end(name: String, output: String) -> Self {
        Self {
            event_type: "tool_end",
            content: None,
            name: Some(name),
            arguments: None,
            output: Some(output),
            message: None,
        }
    }
    fn error(msg: String) -> Self {
        Self {
            event_type: "error",
            content: Some(msg.clone()),
            name: None,
            arguments: None,
            output: None,
            message: Some(msg),
        }
    }
    fn done() -> Self {
        Self {
            event_type: "done",
            content: None,
            name: None,
            arguments: None,
            output: None,
            message: None,
        }
    }
}

// ── Axum server ────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let _ = dotenvy::dotenv();

    // Initialize default session storage directory.
    let _ = gasket_core::JsonlStorage::default_root();

    let state = Arc::new(AppState {
        sessions: DashMap::new(),
    });

    let frontend_dist =
        std::env::var("GASKET_GATEWAY_STATIC_DIR").unwrap_or_else(|_| "../web/dist".to_string());

    let app = Router::new()
        .route("/ws", get(ws_handler))
        .route("/api/commands", get(get_commands))
        .route("/api/sessions/{key}/context", get(get_context))
        .route("/api/sessions/{key}/context/compact", post(compact_context))
        .fallback_service(
            tower_http::services::ServeDir::new(&frontend_dist).not_found_service(
                tower_http::services::ServeFile::new(format!("{frontend_dist}/index.html")),
            ),
        )
        .layer(tower_http::cors::CorsLayer::permissive())
        .with_state(state);

    let port = std::env::var("GASKET_GATEWAY_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);

    let addr = format!("0.0.0.0:{port}");
    info!("gasket-gateway listening on {addr}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("failed to bind");
    axum::serve(listener, app).await.expect("server error");
}

// ── WebSocket handler ──────────────────────────────────────────

async fn ws_handler(
    ws: WebSocketUpgrade,
    Query(params): Query<HashMap<String, String>>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    // `user_id` is untrusted client input. Never use it as a filesystem path
    // component: a malicious `?user_id=../../etc` would otherwise write the
    // session JSONL outside the store root. Validate; fall back to a fresh
    // server-generated UUID when missing or unsafe.
    let session_id = params
        .get("user_id")
        .filter(|s| gasket_core::is_valid_session_id(s))
        .cloned()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    info!("ws upgrade: session={session_id}");
    ws.on_upgrade(move |socket| handle_ws(socket, state, session_id))
}

async fn handle_ws(socket: WebSocket, state: Arc<AppState>, session_id: String) {
    let (ws_tx, mut ws_rx) = socket.split();
    let session = Arc::new(Mutex::new(WsSession {
        sender: ws_tx,
        history: Vec::new(),
        usage_in: 0,
        usage_out: 0,
    }));
    state.sessions.insert(session_id.clone(), session.clone());

    // ── Load host config ────────────────────────────────────
    let host_cfg = match ConfigLoader::load() {
        Ok(c) => c,
        Err(e) => {
            error!("session {session_id}: config error: {e}");
            let err = OutgoingEvent::error(format!("Config error: {e}"));
            let mut s = session.lock().await;
            send_json(&mut s.sender, &err).await;
            let _ = s.sender.send(Message::Close(None)).await;
            state.sessions.remove(&session_id);
            return;
        }
    };

    let cwd = std::env::current_dir().unwrap_or_default();
    let system_prompt = "You are a helpful, concise assistant.".to_string();
    let signal = Arc::new(AtomicBool::new(false));
    let policy = Arc::new(PermissionPolicy::new(Mode::FullAuto, |_, _| true));
    let extra_tools = load_external_tools().await;
    let storage = gasket_core::JsonlStorage::default_root();

    // ── Main event loop ─────────────────────────────────────
    loop {
        let msg = match ws_rx.next().await {
            Some(Ok(Message::Text(t))) => t.to_string(),
            Some(Ok(Message::Close(_))) | None => {
                info!("session {session_id}: ws closed");
                break;
            }
            Some(Ok(Message::Ping(data))) => {
                let _ = session.lock().await.sender.send(Message::Pong(data)).await;
                continue;
            }
            Some(Ok(Message::Pong(_))) | Some(Ok(Message::Binary(_))) => continue,
            Some(Err(e)) => {
                warn!("session {session_id}: ws error: {e}");
                break;
            }
        };

        let incoming: IncomingMessage = match serde_json::from_str(&msg) {
            Ok(m) => m,
            Err(e) => {
                warn!("session {session_id}: bad JSON: {e}");
                continue;
            }
        };

        match incoming.msg_type.as_str() {
            "message" => {
                let user_text = match incoming.content {
                    Some(t) if !t.trim().is_empty() => t,
                    _ => continue,
                };
                info!(
                    "session {session_id}: message (trace {:?})",
                    incoming.trace_id
                );

                // Slash commands are handled server-side; anything else goes
                // to the LLM. Keep this list in sync with `/api/commands`.
                if let Some(cmd) = user_text.strip_prefix('/') {
                    let mut parts = cmd.split_whitespace();
                    let reply = match parts.next() {
                        Some("clear") => {
                            session.lock().await.history.clear();
                            Some(OutgoingEvent::content("(session cleared)".to_string()))
                        }
                        Some("help") => Some(OutgoingEvent::content(
                            "commands: /clear  /help".to_string(),
                        )),
                        Some(other) => {
                            Some(OutgoingEvent::error(format!("unknown command /{other}")))
                        }
                        None => None,
                    };
                    if let Some(ev) = reply {
                        let mut s = session.lock().await;
                        send_json(&mut s.sender, &ev).await;
                    }
                    continue;
                }

                let user_msg = AgentMessage::User(UserMessage {
                    content: vec![ContentBlock::text(user_text)],
                    timestamp: gasket_core::now(),
                });

                // Snapshot current history.
                let history = session.lock().await.history.clone();

                // ── Spawn agent loop ──────────────────────────
                // We run it in a background task so the main task can
                // eavesdrop on incoming messages (like "cancel").
                let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
                let (result_tx, mut result_rx) = tokio::sync::oneshot::channel::<
                    Result<Vec<AgentMessage>, gasket_core::AgentError>,
                >();

                let tools = {
                    let mut t = built_in_tools();
                    t.extend(extra_tools.iter().cloned());
                    t
                };
                let (context, config) = host_cfg.prepare_turn(
                    gasket_host::TurnInputs {
                        system_prompt: &system_prompt,
                        history: &history,
                        tools: &tools,
                        cwd: &cwd,
                        session_id: &session_id,
                    },
                    &signal,
                    policy.clone(),
                    host_cfg.provider_stream_fn(),
                    host_cfg.tunables.max_turns,
                );

                let agent_event_tx = event_tx.clone();

                tokio::spawn(async move {
                    let result = run_agent_loop(vec![user_msg], context, config, |ev| {
                        let _ = agent_event_tx.send(ev);
                    })
                    .await;
                    drop(event_tx); // Signal the forwarder to stop.
                    let _ = result_tx.send(result);
                });

                // ── Forward events to WebSocket ──────────────
                let fwd_session = session.clone();
                let fwd_handle = tokio::spawn(async move {
                    // Track tool call IDs → tool names so we can emit
                    // tool_end with the correct name.
                    let mut tool_names: HashMap<String, String> = HashMap::new();
                    let mut rx = event_rx;
                    while let Some(event) = rx.recv().await {
                        // Capture tool names as they arrive.
                        if let AgentEvent::ToolExecutionStart {
                            tool_call_id,
                            tool_name,
                            ..
                        } = &event
                        {
                            tool_names.insert(tool_call_id.clone(), tool_name.clone());
                        }
                        // Accumulate provider-reported usage for the context API.
                        if let AgentEvent::AfterProviderResponse { response, .. } = &event {
                            if let Some(u) = &response.usage {
                                let mut s = fwd_session.lock().await;
                                s.usage_in += u.input_tokens;
                                s.usage_out += u.output_tokens;
                            }
                        }
                        let json = event_to_ws(&event, &mut tool_names);
                        if let Some(json) = json {
                            let mut s = fwd_session.lock().await;
                            send_json(&mut s.sender, &json).await;
                        }
                    }
                });

                // ── Select loop: agent result vs incoming msgs ──
                loop {
                    tokio::select! {
                        // Agent loop finished
                        result = &mut result_rx => {
                            let _ = fwd_handle.await;

                            // Send "done"
                            {
                                let mut s = session.lock().await;
                                send_json(&mut s.sender, &OutgoingEvent::done()).await;
                            }

                            match result {
                                Ok(Ok(new_msgs)) => {
                                    // Persist
                                    if let Err(e) = storage
                                        .append_messages(&session_id, &new_msgs)
                                        .await
                                    {
                                        warn!("session {session_id}: persist: {e}");
                                    }
                                    let mut s = session.lock().await;
                                    s.history.extend(new_msgs);
                                }
                                Ok(Err(e)) => {
                                    let err = OutgoingEvent::error(format!("{e}"));
                                    let mut s = session.lock().await;
                                    send_json(&mut s.sender, &err).await;
                                    warn!("session {session_id}: agent error: {e}");
                                }
                                Err(e) => {
                                    error!("session {session_id}: agent panic: {e}");
                                }
                            }
                            break; // back to main loop
                        }

                        // Incoming message while agent is running
                        msg = ws_rx.next() => {
                            match msg {
                                Some(Ok(Message::Text(t))) => {
                                    // Cancel only on an explicit
                                    // `{"type":"cancel"}` - never on a user
                                    // message that merely contains "cancel".
                                    let is_cancel = serde_json::from_str::<
                                        IncomingMessage,
                                    >(&t)
                                        .ok()
                                        .is_some_and(|m| m.msg_type == "cancel");
                                    if is_cancel {
                                        info!("session {session_id}: cancel during turn");
                                        signal.store(true, Ordering::Relaxed);
                                    }
                                    // Ignore other messages during agent execution.
                                }
                                Some(Ok(Message::Close(_))) | None => {
                                    info!("session {session_id}: ws closed during turn");
                                    signal.store(true, Ordering::Relaxed);
                                    // Wait for agent to finish cleanly.
                                    let _ = fwd_handle.await;
                                    break;
                                }
                                Some(Ok(Message::Ping(data))) => {
                                    let _ = session.lock().await.sender.send(Message::Pong(data)).await;
                                }
                                Some(Ok(Message::Pong(_))) | Some(Ok(Message::Binary(_))) => {}
                                Some(Err(e)) => {
                                    warn!("session {session_id}: ws error during turn: {e}");
                                    signal.store(true, Ordering::Relaxed);
                                    let _ = fwd_handle.await;
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            "cancel" => {
                // No active turn to cancel — ignore.
                info!("session {session_id}: cancel outside turn");
            }
            other => {
                warn!("session {session_id}: unknown msg type: {other}");
            }
        }
    }

    info!("session {session_id}: ended");
    state.sessions.remove(&session_id);
}

// ── Event → JSON conversion ────────────────────────────────────

/// Convert an [`AgentEvent`] to the frontend's JSON protocol, looking up
/// tool names from `tool_names` (populated by [`ToolExecutionStart`]).
fn event_to_ws(
    event: &AgentEvent,
    tool_names: &mut HashMap<String, String>,
) -> Option<OutgoingEvent> {
    match event {
        AgentEvent::MessageUpdate { delta } => match delta {
            ContentDelta::TextDelta(t) => Some(OutgoingEvent::content(t.clone())),
            ContentDelta::ThinkingDelta(t) => Some(OutgoingEvent::thinking(t.clone())),
            ContentDelta::ToolCallDelta { .. } => {
                // Accumulated server-side; sent as tool_start at execution.
                None
            }
        },
        AgentEvent::ToolExecutionStart {
            tool_name, args, ..
        } => {
            let args_str = serde_json::to_string(args).unwrap_or_default();
            Some(OutgoingEvent::tool_start(tool_name.clone(), args_str))
        }
        AgentEvent::ToolExecutionEnd {
            tool_call_id,
            result,
            ..
        } => {
            let name = tool_names
                .get(tool_call_id)
                .cloned()
                .unwrap_or_else(|| "unknown".to_string());
            let summary = result
                .content
                .iter()
                .find_map(|b| match b {
                    gasket_core::ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            Some(OutgoingEvent::tool_end(name, summary))
        }
        AgentEvent::Error { message } => Some(OutgoingEvent::error(message.clone())),
        _ => None,
    }
}

// ── WS send helper ─────────────────────────────────────────────

async fn send_json(sender: &mut SplitSink<WebSocket, Message>, event: &OutgoingEvent) {
    let text = serde_json::to_string(event).unwrap_or_default();
    if let Err(e) = sender.send(Message::Text(text.into())).await {
        warn!("send failed: {e}");
    }
}

// ── External tools ─────────────────────────────────────────────

async fn load_external_tools() -> Vec<ToolDefinition> {
    let cmds = gasket_host::commands_from_env();
    if cmds.is_empty() {
        return Vec::new();
    }
    match gasket_host::load_external_tools(&cmds).await {
        Ok(t) => {
            info!("loaded {} external tool(s)", t.len());
            t
        }
        Err(e) => {
            warn!("external tools load failed: {e}");
            Vec::new()
        }
    }
}

// ── REST API ───────────────────────────────────────────────────

/// The slash commands this gateway actually supports. The frontend's
/// completer renders this list — every entry MUST have a handler in the WS
/// message loop above.
async fn get_commands() -> Json<Value> {
    Json(json!([
        {
            "name": "clear",
            "description": "Clear the conversation history",
            "aliases": []
        },
        {
            "name": "help",
            "description": "Show available commands",
            "aliases": ["?"]
        }
    ]))
}

/// The frontend keys sessions as `websocket:{id}` while the WS connection
/// registers under bare `{id}` — strip the prefix before looking up.
fn session_key(key: &str) -> &str {
    key.strip_prefix("websocket:").unwrap_or(key)
}

/// Provider-reported tokens accumulated over the session, plus a saturation
/// percentage against the configured window (`GASKET_CONTEXT_WINDOW`,
/// default 128k). The percentage is a display heuristic; the token counts
/// themselves are real API usage.
fn context_stats(usage_in: u64, usage_out: u64) -> Value {
    let window = std::env::var("GASKET_CONTEXT_WINDOW")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(128_000);
    let current = usage_in + usage_out;
    let usage_percent = if window > 0 {
        (current as f64 / window as f64) * 100.0
    } else {
        0.0
    };
    json!({
        "current_tokens": current,
        "usage_percent": usage_percent,
        "is_compressing": false,
    })
}

async fn get_context(State(state): State<Arc<AppState>>, Path(key): Path<String>) -> Json<Value> {
    let stats = match state.sessions.get(session_key(&key)) {
        Some(s) => {
            let s = s.lock().await;
            context_stats(s.usage_in, s.usage_out)
        }
        None => context_stats(0, 0),
    };
    // No watermark/compaction mechanism exists in this architecture — null so
    // the frontend hides the watermark chip instead of rendering undefined.
    Json(json!({ "context_stats": stats, "watermark_info": null }))
}

/// Compact the session's working memory (append-only JSONL untouched), then
/// return fresh stats. Mirrors the CLI's per-turn compaction.
async fn compact_context(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
) -> Json<Value> {
    let mut stats = context_stats(0, 0);
    if let Some(s) = state.sessions.get(session_key(&key)) {
        let mut s = s.lock().await;
        let max = gasket_host::max_messages_from_env();
        s.history = gasket_host::compact_by_count(&s.history, max);
        stats = context_stats(s.usage_in, s.usage_out);
    }
    Json(json!({ "context_stats": stats, "watermark_info": null }))
}
