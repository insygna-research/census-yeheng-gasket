# MCP 客户端接入设计

> 状态:草案 · 日期 2026-08-01 · 对应 workspace `2.0.0`

## 1. 目标

让 gasket 能连接 [Model Context Protocol](https://modelcontextprotocol.io)(MCP)生态的**现成工具服务器**,把 server 暴露的 `tools` 接成 gasket 内核的 `ToolDefinition`,与内置工具、进程内扩展(`gasket-ext`)、私有外部工具(`ExternalToolBridge`)同列供 agent loop 调用。

一句话:agent 无感地多了一批"别人写好的工具"(GitHub、文件系统、数据库、浏览器…),不必为每个工具自己写 Rust。

## 2. 范围(YAGNI 边界)

| 在范围内 | 不在范围内 |
|---|---|
| stdio 传输(subprocess) | Streamable HTTP 传输 |
| **legacy era**(`initialize` 握手,协议版本 `2025-06-18`) | modern era(`2026-07-28`,无握手,每请求 `_meta`);dual-era probe+回退 |
| 只接 **tools** 原语 | resources / prompts / sampling / elicitation / roots / logging |
| text + image 内容类型 | audio / embedded resource / resource link |
| 60s 单调用超时、崩溃不自动重启 | 自动重连、健康检查 |

**为什么只 legacy + 只 tools:** 现存 MCP server 几乎全是 legacy era;gasket 是 agent 框架,tools 是核心原语。这是最小可用范围,符合项目 lightweight 哲学。modern era / 其他原语是后续可叠加项,不影响本次接口。

## 3. 为什么新建 `mcp.rs`,不扩展 `ExternalToolBridge`

两者的**数据流模型根本不同**:

| | `ExternalToolBridge`(私有协议) | `McpBridge`(MCP) |
|---|---|---|
| 配对 | 行序("写一行读一行") | JSON-RPC `id`(server 会在响应前插 `notifications/progress`、`notifications/message`、`tools/list_changed`) |
| 握手 | 无 | `initialize` → `initialized` 通知 |
| list 字段 | `name/description/parameters/label/risk` | `name/title/description/inputSchema/outputSchema/annotations` |
| result 字段 | `content[{type,text}],is_error` | `content[{type,text|data,mimeType}],isError` |
| 配置 | `GASKET_EXTERNAL_TOOLS` 逗号分隔命令 | JSON 文件(需要 env 注入 API key) |

强行共享 `BridgeInner`(行序 reader + 读写互斥)会污染私有 bridge 的简洁性,并为单一用途造间接层。**两者平行存在,都产出 `Vec<ToolDefinition>`,由 `load_all` 统一收集,调用方无感。**

## 4. 架构与数据流

```
~/.gasket/mcp.json  (或 $GASKET_MCP_CONFIG 指定路径)
  [{ "name":"github", "command":"npx",
     "args":["-y","@modelcontextprotocol/server-github"],
     "env":{"GITHUB_PERSONAL_ACCESS_TOKEN":"…"} }]
        │
        ▼  load_all_mcp()
  对每个 server 配置:
        │
        ▼  McpBridge::spawn(name, command, args, env)
   ┌─────────────────────────────────────────────┐
   │ 子进程 (kill_on_drop, stderr inherit)        │
   │                                              │
   │  1. initialize 请求  protocolVersion         │
   │                     "2025-06-18"             │
   │                     clientInfo{gasket}        │
   │                     capabilities: {} (空)     │
   │     ← result{protocolVersion, capabilities,  │
   │              serverInfo, instructions}       │
   │  2. notifications/initialized                │
   │  3. tools/list                               │
   │     ← result{tools:[{name,inputSchema,…}]}   │
   └─────────────────────────────────────────────┘
        │  每个 MCP tool → ToolDefinition(name 加 server 前缀)
        ▼
  Vec<ToolDefinition>  ──┐
                         │ load_all() 合并
  built_in_tools() ──────┤
  ext_tools ──────────── ┤
  private external ───── ┘
        │
        ▼ host.tools
  AgentContext.tools  → run_agent_loop → hooks → execute
        │                                  │
        │                                  ▼  ToolDefinition.execute
        │                          McpBridge::call(name, args)
        │                          ├─ 发 tools/call(id 自增)
        │                          ├─ JSON-RPC reader: 按 id 匹配
        │                          │   response,跳过 notification
        │                          └─ result.content → ContentBlock
```

### 4.1 关键不变量

- **工具名唯一性**:MCP 工具名加 server 前缀 `mcp__{server}__{tool}`,避免与内置工具/跨 server 冲突。前缀后的名字是 `ToolDefinition.name` 和 `ToolDefinition.label`(`label` 用 `{server}/{tool}` 人类可读形式)。
- **配置方只产出 `Vec<ToolDefinition>`**:和 `ExternalToolBridge::spawn` 一样,bridge 的 `Arc` 被每个工具的 `execute` 闭包持有;drop 工具 → drop bridge → `kill_on_drop` 杀子进程。
- **不在内核加 MCP 概念**:`gasket-core` 零改动。MCP 是 host 层的事。

## 5. 配置格式

文件路径:`$GASKET_MCP_CONFIG` 或默认 `~/.gasket/mcp.json`。不存在 = 无 MCP 工具(静默,不报错)。

```json
{
  "mcpServers": {
    "github": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-github"],
      "env": { "GITHUB_PERSONAL_ACCESS_TOKEN": "ghp_xxx" }
    },
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/path"]
    }
  }
}
```

**为什么是 map(不是数组)、用 `mcpServers` 键、JSON:** 这是 MCP 社区事实标准(Claude Desktop / Cline / 各 SDK 的 `mcp.json` 都是这个形状)。用户可以直接复用现有配置文件。map 的 key 就是 server 名(前缀用)。

**为什么不要 `stdio`/`transport` 字段:** 本次只支持 stdio,隐含。加字段会暗示支持 HTTP,是假承诺。

**env 注入:** `env` 字段里的键值**追加**到子进程环境(不替换父进程环境)。MCP server 常需 `GITHUB_PERSONAL_ACCESS_TOKEN`、`API_KEY` 等,继承父进程不够。

### 5.1 配置加载

`fn load_config() -> Vec<McpServerConfig>`:`$GASKET_MCP_CONFIG` 路径优先;否则 `~/.gasket/mcp.json`。文件缺失 → 空 vec。JSON 解析失败 → 返回错误(配置文件坏了是真问题,该报)。`mcpServers` 为空 map → 空 vec。提供 `load_config_from(path)` 给测试用。

## 6. 详细设计:`McpBridge`

### 6.1 结构

```rust
struct McpBridgeInner {
    _child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: tokio::io::BufReader<tokio::process::ChildStdout>,
    next_id: u64,                    // JSON-RPC id 自增(握手后从 3 起)
}

pub struct McpBridge {
    inner: Mutex<McpBridgeInner>,
    timeout: Duration,
    server_name: String,             // 用于工具名前缀
}
```

> 与 `ExternalToolBridge` 结构平行。`next_id` 在 `BridgeInner` 里(而非 bridge),因为 id 是传输层状态,锁保护读写时一并自增。

### 6.2 生命周期

`spawn(name, command, args, env, timeout) -> Result<(Arc<Self>, Vec<ToolDefinition>), McpError>`:

1. `Command::new(command).args(args).envs(env)` + `stdin/stdout piped` + `stderr inherit` + `kill_on_drop(true)`。
2. 握手(每步带 `timeout`):
   - 发 `initialize`(id=1),`params: { protocolVersion: "2025-06-18", clientInfo: {name:"gasket", version: workspace_version}, capabilities: {} }`。读 response(id=1),取 `result.protocolVersion` 存为协商版本(接受 server 回退)。
   - 发 `notifications/initialized`(无 id)。
   - 发 `tools/list`(id=2)。读 response(id=2),解析 `result.tools`。
3. 对每个 MCP tool 构造 `ToolDefinition`(见 6.3)。bridge 的 `Arc` clone 进每个 `execute` 闭包。

### 6.3 工具映射

```
MCP tool                 →  ToolDefinition
─────────────────────────────────────────────
name: "get_weather"      →  name: "mcp__{server}__get_weather"
title: "Weather"          →  label: "{server}/Weather"  (无 title 用 name)
description               →  description
inputSchema (JSON Schema) →  parameters  (直传,本就是 JSON Schema)
(无 risk 字段)            →  risk: RiskLevel::High
```

`execute` 闭包:
```
fn(ctx) -> Future<Result<ToolResult, ToolError>> {
    if ctx.aborted() { return error("aborted") }
    bridge.call(original_name, ctx.args)
      → 发 tools/call(id = self.next_id++)
      → reader 按 id 匹配,跳过 notification
      → result.content 映射成 Vec<ContentBlock>
    result.isError → ToolResult.is_error
}
```

### 6.4 JSON-RPC reader(核心差异点)

私有 bridge 靠行序("写一行读一行")。MCP 不行:tools/call 期间 server 可能先发 `notifications/progress`,list 期间可能发 `notifications/tools/list_changed`。**必须按 `id` 匹配 response,跳过 notification。**

```rust
async fn call(&self, name: &str, args: &serde_json::Value)
    -> Result<McpCallResult, McpError>
{
    let mut guard = self.inner.lock().await;
    let id = guard.next_id;
    guard.next_id += 1;
    write_jsonrpc_request(&mut guard.stdin, id, "tools/call",
                          json!({"name": name, "arguments": args})).await?;
    let deadline = tokio::time::Instant::now() + self.timeout;
    loop {
        let line = read_line_with_deadline(&mut guard.stdout, deadline).await?;
        let msg: JsonRpcMessage = serde_json::from_str(&line)?;
        match msg {
            // 有 id 的 = response(id=1/2/…的响应或错误的 notification id)
            JsonRpcMessage::Response(r) if r.id == id => return map_call_result(r.result),
            JsonRpcMessage::Error(e)   if e.id == id => return Err(map_jsonrpc_error(e)),
            // 无 id 或 id 不符 = notification(progress/log/list_changed/…),跳过
            JsonRpcMessage::Notification(_) | _ => continue,
        }
    }
}
```

> `next_id` 与 stdin/stdout 在同一 `Mutex` 下,保证"取 id → 写请求 → 读到对应 response"是原子序列,**不会**出现两个并发 call 交叉读写。这与 `ExternalToolBridge` 的 `roundtrip` 一致的锁策略。

**进度通知刷新超时(可选,首版不做):** 规范允许收到 `notifications/progress` 时重置超时时钟。首版采用固定 60s 超时(与私有 bridge 一致)。进度通知被当作普通 notification 跳过。

### 6.5 内容映射

```
MCP content item                 → ContentBlock
────────────────────────────────────────────────
{type:"text", text}              → ContentBlock::Text{text}
{type:"image", data, mimeType}   → ContentBlock::Image{image: ImageContent{data, mime_type:mimeType}}
其他类型                         → ContentBlock::Text{text: format!("{:?}", item)}
                                   (降级为文本描述,不丢信息)
```

> `ContentBlock::Image` / `ImageContent` 已存在于 `types/message.rs`(本次直接用上——这是首次有代码路径填充它)。空 content 数组 → 一条空 text(与私有 bridge 行为一致)。

## 7. 集成点

### 7.1 `gasket-host/src/lib.rs` 导出

```rust
pub mod mcp;
pub use mcp::{load_all_mcp, McpBridge, McpError, McpServerConfig};
```

新增 `load_all_mcp() -> Result<Vec<ToolDefinition>, McpError>`:读 `mcp.json`,对每个 server `McpBridge::spawn`,合并所有工具。单个 server 失败 → 该 server 的工具缺失,其余继续(像 `load_external_tools` 一样容错,不整体失败)。

### 7.2 CLI(`gasket-cli/src/main.rs`)

`main()` 里在 `load_external_from_env()` 之后加 `load_mcp_tools()`:
```rust
let mcp_tools = match load_all_mcp().await {
    Ok(t) => { eprintln!("(mcp tools: {})", t.len()); t }
    Err(e) => { eprintln!("(mcp tools load failed: {e})"); vec![] }
};
// …
let mut tools = built_in_tools();
tools.extend(ext_tools);
tools.extend(extra_tools);
tools.extend(mcp_tools);   // ← 新增
```

### 7.3 Gateway(`gasket-gateway/src/ws.rs`)

`handle_ws` 里 `extra_tools` 之后拼 `mcp_tools`:
```rust
let mcp_tools = load_all_mcp().await.unwrap_or_default();
let tools = {
    let mut t = built_in_tools();
    t.extend(extra_tools.iter().cloned());
    t.extend(mcp_tools.iter().cloned());   // ← 新增
    t
};
```

### 7.4 `gasket-core` 改动

**零。** MCP 是纯 host 层适配。

## 8. 错误处理

| 场景 | 行为 |
|---|---|
| `mcp.json` 不存在 | 静默,无 MCP 工具(正常首次使用) |
| `mcp.json` JSON 解析失败 | `McpError`,启动时报错 |
| 某个 server spawn 失败(命令不存在) | 该 server 工具缺失,log warn,其余继续 |
| `initialize` 超时/响应错误 | 该 server 工具缺失 |
| `tools/list` 返回空 | 该 server 贡献 0 个工具(不算错误) |
| `tools/call` 超时 | 工具返回 error tool_result(喂回 LLM,run 继续) |
| `tools/call` server 返回 isError | ToolResult.is_error=true,content 透传 |
| 子进程崩溃 | `read_line` 返回 EOF → McpError,后续 call 持续报错 |

错误类型用 `thiserror`:
```rust
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("call timeout after {0:?}")]
    Timeout(Duration),
    #[error("server returned error {code}: {message}")]
    ServerError { code: i64, message: String },
}
```

## 9. 测试策略

### 9.1 纯函数单测(无需网络/npm)

- **JSON-RPC 消息构造/解析**:`make_initialize`、`parse_jsonrpc_response`、id 匹配逻辑、notification 跳过。喂手写 JSON 字符串,断言解析结果。
- **内容映射**:`mcp_content_to_blocks` 对 text/image/未知类型的输出。
- **工具名前缀**:`mcp__{server}__{tool}` 拼接与冲突避免。
- **配置加载**:`load_config_from` 对缺失文件(空 vec)、空 `mcpServers`(空 vec)、合法配置(vec 正确)的处理。

### 9.2 集成测试(Python mock server)

仿照 `external_tool.rs` 的 `fixture_script()`,写一个 Python MCP server 脚本,实现:
- 响应 `initialize`(回 `2025-06-18` + tools capability)
- 响应 `tools/list`(返回一个 `echo` 工具)
- 响应 `tools/call`(回显参数)

测试:`McpBridge::spawn` 成功 → tools 非空 → `execute` 闭包返回正确结果。覆盖握手 + list + call 全链路。

**不依赖 npm/网络**——用 `python3`(外部工具测试已依赖它)。若 CI 无 python3,该测试标 `#[ignore]` 或用 `#[cfg]` 门控(与现有 external_tool 测试同策略)。

### 9.3 不测的

- 真实 MCP server(npm 包)接入——留给手动冒烟,放 `smoke_llm.rs` 同档的 ignore 测试。

## 10. 配置文件示例(`docs/mcp-example.json` 或 README 一节)

提供最小可复制示例(不含真实 token):
```json
{
  "mcpServers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
    }
  }
}
```

## 11. 验收标准

1. `cargo check --workspace --all-targets` 零错误零警告。
2. `cargo test --workspace` 全绿;新增的纯函数测试 + Python mock 集成测试通过。
3. 放一个合法 `~/.gasket/mcp.json`(指向真实 MCP server),启动 `gasket` CLI,agent 能列出并调用 MCP 工具(手动冒烟)。
4. `gasket-core` 改动行为零(只新增,不改既有类型语义)。
5. 内置工具、进程内扩展、私有外部工具的既有行为不受影响。

## 12. 非目标 / 后续

- modern era(`2026-07-28`)支持:probe + 回退。本次接口预留(legacy 是 `mcp.rs` 内部细节),后续加 modern 不改 `ToolDefinition` 契约。
- Streamable HTTP 传输。
- resources / prompts 原语。
- `tools/list_changed` 通知触发的热重载(本次静默忽略)。
- annotations(如 `readOnlyHint`)→ `RiskLevel` 映射(本次统一 High)。
