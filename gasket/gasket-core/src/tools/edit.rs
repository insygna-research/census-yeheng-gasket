//! `edit` tool — replace text in a file, written atomically.
//!
//! Exact match is tried first. If the literal `old_text` isn't found, a fuzzy
//! fallback tolerates whitespace and curly-quote differences (models frequently
//! mangle indentation, trailing whitespace, or quote style). The fuzzy match is
//! mapped back to the original byte range so the replacement lands exactly
//! where intended. In all cases `old_text` must resolve to exactly one location.

use std::sync::Arc;

use crate::types::tool::{ToolCallCtx, ToolDefinition, ToolResult};
use crate::ContentBlock;

pub fn tool() -> ToolDefinition {
    ToolDefinition {
        name: "edit".into(),
        label: "Edit".into(),
        description: "Replace old_text with new_text in a file. old_text must be unique. Tolerates whitespace/quote differences.".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "old_text": { "type": "string" },
                "new_text": { "type": "string" }
            },
            "required": ["path", "old_text", "new_text"]
        }),
        execute: Arc::new(|ctx| Box::pin(execute(ctx))),
    }
}

async fn execute(ctx: ToolCallCtx) -> Result<ToolResult, crate::error::ToolError> {
    if ctx.aborted() {
        return Ok(ToolResult::error("aborted".to_string()));
    }
    let path = ctx.args["path"]
        .as_str()
        .ok_or_else(|| crate::error::ToolError::Message("path is required".into()))?;
    let old_text = ctx.args["old_text"]
        .as_str()
        .ok_or_else(|| crate::error::ToolError::Message("old_text is required".into()))?;
    let new_text = ctx.args["new_text"]
        .as_str()
        .ok_or_else(|| crate::error::ToolError::Message("new_text is required".into()))?;

    let full = ctx.ctx.cwd.join(path);
    let original = tokio::fs::read_to_string(&full).await?;

    let count = original.matches(old_text).count();
    if count > 1 {
        return Ok(ToolResult::error(format!(
            "old_text appears {} times in {}; it must be unique",
            count, path
        )));
    }

    let (updated, matched_via) = if count == 1 {
        (original.replacen(old_text, new_text, 1), "exact")
    } else {
        // count == 0: fall back to whitespace/quote-tolerant fuzzy match.
        match fuzzy_locate(&original, old_text) {
            Fuzzy::None => {
                return Ok(ToolResult::error(format!(
                    "old_text not found in {path} (exact or fuzzy match)"
                )))
            }
            Fuzzy::Many(n) => {
                return Ok(ToolResult::error(format!(
                    "old_text matches {n} times after whitespace/quote normalization in {path}; it must be unique"
                )))
            }
            Fuzzy::Unique(start, end) => {
                let mut u = String::with_capacity(original.len() + new_text.len());
                u.push_str(&original[..start]);
                u.push_str(new_text);
                u.push_str(&original[end..]);
                (u, "fuzzy")
            }
        }
    };

    // Atomic write via temp file + rename.
    let tmp = full.with_extension("gasket-tmp");
    tokio::fs::write(&tmp, &updated).await?;
    tokio::fs::rename(&tmp, &full).await?;

    Ok(ToolResult {
        content: vec![ContentBlock::text(format!("edited {}", path))],
        details: serde_json::json!({"path": path, "match": matched_via}),
        is_error: false,
    })
}

/// Result of a fuzzy (whitespace- and quote-tolerant) search for `old_text`.
enum Fuzzy {
    /// No match even after normalization.
    None,
    /// Exactly one match; byte range (start inclusive, end exclusive) in the
    /// original text.
    Unique(usize, usize),
    /// Multiple matches after normalization; still ambiguous.
    Many(usize),
}

/// Locate `old_text` in `original` ignoring whitespace and treating curly quotes
/// as their ASCII equivalents. Returns [`Fuzzy::Unique`] with the byte range in
/// the *original* string when there is exactly one normalized match.
fn fuzzy_locate(original: &str, old_text: &str) -> Fuzzy {
    let (norm_file, map) = normalize_fuzzy(original);
    let (norm_old, _) = normalize_fuzzy(old_text);
    if norm_old.is_empty() {
        return Fuzzy::None;
    }

    // Collect non-overlapping match start offsets (byte offsets within norm_file).
    let mut starts: Vec<usize> = Vec::new();
    let mut from = 0usize;
    while let Some(rel) = norm_file[from..].find(&norm_old) {
        let abs = from + rel;
        starts.push(abs);
        from = abs + norm_old.len();
    }
    match starts.len() {
        0 => Fuzzy::None,
        1 => {
            let start_byte = starts[0];
            let end_byte = start_byte + norm_old.len();
            // Convert norm_file byte offsets -> char indices -> original byte offsets.
            let start_char = norm_file[..start_byte].chars().count();
            let end_char = norm_file[..end_byte].chars().count(); // exclusive
            let orig_start = map[start_char];
            let last_off = map[end_char - 1];
            let last_len = original[last_off..].chars().next().map_or(0, |c| c.len_utf8());
            Fuzzy::Unique(orig_start, last_off + last_len)
        }
        n => Fuzzy::Many(n),
    }
}

/// Normalize for fuzzy matching: drop all whitespace and convert curly/smart
/// quotes to straight ASCII quotes. Returns the normalized string plus a map
/// from each normalized char's index to the byte offset of its source char in
/// the input (so a normalized match can be mapped back to the original).
fn normalize_fuzzy(s: &str) -> (String, Vec<usize>) {
    let mut out = String::with_capacity(s.len());
    let mut map = Vec::with_capacity(s.len());
    for (byte_off, ch) in s.char_indices() {
        if ch.is_whitespace() {
            continue;
        }
        let n = match ch {
            '\u{2018}' | '\u{2019}' => '\'', // ‘ ’
            '\u{201C}' | '\u{201D}' => '"',  // “ ”
            c => c,
        };
        out.push(n);
        map.push(byte_off);
    }
    (out, map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::tool::ToolContext;
    use std::sync::atomic::AtomicBool;

    async fn run(args: serde_json::Value, cwd: &std::path::Path) -> ToolResult {
        let t = tool();
        (t.execute)(ToolCallCtx {
            tool_call_id: "x".into(),
            args,
            signal: Arc::new(AtomicBool::new(false)),
            ctx: ToolContext {
                cwd: cwd.to_path_buf(),
                env: Default::default(),
                session_id: "s".into(),
                state_dir: cwd.to_path_buf(),
            },
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn replaces_unique_match() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("f.txt"), "foo bar baz")
            .await
            .unwrap();
        let r = run(
            serde_json::json!({"path": "f.txt", "old_text": "bar", "new_text": "QUX"}),
            tmp.path(),
        )
        .await;
        assert!(!r.is_error);
        assert_eq!(
            tokio::fs::read_to_string(tmp.path().join("f.txt"))
                .await
                .unwrap(),
            "foo QUX baz"
        );
        assert_eq!(r.details["match"], "exact");
    }

    #[tokio::test]
    async fn errors_on_multiple_matches() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("f.txt"), "x x x")
            .await
            .unwrap();
        let r = run(
            serde_json::json!({"path": "f.txt", "old_text": "x", "new_text": "y"}),
            tmp.path(),
        )
        .await;
        assert!(r.is_error);
    }

    #[tokio::test]
    async fn errors_on_missing_match() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("f.txt"), "abc")
            .await
            .unwrap();
        let r = run(
            serde_json::json!({"path": "f.txt", "old_text": "zzz", "new_text": "y"}),
            tmp.path(),
        )
        .await;
        assert!(r.is_error);
    }

    #[tokio::test]
    async fn fuzzy_matches_whitespace_diffs() {
        // File has extra spaces; old_text uses single spaces. Exact fails, fuzzy lands.
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("f.txt"), "foo    bar    baz")
            .await
            .unwrap();
        let r = run(
            serde_json::json!({"path": "f.txt", "old_text": "foo bar baz", "new_text": "X"}),
            tmp.path(),
        )
        .await;
        assert!(!r.is_error);
        assert_eq!(r.details["match"], "fuzzy");
        assert_eq!(
            tokio::fs::read_to_string(tmp.path().join("f.txt"))
                .await
                .unwrap(),
            "X"
        );
    }

    #[tokio::test]
    async fn fuzzy_matches_smart_quotes() {
        // File uses curly quotes; old_text uses straight quotes.
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("f.txt"), "say \u{201C}hello\u{201D} now")
            .await
            .unwrap();
        let r = run(
            serde_json::json!({"path": "f.txt", "old_text": "say \"hello\" now", "new_text": "hi"}),
            tmp.path(),
        )
        .await;
        assert!(!r.is_error);
        assert_eq!(r.details["match"], "fuzzy");
        assert_eq!(
            tokio::fs::read_to_string(tmp.path().join("f.txt"))
                .await
                .unwrap(),
            "hi"
        );
    }

    #[tokio::test]
    async fn fuzzy_preserves_surrounding_text() {
        // Fuzzy match must only replace the matched span, leaving the rest intact.
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("f.txt"), "HEAD  middle  TAIL")
            .await
            .unwrap();
        let r = run(
            serde_json::json!({"path": "f.txt", "old_text": "middle", "new_text": "M"}),
            tmp.path(),
        )
        .await;
        // Exact match exists here, so it should be exact, not fuzzy.
        assert_eq!(r.details["match"], "exact");
        assert_eq!(
            tokio::fs::read_to_string(tmp.path().join("f.txt"))
                .await
                .unwrap(),
            "HEAD  M  TAIL"
        );
    }

    #[tokio::test]
    async fn fuzzy_errors_on_multiple_after_norm() {
        let tmp = tempfile::tempdir().unwrap();
        // Two whitespace-normalized occurrences of "a b".
        tokio::fs::write(tmp.path().join("f.txt"), "a  b and a  b")
            .await
            .unwrap();
        let r = run(
            serde_json::json!({"path": "f.txt", "old_text": "a b", "new_text": "z"}),
            tmp.path(),
        )
        .await;
        assert!(r.is_error);
    }
}
