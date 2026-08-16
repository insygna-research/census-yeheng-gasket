# 桌面端交付（M1）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 Tauri 桌面端成为真实可用的产品：工具审批流端到端走通（core `HookChain` 异步化 → host async approver → 网关 WS 审批协议 → 前端 ApprovalDialog 零改动工作），subagent 死契约明确处置，死依赖清理，release.yml 修复并发布 v2.0.0。

**Architecture:** 核心是 `HookChain::before_tool_call` 从 sync 变 async（手写 boxed future，与既有 `StreamFn` 同一风格，不引入 async-trait）。扩展注册面（`BeforeToolCallHandler`）保持 sync，只有链的调度层异步化。网关新增 `ApprovalRegistry`（id→oneshot 映射 + remember 缓存，纯逻辑可单测）+ WS 收发胶水。CLI 的 stdin approver 用 `spawn_blocking` 包进 async。

**Tech Stack:** Rust 2021、tokio（oneshot/watch/select）、axum、Vue 3 + Tauri、GitHub Actions。

**Spec:** `docs/superpowers/specs/2026-07-31-desktop-delivery-design.md`（本计划是其实施文档，任务一一对应 spec §3/§5/§6）。

## Global Constraints

- 工作区根：`/Users/yeheng/workspaces/Github/gasket/gasket`（内层，含 workspace Cargo.toml）；web 在前一层 `web/`。
- `HookChain` 异步化是 core 公共 API 变更，但全部消费者在仓库内（CLI/gateway/ext/host）；**不留 shim、不留旧 sync 重载**，一次迁移到位。
- 异步化**不引入 async-trait**：手写 `Pin<Box<dyn Future + Send + '_>>`，与 `StreamFn` 同风格。
- 前端协议字段逐字对齐（前端 `useChatSession.ts` 已按 `msg.id` / `msg.tool_name` / `msg.description` / `msg.arguments` 消费，见其 296-301 行）——**禁止改前端协议**。
- 每任务结束跑门禁并提交：`cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets --all-features -- -D warnings`、`cargo test --workspace --all-features`。
- 提交信息用仓库现有约定（`feat(core): ...` / `feat(gateway): ...`）。

## File Structure

- Modify `gasket-core/src/types/tool.rs` — `HookChain` trait 签名（唯一公共 API 变更）
- Modify `gasket-core/src/extension/api.rs` — `ExtensionApiImpl` 的 HookChain impl 包 async
- Modify `gasket-core/src/agent_loop.rs` — hook 调用点 `.await`；`stream_assistant_response` 入口 abort 早退；新增测试
- Modify `gasket-host/src/permission.rs` — `Approver` 类型 + HookChain impl + 测试
- Modify `gasket-host/src/hooks.rs` — `HookStack` impl + mock 测试
- Modify `gasket-cli/src/main.rs` — `stdin_approver` 改 async
- Create `gasket-gateway/src/approval.rs` — `ApprovalRegistry`（纯逻辑 + 单测）
- Modify `gasket-gateway/src/main.rs` — 审批协议 + 模式 env + 契约核对表文档
- Modify `web/src/composables/useChatSession.ts`、`web/src/types/index.ts` — subagent_* 标记 M2
- Modify `gasket/Cargo.toml`、`gasket-core/Cargo.toml`、`gasket-host/Cargo.toml` — 死依赖清理
- Modify `.github/workflows/release.yml` — 去 protoc + desktop job

---

### Task 1: core `HookChain` 异步化 + abort 早退（机械迁移 + 一个行为修复）

**Files:**
- Modify: `gasket-core/src/types/tool.rs:113-128`（trait）
- Modify: `gasket-core/src/extension/api.rs:91-104`（impl）
- Modify: `gasket-core/src/agent_loop.rs:245-248`（调用点）、`agent_loop.rs:356-360`（abort 早退）
- Modify: `gasket-host/src/hooks.rs:26-58`（impl）、`:66-126`（mock + 测试）
- Modify: `gasket-host/src/permission.rs:76-101`（impl）、`:125-176`（测试 await 化）
- Test: `gasket-core/src/agent_loop.rs`（新增 `pre_set_signal_aborts_before_provider_request`）

**Interfaces:**
- Consumes: 现有 `ToolCallVerdict` / `ToolResultMessage` / `StreamFn`（不变）。
- Produces:
  - `HookChain::before_tool_call<'a>(&'a self, &'a str, &'a str, &'a serde_json::Value) -> Pin<Box<dyn Future<Output = ToolCallVerdict> + Send + 'a>>`（`after_tool_call` 保持 sync）
  - `ExtensionApiImpl::before_tool_call`（inherent，sync）与 `BeforeToolCallHandler`（扩展面）**签名不变**——扩展 crate（`gasket-ext`）零改动
  - `stream_assistant_response` 在**发起 provider 请求前**检查 signal：预置 abort 时零请求返回 `StopReason::Aborted` 消息

- [ ] **Step 1: 改 trait 签名（编译失败 = 迁移清单）**

`gasket-core/src/types/tool.rs`，把 trait 定义替换为：

```rust
/// Object-safe hook chain the agent loop consults around each tool call.
///
/// `before_tool_call` is async because hosts may need to ask a human for
/// approval (CLI: stdin; gateway: WebSocket round-trip). `after_tool_call`
/// stays sync — it is a pure transformation (redact etc.).
///
/// Defined in `types` (not `extension`) so `AgentLoopConfig` can hold an
/// `Option<Arc<dyn HookChain>>` without a circular dependency. The concrete
/// implementation is `ExtensionApiImpl`; `None` means "no hooks installed"
/// (the default — used by tests and the bare `agent_loop` helper).
pub trait HookChain: Send + Sync {
    /// Consult all `before_tool_call` handlers. First `Block` wins; otherwise
    /// the last `Modify` wins; default `Allow`.
    fn before_tool_call<'a>(
        &'a self,
        tool_call_id: &'a str,
        tool_name: &'a str,
        args: &'a serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = ToolCallVerdict> + Send + 'a>>;

    /// Consult all `after_tool_call` handlers, each may replace the result.
    fn after_tool_call(&self, tool_call_id: &str, result: &ToolResultMessage) -> ToolResultMessage;
}
```

`tool.rs` 顶部已 `use std::future::Future; use std::pin::Pin;`（为 `ToolFn` 引入），无需加 import。

Run: `cargo check --workspace`
Expected: 编译失败，报错点 = 全部待迁移 impl/调用点清单（本任务 Step 2-5 逐一消灭）。

- [ ] **Step 2: 迁移 `ExtensionApiImpl`（扩展面保持 sync）**

`gasket-core/src/extension/api.rs:91-104` 的 trait impl 替换为：

```rust
impl crate::types::tool::HookChain for ExtensionApiImpl {
    fn before_tool_call<'a>(
        &'a self,
        tool_call_id: &'a str,
        tool_name: &'a str,
        args: &'a serde_json::Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolCallVerdict> + Send + 'a>> {
        Box::pin(async move {
            ExtensionApiImpl::before_tool_call(self, tool_call_id, tool_name, args)
        })
    }

    fn after_tool_call(&self, tool_call_id: &str, result: &ToolResultMessage) -> ToolResultMessage {
        ExtensionApiImpl::after_tool_call(self, tool_call_id, result)
    }
}
```

inherent `before_tool_call`（:58-74）与 `BeforeToolCallHandler`（:14-22）**不动**。`gasket-ext` 零改动。

- [ ] **Step 3: 迁移 `agent_loop.rs` 调用点 + 写 abort 早退的失败测试**

`agent_loop.rs:245-248` 改为：

```rust
        // 1. before_tool_call hook: consult the hook chain if installed.
        //    Block → refuse; Modify → replace args; Allow → proceed.
        let verdict = match &config.hooks {
            Some(h) => h.before_tool_call(&tc.id, &tc.function.name, &args).await,
            None => ToolCallVerdict::Allow,
        };
```

`execute_tool_calls` 本身是 async fn，`args` 是 owned `serde_json::Value`，借用跨 await 合法。

然后在 `agent_loop.rs` 的 `#[cfg(test)] mod tests` 里加失败测试（放现有测试附近）：

```rust
    /// Test stream that must never be polled: the pre-set abort has to stop
    /// the loop before the provider is touched.
    struct PollCountingStream {
        polls: std::sync::atomic::AtomicUsize,
    }
    impl StreamFn for PollCountingStream {
        fn stream(
            &self,
            _model: &ModelSpec,
            _messages: &[AgentMessage],
            _system: &str,
            _tools: &[ToolDefinition],
            _signal: Option<Arc<AtomicBool>>,
        ) -> Pin<Box<dyn futures_util::Stream<Item = StreamChunk> + Send>> {
            let n = self.polls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(n, 0, "provider stream must not be polled after a pre-set abort");
            Box::pin(futures_util::stream::iter(vec![]))
        }
    }

    #[tokio::test]
    async fn pre_set_signal_aborts_before_provider_request() {
        let mut config = test_config(vec![]);
        config.signal = Some(Arc::new(AtomicBool::new(true)));
        config.stream_fn = Arc::new(PollCountingStream {
            polls: Default::default(),
        });
        let ctx = AgentContext {
            system_prompt: "sys".into(),
            messages: vec![],
            tools: vec![],
            cwd: std::env::current_dir().unwrap(),
            env: std::collections::HashMap::new(),
            session_id: "test".into(),
        };
        let msgs = crate::agent_loop(vec![], ctx, config).await.unwrap();
        assert!(
            msgs.iter().any(|m| matches!(
                m,
                AgentMessage::Assistant(a) if a.stop_reason == StopReason::Aborted
            )),
            "pre-set signal must produce an Aborted message"
        );
    }
```

Run: `cargo test -p gasket-core pre_set_signal_aborts_before_provider_request`
Expected: FAIL——`PollCountingStream` 的断言 panic（signal 已置但 loop 仍发起 provider 调用）。

- [ ] **Step 4: 实现 abort 早退**

`stream_assistant_response` 的 `loop {` 之后、`emit(AgentEvent::BeforeProviderRequest ...)` 之前插入（`agent_loop.rs:358-362` 区域）：

```rust
        if is_aborted(config) {
            // A cancel arrived while the host was waiting (e.g. an approval
            // prompt): exit before burning a provider request. Mirrors the
            // in-stream abort path's event shape so hosts see the Aborted
            // message the same way.
            tracing::info!("provider request skipped: aborted");
            let mut msg = AssistantMessage::new(&config.model.id);
            msg.stop_reason = StopReason::Aborted;
            emit(AgentEvent::MessageEnd {
                message: msg.clone(),
            });
            emit(AgentEvent::AfterProviderResponse {
                model: config.model.id.clone(),
                response: msg.clone(),
            });
            return Ok(msg);
        }
```

Run: `cargo test -p gasket-core pre_set_signal_aborts_before_provider_request`
Expected: PASS，stream 从未被 poll。

- [ ] **Step 5: 迁移 `hooks.rs`（impl + mock + 测试）**

`gasket-host/src/hooks.rs` 的 `impl HookChain for HookStack`（:26-58）替换：

```rust
impl HookChain for HookStack {
    fn before_tool_call<'a>(
        &'a self,
        tool_call_id: &'a str,
        tool_name: &'a str,
        args: &'a serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = ToolCallVerdict> + Send + 'a>> {
        Box::pin(async move {
            let mut current = args.clone();
            let mut modified = false;
            for chain in &self.chains {
                match chain.before_tool_call(tool_call_id, tool_name, &current).await {
                    ToolCallVerdict::Block(reason) => return ToolCallVerdict::Block(reason),
                    ToolCallVerdict::Modify(a) => {
                        current = a;
                        modified = true;
                    }
                    ToolCallVerdict::Allow => {}
                }
            }
            if modified {
                ToolCallVerdict::Modify(current)
            } else {
                ToolCallVerdict::Allow
            }
        })
    }

    fn after_tool_call(&self, tool_call_id: &str, result: &ToolResultMessage) -> ToolResultMessage {
        let mut current = result.clone();
        for chain in &self.chains {
            current = chain.after_tool_call(tool_call_id, &current);
        }
        current
    }
}
```

文件顶部补 import：`use std::future::Future; use std::pin::Pin;`。

测试区三个 mock（`BlockBash` / `AllowAll` / `Redact`，:66-97）的 `before_tool_call` 全部改成同一形态：

```rust
    struct BlockBash;
    impl HookChain for BlockBash {
        fn before_tool_call<'a>(
            &'a self,
            _: &'a str,
            name: &'a str,
            _: &'a serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = ToolCallVerdict> + Send + 'a>> {
            Box::pin(async move {
                if name == "bash" {
                    ToolCallVerdict::Block("no bash".into())
                } else {
                    ToolCallVerdict::Allow
                }
            })
        }
        fn after_tool_call(&self, _: &str, r: &ToolResultMessage) -> ToolResultMessage {
            r.clone()
        }
    }

    struct AllowAll;
    impl HookChain for AllowAll {
        fn before_tool_call<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
            _: &'a serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = ToolCallVerdict> + Send + 'a>> {
            Box::pin(async { ToolCallVerdict::Allow })
        }
        fn after_tool_call(&self, _: &str, r: &ToolResultMessage) -> ToolResultMessage {
            r.clone()
        }
    }

    struct Redact;
    impl HookChain for Redact {
        fn before_tool_call<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
            _: &'a serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = ToolCallVerdict> + Send + 'a>> {
            Box::pin(async { ToolCallVerdict::Allow })
        }
        fn after_tool_call(&self, _: &str, r: &ToolResultMessage) -> ToolResultMessage {
            ToolResultMessage {
                tool_call_id: r.tool_call_id.clone(),
                tool_name: r.tool_name.clone(),
                content: vec![gasket_core::ContentBlock::text("[x]".to_string())],
                is_error: r.is_error,
                timestamp: r.timestamp,
            }
        }
    }
```

两个测试中只有 `first_block_wins`（:108-111）含 `before_tool_call` 调用：改为 `#[tokio::test]` 并在调用后加 `.await`。`after_pipes`（:122-126）只调 `after_tool_call`（保持 sync），保持普通 `#[test]` 不动。

```rust
    #[tokio::test]
    async fn first_block_wins() {
        let stack = HookStack::new(vec![Arc::new(AllowAll), Arc::new(BlockBash)]);
        let v = stack.before_tool_call("1", "bash", &serde_json::json!({})).await;
        assert!(matches!(v, ToolCallVerdict::Block(_)));
    }
```

（`after_pipes` 保持原样，无需改动。）

- [ ] **Step 6: 迁移 `permission.rs`（impl 包 async，测试 await 化）**

`gasket-host/src/permission.rs` 的 `impl HookChain for PermissionPolicy`（:76-101）替换为（**Task 1 阶段 approver 仍是 sync bool 闭包**，机械包一层）：

```rust
impl HookChain for PermissionPolicy {
    fn before_tool_call<'a>(
        &'a self,
        _id: &'a str,
        name: &'a str,
        args: &'a serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = ToolCallVerdict> + Send + 'a>> {
        Box::pin(async move {
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
```

文件顶部补：`use std::future::Future; use std::pin::Pin;`（`PermissionPolicy` 的 `approver` 字段类型暂不变，Task 2 才换）。

测试区（:125-176）加一个 helper 并逐测试 await：

```rust
    /// 调 before_tool_call 并取回 verdict（async 迁移后的统一入口）。
    async fn verdict(p: &PermissionPolicy, name: &str) -> ToolCallVerdict {
        p.before_tool_call("x", name, &serde_json::json!({})).await
    }
```

四个测试全部标 `#[tokio::test]`，`p.before_tool_call("x", name, &args)` 改为 `verdict(&p, name).await`。注意 `auto_edit_allows_writes_prompts_bash`（:146-153）的 approver stub 现在仍返回 `bool`（sync），只把调用点改掉，stub 本身 Task 2 再动：

```rust
    #[tokio::test]
    async fn auto_edit_allows_writes_prompts_bash() {
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let p = PermissionPolicy::new(
            Mode::AutoEdit,
            |_, _| {
                calls.fetch_add(1, Ordering::SeqCst);
                true
            },
        );
        assert!(matches!(
            verdict(&p, "write").await,
            ToolCallVerdict::Allow
        ));
        let v = verdict(&p, "bash").await;
        assert!(matches!(v, ToolCallVerdict::Allow)); // approver=true → Allow
        assert_eq!(calls.load(Ordering::SeqCst), 1); // approver 被调用
    }
```

（其余三个测试同样机械迁移；`set_mode_switches_at_runtime` 的两个调用点同改。）

- [ ] **Step 7: 门禁 + 提交**

Run:
```bash
cargo fmt --all -- --check && cargo clippy --workspace --all-targets --all-features -- -D warnings && cargo test --workspace --all-features
git add gasket-core/src/types/tool.rs gasket-core/src/extension/api.rs gasket-core/src/agent_loop.rs gasket-host/src/hooks.rs gasket-host/src/permission.rs
git commit -m "feat(core): 异步化 HookChain::before_tool_call 并加 provider 请求前 abort 早退"
```
Expected: fmt/clippy 全绿；全部测试通过（core 76+1 新、host 30、integration 3 等）。

---

### Task 2: host `PermissionPolicy` async approver + CLI 迁移

**Files:**
- Modify: `gasket-host/src/permission.rs`（`Approver` 类型、`new` 签名、impl 的 `.await`、测试 stub）
- Modify: `gasket-cli/src/main.rs:150-160`（`stdin_approver`）
- Modify: `gasket-host/src/lib.rs` 测试（`Arc::new(...)` approver 包装）、`gasket-host/tests/integration.rs`、`gasket-host/tests/smoke_llm.rs`（`PermissionPolicy::new` 调用点）

**Interfaces:**
- Consumes: Task 1 的 async `HookChain`。
- Produces:
  - `pub type Approver = Arc<dyn for<'a> Fn(&'a str, &'a serde_json::Value) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> + Send + Sync>`
  - `PermissionPolicy::new(mode: Mode, approver: Approver) -> Self`（**注意**：`new` 直接收 `Approver`，调用方自己 `Arc::new`，不再用 `impl Fn` 泛参——HRTB 输出类型无法用普通 `impl Fn` 干净表达）
  - CLI `stdin_approver(name: &str, args: &Value) -> Pin<Box<dyn Future<Output = bool> + Send + '_>>`

- [ ] **Step 1: 换 `Approver` 类型 + `new` 签名 + impl `.await`**

`gasket-host/src/permission.rs`：

```rust
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
```

`new` 改为：

```rust
    pub fn new(mode: Mode, approver: Approver) -> Self {
        Self {
            mode: AtomicU8::new(mode as u8),
            approver,
        }
    }
```

HookChain impl 里 AutoEdit+High 分支的调用改为 `.await`：

```rust
                (Mode::AutoEdit, RiskLevel::High) => {
                    if (self.approver)(name, args).await {
                        ToolCallVerdict::Allow
                    } else {
                        ToolCallVerdict::Block(format!("{name} denied by user"))
                    }
                }
```

- [ ] **Step 2: 迁移测试 stub 为 async**

`permission.rs` 测试里所有 `PermissionPolicy::new(mode, 闭包)` 的闭包改为：

```rust
        let p = PermissionPolicy::new(
            Mode::AutoEdit,
            Arc::new(|_, _| {
                calls.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { false })
            }),
        );
```

（`full_auto_policy` 等 helper 同理：`Arc::new(|_, _| Box::pin(async { true }))`。）

- [ ] **Step 3: 迁移 CLI `stdin_approver`**

`gasket-cli/src/main.rs`：

```rust
fn stdin_approver(
    name: &str,
    _args: &serde_json::Value,
) -> Pin<Box<dyn Future<Output = bool> + Send + '_>> {
    // 读 stdin 是阻塞的，挪到 blocking 池，避免卡住 tokio worker。
    let name = name.to_string();
    Box::pin(async move {
        print!("\n[approve {name}? y/N] ");
        let _ = io::stdout().flush();
        tokio::task::spawn_blocking(move || {
            let mut s = String::new();
            let _ = io::stdin().read_line(&mut s);
            s.trim().eq_ignore_ascii_case("y")
        })
        .await
        .unwrap_or(false)
    })
}
```

顶部 imports 补 `use std::future::Future; use std::pin::Pin;`。`PermissionPolicy::new(mode, stdin_approver)` 调用点改为 `PermissionPolicy::new(mode, Arc::new(stdin_approver))`。

- [ ] **Step 4: 迁移其余 `PermissionPolicy::new` 调用点**

`gasket-host/src/lib.rs` 测试、`gasket-host/tests/integration.rs` 的 `full_auto_policy()`、`gasket-host/tests/smoke_llm.rs` 两处：`PermissionPolicy::new(Mode::X, |_, _| false)` → `PermissionPolicy::new(Mode::X, Arc::new(|_, _| Box::pin(async { false })))`。这些调用点现在都包着 `Arc::new(...)` 再传给 `Host::new`，改后变成 `Arc::new(PermissionPolicy::new(...))` 不变。

- [ ] **Step 5: 门禁 + 提交**

Run:
```bash
cargo fmt --all -- --check && cargo clippy --workspace --all-targets --all-features -- -D warnings && cargo test --workspace --all-features
git add gasket-host gasket-cli
git commit -m "feat(host): PermissionPolicy approver 异步化，CLI 经 spawn_blocking 读 stdin"
```
Expected: 全绿。

---

### Task 3: gateway 审批协议（ApprovalRegistry + WS 接线）

**Files:**
- Create: `gasket-gateway/src/approval.rs`
- Modify: `gasket-gateway/src/main.rs`（`WsSession`、`OutgoingEvent`、`handle_ws`、select 循环、模块 doc 契约表）

**Interfaces:**
- Consumes: Task 2 的 `PermissionPolicy::new(mode, Approver)`。
- Produces:
  - `ApprovalRegistry::{new, register(&mut self, tool_name) -> RegisterOutcome, respond(&mut self, request_id, approved, remember), clear_pending}`
  - `RegisterOutcome::{Remembered(bool), Pending { request_id: String, rx: oneshot::Receiver<bool> }}`
  - WS 出站 `{"type":"approval_request","id","tool_name","description","arguments"}`；入站 `{"type":"approval_response","request_id","approved","remember"}`（前端 `sendApprovalResponse` 已按此发送，零改动）

- [ ] **Step 1: 写 `approval.rs` 失败测试**

Create `gasket-gateway/src/approval.rs`：

```rust
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
        self.pending.insert(request_id.clone(), (tool_name.to_string(), tx));
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
```

测试：

```rust
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
        assert!(matches!(r.register("write"), RegisterOutcome::Pending { .. }));
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
        assert_eq!(rx.blocking_recv(), Err(oneshot::error::RecvError::Closed));
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
```

Run: `cargo test -p gasket-gateway approval`
Expected: 编译失败（approval.rs 还没进 mod）→ 下一步接上后 PASS。

- [ ] **Step 2: `main.rs` 接模块 + `OutgoingEvent` 扩展 + `WsSession` 加 registry**

`gasket-gateway/src/main.rs` 顶部加 `mod approval;` 与 `use approval::{ApprovalRegistry, RegisterOutcome};`。

`OutgoingEvent` 结构体加三个字段（全部 `skip_serializing_if`，不破坏既有事件）：

```rust
#[derive(Serialize)]
struct OutgoingEvent {
    #[serde(rename = "type")]
    event_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    arguments: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}
```

现有构造函数（content/thinking/tool_start/tool_end/error/done）全部加 `id: None, tool_name: None, description: None,`。新增构造函数：

```rust
    fn approval_request(request_id: String, tool_name: String, args: &serde_json::Value) -> Self {
        // description 给前端展示；arguments 保留原始参数。截断防超长。
        let desc = serde_json::to_string(args).unwrap_or_default();
        let desc = if desc.chars().count() > 300 {
            format!("{}...", desc.chars().take(300).collect::<String>())
        } else {
            desc
        };
        Self {
            event_type: "approval_request",
            id: Some(request_id),
            tool_name: Some(tool_name),
            description: Some(desc),
            content: None,
            name: None,
            arguments: Some(args.to_string()),
            output: None,
            message: None,
        }
    }
```

`WsSession` 加字段：

```rust
struct WsSession {
    sender: SplitSink<WebSocket, Message>,
    history: Vec<AgentMessage>,
    usage_in: u64,
    usage_out: u64,
    registry: ApprovalRegistry,
}
```

构造点加 `registry: ApprovalRegistry::new(),`。

新增入站审批响应结构：

```rust
#[derive(Deserialize)]
struct ApprovalResponse {
    request_id: String,
    approved: bool,
    #[serde(default)]
    remember: bool,
}
```

- [ ] **Step 3: `handle_ws` 组装 async approver + 模式 env**

`handle_ws` 中替换 `let policy = Arc::new(PermissionPolicy::new(Mode::FullAuto, |_, _| true));`（当前写死 FullAuto）为：

```rust
    let mode = std::env::var("GASKET_GATEWAY_MODE")
        .ok()
        .and_then(|s| Mode::parse(&s))
        .unwrap_or(Mode::AutoEdit);
    // cancel 信号的双通道：AtomicBool 驱动 loop 中止，watch 解锁挂起的审批。
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let approver_session = session.clone();
    let approver = move |tool_name: &str, args: &serde_json::Value| {
        let session = approver_session.clone();
        let mut cancel_rx = cancel_rx.clone();
        Box::pin(async move {
            let outcome = { session.lock().await.registry.register(tool_name) };
            let (request_id, rx) = match outcome {
                RegisterOutcome::Remembered(v) => return v,
                RegisterOutcome::Pending { request_id, rx } => (request_id, rx),
            };
            {
                let mut s = session.lock().await;
                let ev =
                    OutgoingEvent::approval_request(request_id.clone(), tool_name.to_string(), args);
                send_json(&mut s.sender, &ev).await;
            }
            let timeout_s = std::env::var("GASKET_APPROVAL_TIMEOUT_S")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(300u64);
            tokio::select! {
                r = rx => r.unwrap_or(false),
                _ = cancel_rx.changed() => false,
                _ = tokio::time::sleep(std::time::Duration::from_secs(timeout_s)) => false,
            }
        })
    };
    let policy = Arc::new(PermissionPolicy::new(mode, Arc::new(approver)));
```

`cancel_tx` 需要传给主循环与 select 循环（Step 4/5）。

- [ ] **Step 4: select 循环（回合中）路由 `approval_response` / `cancel`**

`handle_ws` 的回合中 select 循环里，`msg = ws_rx.next()` 分支的 `Some(Ok(Message::Text(t)))` 处理替换为：

```rust
                            Some(Ok(Message::Text(t))) => {
                                if let Ok(incoming) = serde_json::from_str::<IncomingMessage>(&t) {
                                    match incoming.msg_type.as_str() {
                                        "cancel" => {
                                            info!("session {session_id}: cancel during turn");
                                            signal.store(true, Ordering::Relaxed);
                                            let _ = cancel_tx.send(true);
                                        }
                                        "approval_response" => {
                                            if let Ok(resp) =
                                                serde_json::from_str::<ApprovalResponse>(&t)
                                            {
                                                session.lock().await.registry.respond(
                                                    &resp.request_id,
                                                    resp.approved,
                                                    resp.remember,
                                                );
                                            }
                                        }
                                        _ => {} // 回合中的其他消息忽略
                                    }
                                }
                            }
```

同分支的 `Some(Ok(Message::Close(_))) | None` 与 `Some(Err(e))` 里，`signal.store(true, ...)` 之后各加一行 `let _ = cancel_tx.send(true);`（挂起的审批立即解锁）。

回合结束（`result = &mut result_rx` 分支，取到结果后）：加 `session.lock().await.registry.clear_pending();`（放在发送 `done` 之前或之后均可，确保与后续回合隔离）。

- [ ] **Step 5: 主循环（回合外）处理 `approval_response` 与 `cancel` 的 watch**

主循环 `match incoming.msg_type.as_str()` 增加分支：

```rust
            "cancel" => {
                // 回合外 cancel：置 signal + 解锁任何残留审批等待。
                signal.store(true, Ordering::Relaxed);
                let _ = cancel_tx.send(true);
                info!("session {session_id}: cancel outside turn");
            }
            "approval_response" => {
                // 迟到的审批响应（回合已结束，registry 已 clear）：静默忽略。
                if let Ok(resp) = serde_json::from_str::<ApprovalResponse>(&msg) {
                    session
                        .lock()
                        .await
                        .registry
                        .respond(&resp.request_id, resp.approved, resp.remember);
                }
            }
```

`msg` 是循环顶部已取出的 `String`；`ApprovalResponse` 单独解析（`IncomingMessage` 只有 msg_type/content/trace_id 三个字段，装不下审批响应）。

- [ ] **Step 6: 模块 doc 补契约核对表**

`main.rs` 顶部 wire protocol 文档（`### Server → Client` 列表后）追加：

```markdown
//! ### 契约核对表（前端 `useChatSession.ts` / `types/index.ts` 全部消息类型）
//!
//! | 消息 | 方向 | 状态 |
//! |---|---|---|
//! | `message` / `cancel` | C→S | ✅ 已实现 |
//! | `approval_request` / `approval_response` | 双向 | ✅ 已实现（本任务） |
//! | `thinking` / `tool_start` / `tool_end` / `content` / `error` / `done` | S→C | ✅ 已实现 |
//! | `subagent_*`（10 种） | S→C | ⏳ M2 规划（core 子 agent 编排落地后启用；前端处理器已存在，网关不发送） |
```

- [ ] **Step 7: 门禁 + 提交**

Run:
```bash
cargo fmt --all -- --check && cargo clippy --workspace --all-targets --all-features -- -D warnings && cargo test --workspace --all-features
git add gasket-gateway
git commit -m "feat(gateway): WebSocket 审批流（approval_request/approval_response + remember + 超时）"
```
Expected: 全绿；`cargo test -p gasket-gateway` 跑出 approval 模块 6 个单测。

---

### Task 4: 前端 subagent 死契约标记 + 契约核对（无渲染改动）

**Files:**
- Modify: `web/src/composables/useChatSession.ts:267-294`（subagent_* 分支加注释）
- Modify: `web/src/types/index.ts:46-66`（类型注释）
- Modify: `web/src/components/MessageThoughtsPanel.vue:282-290`（渲染处注释）

**Interfaces:** 无（纯注释 + 验证）。

- [ ] **Step 1: 标记 subagent_* 为 M2 规划**

`useChatSession.ts` 的 `processWebSocketMessageInner` 中 `subagent_*` 分支（`:267-294`）上方加一行注释：

```ts
      // subagent_* 协议是 M2（core 子 agent 编排）的预留契约：网关当前
      // 从不发送，这些分支保持无害惰性。M2 落地后由网关协议激活。
```

`types/index.ts` 的 `SubagentWsMessage` 类型上方注释改为：

```ts
/**
 * WebSocket message types for subagent events.
 * M2 规划：网关当前不发送这些消息（core 子 agent 编排未实现），
 * 类型保留供 M2 使用，勿删除。
 */
```

`MessageThoughtsPanel.vue` 两处 `v-if`（`:282-290`）上方加：

```html
      <!-- M2：subagent_* 协议尚未实现，subagents 恒为空，以下两块不会渲染 -->
```

- [ ] **Step 2: 验证前端可构建**

Run（`web/` 目录）：`pnpm build`
Expected: 构建成功（类型检查通过，注释不改行为）。

- [ ] **Step 3: 提交**

```bash
git add web/src
git commit -m "chore(web): 标记 subagent_* 协议为 M2 预留（网关不发送，保持惰性）"
```

---

### Task 5: 死依赖清理

**Files:**
- Modify: `gasket/Cargo.toml`（删除 9 行）
- Modify: `gasket-core/Cargo.toml:32`（`async-trait`——Task 1 采用手写 boxed future，未使用）
- Modify: `gasket-host/Cargo.toml:10`（`colored`）

**Interfaces:** 无。清理后 `Cargo.lock` 由 cargo 自动更新。

- [ ] **Step 1: 删除 workspace 死依赖**

`gasket/Cargo.toml` 删除以下行（全部核实零 `use`，见 2026-07-31 规划侦察）：
- `:75` `teloxide = ...`
- `:78` `serenity = ...`
- `:81` `tokio-tungstenite = ...`
- `:84` `tiktoken-rs = "0.12"`
- `:93` `cron = "0.17"`
- `:99` `async-channel = "2"`
- `:102` `termimad = "0.35"`
- `:117` `dialoguer = "0.12"`
- `:120` `readability = "0.3"`

`gasket-core/Cargo.toml:32` 删除 `async-trait = { workspace = true }`；`gasket-host/Cargo.toml:10` 删除 `colored = { workspace = true }`。

- [ ] **Step 2: 验证无残留引用**

Run:
```bash
grep -rn "teloxide\|serenity\|tokio-tungstenite\|tiktoken\|readability\|termimad\|dialoguer\|async-channel\|async_trait\|colored" --include="*.rs" . | head
cargo check --workspace
```
Expected: 源码零匹配；编译通过。

- [ ] **Step 3: 门禁 + 提交**

Run:
```bash
cargo test --workspace --all-features && cargo clippy --workspace --all-targets --all-features -- -D warnings && cargo fmt --all -- --check
git add Cargo.toml gasket-core/Cargo.toml gasket-host/Cargo.toml Cargo.lock
git commit -m "chore: 清理 12 个零使用依赖（bot/ide/token 相关与 async-trait/colored）"
```
Expected: 全绿。

---

### Task 6: release.yml 修复 + Tauri 桌面打包

**Files:**
- Modify: `.github/workflows/release.yml`（去 protoc；加 desktop job）

**Interfaces:** 无。产物：`gasket-cli` 三平台二进制（既有）+ Tauri 桌面安装包（新）。

- [ ] **Step 1: 移除 release.yml 的 protoc 步骤**

`.github/workflows/release.yml` 中 `build-release` job 的 `Install protoc` 步骤（`arduino/setup-protoc@v3`，与 ci.yml 同源遗留）整块删除。

- [ ] **Step 2: 新增 desktop job**

`build-release` job 之后追加：

```yaml
  desktop:
    name: Desktop (${{ matrix.platform }})
    runs-on: ${{ matrix.platform }}
    strategy:
      fail-fast: false
      matrix:
        platform: [macos-latest, windows-latest]
        include:
          - platform: macos-latest
            args: --target aarch64-apple-darwin
          - platform: windows-latest
            args: ""
    steps:
      - uses: actions/checkout@v4

      - name: Install Rust
        uses: dtolnay/rust-toolchain@stable
        with:
          targets: aarch64-apple-darwin

      - name: Install pnpm
        uses: pnpm/action-setup@v4
        with:
          version: 9

      - name: Setup Node
        uses: actions/setup-node@v4
        with:
          node-version: 20
          cache: pnpm
          cache-dependency-path: web/pnpm-lock.yaml

      - name: Install web deps
        run: pnpm install --frozen-lockfile
        working-directory: web

      - name: Build web dist
        run: pnpm build
        working-directory: web

      - name: Tauri build
        run: pnpm tauri build ${{ matrix.args }}
        working-directory: web
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}

      - name: Upload Desktop Artifact
        uses: actions/upload-artifact@v4
        with:
          name: gasket-desktop-${{ matrix.platform }}
          path: |
            web/src-tauri/target/release/bundle/**/*.dmg
            web/src-tauri/target/release/bundle/**/*.msi
            web/src-tauri/target/release/bundle/**/*.exe
```

> Linux 桌面打包依赖系统 webkit2gtk，首次在 ubuntu runner 上要 `apt-get install libwebkit2gtk-4.1-dev libgtk-3-dev` 等——本 job 先用 mac/windows（Tauri 官方支持的顺滑路径），linux 作为后续增强（在 spec §8 风险表已标注）。

- [ ] **Step 3: YAML 校验 + 提交**

Run: `python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/release.yml')); print('YAML OK')"`
Expected: `YAML OK`。

```bash
git add .github/workflows/release.yml
git commit -m "ci(release): 移除 protoc 遗留并新增 Tauri 桌面打包 job"
```

---

### Task 7: 浏览器 E2E（审批四条路径）+ 最终门禁 + v2.0.0

**Files:** 无代码改动（验证与发布）。

**Interfaces:** 依赖 Task 1-6 全部完成。

- [ ] **Step 1: 启动真实全链路**

Run:
```bash
# 终端 1：网关（.env 已在 gasket/ 下，含真实 LLM 配置）
cd gasket && cargo build -p gasket-gateway
GASKET_GATEWAY_MODE=auto-edit GASKET_APPROVAL_TIMEOUT_S=300 ./target/debug/gasket-gateway
# 终端 2：前端（开发模式指向网关）
cd web && pnpm dev   # VITE_API_URL/WS_URL 默认 localhost:3000，网关默认端口 3000
```

Expected: 网关日志 `gasket-gateway listening on 0.0.0.0:3000`；浏览器打开 `http://localhost:5173` 可见聊天界面。

- [ ] **Step 2: 审批 approve 路径**

浏览器输入：「用 bash 工具运行 `echo hello` 并汇报结果」（bash = High risk，AutoEdit 模式触发审批）。
Expected: 聊天流中出现审批对话框（工具名 bash + 参数摘要）；点 Approve → 工具执行，结果回流，`-> bash [ok]` 样式事件可见。

- [ ] **Step 3: 审批 deny 路径**

输入：「用 bash 运行 `echo blocked`」→ 对话框点 Deny。
Expected: 工具被 Block，模型收到错误 tool_result（"bash denied by user"），回合继续，无 bash 实际执行（终端无输出）。

- [ ] **Step 4: remember 路径**

Step 2 中勾选 remember 后，再发一次同工具消息。
Expected: 第二次不再弹窗（`RegisterOutcome::Remembered` 直通）。`/clear` 不清 memory（按设计，进程重启才清）。

- [ ] **Step 5: 超时路径**

以 `GASKET_APPROVAL_TIMEOUT_S=2` 重启网关，发 bash 请求但不点对话框。
Expected: 约 2 秒后对话框消失/回合继续，工具被 Block（"denied by user"），无挂起。

- [ ] **Step 6: cancel 路径（回合中取消）**

回合进行中（审批对话框挂着）点前端 Stop 按钮。
Expected: 审批立即解锁、回合 Aborted、无额外 provider 请求（Task 1 早退生效——网关日志无第二次 provider request）。

- [ ] **Step 7: 最终门禁**

Run（`gasket/` 目录）:
```bash
cargo fmt --all -- --check && cargo clippy --workspace --all-targets --all-features -- -D warnings && cargo test --workspace --all-features
```
Expected: 全绿。

- [ ] **Step 8: 发布 v2.0.0**

Run:
```bash
git tag v2.0.0 && git push origin main --tags
```
然后在 GitHub 创建 Release（`release.yml` 监听 `published` 事件，自动构建 CLI 三平台 + 桌面安装包）。创建后确认 Actions 两个 workflow 均通过。

---

## Self-Review 记录

- **Spec 覆盖**：§3.1 HookChain 异步化 → Task 1；§3.2 async approver → Task 2；§3.3 审批协议 → Task 3；§3.4 前端 subagent 处置 + 契约表 → Task 4（核实渲染条件后确认无需移除渲染，改为标记）；§3.5 死依赖 → Task 5；§3.6 release + Tauri → Task 6；§5 错误处理（超时/断开/重复响应）→ Task 3 Step 1-5 + Task 7；§6 测试（core/host/gateway/E2E/门禁）→ Task 1/2/3/7；§7 M2 注明 → Task 4；§8 风险 → Task 1（入口早退）、Task 6（linux 打包）。
- **占位符扫描**：无 TBD/TODO 步骤；每个代码步骤含完整可粘贴代码。
- **类型一致性**：`Approver`（HRTB）在 Task 2 定义、Task 3 网关与 Task 7 CLI 按同一签名使用；`RegisterOutcome`/`ApprovalRegistry` 方法签名 Task 3 内自洽；`OutgoingEvent::approval_request(request_id, tool_name, args)` 与前端 `msg.id/tool_name/description/arguments` 字段逐字对齐。
