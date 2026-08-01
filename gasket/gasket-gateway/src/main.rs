//! gasket WebSocket Gateway - bridges the existing Vue 3 frontend to the
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
//! ### Client -> Server
//! ```json
//! {"type":"message","content":"...","trace_id":"..."}
//! {"type":"cancel"}
//! ```
//!
//! ### Server -> Client (streamed per turn)
//! ```json
//! {"type":"thinking","content":"..."}
//! {"type":"tool_start","name":"...","arguments":"..."}
//! {"type":"tool_end","name":"...","output":"..."}
//! {"type":"content","content":"..."}
//! {"type":"error","content":"...","message":"..."}
//! {"type":"done"}
//! ```
//!
//! ### 契约核对表（前端 `useChatSession.ts` / `types/index.ts` 全部消息类型）
//!
//! | 消息 | 方向 | 状态 |
//! |---|---|---|
//! | `message` / `cancel` | C->S | ✅ 已实现 |
//! | `approval_request` / `approval_response` | 双向 | ✅ 已实现（本任务） |
//! | `thinking` / `tool_start` / `tool_end` / `content` / `error` / `done` | S->C | ✅ 已实现 |
//! | `subagent_*`（10 种） | S->C | ⏳ M2 规划（core 子 agent 编排落地后启用；前端处理器已存在，网关不发送） |

use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;
use dashmap::DashMap;
use tracing::info;

use crate::api::{compact_context, get_commands, get_context};
use crate::state::AppState;
use crate::ws::ws_handler;

mod api;
mod approval;
mod event_map;
mod state;
mod wire;
mod ws;

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

// ── Unit tests (pure functions only) ──────────────────────────

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use gasket_core::{AgentEvent, ContentBlock, ContentDelta, ToolResultMessage};
    use serde_json::Value;

    use crate::api::{context_stats, session_key};
    use crate::event_map::event_to_ws;

    /// A `ToolExecutionEnd` whose result carries the given tool name/text,
    /// mirroring what the core emits for denied/timed-out/cancelled calls.
    fn tool_end_event(tool_call_id: &str, tool_name: &str, text: &str) -> AgentEvent {
        AgentEvent::ToolExecutionEnd {
            tool_call_id: tool_call_id.into(),
            result: ToolResultMessage {
                tool_call_id: tool_call_id.into(),
                tool_name: tool_name.into(),
                content: vec![ContentBlock::Text { text: text.into() }],
                is_error: true,
                timestamp: 0,
            },
            is_error: true,
        }
    }

    fn ws_json(event: &AgentEvent, tool_names: &mut HashMap<String, String>) -> Value {
        let ws = event_to_ws(event, tool_names).expect("event maps to an OutgoingEvent");
        serde_json::to_value(&ws).expect("OutgoingEvent serializes")
    }

    #[test]
    fn tool_end_uses_registered_tool_name_when_start_was_seen() {
        let mut tool_names = HashMap::new();
        tool_names.insert("tc1".into(), "bash".into());
        let v = ws_json(&tool_end_event("tc1", "bash", "ok"), &mut tool_names);
        assert_eq!(v["type"], "tool_end");
        assert_eq!(v["name"], "bash");
        assert_eq!(v["output"], "ok");
    }

    #[test]
    fn tool_end_falls_back_to_result_tool_name() {
        // Denied/timed-out/cancelled calls have no preceding ToolExecutionStart,
        // so `tool_names` is empty - the name must come from the result message.
        let mut tool_names = HashMap::new();
        let v = ws_json(
            &tool_end_event("tc1", "bash", "approval denied by user"),
            &mut tool_names,
        );
        assert_eq!(v["type"], "tool_end");
        assert_eq!(v["name"], "bash");
        assert_eq!(v["output"], "approval denied by user");
    }

    #[test]
    fn text_delta_maps_to_content_event() {
        let mut tool_names = HashMap::new();
        let event = AgentEvent::MessageUpdate {
            delta: ContentDelta::TextDelta("hello".into()),
        };
        let v = ws_json(&event, &mut tool_names);
        assert_eq!(v["type"], "content");
        assert_eq!(v["content"], "hello");
    }

    #[test]
    fn unhandled_events_map_to_none() {
        let mut tool_names = HashMap::new();
        let events = [
            AgentEvent::AgentStart,
            AgentEvent::AgentEnd,
            AgentEvent::TurnStart,
            AgentEvent::MessageStart,
            AgentEvent::MessageUpdate {
                delta: ContentDelta::ToolCallDelta {
                    id: "x".into(),
                    name: None,
                    args_delta: "{}".into(),
                },
            },
        ];
        for event in events {
            assert!(
                event_to_ws(&event, &mut tool_names).is_none(),
                "unexpected mapping for {event:?}"
            );
        }
    }

    #[test]
    fn session_key_strips_prefix_and_passes_through_bare_keys() {
        assert_eq!(session_key("websocket:abc123"), "abc123");
        assert_eq!(session_key("abc123"), "abc123");
    }

    /// The `context_stats` scenarios live in ONE test because they mutate the
    /// process-global `GASKET_CONTEXT_WINDOW` env var; a single test function
    /// runs on one thread, so parallel test threads can't race on it.
    #[test]
    fn context_stats_scenarios() {
        // 1. Zero usage -> zero tokens, zero percent (default window).
        std::env::remove_var("GASKET_CONTEXT_WINDOW");
        let stats = context_stats(0, 0, 0);
        assert_eq!(stats["current_tokens"], 0);
        assert_eq!(stats["usage_percent"], 0.0);
        assert_eq!(stats["is_compressing"], false);
        assert_eq!(stats["cumulative_in"], 0);
        assert_eq!(stats["cumulative_out"], 0);

        // 2. Occupancy uses `last_input_tokens` (NOT cumulative in+out).
        // 64k current against the default 128k window = 50%.
        let stats = context_stats(64_000, 100_000, 50_000);
        assert_eq!(stats["current_tokens"], 64_000);
        assert_eq!(stats["usage_percent"], 50.0);
        assert_eq!(stats["cumulative_in"], 100_000);
        assert_eq!(stats["cumulative_out"], 50_000);

        // 3. A configured `GASKET_CONTEXT_WINDOW` is respected.
        std::env::set_var("GASKET_CONTEXT_WINDOW", "50000");
        let stats = context_stats(25_000, 999, 999);
        assert_eq!(stats["current_tokens"], 25_000);
        assert_eq!(stats["usage_percent"], 50.0);

        // 4. Zero window is treated as "no percentage" rather than dividing by zero.
        std::env::set_var("GASKET_CONTEXT_WINDOW", "0");
        let stats = context_stats(1_000, 1_000, 1_000);
        assert_eq!(stats["current_tokens"], 1_000);
        assert_eq!(stats["usage_percent"], 0.0);

        std::env::remove_var("GASKET_CONTEXT_WINDOW");
    }
}
