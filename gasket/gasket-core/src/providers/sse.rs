//! Server-Sent Events line splitter.
//!
//! Shared by [`crate::providers::openai_compat`] and
//! [`crate::providers::anthropic`]. Both providers emit `data: {json}\n\n`
//! frames; this module turns a collected response body into payload strings.
//!
//! Handles the SSE spec essentials: `data:` prefix stripping, multi-line
//! `data:` accumulation, `[DONE]` sentinel, and `event:`/`id:` lines (ignored).

/// Parse a complete SSE frame buffer into its `data:` payload lines.
///
/// Returns one `String` per `data:` field (a frame may carry several). Lines
/// that are not `data:` are dropped. A `data:` line whose payload is exactly
/// `[DONE]` yields `None` to signal end-of-stream to the caller.
pub fn parse_sse_frame(frame: &str) -> Vec<Option<String>> {
    let mut payloads = Vec::new();
    let mut current_data = String::new();
    let mut have_data = false;

    for line in frame.split('\n') {
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("data:") {
            have_data = true;
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            if !current_data.is_empty() {
                current_data.push('\n');
            }
            current_data.push_str(rest);
        } else if line.is_empty() {
            // Blank line terminates the current event.
            if have_data {
                if current_data.trim() == "[DONE]" {
                    payloads.push(None);
                } else {
                    payloads.push(Some(std::mem::take(&mut current_data)));
                }
                current_data.clear();
                have_data = false;
            }
        }
        // event:/id:/retry:/comment lines are intentionally ignored.
    }

    // Flush a trailing non-blank-terminated frame.
    if have_data && !current_data.is_empty() {
        if current_data.trim() == "[DONE]" {
            payloads.push(None);
        } else {
            payloads.push(Some(current_data));
        }
    }

    payloads
}

/// Parse an entire SSE response body (collected text) into payload lines.
///
/// Returns one `Some(json)` per `data:` field, and `None` where the body
/// contained the `[DONE]` sentinel.
pub fn parse_sse_body(body: &str) -> Vec<Option<String>> {
    parse_sse_frame(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_data_frame() {
        let payloads = parse_sse_frame("data: {\"hello\":1}\n\n");
        assert_eq!(payloads, vec![Some("{\"hello\":1}".to_string())]);
    }

    #[test]
    fn parses_done_sentinel() {
        let payloads = parse_sse_frame("data: [DONE]\n\n");
        assert_eq!(payloads, vec![None]);
    }

    #[test]
    fn ignores_non_data_lines() {
        let frame = "event: message\ndata: {\"x\":2}\n\n";
        let payloads = parse_sse_frame(frame);
        assert_eq!(payloads, vec![Some("{\"x\":2}".to_string())]);
    }

    #[test]
    fn parses_multiple_frames_in_one_buffer() {
        let buf = "data: a\n\ndata: b\n\n";
        let payloads = parse_sse_frame(buf);
        assert_eq!(payloads, vec![Some("a".to_string()), Some("b".to_string())]);
    }
}
