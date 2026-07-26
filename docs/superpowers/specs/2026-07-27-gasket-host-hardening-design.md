# gasket-host 夯实设计（查漏补缺）

- 日期：2026-07-27
- 状态：**Draft for Review**（含 Linus review 4 条修订，待批准）
- 范围：加固既有 `gasket-host` crate（config / session / permission / printer），新增一个 `Host` 编排结构 + 一套确定性集成测试地基
- 关联：[`docs/superpowers/specs/2026-07-26-a2a-host-design.md`](./2026-07-26-a2a-host-design.md) §1.1 指出"gasket-host 尚未稳定"是 a2a 不依赖它的原因；本设计正是补这个缺口，让 `Host` 成为未来 a2a `TaskFactory` 可包的稳定调用点

---

## 0.1 目标

gasket-host 目前是薄胶水层（4 组件 + 2 个 `#[ignore]` smoke），只有 CLI 在用且跑得通，但集成主干在 CI **零覆盖**。本设计三件事：

1. **可测试性（V1）**：用确定性 fake `StreamFn` 打开 `build_loop_config` 的 stream_fn 注入点，把 `#[ignore]` smoke 换成离线全链路集成测试，CI 必跑。
2. **正确性补洞（C1/C2/C3/C5）**：`EventPrinter` 终于渲染 `Error` 事件并 flush；`HostError` 补 `Agent`（`Io` 删：死变体）；`SessionManager` 游标语义文档化。
3. **去重 + 稳定调用点（K1/A2/Host）**：`build_context` 消除 CLI/smoke 的 context 手抄；`install_ctrl_c` 助手消除手写 spawn；新增薄 `Host` 结构编排全链路，CLI/a2a/channel 经同一 `run_turn` 复用。

## 0.2 非目标（YAGNI 边界）

| 不做 | 原因 / 触发再做的条件 |
|---|---|
| `metadata.json` / session 名 / `delete_session` | 无消费者；`/sessions` UX 未提需求 |
| `risk_of` 接 `ToolDefinition` 风险元数据 | 硬编码表 V0 够用，仅加文档 |
| `SessionManager::list` 行计数优化 | 当前规模可接受 |
| per-agent profile 框架 | 单一 system_prompt+tools 直接传 `Host::new` 即可 |
| 内建 printer / Host trait 化 | 与"事件回调、a2a 可复用"冲突 |
| a2a `TaskFactory` 实际接线 | 那是 a2a crate 的活；本设计只保证 `Host` 是**对的形状**让它包 |

---

## 1. 缺口清单（查漏，决策依据）

| ID | 缺口 | 真/投机 | 严重 | 本设计 |
|---|---|---|---|---|
| **V1** | 集成主干（ConfigLoader+SessionManager+PermissionPolicy+EventPrinter+run_agent_loop）CI 零覆盖；2 smoke 全 `#[ignore]` | 真 | 高 | **做**：fake StreamFn + 注入点 |
| **C1** | `EventPrinter` 的 `_ => {}` 吞掉 `AgentEvent::Error` 等 → loop 错误终端不可见 | 真 | 中 | **做**：补 Error 分支 |
| **C2** | `EventPrinter` 从不 flush；管道输出卡顿 | 真 | 低-中 | **做**：每次事件 flush |
| **C3** | `HostError::Session(String)` 丢 io 源 | 真 | 低 | **做**：加 `Agent` 变体（`Io` 死变体，删） |
| **C4** | `risk_of` 硬编码工具名表，未知工具一律 High | 真（V0 可接受） | 中 | **仅文档化** |
| **C5** | `append(&self)` / `resume(&mut self)` 签名不一致；忘 resume 会静默写新 session | 真 | 中 | **仅文档化**（行为正确，非 bug） |
| **K1** | 无 `AgentContext` 组装助手，CLI/smoke 各手抄 | 真（今天就有重复） | 中 | **做**：`build_context` |
| **K2** | 无统一 Host runner | 投机（a2a 无真实用户） | 高 YAGNI | **做最小形态**：薄 Host + 事件回调 |
| **K3** | 无 per-agent profile | 投机 | 中 | **不做** |
| **A1** | 无 delete/metadata（`metadata.json` 从未写） | YAGNI | 低 | **不做** |
| **A2** | 无 ctrl_c 助手；CLI 手写 spawn | 真（小重复） | 低 | **做**：`install_ctrl_c` |
| **A3** | `list` 读整个文件计行 | 可接受 | 低 | **不做** |

**范围裁定**：V1 + C1 + C2 + C3 + C5 + K1 + A2 + K2(最小)。砍 A1/C4重做/A3/K3。

---

## 2. 架构

### 2.1 模块边界不变，新增 `Host`

4 个既有模块**职责不动**，各自文件内补洞。新增顶层 `Host` 编排 4 件套 + core 的 `run_agent_loop`，对外只暴露 `run_turn`。CLI 从"手拼 5 件套"变成"持一个 Host、每行调一次 `run_turn`"。

```
gasket-host/src/
├── lib.rs         HostError(补 Agent，删 Io) + 新增 Host + run_turn + install_ctrl_c
├── config.rs      HostConfig + provider_stream_fn + build_loop_config(带 stream_fn) + build_context
├── session.rs     SessionManager(C5 仅文档化)
├── permission.rs  PermissionPolicy(C4 仅文档化)
├── printer.rs     EventPrinter(C1 补 Error 分支 + C2 flush)
└── tests/
    ├── common/mod.rs   新增：FakeStream（dev-only）
    ├── integration.rs  新增：3 个 host_* 离线全链路（V1 验收）
    └── smoke_llm.rs    #[ignore] 保留，改用 Host
```

### 2.2 `Host` 骨架（核心）

```rust
pub struct Host {
    cfg: HostConfig,
    session: SessionManager,
    policy: Arc<PermissionPolicy>,
    signal: Arc<AtomicBool>,
    stream_fn: Arc<dyn StreamFn>,   // new() 用 provider；测试用 with_stream_fn 注入 fake
    system_prompt: String,
    tools: Vec<ToolDefinition>,
    cwd: PathBuf,
    max_turns: usize,               // 默认 cfg.tunables.max_turns
}

impl Host {
    pub fn new(cfg: HostConfig, session: SessionManager,
               policy: PermissionPolicy, system_prompt: String,
               tools: Vec<ToolDefinition>) -> Self { /* cwd=current_dir, signal=fresh, stream_fn=provider */ }

    /// 测试/自定义 provider 注入 stream_fn。
    pub fn with_stream_fn(self, stream_fn: Arc<dyn StreamFn>) -> Self { ... }
    pub fn with_max_turns(self, n: usize) -> Self { ... }

    /// 一次用户输入 → build_context → build_loop_config → run_agent_loop → 持久化。
    /// `on_event` 交出全部 AgentEvent（按值，与 core `run_agent_loop` 的 `E: FnMut(AgentEvent)` 对齐）。
    pub async fn run_turn<E>(
        &mut self, user_msg: AgentMessage, history: &mut Vec<AgentMessage>, on_event: E,
    ) -> Result<Vec<AgentMessage>, HostError>
    where E: FnMut(AgentEvent) { ... }

    pub fn session(&self) -> &SessionManager { &self.session }
    pub fn session_mut(&mut self) -> &mut SessionManager { &mut self.session }
    pub fn policy(&self) -> &PermissionPolicy { &self.policy }
    pub fn signal(&self) -> &AtomicBool { &self.signal }
}
```

**关键不变量**：
- `Host` **不持有 printer/writer**——渲染走 `on_event` 回调。这是 B1 地基，保证 a2a（无终端）能复用同一驱动。
- `signal` 由 Host 持有并暴露 `&AtomicBool`；`install_ctrl_c(host.signal().clone())` 在 main 装一次。
- `run_turn` 内部：`build_context` → `build_loop_config(挂 policy+signal+self.stream_fn)` → `run_agent_loop` → 成功后 `session.append(&new)`。
- **失败不持久化**：loop 返 `Err` → `run_turn` 直接 `Err`，不写半截 transcript。

### 2.3 数据流（单轮）

```
用户输入
  → Host::run_turn(user_msg, &mut history, on_event)
    → cfg.build_context(system_prompt, history, tools, cwd, env, session_id)       [K1]
    → cfg.build_loop_config(max_turns, Some(signal), Some(policy), self.stream_fn.clone()) [V1 注入点]
    → run_agent_loop(vec![user_msg], context, config, |ev| on_event(ev))           [core]
    ← Ok(new_msgs)
    → session.append(&new_msgs)                                                    [持久化]
    → 调用方 history.extend(new_msgs)                                              [与现状一致]
```

---

## 3. 组件改动签名

### 3.1 `config.rs` — K1 + V1 注入点

```rust
impl HostConfig {
    /// 抽出 provider 自带 stream_fn（OpenAiCompat / Anthropic）。
    /// Host::new 用它填 stream_fn 字段；power user 也可直接拿。
    pub fn provider_stream_fn(&self) -> Arc<dyn StreamFn> { ... }

    /// 唯一的 loop config 构造方法——stream_fn 显式传入（Host 传自己的字段，
    /// 测试传 fake）。原"无 stream_fn 重载"删除：gasket-host 是内部 crate，
    /// 唯一调用者 CLI/smoke 都在本次迁移，无外部 userspace 可"零破坏"。
    pub fn build_loop_config(
        &self, max_turns: usize, signal: Option<Arc<AtomicBool>>,
        hooks: Option<Arc<dyn HookChain>>, stream_fn: Arc<dyn StreamFn>,
    ) -> AgentLoopConfig { ... }

    /// 组装 AgentContext。消除 CLI/smoke 各手抄。
    pub fn build_context(
        &self, system_prompt: &str, history: &[AgentMessage], tools: Vec<ToolDefinition>,
        cwd: PathBuf, env: HashMap<String, String>, session_id: &str,
    ) -> AgentContext { ... }
}
```
> **签名变更（内部 breaking，无外部影响）**：`build_loop_config` 从 3 参变 4 参（加 `stream_fn`）。原 `build_loop_config_with` 与无 stream_fn 重载删除——它们是"为测试开的特殊口子 + 保护不存在的调用者"（Linus review #3）。`tools` 取所有权、`history` clone（与现状 `history.clone()` 一致）、`env` 由调用方 `vars().collect()`。

### 3.2 `printer.rs` — C1 + C2

```rust
pub fn on_event(&mut self, ev: &AgentEvent) {
    match ev {
        // 既有 MessageUpdate / ToolExecution* / AfterProviderResponse / TurnEnd 不动
        AgentEvent::Error { message } => {
            let _ = writeln!(self.out, "\n[error] {message}");   // C1
        }
        _ => {}   // Start/End 等保持静默（REPL 不需逐条噪音）
    }
    let _ = self.out.flush();   // C2
}
```

### 3.3 `session.rs` — C5 仅文档化

`append(&self)` / `resume(&mut self)` / `clear(&mut self)` **签名不动**。顶部加 doc 注释钉死：`new()` 生成 current_id；current_id 只被 `new`/`resume`/`clear` 改变；`append` 写入当前 current_id。行为正确，非 bug。

### 3.4 `permission.rs` — C4 仅文档化

`risk_of` 表不动，加注释："未知工具 → High 是安全默认；接 ToolDefinition 风险元数据是未来事，当前无消费者。"

### 3.5 `lib.rs` — HostError + Host + install_ctrl_c

```rust
#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("config error: {0}")]    Config(#[from] gasket_core::ConfigError),
    #[error("session error: {0}")]   Session(String),
    #[error("agent error: {0}")]     Agent(#[from] gasket_core::AgentError),   // run_turn 透传
}

/// 装 SIGINT 协作中止：按一次 Ctrl-C 置 signal=true，loop 下个安全点退出。
/// 每次按下都生效（loop 退出后由调用方重新置 false 再跑下一轮）。
pub fn install_ctrl_c(signal: Arc<AtomicBool>) { /* tokio::spawn ctrl_c 循环 */ }
```
> `Session(String)` 暂留——`SessionManager` 内部已把 `JsonlStorage` 的 `AgentError` `.to_string()`，全面换 source 要动 session.rs 多处，本轮不做。`Agent` 变体是 `run_turn` 透传所必需。
> **不**加 `Io(#[from] std::io::Error)`：无任何路径产生 `HostError::Io`（io 错误已被 `SessionManager` 吞成 `Session(String)`），是死变体（Linus review #2）。

---

## 4. 测试地基（V1 验收核心）

### 4.1 fake StreamFn（确定性，无新公共面）

放 `tests/common/mod.rs`，`futures-util` 进 **dev-dependency**（不进 `[dependencies]`，不影响 release）。**不开 cargo feature、不暴露公共 test 面**——a2a 真要时再提升（YAGNI）。

```rust
pub struct FakeStream {
    scripts: Mutex<VecDeque<Vec<StreamChunk>>>,   // 每次调用弹一个脚本
}
impl StreamFn for FakeStream {
    fn stream(&self, ..) -> Pin<Box<dyn Stream<Item = StreamChunk> + Send>> {
        let chunks = self.scripts.lock().unwrap().pop_front()
            .expect("FakeStream: script underflow — test supplied fewer scripts than stream() calls");
        Box::pin(futures_util::stream::iter(chunks))
    }
}
```
> 第 N 次 stream 调用返回第 N 个脚本。工具调用 = 脚本1（ToolCallDelta→Done）触发工具、脚本2（TextDelta→Done）收尾。完全确定。
> **脚本耗尽 panic，不静默 Done**（Linus review #1）：静默 fallback 会把"测试写错"变成"测试假阳性通过"——尤其失败测试遇 retry 会多次调 stream，静默 Done 会让本该 Err 的路径返回 Ok。

### 4.2 集成测试（`tests/integration.rs`，CI 必跑）

| 测试 | fake 脚本 | 断言 |
|---|---|---|
| `host_basic_chat` | `[TextDelta("pong"), Usage{1,1}, Done]` | ≥1 Assistant；EventPrinter 经回调输出非空；持久化后 `messages.jsonl` 有该条 |
| `host_tool_call` | 脚本1=`[ToolCallDelta(list), Done]`；脚本2=`[TextDelta("done"), Done]` | FullAuto 下产生 ToolResult；history 含 Assistant+ToolResult；持久化 |
| `host_failure_no_persist` | cfg 经 `GASKET_RETRY_MAX=0`（retry off）+ `[Error("boom")]` | `run_turn` 返 `Err`；session 文件**未写** |

> **retry 与失败测试（Linus review #1）**：`agent_loop` 对"无 content 的 Error"会 retry（`can_retry = !emitted_content && attempt <= max_retries`）。失败测试必须 `RetryPolicy::off()`——经 `GASKET_RETRY_MAX=0` 等 env 让 `tunables.retry` 清零（`build_loop_config` 用 `self.tunables.retry.clone()`）——否则 1 个 Error chunk 被 retry，fake 脚本耗尽 panic，测不出失败路径。

`tests/smoke_llm.rs` 保留 `#[ignore]`，改用 `Host`（让真实路径 = 已测路径）。

### 4.3 测试矩阵

| 文件 | 既有 | 新增 |
|---|---|---|
| config.rs | 3 | `build_context` 映射；`build_loop_config` 接受注入 stream_fn |
| permission.rs | 4 | — |
| printer.rs | 2 | `error_event_renders`；flush 后缓冲清空 |
| session.rs | 5 | — |
| lib.rs | — | `Host::new` 构造；`HostError` Agent 转换 |
| integration.rs | — | 3 个 `host_*` |
| smoke_llm.rs | 2 `#[ignore]` | 改用 Host |

---

## 5. CLI 迁移（gasket-cli/main.rs）

```rust
let mut host = Host::new(
    host_cfg, session, PermissionPolicy::new(mode, stdin_approver),
    "You are a helpful, concise assistant.".into(), built_in_tools(),
);
install_ctrl_c(host.signal().clone());

while let Ok(Signal::Success(line)) = rl.read_line(&prompt) {
    if let Some(cmd) = line.trim().strip_prefix('/') {
        handle_slash(cmd, &mut host, &mut history).await;   // host.session_mut()/policy()
        continue;
    }
    host.signal().store(false, Ordering::Relaxed);
    let user_msg = /* 同现状 */;
    match host.run_turn(user_msg, &mut history, |ev| {
        EventPrinter::new(io::stdout()).on_event(&ev);
    }).await {
        Ok(new) => history.extend(new),   // append 已在 run_turn 内做
        Err(e) => eprintln!("\n(run error: {e})"),
    }
}
```
> `/resume` 仍经 `host.session_mut().resume(id)` 返回 `Vec<AgentMessage>` 给 CLI 设 history。`/mode` 经 `host.policy().set_mode()`（AtomicU8，`&self` 够）。`/clear` 经 `host.session_mut().clear()`。

---

## 6. 错误处理

- `run_turn` 返回 `HostError`：`Agent`（loop 失败）/ `Session(String)`（持久化失败）。
- **失败不持久化**：`run_agent_loop` 返 `Err` → `run_turn` 立即 `Err`，不调 `session.append`（`host_failure_no_persist` 守护）。
- CLI 顶层仍 `Box<dyn Error>`；`HostError` 实现了 `Error`，透传即可。

---

## 7. 实施顺序（细节交 writing-plans）

每步独立编译+测试绿才进下一步：

1. `config.rs`：`provider_stream_fn` + `build_loop_config`(带 stream_fn) + `build_context` + 单测
2. `lib.rs`：`HostError` 加 `Agent`（不加 `Io`） + `Host` 结构 + `run_turn` + accessor + 构造单测
3. `printer.rs`：`Error` 分支 + `flush` + 单测
4. `session.rs` / `permission.rs`：仅文档注释
5. `tests/common/mod.rs`：`FakeStream` + `futures-util` dev-dep
6. `tests/integration.rs`：3 个 `host_*`（V1 验收）
7. `gasket-cli/main.rs`：迁移到 `Host` + `install_ctrl_c`
8. `tests/smoke_llm.rs`：改用 `Host`
9. CI：确认 `cargo test -p gasket-host` 跑通集成测试

CI 每个 PR：`cargo build --release` 无 warning、`cargo test -p gasket-host` 全过（含新集成）、`clippy -D warnings`、`fmt --check`。

---

## 8. 风险

| 风险 | 缓解 |
|---|---|
| `Host` 仍是投机抽象（a2a 无真实用户） | 做最小形态：薄结构 + 事件回调，不做 profile/trait 框架；CLI 去重是即时收益，不靠 a2a |
| `build_loop_config` 签名变更（+`stream_fn`） | 内部 breaking：gasket-host 是内部 crate，唯一调用者 CLI/smoke 本次迁移，无外部 userspace；CLI REPL 行为零变化才是真正的"never break userspace" |
| fake StreamFn 与真实 provider 行为分叉 | fake 只验装配/编排链路；真实 LLM 行为由保留的 `#[ignore]` smoke 覆盖 |
| `Host::new` 内部 `current_dir()` 是隐式副作用 | 与现状 CLI 行为一致（CLI 也是启动时取 cwd）；文档化 |

---

## 9. Review 检查清单

- [ ] **Host 不持 printer、渲染走 `on_event` 回调**——认可？（B1 地基，a2a 可复用）
- [ ] **`run_turn` 失败不持久化**、history 由调用方 extend——认可？
- [x] **`build_loop_config` 单方法带 `stream_fn`（删 `_with` 与无参重载）+ `provider_stream_fn`**——内部 breaking，无外部 userspace（Linus review #3，已采纳）
- [x] **fake 放 `tests/common/`、不开 feature、`futures-util` 仅 dev-dep、脚本耗尽 panic**——（Linus review #1，已采纳）
- [x] **`HostError` 删 `Io`、只留 `Agent`**——（Linus review #2，已采纳）
- [x] **`host_failure_no_persist` 用 `GASKET_RETRY_MAX=0`（retry off）**——（Linus review #1，已采纳）
- [ ] **C5/C4 选文档化不改签名**、`HostError` 保留 `Session(String)`——认可这个不彻底？
- [ ] **范围砍 A1/C4重做/A3/K3**——认可？

> Next step：本 spec 批准（Linus review 已完成，4 条修订已回填）→ `docs/superpowers/plans/2026-07-27-gasket-host-hardening.md` → 启动实施阶段 1。
