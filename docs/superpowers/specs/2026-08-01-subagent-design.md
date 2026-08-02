# Subagent 编排设计(M2)

> 状态:草案 · 日期 2026-08-01 · 阶段 3

## 1. 目标

让主 agent 能 spawn **并行子 agent** 处理子任务。每个子 agent 跑独立的 `run_agent_loop`,事件实时转发给前端(前端已预留 9 种 `subagent_*` 消息类型 + 完整状态机)。

一句话:LLM 调用 `spawn_subagents` 工具 → 多个子 agent 并行跑 → 结果汇总回主 agent。

## 2. 前端契约(已就绪,不可改)

前端 `types/index.ts` 定义了 9 种 WS 消息,`useChatSession.ts` 有完整 handler。core/gateway 必须**发送**这些:

| WS 消息 | 载荷 | 时机 |
|---|---|---|
| `subagent_all_started` | `{count}` | 所有子 agent 即将启动 |
| `subagent_started` | `{id, task, index}` | 单个子 agent 启动 |
| `subagent_thinking` | `{id, content}` | 子 agent 思考增量 |
| `subagent_content` | `{id, content}` | 子 agent 文本增量 |
| `subagent_tool_start` | `{id, name, arguments?}` | 子 agent 工具开始 |
| `subagent_tool_end` | `{id, tool_id?, name, output?}` | 子 agent 工具结束 |
| `subagent_completed` | `{id, index, summary, tool_count}` | 子 agent 完成 |
| `subagent_error` | `{id, index, error}` | 子 agent 失败 |
| `subagent_synthesizing` | `{}` | 所有子 agent 完成,主 agent 汇总中 |

## 3. 设计

### 3.1 新增工具:`spawn_subagents`(`core/tools/subagent.rs`)

```
工具: spawn_subagents
参数: {
  tasks: [{ task: string }]  // 每个任务的描述
}
风险: Medium
```

这个工具的 `execute` 闭包**不是**普通同步执行——它需要访问 `AgentContext`(tools/stream_fn/hooks)来为每个子任务构造子 agent loop。但 `ToolFn` 签名只拿到 `ToolCallCtx`(args/signal/ctx),拿不到父 agent 的 config。

**解决方案**:子 agent 编排逻辑不放在工具闭包里,而是放在 `agent_loop` 层。`spawn_subagents` 是一个**特殊工具标记**——`execute_tool_calls` 检测到它时,不执行普通闭包,而是走 subagent 编排路径。

> 更简洁的替代:在 `ToolContext` 里加一个 `Option<Arc<SubagentSpawner>>` 字段,host 在构造 context 时填入。工具闭包调 `ctx.ctx.subagent_spawner.spawn(tasks)`。这比"特殊工具检测"更干净——工具不需要知道 subagent 的存在,只是调一个 context 提供的能力。**采用这个方案。**

### 3.2 `SubagentSpawner`(`core/src/subagent.rs`)

```rust
pub struct SubagentSpawn {
    pub task: String,
}

pub struct SubagentResult {
    pub id: String,
    pub task: String,
    pub index: usize,
    pub summary: String,    // 前 200 字符的 assistant 最终文本
    pub tool_count: usize,
    pub error: Option<String>,
}

pub trait SubagentSpawner: Send + Sync {
    fn spawn(
        &self,
        tasks: Vec<SubagentSpawn>,
        emit: Box<dyn Fn(SubagentEvent) + Send>,
    ) -> Pin<Box<dyn Future<Output = Vec<SubagentResult>> + Send>>;
}

pub enum SubagentEvent {
    AllStarted { count: usize },
    Started { id: String, task: String, index: usize },
    Thinking { id: String, content: String },
    Content { id: String, content: String },
    ToolStart { id: String, name: String, arguments: Option<String> },
    ToolEnd { id: String, tool_id: Option<String>, name: String, output: Option<String> },
    Completed { id: String, index: usize, summary: String, tool_count: usize },
    Error { id: String, index: usize, error: String },
    Synthesizing,
}
```

`SubagentEvent` 的变体与前端 9 种 WS 消息一一对应。gateway 的 event_map 把它转成 WS JSON。

### 3.3 并行执行(`host/src/subagent.rs`)

host 实现 `SubagentSpawner`:

```rust
impl SubagentSpawner for HostSubagentSpawner {
    fn spawn(&self, tasks, emit) -> Future<Vec<SubagentResult>> {
        emit(SubagentEvent::AllStarted { count: tasks.len() });
        // tokio::spawn 每个子任务,各自跑 run_agent_loop
        let handles: Vec<_> = tasks.enumerate().map(|(i, task)| {
            let id = uuid::Uuid::new_v4().to_string();
            let sub_context = self.build_subagent_context(&task); // 同 tools,新 messages
            let sub_config = self.build_subagent_config();        // 同 stream_fn/hooks,更少 max_turns
            tokio::spawn(async move {
                emit(Started { id, task, index: i+1 });
                let result = run_agent_loop(vec![user_msg(task)], sub_context, sub_config, |ev| {
                    // 子 agent 的 AgentEvent → SubagentEvent → emit
                    map_subagent_event(&id, ev, &emit);
                }).await;
                emit(Completed { id, summary: extract_summary(&result), tool_count: count_tools(&result) });
                result
            })
        }).collect();
        emit(SubagentEvent::Synthesizing);
        join_all(handles).await → collect results
    }
}
```

关键设计:
- **子 agent 用同一套 tools/stream_fn/hooks**(继承父)。
- **子 agent 的 max_turns 更少**(如 10,避免无限循环)。
- **子 agent 不写磁盘**(它们是临时 worker,结果经 ToolResult 回主 agent,主 agent 持久化)。
- **子 agent 的 AbortSignal 独立**(但跟随父 signal——父取消,子也取消)。

### 3.4 子 agent 事件映射

子 agent 跑自己的 `run_agent_loop`,它的 `AgentEvent` 被一个 adapter 转成 `SubagentEvent`:

| 子 agent AgentEvent | → SubagentEvent |
|---|---|
| `MessageUpdate { TextDelta }` | `Content { id, content }` |
| `MessageUpdate { ThinkingDelta }` | `Thinking { id, content }` |
| `ToolExecutionStart` | `ToolStart { id, name, arguments }` |
| `ToolExecutionEnd` | `ToolEnd { id, tool_id, name, output }` |
| (loop 结束,提取 summary) | `Completed { id, summary, tool_count }` |
| (loop 报错) | `Error { id, error }` |

### 3.5 Gateway 转发(`event_map.rs` 扩展)

`spawn_subagents` 工具执行时,`SubagentEvent` 流经一个新的事件通道。gateway 的 forwarder 把它们转成 WS JSON:

```rust
// event_map.rs 新增
fn subagent_event_to_ws(event: &SubagentEvent) -> OutgoingEvent { ... }
```

### 3.6 结果汇总

所有子 agent 完成后,`spawn_subagents` 工具返回一个 ToolResult,内容是每个子 agent 的 task + summary:

```text
Subagent 1 (search codebase): Found 3 relevant files: ...
Subagent 2 (write tests): Created test_foo.rs with 5 tests
Subagent 3 (review): No issues found in the diff
```

主 agent 收到这个 ToolResult,继续推理(可能直接回复用户,或做进一步综合)。

## 4. 不碰

- 前端代码(已有完整 handler,零改动)。
- 磁盘 JSONL 格式。
- WS 协议(只新增 subagent_* 发送端,不改消息格式)。
- CLI(CLI 暂不接入 subagent——它没有多路事件通道;先做 gateway)。

## 5. 验收

1. LLM 调用 `spawn_subagents` 带 N 个任务。
2. 前端实时显示 N 个子 agent 的思考/工具调用/进度。
3. 全部完成后,主 agent 收到汇总 ToolResult。
4. 子 agent 的工具调用受同一权限策略约束(hooks 继承)。
5. 父取消(Ctrl-C / WS cancel)→ 所有子 agent 协作中止。
6. `cargo check + test` 全绿。
