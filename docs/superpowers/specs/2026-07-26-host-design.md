# gasket Host 层设计

- 日期：2026-07-26
- 状态：已批准（待实现）
- 范围：`gasket-host`（可复用 lib）+ `gasket-cli`（REPL bin）

## 1. 目标

为 gasket-core 的纯函数 agent loop 设计一个真实可用的 Host 层：

- 一个**可复用的 `gasket-host` 库**，承载 host 侧逻辑（配置加载、session 管理、权限策略、事件渲染），未来 CLI 与 bot 共用。
- 一个**`gasket-cli` REPL 二进制**，用 `gasket-host` 组装出交互式终端 agent：多轮对话、session 恢复、流式输出、三档权限。

架构基调（本 session 已确立）：**loop 是无状态纯函数，host 持有状态/配置/持久化/策略**。配置从 host 单向流入 loop，loop 不读 env、不知道 host 存在。

## 2. 非目标（YAGNI）

- **不抽 `Frontend` trait**：只有一个前端（CLI）时不预先抽象。bot 出现时再提取（extract on second use）。
- **不做 context compaction / 成本预算 / MCP**：这些是生产级 coding agent 的范畴，gasket 定位轻量个人助手，按需再做。
- **不把 REPL 循环放进 lib**：reedline/stdin 是 CLI-specific，进 lib 就是 IO 混逻辑的 god class。
- **不给 core 加 `RiskLevel`/`PermissionPolicy`**：权限机制（`before_tool_call` hook）已在 core，策略归 host。

## 3. 架构

### Workspace 布局（2 个新 crate）

```
gasket/
  gasket-core/      既有 lib（纯 loop + tools + providers + storage + config loaders + hooks + events）
  gasket-host/      新 lib（可复用 host 模块）
    src/{lib,config,session,permission,printer}.rs
    Cargo.toml      deps: gasket-core, dotenvy, colored
  gasket-cli/       新 bin（REPL）
    src/main.rs
    Cargo.toml      deps: gasket-host, gasket-core, reedline, tokio, colored
```

lib/bin 分离：CLI-only 的 `reedline` 不污染 `gasket-host`；`gasket-host` 只依赖纯逻辑 + 输出渲染依赖（`colored`）。

### 依赖关系

```
gasket-cli --> gasket-host --> gasket-core
```

## 4. 组件

### 4.1 `ConfigLoader`（gasket-host/src/config.rs）

host 启动时调一次，把环境配置聚合成一个结构。

```rust
pub struct HostConfig {
    pub provider: ProviderConfig,   // 连接（base_url/key/model/api/proxy）
    pub tunables: AgentTunables,    // loop 旋钮（max_turns/max_tokens/thinking/retry）
}

impl ConfigLoader {
    /// 加载顺序：dotenvy::dotenv()（best-effort）-> ProviderConfig::from_env() -> AgentTunables::from_env()。
    pub fn load() -> Result<HostConfig, HostError>;
}
```

- 缺 `GASKET_LLM_*` 必填项时返回清晰错误（CLI 据此退出，不静默 mock）。
- 复用 core 已有的 `from_env` / `from_env_with` 可注入模式，便于测试。

### 4.2 `SessionManager`（gasket-host/src/session.rs）

包 `JsonlStorage`，补 core 没有的"当前 session / 列举 / 最近"语义。

```rust
pub struct SessionInfo { pub id: String, pub mtime: SystemTime, pub msg_count: usize }

pub struct SessionManager { storage: JsonlStorage, current_id: String }

impl SessionManager {
    pub fn new() -> Self;                                   // 默认 root ~/.gasket/sessions，新建 uuid
    pub fn current_id(&self) -> &str;
    pub fn resume(&mut self, id: &str) -> Result<Vec<AgentMessage>, HostError>;
    pub fn resume_last(&mut self) -> Result<Vec<AgentMessage>, HostError>;
    pub fn list(&self) -> Result<Vec<SessionInfo>, HostError>;
    pub fn append(&self, msgs: &[AgentMessage]) -> Result<(), HostError>;  // 增量持久化
    pub fn clear(&mut self);                                // 开新 session_id
}
```

- `resume_last`：扫 `sessions/` 子目录按 mtime 取最新，`load_messages` 灌回。
- `append` 只写本轮新增消息（append-only 增量，不重写全量）。

### 4.3 `PermissionPolicy`（gasket-host/src/permission.rs）

实现 core 的 `HookChain` trait；三档模式 + 工具风险映射 + 确认闭包。

```rust
pub enum Mode { Suggest, AutoEdit, FullAuto }   // Copy, FromStr（--mode 解析）
pub enum RiskLevel { Low, Medium, High }

pub struct PermissionPolicy {
    mode: AtomicU8,                                          // 运行时 /mode 可切
    approver: Arc<dyn Fn(&str, &serde_json::Value) -> bool + Send + Sync>,
}

impl PermissionPolicy {
    pub fn new(mode: Mode, approver: impl Fn(&str, &serde_json::Value) -> bool + Send + Sync + 'static) -> Self;
    pub fn set_mode(&self, mode: Mode);
}

impl HookChain for PermissionPolicy {
    fn before_tool_call(&self, id, name, args) -> ToolCallVerdict;
    fn after_tool_call(&self, ...) -> /* passthrough */;
}
```

**工具 -> 风险映射**（按名，host 端维护）：`read`/`list`/`grep` = Low；`write`/`edit` = Medium；`bash` = High；未知 = High。

**决策表**：

| Mode \ Risk | Low | Medium | High (bash) |
|---|---|---|---|
| Suggest | Allow | Block("read-only mode") | Block |
| AutoEdit | Allow | Allow | `approver(name,args)` ? Allow : Block |
| FullAuto | Allow | Allow | Allow |

- `approver` 闭包是唯一的小抽象，但"确认路径"本身需要它：CLI 传读 stdin 的闭包，bot 传自己的。不是投机抽象。
- Block 时 core 已把原因作 error tool_result 回灌模型（既有机制），host 不额外处理。
- `approver` 在 `before_tool_call`（sync）里调；CLI 闭包同步读 stdin，单用户 REPL 下阻塞 runtime 可接受（loop 本就在等用户输入）。

### 4.4 `EventPrinter`（gasket-host/src/printer.rs）

消费 `AgentEvent`，渲染到注入的 writer（可测、可复用）。

```rust
pub struct EventPrinter<W: Write> { out: W }

impl<W: Write> EventPrinter<W> {
    pub fn new(out: W) -> Self;
    pub fn on_event(&mut self, ev: &AgentEvent);
}
```

| 事件 | 渲染 |
|---|---|
| `MessageUpdate(TextDelta)` | 流式打印 text（`colored` 高亮） |
| `ToolExecutionStart` | `-> {tool_name}` |
| `ToolExecutionEnd` | 结果首行/截断摘要 |
| `AfterProviderResponse` | `[in: X, out: Y]`（usage 观测，host 自己算，不进 core） |
| `TurnEnd` | 分隔行 |

- **注入 `Write`** 而非直接 stdout：测试用 `Vec<u8>` 捕获，bot 可换 writer。零成本抽象。

### 4.5 CLI（gasket-cli/src/main.rs）

REPL 循环（CLI-specific，不进 lib）。

**启动**：
1. `ConfigLoader::load()` -> `HostConfig`（失败则打印 "set GASKET_LLM_* in .env or env" 退出）。
2. `SessionManager::new()`；若 `--resume <id|last>` 参数则调 `resume`。
3. `PermissionPolicy::new(mode, stdin_approver)`（`--mode` 参数，默认 AutoEdit）。
4. `EventPrinter::new(io::stdout())`。
5. 组装 `AgentContext { system_prompt, messages: history, tools: built_in_tools(), cwd, session_id }` 与 `AgentLoopConfig { model, stream_fn(按 provider.api 建 OpenAiCompat/AnthropicProvider), hooks: Some(policy), retry, signal }`。

**REPL 循环**（reedline）：
- 读输入行。
- `/` 开头 -> slash 命令：
  - `/resume [id|last]` -> `SessionManager::resume`，history = 返回值。
  - `/clear` -> `SessionManager::clear()`，history 清空。
  - `/mode <suggest|auto-edit|full-auto>` -> `policy.set_mode(...)`。
  - `/sessions` -> 列出 `SessionManager::list()`。
  - `/help`、`/exit`。
- 否则 -> 构造 `UserMessage`：
  1. `run_agent_loop(vec![user_msg], context(含 history), config, |ev| printer.on_event(ev))`。
  2. `history.extend(&returned_new_messages)`。
  3. `SessionManager::append(&new_messages)`（增量持久化）。
  4. 下轮用 `history` 重建 `AgentContext`（loop 无状态，每轮消费 context）。

## 5. 数据流

**启动（一次）**：
```
env/.env --ConfigLoader::load()--> HostConfig
SessionManager::new() --或 resume--> session_id (+ history)
PermissionPolicy::new(mode, approver) --> HookChain
--> AgentLoopConfig{model, stream_fn, hooks:Some(policy), retry, signal}
--> AgentContext{system_prompt, messages:history, tools, cwd, session_id}
```

**每轮**：
```
reedline 读行
  /cmd --> 处理 slash（可能改 session/policy.mode），continue
  else --> UserMessage
           run_agent_loop([user_msg], context, config, |ev| printer.on_event(ev))
           history.extend(new_messages)
           SessionManager.append(new_messages)      // 增量
           下轮用 history 重建 context
```

**关键不变量**：
- loop 无状态纯函数；CLI 持 `history: Vec<AgentMessage>`、每轮重建 context。
- 持久化是增量 append；崩溃后重启 = `load_messages` 重建 history。
- 配置单向流入 loop（一次），loop 不读 env。

## 6. 错误处理

| 场景 | 处理 |
|---|---|
| 缺 LLM 配置 | 打印 "set GASKET_LLM_* in .env or env" 并退出（真实 host 不静默 mock；cli_host 示例保留作 mock 冒烟） |
| `/resume` id 不存在 | 报错 "no session {id}"，留在当前 session |
| 权限 Block | core 把原因作 error tool_result 回灌模型；REPL 显示 `-> bash (blocked: read-only mode)` |
| LLM 重试耗尽 | `stop_reason::Error` -> printer 显示错误，REPL 继续（等下一条） |
| 工具错误 | core 回灌；printer 显示摘要 |
| 单轮 IO/解析异常 | 捕获、打印、继续 REPL（不因一轮坏掉退出） |

## 7. 测试

- **ConfigLoader**：env 注入（复用 `from_env_with` 模式）断言 `HostConfig` 字段。
- **SessionManager**：tempdir 跑 `new`/`resume`/`append`/`list`/`resume_last`/`clear` 全往返。
- **PermissionPolicy**：表驱动 `(mode × risk) -> {Allow, Block, Confirm}`；approver stub 验证确认路径被调用且返回值生效；`set_mode` 运行时切换。
- **EventPrinter**：喂 canned `AgentEvent` 序列，断言输出含期望子串（`out: &mut Vec<u8>` 捕获）。
- **CLI**：reedline/TTY 难单测。抽 `Repl::run_once(input: &str)` 方法供集成测试 pipe stdin；完整 E2E 靠模块测试 + 手动冒烟。

## 8. 未来演进

- **bot 前端**：复用 `ConfigLoader`/`SessionManager`/`PermissionPolicy`/`EventPrinter`，写 bot 自己的循环（用 teloxide/serenity）。届时若两前端的循环结构收敛，再提取 `Frontend` trait（extract on second use）。
- **`approver` 异步化**：若 bot 需要异步审批（发消息等用户点按钮），把 `approver` 从 sync 闭包改为 async，或引入 `Approver` trait。CLI 的 sync stdin 路径先不改。
- **context compaction**：真撞 context window 时，host 在重建 context 前截断/摘要 history（host 层，不进 core loop）。
