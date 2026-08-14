//! REST API handlers and helpers (slash commands, context stats, compaction).

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::Json;
use serde_json::{json, Value};
use tracing::{info, warn};

use gasket_core::ToolDefinition;

use crate::state::AppState;

// ── External tools ─────────────────────────────────────────────
use gasket_host::SessionManager;

pub(crate) async fn load_external_tools() -> Vec<ToolDefinition> {
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
/// drives the saturation percentage against `GASKET_CONTEXT_WINDOW` (default
/// 128k). `cumulative_in`/`cumulative_out` are the real accumulated API spend
/// across the session. The percentage is a display heuristic; the token counts
/// themselves are real API usage.
pub(crate) fn context_stats(last_input_tokens: u64, usage_in: u64, usage_out: u64) -> Value {
    let window = std::env::var("GASKET_CONTEXT_WINDOW")
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

/// Compact the session's working memory (append-only JSONL untouched), then
/// return fresh stats. Mirrors the CLI's per-turn compaction.
pub(crate) async fn compact_context(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
) -> Json<Value> {
    let mut stats = context_stats(0, 0, 0);
    if let Some(s) = state.sessions.get(session_key(&key)) {
        let mut s = s.lock().await;
        let mut b = gasket_host::ContextBudget::from_env();
        b.record_input_tokens(s.last_input_tokens);
        s.history = b.compact(&s.history);
        stats = context_stats(s.last_input_tokens, s.usage_in, s.usage_out);
    }
    Json(json!({ "context_stats": stats, "watermark_info": null }))
}

/// List all sessions on disk (id, msg_count, mtime). Does NOT depend on
/// active WS connections — reads the JSONL store directly. Used by the
/// frontend to discover sessions created by the CLI or other devices.
pub(crate) async fn list_sessions() -> Json<Value> {
    let mgr = SessionManager::new();
    match mgr.list().await {
        Ok(mut sessions) => {
            // Newest first.
            sessions.sort_by_key(|s| std::cmp::Reverse(s.mtime));
            Json(json!({
                "sessions": sessions.iter().map(|s| json!({
                    "id": s.id,
                    "msg_count": s.msg_count,
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
