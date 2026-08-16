//! conga WebSocket Gateway - bridges the existing Vue 3 frontend to the
//! conga agent loop via WebSocket + JSON.
//!
//! ## Architecture
//!
//! Each WebSocket connection is one session with one `Host`. The main tokio
//! task loops on incoming messages. When a `"message"` arrives it runs the
//! turn inline (`run_turn`) and multiplexes it against incoming frames
//! (cancel, approvals) in a secondary select loop.
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
//! | `subagent_*`（10 种） | S->C | ✅ 已实现（core 子 agent 编排 + gateway 事件转发 + 前端渲染） |

use std::sync::Arc;

use axum::routing::{delete, get, post, put};
use axum::Router;
use dashmap::DashMap;
use tracing::info;

use crate::api::{
    compact_context, delete_session, get_commands, get_context, get_messages, list_sessions,
    rename_session, search_sessions,
};
use crate::state::AppState;
use crate::ws::ws_handler;

mod api;
mod state;
mod wire;
mod ws;

// ── Axum server ────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let _ = dotenvy::dotenv();

    let state = Arc::new(AppState {
        sessions: DashMap::new(),
        store_root: conga::JsonlStorage::default_root().base_dir_clone(),
        index_db: conga::storage::config_dir().join("index.db"),
        search_ready: tokio::sync::OnceCell::new(),
    });
    let frontend_dist =
        std::env::var("CONGA_GATEWAY_STATIC_DIR").unwrap_or_else(|_| "../web/dist".to_string());

    let app = Router::new()
        .route("/ws", get(ws_handler))
        .route("/api/sessions", get(list_sessions))
        .route("/api/commands", get(get_commands))
        .route("/api/sessions/search", get(search_sessions))
        .route("/api/sessions/{key}/context", get(get_context))
        .route("/api/sessions/{key}/context/compact", post(compact_context))
        .route("/api/sessions/{key}/messages", get(get_messages))
        .route("/api/sessions/{key}/name", put(rename_session))
        .route("/api/sessions/{key}", delete(delete_session))
        .fallback_service(
            tower_http::services::ServeDir::new(&frontend_dist).not_found_service(
                tower_http::services::ServeFile::new(format!("{frontend_dist}/index.html")),
            ),
        )
        .layer(tower_http::cors::CorsLayer::permissive())
        .with_state(state);

    let port = std::env::var("CONGA_GATEWAY_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);

    let addr = format!("0.0.0.0:{port}");
    info!("conga-gateway listening on {addr}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("failed to bind");
    axum::serve(listener, app).await.expect("server error");
}

// ── Unit tests (pure functions only) ──────────────────────────

#[cfg(test)]
mod tests {
    use crate::api::{context_stats, session_key};

    #[test]
    fn session_key_strips_prefix_and_passes_through_bare_keys() {
        assert_eq!(session_key("websocket:abc123"), "abc123");
        assert_eq!(session_key("abc123"), "abc123");
    }

    /// The `context_stats` scenarios live in ONE test because they mutate the
    /// process-global `CONGA_CONTEXT_WINDOW` env var; a single test function
    /// runs on one thread, so parallel test threads can't race on it.
    #[test]
    fn context_stats_scenarios() {
        // 1. Zero usage -> zero tokens, zero percent (default window).
        std::env::remove_var("CONGA_CONTEXT_WINDOW");
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

        // 3. A configured `CONGA_CONTEXT_WINDOW` is respected.
        std::env::set_var("CONGA_CONTEXT_WINDOW", "50000");
        let stats = context_stats(25_000, 999, 999);
        assert_eq!(stats["current_tokens"], 25_000);
        assert_eq!(stats["usage_percent"], 50.0);

        // 4. Zero window is treated as "no percentage" rather than dividing by zero.
        std::env::set_var("CONGA_CONTEXT_WINDOW", "0");
        let stats = context_stats(1_000, 1_000, 1_000);
        assert_eq!(stats["current_tokens"], 1_000);
        assert_eq!(stats["usage_percent"], 0.0);

        std::env::remove_var("CONGA_CONTEXT_WINDOW");
    }
}
