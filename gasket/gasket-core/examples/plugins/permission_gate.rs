//! Example plugin: `permission_gate` — block dangerous commands.
//!
//! Demonstrates `register_before_tool_call`: a hook that runs before every
//! tool call and can Block it. This is the canonical "policy" plugin.
//!
//! When the model tries to call `bash` with `rm -rf`, `sudo`, or `chmod 777`,
//! this plugin returns `Block` — the agent loop (see `agent_loop::execute_tool_calls`)
//! skips execution and sends the block reason back to the model as an error
//! tool result. The model then reacts (asks the user, picks a safer command,
//! etc.). No dangerous command ever runs.

use gasket_core::extension::BeforeToolCallHandler;
use gasket_core::{ExtensionApi, ToolCallVerdict};

/// A gate that blocks bash commands matching dangerous patterns.
struct DangerousCommandGate;

/// Substrings that mark a bash command as too dangerous to auto-run.
const BLOCKED_PATTERNS: &[&str] = &["rm -rf", "sudo ", "chmod 777", "mkfs", ":(){:|:&};:"];

impl BeforeToolCallHandler for DangerousCommandGate {
    fn call(
        &self,
        _tool_call_id: &str,
        tool_name: &str,
        args: &serde_json::Value,
        _ctx: &gasket_core::ExtensionContext,
    ) -> ToolCallVerdict {
        if tool_name != "bash" {
            return ToolCallVerdict::Allow;
        }
        let cmd = args["command"].as_str().unwrap_or("");
        if BLOCKED_PATTERNS.iter().any(|p| cmd.contains(p)) {
            return ToolCallVerdict::Block(format!(
                "Refused: command matches a dangerous pattern ({}). \
                 Ask the user to confirm before retrying.",
                BLOCKED_PATTERNS.iter().cloned().collect::<Vec<_>>().join(", ")
            ));
        }
        ToolCallVerdict::Allow
    }
}

/// Install the gate.
pub fn register(api: &mut impl ExtensionApi) {
    api.register_before_tool_call(Box::new(DangerousCommandGate));
}
