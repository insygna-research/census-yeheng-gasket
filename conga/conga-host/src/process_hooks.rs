//! Process-out PreToolUse hooks: config discovery + external-command gate.
//!
//! Claude-compatible subset: commands run via `sh -c`, receive a JSON event
//! on stdin, and answer with exit codes / a stdout decision object. Composed
//! into the host's HookStack BEFORE the permission policy, so a failing or
//! wedged hook fails OPEN (warn + allow) — the policy underneath is still
//! the floor gate. See docs/hooks.md for the config schema.

use std::path::Path;
use std::time::Duration;

/// Default per-hook deadline (Claude-compatible `timeout` is in seconds).
const DEFAULT_TIMEOUT_SECS: u64 = 10;

/// Which tools a hook applies to. Empty/`*`/`all` matcher → `All`.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolMatcher {
    All,
    Names(Vec<String>),
}

impl ToolMatcher {
    pub fn matches(&self, tool: &str) -> bool {
        match self {
            ToolMatcher::All => true,
            ToolMatcher::Names(names) => names.iter().any(|n| n == tool),
        }
    }
}

/// One loaded process hook, ready to run.
#[derive(Debug, Clone)]
pub struct ProcessHook {
    pub command: String,
    pub tools: ToolMatcher,
    pub timeout: Duration,
}

/// The on-disk file (Claude-compatible shape; only `PreToolUse` is read,
/// unknown keys are ignored for forward compatibility).
#[derive(serde::Deserialize)]
struct HooksFile {
    #[serde(default)]
    #[serde(rename = "PreToolUse")]
    pre_tool_use: Vec<MatcherGroup>,
}

#[derive(serde::Deserialize)]
struct MatcherGroup {
    #[serde(default)]
    matcher: String,
    #[serde(default)]
    hooks: Vec<CommandHook>,
}

#[derive(serde::Deserialize)]
struct CommandHook {
    #[serde(default)]
    r#type: String,
    command: String,
    #[serde(default)]
    timeout: u64,
}

impl Default for CommandHook {
    fn default() -> Self {
        Self {
            r#type: String::new(),
            command: String::new(),
            timeout: DEFAULT_TIMEOUT_SECS,
        }
    }
}

fn parse_matcher(raw: &str) -> ToolMatcher {
    let normalized = raw.trim().to_ascii_lowercase();
    if normalized.is_empty() || normalized == "*" || normalized == "all" {
        return ToolMatcher::All;
    }
    ToolMatcher::Names(
        normalized
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
    )
}

/// Global `<global_root>/hooks.json` entries first, then project
/// `<project_dir>/.conga/hooks.json` entries appended (both run; the chain
/// applies first-Block-wins / last-Modify-wins, mirroring HookStack).
/// Malformed or unreadable files are skipped with a warning — fail-open,
/// never abort assembly. Only `type: "command"` hooks are supported.
pub fn load_process_hooks(global_root: &Path, project_dir: &Path) -> Vec<ProcessHook> {
    let mut out = Vec::new();
    load_file(&global_root.join("hooks.json"), &mut out);
    load_file(&project_dir.join(".conga").join("hooks.json"), &mut out);
    out
}

fn load_file(path: &Path, out: &mut Vec<ProcessHook>) {
    let raw = match std::fs::read_to_string(path) {
        Ok(r) => r,
        Err(_) => return, // no file = no hooks; not an error
    };
    let file: HooksFile = match serde_json::from_str(&raw) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "hooks.json unparseable; skipping file");
            return;
        }
    };
    for group in file.pre_tool_use {
        let tools = parse_matcher(&group.matcher);
        for h in group.hooks {
            // Empty type defaults to "command" (Claude's only current kind);
            // any other explicit type is skipped loudly, not fatal.
            let kind = h.r#type.trim().to_ascii_lowercase();
            if !kind.is_empty() && kind != "command" {
                tracing::warn!(
                    path = %path.display(),
                    kind = %kind,
                    "unsupported hook type; skipping entry"
                );
                continue;
            }
            if h.command.trim().is_empty() {
                tracing::warn!(path = %path.display(), "hook with empty command; skipping entry");
                continue;
            }
            let timeout = if h.timeout == 0 {
                Duration::from_secs(DEFAULT_TIMEOUT_SECS)
            } else {
                Duration::from_secs(h.timeout)
            };
            out.push(ProcessHook {
                command: h.command,
                tools: tools.clone(),
                timeout,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hook_json(body: &str) -> String {
        format!("{{\"PreToolUse\": [{}]}}", body)
    }

    #[test]
    fn empty_and_missing_files_yield_no_hooks() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(load_process_hooks(&tmp.path().join("g"), tmp.path()).is_empty());
        assert!(load_process_hooks(tmp.path(), tmp.path()).is_empty());
    }

    #[test]
    fn parses_matcher_hooks_and_timeout() {
        let g = tempfile::tempdir().unwrap();
        let p = tempfile::tempdir().unwrap();
        std::fs::write(
            g.path().join("hooks.json"),
            hook_json(
                r#"{"matcher": "bash, write", "hooks": [
                    {"type": "command", "command": "exit 0"},
                    {"type": "command", "command": "exit 2", "timeout": 3}
                ]}"#,
            ),
        )
        .unwrap();
        let hooks = load_process_hooks(g.path(), p.path());
        assert_eq!(hooks.len(), 2);
        assert_eq!(
            hooks[0].tools,
            ToolMatcher::Names(vec!["bash".into(), "write".into()])
        );
        assert_eq!(hooks[0].timeout, std::time::Duration::from_secs(10));
        assert_eq!(hooks[1].timeout, std::time::Duration::from_secs(3));
        assert!(hooks[0].tools.matches("bash"));
        assert!(!hooks[0].tools.matches("read"));
        assert!(ToolMatcher::All.matches("anything"));
    }

    #[test]
    fn star_and_empty_matcher_mean_all_tools() {
        let g = tempfile::tempdir().unwrap();
        let p = tempfile::tempdir().unwrap();
        std::fs::write(
            g.path().join("hooks.json"),
            hook_json(r#"{"matcher": "*", "hooks": [{"command": "true"}]}"#),
        )
        .unwrap();
        let hooks = load_process_hooks(g.path(), p.path());
        assert!(matches!(hooks[0].tools, ToolMatcher::All));
        // "type" defaults to "command" when absent.
        assert_eq!(hooks[0].command, "true");
    }

    #[test]
    fn project_entries_run_after_global() {
        let g = tempfile::tempdir().unwrap();
        let p = tempfile::tempdir().unwrap();
        std::fs::write(
            g.path().join("hooks.json"),
            hook_json(r#"{"hooks": [{"command": "global-first"}]}"#),
        )
        .unwrap();
        std::fs::create_dir_all(p.path().join(".conga")).unwrap();
        std::fs::write(
            p.path().join(".conga/hooks.json"),
            hook_json(r#"{"hooks": [{"command": "project-second"}]}"#),
        )
        .unwrap();
        let hooks = load_process_hooks(g.path(), p.path());
        assert_eq!(hooks.len(), 2);
        assert_eq!(hooks[0].command, "global-first");
        assert_eq!(hooks[1].command, "project-second");
    }

    #[test]
    fn malformed_file_is_skipped_loudly() {
        let g = tempfile::tempdir().unwrap();
        let p = tempfile::tempdir().unwrap();
        std::fs::write(g.path().join("hooks.json"), "{ not json").unwrap();
        std::fs::create_dir_all(p.path().join(".conga")).unwrap();
        std::fs::write(
            p.path().join(".conga/hooks.json"),
            hook_json(r#"{"hooks": [{"command": "ok"}]}"#),
        )
        .unwrap();
        // Bad global skipped; good project file still loads.
        let hooks = load_process_hooks(g.path(), p.path());
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].command, "ok");
    }

    #[test]
    fn unsupported_event_keys_are_ignored_not_rejected() {
        let g = tempfile::tempdir().unwrap();
        let p = tempfile::tempdir().unwrap();
        // PostToolUse/SessionStart present: parse must not fail, only
        // PreToolUse hooks are collected (forward compatibility).
        std::fs::write(
            g.path().join("hooks.json"),
            format!(
                "{{\"PostToolUse\": [{{\"hooks\": [{{\"command\": \"x\"}}]}}], \"PreToolUse\": [{{\"hooks\": [{{\"command\": \"y\"}}]}}]}}"
            ),
        )
        .unwrap();
        let hooks = load_process_hooks(g.path(), p.path());
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].command, "y");
    }

    #[test]
    fn non_command_hook_type_is_skipped() {
        let g = tempfile::tempdir().unwrap();
        let p = tempfile::tempdir().unwrap();
        std::fs::write(
            g.path().join("hooks.json"),
            hook_json(r#"{"hooks": [{"type": "http", "command": "no"}, {"command": "yes"}]}"#),
        )
        .unwrap();
        let hooks = load_process_hooks(g.path(), p.path());
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].command, "yes");
    }
}
