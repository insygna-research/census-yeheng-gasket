//! PermissionPolicy: 实装 core 的 HookChain，三档模式 + 工具风险 + approver 闭包。
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use conga::{HookChain, RiskLevel, ToolCallVerdict, ToolResultMessage};
use parking_lot::RwLock as PlRwLock;
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
    /// Shared abort flag (the Host's signal). When set while waiting on the
    /// approver, the wait is abandoned — cancellation is centralized here
    /// instead of trusting every approver to be cancel-aware.
    signal: PlRwLock<Option<Arc<std::sync::atomic::AtomicBool>>>,
}

impl PermissionPolicy {
    pub fn new(mode: Mode, approver: Approver) -> Self {
        Self {
            mode: AtomicU8::new(mode as u8),
            approver,
            signal: PlRwLock::new(None),
        }
    }

    /// Attach the shared abort signal (the Host's). Call once after the Host
    /// owning the signal is built; the policy is typically Arc-shared into
    /// the Host's hook chain before that point.
    pub fn set_signal(&self, signal: Arc<std::sync::atomic::AtomicBool>) {
        *self.signal.write() = Some(signal);
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

    /// Poll the shared abort signal while the approver decides. The HookChain
    /// contract requires human-blocking implementors to return promptly on
    /// cancel; an approver parked on stdin or a dead WS client cannot.
    async fn await_approver(&self, name: &str, args: &serde_json::Value) -> ToolCallVerdict {
        let signal = self.signal.read().clone();
        let mut approver = std::pin::pin!((self.approver)(name, args));
        loop {
            if let Some(sig) = &signal {
                if sig.load(Ordering::Relaxed) {
                    return ToolCallVerdict::Block(format!("{name} aborted"));
                }
            }
            tokio::select! {
                biased;
                _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => continue,
                allowed = &mut *approver => {
                    return if allowed {
                        ToolCallVerdict::Allow
                    } else {
                        ToolCallVerdict::Block(format!("{name} denied by user"))
                    };
                }
            }
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
                (Mode::AutoEdit, RiskLevel::High) => self.await_approver(name, args).await,
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

    /// P1-5 regression: an approver that never resolves must not out-wait the
    /// abort signal — the verdict must return promptly with a Block("…aborted").
    #[tokio::test]
    async fn approver_wait_is_cancel_aware() {
        let policy = PermissionPolicy::new(
            Mode::AutoEdit,
            Arc::new(|_n, _a| Box::pin(async { std::future::pending::<bool>().await })),
        );
        let signal = Arc::new(std::sync::atomic::AtomicBool::new(false));
        policy.set_signal(signal.clone());

        signal.store(true, Ordering::Relaxed);
        let started = std::time::Instant::now();
        let verdict = policy
            .before_tool_call("2", "bash", &serde_json::json!({}), RiskLevel::High)
            .await;
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        match verdict {
            ToolCallVerdict::Block(msg) => assert!(msg.contains("aborted"), "{msg}"),
            v => panic!("expected Block, got {v:?}"),
        }
    }
}
