//! PermissionPolicy: 实装 core 的 HookChain，三档模式 + 工具风险 + approver 闭包。
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use gasket_core::{HookChain, ToolCallVerdict, ToolResultMessage};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Suggest,
    AutoEdit,
    FullAuto,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "suggest" => Some(Mode::Suggest),
            "auto-edit" | "autoedit" | "auto" => Some(Mode::AutoEdit),
            "full-auto" | "fullauto" | "full" => Some(Mode::FullAuto),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiskLevel {
    Low,
    Medium,
    High,
}

pub struct PermissionPolicy {
    mode: AtomicU8,
    approver: Arc<dyn Fn(&str, &serde_json::Value) -> bool + Send + Sync>,
}

impl PermissionPolicy {
    pub fn new(
        mode: Mode,
        approver: impl Fn(&str, &serde_json::Value) -> bool + Send + Sync + 'static,
    ) -> Self {
        Self {
            mode: AtomicU8::new(mode as u8),
            approver: Arc::new(approver),
        }
    }

    pub fn set_mode(&self, mode: Mode) {
        self.mode.store(mode as u8, Ordering::Relaxed);
    }

    fn mode(&self) -> Mode {
        match self.mode.load(Ordering::Relaxed) {
            0 => Mode::Suggest,
            1 => Mode::AutoEdit,
            2 => Mode::FullAuto,
            _ => Mode::AutoEdit,
        }
    }

    pub fn risk_of(tool_name: &str) -> RiskLevel {
        match tool_name {
            "read" | "list" | "grep" => RiskLevel::Low,
            "write" | "edit" => RiskLevel::Medium,
            _ => RiskLevel::High,
        }
    }
}

impl HookChain for PermissionPolicy {
    fn before_tool_call(
        &self,
        _id: &str,
        name: &str,
        args: &serde_json::Value,
    ) -> ToolCallVerdict {
        let risk = Self::risk_of(name);
        match (self.mode(), risk) {
            (Mode::FullAuto, _) => ToolCallVerdict::Allow,
            (Mode::AutoEdit, RiskLevel::Low) | (Mode::AutoEdit, RiskLevel::Medium) => {
                ToolCallVerdict::Allow
            }
            (Mode::AutoEdit, RiskLevel::High) => {
                if (self.approver)(name, args) {
                    ToolCallVerdict::Allow
                } else {
                    ToolCallVerdict::Block(format!("{name} denied by user"))
                }
            }
            (Mode::Suggest, RiskLevel::Low) => ToolCallVerdict::Allow,
            (Mode::Suggest, _) => {
                ToolCallVerdict::Block(format!("{name} not allowed in suggest (read-only) mode"))
            }
        }
    }

    fn after_tool_call(&self, _id: &str, result: &ToolResultMessage) -> ToolResultMessage {
        result.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn approver(
        allow: bool,
    ) -> (
        Arc<dyn Fn(&str, &serde_json::Value) -> bool + Send + Sync>,
        Arc<AtomicUsize>,
    ) {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let f: Arc<dyn Fn(&str, &serde_json::Value) -> bool + Send + Sync> =
            Arc::new(move |_n, _a| {
                c.fetch_add(1, Ordering::SeqCst);
                allow
            });
        (f, calls)
    }

    #[test]
    fn suggest_blocks_bash_and_writes() {
        let (a, _) = approver(false);
        let p = PermissionPolicy::new(Mode::Suggest, {
            let a = a.clone();
            move |n, args| a(n, args)
        });
        assert!(matches!(
            p.before_tool_call("x", "read", &serde_json::json!({})),
            ToolCallVerdict::Allow
        ));
        assert!(matches!(
            p.before_tool_call("x", "write", &serde_json::json!({})),
            ToolCallVerdict::Block(_)
        ));
        assert!(matches!(
            p.before_tool_call("x", "bash", &serde_json::json!({})),
            ToolCallVerdict::Block(_)
        ));
    }

    #[test]
    fn auto_edit_allows_writes_prompts_bash() {
        let (a, calls) = approver(true);
        let p = PermissionPolicy::new(Mode::AutoEdit, {
            let a = a.clone();
            move |n, args| a(n, args)
        });
        assert!(matches!(
            p.before_tool_call("x", "write", &serde_json::json!({})),
            ToolCallVerdict::Allow
        ));
        let v = p.before_tool_call("x", "bash", &serde_json::json!({}));
        assert!(matches!(v, ToolCallVerdict::Allow));
        assert_eq!(calls.load(Ordering::SeqCst), 1); // approver 被调用
    }

    #[test]
    fn full_auto_allows_everything() {
        let p = PermissionPolicy::new(Mode::FullAuto, |_, _| false);
        assert!(matches!(
            p.before_tool_call("x", "bash", &serde_json::json!({})),
            ToolCallVerdict::Allow
        ));
    }

    #[test]
    fn set_mode_switches_at_runtime() {
        let p = PermissionPolicy::new(Mode::Suggest, |_, _| false);
        assert!(matches!(
            p.before_tool_call("x", "bash", &serde_json::json!({})),
            ToolCallVerdict::Block(_)
        ));
        p.set_mode(Mode::FullAuto);
        assert!(matches!(
            p.before_tool_call("x", "bash", &serde_json::json!({})),
            ToolCallVerdict::Allow
        ));
    }
}
