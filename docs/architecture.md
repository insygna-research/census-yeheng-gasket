# gasket 架构设计

> 对应 workspace 版本 `2.0.0` · 仓库 [YeHeng/gasket](https://github.com/YeHeng/gasket) · MIT
>
> 本文面向想理解 gasket 内部结构、做二次开发或集成的工程师。若只想安装使用,请阅读 [使用文档](./usage.md)。

---

## 1. gasket 是什么

gasket 是一个**轻量级、可自托管的个人 AI 助手框架**(自述:*"A lightweight personal AI assistant framework"*)。它把"一个能调用工具、能流式输出、能管理会话与权限的 LLM agent"做成了分层可复用的 Rust 工作区,并配有一个 Vue 3 的 Web / 桌面前端。

关键词(workspace `Cargo.toml`):`ai / agent / chatbot / llm`。

### 设计哲学

gasket 的内核借鉴 "pi-style" 的可插拔 agent 设计,核心有三条原则,贯穿整个代码库:

| 原则 | 含义 | 体现在 |
|---|---|---|
| **loop 是无状态纯函数** | agent 推理循环不持有状态,状态全部由上层 host 持有 | `agent_loop` 只接收 `AgentContext` + `AgentLoopConfig`,返回新消息 |
| **provider 通过依赖注入接入** | 内核不知道"具体哪家 LLM",只认一个 `StreamFn` trait | `AgentLoopConfig.stream_fn` |
| **插件用进程内 Rust crate,而非动态加载** | 额外工具 / hook 由扩展 crate 在启动时 `register`,可选挂 Cargo feature | `gasket-ext` + `ExtensionApi` |

> 这三条决定了 gasket 的可测试性(注入 mock provider)、可复用性(同一套 host 同时驱动 CLI 和 Web 网关)和可扩展性(加工具不必改内核)。

---

## 2. 顶层架构与 Crate 分层

gasket 后端是一个 Cargo workspace(`gasket/Cargo.toml`),包含 5 个 crate,呈"内核 → 宿主 → 前端壳"的严格分层。

### 依赖关系图

```
                         ┌──────────────────────────────────────────┐
                         │            gasket-core (内核)             │
                         │  agent_loop · types · tools · providers   │
                         │  extension · storage                      │
                         └────────────────────┬─────────────────────┘
                                              │ 依赖
                ┌─────────────────────────────┴──────────────────────────┐
                ▼                                                          ▼
   ┌─────────────────────┐                                       ┌────────────────────┐
   │    gasket-host      │                                       │    gasket-ext      │
   │ config · session    │                                       │  hello · todo      │
   │ permission · compact│                                       │  search            │
   │ hooks · external    │                                       │  permission_gate   │
   └──────────┬──────────┘                                       └─────────┬──────────┘
              │                                                            │ (可选 feature)
   ┌──────────┴──────────────────────┐                                     │
   ▼                                 ▼                                     ▼
┌──────────────────┐         ┌──────────────────┐               ┌──────────────────┐
│  gasket-gateway  │         │   gasket-cli     │◄──────────────┘  ext feature
│ (bin, WS 网关)   │         │  (bin, REPL)     │
│ core + host      │         │ host + core      │
└──────────────────┘         └──────────────────┘
        │
        ▼  (WebSocket + REST + 静态托管 web/dist)
┌──────────────────────────────────────────────────────────────┐
│                web/  (Vue 3 + Vite + Tauri 2)                 │
│   浏览器应用  ──┐                                             │
│   Tauri 桌面壳 ─┴─ 同一份 src,经 ws://host:3000 连接 gateway │
└──────────────────────────────────────────────────────────────┘
```

### 各 crate 职责一览

| crate | 类型 | 职责 | 关键依赖 |
|---|---|---|---|
| **`gasket-core`** | lib(`gasket_core`) | 内核:agent loop、消息/事件/工具类型、内置工具、LLM provider、扩展 API、JSONL 存储。**无内部依赖** | reqwest、ignore、glob、regex、async-stream |
| **`gasket-host`** | lib | 可复用宿主层:配置加载、会话管理、权限策略、hook 组合、上下文压缩、外部工具桥接、事件渲染。把 loop 装进一个 `Host` 驱动器 | `gasket-core` |
| **`gasket-ext`** | lib | 可选的进程内扩展 crate(`hello`/`todo`/`search`/`permission_gate`),启动时经 `ExtensionApi` 注册工具与 hook | `gasket-core` |
| **`gasket-gateway`** | bin(`gasket-gateway`) | WebSocket 网关服务器,把 Vue 前端桥接到 agent loop,并提供 REST 上下文接口 + 托管前端静态资源 | `gasket-core` + `gasket-host`、axum |
| **`gasket-cli`** | bin(`gasket`) | 交互式终端 REPL agent,每行输入调一次 `run_turn`。带斜杠命令 | `gasket-host` + `gasket-core` + 可选 `gasket-ext`、reedline |

> **两个二进制要分清**:包名是 `gasket-cli`,但产出的二进制名是 **`gasket`**;另一个二进制是 **`gasket-gateway`**。

### 分层原则

- **`core` 是接缝最少的纯内核**:它不知道"谁来调用""配置从哪来""结果如何渲染"。这些全由 `host` 决定。
- **`host` 是可复用胶水**:同一套 `Host::run_turn` 既驱动 CLI 的终端打印,也驱动 gateway 的 WebSocket 推流——区别只在 `on_event` 回调的实现。
- **`gateway` / `cli` 是两种"前端壳"**:各自负责传输(WS / stdin)和呈现,共用 `host`。

---

## 3. 核心概念

| 概念 | 定义 | 代码位置 |
|---|---|---|
| **Session** | 一次连续对话,对应磁盘上 `~/.gasket/sessions/<id>.jsonl` 的一份 append-only 全量记录 | `host/src/session.rs`(`SessionManager`) |
| **Host** | 把 config/session/policy/hooks/stream_fn 组装在一起的驱动器;对外暴露 `run_turn` | `host/src/lib.rs:43` |
| **Agent Loop** | 无状态的推理循环:调 LLM → 解析响应 → 执行工具 → 把工具结果喂回 → 直到结束或超限 | `core/src/agent_loop.rs` |
| **Tool** | 一个带 JSON Schema 参数、风险等级、执行闭包的函数,LLM 可主动调用 | `core/src/types/tool.rs`(`ToolDefinition`) |
| **Hook** | 围绕每次工具调用的拦截器:`before_tool_call` 可 Allow/Block/Modify,`after_tool_call` 可改写结果(如脱敏) | `core/src/types/tool.rs`(`HookChain`) |
| **Provider** | 一个实现了 `StreamFn` 的 LLM 客户端;内核只认这个 trait,不认具体厂商 | `core/src/providers/mod.rs` |
| **Compaction** | 在喂给 LLM 之前**压缩工作内存**(只缩内存,不改盘),避免上下文溢出 | `host/src/compact.rs`(`ContextBudget`) |
| **Gateway** | 把单个 WebSocket 连接当作一个会话,在后台任务里跑 agent loop 并把事件流式回推 | `gasket-gateway/src/main.rs` |

---

## 4. 请求生命周期(数据流)

gasket 有两条入口路径,但都汇聚到同一个 `Host::run_turn` → `run_agent_loop`。

### 4.1 共同内核:`run_turn`

`Host::run_turn(user_msg, history, on_event)`(`host/src/lib.rs:142`)是整条流水线的枢纽:

```
run_turn(user_msg, history, on_event)
   │
   1. cfg.prepare_turn(TurnInputs{ system_prompt, history, tools, cwd, session_id },
                        signal, hooks, stream_fn, max_turns)
        └─ 组装出 (AgentContext, AgentLoopConfig)
   │
   2. run_agent_loop(vec![user_msg], context, config, on_event)
        └─ 无状态推理循环(见 4.3),on_event 实时回调每个 AgentEvent
   │
   3. 仅当成功: session.append(new_msgs)  ← 失败的 run 不写任何部分 transcript
   │
   └─ 返回这一轮新增的 AgentMessage 列表(调用方把它 extend 进自己的 history)
```

关键不变量:**`history` 是调用方拥有的 transcript**,`run_turn` 只把它 clone 进本次 context;**磁盘 JSONL 只在成功时追加**——失败不留下半截对话。

### 4.2 路径 A:CLI REPL

```
用户在终端敲一行
   │  Reedline 读行 (cli/src/main.rs:105)
   ▼
若是 / 开头 → 斜杠命令 (/mode /resume /clear /sessions /reload-tools)
否则构造 UserMessage
   │  压缩检查: budget.needs_compaction() ? budget.compact(&history) : history   (cli/src/main.rs:119)
   ▼
host.run_turn(user_msg, &history, |ev| {
     printer.on_event(&ev);                       ← 终端实时渲染
     从 AfterProviderResponse 提取 usage.input_tokens → budget.record_input_tokens(...)
})
   │
   ▼
history.extend(new_msgs)   ← 内存 transcript 更新;磁盘已是 append-only
```

### 4.3 内核循环:`run_agent_loop` 单轮结构

```
for turn in 0..max_turns {                       ← 外层循环,受 GASKET_MAX_TURNS 限制
    若 signal 被置位 → 协作式中止,返回已累积的 partial transcript

    stream = stream_fn.stream(model, messages, system_prompt, tools, signal)
              └─ 仅在"流出首个 chunk 之前"的失败才重试 (RetryPolicy);
                 流中途失败则直接上报,不重试(避免重复输出)
    consume(stream):                              ← 消费 StreamChunk 流
        TextDelta      → on_event(MessageUpdate) + 累积 assistant 文本
        ToolCallDelta  → 累积工具调用 (id/name/args)
        ThinkingDelta  → on_event(思考过程)
        Usage{in,out}  → 记录用量
        Done / Error   → 结束本turn

    若 stop_reason == ToolUse 且未超 max_tool_calls_per_turn:
        for each tool_call:
            verdict = hooks.before_tool_call(id, name, args, risk)   ← 异步,可能等人审批
                Allow | Block(reason) | Modify(new_args)
            match verdict:
                Allow/Modify → execute(tool) → after_tool_call(result) → 追加 ToolResult
                Block        → 追加带 reason 的 ToolResult(不执行)
        把所有 ToolResult 加入 messages,继续外层循环(再问 LLM)

    若 stop_reason == EndTurn → 跳出循环
}
on_event(AgentEnd);  返回本轮新增消息
```

### 4.4 路径 B:Gateway(WebSocket)

```
浏览器/桌面端 ──WS──► /ws?user_id=<chatId>
   │  每条 WS 连接 = 一个 session (gasket-gateway/src/ws.rs)
   ▼
收到 {"type":"message","content":"...","trace_id":"..."}
   │  spawn 后台 agent loop 任务
   ▼
进入 secondary select! 多路复用:
   ├─ agent 事件分支: AgentEvent → event_to_ws() → JSON → 推回 WS
   │     (thinking / tool_start / tool_end / content / error / done)
   └─ 入站消息分支: {"type":"cancel"} → 置 signal 中止
                    {"type":"approval_response",...} → 唤醒挂起的审批等待
```

需要人工审批时,gateway 会向客户端推 `approval_request`,然后用 `ApprovalRegistry`(oneshot + cancel + 超时三路等待)挂起,直到客户端回 `approval_response`、用户取消、或超时。

---

## 5. gasket-core 内核详解

内核导出见 `core/src/lib.rs`。

### 5.1 类型系统(`types/`)

| 类型 | 作用 | 文件 |
|---|---|---|
| `AgentMessage` | 枚举 `User` / `Assistant` / `ToolResult`,一条对话的最小单元 | `types/message.rs` |
| `ContentBlock` | 消息内容块:`Text` / `ToolCall` / 图片等 | `types/message.rs` |
| `AgentEvent` | 内核向外发出的事件流:`MessageUpdate`、`ToolExecutionStart/End`、`AfterProviderResponse`、`TurnStart`… | `types/event.rs` |
| `ContentDelta` | 增量:`TextDelta` / `ToolCallDelta` / …(事件载荷) | `types/event.rs` |
| `ToolDefinition` | 工具定义:`name`/`label`/`description`/`parameters`(JSON Schema)/`risk`/`execute` | `types/tool.rs:28` |
| `RiskLevel` | `Low` / `Medium` / `High`(默认 `High`) | `types/tool.rs:18` |
| `HookChain` | 拦截器 trait:`before_tool_call`(async,返 verdict)+ `after_tool_call`(sync) | `types/tool.rs:138` |
| `ToolCallVerdict` | `Allow` / `Block(reason)` / `Modify(args)` | `types/tool.rs:114` |
| `AgentContext` | 一次 run 的输入:system_prompt、messages、tools、cwd、env、session_id | `types/context.rs:14` |
| `AgentLoopConfig` | 一次 run 的配置:model、thinking、max_turns、max_tool_calls_per_turn、signal、**stream_fn**、hooks、retry | `types/context.rs:25` |
| `StreamFn` | **provider 接缝**:trait,`stream(model,messages,system,tools,signal) -> Stream<StreamChunk>` | `types/context.rs:207` |
| `StreamChunk` | provider 产出的事件:`TextDelta`/`ToolCallDelta`/`ThinkingDelta`/`Usage`/`Done`/`Error` | `types/context.rs:187` |
| `ModelSpec` / `ProviderApi` | 模型规格 + 协议族(`OpenAiCompat` / `Anthropic`) | `types/context.rs:159,168` |

> **为什么 `HookChain` 定义在 `types` 而不是 `extension`?** 这样 `AgentLoopConfig` 能持有 `Option<Arc<dyn HookChain>>` 而**不引入循环依赖**(concrete 实现是 `ExtensionApiImpl`)。

### 5.2 内置工具(`tools/`)

`built_in_tools()` 返回 6 个内置工具,均带风险分级:

| 工具 | 文件 | 用途 | 典型风险 |
|---|---|---|---|
| `read` | `tools/read.rs` | 读文件 | Low |
| `write` | `tools/write.rs` | 写文件 | High |
| `edit` | `tools/edit.rs` | 编辑文件 | High |
| `bash` | `tools/bash.rs` | 执行 shell | High |
| `grep` | `tools/grep.rs` | 正则搜索(基于 `ignore`,尊重 .gitignore) | Low |
| `list` | `tools/list.rs` | 列目录(基于 `ignore`+`glob`) | Low |

工具执行闭包签名(`ToolFn`):`Arc<dyn Fn(ToolCallCtx) -> Future<Output=Result<ToolResult,ToolError>>>`。`ToolContext.state_dir`(`~/.gasket/tool_state/<session>/<tool>/`)是每个工具的**私有**状态目录;`ToolCallCtx.aborted()` 用于长循环里协作式中止。

### 5.3 LLM Provider(`providers/`)

- **`ProviderConfig`**(`providers/mod.rs:26`):从环境读取连接配置。必填 `GASKET_LLM_BASE_URL` / `GASKET_LLM_KEY` / `GASKET_LLM_MODEL`;`GASKET_LLM_API` 选 `openai`(默认)或 `anthropic`。
- **两个实现**,都实现 `StreamFn`:
  - `OpenAiCompat`(`openai_compat.rs`):OpenAI 兼容协议——DeepSeek、智谱、xAI、Groq、Ollama、vLLM 等。
  - `AnthropicProvider`(`anthropic.rs`):Anthropic 原生 messages API。
- **`sse.rs`**:SSE 流解析,把 HTTP chunk 流切成 `StreamChunk`。
- **代理**:支持 `GASKET_LLM_PROXY`(http+https 通吃)/ `GASKET_LLM_HTTP_PROXY` / `GASKET_LLM_HTTPS_PROXY`,按 scheme 取优先级。

### 5.4 扩展 API(`extension/`)

`ExtensionApi` 是扩展注册口。核心区分:**事件**是纯观察(emit 闭包,不返值),**hook** 返回 verdict 控制流程——两者在类型层不可混淆(`extension/api.rs`)。`ExtensionApiImpl` 同时是工具容器和 `HookChain` 实现。

### 5.5 存储(`storage/`)

`JsonlStorage`:会话存为 JSONL(每行一条 `AgentMessage`)。亮点是 **torn-tail 自愈**——最后一行解析失败(进程崩溃截断)会自动丢弃并截断文件;中间行损坏才报错带行号,从而区分"崩溃产物"和"真实损坏"。

---

## 6. gasket-host 宿主层详解

宿主层把内核的"无状态循环"包装成一个有状态、可复用的驱动器,目录 `host/src/`。

### 6.1 `Host` 编排器(`lib.rs:43`)

`Host` 持有:配置 `HostConfig`、会话 `SessionManager`、权限策略 `Arc<PermissionPolicy>`、hook 链、协作中止信号 `Arc<AtomicBool>`、注入的 `stream_fn`、系统提示、工具列表、cwd、max_turns。

设计要点:

- **不持有 printer/writer**:渲染走 `run_turn` 的 `on_event` 回调,所以非终端前端(gateway)能驱动同一份代码。
- **`stream_fn` 默认取 provider 自身**;测试用 `with_stream_fn` 注入 fake。
- **`signal` 是共享中止旗**:每次 `Ctrl-C` 都被记录,`run_turn` 在下一轮重新清零。
- **hook 链可叠加**:`with_hooks` 让宿主在权限策略之上再压一层(如扩展的 pattern gate)。`Host::new` 默认 hook 链就是 `[policy]`。

### 6.2 各子模块

| 模块 | 职责 | 关键导出 |
|---|---|---|
| `config.rs` | 从 env 读取并组装 `HostConfig`(`ProviderConfig` + `AgentTunables` + system prompt + cwd),产出 `TurnInputs` | `ConfigLoader` / `HostConfig` / `TurnInputs` |
| `session.rs` | 会话 CRUD、列出、恢复(`resume`/`resume_last`)、append、clear;落盘 JSONL | `SessionManager` / `SessionInfo` |
| `permission.rs` | 权限策略:三档 `Mode` × 工具 `RiskLevel` 决策,内部持 approver 回调 | `Mode` / `PermissionPolicy` |
| `hooks.rs` | 把多个 `HookChain` 串成栈;`before` 取首个 Block / 末个 Modify,`after` 链式改写 | `HookStack` |
| `compact.rs` | 上下文压缩(见第 9 章) | `ContextBudget` / `compact_by_count` |
| `external_tool.rs` | 从 `GASKET_EXTERNAL_TOOLS` 白名单加载外部命令工具 | `ExternalToolBridge` / `commands_from_env` / `load_all` |
| `printer.rs` | 把 `AgentEvent` 渲染到终端(含 Error 分支与 flush) | `EventPrinter` |

### 6.3 `install_ctrl_c`(`lib.rs:174`)

安装一个 SIGINT 处理器,把共享 `signal` 置位(协作式中止)。在 cooked tty 模式下流式输出中的 `Ctrl-C` 会被它捕获;在 prompt 行(raw 模式)下 `Ctrl-C` 是 reedline 的按键事件,不触发这里。

---

## 7. gasket-gateway 网关详解

网关(`gasket-gateway/src/`)是前端与内核之间的桥,基于 axum。模块:`main`(路由/启动)、`state`(共享 `AppState`)、`ws`(WS 连接处理)、`wire`(协议类型)、`event_map`(AgentEvent→WS JSON)、`api`(REST)、`approval`(审批登记)。

### 7.1 启动与路由(`main.rs`)

- 初始化默认会话目录、加载 `.env`。
- 路由:

| 路由 | 方法 | 作用 |
|---|---|---|
| `/ws` | GET(升级 WS) | WebSocket 连接入口,每连接一会话 |
| `/api/commands` | GET | 斜杠命令列表(供前端补全) |
| `/api/sessions/{key}/context` | GET | 上下文统计(token 占用、压缩标志、水印) |
| `/api/sessions/{key}/context/compact` | POST | 手动触发压缩 |
| *(fallback)* | — | 托管 `web/dist` 静态资源,SPA 回退到 `index.html` |

- 端口 `GASKET_GATEWAY_PORT`(默认 **3000**),监听 `0.0.0.0`;静态目录 `GASKET_GATEWAY_STATIC_DIR`(默认 `../web/dist`);CORS 放开。

### 7.2 连接模型(每连接一会话)

每条 WS 连接就是一个独立会话。收到 `"message"` 时:在后台任务里跑 agent loop,主任务进入一个 **select! 多路复用**,同时处理"agent 事件 → 推 WS"和"入站消息(cancel / approval_response)"。

### 7.3 Wire 协议(前端 ↔ 网关)

**Client → Server**

```json
{ "type": "message", "content": "...", "trace_id": "..." }
{ "type": "cancel" }
{ "type": "approval_response", "request_id": "...", "approved": true, "remember": false }
```

**Server → Client**(每轮流式)

| `type` | 载荷 | 含义 |
|---|---|---|
| `thinking` | `content` | 思考过程增量 |
| `tool_start` | `name`,`arguments` | 工具开始执行 |
| `tool_end` | `name`,`output?`,`error?`,`tool_id?` | 工具结束/出错 |
| `content` | `content` | 助手文本增量 |
| `error` | `content?`,`message?` | 错误横幅 |
| `done` | — | 本轮结束 |
| `approval_request` | `id`,`tool_name`,`description`,`arguments` | 请求人工审批 |
| `subagent_*`(10 种) | — | ⏳ **M2 预留**:前端已有处理器,网关暂不发送 |

### 7.4 审批(`approval.rs`)

`ApprovalRegistry` 登记在途审批并维护 "remember" 缓存。`wait_for_decision` 用 **oneshot(用户决策)/ cancel(中止)/ 超时**三路 `select` 等待,避免闩锁毒化——`approval.rs` 内有专门的回归测试覆盖。

> **双通道取消**:协作中止用 `AtomicBool` 驱动 loop 退出,**同时**用 `watch` channel 解锁可能正挂起在审批上的等待,二者配合防止取消后闩锁泄漏。

---

## 8. 工具系统与权限模型

### 8.1 工具定义与风险

每个工具自带 `RiskLevel`(`Low`/`Medium`/`High`,默认 `High`,定义在 `ToolDefinition` 上)。这让 agent loop 能把风险转告 hook,**而不依赖一张硬编码的工具名表**(这正是 commit `336c8d3` "move tool risk to ToolDefinition, drop host risk_of table" 的意图)。

### 8.2 Hook 链与 Verdict

`HookChain` 在每次工具调用前后被咨询:

- **`before_tool_call`(异步)**:返回 `ToolCallVerdict`。组合规则:**首个 `Block` 获胜;否则末个 `Modify` 获胜;默认 `Allow`**。异步是为了支持"等人决策"(CLI 读 stdin / gateway WS 往返)。
- **`after_tool_call`(同步)**:纯变换,如脱敏/截断,可替换 `ToolResult`。

> **取消契约**:loop 挂在 `before_tool_call().await` 期间,abort 信号**不会**自动取消该 future;可能阻塞等人的实现必须自行检查信号或接受 cancel channel,置位时及时返回。

### 8.3 三档权限模式 × 三档风险

`Mode`(`host/src/permission.rs`):`Suggest` / `AutoEdit` / `FullAuto`,配合 approver 回调决定每个工具调用是自动放行、提示审批、还是直接阻断。默认值因入口而异:CLI 默认 `AutoEdit`(`--mode=` 可改),gateway 默认 `auto-edit`(`GASKET_GATEWAY_MODE`,见 ws.rs)。

典型决策矩阵(语义,具体以代码为准):

| | Risk=Low | Risk=Medium | Risk=High |
|---|---|---|---|
| **Suggest** | 提示审批 | 提示审批 | 提示审批 |
| **AutoEdit** | 自动放行 | 提示审批 | 提示审批 |
| **FullAuto** | 自动放行 | 自动放行 | 自动放行 |

审批入口:CLI 经 `stdin_approver`(`cli/src/main.rs:142`,stdin 读挪到 blocking 池避免卡 tokio worker);gateway 经 WS `approval_request`/`approval_response` 往返。

---

## 9. 上下文压缩(Compaction)

压缩是**纯宿主策略**(`host/src/compact.rs`),目的是在喂给 LLM 前缩小工作 transcript。三个硬约束:

1. **只缩内存,不改盘**——`~/.gasket/sessions/*.jsonl` 始终是 append-only 全量记录,压缩只作用于本次喂给 LLM 的 `history`。
2. **无 LLM 摘要**——不调用模型做总结,只做"丢弃最旧的若干组 + 前置一条 `[compacted N earlier messages]` 提示"。
3. **永不切断 tool_call ↔ result**——见 `atomic_groups`。

### 9.1 原子组(`atomic_groups`)

把消息切成 `[start, end)` 原子组:一条 `Assistant` 开一组,并把它**紧跟**的若干 `ToolResult` 吸收进同一组;其余消息各成单组。压缩以组为单位取舍,保证 `Assistant(tool_call)` 永不与其 `ToolResult` 分离(否则 LLM 会收到孤儿 tool_result 而报协议错误)。

### 9.2 两种触发模式

| 模式 | 触发 | 实现 |
|---|---|---|
| **Token 感知(主)** | provider 上报的 `usage.input_tokens` 超过 `window` 的 `threshold_pct`(默认 80%)时触发;压缩后留到 `target_pct`(默认 50%)——**带滞后**,避免在阈值附近反复压缩 | `ContextBudget`(`compact.rs:119`) |
| **条数兜底** | 当尚无 usage 数据(`last_input_tokens==0`)时,按消息条数 `GASKET_COMPACT_MAX_MESSAGES`(默认 80)压缩 | `compact_by_count`(`compact.rs:55`) |

`ContextBudget::compact` 在超阈值时,按比例 `kept_groups = total * target_tokens / last_input_tokens` 从最新端保留整组,前置一条提示;至少保留一组、至少丢弃一组。

### 9.3 数据来源

`usage.input_tokens` 由调用方从 `AgentEvent::AfterProviderResponse` 里提取,经 `ContextBudget::record_input_tokens` 喂入(CLI 在 `run_turn` 的 `on_event` 闭包里做,gateway 在 per-turn 处理里做)。这正是 commit `0ba96fc` 计划文档 "context-compaction" 落地的核心:用 provider 真实 usage 替代估算。

---

## 10. 前端架构(web/)

前端目录 `web/`,一套代码、两种形态:浏览器 Web 应用 + Tauri 桌面应用。

### 10.1 技术栈

| 维度 | 选型 |
|---|---|
| 框架 | Vue 3(Composition API + `<script setup lang="ts">`) |
| 构建 | Vite 7;生产 `vue-tsc -b && vite build`;别名 `@` → `./src` |
| 状态 | Pinia 3 |
| 样式 | Tailwind 3.4 + Less;shadcn-vue / radix-vue 模型,基于 HSL CSS 变量的 Token 体系 + 自定义 `th-*` 语义类 |
| UI 基件 | `radix-vue`(ScrollArea/Collapsible)、`@headlessui/vue`(菜单)、`lucide-vue-next`(图标)、`cn()` 类合并 |
| 内容渲染 | `marked` + `marked-highlight` + `highlight.js`(github-dark)+ `dompurify`(XSS 清洗)+ `mermaid`(图表,延迟加载) |
| 桌面运行时 | Tauri 2(`@tauri-apps/api` + `@tauri-apps/cli`) |
| 工具 | `@vueuse/core`;包管理标准化用 **pnpm** |

### 10.2 双形态:浏览器 + Tauri 桌面(同一份代码)

```
            web/src (同一份 Vue 代码)
               │
       ┌───────┴────────┐
       ▼                ▼
  Vite dev/build     Tauri 打包
  → dist/            → 桌面 App(.dmg/.msi/.exe)
       │                │
       └────────┬───────┘
                ▼
     都经 ws://<host>:3000 连接 gasket-gateway
```

**关键事实**:Tauri 桌面端是一个**轻量 WebView 壳**——`src-tauri/src/lib.rs` 里**没有定义任何 `#[tauri::command]`、没有 IPC、没有浏览器/桌面分支代码**。前端从不调用 Tauri 的 `invoke`。桌面端只是把同一份 Vite 产物装进一个窗口,仍像浏览器一样经 `localhost:3000` 的 WS/HTTP 与 gateway 通信。

> **部署含义**:桌面应用**不是一个纯本地离线应用**,运行时仍需要一个可达的 gasket-gateway 后端(可在本机或远端)。

### 10.3 项目结构(`web/src/`)

```
src/
├── main.ts            引导:createApp + Pinia + 样式
├── App.vue            根:可调宽/可折叠侧边栏 + 主聊天区
├── components/
│   ├── ui/            shadcn-vue 风格基件 (button/input/scroll-area/...)
│   ├── ChatArea.vue        单聊天顶层容器
│   ├── ChatHeader.vue      状态/上下文条/主题/压缩 按钮
│   ├── ChatInput.vue       输入框 + 斜杠命令补全 + 发送/停止
│   ├── MessageBubble.vue   消息渲染 (Markdown/mermaid/代码)
│   ├── MessageThoughtsPanel.vue  思考 + 工具调用时间轴
│   ├── ApprovalDialog.vue  工具审批模态框
│   ├── SubagentGridPanel.vue        子 agent 面板(M2 预留)
│   └── SubagentThoughtsPanel.vue
├── composables/
│   ├── useChatSession.ts        核心:WS 处理/消息流/REST 上下文/发送/审批/停止
│   ├── useTheme.ts              模块级单例主题状态
│   └── useResizableSidebar.ts   侧边栏拖拽,持久化 localStorage
├── hooks/useIMWebSocket.ts      底层 WS 封装(连/重连/发/关)
├── stores/chatStore.ts          Pinia:全部聊天/消息/工具调用/子 agent 状态
├── lib/utils.ts                 cn() 类合并
├── styles/                      Less 主题 + Tailwind;themes/(亮/暗、5 色相、12 种 Markdown 风格)
└── types/index.ts               全部 TS 接口
```

### 10.4 与后端的双通道通信

前端用**两条**通道与 gateway 通信,均由 env 驱动:`VITE_WS_URL`(默认 `ws://localhost:3000`)、`VITE_API_URL`(默认 `http://localhost:3000`)。

**WebSocket(主,流式)** — `hooks/useIMWebSocket.ts`:

- 连接 URL:`${VITE_WS_URL}/ws?user_id=${chatId}` —— **`chatId` 被复用为网关的 `user_id` 会话标识**。
- 重连:指数退避,最多 5 次,之后显示手动 "Reconnect" 按钮。
- 入站消息(`composables/useChatSession.ts` 的 switch):`thinking` / `tool_start` / `tool_end` / `content` / `error` / `done` / `approval_request` / `subagent_*`(M2,惰性)。
- 出站消息:`{type:'message',content,trace_id}`、`{type:'cancel'}`、`{type:'approval_response',request_id,approved,remember}`。

**REST(辅,上下文元数据)** — `composables/useChatSession.ts`:

| 端点 | 作用 |
|---|---|
| `GET /api/sessions/{key}/context` | 拉取 `context_stats`(token 预算/占用百分比/压缩标志)与 `watermark_info` |
| `POST /api/sessions/{key}/compact` | 强制压缩 |
| `GET /api/commands` | 斜杠命令补全列表 |

其中 `{key}` = `encodeURIComponent("websocket:" + chatId)`,即 WS user-id 加 `websocket:` 前缀。

### 10.5 状态管理:三层、无全局中央 store

| 层 | 载体 | 职责 | 持久化 |
|---|---|---|---|
| 持久聊天域 | Pinia `chatStore` | 所有聊天/消息/工具调用/子 agent CRUD | `localStorage['gasket_chats']`(含旧 `gasket_sessions` 迁移) |
| 瞬时会话 | `useChatSession`(每聊天一个) | 连接状态机(`disconnected\|idle\|sending\|receiving`)、审批队列、子 agent 跟踪、5 分钟超时兜底 | 不持久化 |
| 主题 | `useTheme`(**模块级单例**,非 Pinia) | 亮/暗、5 色相、12 种 Markdown 风格 | `localStorage['gasket_theme_v2']`(含旧 `gasket_theme` 迁移) |

> 主题用自定义 `th-*` 工具类(`th-app-bg`/`th-text`/`th-border`/`th-gradient-brand`...)代替原始 Tailwind 配色,整张调色板可经 CSS 变量 + `data-hue`/`data-md-style` 属性整体切换。

### 10.6 渲染策略:流式刻意降级

流式输出期间(`isReceiving`),消息**只渲染为转义纯文本**(`MessageBubble.vue`),避免每个 chunk 都跑一遍 `marked.parse + DOMPurify`。完整的 Markdown / Mermaid 渲染**只在流结束后**触发。这是用一次首屏流畅度换渲染开销的务实取舍。

### 10.7 预留特性:子 agent(M2)

前端已内置完整的 `subagent_*` 消息类型、store 字段与 switch 分支(`types/index.ts`、`useChatSession.ts`),并标注为 **"M2"**。当前 gateway **从不发送**这些消息——这是为未来 core 层子 agent 编排预留的前端契约,目前是惰性代码。

---

## 11. 扩展机制(gasket-ext)

`gasket-ext` 是可选的进程内扩展 crate,启动时经 `ExtensionApi` 注册工具与 hook:

- `register_all(&mut api)` 把 `hello` / `todo` / `search` / `permission_gate` 注册进去。
- CLI 通过 Cargo feature `ext`(`--features ext`)链接它;gateway 可类似接入。
- **事件 vs hook**:事件是纯观察(emit 闭包),hook 返回 verdict 控制流程,二者在类型层不可混淆(见 5.4)。
- 搜索扩展(`search.rs`)支持多家 provider:Brave / Tavily / Serper / SerpAPI / Exa / Firecrawl,由 `GASKET_SEARCH_PROVIDER` + 对应 `*_API_KEY` 选择。

> 想加自己的工具:实现一个返回 `Vec<ToolDefinition>`(+ 可选 `HookChain`)的注册函数,在宿主启动时调用;无需改内核。

---

## 12. 关键设计决策(Why)

| 决策 | 动机 |
|---|---|
| **`stream_fn` 依赖注入** | agent loop 与具体 LLM 彻底解耦,测试用 `MockStream` 注入 canned chunk 序列(`agent_loop.rs` 测试)即可,不必打真实网络 |
| **事件 vs hook 类型分离** | 观察与控流不可混淆:事件只 emit、hook 返 verdict。类型层强制,杜绝误用 |
| **持久化仅在成功时** | 失败的 run 不写部分 transcript,避免磁盘上出现"半截对话"污染下次上下文 |
| **JSONL torn-tail 自愈** | 进程崩溃常留下半行;按"末行坏=截断它、中行坏=报错"区分崩溃产物与真实损坏 |
| **审批双通道取消** | `AtomicBool` 驱动 loop 中止 + `watch` channel 解锁挂起审批,防止取消后闩锁毒化 |
| **前端壳不 IPC** | Tauri 桌面端不定义 command、不分支,整套前端就是一个 PWA 风格客户端经 WS/HTTP 连后端——一套代码、零分支成本 |
| **压缩只缩内存不改盘** | 全量 JSONL 永远是真相源;工作内存压缩有损但不破坏 protocol(原子组保护 tool_call↔result) |

---

## 13. 已知边界与预留

阅读本文时请注意以下**当前状态**,避免误判:

- **Dockerfile 已过时**:根 `Dockerfile` 引用的 crate 路径(`types/`、`storage/`、`engine/`...)是旧结构,`EXPOSE 18790` 也对不上 gateway 默认端口 3000。部署见 [使用文档 §Docker](./usage.md) 的说明。
- **版本号不统一**:workspace `2.0.0` vs `web/package.json` `0.0.0` vs `tauri.conf.json` `0.0.0`。以 workspace `2.0.0` 为准。
- **子 agent 协议为 M2 预留**:前端契约已就位,core/gateway 尚未发送 `subagent_*`。
- **无顶层 README / docs 索引**:`docs/superpowers/` 下均为内部设计/实施文档(中文,带日期/状态元信息),本文与 [使用文档](./usage.md) 是首批面向用户的文档。
- **Linux 桌面构建未在 CI**:Tauri 桌面产物 CI 仅覆盖 macOS(`.dmg`)与 Windows(`.msi`/`.exe`);Linux 桌面需本机具备 webkit2gtk 等系统依赖自行构建。
