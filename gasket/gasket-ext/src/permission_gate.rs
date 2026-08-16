//! Block dangerous bash patterns via `before_tool_call`.

use gasket_core::extension::BeforeToolCallHandler;
use gasket_core::{ExtensionApi, RiskLevel, ToolCallVerdict};

struct DangerousCommandGate;

const BLOCKED_PATTERNS: &[&str] = &["rm -rf", "sudo ", "chmod 777", "mkfs", ":(){:|:&};:"];

impl BeforeToolCallHandler for DangerousCommandGate {
    fn call(
        &self,
        _tool_call_id: &str,
        tool_name: &str,
        args: &serde_json::Value,
        _risk: RiskLevel,
    ) -> ToolCallVerdict {
        if tool_name != "bash" {
            return ToolCallVerdict::Allow;
        }
        let cmd = args["command"].as_str().unwrap_or("");
        if BLOCKED_PATTERNS.iter().any(|p| cmd.contains(p)) {
            return ToolCallVerdict::Block(format!(
                "Refused: command matches a dangerous pattern ({}). \
                 Ask the user to confirm before retrying.",
                BLOCKED_PATTERNS.join(", ")
            ));
        }
        ToolCallVerdict::Allow
    }
}

pub fn register(api: &mut dyn ExtensionApi) {
    api.register_before_tool_call(Box::new(DangerousCommandGate));
}
