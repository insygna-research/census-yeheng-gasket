//! Human-readable previews for tool approval requests.
//!
//! The approval dialog used to show raw JSON arguments. For file-mutating
//! tools (`edit`, `write`) the user needs to see the CHANGE, not the schema:
//! `edit` hunks render as old→new, `write` over an existing file renders as
//! a line diff; a `write` of a new file shows its head. Tools whose
//! arguments already read well (`bash` command strings) get `None` and the
//! dialog falls back to the arguments view.

use std::path::Path;

/// Build the preview shown in the approval dialog. `None` = no preview
/// (dialog renders the raw arguments instead).
pub fn approval_preview(tool: &str, args: &serde_json::Value, cwd: &Path) -> Option<String> {
    match tool {
        "edit" => edit_preview(args, cwd),
        "write" => write_preview(args, cwd),
        _ => None,
    }
}

fn edit_preview(args: &serde_json::Value, _cwd: &Path) -> Option<String> {
    let path = args["path"].as_str()?;
    let edits = args["edits"].as_array()?;
    if edits.is_empty() {
        return None;
    }
    let mut out = format!("# {path}\n");
    for e in edits {
        let old_text = e["old_text"].as_str()?;
        let new_text = e["new_text"].as_str().unwrap_or("");
        out.push_str(&hunk_diff(old_text, new_text));
    }
    Some(out)
}

/// Longest-common-subsequence line diff with `- `/`+ ` prefixes. Bounded:
/// inputs over 1_000 lines are truncated (head) before diffing so a
/// pathological preview cannot burn the approval dialog.
fn line_diff(a: &str, b: &str) -> String {
    const MAX_LINES: usize = 1_000;
    let a_lines: Vec<&str> = a.lines().take(MAX_LINES).collect();
    let b_lines: Vec<&str> = b.lines().take(MAX_LINES).collect();
    let (n, m) = (a_lines.len(), b_lines.len());

    // LCS table (u16 to keep the O(n*m) allocation tame for the cap).
    let mut lcs = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if a_lines[i] == b_lines[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i][j + 1].max(lcs[i + 1][j])
            };
        }
    }

    let mut out = String::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a_lines[i] == b_lines[j] {
            out.push_str("  ");
            out.push_str(a_lines[i]);
            out.push('\n');
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            out.push_str("- ");
            out.push_str(a_lines[i]);
            out.push('\n');
            i += 1;
        } else {
            out.push_str("+ ");
            out.push_str(b_lines[j]);
            out.push('\n');
            j += 1;
        }
    }
    while i < n {
        out.push_str("- ");
        out.push_str(a_lines[i]);
        out.push('\n');
        i += 1;
    }
    while j < m {
        out.push_str("+ ");
        out.push_str(b_lines[j]);
        out.push('\n');
        j += 1;
    }
    out
}

/// Lexical resolve for preview reads (best effort; the tool's own
/// confinement governs execution, previews only need to find the file).
fn resolve_for_preview(cwd: &Path, requested: &str) -> std::path::PathBuf {
    let p = Path::new(requested);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

fn write_preview(args: &serde_json::Value, cwd: &Path) -> Option<String> {
    let path = args["path"].as_str()?;
    let content = args["content"].as_str()?;
    let full = resolve_for_preview(cwd, path);
    match std::fs::read_to_string(&full) {
        Ok(existing) => {
            if existing == content {
                return Some(format!("# {path}\n(no changes)"));
            }
            Some(format!("# {path}\n{}", line_diff(&existing, content)))
        }
        Err(_) => {
            // New file: show its head.
            let lines: Vec<&str> = content.lines().collect();
            let head: Vec<&str> = lines.iter().take(20).copied().collect();
            let mut s = format!("# {path} (new file)\n");
            for l in head {
                s.push_str("+ ");
                s.push_str(l);
                s.push('\n');
            }
            if lines.len() > 20 {
                s.push_str(&format!("+ ... ({} more lines)\n", lines.len() - 20));
            }
            Some(s)
        }
    }
}

/// One edit hunk: show removed lines then added lines (git-style prefixes),
/// keeping the block small.
fn hunk_diff(old_text: &str, new_text: &str) -> String {
    if old_text == new_text {
        return "(no textual change)\n".into();
    }
    format!("{}\n", line_diff(old_text, new_text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_preview_shows_hunks() {
        let args = serde_json::json!({
            "path": "src/lib.rs",
            "edits": [
                {"old_text": "fn old() {\n    stop();\n}", "new_text": "fn old() {\n    start();\n}"}
            ]
        });
        let p = edit_preview(&args, Path::new("/tmp")).unwrap();
        assert!(p.contains("# src/lib.rs"));
        assert!(p.contains("-     stop();"));
        assert!(p.contains("+     start();"));
    }

    #[test]
    fn edit_preview_multi_hunk() {
        let args = serde_json::json!({
            "path": "a.txt",
            "edits": [
                {"old_text": "one", "new_text": "ONE"},
                {"old_text": "two", "new_text": "TWO"}
            ]
        });
        let p = edit_preview(&args, Path::new("/tmp")).unwrap();
        assert!(p.contains("- one"));
        assert!(p.contains("+ ONE"));
        assert!(p.contains("- two"));
        assert!(p.contains("+ TWO"));
    }

    #[test]
    fn write_existing_file_diffs() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "alpha\nbeta\n").unwrap();
        let args = serde_json::json!({"path": "f.txt", "content": "alpha\ngamma\n"});
        let p = write_preview(&args, tmp.path()).unwrap();
        assert!(p.contains("# f.txt"));
        assert!(p.contains("- beta"));
        assert!(p.contains("+ gamma"));
    }

    #[test]
    fn write_new_file_shows_head() {
        let tmp = tempfile::tempdir().unwrap();
        let args = serde_json::json!({"path": "new.txt", "content": "l1\nl2\nl3\n"});
        let p = write_preview(&args, tmp.path()).unwrap();
        assert!(p.contains("(new file)"));
        assert!(p.contains("+ l1"));
        assert!(p.contains("+ l3"));
    }

    #[test]
    fn bash_gets_no_preview() {
        assert!(approval_preview(
            "bash",
            &serde_json::json!({"command": "rm -rf /"}),
            Path::new("/tmp")
        )
        .is_none());
    }

    #[test]
    fn missing_path_is_none() {
        assert!(
            approval_preview("edit", &serde_json::json!({"edits": []}), Path::new("/tmp"))
                .is_none()
        );
    }

    #[test]
    fn identical_content_is_no_change() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "same\n").unwrap();
        let args = serde_json::json!({"path": "f.txt", "content": "same\n"});
        let p = write_preview(&args, tmp.path()).unwrap();
        assert!(p.contains("(no changes)"));
    }
}
