//! Inbound wire protocol types sent by the frontend over WebSocket/JSON.
//! (The outbound event schema is owned by the host: `gasket_host::wire`.)

use serde::Deserialize;

#[derive(Deserialize)]
pub(crate) struct IncomingMessage {
    #[serde(rename = "type")]
    pub(crate) msg_type: String,
    pub(crate) content: Option<String>,
    pub(crate) trace_id: Option<String>,
}

/// Inbound `{"type":"approval_response","request_id":"ap1","approved":true,"remember":false}`
/// from the frontend. `remember` is optional (defaults false).
#[derive(Deserialize)]
pub(crate) struct ApprovalResponse {
    pub(crate) request_id: String,
    pub(crate) approved: bool,
    #[serde(default)]
    pub(crate) remember: bool,
}
