# Harness Agent 升级(P0+P1+补充项)实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 conga 从"带工具栏的聊天 agent"升级为真正的编码 harness agent:真实系统提示 + 项目上下文 + git 视角、多 hunk 编辑 + diff 审批、持久 shell、vision 减法、中途 steer、子代理持久化 + 全文返回、todo 内置、压缩 pin、限流感知、工具名冲突检测、多模型路由、文档纠偏。

**Architecture:** 全部改动塞进现有接缝,不加新层:`TurnInputs.system_prompt`(每轮字符串)承载环境注入;`AgentLoopConfig` 增 `steer` 字段(共享队列);`persist` 回调接子日志;`gather_tools` 是冲突检测的唯一收口。零新增外部依赖(diff 用自带 LCS,日期用 civil-from-days,jitter 用系统纳秒)。

**Tech Stack:** Rust workspace(tokio/axum/serde),Vue 3 + TS 前端。

## Global Constraints

- 零新增 Cargo 依赖;零新增 npm 依赖。
- 事件日志 append-only 不变量不破坏:压缩/pin/截断只作用于 wire view。
- 每个任务:`cd conga && cargo test -p <crate>` 通过后才进下一任务;最后统一 `cargo fmt --all && cargo clippy --all-features -- -D warnings && cargo test --all-features`,前端 `pnpm build`。
- 工具 schema 是模型面契约(无 userspace),直接切换,不留兼容 shim。
- 系统提示、注释、commit message 英文;面向用户的说明中文。

---

### Task 1: Vision 减法 — 删除死的 Image 类型

**Files:**
- Modify: `conga/conga/src/types/message.rs:202-228`(删 `ContentBlock::Image` + `ImageContent`)
- Modify: `conga/conga-host/src/mcp.rs:28,238-243`(image → 文本占位)、`:873-877`(测试改断言)

**Interfaces:**
- Produces: `ContentBlock` 只剩 Text/ToolCall/Thinking。MCP image 内容映射为 `ContentBlock::text("[image content omitted: {mime_type}, {N} bytes]")` — 类型不再撒谎。

- [x] 删除变体与结构体,修 mcp.rs 映射与测试
- [x] `cargo test -p conga -p conga-host` 通过

### Task 2: RetryPolicy 限流感知 + jitter

**Files:**
- Modify: `conga/conga/src/types/context.rs:88-119`(RetryPolicy 增 `jitter: bool`)
- Modify: `conga/conga/src/agent_loop.rs:528-539,675-684`(backoff 判 429、加 jitter)
- Modify: `conga/conga/src/types/context.rs` AgentTunables::from_env 构造点

**Interfaces:**
- Produces: `fn backoff_ms(attempt, policy, rate_limited: bool) -> u64` — 429 时下限为 `max(initial*4, base)`,jitter 为 `±delay/4`(系统纳秒取模)。providers 的错误串已含 HTTP 状态(非 2xx 构造时带 code);loop 用 `error.contains("429")` 判定(自产字符串,有界)。

- [x] 单测:429 → 更长退避;jitter 有界且 ≥0;非 429 行为不变
- [x] `cargo test -p conga`

### Task 3: 中途 steer — 共享输入队列

**Files:**
- Create: `conga/conga/src/steer.rs`(`SteerQueue`:Arc<Mutex<VecDeque<String>>>,`push`/`drain`,Clone+Default)
- Modify: `conga/conga/src/types/context.rs`(AgentLoopConfig 增 `pub steer: Option<SteerQueue>`)
- Modify: `conga/conga/src/agent_loop.rs:50`(外层循环顶部 drain:每条 → User 消息 push 进 context+new_messages,`persist(SessionEvent::User)` 落盘)
- Modify: `conga/conga-host/src/config.rs:57-81`(build_loop_config 置 None)

**Interfaces:**
- Produces: `SteerQueue`;`Host::steer() -> SteerQueue`(Task 10 接)。注入消息走真实 User 事件 → 重启存活、derive_messages 天然还原。

- [x] 单测:turn 1 期间 push,turn 2 的 LLM 输入含该消息且 persist 收到 User 事件
- [x] `cargo test -p conga -p conga-host`

### Task 4: edit 多 hunk + 全有或全无

**Files:**
- Modify: `conga/conga-host/src/tools/edit.rs`(schema → `{path, edits: [{old_text, new_text}...]}`)

**Interfaces:**
- Consumes: 现有 `resolve_within_cwd`、fuzzy 机制、tmp+rename。
- Produces: 先对全部 hunk 定位(exact→fuzzy,各自唯一),任一失败 → 报错不改文件;全部命中 → 依序替换、原子写;result 列出每个 hunk 的 match 类型。

- [x] 重写测试:多 hunk 成功 / 第 N hunk 失败整体不动 / 混合 exact+fuzzy / 重复匹配报错
- [x] `cargo test -p conga-host edit`

### Task 5: bash 持久 shell + 后台执行

**Files:**
- Create: `conga/conga-host/src/tools/shell.rs`(持久 shell 注册表,unix)
- Modify: `conga/conga-host/src/tools/bash.rs`(unix 走持久 shell;Windows 保持一次性,平台分支与现状同构)

**Interfaces:**
- Produces: `PersistentShell::run(session_id, command, timeout, cwd, env, background_log_dir) -> ShellOutcome{output, exit_hint}`。实现:每 session 一个 `sh` 子进程(pipes,sentinel `__CONGA_D_<n>_$?` 协议),tokio::Mutex 串行化;超时杀 shell 重置(输出带 `[shell reset after timeout]`);`run_in_background` → 命令包 `> log 2>&1 &`,log 在 state_dir/bg/ 下(read 工具可达)。env_clear + 去 CONGA_* 前缀与现状一致;沙箱 confine 沿用。
- Produces: 工具 schema 增 `run_in_background: bool`(默认 false)。

- [x] 单测:cwd 跨调用持久(`cd` 后 pwd 变化)/ env 导出持久 / 超时重置 / 后台启动返回日志路径 / 输出走 spill
- [x] `cargo test -p conga-host bash shell`

### Task 6: todo 内置

**Files:**
- Create: `conga/conga-host/src/tools/todo.rs`(从 `conga/conga-ext/src/todo.rs` 迁移,`register` → `tool()`)
- Modify: `conga/conga-host/src/tools/mod.rs`(注册,9 个内置工具)
- Delete/Modify: `conga/conga-ext/src/todo.rs`、`conga/conga-ext/src/lib.rs`(从 register_all 移除,更新工具清单测试)

- [x] 单测沿用 todo 现有断言;built_in_tools 数量 = 9
- [x] `cargo test --all-features`(workspace)

### Task 7: 真实系统提示 + 项目上下文 + git 环境块

**Files:**
- Create: `conga/conga-host/src/prompt.rs`
- Modify: `conga/conga-host/src/assembly.rs:273`(静态部分)
- Modify: `conga/conga-host/src/lib.rs` run_turn(动态部分)

**Interfaces:**
- Produces:
  - `pub const CODING_AGENT_PROMPT: &str` — 工具纪律/验证义务/手术式修改/何时停。
  - `pub fn append_project_doc(base: &str, cwd: &Path) -> String` — cwd 向上(≤8 级)找 AGENTS.md → CLAUDE.md,首个命中者注入(≤16KB 截断标注),未命中静默。
  - `pub fn env_snapshot(cwd: &Path) -> String` — UTC 日期(civil-from-days,零依赖)+ `git status --porcelain`(≤64 行)+ `git diff --stat`(≤40 行),非 git 目录返回空串;子进程 3s 超时防挂。
- 静态链:`append_skills(append_project_doc(CODING_AGENT_PROMPT, cwd), cwd)`;run_turn 每轮拼 `<environment>` 块(快照为空则不拼)。

- [x] 单测:AGENTS.md 注入与截断 / 优先级 / 环境块格式 / 非 git 目录为空
- [x] `cargo test -p conga-host prompt`

### Task 8: 工具名冲突检测

**Files:**
- Modify: `conga/conga-host/src/assembly.rs:62-104`(gather_tools 收口去重)

**Interfaces:**
- Produces: 拼接后按 name 首见保留,重复项 `tracing::warn!` 列出被丢弃者(built-in > ext-prepend > external > MCP > append 的既有序即优先序)。

- [x] 单测:同名工具只留第一个并记 warn
- [x] `cargo test -p conga-host assembly`

### Task 9: 审批 diff 预览

**Files:**
- Create: `conga/conga-host/src/preview.rs`(LCS 行 diff,~60 行,零依赖)
- Modify: `conga/conga-host/src/wire.rs`(OutgoingEvent 增 `preview` 字段 + approval_request 构造带预览)
- Modify: `conga/conga-host/src/assembly.rs`(ApprovalEmit 4 参:增 `preview: Option<String>`;approver 闭包计算)
- Modify: `conga/conga-gateway/src/ws.rs`、`web/src-tauri/src/chat.rs`(转发链)
- Modify: `web/src/types/index.ts`、`web/src/composables/useChatSession.ts`、`web/src/components/ApprovalDialog.vue`(渲染 diff 块)

**Interfaces:**
- Produces: `pub fn approval_preview(tool: &str, args: &serde_json::Value, cwd: &Path) -> Option<String>` — `edit`:每 hunk old→new 渲染 `@@` diff;`write`:已有文件读出 diff(≤100 行截断),新文件显示头部;`bash`:None(命令已在 arguments)。WS `approval_request` 增 `preview?` 字段,前端优先渲染 preview。

- [x] 单测:edit/write 预览生成、bash None、非存在路径容错
- [x] `cargo test -p conga-host preview`;前端 `pnpm build`

### Task 10: 子代理持久化 + 全文结果 + Host::steer 接线

**Files:**
- Modify: `conga/conga-host/src/subagent_types.rs`(SubagentResult 增 `output: String`、`log_path: Option<String>`)
- Modify: `conga/conga-host/src/subagent.rs`(persist 接子日志;extract 全文)
- Modify: `conga/conga-host/src/tools/subagent.rs`(结果含全文 + log 路径,spill 截断)
- Modify: `conga/conga-host/src/assembly.rs`(spawner 增 parent session id + 子日志根)、`lib.rs`(Host 持 SteerQueue,`steer()` 访问器;prepare_turn 注入 config.steer)

**Interfaces:**
- Produces: 子日志落 `sessions/<parent>/sub/<uuid>/events.jsonl`(EventStorage::new(root.join(parent).join("sub")),persist 用 append_event_sync);不进会话列表。结果全文 = 全部 assistant 文本块拼接,工具层 spill_or_truncate。
- Produces: `Host::steer() -> conga::SteerQueue`。

- [x] 单测:spawn 后子日志文件存在且含 User/Assistant;result.output 非截断摘要
- [x] `cargo test -p conga-host`

### Task 11: 多模型路由(fast 模型给子代理)

**Files:**
- Modify: `conga/conga/src/providers/mod.rs`(ProviderConfig 增 `from_env_prefixed(prefix)` 泛化现有 from_env_with)
- Modify: `conga/conga-host/src/assembly.rs`(CONGA_FAST_LLM_* 齐全 → spawner 的 loop_config 换 model+stream_fn)
- Modify: `conga/.env.example`(新旋钮文档)

**Interfaces:**
- Produces: `ProviderConfig::from_env_prefixed("CONGA_FAST_LLM")` 读 `_BASE_URL/_KEY/_MODEL/_API`(+代理沿用主配置);缺任一 → None → 子代理继承父模型。

- [x] 单测:前缀读取 / 缺项 None
- [x] `cargo test -p conga -p conga-host`

### Task 12: 压缩 pin 首组 + 老工具结果截断

**Files:**
- Modify: `conga/conga-host/src/compact.rs`

**Interfaces:**
- Produces: `compact_by_count` 两段:① 超预算时,先对"最新 5 组之外"的 ToolResult 文本块截到头部 400 字符(wire view 改写,日志不动);② 仍超 → 丢最旧的非首组;首组(原始任务)永不丢,notice 改为 `[compacted N earlier messages; original task kept]`。

- [x] 单测:首组在任意预算下保留;老工具结果被截;新组不截
- [x] `cargo test -p conga-host compact`

### Task 13: 传输层 steer 接线(gateway + 桌面)

**Files:**
- Modify: `conga/conga-gateway/src/ws.rs:374-383`(turn 中 "message" → `host.steer().push(text)` + `WireEvent::Queued`)
- Modify: `conga/web/src-tauri/src/chat.rs:337-341`(同语义;turn_active 占用改为 steer)
- Modify: `conga/conga-host/src/wire.rs` + 前端三件套(`queued` 类型:气泡 + toast)

**Interfaces:**
- Consumes: `Host::steer()`(Task 10)。turn 外 message 路径不变(正常 run_turn)。
- Produces: WS 出站 `{"type":"queued","message":"..."}`;前端把该文本渲染为已入列用户消息。

- [x] gateway 集成测试:turn 中发 message → 不再 Busy,下一轮 LLM 调用包含该文本
- [x] `cargo test -p conga-gateway`;前端 `pnpm build`

### Task 14: 文档纠偏 + 新特性文档

**Files:**
- Modify: `docs/architecture.md`(tools 归属 conga-host、9 内置工具、持久 bash、steer 协议、环境注入、子代理持久化、fast 模型、冲突去重、vision 移除说明)
- Modify: `README.md`、`docs/usage.md`、`conga/.env.example`

- [x] 逐节核对与代码一致(工具清单、wire 协议表、env 表)

### Task 15: 终验

- [x] `cd conga && cargo fmt --all && cargo clippy --all-features -- -D warnings && cargo test --all-features`
- [x] `cd web && pnpm build`(vue-tsc 通过)
- [x] 冒烟:`cargo run --bin conga` REPL 起动,`/help` 正常(无 LLM key 环境下至少 CLI 装配不炸)
