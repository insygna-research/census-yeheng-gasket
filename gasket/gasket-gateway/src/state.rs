//! Shared server state: the session map and per-connection WebSocket session.

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use dashmap::DashMap;
use futures_util::stream::SplitSink;
use gasket_core::AgentMessage;
use tokio::sync::Mutex;

use crate::approval::ApprovalRegistry;

pub(crate) struct AppState {
    pub(crate) sessions: DashMap<String, Arc<Mutex<WsSession>>>,
}

pub(crate) struct WsSession {
    pub(crate) sender: SplitSink<WebSocket, Message>,
    pub(crate) history: Vec<AgentMessage>,
    /// Provider-reported token usage accumulated across turns (fed by
    /// `AfterProviderResponse` events in the forwarder).
    pub(crate) usage_in: u64,
    pub(crate) usage_out: u64,
    /// Most recent provider-reported input-token count for this turn (current
    /// window occupancy). Distinct from `usage_in/out` which accumulate cost.
    pub(crate) last_input_tokens: u64,
    pub(crate) registry: ApprovalRegistry,
}
