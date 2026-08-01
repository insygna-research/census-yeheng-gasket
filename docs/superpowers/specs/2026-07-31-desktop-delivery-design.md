# gasket 桌面端交付设计（M1）

- 日期：2026-07-31
- 状态：Draft for Review
- 范围：网关审批流（core `HookChain` 异步化，唯一公共 API 变更）+ 前端契约对齐 + 死依赖清理 + Tauri 打包发布
- 背景：core/host/CLI/ext/gateway 已完成（128 测试全绿）。桌面端（Tauri + Vue）聊天主链路已通（thinking/tool/content/error/done + context 统计 + 命令补全），但**审批流与 subagent 两整块功能是死契约**：前端有完整 ApprovalDialog / SubagentGridPanel / SubagentThoughtsPanel 与协议处理，网关从不发送 `approval_request` / `subagent_*`，也不处理 `approval_response`（落到 unknown-type warn 分支）。

## 1. 目标

让 Tauri 桌面端成为真实可用的产品：

1. **审批流全链路**：bash 等高风险工具在桌面端弹出审批对话框，approve/deny/remember 全部生效。
2. **契约诚实**：前端所有消息类型与网关逐一核对；subagent 死 UI 明确处置（保留待 M2 或移除渲染），不留「假装能用」。
3. **发布收敛**：死依赖清理、release.yml 修复 + Tauri 打包、v2.0.0 发布。

## 2. 非目标（YAGNI）

| 不做 | 原因 |
|---|---|
| 子 agent 编排（M2） | 独立的大特性；本设计只决定 UI 处置，不做协议 |
| bot 前端（teloxide） | spec 已两次推迟；无真实需求证据 |
| A2A | 无 spec 无实现，继续推迟 |
| token 感知压缩 | `compact_by_count` V0 在跑，未撞墙 |
| `after_tool_call` 异步化 | 纯变换（redact），无 IO 需求，保持 sync |

## 3. 架构

### 3.1 core：`HookChain::before_tool_call` 异步化（唯一 core API 变更）

```rust
pub trait HookChain: Send + Sync {
    fn before_tool_call(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        args: &serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = ToolCallVerdict> + Send + '_>>;
    // after_tool_call 保持 sync
}
```

- 手写 boxed future，与既有 `StreamFn` 同一风格（`async_trait` 不引入；该依赖现在零 usage，变 used-or-removed）。
- `execute_tool_calls` 的 hook 调用点加 `.await`。
- 影响面（已核实，全部内部）：`agent_loop.rs` 调用点、core 测试（~6 处 mock hook）、host `PermissionPolicy` / `HookStack` / `ExtensionApiImpl`、ext `permission_gate`。无外部 userspace。

### 3.2 host：`PermissionPolicy` async approver

```rust
pub type Approver = Arc<
    dyn Fn(&str, &serde_json::Value) -> Pin<Box<dyn Future<Output = bool> + Send>> + Send + Sync,
>;
```

- CLI：现有 sync stdin 逻辑包 `tokio::task::spawn_blocking`（REPL 单用户阻塞可接受，行为不变）。
- `HookStack` 组合语义不变（first-Block-wins / last-Modify / after 串联），实现改 async。
- `ExtensionApiImpl`（ext 的 `permission_gate`）同步包一层 `async move`。

### 3.3 gateway：审批协议（与前端现有代码逐字段对齐）

**Server → Client**（前端 `useChatSession` 已按此消费）：
```json
{"type":"approval_request","id":"<request_id>","tool_name":"<name>","description":"<args>"}
```

**Client → Server**（前端 `sendApprovalResponse` 已按此发送）：
```json
{"type":"approval_response","request_id":"<id>","approved":true,"remember":false}
```

**WsSession 新增**：
```rust
pending_approvals: HashMap<String, oneshot::Sender<bool>>,
approval_memory: HashMap<String, bool>,   // remember=true 的按工具名缓存
approval_seq: u64,
```

**approver 闭包**（构造于 `handle_ws`，捕获 session + cancel_watch）：
1. `remember` 命中缓存 → 立即返回缓存值。
2. 锁 session：发 `approval_request`（description = args 截断摘要）→ 注册 oneshot。
3. `select!` 三路：`oneshot` 收到响应 → 返回值；`cancel_watch.changed()`（WS 断开/cancel 消息触发）→ `false`；`timeout`（`GASKET_APPROVAL_TIMEOUT_S`，默认 300）→ `false`。
4. 返回 `false` = `ToolCallVerdict::Block("denied by user")`，core 已有机制回灌模型。

**主循环**：select 期间收到 `approval_response` → 查 `pending_approvals` 发 true/false；`remember=true` 写入 `approval_memory`；未知 `request_id` 忽略。`cancel` 消息 / WS close 额外触发 `cancel_watch`。

**网关模式**：`PermissionPolicy` 从 env `GASKET_GATEWAY_MODE` 解析（默认 `auto-edit`），否则 bash 永远 FullAuto 放行、审批流永远不触发。

**abort 窗口修复**（cancel 后不空烧 provider 请求）：`stream_assistant_response` 入口补 `is_aborted(config)` 早退。当前 abort 检查只在批内工具调用间（agent_loop.rs:210）和流式 chunk 循环内（:455）——cancel-during-approval 后 loop 会先发一次 provider 请求才在首个 chunk 处退出。

### 3.4 前端

- `ApprovalDialog` 已就绪（`pendingApprovals` → `ChatArea.currentApproval`），协议对齐后零改动工作。
- subagent 处置（实施时按渲染条件定）：`SubagentGridPanel` / `SubagentThoughtsPanel` 若默认隐藏（无消息不渲染）→ 保留组件与处理器（M2 资产，无害）；若可见空面板 → 移除渲染。处理器保留不删——它们只响应不存在的消息。
- 契约审计：前端全部 `type` 分支 vs 网关 `event_to_ws` + 入站 match 逐一核对，产出核对表。

### 3.5 死依赖清理（全部已核实零 `use`）

| 位置 | 依赖 |
|---|---|
| workspace | teloxide, serenity, tokio-tungstenite, tiktoken-rs, readability, cron, termimad, dialoguer, async-channel |
| gasket-core | async-trait（若本设计不采用） |
| gasket-host | colored |

清理后 `cargo tree -e normal` 与 `Cargo.lock` 核对无残留。

### 3.6 发布

- `release.yml`：移除 protoc 步骤（全仓无 proto 依赖，与 ci.yml 同步）；新增 `desktop` job（pnpm install → `pnpm tauri build` → 上传产物；Linux runner 需 webkit2gtk 系统依赖）。
- 全部完成后打 `v2.0.0` tag。

## 4. 数据流（审批轮）

```
用户发消息 → 网关 spawn agent loop → LLM 请求 bash
  → execute_tool_calls → hooks.before_tool_call (async)
    → PermissionPolicy::AutoEdit + High risk
      → approver(name, args)
        → 锁 session：WS 发 approval_request → 注册 oneshot
        → 前端 ApprovalDialog 弹出 → 用户点 Approve/Deny（可勾 remember）
        → WS 回 approval_response → 主循环 → oneshot.send(approved)
        → select! 取到 → 返回 bool
  → Allow → 执行 bash；Block → 错误 tool_result 回灌模型（既有机制）
```

## 5. 错误处理

| 场景 | 处理 |
|---|---|
| 审批超时（默认 300s） | `false` → Block("approval timed out") 回灌模型 |
| WS 断开 / cancel 时挂起审批 | cancel_watch 触发 → Block + signal → loop 在入口早退（3.3 修复） |
| 未知 request_id | 忽略 |
| 双端同时审批（重复点击） | 第一个 oneshot 已消费，第二个 send 失败静默（oneshot 语义天然防重） |

## 6. 测试

- **core**：mock hook 调用点机械迁移 `.await`；`stream_assistant_response` 入口 abort 早退补一个测试（signal 预置 → 零 chunk 消费即 Aborted）。
- **host**：PermissionPolicy 表驱动测试迁移 async approver（stub 返回 `Box::pin(async { ... })`）；新增「approver future 不 resolve 时 set_mode 仍可用」不需要——现有 4 测试迁移即可。
- **gateway（本轮首次补测试）**：`event_to_ws` / `session_key` / `context_stats` 单测 + 审批轮 fake 集成测试（fake WS 收发 + fake StreamFn，复用 host 的 FakeStream 模式）。
- **E2E**：浏览器驱动真实 ApprovalDialog（approve / deny / remember / 超时四条路径）。
- 门禁：fmt + `clippy -D warnings` + 全仓测试。

## 7. 未来演进（M2 候选）

- **子 agent 编排**：core 并行 agent（任务拆分 → 独立 loop 并发 → 事件流 → 汇总），网关实现 `subagent_*` 协议（前端 10 种消息类型已定义），激活现有 UI。独立 spec。
- bot 前端 / A2A：继续推迟。

## 8. 风险

| 风险 | 缓解 |
|---|---|
| `HookChain` async 化是 core 公共 API 变更 | 全部消费者内部（CLI/gateway/ext/host），无外部 userspace；一次迁移到位，不留 shim |
| cancel-during-approval 的 provider 请求窗口 | 3.3 入口早退，一行修复 + 测试 |
| Tauri Linux 打包（webkit2gtk 系统依赖） | CI 环境问题非代码问题；desktop job 先用 mac/windows，linux 标注依赖 |
| 前端 subagent 处置判断 | 实施时看渲染条件；保留 vs 移除二选一，不留「假装能用」 |
| 审批挂起导致 loop 不响应 cancel | select! 三路含 cancel_watch，cancel 必然解锁 |
