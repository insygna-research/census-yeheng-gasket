//! REST API handlers and helpers (slash commands, context stats, compaction).

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use tracing::{info, warn};

use conga::ToolDefinition;

use crate::state::AppState;

// ── External tools ─────────────────────────────────────────────
use conga_host::SessionManager;

pub(crate) async fn load_external_tools() -> Vec<ToolDefinition> {
    let cmds = conga_host::commands_from_env();
    if cmds.is_empty() {
        return Vec::new();
    }
    match conga_host::load_external_tools(&cmds).await {
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
/// completer renders this list - every entry MUST have a handler in the WS
/// message loop.
pub(crate) async fn get_commands() -> Json<Value> {
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
/// registers under bare `{id}` - strip the prefix before looking up.
pub(crate) fn session_key(key: &str) -> &str {
    key.strip_prefix("websocket:").unwrap_or(key)
}

/// Context occupancy for the frontend. `last_input_tokens` is the current
/// window occupancy (most recent provider-reported input-token count) and
/// drives the saturation percentage against `CONGA_CONTEXT_WINDOW` (default
/// 128k). `cumulative_in`/`cumulative_out` are the real accumulated API spend
/// across the session. The percentage is a display heuristic; the token counts
/// themselves are real API usage.
pub(crate) fn context_stats(last_input_tokens: u64, usage_in: u64, usage_out: u64) -> Value {
    let window = std::env::var("CONGA_CONTEXT_WINDOW")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(128_000);
    let usage_percent = if window > 0 {
        (last_input_tokens as f64 / window as f64) * 100.0
    } else {
        0.0
    };
    json!({
        "current_tokens": last_input_tokens,
        "usage_percent": usage_percent,
        "is_compressing": false,
        "cumulative_in": usage_in,
        "cumulative_out": usage_out,
    })
}

pub(crate) async fn get_context(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
) -> Json<Value> {
    let stats = match state.sessions.get(session_key(&key)) {
        Some(s) => {
            let s = s.lock().await;
            context_stats(s.last_input_tokens, s.usage_in, s.usage_out)
        }
        None => context_stats(0, 0, 0),
    };
    // No watermark/compaction mechanism exists in this architecture - null so
    // the frontend hides the watermark chip instead of rendering undefined.
    Json(json!({ "context_stats": stats, "watermark_info": null }))
}

/// Compaction is now internal to `run_turn`: every turn the host re-derives
/// history from the event log and compacts it in memory (the append-only
/// log itself is never rewritten). This endpoint remains for frontend
/// compatibility and just returns fresh stats.
pub(crate) async fn compact_context(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
) -> Json<Value> {
    let stats = match state.sessions.get(session_key(&key)) {
        Some(s) => {
            let s = s.lock().await;
            context_stats(s.last_input_tokens, s.usage_in, s.usage_out)
        }
        None => context_stats(0, 0, 0),
    };
    Json(json!({ "context_stats": stats, "watermark_info": null }))
}

/// Backend-truth transcript for a session (D3): `derive_messages` over the
/// on-disk event log, migrating a legacy `messages.jsonl` once. Reads disk,
/// not the live connection, so it also serves sessions created by the CLI
/// or other devices. Unknown key → 404; a corrupt log → 500 (fail loud,
/// never silently adopt).
pub(crate) async fn get_messages(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
) -> Response {
    let key = session_key(&key);
    let storage = conga::EventStorage::new(state.store_root.clone());
    if !storage.has_events(key) && !storage.messages_path(key).exists() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("unknown session: {key}") })),
        )
            .into_response();
    }
    let mgr = SessionManager::with_root(state.store_root.clone());
    match mgr.open_or_migrate(key).await {
        Ok(events) => Json(conga::derive_messages(&events)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// List all sessions on disk (id, msg_count, mtime, name). Does NOT depend on
/// active WS connections — reads the JSONL store directly. Used by the
/// frontend to discover sessions created by the CLI or other devices.
pub(crate) async fn list_sessions(State(state): State<Arc<AppState>>) -> Json<Value> {
    let mgr = SessionManager::with_root(state.store_root.clone());
    match mgr.list().await {
        Ok(mut sessions) => {
            // Newest first.
            sessions.sort_by_key(|s| std::cmp::Reverse(s.mtime));
            Json(json!({
                "sessions": sessions.iter().map(|s| json!({
                    "id": s.id,
                    "msg_count": s.msg_count,
                    "name": s.name,
                    "mtime": s.mtime
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0),
                })).collect::<Vec<_>>()
            }))
        }
        Err(e) => {
            warn!("list_sessions error: {e}");
            Json(json!({ "sessions": [], "error": e.to_string() }))
        }
    }
}

#[derive(serde::Deserialize)]
pub(crate) struct SearchParams {
    q: String,
    limit: Option<usize>,
}

/// Full-text search across all sessions' event logs. The first request per
/// process builds/updates the FTS5 sidecar index (reindex-on-demand, then
/// latched); a store/index failure is a 500 (fail loud, same policy as
/// `get_messages`). No hits is a legitimate empty list — not a 404.
/// The engine itself lives in `conga_host::session_index` (feature
/// `session-index`); the gateway is only transport.
pub(crate) async fn search_sessions(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<SearchParams>,
) -> Response {
    let q = params.q.trim().to_string();
    if q.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "q must be non-empty" })),
        )
            .into_response();
    }
    let limit = params.limit.unwrap_or(20).clamp(1, 100);
    let root = state.store_root.clone();
    let db = state.index_db.clone();
    let init = state
        .search_ready
        .get_or_init(|| async {
            tokio::task::spawn_blocking(move || conga_host::session_index::reindex(&root, &db))
                .await
                .map_err(|e| anyhow::anyhow!("engine task join failed: {e}"))?
                .map(|_| ())
        })
        .await;
    if let Err(e) = init {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("index build failed: {e}") })),
        )
            .into_response();
    }
    let root = state.store_root.clone();
    let db = state.index_db.clone();
    let hits = match tokio::task::spawn_blocking(move || {
        conga_host::session_index::search(&root, &db, &q, limit)
    })
    .await
    {
        Ok(Ok(hits)) => hits,
        Ok(Err(e)) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("engine task join failed: {e}") })),
            )
                .into_response();
        }
    };
    Json(json!({ "hits": hits })).into_response()
}

/// Rename a session: persist the display name in the session's `meta.json`
/// sidecar. Creates the session directory if needed, so a chat can be named
/// before its first turn lands on disk. 400 on an unsafe id or bad name.
pub(crate) async fn rename_session(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let key = session_key(&key);
    if !conga::is_valid_session_id(key) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid session id" })),
        )
            .into_response();
    }
    let name = body
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if name.is_empty() || name.chars().count() > 200 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "name must be 1..=200 chars" })),
        )
            .into_response();
    }
    let storage = conga::EventStorage::new(state.store_root.clone());
    let meta = conga::SessionMeta {
        name: Some(name.to_string()),
    };
    match storage.write_meta(key, &meta).await {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// Delete a session's on-disk data wholesale (event log + meta sidecar).
/// Refuses while a live WS connection holds the session (409) — deleting
/// under a running turn would silently restart its log. Unknown key -> 404.
pub(crate) async fn delete_session(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
) -> Response {
    let key = session_key(&key);
    if !conga::is_valid_session_id(key) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid session id" })),
        )
            .into_response();
    }
    if state.sessions.contains_key(key) {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "session has an active connection" })),
        )
            .into_response();
    }
    let storage = conga::EventStorage::new(state.store_root.clone());
    match storage.remove_session(key).await {
        Ok(true) => Json(json!({ "ok": true })).into_response(),
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("unknown session: {key}") })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use axum::routing::{delete, get, put};
    use axum::Router;
    use conga::types::message::{ContentBlock, UserMessage};
    use conga::{AgentMessage, EventStorage, SessionEvent};
    use dashmap::DashMap;
    use tower::util::ServiceExt;

    fn user_event(text: &str) -> SessionEvent {
        SessionEvent::User(AgentMessage::User(UserMessage {
            content: vec![ContentBlock::text(text)],
            timestamp: 1,
        }))
    }

    fn test_state(root: std::path::PathBuf) -> Arc<AppState> {
        Arc::new(AppState {
            sessions: DashMap::new(),
            store_root: root.clone(),
            index_db: root.join("index.db"),
            search_ready: tokio::sync::OnceCell::new(),
        })
    }

    fn api_router(state: Arc<AppState>) -> Router {
        Router::new()
            .route("/api/sessions", get(list_sessions))
            .route("/api/sessions/search", get(search_sessions))
            .route("/api/sessions/{key}/messages", get(get_messages))
            .route("/api/sessions/{key}/name", put(rename_session))
            .route("/api/sessions/{key}", delete(delete_session))
            .with_state(state)
    }

    fn get_uri(uri: &str) -> Request<Body> {
        Request::builder().uri(uri).body(Body::empty()).unwrap()
    }

    #[tokio::test]
    async fn messages_returns_derived_array_for_known_session() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = EventStorage::new(tmp.path().to_path_buf());
        storage
            .append_event("sess-1", &user_event("hello"))
            .await
            .unwrap();
        // Turn markers project away — the endpoint returns messages only.
        storage
            .append_event("sess-1", &SessionEvent::TurnStart)
            .await
            .unwrap();

        let app = api_router(test_state(tmp.path().to_path_buf()));
        // Frontend-style prefixed key exercises the prefix stripping.
        let res = app
            .oneshot(get_uri("/api/sessions/websocket:sess-1/messages"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let arr = v.as_array().expect("top-level JSON array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["role"], "user");
        assert_eq!(arr[0]["content"][0]["type"], "text");
        assert_eq!(arr[0]["content"][0]["text"], "hello");
    }

    #[tokio::test]
    async fn messages_unknown_session_is_404() {
        let tmp = tempfile::tempdir().unwrap();
        let app = api_router(test_state(tmp.path().to_path_buf()));
        let res = app
            .oneshot(get_uri("/api/sessions/never-existed/messages"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn search_route_returns_hits_and_rejects_blank_q() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = EventStorage::new(tmp.path().to_path_buf());
        storage
            .append_event("sess-1", &user_event("the flaky test"))
            .await
            .unwrap();
        let app = api_router(test_state(tmp.path().to_path_buf()));
        let res = app
            .clone()
            .oneshot(get_uri("/api/sessions/search?q=flaky"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["hits"][0]["session_id"], "sess-1");
        assert!(v["hits"][0]["snippet"].as_str().unwrap().contains("flaky"));
        assert!(
            v["hits"][0]["name"].is_null(),
            "unnamed session serializes name as null"
        );
        let res = app
            .oneshot(get_uri("/api/sessions/search?q=%20"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn rename_then_list_shows_name() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = EventStorage::new(tmp.path().to_path_buf());
        storage
            .append_event("sess-1", &user_event("hello"))
            .await
            .unwrap();

        let app = api_router(test_state(tmp.path().to_path_buf()));
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/sessions/sess-1/name")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"name":"Release notes"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let res = app.oneshot(get_uri("/api/sessions")).await.unwrap();
        let body = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["sessions"][0]["id"], "sess-1");
        assert_eq!(v["sessions"][0]["name"], "Release notes");
    }

    #[tokio::test]
    async fn rename_rejects_empty_and_overlong_names() {
        let tmp = tempfile::tempdir().unwrap();
        let app = api_router(test_state(tmp.path().to_path_buf()));
        for body in [r#"{"name":"  "}"#, r#"{"name":""}"#, r#"{"other":1}"#] {
            let res = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("PUT")
                        .uri("/api/sessions/sess-1/name")
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::BAD_REQUEST, "body: {body}");
        }
    }

    #[tokio::test]
    async fn delete_removes_session_from_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = EventStorage::new(tmp.path().to_path_buf());
        storage
            .append_event("sess-1", &user_event("hello"))
            .await
            .unwrap();

        let app = api_router(test_state(tmp.path().to_path_buf()));
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/sessions/sess-1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(!storage.has_events("sess-1"));

        // Second delete is an honest 404, not a silent success.
        let res = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/sessions/sess-1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }
}
