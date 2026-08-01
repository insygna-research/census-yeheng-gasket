//! PermissionPolicy: 实装 core 的 HookChain，三档模式 + 工具风险 + approver 闭包。
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use gasket_core::{HookChain, RiskLevel, ToolCallVerdict, ToolResultMessage};

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

/// 工具审批闭包：`(tool_name, args) -> 是否允许`。返回 future，宿主可
/// 挂起回合等待人工决策（CLI 读 stdin；gateway 走 WebSocket 往返）。
/// HRTB 允许 future 借用入参。
pub type Approver = Arc<
    dyn for<'a> Fn(
            &'a str,
            &'a serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>>
        + Send
        + Sync,
>;

pub struct PermissionPolicy {
    mode: AtomicU8,
    approver: Approver,
}

impl PermissionPolicy {
    pub fn new(mode: Mode, approver: Approver) -> Self {
        Self {
            mode: AtomicU8::new(mode as u8),
            approver,
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
}

impl HookChain for PermissionPolicy {
    fn before_tool_call<'a>(
        &'a self,
        _id: &'a str,
        name: &'a str,
        args: &'a serde_json::Value,
        risk: RiskLevel,
    ) -> Pin<Box<dyn Future<Output = ToolCallVerdict> + Send + 'a>> {
        Box::pin(async move {
            match (self.mode(), risk) {
                (Mode::FullAuto, _) => ToolCallVerdict::Allow,
                (Mode::AutoEdit, RiskLevel::Low) | (Mode::AutoEdit, RiskLevel::Medium) => {
                    ToolCallVerdict::Allow
                }
                (Mode::AutoEdit, RiskLevel::High) => {
                    if (self.approver)(name, args).await {
                        ToolCallVerdict::Allow
                    } else {
                        ToolCallVerdict::Block(format!("{name} denied by user"))
                    }
                }
                (Mode::Suggest, RiskLevel::Low) => ToolCallVerdict::Allow,
                (Mode::Suggest, RiskLevel::Medium) | (Mode::Suggest, RiskLevel::High) => {
                    ToolCallVerdict::Block("read-only mode".into())
                }
            }
        })
    }

    fn after_tool_call(&self, _id: &str, result: &ToolResultMessage) -> ToolResultMessage {
        result.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn approver(allow: bool) -> (Approver, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let f: Approver = Arc::new(move |_n, _a| {
            c.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { allow })
        });
        (f, calls)
    }

    /// 调 before_tool_call 并取回 verdict（async 迁移后的统一入口）。
    async fn verdict(p: &PermissionPolicy, name: &str, risk: RiskLevel) -> ToolCallVerdict {
        p.before_tool_call("x", name, &serde_json::json!({}), risk)
            .await
    }

    #[tokio::test]
    async fn suggest_blocks_bash_and_writes() {
        let (a, _) = approver(false);
        let p = PermissionPolicy::new(Mode::Suggest, a);
        assert!(matches!(
            verdict(&p, "read", RiskLevel::Low).await,
            ToolCallVerdict::Allow
        ));
        assert!(matches!(
            verdict(&p, "write", RiskLevel::Medium).await,
            ToolCallVerdict::Block(_)
        ));
        assert!(matches!(
            verdict(&p, "bash", RiskLevel::High).await,
            ToolCallVerdict::Block(_)
        ));
    }

    #[tokio::test]
    async fn auto_edit_allows_writes_prompts_bash() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c = calls.clone();
        let p = PermissionPolicy::new(
            Mode::AutoEdit,
            Arc::new(move |_, _| {
                c.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { true })
            }),
        );
        assert!(matches!(
            verdict(&p, "write", RiskLevel::Medium).await,
            ToolCallVerdict::Allow
        ));
        let v = verdict(&p, "bash", RiskLevel::High).await;
        assert!(matches!(v, ToolCallVerdict::Allow));
        assert_eq!(calls.load(Ordering::SeqCst), 1); // approver 被调用
    }

    #[tokio::test]
    async fn auto_edit_denies_bash_when_approver_rejects() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c = calls.clone();
        let p = PermissionPolicy::new(
            Mode::AutoEdit,
            Arc::new(move |_, _| {
                c.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { false })
            }),
        );
        assert!(matches!(
            verdict(&p, "write", RiskLevel::Medium).await,
            ToolCallVerdict::Allow
        ));
        let v = verdict(&p, "bash", RiskLevel::High).await;
        assert!(matches!(
            &v,
            ToolCallVerdict::Block(msg) if msg == "bash denied by user"
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1); // approver 被调用
    }

    #[tokio::test]
    async fn full_auto_allows_everything() {
        let p = PermissionPolicy::new(Mode::FullAuto, Arc::new(|_, _| Box::pin(async { false })));
        assert!(matches!(
            verdict(&p, "bash", RiskLevel::High).await,
            ToolCallVerdict::Allow
        ));
    }

    #[tokio::test]
    async fn set_mode_switches_at_runtime() {
        let p = PermissionPolicy::new(Mode::Suggest, Arc::new(|_, _| Box::pin(async { false })));
        assert!(matches!(
            verdict(&p, "bash", RiskLevel::High).await,
            ToolCallVerdict::Block(_)
        ));
        p.set_mode(Mode::FullAuto);
        assert!(matches!(
            verdict(&p, "bash", RiskLevel::High).await,
            ToolCallVerdict::Allow
        ));
    }
}
