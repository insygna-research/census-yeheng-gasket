//! REST API handlers. Thin transport over [`conga_host::session_api`] —
//! validation rules, DTO shapes, and fail-loud policies live in conga-host,
//! shared with the desktop app's Tauri commands. This file only maps
//! `SessionApiError` to HTTP statuses and extracts axum inputs.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use tracing::warn;

use conga_host::SessionApiError;

use crate::state::AppState;

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

/// Map a session-API failure to its HTTP status + error body.
fn err_response(e: &SessionApiError) -> Response {
    let status = match e {
        SessionApiError::BadRequest(_) => StatusCode::BAD_REQUEST,
        SessionApiError::NotFound(_) => StatusCode::NOT_FOUND,
        SessionApiError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, Json(json!({ "error": e.to_string() }))).into_response()
}

/// Context occupancy for the frontend (see `conga_host::wire::context_stats`
/// for the shape). Reads the live connection's counters when present; the
/// window knob is the settings/env-resolved `effective_max_tokens()`
/// (settings.json `maxTokens` > `CONGA_CONTEXT_WINDOW` > 128k).
async fn stats_of(state: &AppState, key: &str) -> Value {
    let max_tokens = conga_host::settings::effective_max_tokens();
    match state.sessions.get(key) {
        Some(s) => {
            let s = s.lock().await;
            conga_host::wire::context_stats(
                s.last_input_tokens,
                s.usage_in,
                s.usage_out,
                s.cache_read,
                s.cache_write,
                max_tokens,
            )
        }
        None => conga_host::wire::context_stats(0, 0, 0, 0, 0, max_tokens),
    }
}

pub(crate) async fn get_context(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
) -> Json<Value> {
    let stats = stats_of(&state, &key).await;
    Json(json!({ "context_stats": stats }))
}

/// Compaction is now internal to `run_turn`: every turn the host re-derives
/// history from the event log and compacts it in memory (the append-only
/// log itself is never rewritten). This endpoint remains for frontend
/// compatibility and just returns fresh stats.
pub(crate) async fn compact_context(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
) -> Json<Value> {
    let stats = stats_of(&state, &key).await;
    Json(json!({ "context_stats": stats }))
}

/// Backend-truth transcript for a session: `derive_messages` over the
/// on-disk event log. Unknown key → 404; a corrupt log → 500 (fail loud,
/// never silently adopt).
pub(crate) async fn get_messages(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
) -> Response {
    match conga_host::session_messages(&state.store_root, &key).await {
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("unknown session: {key}") })),
        )
            .into_response(),
        Ok(Some(messages)) => Json(messages).into_response(),
        Err(e) => {
            warn!("get_messages {key}: {e}");
            err_response(&e)
        }
    }
}

// ── Web-UI LLM env settings (file-backed, applied per LLM call) ──────────

/// The masked settings view: raw API keys never cross this API (the
/// gateway listens on 0.0.0.0 with open CORS). See
/// `conga_host::settings::settings_to_masked_json`.
pub(crate) async fn get_settings() -> Json<Value> {
    Json(conga_host::settings::settings_to_masked_json(
        &conga_host::settings::load_settings(),
    ))
}

/// Validate → merge (blank `apiKey` keeps the stored one) → persist
/// atomically. The next LLM call picks the new provider up (the host
/// re-resolves the provider from this file every turn).
pub(crate) async fn put_settings(Json(payload): Json<Value>) -> Response {
    match conga_host::settings::put_settings(&payload) {
        Ok(masked) => Json(masked).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

/// List all sessions on disk (id, msg_count, mtime, name). Does NOT depend
/// on active WS connections — reads the JSONL store directly. Used by the
/// frontend to discover sessions created by the CLI or other devices.
pub(crate) async fn list_sessions(State(state): State<Arc<AppState>>) -> Json<Value> {
    match conga_host::list_sessions(&state.store_root).await {
        Ok(sessions) => Json(json!({ "sessions": sessions })),
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

/// Full-text search across all sessions' event logs. The engine (and the
/// incremental reindex check) lives in `conga_host::session_api` /
/// `conga_host::session_index`; the gateway is only transport. No hits is a
/// legitimate empty list — not a 404.
pub(crate) async fn search_sessions(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<SearchParams>,
) -> Response {
    let limit = params.limit.unwrap_or(20).clamp(1, 100);
    let root = state.store_root.clone();
    let db = state.index_db.clone();
    let q = params.q;
    match tokio::task::spawn_blocking(move || {
        conga_host::session_api::search_sessions(&root, &db, &q, limit)
    })
    .await
    {
        Ok(Ok(hits)) => Json(json!({ "hits": hits })).into_response(),
        Ok(Err(e)) => err_response(&e),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("engine task join failed: {e}") })),
        )
            .into_response(),
    }
}

/// Rename a session: persist the display name in the session's `meta.json`
/// sidecar. Validation (id whitelist, name 1..=200 chars) is shared with
/// the desktop app in `conga_host::session_api`.
pub(crate) async fn rename_session(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let name = body.get("name").and_then(|v| v.as_str()).unwrap_or("");
    match conga_host::rename_session(&state.store_root, &key, name).await {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => err_response(&e),
    }
}

/// Delete a session's on-disk data wholesale (event log + meta sidecar).
/// Refuses while a live WS connection holds the session (409) — deleting
/// under a running turn would silently restart its log. Unknown key -> 404.
pub(crate) async fn delete_session(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
) -> Response {
    if state.sessions.contains_key(&key) {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "session has an active connection" })),
        )
            .into_response();
    }
    match conga_host::delete_session(&state.store_root, &key).await {
        Ok(true) => Json(json!({ "ok": true })).into_response(),
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("unknown session: {key}") })),
        )
            .into_response(),
        Err(e) => err_response(&e),
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
            auth_token: std::sync::Arc::new("test-token".to_string()),
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
        let res = app
            .oneshot(get_uri("/api/sessions/sess-1/messages"))
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

    // ── Token auth middleware ─────────────────────────────────

    fn authed_router(state: Arc<AppState>) -> Router {
        let for_middleware = state.clone();
        Router::new()
            .route("/api/sessions", get(list_sessions))
            // Stands in for the static SPA fallback: non-/api, non-/ws
            // paths must pass without a token.
            .route("/ping", get(|| async { "pong" }))
            .layer(axum::middleware::from_fn_with_state(
                for_middleware,
                crate::auth::require_token,
            ))
            .with_state(state)
    }

    fn authed_state(root: std::path::PathBuf, token: &str) -> Arc<AppState> {
        Arc::new(AppState {
            sessions: DashMap::new(),
            store_root: root.clone(),
            index_db: root.join("index.db"),
            auth_token: std::sync::Arc::new(token.to_string()),
        })
    }
    #[tokio::test]
    async fn api_without_token_is_401() {
        let tmp = tempfile::tempdir().unwrap();
        let app = authed_router(authed_state(tmp.path().to_path_buf(), "secret"));
        let res = app.oneshot(get_uri("/api/sessions")).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn api_with_wrong_token_is_401() {
        let tmp = tempfile::tempdir().unwrap();
        let app = authed_router(authed_state(tmp.path().to_path_buf(), "secret"));
        let req = Request::builder()
            .uri("/api/sessions")
            .header("authorization", "Bearer wrong")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn api_with_bearer_token_passes() {
        let tmp = tempfile::tempdir().unwrap();
        let app = authed_router(authed_state(tmp.path().to_path_buf(), "secret"));
        let req = Request::builder()
            .uri("/api/sessions")
            .header("authorization", "Bearer secret")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn api_with_query_token_passes() {
        // Browser WebSocket cannot set headers; ?token= must work.
        let tmp = tempfile::tempdir().unwrap();
        let app = authed_router(authed_state(tmp.path().to_path_buf(), "secret"));
        let res = app
            .oneshot(get_uri("/api/sessions?token=secret"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn non_api_path_passes_without_token() {
        // Static SPA assets are exempt: the app must load before the user
        // can enter the token.
        let tmp = tempfile::tempdir().unwrap();
        let app = authed_router(authed_state(tmp.path().to_path_buf(), "secret"));
        let res = app.oneshot(get_uri("/ping")).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }
}
