//! WebSocket upgrade handler and the per-connection session loop.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use conga::AgentEvent;

use conga_host::event_map::event_to_ws;
use conga_host::wire::OutgoingEvent;

use crate::state::{AppState, WsSession};
use crate::wire::{ApprovalResponse, IncomingMessage};

/// Everything written to the socket flows through ONE ordered channel and a
/// single writer task. A single writer guarantees cross-stream ordering:
/// without it, the turn-boundary `done` could overtake the last subagent
/// event (the frontend skips `done` while subagents are active → stuck UI),
/// and approval requests could overtake the tool_start they belong to.
enum WireEvent {
    Agent(conga::AgentEvent),
    Subagent(conga_host::SubagentEvent),
    Approval {
        request_id: String,
        tool_name: String,
        args: serde_json::Value,
    },
    /// Reply to a message received while a turn is already running.
    Busy(String),
    /// Slash-command reply that bypasses `run_turn` (goes through the same
    /// ordered channel as everything else — a single writer means exactly
    /// that, no direct-sender shortcuts). Always followed by `Done` so the
    /// frontend's turn-boundary handling fires.
    Reply(OutgoingEvent),
    Done,
    Error(String),
}

pub(crate) async fn ws_handler(
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
        .filter(|s| conga::is_valid_session_id(s))
        .cloned()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    info!("ws upgrade: session={session_id}");
    ws.on_upgrade(move |socket| handle_ws(socket, state, session_id))
}

async fn handle_ws(socket: WebSocket, state: Arc<AppState>, session_id: String) {
    let (ws_tx, mut ws_rx) = socket.split();
    let session = Arc::new(Mutex::new(WsSession {
        sender: ws_tx,
        usage_in: 0,
        usage_out: 0,
        last_input_tokens: 0,
        turn_start: None,
    }));
    state.sessions.insert(session_id.clone(), session.clone());

    // ── Single ordered wire channel ──────────────────────────
    // All outbound events (main agent stream, subagent events, approval
    // requests, turn-boundary done/error) queue here; one writer task owns
    // the socket, preserving order across streams.
    let (wire_tx, mut wire_rx) = tokio::sync::mpsc::unbounded_channel::<WireEvent>();
    let wire_session = session.clone();
    tokio::spawn(async move {
        // tool_call_id → tool name, per turn (cleared on Done).
        let mut tool_names: HashMap<String, String> = HashMap::new();
        while let Some(ev) = wire_rx.recv().await {
            let payload: Option<String> = match ev {
                WireEvent::Agent(event) => {
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
                            let mut s = wire_session.lock().await;
                            s.usage_in += u.input_tokens;
                            s.usage_out += u.output_tokens;
                            s.last_input_tokens = u.input_tokens;
                        }
                    }
                    event_to_ws(&event, &mut tool_names)
                        .map(|ev| serde_json::to_string(&ev).unwrap_or_default())
                }
                WireEvent::Subagent(ev) => {
                    if let conga_host::SubagentEvent::Usage {
                        input_tokens,
                        output_tokens,
                    } = ev
                    {
                        // Sub-agent provider usage counts toward the session's
                        // token totals; it has no WS message of its own. (The
                        // parent's compaction budget is NOT touched: sub-agent
                        // messages never enter the main history.)
                        let mut s = wire_session.lock().await;
                        s.usage_in += input_tokens;
                        s.usage_out += output_tokens;
                        None
                    } else {
                        conga_host::event_map::subagent_event_to_ws(&ev)
                            .map(|v| serde_json::to_string(&v).unwrap_or_default())
                    }
                }
                WireEvent::Approval {
                    request_id,
                    tool_name,
                    args,
                } => {
                    let ev = OutgoingEvent::approval_request(request_id, tool_name, &args);
                    Some(serde_json::to_string(&ev).unwrap_or_default())
                }
                WireEvent::Busy(msg) => {
                    let ev = OutgoingEvent::busy(msg);
                    Some(serde_json::to_string(&ev).unwrap_or_default())
                }
                WireEvent::Reply(ev) => Some(serde_json::to_string(&ev).unwrap_or_default()),
                WireEvent::Done => {
                    // Turn boundary: the tool-name cache is per-turn.
                    tool_names.clear();
                    // Emit a usage summary line: cumulative tokens +
                    // elapsed time. `turn_start` is set by the main loop
                    // just before run_turn; None only if Done arrives
                    // without a preceding turn (shouldn't happen, but
                    // degrade to a plain done instead of crashing).
                    let s = wire_session.lock().await;
                    let elapsed_ms = s
                        .turn_start
                        .map(|t| t.elapsed().as_millis() as u64)
                        .unwrap_or(0);
                    let ev = if elapsed_ms > 0 {
                        OutgoingEvent::done_with_summary(s.usage_in, s.usage_out, elapsed_ms)
                    } else {
                        OutgoingEvent::done()
                    };
                    drop(s); // release lock before send_json below
                    Some(serde_json::to_string(&ev).unwrap_or_default())
                }
                WireEvent::Error(msg) => {
                    let ev = OutgoingEvent::error(msg);
                    Some(serde_json::to_string(&ev).unwrap_or_default())
                }
            };
            if let Some(payload) = payload {
                let mut s = wire_session.lock().await;
                let _ = s
                    .sender
                    .send(axum::extract::ws::Message::Text(payload.into()))
                    .await;
            }
        }
    });

    // ── Assemble the session's Host ──────────────────────
    // One shared wiring for every transport (conga_host::assembly): config
    // load, fail-loud log resume (corruption refuses the connection —
    // never adopt-and-restart), skills, permission mode + approver, tool
    // set, sub-agent spawner. The gateway owns only transport plumbing:
    // this ordered channel and the message loop below.
    let approval_emit: conga_host::ApprovalEmit = {
        let wire = wire_tx.clone();
        Arc::new(
            move |request_id: String, tool_name: String, args: serde_json::Value| {
                let _ = wire.send(WireEvent::Approval {
                    request_id,
                    tool_name,
                    args,
                });
            },
        )
    };
    let subagent_emit: conga_host::SubagentEmit = {
        let wire = wire_tx.clone();
        Arc::new(move |ev: conga_host::SubagentEvent| {
            let _ = wire.send(WireEvent::Subagent(ev));
        })
    };
    let assembly =
        match conga_host::SessionAssembly::build(
            &state.store_root,
            &session_id,
            Vec::new(),
            approval_emit,
            subagent_emit,
        )
        .await
        {
            Ok(a) => a,
            Err(e) => {
                error!("session {session_id}: {e}");
                let err = OutgoingEvent::error(e.to_string());
                let mut s = session.lock().await;
                send_json(&mut s.sender, &err).await;
                let _ = s.sender.send(Message::Close(None)).await;
                state.sessions.remove(&session_id);
                return;
            }
        };
    let conga_host::SessionAssembly {
        host,
        registry,
        cancel_tx,
    } = assembly;
    // Cancel sets the Host's shared abort flag; run_turn reads it at safe points.
    let signal = host.signal().clone();

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
                            // Unified /clear: append a Cleared fact to THIS
                            // session's log — the id does NOT rotate, so the
                            // connection, REST readers, and the FTS index
                            // keep addressing the same chat (no ghost
                            // sessions). derive_messages truncates on the
                            // next turn. Reset the display counters too.
                            match host.clear_session().await {
                                Ok(()) => {
                                    let mut s = session.lock().await;
                                    s.usage_in = 0;
                                    s.usage_out = 0;
                                    s.last_input_tokens = 0;
                                    Some(OutgoingEvent::content(
                                        "(session cleared)".to_string(),
                                    ))
                                }
                                Err(e) => {
                                    Some(OutgoingEvent::error(format!("clear failed: {e}")))
                                }
                            }
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
                        // Slash commands are not turns: clear turn_start so the
                        // done event renders without a stale elapsed/usage
                        // summary from a previous turn.
                        session.lock().await.turn_start = None;
                        // Reply + done ride the ordered channel like every
                        // other outbound event — single writer, no shortcuts.
                        let _ = wire_tx.send(WireEvent::Reply(ev));
                        let _ = wire_tx.send(WireEvent::Done);
                    }
                    continue;
                }

                // Record turn start for the done-summary line. Set before
                // run_turn begins so the elapsed time covers the whole turn.
                session.lock().await.turn_start = Some(std::time::Instant::now());

                // The event log is the source of truth: run_turn derives
                // (and compacts) history from it internally.

                // ── Run the turn inline, multiplexing cancel/approval ──
                // run_turn drives the agent loop inline; the sync on_event
                // closure forwards events to the connection-wide wire channel
                // (whose single writer task owns the socket and ordering).
                // This is the same run_turn the CLI uses. On close/error we
                // break immediately: dropping `turn` is cancel-safe (the
                // event log already holds every fact the turn produced), and
                // it stops us re-polling an exhausted ws_rx (a Stream
                // contract violation).
                //
                // Turn serialization: one connection = one Host = one turn
                // at a time. The turn future is created, pinned, and polled
                // to completion inside this match arm — this loop cannot
                // reach a second run_turn until the current one resolves,
                // and Host itself rejects any concurrent run_turn
                // (`TurnInProgress`) as a backstop.
                let turn_wire = wire_tx.clone();
                let mut closing = false;
                let turn_outcome: Option<Result<conga_host::TurnSummary, conga::AgentError>> = {
                    let turn = host.run_turn(&user_text, {
                        let wire = turn_wire.clone();
                        move |ev| {
                            let _ = wire.send(WireEvent::Agent(ev));
                        }
                    });
                    tokio::pin!(turn);

                    let mut outcome = None;
                    loop {
                        tokio::select! {
                            res = &mut turn => {
                                outcome = Some(res);
                                break;
                            }
                            msg = ws_rx.next() => {
                                match msg {
                                    Some(Ok(Message::Text(t))) => {
                                        if let Ok(incoming) =
                                            serde_json::from_str::<IncomingMessage>(&t)
                                        {
                                            match incoming.msg_type.as_str() {
                                                "cancel" => {
                                                    info!("session {session_id}: cancel during turn");
                                                    signal.cancel();
                                                    let _ = cancel_tx.send(true);
                                                }
                                                "approval_response" => {
                                                    if let Ok(resp) = serde_json::from_str::<
                                                        ApprovalResponse,
                                                    >(&t)
                                                    {
                                                        registry.lock().unwrap().respond(
                                                            &resp.request_id,
                                                            resp.approved,
                                                            resp.remember,
                                                        );
                                                    }
                                                }
                                                "message" => {
                                                    // A message during a turn
                                                    // cannot be accepted. Never
                                                    // drop user input silently:
                                                    // tell them.
                                                    let _ = turn_wire.send(WireEvent::Busy(
                                                        "The agent is busy processing your previous request; this message was not accepted."
                                                            .into(),
                                                    ));
                                                }
                                                _ => {}
                                            }
                                        }
                                    }
                                    Some(Ok(Message::Ping(data))) => {
                                        let _ = session.lock().await.sender.send(Message::Pong(data)).await;
                                    }
                                    Some(Ok(Message::Pong(_))) | Some(Ok(Message::Binary(_))) => {}
                                    Some(Ok(Message::Close(_))) | None => {
                                        info!("session {session_id}: ws closed during turn");
                                        signal.cancel();
                                        let _ = cancel_tx.send(true);
                                        closing = true;
                                        break;
                                    }
                                    Some(Err(e)) => {
                                        warn!("session {session_id}: ws error during turn: {e}");
                                        signal.cancel();
                                        let _ = cancel_tx.send(true);
                                        closing = true;
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    outcome
                }; // turn dropped

                // Turn boundary: clear in-flight approvals regardless of outcome.
                registry.lock().unwrap().clear_pending();

                if !closing {
                    // done/error are queued AFTER every event the turn emitted
                    // (all subagent events were queued before spawn returned),
                    // so the frontend sees a complete picture before the
                    // turn-boundary markers.
                    let _ = wire_tx.send(WireEvent::Done);
                    match turn_outcome {
                        // The log already holds everything the turn produced
                        // (persisted event-by-event); no in-memory history to
                        // rewire.
                        Some(Ok(_summary)) => {}
                        Some(Err(e)) => {
                            let _ = wire_tx.send(WireEvent::Error(format!("{e}")));
                            warn!("session {session_id}: agent error: {e}");
                        }
                        None => {}
                    }
                }

                if closing {
                    break;
                }
            }
            "cancel" => {
                // 回合外 cancel：置 signal + 解锁任何残留审批等待。
                signal.cancel();
                let _ = cancel_tx.send(true);
                info!("session {session_id}: cancel outside turn");
            }
            "approval_response" => {
                // 迟到的审批响应（回合已结束，registry 已 clear）：静默忽略。
                if let Ok(resp) = serde_json::from_str::<ApprovalResponse>(&msg) {
                    registry.lock().unwrap().respond(
                        &resp.request_id,
                        resp.approved,
                        resp.remember,
                    );
                }
            }
            other => {
                warn!("session {session_id}: unknown msg type: {other}");
            }
        }
    }

    info!("session {session_id}: ended");
    state.sessions.remove(&session_id);
}

// ── WS send helper ─────────────────────────────────────────────

async fn send_json(sender: &mut SplitSink<WebSocket, Message>, event: &OutgoingEvent) {
    let text = serde_json::to_string(event).unwrap_or_default();
    if let Err(e) = sender.send(Message::Text(text.into())).await {
        warn!("send failed: {e}");
    }
}
