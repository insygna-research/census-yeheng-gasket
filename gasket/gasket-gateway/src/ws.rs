//! WebSocket upgrade handler and the per-connection session loop.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use gasket_core::{
    built_in_tools, run_agent_loop, AgentEvent, AgentMessage, ContentBlock, UserMessage,
};

use gasket_host::permission::Approver;
use gasket_host::{ConfigLoader, Mode, PermissionPolicy};

use crate::api::load_external_tools;
use crate::approval::{self, ApprovalRegistry, RegisterOutcome};
use crate::event_map::event_to_ws;
use crate::state::{AppState, WsSession};
use crate::wire::{ApprovalResponse, IncomingMessage, OutgoingEvent};

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

    let cwd = std::env::current_dir().unwrap_or_default();
    let system_prompt = "You are a helpful, concise assistant.".to_string();
    let signal = Arc::new(AtomicBool::new(false));
    let mode = std::env::var("GASKET_GATEWAY_MODE")
        .ok()
        .and_then(|s| Mode::parse(&s))
        .unwrap_or(Mode::AutoEdit);
    // cancel 信号的双通道：AtomicBool 驱动 loop 中止，watch 解锁挂起的审批。
    // 闭包只保留 Sender，每次审批 subscribe() 出新 receiver--Receiver::clone()
    // 会复制旧 observed-version，一次 cancel 后所有克隆都会立即命中 changed()
    // （闩锁），见 approval.rs 的 wait_for_decision 测试。
    let (cancel_tx, _cancel_rx) = tokio::sync::watch::channel(false);
    let approver_session = session.clone();
    // 闭包侧持有 Sender 的克隆（原 Sender 保留在主循环供 cancel 使用）。
    let approver_cancel_tx = cancel_tx.clone();
    // 显式标注 Approver：闭包返回的 Box::pin(async …) 需要在这里按
    // `Pin<Box<dyn Future + Send>>` 非大小化（裸闭包推断会把返回类型
    // 锁死为具体 async block，后续再转 Arc<dyn Fn…> 会失败）。
    let approver: Approver = Arc::new(move |tool_name: &str, args: &serde_json::Value| {
        let session = approver_session.clone();
        let cancel_tx = approver_cancel_tx.clone();
        Box::pin(async move {
            let outcome = { session.lock().await.registry.register(tool_name) };
            let (request_id, rx) = match outcome {
                RegisterOutcome::Remembered(v) => return v,
                RegisterOutcome::Pending { request_id, rx } => (request_id, rx),
            };
            {
                let mut s = session.lock().await;
                let ev = OutgoingEvent::approval_request(
                    request_id.clone(),
                    tool_name.to_string(),
                    args,
                );
                send_json(&mut s.sender, &ev).await;
            }
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
    let storage = gasket_core::JsonlStorage::default_root();
    // Per-connection token-aware compaction budget. Fed from
    // `last_input_tokens` (read off the session) before each turn; compaction
    // runs on the history snapshot so the budget never crosses task boundaries.
    let mut budget = gasket_host::ContextBudget::from_env();

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
                    // Track tool call IDs -> tool names so we can emit
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
                                s.last_input_tokens = u.input_tokens;
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
                            // 回合结束：清空在途审批，与后续回合隔离。
                            session.lock().await.registry.clear_pending();

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
                                            _ => {} // 回合中的其他消息忽略
                                        }
                                    }
                                }
                                Some(Ok(Message::Close(_))) | None => {
                                    info!("session {session_id}: ws closed during turn");
                                    signal.store(true, Ordering::Relaxed);
                                    let _ = cancel_tx.send(true);
                                    // Wait for agent to finish cleanly.
                                    let _ = fwd_handle.await;
                                    // 回合结束（ws 断开）：与 result 分支一致，清空在途审批。
                                    session.lock().await.registry.clear_pending();
                                    break;
                                }
                                Some(Ok(Message::Ping(data))) => {
                                    let _ = session.lock().await.sender.send(Message::Pong(data)).await;
                                }
                                Some(Ok(Message::Pong(_))) | Some(Ok(Message::Binary(_))) => {}
                                Some(Err(e)) => {
                                    warn!("session {session_id}: ws error during turn: {e}");
                                    signal.store(true, Ordering::Relaxed);
                                    let _ = cancel_tx.send(true);
                                    let _ = fwd_handle.await;
                                    // 回合结束（ws 错误）：与 result 分支一致，清空在途审批。
                                    session.lock().await.registry.clear_pending();
                                    break;
                                }
                            }
                        }
                    }
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
