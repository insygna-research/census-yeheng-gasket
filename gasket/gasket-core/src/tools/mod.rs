//! Built-in tools: read / write / edit / bash / list / grep.
//!
//! See `gasket-refactor-plan.md` §7.

pub mod bash;
pub mod edit;
pub mod grep;
pub mod list;
pub mod read;
pub mod write;

use std::path::{Component, Path, PathBuf};

use crate::types::tool::ToolDefinition;

/// The 6 built-in tools, ready to drop into `AgentContext.tools`.
pub fn built_in_tools() -> Vec<ToolDefinition> {
    vec![
        read::tool(),
        write::tool(),
        edit::tool(),
        bash::tool(),
        list::tool(),
        grep::tool(),
    ]
}

/// Resolve `requested` against `cwd`, rejecting any `..` or absolute component
/// that would escape `cwd`. Lexical only (no symlink resolution) - enough to
/// stop `../../etc/passwd`-style traversal without requiring the target file to
/// exist (so `write` can still create new files).
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
    Ok(resolved)
}
