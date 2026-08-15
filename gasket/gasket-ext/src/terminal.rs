//! `terminal` tool — run commands on a PTY, with run/read/send actions and a
//! per-session output ring buffer. Lives in gasket-ext behind Cargo feature
//! `terminal`; the session registry is process-global within this crate.

use std::collections::VecDeque;

/// Rolling output buffer for one PTY session, capped at MAX_BYTES: pushing
/// past the cap evicts whole oldest chunks until back under it.
struct OutputRing {
    chunks: VecDeque<String>,
    bytes: usize,
}

impl OutputRing {
    const MAX_BYTES: usize = 64 * 1024;

    fn push_str(&mut self, s: &str) {
        let mut s = s.to_string();
        if s.len() > Self::MAX_BYTES {
            // Char-safe tail: never slice through a multi-byte char.
            let mut cut = Self::MAX_BYTES;
            while !s.is_char_boundary(cut) {
                cut -= 1;
            }
            s = s[cut..].to_string();
        }
        self.bytes += s.len();
        self.chunks.push_back(s);
        while self.bytes > Self::MAX_BYTES {
            let Some(front) = self.chunks.pop_front() else {
                break;
            };
            self.bytes -= front.len();
        }
    }

    /// Take everything buffered (empty string when nothing new).
    fn drain(&mut self) -> String {
        let out: String = self.chunks.drain(..).collect();
        self.bytes = 0;
        out
    }
}

impl Default for OutputRing {
    fn default() -> Self {
        Self {
            chunks: VecDeque::new(),
            bytes: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_returns_and_clears() {
        let mut r = OutputRing::default();
        r.push_str("hello\n");
        r.push_str("world\n");
        assert_eq!(r.drain(), "hello\nworld\n");
        assert_eq!(r.drain(), "", "second drain is empty");
    }

    #[test]
    fn cap_evicts_oldest_first() {
        let mut r = OutputRing::default();
        // 2 chunks of half the cap (plus 1) -> over cap after the second push.
        r.push_str(&"a".repeat(OutputRing::MAX_BYTES / 2));
        r.push_str(&"b".repeat(OutputRing::MAX_BYTES / 2 + 1));
        let out = r.drain();
        assert!(out.starts_with('b'), "oldest chunk evicted first");
        assert!(out.len() <= OutputRing::MAX_BYTES);
    }

    #[test]
    fn oversized_single_chunk_is_truncated_to_cap() {
        let mut r = OutputRing::default();
        r.push_str(&"x".repeat(OutputRing::MAX_BYTES * 2));
        assert!(r.drain().len() <= OutputRing::MAX_BYTES);
    }
}
