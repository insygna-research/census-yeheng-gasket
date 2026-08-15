//! Built-in tools: read / write / edit / bash / list / grep / fetch / spawn_subagents.

pub mod bash;
pub mod edit;
pub mod fetch;
pub mod grep;
pub mod list;
pub mod read;
pub mod subagent;
pub mod write;

use std::path::{Component, Path, PathBuf};

use crate::types::tool::ToolDefinition;

/// The 8 built-in tools, ready to drop into `AgentContext.tools`.
pub fn built_in_tools() -> Vec<ToolDefinition> {
    vec![
        read::tool(),
        write::tool(),
        edit::tool(),
        bash::tool(),
        list::tool(),
        grep::tool(),
        fetch::tool(),
        subagent::tool(),
    ]
}

/// Shared cap for tool textual output (bash stdout/stderr, fetch body).
pub(crate) const MAX_OUTPUT_BYTES: usize = 200_000;

/// Char-safe truncation to [`MAX_OUTPUT_BYTES`] with an indicator. Never
/// slices through a multi-byte UTF-8 char (a plain `String::truncate` panics
/// when the byte limit lands mid-char).
pub(crate) fn truncate_output(s: &str) -> String {
    if s.len() <= MAX_OUTPUT_BYTES {
        return s.to_string();
    }
    let mut cut = MAX_OUTPUT_BYTES;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = s[..cut].to_string();
    out.push_str("\n...(truncated)");
    out
}

/// Resolve `requested` against `cwd`, rejecting any `..` or absolute component
/// that would escape `cwd`, and re-checking the symlink-resolved target so a
/// symlink inside `cwd` can't point outside it (lexical `..`-checking alone
/// doesn't catch that).
pub(crate) fn resolve_within_cwd(cwd: &Path, requested: &str) -> Result<PathBuf, String> {
    let cwd_canon = cwd
        .canonicalize()
        .map_err(|e| format!("cwd not accessible: {e}"))?;
    let mut resolved = cwd_canon.clone();
    for comp in Path::new(requested).components() {
        match comp {
            Component::CurDir => {}
            Component::Normal(c) => resolved.push(c),
            Component::ParentDir => {
                // Only allow `..` while we're still strictly inside cwd.
                if resolved == cwd_canon || !resolved.starts_with(&cwd_canon) {
                    return Err(format!("path escapes working directory: {requested}"));
                }
                resolved.pop();
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(format!("absolute paths are not allowed: {requested}"));
            }
        }
    }

    // The target (or, if it doesn't exist yet, its nearest existing ancestor)
    // may be reached through a symlink that leads outside cwd. Canonicalize
    // that ancestor and re-check, then re-append the not-yet-existing tail.
    let (existing, tail) = nearest_existing_ancestor(&resolved);
    let existing_canon = existing
        .canonicalize()
        .map_err(|e| format!("path not accessible: {e}"))?;
    if !existing_canon.starts_with(&cwd_canon) {
        return Err(format!("path escapes working directory: {requested}"));
    }
    // `.join` on an empty `tail` still appends a stray trailing separator
    // (turning a file path into a dir-looking one), so skip it when empty.
    if tail.as_os_str().is_empty() {
        Ok(existing_canon)
    } else {
        Ok(existing_canon.join(tail))
    }
}

/// Walk `path` up to the nearest ancestor that exists on disk. Returns that
/// ancestor plus the (necessarily non-existent, so symlink-free) remainder.
fn nearest_existing_ancestor(path: &Path) -> (PathBuf, PathBuf) {
    let mut current = path.to_path_buf();
    let mut names = Vec::new();
    while !current.exists() {
        let Some(name) = current.file_name() else {
            break; // reached the root without finding an existing component
        };
        names.push(name.to_os_string());
        current.pop();
    }
    // `names` was collected leaf-to-root; rebuild root-to-leaf. Pushing onto
    // an empty PathBuf via `.join("")` would add a stray trailing separator,
    // so accumulate with `push` instead.
    let mut tail = PathBuf::new();
    for name in names.into_iter().rev() {
        tail.push(name);
    }
    (current, tail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_lexical_escape() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(resolve_within_cwd(tmp.path(), "../../etc/passwd").is_err());
        assert!(resolve_within_cwd(tmp.path(), "/etc/passwd").is_err());
    }

    #[test]
    fn allows_path_within_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "x").unwrap();
        let resolved = resolve_within_cwd(tmp.path(), "f.txt").unwrap();
        assert_eq!(resolved, tmp.path().canonicalize().unwrap().join("f.txt"));
    }

    #[test]
    fn allows_new_file_in_existing_subdir() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        let resolved = resolve_within_cwd(tmp.path(), "sub/new.txt").unwrap();
        assert_eq!(
            resolved,
            tmp.path().canonicalize().unwrap().join("sub/new.txt")
        );
    }
    #[test]
    fn truncate_output_is_char_safe_at_multibyte_boundary() {
        // 3-byte chars: make the byte limit land inside the final char.
        let n = MAX_OUTPUT_BYTES / 3 + 10;
        let s = "你".repeat(n);
        assert!(s.len() > MAX_OUTPUT_BYTES);
        let out = truncate_output(&s);
        assert!(out.ends_with("...(truncated)"));
        assert!(out.len() < s.len());
        // The retained prefix is valid UTF-8 by construction; re-encode check.
        let trimmed = out.trim_end_matches("\n...(truncated)");
        assert_eq!(trimmed.chars().count() * 3, trimmed.len());
    }

    #[test]
    fn truncate_output_noop_under_limit() {
        assert_eq!(truncate_output("small"), "small");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape() {
        // A symlink inside cwd pointing outside it must not let a request
        // through, even though the lexical (no `..`) check alone would allow it.
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "s3cr3t").unwrap();
        std::os::unix::fs::symlink(outside.path(), tmp.path().join("escape")).unwrap();

        let r = resolve_within_cwd(tmp.path(), "escape/secret.txt");
        assert!(r.is_err(), "symlink escape must be rejected, got {r:?}");
    }
}
