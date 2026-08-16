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

/// Incremental SSE frame splitter for a live byte stream.
///
/// A frame ends at a blank line (`\n\n` or `\r\n\r\n`). Bytes arrive split at
/// arbitrary boundaries — mid-frame, mid-UTF-8 — so [`push`](Self::push)
/// buffers until a full separator is present and each complete frame goes
/// through [`parse_sse_frame`]. [`finish`](Self::finish) flushes a trailing
/// frame whose blank line never arrived (some servers omit the final one).
pub(crate) struct SseFrameSplitter {
    buf: Vec<u8>,
}

impl SseFrameSplitter {
    pub(crate) fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Feed freshly received bytes; returns every complete frame with its
    /// terminating separator removed.
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(bytes);
        let mut frames = Vec::new();
        while let Some((start, end)) = find_separator(&self.buf) {
            let frame = String::from_utf8_lossy(&self.buf[..start]).into_owned();
            self.buf.drain(..end);
            frames.push(frame);
        }
        frames
    }

    /// End of stream: return a trailing frame that was never terminated by a
    /// blank line, if any bytes remain buffered.
    pub(crate) fn finish(&mut self) -> Vec<String> {
        if self.buf.is_empty() {
            return Vec::new();
        }
        let frame = String::from_utf8_lossy(&self.buf).into_owned();
        self.buf.clear();
        vec![frame]
    }
}

impl Default for SseFrameSplitter {
    fn default() -> Self {
        Self::new()
    }
}

/// Find the earliest frame separator in `buf`. Returns `(start, end)` byte
/// offsets of the separator (the frame is `buf[..start]`). The two spellings
/// cannot overlap (`\r\n\r\n` never contains `\n\n`), so the earliest match
/// is unambiguous.
fn find_separator(buf: &[u8]) -> Option<(usize, usize)> {
    let lf_lf = buf
        .windows(2)
        .position(|w| w == b"\n\n")
        .map(|i| (i, i + 2));
    let crlf_crlf = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| (i, i + 4));
    match (lf_lf, crlf_crlf) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (Some(a), None) | (None, Some(a)) => Some(a),
        (None, None) => None,
    }
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

    #[test]
    fn splitter_handles_chunks_split_mid_frame() {
        let mut s = SseFrameSplitter::new();
        // Frame 1 arrives in two pieces, frame 2 whole.
        assert!(s.push(b"data: {\"a\"").is_empty());
        assert!(s.push(b":1}\n").is_empty());
        assert_eq!(
            s.push(b"\ndata: {\"b\":2}\n\n"),
            vec!["data: {\"a\":1}".to_string(), "data: {\"b\":2}".to_string()]
        );
        assert!(s.push(b"data: partial").is_empty());
        assert_eq!(s.finish(), vec!["data: partial".to_string()]);
    }

    #[test]
    fn splitter_handles_crlf_separators_and_split_utf8() {
        let frame = "data: {\"t\":\"你\"}"; // multi-byte UTF-8 in the payload
        let wire = format!("{frame}\r\n\r\n");
        // Split at every possible byte boundary; the frame must survive intact.
        for cut in 1..wire.len() {
            let mut s = SseFrameSplitter::new();
            assert!(s.push(&wire.as_bytes()[..cut]).is_empty(), "cut {cut}");
            let frames = s.push(&wire.as_bytes()[cut..]);
            assert_eq!(frames, vec![frame.to_string()], "cut {cut}");
        }
    }

    #[test]
    fn splitter_eager_multiple_frames_per_chunk() {
        let mut s = SseFrameSplitter::new();
        let frames = s.push(b"data: a\n\ndata: b\n\ndata: c\n\n");
        assert_eq!(
            frames,
            vec![
                "data: a".to_string(),
                "data: b".to_string(),
                "data: c".to_string()
            ]
        );
        assert!(s.finish().is_empty());
    }

    #[test]
    fn complete_frame_yields_payload_without_trailing_blank() {
        // A frame extracted by the splitter has no terminating blank line;
        // parse_sse_frame's trailing-flush path must still yield its payload.
        let payloads = parse_sse_frame("data: {\"x\":1}");
        assert_eq!(payloads, vec![Some("{\"x\":1}".to_string())]);
    }
}
