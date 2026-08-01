//! 审批请求登记：id → oneshot 决策通道 + 按工具名的 remember 缓存。
//! 纯逻辑、无 IO，可单测；WS 收发胶水在 main.rs 的 approver 闭包里。

use std::collections::HashMap;

use tokio::sync::oneshot;

/// 一次 `register` 的结果。
#[derive(Debug)]
pub enum RegisterOutcome {
    /// 该工具此前被 remember，直接复用历史决策。
    Remembered(bool),
    /// 需要人工审批：request_id 用于回填决策，rx 是等待端。
    Pending {
        request_id: String,
        rx: oneshot::Receiver<bool>,
    },
}

/// 追踪在途审批与 remember 决策。同一时刻至多一个在途审批
/// （execute_tool_calls 串行 await hook），但设计上不依赖这个假设。
pub struct ApprovalRegistry {
    pending: HashMap<String, (String, oneshot::Sender<bool>)>,
    memory: HashMap<String, bool>,
    seq: u64,
}

impl ApprovalRegistry {
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
            memory: HashMap::new(),
            seq: 0,
        }
    }

    /// 注册一次审批。remember 命中时直接返回历史决策；否则分配
    /// 自增 request_id（格式 `ap{seq}`）并登记决策通道。
    pub fn register(&mut self, tool_name: &str) -> RegisterOutcome {
        if let Some(decided) = self.memory.get(tool_name) {
            return RegisterOutcome::Remembered(*decided);
        }
        self.seq += 1;
        let request_id = format!("ap{}", self.seq);
        let (tx, rx) = oneshot::channel();
        self.pending
            .insert(request_id.clone(), (tool_name.to_string(), tx));
        RegisterOutcome::Pending { request_id, rx }
    }

    /// 回填决策。未知 request_id 静默忽略（迟到/重复响应）。
    /// `remember=true` 时按工具名缓存决策供后续复用。
    pub fn respond(&mut self, request_id: &str, approved: bool, remember: bool) {
        if let Some((tool_name, tx)) = self.pending.remove(request_id) {
            let _ = tx.send(approved);
            if remember {
                self.memory.insert(tool_name, approved);
            }
        }
    }

    /// 回合结束时清空在途审批：sender 全部 drop，等待端 select!
    /// 的 oneshot 分支以 Err 立即返回（调用方按 false 处理），不会挂起。
    pub fn clear_pending(&mut self) {
        self.pending.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_then_respond_resolves() {
        let mut r = ApprovalRegistry::new();
        let RegisterOutcome::Pending { request_id, rx } = r.register("bash") else {
            panic!("first approval must be pending");
        };
        assert_eq!(request_id, "ap1");
        r.respond(&request_id, true, false);
        assert_eq!(rx.blocking_recv(), Ok(true));
    }

    #[test]
    fn remembered_decision_bypasses_approval() {
        let mut r = ApprovalRegistry::new();
        let RegisterOutcome::Pending { request_id, .. } = r.register("bash") else {
            panic!();
        };
        r.respond(&request_id, true, true); // remember=true
        match r.register("bash") {
            RegisterOutcome::Remembered(true) => {}
            other => panic!("expected remembered(true), got {other:?}"),
        }
        // 其他工具不受影响
        assert!(matches!(
            r.register("write"),
            RegisterOutcome::Pending { .. }
        ));
    }

    #[test]
    fn duplicate_response_is_ignored() {
        let mut r = ApprovalRegistry::new();
        let RegisterOutcome::Pending { request_id, rx } = r.register("bash") else {
            panic!();
        };
        r.respond(&request_id, true, false);
        r.respond(&request_id, false, false); // 第二次：no-op
        assert_eq!(rx.blocking_recv(), Ok(true), "first decision wins");
    }

    #[test]
    fn unknown_request_id_is_ignored() {
        let mut r = ApprovalRegistry::new();
        r.respond("ap999", true, false); // 不 panic
    }

    #[test]
    fn clear_pending_drops_waiters() {
        let mut r = ApprovalRegistry::new();
        let RegisterOutcome::Pending { rx, .. } = r.register("bash") else {
            panic!();
        };
        r.clear_pending();
        // tokio 1.53 的 RecvError 是无公开构造函数的单元结构体，
        // 只能断言收端以 Err（通道关闭）返回。
        assert!(rx.blocking_recv().is_err());
    }

    #[test]
    fn seq_increments_per_request() {
        let mut r = ApprovalRegistry::new();
        let RegisterOutcome::Pending { request_id, .. } = r.register("bash") else {
            panic!();
        };
        assert_eq!(request_id, "ap1");
        let RegisterOutcome::Pending { request_id, .. } = r.register("write") else {
            panic!();
        };
        assert_eq!(request_id, "ap2");
    }
}
