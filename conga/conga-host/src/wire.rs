//! Wire DTO for the frontend's streaming chat protocol.
//!
//! This schema is owned by the host (single definition) and shared by every
//! transport: the gateway serializes it onto a WebSocket, the Tauri desktop
//! backend emits the same JSON as the payload of its `chat-event` IPC event.
//! Field-for-field compatibility with the frontend's `processWebSocketMessage`
//! is the contract - add fields, never rename.

use serde::Serialize;

/// One server -> client event. Optional fields are omitted from the JSON when
/// absent, matching the original gateway wire shape exactly.
#[derive(Serialize)]
pub struct OutgoingEvent {
    #[serde(rename = "type")]
    event_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    /// Stable id pairing a `tool_start` with a `tool_end` (only on those).
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    arguments: Option<String>,
    /// Human-readable diff preview for `approval_request` (edit/write).
    #[serde(skip_serializing_if = "Option::is_none")]
    preview: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    /// Cumulative input tokens for the whole session (only on `done`).
    #[serde(skip_serializing_if = "Option::is_none")]
    usage_in: Option<u64>,
    /// Cumulative output tokens for the whole session (only on `done`).
    #[serde(skip_serializing_if = "Option::is_none")]
    usage_out: Option<u64>,
    /// Wall-clock duration of this turn in milliseconds (only on `done`).
    #[serde(skip_serializing_if = "Option::is_none")]
    elapsed_ms: Option<u64>,
}

impl OutgoingEvent {
    fn base(event_type: &'static str) -> Self {
        Self {
            event_type,
            id: None,
            tool_call_id: None,
            tool_name: None,
            description: None,
            content: None,
            name: None,
            arguments: None,
            preview: None,
            output: None,
            message: None,
            usage_in: None,
            usage_out: None,
            elapsed_ms: None,
        }
    }

    pub fn content(s: String) -> Self {
        let mut ev = Self::base("content");
        ev.content = Some(s);
        ev
    }
    pub fn thinking(s: String) -> Self {
        let mut ev = Self::base("thinking");
        ev.content = Some(s);
        ev
    }
    pub fn tool_start(name: String, args: String, tool_call_id: String) -> Self {
        let mut ev = Self::base("tool_start");
        ev.name = Some(name);
        ev.arguments = Some(args);
        ev.tool_call_id = Some(tool_call_id);
        ev
    }
    pub fn tool_end(name: String, output: String, tool_call_id: String) -> Self {
        let mut ev = Self::base("tool_end");
        ev.name = Some(name);
        ev.output = Some(output);
        ev.tool_call_id = Some(tool_call_id);
        ev
    }
    pub fn error(msg: String) -> Self {
        let mut ev = Self::base("error");
        ev.content = Some(msg.clone());
        ev.message = Some(msg);
        ev
    }
    pub fn done() -> Self {
        Self::base("done")
    }
    /// Turn-boundary `done` carrying a usage summary. The frontend renders
    /// one line: elapsed time + cumulative input/output tokens.
    pub fn done_with_summary(usage_in: u64, usage_out: u64, elapsed_ms: u64) -> Self {
        let mut ev = Self::base("done");
        ev.usage_in = Some(usage_in);
        ev.usage_out = Some(usage_out);
        ev.elapsed_ms = Some(elapsed_ms);
        ev
    }
    /// Reply to a message received while a turn is already running. Kept
    /// distinct from `error` so the frontend can show a toast without
    /// clearing the in-flight conversation state.
    pub fn busy(msg: String) -> Self {
        let mut ev = Self::base("busy");
        ev.content = Some(msg.clone());
        ev.message = Some(msg);
        ev
    }
    /// Acknowledgment for a mid-turn user message that was queued for
    /// steering. The frontend renders the text as a pending user bubble.
    pub fn queued(text: String) -> Self {
        let mut ev = Self::base("queued");
        ev.message = Some(text);
        ev
    }
    pub fn approval_request(
        request_id: String,
        tool_name: String,
        args: &serde_json::Value,
        preview: Option<String>,
    ) -> Self {
        // description 给前端展示；arguments 保留原始参数。截断防超长。
        let desc = serde_json::to_string(args).unwrap_or_default();
        let desc = if desc.chars().count() > 300 {
            format!("{}...", desc.chars().take(300).collect::<String>())
        } else {
            desc
        };
        let mut ev = Self::base("approval_request");
        ev.id = Some(request_id);
        ev.tool_name = Some(tool_name);
        ev.description = Some(desc);
        ev.arguments = Some(args.to_string());
        ev.preview = preview;
        ev
    }
}

// ── Context-occupancy payload (shared by the gateway REST route and the
// desktop `get_context` command — one JSON shape, one window knob) ────────

/// Context occupancy for the frontend. `last_input_tokens` is the current
/// window occupancy (the most recent provider-reported input-token count)
/// and drives the saturation percentage against `CONGA_CONTEXT_WINDOW`
/// (default 128k). `cumulative_in`/`cumulative_out` are the real
/// accumulated API spend across the session. The percentage is a display
/// heuristic; the token counts themselves are real API usage.
pub fn context_stats(last_input_tokens: u64, usage_in: u64, usage_out: u64) -> serde_json::Value {
    let window: u64 = std::env::var("CONGA_CONTEXT_WINDOW")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(128_000);
    let usage_percent = if window > 0 {
        (last_input_tokens as f64 / window as f64) * 100.0
    } else {
        0.0
    };
    serde_json::json!({
        "current_tokens": last_input_tokens,
        "usage_percent": usage_percent,
        "is_compressing": false,
        "cumulative_in": usage_in,
        "cumulative_out": usage_out,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The scenarios live in ONE test because they mutate the process-global
    /// `CONGA_CONTEXT_WINDOW` env var; a single test function runs on one
    /// thread, so parallel test threads can't race on it.
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

        // 4. Zero window is "no percentage", not a division by zero.
        std::env::set_var("CONGA_CONTEXT_WINDOW", "0");
        let stats = context_stats(1_000, 1_000, 1_000);
        assert_eq!(stats["current_tokens"], 1_000);
        assert_eq!(stats["usage_percent"], 0.0);

        std::env::remove_var("CONGA_CONTEXT_WINDOW");
    }
}
