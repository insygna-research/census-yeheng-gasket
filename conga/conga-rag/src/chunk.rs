//! Structure-aware chunking — pure function.

/// One output chunk. `ordinal` is the 0-based position within the document.
#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    pub ordinal: usize,
    pub content: String,
}

pub fn chunk(text: &str, target_chars: usize, overlap_chars: usize) -> Vec<Chunk> {
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut cur = String::new();
    let mut ordinal = 0usize;

    fn flush(cur: &mut String, chunks: &mut Vec<Chunk>, ordinal: &mut usize) {
        let content = cur.trim().to_string();
        if !content.is_empty() {
            chunks.push(Chunk {
                ordinal: *ordinal,
                content,
            });
            *ordinal += 1;
        }
        cur.clear();
    }

    for para in text.split("\n\n") {
        let is_heading = para
            .lines()
            .next()
            .map(|l| l.starts_with("# ") || l.starts_with("## "))
            .unwrap_or(false);

        for piece in soft_split(para, target_chars) {
            if !cur.is_empty()
                && (is_heading || cur.chars().count() + piece.chars().count() + 2 > target_chars)
            {
                // Emit current chunk; carry the overlap tail into the next.
                let tail = tail_chars(&cur, overlap_chars);
                flush(&mut cur, &mut chunks, &mut ordinal);
                if !tail.is_empty() {
                    cur.push_str(&tail);
                    cur.push_str("\n\n");
                }
            }
            if !cur.is_empty() {
                cur.push_str("\n\n");
            }
            cur.push_str(&piece);
        }
    }
    flush(&mut cur, &mut chunks, &mut ordinal);
    chunks
}

/// Split an oversize paragraph at sentence boundaries, then whitespace, then
/// hard character cuts — never exceeding `target` unless a single
/// non-splittable run is itself longer.
fn soft_split(para: &str, target: usize) -> Vec<String> {
    if para.chars().count() <= target {
        return vec![para.to_string()];
    }
    let chars: Vec<char> = para.chars().collect();
    let mut out = Vec::new();
    let mut start = 0usize;
    while start < chars.len() {
        let end = (start + target).min(chars.len());
        if end == chars.len() {
            out.push(chars[start..end].iter().collect());
            break;
        }
        let mut cut = end;
        for i in (start..end).rev() {
            if "。！？!?；;".contains(chars[i]) {
                cut = i + 1;
                break;
            }
        }
        if cut == end {
            for i in (start..end).rev() {
                if chars[i].is_whitespace() {
                    cut = i + 1;
                    break;
                }
            }
        }
        out.push(chars[start..cut].iter().collect());
        start = cut;
    }
    out
}

/// Last `n` chars of `s`, char-boundary safe.
fn tail_chars(s: &str, n: usize) -> String {
    let count = s.chars().count();
    if n == 0 || count == 0 {
        return String::new();
    }
    let skip = count.saturating_sub(n);
    s.chars().skip(skip).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contents(chunks: &[Chunk]) -> Vec<&str> {
        chunks.iter().map(|c| c.content.as_str()).collect()
    }

    #[test]
    fn short_text_is_one_chunk() {
        let out = chunk("hello world", 100, 10);
        assert_eq!(contents(&out), vec!["hello world"]);
        assert_eq!(out[0].ordinal, 0);
    }

    #[test]
    fn ordinals_are_contiguous() {
        let text = (0..20)
            .map(|i| format!("paragraph {i} {}", "x".repeat(40)))
            .collect::<Vec<_>>()
            .join("\n\n");
        let out = chunk(&text, 150, 20);
        assert!(out.len() > 1);
        for (i, c) in out.iter().enumerate() {
            assert_eq!(c.ordinal, i);
        }
    }

    #[test]
    fn heading_forces_new_chunk() {
        let text = format!("{}aa bb\n\n# Heading\n\ncc dd", "intro ".repeat(10));
        let out = chunk(&text, 200, 0);
        assert!(out.len() >= 2);
        let joined = contents(&out).join("\x1f");
        assert!(
            joined.contains("\x1f# Heading"),
            "heading must start a new chunk"
        );
    }

    #[test]
    fn chunks_respect_target() {
        let text = (0..30)
            .map(|i| format!("para-{i} {}", "y".repeat(30)))
            .collect::<Vec<_>>()
            .join("\n\n");
        let target = 200usize;
        let out = chunk(&text, target, 30);
        for c in &out {
            assert!(
                c.content.chars().count() <= target + 30,
                "chunk too big: {}",
                c.content.chars().count()
            );
        }
    }

    #[test]
    fn oversize_paragraph_soft_splits() {
        let one = "z".repeat(500);
        let out = chunk(&one, 120, 20);
        assert!(
            out.len() >= 4,
            "500 chars / 120 target must split, got {}",
            out.len()
        );
    }

    #[test]
    fn overlap_repeats_tail() {
        let text = (0..10)
            .map(|i| format!("para-{i} {}", "w".repeat(60)))
            .collect::<Vec<_>>()
            .join("\n\n");
        let out = chunk(&text, 150, 30);
        assert!(out.len() > 1);
        let tail: String = out[0]
            .content
            .chars()
            .rev()
            .take(30)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        assert!(
            out[1].content.starts_with(&tail),
            "second chunk should start with overlap tail"
        );
    }

    #[test]
    fn utf8_never_panics() {
        let text = "你好世界。".repeat(100);
        let out = chunk(&text, 37, 9);
        assert!(!out.is_empty());
        for c in &out {
            assert!(c.content.chars().count() > 0);
        }
    }
}
