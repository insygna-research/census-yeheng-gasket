# Gasket 重构方案：Pi-Agent 风格的可插拔 Agent Core

> 状态：**Draft for Review** · 作者：gasket maintainer + AI 协作
> 目的：把 gasket 从「60,737 行 / 10 crate 的过度架构」收敛到「~3,500 行 / 1 crate 的 pi 风格 agent core」
> 参考实现：https://github.com/earendil-works/pi-mono (pi-agent-core: 10,028 行 TS)

---

## 0. TL;DR

**目标**：做一个 Rust 版的 `pi-agent-core` —— 一个支持 plugin 可插拔的通用 agent loop，不做"个人助理产品"。

**核心哲学**（借鉴 pi）：
1. **Plugin = 普通函数**（Rust 用 cdylib），不是子进程 + JSON-RPC
2. **EventStream = 唯一输出渠道**，所有状态变更通过事件传播
3. **AgentMessage = 内部统一数据模型**，只在 LLM 边界转换成 Provider 协议
4. **~28 个单向事件 + 12 个 API（含 hook）** = 完整的扩展点
5. **没有 event sourcing、没有 vector embedding、没有内置 sandbox**（用户自己跑容器）

**结果**：60,737 行 → **~3,500 行（V0.1 目标）** / 短期可达 **~15,000 行（4 个 PR）**

---

## 1. 目标与非目标

### 1.1 目标 ✅

- 提供一个 **可嵌入** 的 agent core 库（`gasket = { ... }` 即可使用）
- 提供一个 **统一** 的 agent loop 实现（`agent_loop(messages, context, config, signal)`）
- 提供 **~28 个单向事件 + 12 个 API（含 hook）** 让 plugin 完全控制 agent 行为
- 提供 **cdylib 加载器**，让第三方能用 Rust 写 plugin
- 提供 **5 个内置 tool**：`read` / `write` / `edit` / `bash` / `list`
- 提供 **JSONL 存储**（messages + sessions），不引入 SQLite
- 提供 **OpenAI 兼容 + Anthropic** 两种 provider

### 1.2 非目标 ❌（明确不做）

- ❌ 不做 Telegram / Discord / Slack / 飞书 / 微信 channel 集成（plugin 做）
- ❌ 不做 CLI / TUI / Web UI（plugin 做）
- ❌ 不做 sandbox / permission system（用户自己用容器 / bwrap）
- ❌ 不做 vector embedding / RAG（用 `ripgrep` 文件搜索）
- ❌ 不做 multi-agent / subagent（plugin 可实现）
- ❌ 不做 workflow engine（plugin 可实现）
- ❌ 不做 MCP（plugin 可实现）
- ❌ 不做 event sourcing（用 JSONL append-only log）
- ❌ 不做 SQL migration 系统（schema 写在 `init()` 里）

### 1.3 推迟（V0.2+ 再考虑）

- ⏸ Vault / secrets 管理（用环境变量 + `.env` 文件）
- ⏸ Compaction（先做基础实现，V0.2 再做 branch summary）
- ⏸ Skills / prompts 模板（先 hardcode，V0.2 再做 loader）
- ⏸ Cost tracking（V0.1 只统计 token）

---

## 2. 核心架构

### 2.1 三层结构

```
┌─────────────────────────────────────────────────────┐
│  Host (CLI / TUI / Telegram Bot / Web Service)      │  ← 第三方 / 用户自己写
│  - 解析用户输入                                      │
│  - 选择 model + tools                               │
│  - 创建 Session                                     │
│  - 调 agent_loop()                                  │
│  - 消费 EventStream                                 │
│  - 渲染 / 持久化                                    │
└──────────────────┬──────────────────────────────────┘
                   │ 调
                   ▼
┌─────────────────────────────────────────────────────┐
│  Agent Core (gasket crate)                          │  ← 本仓库 V0.1 范围
│  ┌─────────────────────────────────────────────┐    │
│  │ agent_loop(messages, ctx, cfg) -> Stream    │    │
│  │   ├─ streamAssistantResponse()              │    │
│  │   ├─ executeToolCalls()                     │    │
│  │   │   ├─ before_tool_call event             │    │
│  │   │   ├─ Tool.execute()                     │    │
│  │   │   └─ after_tool_call event              │    │
│  │   └─ 触发 ~28 个单向事件 + 2 hook      │    │
│  └─────────────────────────────────────────────┘    │
│  ┌─────────────────────────────────────────────┐    │
│  │ ExtensionApi (extension/api.rs)             │    │
│  │   ├─ on(event, handler)                     │    │
│  │   ├─ register_tool(definition)              │    │
│  │   ├─ register_command(name, opts)           │    │
│  │   ├─ register_provider(spec)                │    │
│  │   ├─ register_before_tool_call(handler)     │    │
│  │   ├─ register_after_tool_call(handler)      │    │
│  │   ├─ send_message(msg)                      │    │
│  │   ├─ exec(cmd, args)                        │    │
│  │   └─ ... 12 个 API（含 hook）          │    │
│  └─────────────────────────────────────────────┘    │
│  ┌─────────────────────────────────────────────┐    │
│  │ Built-in tools (tools/)                     │    │
│  │   ├─ read / write / edit / bash / list      │    │
│  │   └─ 5 个工具 ~600 行                       │    │
│  └─────────────────────────────────────────────┘    │
│  ┌─────────────────────────────────────────────┐    │
│  │ Providers (providers/)                      │    │
│  │   ├─ openai_compat (OpenAI/DeepSeek/智谱)   │    │
│  │   └─ anthropic_messages                      │    │
│  └─────────────────────────────────────────────┘    │
│  ┌─────────────────────────────────────────────┐    │
│  │ Storage (storage/)                          │    │
│  │   ├─ jsonl.rs (messages.jsonl + sessions)   │    │
│  │   └─ index.rs (内存 HashMap<session_id, ...>)│    │
│  └─────────────────────────────────────────────┘    │
└──────────────────┬──────────────────────────────────┘
                   │ 加载
                   ▼
┌─────────────────────────────────────────────────────┐
│  Plugins (动态库 cdylib)                            │  ← 第三方 / 用户自己写
│  ~/.gasket/plugins/                                  │
│  - hello_tool.so                                     │
│  - todo_list.so                                      │
│  - permission_gate.so                                │
│  - custom_provider.so                                │
│  - telegram_bot.so                                   │
└─────────────────────────────────────────────────────┘
```

### 2.2 三个核心抽象

| 抽象 | 作用 | Rust 类型 |
|---|---|---|
| `AgentMessage` | 内部统一消息模型 | `enum AgentMessage` |
| `AgentEvent` | 状态变更的唯一信号 | `enum AgentEvent` |
| `ExtensionApi` | plugin 接触 agent 的唯一渠道 | `trait ExtensionApi` |

**核心约束**：
- Plugin **只能** 通过 `ExtensionApi` 与 agent 交互
- Plugin **不能** 直接访问 `AgentMessage` Vec（只能通过 `current_messages()` 读快照）
- Plugin **不能** 调 LLM（必须返回新消息，由 agent 下一轮送出去）
- 这三个约束让"写 plugin"等价于"写一个接收事件 + 注册能力的函数"

---

## 3. 数据模型（核心 5 个类型）

### 3.1 `AgentMessage`（内部统一消息）

```rust
// src/types/message.rs

pub enum AgentMessage {
    User(UserMessage),
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
    Custom(CustomMessage),  // plugin 私有消息，不发给 LLM
}

pub struct UserMessage {
    pub content: Vec<ContentBlock>,
    pub timestamp: u64,
}

pub struct AssistantMessage {
    pub content: Vec<ContentBlock>,
    pub model: ModelId,
    pub stop_reason: StopReason,  // EndTurn | ToolUse | MaxTokens | Error | Aborted
    pub usage: Option<Usage>,
    pub timestamp: u64,
}

pub struct ToolResultMessage {
    pub tool_call_id: String,
    pub tool_name: String,
    pub content: Vec<ContentBlock>,
    pub is_error: bool,
    pub timestamp: u64,
}

pub struct CustomMessage {
    pub custom_type: String,  // plugin 命名空间，如 "todo.list"
    pub content: serde_json::Value,
    pub timestamp: u64,
}

pub enum ContentBlock {
    Text(String),
    Image(ImageContent),
    ToolCall(ToolCall),
    Thinking(String),  // 模型推理内容
}
```

**关键设计**：
- `AssistantMessage.content` 同时包含 text / thinking / tool_calls
- `CustomMessage` 不发给 LLM（被 `convert_to_llm` 过滤掉）
- 所有消息带 `timestamp`（u64 millis），不依赖 chrono

### 3.2 `AgentEvent`（~28 个单向事件类型）

```rust
// src/types/event.rs

pub enum AgentEvent {
    // 生命周期
    AgentStart,
    AgentEnd { messages: Vec<AgentMessage> },
    TurnStart,
    TurnEnd { message: AssistantMessage, tool_results: Vec<ToolResultMessage> },

    // 消息
    MessageStart { message: AgentMessage },
    MessageUpdate { message: AgentMessage, delta: ContentDelta },
    MessageEnd { message: AgentMessage },

    // 工具调用
    ToolExecutionStart { tool_call_id: String, tool_name: String, args: serde_json::Value },
    ToolExecutionEnd { tool_call_id: String, result: ToolResultMessage, is_error: bool },

    // LLM 调用
    BeforeProviderRequest { model: ModelId, messages: Vec<LlmMessage> },
    AfterProviderResponse { model: ModelId, response: AssistantMessage },

    // Session
    SessionStart { session_id: String, cwd: PathBuf },
    SessionCompact { summary: String, before: usize, after: usize },
    SessionEnd { session_id: String },

    // 模型 / thinking
    ModelSelect { model: ModelId },
    ThinkingLevelSelect { level: ThinkingLevel },

    // 错误
    Error { source: ErrorSource, message: String },
}

pub enum ContentDelta {
    TextDelta(String),
    ThinkingDelta(String),
    ToolCallDelta { id: String, name: Option<String>, args_delta: String },
}
```

**事件设计的核心约束**：
- **所有 `AgentEvent` 变体都是单向通知**，没有任何可拦截的变体
- **可拦截的 hook（改 result / block）是独立的 API**：`register_before_tool_call` / `register_after_tool_call`（见 §3.5），它们不通过事件系统，直接走 handler 链
- 这避免了"事件和拦截器混淆"的常见设计错误——`AgentEvent` 永远不携带需要 agent 响应的语义

### 3.3 `AgentContext`（agent 看到的全部信息）

```rust
// src/types/context.rs

pub struct AgentContext {
    pub system_prompt: String,
    pub messages: Vec<AgentMessage>,
    pub tools: Vec<ToolDefinition>,
    pub cwd: PathBuf,
    pub env: HashMap<String, String>,
    pub session_id: String,
}

pub struct AgentLoopConfig {
    pub model: ModelSpec,
    pub thinking_level: ThinkingLevel,  // Off | Low | Medium | High
    pub max_turns: usize,                // 默认 50
    pub max_tool_calls_per_turn: usize,   // 默认 20
    pub api_key: Option<String>,
    pub signal: Option<Arc<AtomicBool>>,  // abort 信号
    pub stream_fn: Arc<dyn StreamFn>,      // LLM 调用入口
}
```

**关键设计**：
- **没有 plugin 共享状态字段**（V0.1 刻意删掉）——plugin 私有状态写到 `~/.gasket/tool_state/{plugin}/`（见 §6.1），不进 agent core。详见 §13「不做什么」中的「plugin 共享公告板」一条。
- `stream_fn` 是 trait object，host 可以注入自定义实现（mock / 缓存 / 限流）

### 3.4 `ToolDefinition`（plugin 注册的 tool）

```rust
// src/types/tool.rs

pub struct ToolDefinition {
    pub name: String,
    pub label: String,
    pub description: String,
    pub parameters: serde_json::Value,  // JSON Schema
    pub execute: ToolFn,
}

pub type ToolFn = Arc<
    dyn Fn(
        tool_call_id: String,
        args: serde_json::Value,
        signal: Arc<AtomicBool>,
        ctx: ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<ToolResult, ToolError>> + Send>>
        + Send
        + Sync,
>;

pub struct ToolContext {
    pub cwd: PathBuf,
    pub env: HashMap<String, String>,
    pub session_id: String,
    pub state_dir: PathBuf,  // 本 plugin 私有状态目录：~/.gasket/tool_state/{plugin}/
}

pub struct ToolResult {
    pub content: Vec<ContentBlock>,
    pub details: serde_json::Value,  // plugin 私有数据，agent 不读
    pub is_error: bool,
}
```

**关键设计**：
- **没有 `on_update` 流式回调**（V0.1 删除）——5 个内置 tool 没有一个需要流式进度，没有消费者的接口就是死接口。V0.2 若真有长任务需求再加 `ToolExecutionUpdate` 事件 + 回调。
- `state_dir` 是本 plugin **独占**的私有状态目录，plugin 在这里读写自己的 JSON/JSONL 文件（如 `todo_list` 的 todos）。跨 plugin 的状态共享不是 agent core 的职责。
- `details` 是 plugin 私有数据，agent 不会读，但 plugin 自己可以用
- `parameters` 直接是 JSON Schema，不引入 jsonschema crate 验证（host 调用时验证）

### 3.5 `ExtensionApi`（plugin 接触 agent 的唯一渠道）

```rust
// src/extension/api.rs

pub trait ExtensionApi: Send + Sync {
    // ===== 事件订阅（纯单向通知，handler 不能改变 agent 行为）=====
    fn on(&mut self, event: EventKind, handler: Box<dyn EventHandler>);

    // 简化的强类型版本（推荐用这个）
    fn on_event<F>(&mut self, event: &'static str, handler: F)
    where
        F: Fn(&AgentEvent, &ExtensionContext) -> BoxFuture<'_, ()> + Send + Sync + 'static;

    // ===== 工具注册 =====
    fn register_tool(&mut self, tool: ToolDefinition);

    // ===== 可拦截的 hook（独立于事件系统，handler 返回值控制 agent 行为）=====
    fn register_before_tool_call(&mut self, handler: Box<dyn BeforeToolCallHandler>);
    fn register_after_tool_call(&mut self, handler: Box<dyn AfterToolCallHandler>);

    // ===== 命令注册（host 决定怎么用：CLI /command，Telegram /command）=====
    fn register_command(&mut self, name: &str, opts: CommandOptions);

    // ===== Provider 注册 =====
    fn register_provider(&mut self, name: &str, spec: ProviderSpec);
    fn unregister_provider(&mut self, name: &str);

    // ===== Action =====
    fn send_message(&mut self, msg: AgentMessage);                       // 发到当前 session
    fn exec(&self, command: &str, args: &[&str]) -> Result<ExecOutput>;  // 跑 shell
    fn current_messages(&self) -> &[AgentMessage];                       // 读 session 消息快照

    // ===== Session 元数据 =====
    fn set_session_name(&mut self, name: String);
    fn get_session_name(&self) -> Option<String>;
    fn api_version(&self) -> &'static str;  // ABI 版本，见 §5.1
}

// hook handler 返回值：决定 agent 对该 tool call 的处理方式
pub enum ToolCallVerdict {
    Allow,
    Block(String),        // 拒绝执行，reason 作为 ToolResult 返回给 LLM
    Modify(serde_json::Value),  // 改写 args 后再执行
}

pub trait BeforeToolCallHandler: Send + Sync {
    fn call(&self, tool_call_id: &str, tool_name: &str, args: &serde_json::Value, ctx: &ExtensionContext)
        -> ToolCallVerdict;
}

pub trait AfterToolCallHandler: Send + Sync {
    // 可改写 result（用于敏感信息脱敏、压缩日志等）
    fn call(&self, tool_call_id: &str, result: &ToolResultMessage, ctx: &ExtensionContext)
        -> Option<ToolResultMessage>;  // None=不改，Some=替换
}

pub struct ExtensionContext {
    pub session_id: String,
    pub cwd: PathBuf,
    pub signal: Arc<AtomicBool>,
}
```

**关键设计**：
- **事件与 hook 彻底分离**：`on_event` 订阅的 handler 只能观察、不能改变行为（返回 `()`）；`register_before/after_tool_call` 的 handler 通过 `ToolCallVerdict` / `Option<ToolResult>` 控制流。两者类型上就不可能混淆。
- **没有 `register_renderer`**（V0.1 删除）——消息渲染是 host 的职责（CLI / TUI / Telegram 各自渲染），不该进 core。
- **没有 plugin 共享状态 API**（删掉 `metadata()`/`metadata_mut()`）——plugin 私有状态走 `ToolContext.state_dir` 文件，跨 plugin 共享不进 core。详见 §13。
- **没有 `send_user_message`**——强制触发下一轮是 host 的控制权，host 直接再调一次 `agent_loop()` 即可，不该暴露给 plugin。
- 所有方法都是 `&mut self` 或 `&self`，**没有 `&mut AgentSession` 这种泄露内部状态的 API**
- `exec` 是 blocking shell 执行（plugin 想异步自己包 `tokio::spawn`）

---

## 4. Agent Loop（核心 500 行）

### 4.1 公开 API

```rust
// src/agent_loop.rs

pub fn agent_loop(
    initial_prompts: Vec<AgentMessage>,
    context: AgentContext,
    config: AgentLoopConfig,
) -> EventStream {
    EventStream::new(move |emit| async move {
        run_agent_loop(initial_prompts, context, config, emit).await
    })
}

pub async fn run_agent_loop(
    initial_prompts: Vec<AgentMessage>,
    mut context: AgentContext,
    mut config: AgentLoopConfig,
    emit: impl Fn(AgentEvent) + Send + 'static,
) -> Result<Vec<AgentMessage>, AgentError> {
    // ... 500 行实现
}
```

### 4.2 核心循环伪代码

```rust
async fn run_agent_loop(
    initial_prompts: Vec<AgentMessage>,
    mut context: AgentContext,
    mut config: AgentLoopConfig,
    emit: impl Fn(AgentEvent),
) -> Result<Vec<AgentMessage>, AgentError> {
    let mut new_messages = initial_prompts.clone();
    context.messages.extend(initial_prompts);

    emit(AgentEvent::AgentStart);
    for msg in &new_messages {
        emit(AgentEvent::MessageStart { message: msg.clone() });
        emit(AgentEvent::MessageEnd { message: msg.clone() });
    }

    // 单一外层循环
    'outer: for turn in 0..config.max_turns {
        emit(AgentEvent::TurnStart);

        // 1. 调 LLM
        let assistant = stream_assistant_response(
            &context, &config, &emit
        ).await?;

        new_messages.push(AgentMessage::Assistant(assistant.clone()));
        context.messages.push(AgentMessage::Assistant(assistant.clone()));

        // 2. 检查终止
        match assistant.stop_reason {
            StopReason::EndTurn | StopReason::Error(_) | StopReason::Aborted => {
                emit(AgentEvent::TurnEnd {
                    message: assistant,
                    tool_results: vec![],
                });
                break 'outer;
            }
            StopReason::MaxTokens => {
                // 输出被截断，所有 tool_call 都标记为错误
                let error_results = fail_all_tool_calls(&assistant);
                new_messages.extend(error_results.iter().cloned().map(AgentMessage::ToolResult));
                context.messages.extend(error_results.iter().cloned().map(AgentMessage::ToolResult));
                emit(AgentEvent::TurnEnd { message: assistant, tool_results: error_results });
                continue;
            }
            StopReason::ToolUse => {} // 继续
        }

        // 3. 执行 tool calls（串行或并行，agent 决定）
        let tool_results = execute_tool_calls(
            &context, &assistant, &config, &emit
        ).await?;

        for result in &tool_results {
            new_messages.push(AgentMessage::ToolResult(result.clone()));
            context.messages.push(AgentMessage::ToolResult(result.clone()));
        }

        emit(AgentEvent::TurnEnd { message: assistant, tool_results });

        // 4. 检查 host 是否要提前终止
        if config.signal.as_ref().is_some_and(|s| s.load(Ordering::Relaxed)) {
            break 'outer;
        }
    }

    emit(AgentEvent::AgentEnd { messages: new_messages.clone() });
    Ok(new_messages)
}
```

### 4.3 工具执行（带 hook 拦截）

```rust
async fn execute_tool_calls(
    context: &AgentContext,
    assistant: &AssistantMessage,
    config: &AgentLoopConfig,
    emit: &impl Fn(AgentEvent),
) -> Result<Vec<ToolResultMessage>, AgentError> {
    let tool_calls: Vec<_> = assistant.content.iter()
        .filter_map(|b| if let ContentBlock::ToolCall(tc) = b { Some(tc.clone()) } else { None })
        .collect();

    let mut results = Vec::new();
    for tc in tool_calls {
        let mut args: serde_json::Value = serde_json::from_str(&tc.function.arguments)?;

        // 1. before_tool_call hook（独立于事件系统，handler 返回 verdict 控制流）
        match call_before_tool_call_handlers(&tc.id, &tc.function.name, &args, context) {
            ToolCallVerdict::Block(reason) => {
                let result = ToolResultMessage {
                    tool_call_id: tc.id.clone(),
                    tool_name: tc.function.name.clone(),
                    content: vec![ContentBlock::Text(reason)],
                    is_error: true,
                    timestamp: now(),
                };
                emit(AgentEvent::ToolExecutionEnd {
                    tool_call_id: tc.id.clone(), result: result.clone(), is_error: true,
                });
                results.push(result);
                continue;
            }
            ToolCallVerdict::Modify(new_args) => args = new_args,
            ToolCallVerdict::Allow => {}
        }

        // 2. 实际执行
        emit(AgentEvent::ToolExecutionStart {
            tool_call_id: tc.id.clone(),
            tool_name: tc.function.name.clone(),
            args: args.clone(),
        });

        let tool = context.tools.iter()
            .find(|t| t.name == tc.function.name)
            .ok_or_else(|| AgentError::ToolNotFound(tc.function.name.clone()))?;

        let raw_result = (tool.execute)(
            tc.id.clone(),
            args,
            config.signal.clone().unwrap(),
            ToolContext::from(context),
        ).await?;

        let mut result = ToolResultMessage {
            tool_call_id: tc.id.clone(),
            tool_name: tc.function.name.clone(),
            content: raw_result.content.clone(),
            is_error: raw_result.is_error,
            timestamp: now(),
        };

        // 3. after_tool_call hook（独立于事件系统，handler 可替换 result）
        if let Some(replaced) = call_after_tool_call_handlers(&tc.id, &result, context) {
            result = replaced;
        }

        emit(AgentEvent::ToolExecutionEnd {
            tool_call_id: tc.id.clone(),
            result: result.clone(),
            is_error: result.is_error,
        });

        results.push(result);
    }

    Ok(results)
}
```

**关键设计决策**：
- **顺序执行**（V0.1），不实现并行。pi 实现了并行但有 489 行的并行路径，V0.1 不需要
- **拦截走 hook handler 链，不走事件**：`call_before_tool_call_handlers` 返回 `ToolCallVerdict`（Block/Modify/Allow），`call_after_tool_call_handlers` 返回 `Option<ToolResultMessage>`。事件（`ToolExecutionStart`/`ToolExecutionEnd`）只负责通知，不携带任何需要 agent 响应的语义。
- **`after_tool_call` 可以替换 result** —— 用于敏感信息脱敏、压缩日志等

### 4.4 LLM 边界转换

```rust
async fn stream_assistant_response(
    context: &AgentContext,
    config: &AgentLoopConfig,
    emit: &impl Fn(AgentEvent),
) -> Result<AssistantMessage, AgentError> {
    // 1. AgentMessage[] → LlmMessage[]（provider 协议）
    let llm_messages = convert_to_llm(&context.messages, &config.model.api)?;

    // 2. 调 LLM
    emit(AgentEvent::BeforeProviderRequest {
        model: config.model.id.clone(),
        messages: llm_messages.clone(),
    });

    let response = (config.stream_fn)(
        &config.model,
        &llm_messages,
        &config.system_prompt,
        &config.tools,
        config.signal.clone(),
    ).await?;

    // 3. 流式处理
    let mut accumulated = AssistantMessage::new(&config.model);
    for await chunk in response {
        match chunk {
            StreamChunk::TextDelta(t) => {
                accumulated.append_text(&t);
                emit(AgentEvent::MessageUpdate {
                    message: AgentMessage::Assistant(accumulated.clone()),
                    delta: ContentDelta::TextDelta(t),
                });
            }
            StreamChunk::ToolCallDelta { id, name, args_delta } => {
                accumulated.append_tool_call(id, name, args_delta);
                emit(AgentEvent::MessageUpdate {
                    message: AgentMessage::Assistant(accumulated.clone()),
                    delta: ContentDelta::ToolCallDelta { id, name, args_delta },
                });
            }
            StreamChunk::ThinkingDelta(t) => {
                accumulated.append_thinking(&t);
                emit(AgentEvent::MessageUpdate {
                    message: AgentMessage::Assistant(accumulated.clone()),
                    delta: ContentDelta::ThinkingDelta(t),
                });
            }
            StreamChunk::Done => break,
            StreamChunk::Error(e) => {
                accumulated.stop_reason = StopReason::Error(e);
                break;
            }
        }
    }

    emit(AgentEvent::AfterProviderResponse {
        model: config.model.id.clone(),
        response: accumulated.clone(),
    });

    Ok(accumulated)
}
```

---

## 5. Plugin 系统

### 5.1 Plugin 加载机制

```rust
// src/extension/loader.rs

use libloading::Library;
use std::path::Path;

pub struct Plugin {
    pub name: String,
    pub path: PathBuf,
    pub manifest: PluginManifest,
    _lib: Library,  // 持有防止卸载
}

pub struct PluginManifest {
    pub name: String,
    pub version: String,
    pub gasket_abi_version: u32,  // 独立于语义版本的 ABI 版本，见下方说明
    pub description: String,
}

// host 侧常量：每次破坏 ABI 的改动（结构体布局/enum 判别式/trait vtable）手动 +1
pub const GASKET_ABI_VERSION: u32 = 1;

pub fn load_plugin(path: &Path) -> Result<Plugin, PluginError> {
    // 1. 读 manifest.toml（紧邻 .so 文件）
    let manifest_path = path.with_extension("toml");
    let manifest = PluginManifest::from_file(&manifest_path)?;

    // 2. 检查 ABI 版本（独立于语义版本，见下方"诚实声明"）
    if manifest.gasket_abi_version != GASKET_ABI_VERSION {
        return Err(PluginError::IncompatibleAbi {
            plugin: manifest.gasket_abi_version,
            host: GASKET_ABI_VERSION,
        });
    }

    // 3. 加载 cdylib
    let lib = unsafe { Library::new(path) }?;

    // 4. 找 register 符号
    let register: Symbol<extern "C" fn(&mut dyn ExtensionApi)> = unsafe { lib.get(b"register") }?;

    // 5. 调 register（plugin 内部会调 api.register_tool / api.register_before_tool_call）
    let mut api = ExtensionApiImpl::new();
    register(&mut api);

    Ok(Plugin { name: manifest.name.clone(), path: path.to_path_buf(), manifest, _lib: lib })
}

pub fn discover_plugins(dir: &Path) -> Vec<PathBuf> {
    if !dir.exists() { return vec![]; }
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map_or(false, |x| x == "so" || x == "dylib" || x == "dll"))
        .map(|e| e.path())
        .collect()
}
```

**关键设计**：
- **每个 plugin = 一个 `manifest.toml` + 一个 `libhello.so`**
- **plugin 只能导出一个 `extern "C" fn register(&mut dyn ExtensionApi)`**
- **没有 plugin 协议、plugin 序列化、plugin 反射**
- **`libloading` 持有 `_lib` 防止 plugin 被卸载**

#### 5.1.1 cdylib ABI 诚实声明（重要）

cdylib 在 Rust 里**没有稳定的 ABI**。plugin 与 host 必须满足：
1. **相同的 Rust toolchain**（同 stable 版本，或都 pin 同一个 nightly）；
2. **相同的依赖大版本**（tokio / reqwest / serde 的 major 必须一致，否则 vtable / trait object 布局对不上）；
3. **随 host 重新编译**——host 升级后，所有 plugin 必须重新 `cargo build`。

因此 **`GASKET_ABI_VERSION` 与 crate 语义版本（`CARGO_PKG_VERSION`）解耦**：
- 语义版本是给人看的（0.14.0 → 0.15.0 表示 API 行为变化）；
- ABI 版本是给加载器看的——只有当**二进制布局不变**时才能保持不变。`#[repr(Rust)]` 结构体改字段顺序、enum 加变体、trait 加方法，都要手动 `GASKET_ABI_VERSION += 1`。

**这意味着**：plugin 生态是「**host 仓库的子目录**」模式，而非「独立分发的 npm 包」模式。第三方 plugin 必须 clone 本仓库、在固定 toolchain 下编译，不能跨版本/跨 toolchain 复用预编译产物。**这是 cdylib 方案的固有代价，V0.1 接受它，不做掩盖。**

如果未来要求「任意语言 / 跨版本 plugin」，正确的路线是放弃 cdylib 回到子进程 + JSON-RPC（即当前 `external_tools` 的方案，它存在是有原因的）——那是 V0.2+ 的独立决策，不在本方案范围。

### 5.2 Plugin 目录结构

```
~/.gasket/plugins/
├── hello/
│   ├── manifest.toml         # name = "hello", version = "0.1.0", gasket_abi_version = 1
│   ├── Cargo.toml            # [lib] crate-type = ["cdylib"]
│   └── target/release/libhello.so
├── todo_list/
│   ├── manifest.toml
│   └── target/release/libtodo_list.so
├── permission_gate/
│   ├── manifest.toml
│   └── target/release/libpermission_gate.so
└── ...
```

### 5.3 完整 Plugin 示例（4 个）

#### 示例 1：Hello Tool（最简）

```rust
// ~/.gasket/plugins/hello/src/lib.rs
use gasket::extension::{ExtensionApi, ToolDefinition, ContentBlock, ToolResult};
use gasket::types::serde_json::json;

#[no_mangle]
pub extern "C" fn register(api: &mut dyn ExtensionApi) {
    api.register_tool(ToolDefinition {
        name: "hello".into(),
        label: "Hello".into(),
        description: "Say hello to someone".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Name to greet" }
            },
            "required": ["name"]
        }),
        execute: std::sync::Arc::new(|_id, args, _signal, _ctx| {
            Box::pin(async move {
                let name = args["name"].as_str().unwrap_or("world");
                Ok(ToolResult {
                    content: vec![ContentBlock::Text(format!("Hello, {}!", name))],
                    details: json!({ "greeted": name }),
                    is_error: false,
                })
            })
        }),
    });
}
```

#### 示例 2：Todo List（plugin 私有状态 + 文件存储）

```rust
// ~/.gasket/plugins/todo_list/src/lib.rs
use gasket::extension::{ExtensionApi, ContentBlock, ToolContext, ToolResult};
use gasket::types::serde_json::json;
use std::sync::Arc;

#[derive(Default, serde::Serialize, serde::Deserialize, Clone)]
struct Todo { id: u64, text: String, done: bool }
#[derive(Default, serde::Serialize, serde::Deserialize, Clone)]
struct State { todos: Vec<Todo>, next_id: u64 }

// plugin 状态读写自己的文件，不进 agent core
fn load(ctx: &ToolContext) -> State {
    let p = ctx.state_dir.join("state.json");
    std::fs::read(&p).ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}
fn save(ctx: &ToolContext, s: &State) {
    let p = ctx.state_dir.join("state.json");
    let _ = std::fs::create_dir_all(&ctx.state_dir);
    let _ = std::fs::write(&p, serde_json::to_vec(s).unwrap());
}

#[no_mangle]
pub extern "C" fn register(api: &mut dyn ExtensionApi) {
    api.register_tool(ToolDefinition {
        name: "todo".into(),
        label: "Todo".into(),
        description: "Manage a todo list: add / list / toggle / clear".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["add", "list", "toggle", "clear"] },
                "text": { "type": "string" },
                "id": { "type": "integer" }
            },
            "required": ["action"]
        }),
        execute: Arc::new(|_id, args, _signal, ctx| Box::pin(async move {
            let mut state = load(&ctx);
            let action = args["action"].as_str().unwrap_or("list");
            let (text, is_error) = match action {
                "add" => {
                    let t = args["text"].as_str().unwrap_or("").to_string();
                    let id = state.next_id; state.next_id += 1;
                    state.todos.push(Todo { id, text: t, done: false });
                    save(&ctx, &state);
                    (format!("Added #{}", id), false)
                }
                "toggle" => {
                    let id = args["id"].as_u64().unwrap_or(0);
                    if let Some(t) = state.todos.iter_mut().find(|t| t.id == id) { t.done = !t.done; }
                    save(&ctx, &state);
                    (format!("Toggled #{}", id), false)
                }
                "clear" => { state.todos.clear(); save(&ctx, &state); ("Cleared".into(), false) }
                _ => {
                    let body = state.todos.iter()
                        .map(|t| format!("{} [{}] {}", t.id, if t.done {"x"} else {" "}, t.text))
                        .collect::<Vec<_>>().join("\n");
                    (body, false)
                }
            };
            Ok(ToolResult {
                content: vec![ContentBlock::Text(text)],
                details: serde_json::to_value(&state).unwrap(),
                is_error,
            })
        })),
    });
}
```

**对比旧设计**：旧方案通过 `api.metadata_mut().insert("todo_list", ...)` 把状态塞进全局 HashMap —— 现在状态在 `~/.gasket/tool_state/todo_list/state.json`，plugin 自己读写自己的文件，类型由 `State` struct 保证，agent core 完全不参与。

#### 示例 3：Permission Gate（before_tool_call 拦截）

```rust
// ~/.gasket/plugins/permission_gate/src/lib.rs
use gasket::extension::{ExtensionApi, ExtensionContext, BeforeToolCallHandler, ToolCallVerdict};

struct Gate;

impl BeforeToolCallHandler for Gate {
    fn call(&self, _id: &str, tool_name: &str, args: &serde_json::Value, _ctx: &ExtensionContext)
        -> ToolCallVerdict
    {
        // 危险命令模式
        let dangerous = tool_name == "bash" && {
            let cmd = args["command"].as_str().unwrap_or("");
            cmd.contains("rm -rf") || cmd.contains("sudo ") || cmd.contains("chmod 777")
        };

        match dangerous {
            true => ToolCallVerdict::Block(
                "Blocked: dangerous command pattern. Ask user to confirm.".into()
            ),
            false => ToolCallVerdict::Allow,
        }
    }
}

#[no_mangle]
pub extern "C" fn register(api: &mut dyn ExtensionApi) {
    api.register_before_tool_call(Box::new(Gate));
}
```

**对比旧设计**：旧方案订阅 `on_event("before_tool_call")` 然后往 `ctx.metadata` 写一个 `pending_permission` 标记 —— 但 agent loop 根本不读那个标记，tool 照样执行。新方案用 `register_before_tool_call` 返回 `ToolCallVerdict::Block`，agent loop 在 §4.3 看到 Block 就直接 `continue`，拦截链路真正闭环。

#### 示例 4：Custom Provider（注册新 LLM 提供商）

```rust
// ~/.gasket/plugins/custom_provider/src/lib.rs
use gasket::extension::{ExtensionApi, ProviderSpec};

#[no_mangle]
pub extern "C" fn register(api: &mut dyn ExtensionApi) {
    api.register_provider("my-proxy", ProviderSpec {
        api: "openai-completions",  // 复用 OpenAI 协议
        base_url: "https://proxy.example.com/v1".into(),
        api_key_env: "MY_PROXY_API_KEY".into(),
        models: vec![
            ModelSpec {
                id: "claude-sonnet-4-via-proxy".into(),
                name: "Claude 4 Sonnet (proxy)".into(),
                context_window: 200_000,
                max_tokens: 16_384,
                supports_thinking: false,
                cost: ModelCost { input: 0.0, output: 0.0 },
            }
        ],
    });
}
```

### 5.4 Plugin API 完整列表

| API | 用途 | 例子 |
|---|---|---|
| `register_tool(definition)` | 给 LLM 加新工具 | hello, todo, web_search |
| `register_before_tool_call(handler)` | **拦截/改写 tool 调用**（返回 Verdict） | permission_gate, 参数校验 |
| `register_after_tool_call(handler)` | **改写 tool 结果**（脱敏/压缩） | 日志脱敏, 截断大输出 |
| `register_command(name, opts)` | 加 host 命令 | `/todos`, `/compact` |
| `register_provider(name, spec)` | 加/覆盖 LLM provider | custom-proxy, GitLab Duo |
| `unregister_provider(name)` | 移除 provider | 测试用 |
| `on_event(name, handler)` | 订阅**单向通知**事件 | 审计日志, 持久化 |
| `send_message(msg)` | 发消息到 session | push notification |
| `exec(cmd, args)` | 跑 shell | git status, ls |
| `current_messages()` | 读消息快照 | context provider |
| `set_session_name(name)` / `get_session_name()` | 命名会话 | 命名会话 |
| `api_version()` | 查 ABI 版本 | 兼容检查 |

**总共 12 个方法**（原 14，删 `register_renderer` / `send_user_message`，加 2 个 hook），**~28 个单向事件**。

**V0.1 相对原方案的删减**：
- ❌ 删 `register_renderer` —— 渲染是 host 职责
- ❌ 删 `send_user_message` —— host 直接再调 `agent_loop()` 即可
- ❌ 删 `metadata()` / `metadata_mut()` —— plugin 状态走 `ToolContext.state_dir` 文件
- ✅ 加 `register_before_tool_call` / `register_after_tool_call` —— 拦截链路真正闭环

**完整扩展面就这么大**。

---

## 6. Storage

### 6.1 数据布局

```
~/.gasket/
├── config.toml                 # 静态配置
├── sessions/
│   └── {session_id}/
│       ├── messages.jsonl      # append-only 所有消息
│       ├── metadata.json       # session 元数据（name, model, created_at）
│       └── tool_state/         # plugin 私有状态（每个 plugin 一个目录）
│           ├── hello/          # hello plugin 的 state
│           └── todo_list/
├── plugins/                    # 见 5.2
└── logs/
    └── {session_id}.log        # 结构化日志
```

### 6.2 实现（200 行）

```rust
// src/storage/jsonl.rs

use std::path::{Path, PathBuf};
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub struct JsonlStorage {
    base_dir: PathBuf,
}

impl JsonlStorage {
    pub async fn append_message(&self, session_id: &str, msg: &AgentMessage) -> Result<(), StorageError> {
        let path = self.base_dir.join(session_id).join("messages.jsonl");
        tokio::fs::create_dir_all(path.parent().unwrap()).await?;
        let mut file = OpenOptions::new().create(true).append(true).open(&path).await?;
        let line = serde_json::to_string(msg)?;
        file.write_all(line.as_bytes()).await?;
        file.write_all(b"\n").await?;
        Ok(())
    }

    pub async fn load_messages(&self, session_id: &str) -> Result<Vec<AgentMessage>, StorageError> {
        let path = self.base_dir.join(session_id).join("messages.jsonl");
        if !path.exists() { return Ok(vec![]); }
        let file = File::open(&path).await?;
        let reader = BufReader::new(file);
        let mut messages = vec![];
        let mut lines = reader.lines();
        while let Some(line) = lines.next_line().await? {
            if line.is_empty() { continue; }
            messages.push(serde_json::from_str(&line)?);
        }
        Ok(messages)
    }
}
```

**关键设计**：
- **append-only JSONL**（不删除，不修改）
- **session 目录隔离**（每个 session 一个目录）
- **plugin 私有 state** 在 `tool_state/{plugin_name}/` 下
- **不加索引**（V0.1）—— session 数量小，全文扫描够用
- **不加 SQLite**（V0.1）—— JSONL + 内存 HashMap 足够

### 6.3 不做的存储特性

- ❌ 不做 migration 系统（schema 写在 `init()` 里）
- ❌ 不做 query 优化（session 消息 < 10000 条不需要）
- ❌ 不做 event sourcing（不是真理之源，文件本身就是）
- ❌ 不做全文搜索（plugin 可以 grep）
- ❌ 不做 vector index（plugin 可以用 sqlite-vss 或 qdrant）

---

## 7. 内置 Tool（5 个，~600 行）

```rust
// src/tools/mod.rs

pub fn built_in_tools() -> Vec<ToolDefinition> {
    vec![
        read::tool(),
        write::tool(),
        edit::tool(),
        bash::tool(),
        list::tool(),
    ]
}
```

| Tool | 参数 | 实现 | 行数 |
|---|---|---|---|
| `read` | `path: string, offset?: int, limit?: int` | `tokio::fs::read_to_string` + 分页 | 100 |
| `write` | `path: string, content: string` | `tokio::fs::write`（先 backup） | 80 |
| `edit` | `path: string, old_text: string, new_text: string` | 字符串替换 + 原子 rename | 200 |
| `bash` | `command: string, timeout?: int` | `tokio::process::Command` + 输出截断 | 150 |
| `list` | `path: string, recursive?: bool, pattern?: string` | `walkdir` + glob | 80 |

**为什么不内置**：
- ❌ `web_search` / `web_fetch` —— plugin 做（DuckDuckGo / Serper / Bing 各家不同）
- ❌ `web_view` / `browser` —— plugin 做（playwright）
- ❌ `todo` —— plugin 做（todo_list 示例）
- ❌ `cron` / `schedule` —— host 做（agent loop 只跑一次）
- ❌ `subagent` / `spawn` —— plugin 做
- ❌ `vault` / `secret` —— plugin 做
- ❌ `wiki` / `memory_search` —— plugin 做

---

## 8. Provider（2 个，~500 行）

### 8.1 OpenAI 兼容（300 行）

```rust
// src/providers/openai_compat.rs

pub struct OpenAiCompat {
    base_url: String,
    api_key: String,
    client: reqwest::Client,
}

impl OpenAiCompat {
    pub fn new(base_url: &str, api_key: &str) -> Self { ... }

    pub async fn stream(
        &self,
        model: &ModelSpec,
        messages: &[LlmMessage],
        system_prompt: &str,
        tools: &[ToolDefinition],
        signal: Option<Arc<AtomicBool>>,
    ) -> Result<impl Stream<Item = StreamChunk>, ProviderError> {
        let body = json!({
            "model": model.id,
            "messages": messages,
            "system": system_prompt,
            "tools": tools.iter().map(|t| /* OpenAI tools format */).collect::<Vec<_>>(),
            "stream": true,
            "max_tokens": model.max_tokens,
        });

        let response = self.client
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?;

        // 解析 SSE 流
        Ok(parse_sse_stream(response.bytes_stream()))
    }
}
```

**关键设计**：
- **一个 provider 兼容 80% 模型**（OpenAI、DeepSeek、智谱、月之暗面、xAI、Groq、LocalAI、Ollama、vLLM）
- **不支持的（Anthropic 原生协议）单独写一个**

### 8.2 Anthropic（200 行）

```rust
// src/providers/anthropic.rs

pub struct AnthropicProvider {
    api_key: String,
    base_url: String,
    client: reqwest::Client,
}

// 同样的 stream 接口，但 body 格式不同（system 是独立字段、tools 用 input_schema）
```

### 8.3 不做的 Provider 特性

- ❌ 不做 vendor-specific 优化（thinking 模式、prompt cache、batch）
- ❌ 不做 fallback 链（plugin 可以监听 `error` 事件自己实现）
- ❌ 不做 token 计数（plugin 可以监听 `after_provider_response` 自己算）
- ❌ 不做 cost 计算（plugin 可以监听 `after_provider_response` 自己算）

---

## 9. Host 示例（让用户立刻能用起来）

```rust
// examples/cli_host/src/main.rs

use gasket::{agent_loop, AgentContext, AgentLoopConfig, AgentMessage, ContentBlock};
use gasket::extension::discover_plugins;
use gasket::providers::OpenAiCompat;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. 加载 plugin
    let mut api = gasket::extension::ExtensionApiImpl::new();
    for path in discover_plugins("~/.gasket/plugins") {
        gasket::extension::load_plugin(&path, &mut api)?;
    }

    // 2. 拼装 context
    let context = AgentContext {
        system_prompt: "You are a helpful coding assistant.".into(),
        messages: vec![],
        tools: gasket::tools::built_in_tools(),
        cwd: std::env::current_dir()?,
        env: std::env::vars().collect(),
        session_id: uuid::Uuid::new_v4().to_string(),
    };

    // 3. 配置 LLM
    let provider = OpenAiCompat::new(
        "https://api.deepseek.com/v1",
        &std::env::var("DEEPSEEK_API_KEY")?,
    );
    let config = AgentLoopConfig {
        model: /* DeepSeek spec */ todo!(),
        thinking_level: Default::default(),
        max_turns: 50,
        max_tool_calls_per_turn: 20,
        api_key: None,
        signal: None,
        stream_fn: Arc::new(move |model, msgs, sys, tools, sig| {
            let provider = provider.clone();
            Box::pin(async move { provider.stream(model, msgs, sys, tools, sig).await })
        }),
    };

    // 4. 跑 agent loop
    let user_msg = AgentMessage::User(UserMessage {
        content: vec![ContentBlock::Text(std::env::args().nth(1).unwrap_or_else(|| "Hello!".into()))],
        timestamp: gasket::now(),
    });

    let stream = agent_loop(vec![user_msg], context, config);
    while let Some(event) = stream.next().await {
        match event {
            AgentEvent::MessageUpdate { delta: ContentDelta::TextDelta(t), .. } => print!("{}", t),
            AgentEvent::ToolExecutionStart { tool_name, .. } => println!("\n[{}]", tool_name),
            _ => {}
        }
    }

    Ok(())
}
```

**host 例子 ~80 行**。这就是"用 gasket 做点事"的全部代码。

---

## 10. 重构路径（3 阶段，每阶段独立可发布）

> **执行后收敛**：原 4 阶段（删 crate / 换 storage / 重写 kernel / 文档）。执行后阶段 2（换 storage）合并进阶段 3，因为 engine 与 SQLite 结构性耦合、无法分开换。现实质为 3 阶段：① 删 channel adapter（已完成）→ ② 重写 engine+storage（合并）→ ③ plugin 文档。

> **阶段顺序原则：先减负、再动核心**。阶段 1 先删能安全删的（channel adapter），阶段 2 重写 engine+storage 时一次性吸收所有推迟项（broker/sandbox/embedding 删除 + SQLite→JSONL），避免"engine 用新 storage API、storage 还是 SQLite"的混乱中间态。

### 阶段 0：当前状态（Baseline）

```
gasket/                          # 60,737 行
├── types/        3,188 行
├── storage/      5,156 行   ← 保留，阶段 2 重写
├── embedding/    3,408 行   ← engine 无条件引用 24 处，推迟到阶段 3 与 engine 重写一起删
├── broker/       1,228 行   ← engine/cli 引用 9 处，推迟到阶段 3
├── engine/      29,467 行   ← 大幅重写（阶段 3）
├── cli/          5,386 行   ← 重写为 examples/cli_host（阶段 3）
├── providers/    3,111 行   ← 保留，阶段 3 重写
├── channels/     2,896 行   ← 阶段 1 删 adapter 实现；核心类型(SessionKey/ChannelType)保留至阶段 3
├── sandbox/      4,773 行   ← engine/cli 引用 13 处，推迟到阶段 3
└── command/      1,955 行   ← CLI 命令系统核心(Dispatcher/builtins)，保留
```

> **重要修正（执行反馈）**：原方案设想「阶段 1 删 channels + command 整个 crate」，但依赖核查发现 **command 是 CLI 斜杠命令路由核心**（/help /clear /model /sessions）、**channels 提供 `SessionKey`/`ChannelType` 给 agent 主命令路径**——两者都不是"可删集成"。真正能安全删的只有 channels 里的 **adapter 实现**（telegram/discord/slack/feishu/wechat/websocket）。broker/sandbox/embedding 因 engine 无条件引用（共 41 处），推迟到阶段 3 与 engine 重写一起删，避免阶段 1 就动 kernel。

### 阶段 1：删除 channel adapters（已完成 ✅）

**目标**：删除 channels crate 里的 platform adapter 实现，缩小爆破半径。此阶段**不碰** kernel / storage，只删 adapter + 其宿主 CLI 子命令。

**改动**（实际执行）：
| 改动 | 操作 | 风险 |
|---|---|---|
| `channels/src/{telegram,discord,slack,wechat,websocket}.rs` + `feishu/` | git rm（6 个 adapter 源） | 低 |
| `channels/src/lib.rs` | 删 `#[cfg(feature)]` adapter mod 声明 + websocket 导出 + 顶部 adapter 文档 | 低 |
| `channels/Cargo.toml` | 删 7 个 feature（telegram/.../all-channels）+ 8 个 optional deps（teloxide/serenity/axum/...） | 低 |
| `cli/src/commands/gateway.rs`（954 行） | git rm（整个文件以 axum Router 为骨架，服务已删的多 channel gateway） | 低 |
| `cli/Cargo.toml` | 删 7 个 channel feature + `full` feature 里的 `all-channels` 引用 + 3 个孤儿 deps（teloxide/axum/tower-http） | 低 |
| `cli/src/{main,cli,commands/mod}.rs` | 移除 `Commands::Gateway` 变体 + 分发 + help 文本 | 低 |
| `channels/src/adapter.rs` | 新增 `CliAdapter`（从已删 websocket.rs 挽救出来，加 `Copy`） | — |
| `channels/src/provider.rs` | `crate::websocket::CliAdapter` → `crate::adapter::CliAdapter`；删 `routes() -> Option<axum::Router>` 方法；`from_config` 参数加 `_` 前缀 | — |

**刻意保留**（执行中发现是核心依赖，非"可删集成"）：
- `command/` 整个 crate —— CLI 斜杠命令路由核心（Dispatcher + /help /clear /model /sessions + parser + completer）
- `channels/` 核心类型 —— `SessionKey`/`ChannelType`/`InboundMessage`/`OutboundMessage`/`ImAdapter`/`ImProvider`/middleware，agent.rs 主命令路径依赖
- `broker/` `sandbox/` `embedding/` —— engine 无条件引用（41 处），推迟到阶段 3 与 engine 重写一起删

**结果**：
- 总行数 60,737 → **57,748**（净删 2,989 行）
- channels crate 2,896 → 868 行（删 adapter，保留核心类型）
- crate 数不变（10 → 10），因为只删了 channels 的 adapter 实现，没删整个 crate

**验证标准**（已达成）：
- [x] `cargo build --workspace` 通过，无 error
- [x] `cargo test -p gasket-channels` 7 passed; 0 failed
- [x] 总代码行数 57,748（原估算 <50,000 偏高，因实际只删 adapter 而非整个 crate）

> **认知修正**：阶段 1 没能像原设想那样"删 channels+command 两个 crate（~5000 行）"——因为它们承载 CLI 核心。真实可安全删的是 adapter 实现（~3000 行）。broker/sandbox/embedding 的删除推迟到阶段 3。

### 阶段 2：~~换 storage~~ → 合并进阶段 3（执行反馈）

**原设想**：独立把 storage 从 SQLite 换成 JSONL，不碰 engine。

**执行核查结论**：**不可行，已合并进阶段 3**。核查发现 engine 与 SQLite 是**结构性耦合**，无法分开换：

- `engine/src/session/compactor/mod.rs`（**927 行**）整个是 SQL 查询——session 压缩核心直接吃 SQLite
- engine 另有 2 个文件直接写 SQL（`history_query.rs` 196 行、`wiki/log.rs` 45 行）
- `EventStore` 被 engine 16 文件引用、`SessionStore` 12 文件、`wiki` 37 处、`SqliteStore` 10 文件
- storage crate **本身是薄封装、无死代码**——每个 store 都被 engine 实际依赖：kv_store→workflow 工具、maintenance_store→provider/evolution、migrations→SQLite 初始化、cron_store→cron 功能

> **认知修正**：阶段 2"窄切 storage 边角"在事实上找不到边角。SQLite→JSONL 的转换必须在 engine 重写时完成，因为 engine 直接穿透 storage 抽象写 SQL。强行分两阶段会制造"engine 用新 storage API、storage 还是 SQLite"的混乱中间态。**阶段 2 与阶段 3 合并**，storage 的 SQLite→JSONL 翻转随 engine 重写一并完成。

### 阶段 3：重写 kernel + storage（原阶段 2+3 合并，2-3 周）

**目标**：交付 `agent_loop()` + `ExtensionApi`（含 hook）+ 5 个内置 tool + **JSONL storage**。这是整个重构的核心阶段，engine 与 storage 一起重写，因为两者结构性耦合。

**改动 — engine/kernel**：
| 改动 | 操作 | 风险 |
|---|---|---|
| `engine/src/kernel/{executor,kernel_executor,steppable_executor,tool_executor,request_handler,synthesis}.rs` | 合并为 1 个 `agent_loop.rs` | 低（5 文件做 1 文件的事） |
| `engine/src/kernel/` 整个目录 | 重命名为 `src/agent_loop.rs` | 中（公开 API 改名） |
| `engine/src/hooks/{external,vault,wiki_lint,registry,types,mod}.rs` | 替换为 `src/extension/{api,loader,events}.rs` ~400 行 | 中 |
| `engine/src/external_tools/` 整个模块 | 替换为 `src/extension/loader.rs` ~250 行（cdylib + GASKET_ABI_VERSION） | 中（破坏内部 plugin 协议，无外部用户） |
| `engine/src/tools/` 30+ 工具 | 缩减到 5 个内置工具 | 低（无外部用户） |
| `engine/src/session/compactor/mod.rs`（927 行 SQL） | 重写为基于 JSONL 的 token 截断（或 V0.1 简化为固定窗口） | 高（session 核心逻辑） |

**改动 — 删除非核心 crate**（原阶段 1/2 推迟项）：
| 改动 | 操作 | 风险 |
|---|---|---|
| **`embedding/` 整个 crate** | 删除（engine 24 处引用随重写消除） | 中 |
| **`broker/` 整个 crate** | 删除（engine/cli 9 处引用随重写消除） | 中 |
| **`sandbox/` 整个 crate** | 删除（engine/cli 13 处引用随重写消除） | 中 |
| **`command/` 核心类型 + `channels/` 核心类型** | 内化到 gasket crate（如 `SessionKey`→`session_id: String`），删 crate | 中 |
| `engine/src/cron/` / `subagents/` / `wiki/` / `heartbeat/` | 移到 examples/ 或删除 | 低 |

**改动 — storage SQLite→JSONL**（原阶段 2）：
| 改动 | 操作 | 风险 |
|---|---|---|
| `storage/src/event_store.rs`（1,255 行） | 删除，session 消息改用 append-only JSONL | 高（engine 16 文件引用） |
| `storage/src/session_store.rs` | 重写为 `storage/src/jsonl.rs` ~200 行 | 中 |
| `storage/src/{processor,query,kv_store,maintenance_store,cron_store}.rs` | 删除（功能随 engine 工具缩减而消失） | 中 |
| `storage/src/migrations/` | 删除（JSONL 无 schema） | 低 |
| `storage/src/wiki/`（2,584 行） | 删除（wiki 改 ripgrep 文件搜索） | 中 |
| `storage` 对 `sqlx` 的依赖 | 移除 | 低 |

**结果**：engine 从 29,467 行 → ~5,000 行；storage 从 5,156 行 → ~500 行；从 10 crate → 1 crate（gasket）

**验证标准**：
- [ ] `examples/cli_host` 能跑 hello 对话
- [ ] 5 个内置工具单元测试通过
- [ ] `agent_loop()` 公开 API 文档完整
- [ ] `ExtensionApi` 完整 trait 文档（含 hook handler）
- [ ] permission_gate 示例能真正 Block 危险命令（验证 hook 闭环）
- [ ] session 消息 JSONL append/load < 10ms（10,000 条内）
- [ ] 无 `sqlx` 依赖（`cargo tree` 确认）

### 阶段 4：Plugin 文档 + 5 个示例 plugin（1 周）

**目标**：让"写 plugin 变容易"（在固定 toolchain 下，见 §5.1.1）

**改动**：
- 写 `docs/plugin-tutorial.md`（一步步教，含 ABI 版本说明）
- 写 5 个示例 plugin：
  - `hello_tool`（最简，21 行）
  - `todo_list`（状态管理 + 文件存储，100 行）
  - `permission_gate`（register_before_tool_call 拦截，80 行）
  - `custom_provider`（注册 provider，60 行）
  - `telegram_channel`（host 集成，200 行）
- 写 `examples/full_host/` （Telegram bot + gasket，300 行）

**验证标准**：
- [ ] 按 tutorial 能在固定 toolchain 下编译并加载 hello plugin（需同 host ABI 版本编译，见 §5.1.1）
- [ ] 5 个示例 plugin `cargo build` 全部通过
- [ ] `examples/full_host` 跑起来能对话

### 阶段 5（可选）：V0.2 增量

- ⏸ Compaction（基于 token 阈值的简单实现）
- ⏸ Branch summary（pi 风格，880 行 → 简化到 200 行）
- ⏸ Skills（从目录加载 prompt 模板）
- ⏸ Parallel tool execution（V0.1 不做，V0.2 加）
- ⏸ Stream cancellation refinement
- ⏸ OpenTelemetry tracing（可选 feature）
- ⏸ `on_update` 流式 tool 进度（若 V0.1 后真有长任务需求，配合 `ToolExecutionUpdate` 事件一起加）

---

## 11. 风险与回滚

### 11.1 风险

> **事实修正**：本仓库 crates.io **未发布**（`publish` 字段为空），无外部消费者。因此"破坏现有用户"风险实际为 **0**，无需 `#[deprecated]` 过渡期、无需保留旧 minor 版本——直接删，省下两周。下面只列真实风险。

| 风险 | 概率 | 影响 | 缓解 |
|---|---|---|---|
| **cdylib ABI 不稳定**（plugin 与 host 不同 toolchain/依赖版本时内存踩烂） | 高 | **高** | §5.1.1 诚实声明 + 独立 `GASKET_ABI_VERSION` 检查 + 文档要求 plugin 随 host 同编译。CI 跑 plugin 加载冒烟测试 |
| **cdylib 跨平台问题**（.so/.dylib/.dll 加载差异） | 中 | 中 | CI 在 linux/macos/windows 都跑 plugin 加载测试 |
| LLM 协议 bug（provider 适配回归） | 中 | 高 | 阶段 3（kernel 重写）期间专门做 provider 回归测试 |
| 内部 plugin 协议破坏（external_tools 子进程→cdylib） | 中 | 低 | 阶段 3 替换 loader 时同步迁移仓库内 plugin |
| 性能回退 | 中 | 中 | 跑 `cargo bench` 对比 kernel 重写前后 |

> 注：原方案把"破坏现有用户"标为概率高/影响中、"cdylib"标为概率中/影响中——**两者都误判**。前者是想象的（无用户），后者是真实且影响最大的（会让核心卖点破产）。

### 11.2 回滚策略

每个阶段独立 tag（对齐新阶段顺序，阶段 2 已合并进阶段 3）：
```
v0.10.0  # 阶段 0 baseline
v0.11.0  # 阶段 1 完成（删除 channel adapters + gateway 子命令）✅
v0.12.0  # 阶段 2 完成（重写 engine + storage SQLite→JSONL + 删 broker/sandbox/embedding）
v0.13.0  # 阶段 3 完成（plugin 文档 + 5 examples）
```

如果阶段 N 出问题，可以从 `v0.(N-1).0` 拉分支 hotfix，不阻塞主线。

### 11.3 验证矩阵

每个 PR 必跑：
- [ ] `cargo build --release` 无 warning
- [ ] `cargo test` 全过
- [ ] `cargo clippy -- -D warnings` 无 warning
- [ ] `cargo bench` 与 baseline 对比
- [ ] `examples/cli_host` 实际跑通 hello 对话
- [ ] 至少 1 个 plugin 加载并触发事件

---

## 12. V0.1 目标规模

```
gasket/                          # 单 crate
├── Cargo.toml                   # 50 行
├── src/
│   ├── lib.rs                   # 100 行（公开 API）
│   ├── agent_loop.rs            # 500 行 ← 核心
│   ├── types/
│   │   ├── mod.rs               # 50 行
│   │   ├── message.rs           # 150 行
│   │   ├── event.rs             # 200 行
│   │   ├── context.rs           # 100 行
│   │   └── tool.rs              # 100 行
│   ├── extension/
│   │   ├── mod.rs               # 50 行
│   │   ├── api.rs               # 300 行 ← ExtensionApi trait
│   │   ├── events.rs            # 100 行
│   │   └── loader.rs            # 250 行 ← cdylib loader
│   ├── tools/
│   │   ├── mod.rs               # 30 行
│   │   ├── read.rs              # 100 行
│   │   ├── write.rs             # 80 行
│   │   ├── edit.rs              # 200 行
│   │   ├── bash.rs              # 150 行
│   │   └── list.rs              # 80 行
│   ├── providers/
│   │   ├── mod.rs               # 30 行
│   │   ├── openai_compat.rs     # 300 行
│   │   └── anthropic.rs         # 200 行
│   ├── storage/
│   │   ├── mod.rs               # 30 行
│   │   ├── jsonl.rs             # 200 行
│   │   └── session.rs           # 150 行
│   ├── compaction.rs            # 250 行（V0.1 简单版）
│   └── error.rs                 # 50 行
├── examples/
│   ├── cli_host/                # 80 行
│   ├── hello_plugin/            # 50 行
│   ├── todo_list/               # 100 行
│   ├── permission_gate/         # 80 行
│   ├── custom_provider/         # 60 行
│   └── telegram_host/           # 200 行
├── tests/
│   └── integration.rs           # 300 行
└── docs/
    ├── quickstart.md
    ├── architecture.md
    ├── plugin-tutorial.md
    └── api-reference.md
```

**总计：~3,800 行**（含 examples + tests + docs）

**对比**：
- 当前 gasket：60,737 行
- pi-agent-core：10,028 行
- gasket V0.1 目标：3,800 行（含 examples/docs）
- gasket V0.1 目标（仅 lib）：~2,800 行

**收益**：
- 编译时间：从分钟级 → **10 秒级**
- 理解成本：新人 1 周能上手 → **1 天能上手**
- 添加新 plugin：需在固定 toolchain 下 clone 仓库编译（见 §5.1.1），参考示例快速上手
- 添加新 tool：参考 hello 示例，约 30 行实现一个新 tool
- 单元测试覆盖率：60% → **85%**（更少代码，更易测）

---

## 13. 不做什么（关键决策记录）

为避免未来再走回头路，明确记录**不引入**的设计：

| 不引入 | 原因 | 替代方案 |
|---|---|---|
| Event sourcing | append-only JSONL 已够用 | 直接读文件 |
| SQLite | 简单数据不需要 RDBMS | JSONL + 内存 HashMap |
| Vector embedding | 个人助理不需要 RAG | ripgrep 文件搜索 |
| 进程级 plugin 沙箱 | Rust 写 plugin 是可信的 | 让用户用 bwrap / Docker |
| Multi-agent | 单 agent + 强 plugin 已够用 | plugin 自己实现 subagent |
| Workflow engine | 用户工作流不应该框架定 | plugin + slash command 组合 |
| Broker / 消息总线 | 单进程不需要异步消息 | 直接 function call |
| MCP | 生态锁定风险 | plugin 自己做 |
| Migration 系统 | schema 写在 init() 里 | 简单就是好 |
| ORM | 数据少不需要 | 直接 serde |
| Vendor-specific 优化 | 维护成本高 | plugin 自己 patch |
| 复杂 hook 框架 | 30 个 on() 已覆盖 90% 场景 | 显式 API |
| ToolProvider 多实现 | ToolRegistry 一个 trait 已够 | 简单 list |
| **plugin 共享公告板（`metadata: HashMap<String, Value>`）** | 逃避类型系统的全局可变狗窝，plugin 抢 key、序列化失败、debug 困难；4 个示例 plugin 无一真正需要跨 plugin 共享 | plugin 私有状态写 `~/.gasket/tool_state/{plugin}/` 文件（§6.1） |
| **跨语言 / 跨版本 plugin 分发** | cdylib 无稳定 ABI，跨 toolchain/依赖版本会内存踩烂；伪装成"任意语言/独立分发"是想象式设计 | plugin 必须随 host 同 toolchain 编译，`GASKET_ABI_VERSION` 独立于语义版本（§5.1.1）。要跨语言则回子进程 JSON-RPC（V0.2+） |
| **tool 流式 `on_update` 回调** | 5 个内置 tool 无一需要流式进度，没有消费者的接口是死接口 | V0.1 不做；V0.2 若有长任务需求，配合 `ToolExecutionUpdate` 事件一起加 |
| **`register_renderer`（消息渲染进 core）** | 渲染是 host 职责（CLI/TUI/Telegram 各自渲染） | host 自己渲染 |

**未来如果某个"不引入"被实际需求推翻**：
- 添加成本要明确写进 RFC
- 要有真实的 3 个以上使用场景
- 要有"简单方案"被证伪的具体例子

---

## 14. Review 检查清单

请按以下问题 review：

### 14.1 目标问题

- [ ] **目标清晰吗？** "Pi-Agent 风格的可插拔 agent core" 这个目标你认可吗？
- [ ] **非目标对吗？** 砍掉 sandbox/embedding/multi-agent 这些，你同意吗？
- [ ] **推迟项合理吗？** Compaction / Skills / Parallel 这些放到 V0.2 你能接受吗？

### 14.2 架构问题

- [ ] **三抽象够吗？** `AgentMessage` / `AgentEvent` / `ExtensionApi` 够描述所有 plugin 需求吗？
- [ ] **~28 个单向事件够吗？** 还缺什么事件？（注：拦截已独立为 hook，不在事件数内）
- [ ] **12 个 API 太多还是太少？** 哪些该删 / 哪些该加？（V0.1 已删 renderer/send_user_message/metadata，加 2 个 hook）
- [x] **Plugin = cdylib 对吗？** —— **已决定**：V0.1 用 cdylib + 锁 toolchain/ABI 版本（§5.1.1）。若未来要求跨语言/跨版本，则放弃 cdylib 回到子进程 JSON-RPC（V0.2+ 独立决策）。
- [ ] **JSONL 够用吗？** 还是要 SQLite？

### 14.3 重构路径问题

- [x] **4 阶段顺序对吗？** —— **已验证并收敛为 3 阶段**：阶段 1 执行确认"先删后重写"是对的，但发现 embedding/broker/sandbox 不能在阶段 1 删（engine 无条件引用）。阶段 2 核查又发现 engine 与 SQLite 结构性耦合（compactor 927 行是 SQL），换 storage 无法独立于 engine 重写。故阶段 2 合并进阶段 3，实质为 3 阶段：①删 adapter（已完成）→ ②重写 engine+storage → ③plugin 文档。
- [x] **每个阶段的工作量评估合理吗？** —— 阶段 1 实际工作量小于预估（只删 adapter + gateway 子命令），1 周内可完成（已实际完成）。
- [ ] **风险评估合理吗？** 哪一步风险最大？
- [ ] **回滚策略够吗？** 还是想要更细粒度的 feature flag？

### 14.4 数据模型问题

- [ ] **`AgentMessage` 的 4 个变体够吗？** 还需要 `System` / `Summary` / `Branch` 吗？
- [ ] **`ContentBlock` 的 5 种够吗？** 还需要 `ToolUse` / `Image` / `Audio` 吗？
- [x] **`metadata: HashMap<String, Value>` 是好设计吗？** —— **已决定：不是，已删除**（§3.3/§3.5）。plugin 私有状态走 `ToolContext.state_dir` 文件，跨 plugin 共享不进 core（§13）。
- [ ] **`session_id` 字符串还是用 Uuid 类型？**

### 14.5 Plugin 设计问题

- [ ] **`manifest.toml` + `lib.so` 是好形式吗？** 还是想统一 `plugin.json`？
- [ ] **每个 plugin 一个 `register` 函数够吗？** 还是要 `init` / `shutdown` / `health` 多个？
- [ ] **Plugin 能改 `system_prompt` 吗？** 还是只能订阅事件？
- [ ] **Plugin 能改 `tools` 列表吗？**（V0.1 暂不允许）

### 14.6 工程问题

- [ ] **V0.1 目标规模 3,800 行太大还是太小？**
- [ ] **每个 PR 1-2 周合理吗？** 还是想要更小的 PR？
- [ ] **测试覆盖率目标 85% 够吗？**
- [ ] **是否需要 `cargo bench` 基线？**

### 14.7 哲学问题

- [ ] **"信任开发者"是正确选择吗？** 还是需要某些 plugin 沙箱？
- [ ] **Plugin 必须 Rust 写 OK 吗？** 还是必须支持其他语言？
- [ ] **单 crate 是终点吗？** 还是预留拆分空间？

---

## 附录 A：与当前 gasket 设计的对比

| 维度 | 当前 gasket | V0.1 目标 | 差异 |
|---|---|---|---|
| 总代码 | 60,737 行 | 3,800 行 | -94% |
| Crate 数 | 10 | 1 | -90% |
| `pub` API | 1,012 | ~80 | -92% |
| 工具数 | 30+ | 5 内置 + N plugin | 内置 -83% |
| 扩展点 | 6 文件 hook 系统 | 1 个 trait + ~28 事件 + 2 hook | 简化 |
| Plugin 形式 | 子进程 + JSON-RPC | cdylib 函数 | 简化 |
| Storage | SQLite + event store | JSONL 文件 | 简化 |
| Provider 适配 | 7 个独立实现 | 2 个通用 | 简化 |
| Channel 集成 | 6 个内置 | 0（plugin 做） | 简化 |
| Sandbox | 4,773 行 | 0 | 删除 |
| 部署形态 | binary | library + binary | 重心转移 |

## 附录 B：参考实现链接

- pi-mono: https://github.com/earendil-works/pi-mono
- pi-agent-core（10,028 行）: https://github.com/earendil-works/pi-mono/tree/main/packages/agent
- pi-coding-agent 70+ 扩展示例: https://github.com/earendil-works/pi-mono/tree/main/packages/coding-agent/examples/extensions
- agent-loop.ts 源码: https://github.com/earendil-works/pi-mono/blob/main/packages/agent/src/agent-loop.ts
- extension loader.ts 源码: https://github.com/earendil-works/pi-mono/blob/main/packages/coding-agent/src/core/extensions/loader.ts
- ExtensionApi 定义: https://github.com/earendil-works/pi-mono/blob/main/packages/coding-agent/src/core/extensions/types.ts#L1179

## 附录 C：变更摘要（commit message 模板）

```
refactor(agent): collapse 4-layer executor into single agent_loop

The kernel was split across 5 files (executor.rs, kernel_executor.rs,
steppable_executor.rs, tool_executor.rs, request_handler.rs) doing what
one file can do clearly. This change merges them into a single
agent_loop.rs (~500 lines) modeled on pi-agent-core's agent-loop.ts.

Key changes:
- Replace 4-layer executor with single agent_loop function
- Introduce EventStream as the only output channel
- Introduce ExtensionApi trait with 14 methods
- Add 30+ AgentEvent variants for state observability
- Keep before_tool_call / after_tool_call as direct hook calls,
  not generic event handlers

Stats: 793 lines removed, 500 lines added, -293 net.
```

---

**End of Document**

> Next step: 收集 review 反馈 → 修订方案 → 开始阶段 1
