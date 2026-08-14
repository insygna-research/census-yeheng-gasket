# gasket × dsh 对齐改造计划(供 review)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 gasket 的会话真相源从"调用方内存 Vec + 成功后才落盘"换成"dsh 式 append-only 事件日志 + 纯投影",并按优先级落地收件箱、取消语义、压缩日志化、工具并发、工程设施六组改造。

**Architecture:** 新增 `SessionEvent` 词汇表与 `derive_messages()` 纯投影(gasket-core);`JsonlStorage` 泛化为事件日志存储,保留 torn-tail 自愈;agent loop 经 `AgentLoopConfig.persist` 回调实时落盘(assistant 先于工具执行落盘);`Host::run_turn` 改为从日志派生历史、逐事件追加;网关/CLI 只做接线。不做运行时插件系统、不做 Cordis 式 DI——gasket 的 Rust 编译期组合已经是对的。

**Tech Stack:** Rust(tokio、serde、axum、uuid),现有依赖零新增。

## Global Constraints

- 每个任务结束 `cd gasket && cargo test --all-features`、`cargo clippy --all-features -- -D warnings`、`cargo fmt --all` 全绿。
- 存储切换是破坏性变更:旧 `messages.jsonl` 一次性迁移为 `events.jsonl`,旧文件重命名保留(`messages.jsonl.migrated`),不删除用户数据。
- 未知事件类型解析失败 = 加载失败(fail closed),禁止静默丢弃或降级清空。
- 不新增 crate、不新增外部依赖。
- loop 保持无状态:`persist` 与 `emit` 同级,都是回调。
- 文档行号引用在本计划落地时同步更新(Phase 5 的 doc-drift gate 上线前手工)。

---

## 0. Review 决策点(先看这里)

| # | 决策 | 我的建议 | 备选 |
|---|---|---|---|
| D1 | 迁移后旧文件 | 重命名 `.migrated` 保留 | 直接删(更干净,但丢回滚路径) |
| D2 | 取消原因是否入日志 | 入(dsh 只存粗粒度 `aborted`;gasket 自有格式无兼容负担,`TurnEnd::Aborted{cause}` 对调试/审计有用) | 学 dsh 只存粗粒度 |
| D3 | `GET /api/sessions/{key}/messages` 端点 | Phase 0 就加(后端真相出口,前端 true-up 延后单独做) | 连端点一起延后 |
| D4 | 工具并发度 / 工具超时默认值 | k=4 并发;`GASKET_TOOL_TIMEOUT_S` 默认 120,0=关闭 | 更保守 k=2 |
| D5 | Phase 4(并发)是否可与 Phase 1(收件箱)并行 | 可以(互不触碰同一文件的核心路径) | 串行 |
| D6 | CLI 运行中输入队列(Phase 1 的 CLI 半边) | 延后(CLI 的 reedline 交互改造复杂度高、收益低;先做网关侧) | 一起做 |

**阶段依赖**:

```
Phase 0 事件日志(基础,必须最先)
   ├─→ Phase 1 收件箱(消费事件日志)
   ├─→ Phase 2 取消语义(TurnEnd 类型 Phase 0 已预留)
   └─→ Phase 3 压缩日志化(消费事件日志)
Phase 4 工具并发+超时(独立,可并行)
Phase 5 工程设施(独立,随时;ADR 目录在 Phase 0 Task 6 已 seeded)
```

**明确非目标**(dsh 有、gasket 不抄):运行时插件/Cordis DI、capability seam 三件套、`request/header` 事件(配置来自 env,跨配置 replay 价值低)、`assistant/chunk` token 级保真(UI 实时流已够)、`session/end-seed`/fork API(resume 已覆盖)、merge-extensible `ignorable` 机制(gasket 读写两端同源,单写者无需前向兼容)、per-file 100% 覆盖率、文档字数预算。

---

## 1. Phase 0:事件溯源会话日志(详细计划)

### Task 1: `SessionEvent` 词汇表 + `derive_messages` 投影

**Files:**
- Create: `gasket/gasket-core/src/types/session_event.rs`
- Modify: `gasket/gasket-core/src/types/mod.rs`(mod + re-export)
- Modify: `gasket/gasket-core/src/lib.rs`(导出)

**Interfaces(后续任务依赖的精确形态):**

```rust
// types/session_event.rs
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use crate::types::message::AgentMessage;

/// Token 用量,随产生它的 assistant 消息一起落盘(dsh:"the model output
/// and its accounting travel together")。若 types/context.rs 已有等价结构,
/// 复用之并补 Serialize/Deserialize/Clone/PartialEq derives。
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage { pub input_tokens: u64, pub output_tokens: u64 }

/// 会话事件日志的 append-only 词汇表。User/Assistant/ToolResult 直接包
/// AgentMessage 对应变体(serde 兼容旧消息行,迁移即按判别式包裹)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    TurnStart,
    User(AgentMessage),                                // AgentMessage::User
    Assistant { message: AgentMessage, usage: Option<Usage> }, // AgentMessage::Assistant
    ToolResult(AgentMessage),                          // AgentMessage::ToolResult
    TurnEnd { reason: TurnEndReason },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TurnEndReason {
    Completed,
    Aborted { cause: Option<CancelCause> },
    Error { message: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CancelCause { User, Parent, Hook { reason: String } }

impl SessionEvent {
    /// 消息 → 事件(仅三类消息面;TurnStart/TurnEnd 不由此构造)。
    pub fn from_message(msg: &AgentMessage, usage: Option<Usage>) -> Option<SessionEvent>;
}

/// 纯投影:日志 → 模型可见消息。TurnStart/TurnEnd 不产出消息。
/// 崩溃遗留的无 TurnEnd 尾部按现状投影(partial 事实完整保留)。
pub fn derive_messages(log: &[SessionEvent]) -> Vec<AgentMessage>;
```

- [ ] **Step 1: 写失败测试**(inline `#[cfg(test)]` in session_event.rs)

```rust
#[test]
fn derive_projects_only_surface_events() {
    let log = vec![
        SessionEvent::TurnStart,
        SessionEvent::User(AgentMessage::user("hi")),
        SessionEvent::Assistant { message: AgentMessage::assistant_text("hello"), usage: Some(Usage { input_tokens: 10, output_tokens: 5 }) },
        SessionEvent::TurnEnd { reason: TurnEndReason::Completed },
    ];
    assert_eq!(derive_messages(&log).len(), 2); // user + assistant
}

#[test]
fn derive_tolerates_missing_turn_end() { // torn-tail 崩溃遗留
    let log = vec![
        SessionEvent::TurnStart,
        SessionEvent::User(AgentMessage::user("hi")),
        SessionEvent::Assistant { message: AgentMessage::assistant_text("partial"), usage: None },
    ];
    assert_eq!(derive_messages(&log).len(), 2);
}

#[test]
fn serde_tag_shape_is_snake_case() {
    let ev = SessionEvent::TurnEnd { reason: TurnEndReason::Aborted { cause: Some(CancelCause::User) } };
    let s = serde_json::to_string(&ev).unwrap();
    assert!(s.contains(r#""type":"turn_end""#));
    assert!(s.contains(r#""kind":"aborted""#));
}

#[test]
fn unknown_type_discriminant_fails_to_parse() { // fail closed
    assert!(serde_json::from_str::<SessionEvent>(r#"{"type":"wat","data":1}"#).is_err());
}
```

(注:`AgentMessage::user`/`assistant_text` 构造辅助若不存在,在 `types/message.rs` 补 `#[cfg(test)]` 或公开构造,以实际载荷类型为准。)

- [ ] **Step 2: 跑测试确认失败** — `cd gasket && cargo test -p gasket-core session_event`
- [ ] **Step 3: 最小实现** `SessionEvent`/`TurnEndReason`/`CancelCause`/`Usage`/`from_message`/`derive_messages` + mod 导出
- [ ] **Step 4: 跑测试确认通过**
- [ ] **Step 5: Commit** — `feat(core): session event vocabulary and derive_messages projection`

### Task 2: 事件日志存储(泛化 torn-tail 扫描)

**Files:**
- Modify: `gasket/gasket-core/src/storage/mod.rs`(抽取通用行扫描,新增事件存取)
- Modify: `gasket/gasket-core/src/lib.rs`(导出)

**Interfaces:**

```rust
// storage/mod.rs 内新增(与 JsonlStorage 同文件,复用目录布局与会话校验)
pub struct EventStorage { root: PathBuf }   // ~/.gasket/sessions/<id>/events.jsonl

impl EventStorage {
    pub fn new(root: PathBuf) -> Self;
    pub fn events_path(&self, session_id: &str) -> PathBuf;
    pub fn messages_path(&self, session_id: &str) -> PathBuf; // 旧文件探测/迁移用
    pub fn has_events(&self, session_id: &str) -> bool;
    pub fn append_event(&self, session_id: &str, ev: &SessionEvent) -> Result<(), AgentError>;
    pub fn append_events(&self, session_id: &str, evs: &[SessionEvent]) -> Result<(), AgentError>;
    pub fn load_events(&self, session_id: &str) -> Result<Vec<SessionEvent>, AgentError>;
    pub fn load_messages(&self, session_id: &str) -> Result<Vec<AgentMessage>, AgentError>; // 迁移源
    pub fn rename_legacy(&self, session_id: &str) -> Result<(), AgentError>; // messages.jsonl → .migrated
}

// 从 parse_transcript 抽取的通用扫描(单实现,两个存储共用):
fn scan_jsonl<T: serde::de::DeserializeOwned>(
    path: &Path, repair_tail: bool,
) -> Result<Vec<T>, AgentError>;
// 语义保持:末行坏 → 丢弃 + 就地截断(repair_torn_tail);中间行坏 → Err 带行号。
```

- [ ] **Step 1: 写失败测试**(inline,与现有 11 个存储测试同风格)

```rust
#[tokio::test]
async fn events_round_trip() { /* append 3 events → load → eq */ }

#[tokio::test]
async fn events_torn_tail_last_line_dropped_and_repaired() {
    // 写两行合法 + 半行 JSON → load 返回 2,文件被截断
}

#[tokio::test]
async fn events_mid_file_corruption_errors_with_line_number() { /* 第 1 行坏 → Err(Transcript) */ }

#[tokio::test]
async fn events_unknown_variant_fails_load() {
    // 手写一行 {"type":"from_the_future",...} → Err —— fail closed 是存储层契约
}
```

- [ ] **Step 2: 确认失败** — `cargo test -p gasket-core storage`
- [ ] **Step 3: 实现** — 抽 `scan_jsonl::<T>`(现 `parse_transcript` 改为 `scan_jsonl::<AgentMessage>` 的薄包装,11 个既有测试不动、必须仍绿);`EventStorage` 按 `JsonlStorage` 同样的 O_APPEND + 单次 `write_all("line\n")` 纪律实现
- [ ] **Step 4: 全部存储测试通过**(新 4 + 旧 11)
- [ ] **Step 5: Commit** — `feat(core): event log storage with shared torn-tail scanner`

### Task 3: loop 持久化回调

**Files:**
- Modify: `gasket/gasket-core/src/types/context.rs`(`AgentLoopConfig` 增字段)
- Modify: `gasket/gasket-core/src/agent_loop.rs`

**Interfaces:**

```rust
// AgentLoopConfig 新增(与 emit 同级的回调,loop 仍无状态):
pub persist: Option<Arc<dyn Fn(&SessionEvent) -> Result<(), AgentError> + Send + Sync>>,
```

落盘时序(崩溃安全的核心不变量):

1. assistant 消息组装完成之后、**任何工具执行之前** → `persist(Assistant{message, usage})`
2. 每个工具结果完成(含 `after_tool_call` 改写后)→ `persist(ToolResult)`
3. `persist` 返回 `Err` → 立即终止本轮,向上返回 `Err`(存储失败 fail loud,不静默续跑)
4. `persist: None` → 行为与现状完全一致(14 个既有 loop 测试不动、必须仍绿)

- [ ] **Step 1: 写失败测试**(agent_loop.rs inline tests,复用 MockStream 模式)

```rust
#[tokio::test]
async fn persist_writes_assistant_before_tool_results() {
    // 脚本流:text + tool_call;收集 persist 调用序
    // assert 顺序 == [Assistant, ToolResult];Assistant 内含 tool_call block
}

#[tokio::test]
async fn persist_error_aborts_run() {
    // persist 闭包第 1 次即 Err → run_agent_loop 返回 Err
}

#[tokio::test]
async fn blocked_tool_call_still_persists_result() {
    // hook Block → ToolResult(is_error, reason) 仍 persist(被拒也是事实)
}
```

- [ ] **Step 2: 确认失败** — `cargo test -p gasket-core agent_loop`
- [ ] **Step 3: 实现**(在 `run_agent_loop` 主循环与 `execute_tool_calls` 内插桩;`stream_assistant_response` 返回值需带上本次 Usage —— 从 StreamChunk::Usage 累积值透出,`AgentEvent::AfterProviderResponse` 通道不变)
- [ ] **Step 4: 新旧测试全绿**
- [ ] **Step 5: Commit** — `feat(core): loop persist callback, assistant precedes tool execution on disk`

### Task 4: Host 改造 + 一次性迁移

**Files:**
- Modify: `gasket/gasket-host/src/session.rs`(open_or_migrate,删 resume_or_adopt)
- Modify: `gasket/gasket-host/src/lib.rs`(`run_turn` 重签名)
- Modify: `gasket/gasket-host/src/config.rs`(`TurnInputs` 摄取派生历史)
- Modify: `gasket/gasket-host/tests/integration.rs`(4 个既有测试改造)
- Modify: `gasket/gasket-core/examples/cli_host.rs`(接线更新)

**Interfaces:**

```rust
// session.rs
impl SessionManager {
    /// 打开或迁移:events.jsonl 存在 → load;否则 messages.jsonl 存在 →
    /// 旧消息逐条 SessionEvent::from_message 包裹写入 events.jsonl,
    /// 旧文件 rename 为 messages.jsonl.migrated(D1);两者皆无 → 新会话。
    /// 中间行损坏 → Err(不再 adopt,fail closed)。
    pub async fn open_or_migrate(&self, session_id: &str) -> Result<Vec<SessionEvent>, AgentError>;
}

// lib.rs —— history 参数消失,日志是唯一真相
pub struct TurnSummary {
    pub reason: TurnEndReason,
    pub new_messages: Vec<AgentMessage>, // 本次新增(调用方做 UI/统计用,真相仍在日志)
}

impl Host {
    pub async fn run_turn(
        &self, user_msg: &str, on_event: impl FnMut(AgentEvent) + Send,
    ) -> Result<TurnSummary, AgentError>;
}
```

`run_turn` 新流程:

```
persist(TurnStart) → persist(User) → history = derive_messages(load_events)
→ budget.compact(&history)(语义不变:只缩内存)→ prepare_turn(注入 persist 闭包)
→ run_agent_loop(loop 内逐事件落盘)
→ persist(TurnEnd{reason})   // Ok+无信号=Completed;Ok+信号=Aborted{cause};Err=Error{msg}
→ 返回 TurnSummary
```

配套:`SessionManager::open_or_migrate` 顺带从日志尾部扫最后一个带 usage 的 Assistant 事件 → `ContextBudget::record_input_tokens`(重启后 token 感知压缩不再退化为按条数)。

- [ ] **Step 1: 写失败测试**(host/tests/integration.rs)

```rust
#[tokio::test]
async fn mid_turn_failure_preserves_side_effect() { // 本计划存在的理由
    // 工具 = 写临时文件;MockStream:第 1 响应 tool_call,第 2 响应流中途 Error
    // assert: 文件已存在(副作用发生)
    // assert: 重新 open 会话 → derive 含 user+assistant+toolresult
    // assert: 日志无 TurnEnd 或 TurnEnd=Error(崩溃/失败不留"半截对话"但留全部事实)
}

#[tokio::test]
async fn aborted_turn_persists_partial_facts() {
    // 工具执行间隙置位 signal → TurnEnd{Aborted} 落盘;已完成的 ToolResult 在日志里
}

#[tokio::test]
async fn success_path_log_equals_legacy_messages() {
    // 完整成功轮:derive_messages(log) == 旧行为的 history + new_msgs
}

#[tokio::test]
async fn legacy_messages_migrate_once_and_keep_backup() {
    // 造旧 messages.jsonl → open_or_migrate → events.jsonl 出现,
    // messages.jsonl.migrated 保留,二次 open 不再迁移
}

#[tokio::test]
async fn corrupted_session_errors_instead_of_adopting() {
    // 中间行损坏 → open_or_migrate Err(旧 resume_or_adopt 行为删除)
}
```

- [ ] **Step 2: 确认失败** — `cargo test -p gasket-host`
- [ ] **Step 3: 实现**(CLI main.rs 的本地 `history: Vec` 与 `history.extend` 删除,压缩改作用于派生副本 —— CLI 属 Task 5 但编译需同步改,先最小接线)
- [ ] **Step 4: host + core 全绿**
- [ ] **Step 5: Commit** — `feat(host)!: event-sourced run_turn with one-shot legacy migration`
