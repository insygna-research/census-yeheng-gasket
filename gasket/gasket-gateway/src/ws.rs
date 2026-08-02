//! WebSocket upgrade handler and the per-connection session loop.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use gasket_core::{
    built_in_tools, AgentEvent, AgentMessage, ContentBlock, UserMessage,
};

use gasket_host::permission::Approver;
use gasket_host::{
    load_all_mcp, ConfigLoader, ContextBudget, Host, HostSubagentSpawner, Mode,
    PermissionPolicy, SessionManager,
};

use crate::api::load_external_tools;
use crate::approval::{self, ApprovalRegistry, RegisterOutcome};
use crate::event_map::event_to_ws;
use crate::state::{AppState, WsSession};
use crate::wire::{ApprovalResponse, IncomingMessage, OutgoingEvent};

/// Everything written to the socket flows through ONE ordered channel and a
/// single writer task. A single writer guarantees cross-stream ordering:
/// without it, the turn-boundary `done` could overtake the last subagent
/// event (the frontend skips `done` while subagents are active → stuck UI),
/// and approval requests could overtake the tool_start they belong to.
enum WireEvent {
    Agent(gasket_core::AgentEvent),
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
        last_input_tokens: 0,
        registry: ApprovalRegistry::new(),
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

    // ── Resume prior transcript (reconnect keeps context) ──
    // Host's SessionManager owns the on-disk JSONL; the in-memory
    // WsSession.history is the working copy (compaction + REST context API).
    let mut session_mgr = SessionManager::new();
    let resumed = session_mgr.resume_or_adopt(&session_id).await;
    if !resumed.is_empty() {
        info!("session {session_id}: resumed {} msgs", resumed.len());
        session.lock().await.history = resumed;
    }

    let system_prompt = "You are a helpful, concise assistant.".to_string();
    let mode = std::env::var("GASKET_GATEWAY_MODE")
        .ok()
        .and_then(|s| Mode::parse(&s))
        .unwrap_or(Mode::AutoEdit);
    // cancel 信号的双通道：AtomicBool 驱动 loop 中止，watch 解锁挂起的审批。
    // 闭包只保留 Sender，每次审批 subscribe() 出新 receiver--Receiver::clone()
    // 会复制旧 observed-version，一次 cancel 后所有克隆都会立即命中 changed()
    // （闩锁），见 approval.rs 的 wait_for_decision 测试。
    let (cancel_tx, _cancel_rx) = tokio::sync::watch::channel(false);

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
                    if let gasket_core::SubagentEvent::Usage {
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
                        crate::event_map::subagent_event_to_ws(&ev)
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
                WireEvent::Done => {
                    // Turn boundary: the tool-name cache is per-turn.
                    tool_names.clear();
                    let ev = OutgoingEvent::done();
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

    let approver_session = session.clone();
    // 闭包侧持有 Sender 的克隆（原 Sender 保留在主循环供 cancel 使用）。
    let approver_cancel_tx = cancel_tx.clone();
    // 显式标注 Approver：闭包返回的 Box::pin(async …) 需要在这里按
    // `Pin<Box<dyn Future + Send>>` 非大小化（裸闭包推断会把返回类型
    // 锁死为具体 async block，后续再转 Arc<dyn Fn…> 会失败）。
    let approver_wire = wire_tx.clone();
    let approver: Approver = Arc::new(move |tool_name: &str, args: &serde_json::Value| {
        let session = approver_session.clone();
        let cancel_tx = approver_cancel_tx.clone();
        let wire = approver_wire.clone();
        Box::pin(async move {
            let outcome = { session.lock().await.registry.register(tool_name) };
            let (request_id, rx) = match outcome {
                RegisterOutcome::Remembered(v) => return v,
                RegisterOutcome::Pending { request_id, rx } => (request_id, rx),
            };
            // Approval requests go through the same ordered channel as every
            // other wire event, so a request can never overtake the
            // tool_start event of the call it belongs to.
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
    });
    let policy = Arc::new(PermissionPolicy::new(mode, approver));
    let extra_tools = load_external_tools().await;
    let mcp_tools = load_all_mcp().await;
    // Built-in tools built once; the sub-agent set is filtered from this
    // same Vec (minus `spawn_subagents`), so built_in_tools() is never
    // called twice per connection.
    let built_in = built_in_tools();
    let subagent_tools: Vec<_> = built_in
        .iter()
        .filter(|t| t.name != "spawn_subagents")
        .cloned()
        .collect();
    // Parent agent gets built-in + external + MCP.
    let tools = {
        let mut t = built_in;
        t.extend(extra_tools.iter().cloned());
        t.extend(mcp_tools.iter().cloned());
        t
    };
    // Per-connection Host drives the same run_turn pipeline the CLI uses; its
    // resumed SessionManager owns the on-disk transcript (appends on success).
    let spawner_cfg = host_cfg.clone();
    let spawner_policy = Arc::clone(&policy);
    let mut host = Host::new(host_cfg, session_mgr, policy, system_prompt, tools);
    // Subagent spawner: events forwarded to WS via the wire channel.
    {
        let spawner_signal = host.signal().clone();
        let ws_emit: Arc<dyn Fn(gasket_core::SubagentEvent) + Send + Sync> = {
            let wire = wire_tx.clone();
            Arc::new(move |ev: gasket_core::SubagentEvent| {
                let _ = wire.send(WireEvent::Subagent(ev));
            })
        };
        let spawner_stream_fn = spawner_cfg.provider_stream_fn();
        let spawner_hooks: Arc<dyn gasket_core::HookChain> = Arc::new(
            gasket_host::HookStack::new(vec![spawner_policy]),
        );
        // Sub-agents get the built-in tool set minus `spawn_subagents`
        // (nesting is disabled — sub-agent contexts carry no spawner).
        // MCP/external tools are deliberately excluded: their servers are
        // shared per-connection and not built for 5 parallel loops. The
        // shared permission policy still gates every tool call they do get.
        // Loop-config template from the parent's provider/tunables; the
        // spawner clones it per sub-agent (capping max_turns, pinning the
        // shared signal + policy hooks).
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
            .with_ws_emit(ws_emit),
        );
        host = host.with_spawner(spawner);
    }
    // Cancel sets the Host's shared abort flag; run_turn reads it at safe points.
    let signal = host.signal().clone();
    // Per-connection token-aware compaction budget. Fed from `last_input_tokens`
    // (read off the session) before each turn; compaction runs on the history
    // snapshot so the budget never crosses the turn boundary.
    let mut budget = ContextBudget::from_env();

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

                // Snapshot current history and feed the per-turn compaction
                // budget from the most recent provider report, then compact if
                // over threshold. One lock grabs both history and token count.
                let history = {
                    let s = session.lock().await;
                    budget.record_input_tokens(s.last_input_tokens);
                    s.history.clone()
                };
                let history = if budget.needs_compaction() {
                    budget.compact(&history)
                } else {
                    history
                };

                // ── Run the turn inline, multiplexing cancel/approval ──
                // run_turn drives the agent loop inline; the sync on_event
                // closure forwards events to the connection-wide wire channel
                // (whose single writer task owns the socket and ordering).
                // This is the same run_turn the CLI uses. On close/error we
                // break immediately: dropping `turn` is cancel-safe (run_turn
                // persists only on success), and it stops us re-polling an
                // exhausted ws_rx (a Stream contract violation).
                let turn_wire = wire_tx.clone();
                let mut closing = false;
                let turn_outcome: Option<Result<Vec<AgentMessage>, gasket_host::HostError>> = {
                    let turn = host.run_turn(user_msg, &history, {
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
                                                    signal.store(true, Ordering::Relaxed);
                                                    let _ = cancel_tx.send(true);
                                                }
                                                "approval_response" => {
                                                    if let Ok(resp) = serde_json::from_str::<
                                                        ApprovalResponse,
                                                    >(&t)
                                                    {
                                                        session.lock().await.registry.respond(
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
                                        signal.store(true, Ordering::Relaxed);
                                        let _ = cancel_tx.send(true);
                                        closing = true;
                                        break;
                                    }
                                    Some(Err(e)) => {
                                        warn!("session {session_id}: ws error during turn: {e}");
                                        signal.store(true, Ordering::Relaxed);
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
                session.lock().await.registry.clear_pending();

                if !closing {
                    // done/error are queued AFTER every event the turn emitted
                    // (all subagent events were queued before spawn returned),
                    // so the frontend sees a complete picture before the
                    // turn-boundary markers.
                    let _ = wire_tx.send(WireEvent::Done);
                    match turn_outcome {
                        Some(Ok(new_msgs)) => {
                            session.lock().await.history.extend(new_msgs);
                        }
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
                signal.store(true, Ordering::Relaxed);
                let _ = cancel_tx.send(true);
                info!("session {session_id}: cancel outside turn");
            }
            "approval_response" => {
                // 迟到的审批响应（回合已结束，registry 已 clear）：静默忽略。
                if let Ok(resp) = serde_json::from_str::<ApprovalResponse>(&msg) {
                    session.lock().await.registry.respond(
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
