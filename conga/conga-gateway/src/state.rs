//! Shared server state: the session map and per-connection WebSocket session.

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use dashmap::DashMap;
use futures_util::stream::SplitSink;
use tokio::sync::Mutex;

pub(crate) struct AppState {
    pub(crate) sessions: DashMap<String, Arc<Mutex<WsSession>>>,
    /// Root of the on-disk session store (`~/.conga/sessions`). REST
    /// endpoints that read disk (list, messages) and new WS connections
    /// build their `SessionManager` from this root; tests inject a tempdir.
    pub(crate) store_root: PathBuf,
    /// FTS5 sidecar index (production: `~/.conga/index.db`; tests inject
    /// a tempdir path). Reindex is an incremental high-water check run per
    /// search request (see `conga_host::session_api::search_sessions`).
    pub(crate) index_db: PathBuf,
}

/// Per-connection state. The transcript itself is NOT kept here - the
/// on-disk event log is the single source of truth and history is derived
/// from it (`derive_messages`) wherever needed. Only connection-scoped
/// stats live in memory; approvals live in the session's
/// `SessionAssembly::registry` (see ws.rs).
pub(crate) struct WsSession {
    pub(crate) sender: SplitSink<WebSocket, Message>,
    /// Provider-reported token usage accumulated across turns (fed by
    /// `AfterProviderResponse` events in the forwarder).
    pub(crate) usage_in: u64,
    pub(crate) usage_out: u64,
    /// Most recent provider-reported input-token count for this turn (current
    /// window occupancy). Distinct from `usage_in/out` which accumulate cost.
    pub(crate) last_input_tokens: u64,
    /// Turn start timestamp for the `done` summary line (elapsed time).
    /// Set by the main loop just before `run_turn`, read by the forwarder
    /// when it serializes the `done` event.
    pub(crate) turn_start: Option<std::time::Instant>,
}
