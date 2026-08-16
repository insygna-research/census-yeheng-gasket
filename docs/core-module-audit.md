# conga 内核模块归属审计(core module ownership audit)

> 审计对象:基座 crate `conga/conga/src/`(BASE 538c221,2026-08-16)。
> 目的:为「conga = 最小 agent-loop 底座 + built-in tools 走 Cargo feature」的重定位(T4/T5/T6)提供基于 file:line 证据的模块归属表。
> 方法:纯静态分析(grep / read)。审计期间 T6 正在并行修改 `conga/conga/Cargo.toml`、`src/lib.rs` 及下游 manifest(feature `built-in-tools` 已落地),本表按 BASE + 当前工作区状态记录,不依赖任何构建产物。

## 0. 结论速览

| 分类 | 文件数 | 模块 |
|---|---|---|
| **底座必需 core-required** | 20 | `lib.rs`, `agent_loop.rs`, `error.rs`, `extension/{mod,api}.rs`, `guard.rs`, `providers/{mod,anthropic,openai_compat,sse}.rs`, `proxy.rs`, `storage/mod.rs`, `subagent.rs`(trait 文件), `types/{mod,context,message,tool,event,session_event}.rs`, `test_util.rs`(cfg(test) 内部) |
| **host 专属 host-only** | 10 | `tools/*` 全部 10 个文件(mod/read/write/edit/bash/list/grep/fetch/sandbox/subagent)——仅被 host 层二进制(conga-host / conga-cli / conga-gateway / web/src-tauri)与 example/文档消费 |
| **待裁决 undecided** | 0 | 无:未发现任何「内核 + host 混用」的非 tools 模块 |

与控制器锁定裁决(progress.md §Controller rulings #2/#3)一致:`tools/*` 全量 10 文件进 `built-in-tools` feature;其余全部无条件保留。**未发现与裁决冲突的证据。**

## 1. 底座必需模块(core-required)

### 1.1 `lib.rs`(crate 根,54 行)

- **pub 项**:`pub mod` 声明 ×10(L7-16)+ 约 40 个 re-export(L20-44)+ `pub fn now()`(L49)。
- **使用点**:`now()` 被 conga-host/src/compact.rs:89、lib.rs:233、subagent.rs:147、tests/integration.rs:49、examples/{cli_host.rs:48, plugins.rs:36} 使用;`conga::RiskLevel` re-export 被 conga-host/src/lib.rs:31 再导出。
- **分类**:底座必需 —— 它就是无条件公共 API 面。
- **建议**:T6 已将 `pub mod tools`(L15)与 `pub use tools::built_in_tools` 包上 `#[cfg(feature = "built-in-tools")]`(lib.rs:30-31),其余不动。

### 1.2 `agent_loop.rs`(1974 行,单测自 L631 起)

- **pub 项**:`pub async fn run_agent_loop`(L26)、`pub async fn agent_loop`(L126)。
- **使用点(跨 crate)**:conga-host/src/subagent.rs:15-17(`run_agent_loop` + 各类型)、conga-host/src/config.rs:8-10;conga-gateway 经 conga-host 间接;web/src-tauri/src/chat.rs 经 conga-host 间接;examples/cli_host.rs:20-23、examples/plugins.rs:10-13、tests/plugins_example.rs:5-8(直接 `conga::agent_loop`,L74/L127)。
- **内部依赖**:`crate::guard::RepeatGuard`(L48、L226、L374)、`crate::now()`(L195/216/365)、`crate::extension::ExtensionApiImpl`(仅单测 L1920)、types/error/providers 零依赖 providers(只认 `StreamFn`)。**不引用 `crate::tools`**(grep `crate::tools` 在 tools/ 之外零命中)。
- **分类**:底座必需 —— 循环本体。
- **建议**:原样保留,与 tools 解耦已天然成立。

### 1.3 `error.rs`(45 行)

- **pub 项**:`pub enum AgentError`(L9)、`pub enum ToolError`(L32)+ `From<ToolError>`(L41)。
- **使用点**:crate 内 types/context.rs:8、types/tool.rs:10、agent_loop.rs:8;跨 crate:conga-host/src/config.rs:9、lib.rs:8、session.rs:12;conga-ext/src/search.rs:16(`ToolError`)。
- **分类**:底座必需 —— types/loop 契约的一部分。
- **建议**:无条件保留。

### 1.4 `extension/`(mod.rs 11 行 + api.rs 177 行)

- **pub 项**:`ExtensionApi` / `ExtensionApiImpl`(api.rs:35/48)、`BeforeToolCallHandler`(L13)、`AfterToolCallHandler`(L25);mod.rs:10 re-export。
- **使用点(跨 crate,即 conga-ext 的全部入口)**:conga-ext/src/lib.rs:13、hello.rs:5、permission_gate.rs:3-4、search.rs:16、terminal.rs:10-12、todo.rs:7;conga-cli/src/main.rs:19(`ExtensionApiImpl`);web/src-tauri/src/chat.rs:329(`conga::ExtensionApiImpl::new()`);examples/plugins.rs:9;tests/plugins_example.rs:6、L40/85/120。types/tool.rs:150-153 的文档注明 `HookChain` 的具体实现是 `ExtensionApiImpl`(类型层耦合,非 use)。
- **分类**:底座必需 —— 无条件公共 API 面(扩展契约本身)。
- **建议**:无条件保留。

### 1.5 `guard.rs`(76 行)

- **pub 项**:`pub struct RepeatGuard`(L10)、`pub fn repeat_advisory`(L42)。
- **使用点**:**仅** agent_loop.rs:48、226、L374(+ 本文件单测 L55-75)。全仓 grep 无任何跨 crate 使用。
- **分类**:底座必需 —— 循环内部卫生组件(不是 host 专属:它只被 loop 用)。
- **建议**:无条件保留(控制器裁决 #2 已列名)。

### 1.6 `providers/`(mod.rs 221 行 + anthropic.rs 646 行 + openai_compat.rs 575 行 + sse.rs 202 行)

- **pub 项**:`ProviderConfig`(mod.rs:26)、`ConfigError`(mod.rs:40)、`AnthropicProvider`(anthropic.rs:25)、`OpenAiCompat`(openai_compat.rs:24)、`pub fn parse_sse_frame`(sse.rs:15);`SseFrameSplitter` 为 pub(crate)(sse.rs:63)。
- **使用点(跨 crate)**:conga-host/src/config.rs:10(`ProviderConfig`);examples/cli_host.rs:21-22(两个 provider);sse 仅被本目录 anthropic.rs:123/130、openai_compat.rs:116/123 使用(零跨 crate)。
- **依赖**:`reqwest`(anthropic.rs:28/36、openai_compat.rs:27/35、mod.rs:35/48/51/101-116)、`async_stream`(anthropic.rs:93、openai_compat.rs:87)、`futures_util`(L12/L11/L6)。
- **分类**:底座必需 —— `StreamFn` 的官方实现。
- **建议**:无条件保留(reqwest 因此不可变成 optional —— 与裁决 #2 的注释一致)。

### 1.7 `proxy.rs`(190 行)

- **pub 项**:`set_tool_proxy`(L18)、`tool_proxy`(L34)、`validate_tool_proxy`(L52)、`apply_tool_proxy`(L79);`pub(crate) mod test_util`(L97,全局 override 测试锁)。
- **使用点(全部跨 crate 用户,证据见 §3 Q2)**:conga-ext/src/search.rs:539(`apply_tool_proxy`)、815/837(`set_tool_proxy`,单测);conga-host/src/mcp.rs:448(`tool_proxy`)、1215/1223/1229(`set_tool_proxy`,单测);web/src-tauri/src/lib.rs:135(`set_tool_proxy`)、176(`validate_tool_proxy`)。crate 内:tools/fetch.rs:52/100/396/410(feature 内)。
- **分类**:底座必需 —— conga-ext(host 的下层)直接依赖,挪去 host 会倒置分层(裁决 #2)。
- **建议**:无条件保留。

### 1.8 `storage/mod.rs`(1216 行,单测自 L710 起)

- **pub 项**:`config_dir`(L42)、`is_valid_session_id`(L57)、`JsonlStorage`(L75)、`SessionMeta`(L422)、`EventStorage`(L434)。
- **使用点(跨 crate)**:conga-host/src/session.rs:12、session_index.rs:12、skills.rs:25(`storage::config_dir`)、mcp.rs:107;conga-gateway/src/api.rs:335、main.rs:67、ws.rs:609;web/src-tauri/src/lib.rs:12/17/105/106/122(`JsonlStorage`/`EventStorage`/`storage::config_dir`)。crate 内:仅 tools/mod.rs:134(`resolve_read_path` → `crate::storage::config_dir()`;将随 tools 一起进 feature)。
- **分类**:底座必需 —— 会话持久化契约(events.jsonl)。
- **建议**:无条件保留;`tools/mod.rs` 对它的引用随 feature 一起被 gate,不构成反向依赖。

### 1.9 `subagent.rs`(trait 文件,121 行)

- **pub 项**:`SubagentSpawn`(L14)、`SubagentResult`(L20)、`SubagentEvent`(L34,10 变体)、`trait SubagentSpawner`(L90)、`NoopSubagentSpawner`(L99)。
- **使用点**:见 §3 Q3(types/context.rs:25、types/tool.rs:86、tools/subagent.rs:63、host/gateway/web 各处)。
- **分类**:底座必需 —— `AgentContext.spawner` / `ToolContext.spawner` 字段的类型即来自此文件,移除即破坏 types 契约。
- **建议**:无条件保留(裁决 #2)。

### 1.10 `types/`(mod.rs 7 行 + context.rs 300 行 + message.rs 385 行 + tool.rs 167 行 + event.rs 79 行 + session_event.rs 291 行)

- **pub 项(主要)**:context.rs — `AgentContext`(L16,含 `spawner` 字段 L25)、`AgentLoopConfig`(L43)、`RetryPolicy`(L93)、`AgentTunables`(L136)、`ModelSpec`(L192)、`ProviderApi`(L201)、`ThinkingLevel`(L210)、`StreamChunk`(L220)、`trait StreamFn`(L245);message.rs — `AgentMessage`(L12)、`UserMessage`(L47)、`AssistantMessage`(L55)、`ToolResultMessage`(L182)、`CustomMessage`(L193)、`ContentBlock`(L202)、`ImageContent`(L225)、`ToolCall`(L231)、`FunctionCall`(L237)、`StopReason`(L246)、`ModelId`(L261)、`Usage`(L265);tool.rs — `RiskLevel`(L18)、`ToolDefinition`(L28)、`ToolFn`(L50)、`ToolCallCtx`(L58)、`ToolContext`(L78,含 `spawner` 字段 L86)、`ToolResult`(L103)、`ToolCallVerdict`(L130)、`trait HookChain`(L154);event.rs — `AgentEvent`(L14)、`ContentDelta`(L71);session_event.rs — `SessionEvent`(L15)、`TurnEndReason`(L35)、`CancelCause`(L44)、`derive_messages`(L69)、`repair_unanswered_tool_calls`(L88)。
- **使用点**:全部 5 个下游 crate 大面积使用(抽样:conga-host/src/lib.rs:8-10、config.rs:8-10、event_map.rs:8、hooks.rs:7、permission.rs:7、printer.rs:4、compact.rs:9、session.rs:12、external_tool.rs:18、mcp.rs:18-19、subagent.rs:15-18、session_index.rs:12;conga-gateway/src/api.rs:12/334-335、ws.rs:15/609;conga-cli/src/main.rs:7;conga-ext 各文件;web/src-tauri/src/chat.rs:22/48、lib.rs:12/63/74)。
- **分类**:底座必需 —— 数据模型即内核。
- **建议**:无条件保留,`spawner` 字段随 subagent.rs 一起保持(裁决 #2)。

### 1.11 `test_util.rs`(14 行,`#[cfg(test)] pub(crate)`,非公开模块)

- **pub 项**:`pub(crate) fn fake_env`(L6)。
- **使用点**:proxy.rs:111、providers/mod.rs:145、types/context.rs:259。
- **分类**:底座必需(测试基础设施,不出 crate)。
- **建议**:不动。注意与 `proxy.rs:97` 内嵌的 `pub(crate) mod test_util`(LOCK)是两个东西,后者留在 proxy.rs 内即可。

## 2. host 专属模块(host-only,11 个文件 → `built-in-tools` feature)

目录 `tools/` 全部 11 个文件。共同点:crate 外**没有任何代码直接 use `tools::` 子模块或其 pub(crate) 辅助函数**——下游一律只经 `conga::built_in_tools()`(mod.rs:18,lib.rs:30-31 re-export)拿工具。grep `conga::tools::` 全仓零命中(命中的两处在 tools/ 内部自测:fetch.rs:244-251、grep.rs:4 文档链接)。

| 文件 | 行数 | pub 项 | crate 外使用点(全部经 built_in_tools) |
|---|---|---|---|
| `tools/mod.rs` | 337 | `built_in_tools()`(L18);`pub mod` ×9(L3-11) | conga-cli/src/main.rs:36;conga-host/src/subagent.rs:451、tests/integration.rs:111/178/248/343/561、tests/smoke_llm.rs:36/78;conga-gateway/src/ws.rs:292;web/src-tauri/src/chat.rs:333;examples/cli_host.rs:39;docs/plugin-tutorial.md:30 |
| `tools/read.rs` | 179 | `tool()`(L11) | 仅经 built_in_tools |
| `tools/write.rs` | 130 | `tool()`(L8) | 仅经 built_in_tools |
| `tools/edit.rs` | 326 | `tool()`(L14) | 仅经 built_in_tools |
| `tools/bash.rs` | 356 | `tool()`(L12) | 仅经 built_in_tools |
| `tools/list.rs` | 233 | `tool()`(L22) | 仅经 built_in_tools |
| `tools/grep.rs` | 395 | `tool()`(L26) | 仅经 built_in_tools |
| `tools/fetch.rs` | 425 | `tool()`(L12) | 仅经 built_in_tools |
| `tools/sandbox.rs` | 226 | pub(crate):`sandbox_enabled`(L22)、`confine`(L29) | **唯一**调用方 tools/bash.rs:48-49 |
| `tools/subagent.rs`(工具) | 209 | `tool()`(L9) | 仅经 built_in_tools(mod.rs:27) |

`mod.rs` 内的 pub(crate) 辅助(`MAX_OUTPUT_BYTES` L32、`truncate_output` L37、`spill_or_truncate` L55、`resolve_within_cwd` L88、`resolve_read_path` L133、`resolve_read_path_in` L138)使用点全部在 tools/ 内部:bash.rs:72-73、edit.rs:47、fetch.rs:88/244-251、grep.rs:73、list.rs:49、read.rs:40、write.rs:37、mod.rs 自测。conga-ext/src/terminal.rs:253-255 的注释明确佐证:「core's spill_or_truncate is pub(crate) and not exported, so ext returns the drained text directly」——ext 想用都用不了。

- **分类**:host 专属(11/11)。
- **建议**:全部进 `built-in-tools` feature(T6 已执行:`built-in-tools = ["dep:ignore","dep:glob","dep:regex","dep:dom_query","dep:url"]`,Cargo.toml:65;`sandbox-landlock` 维持只管 `dep:landlock`,Cargo.toml:67)。`tools/subagent.rs` 工具对 `crate::subagent::NoopSubagentSpawner`(tools/subagent.rs:63)与 `crate::proxy`(fetch.rs:52/100)的引用都是 tools→core 的正向依赖,gate 后不产生反向问题。

## 3. 六个具体问题(逐条 file:line 证据)

### Q1:`tools/` 之外是否有人用 `tools::sandbox` 或 `mod.rs` 辅助函数?

**没有。**`tools::sandbox` 的全部调用点在 tools/bash.rs:48-49;`resolve_read_path`/`resolve_within_cwd`/`resolve_read_path_in`/`spill_or_truncate`/`truncate_output`/`MAX_OUTPUT_BYTES` 的全部调用点在 tools/{read.rs:40, write.rs:37, edit.rs:47, grep.rs:73, list.rs:49, fetch.rs:88+244+251, bash.rs:72-73} 与 mod.rs 自测(均为 pub(crate),外部本就不可见)。conga-host/src/skills.rs **不**使用它们(它只调 `conga::storage::config_dir()`,skills.rs:25,自己扫目录);conga-ext/src/terminal.rs:253-255 注释确证 ext 拿不到 spill_or_truncate。⇒ 整个 gate 无需为这些 helper 留任何无条件出口。

### Q2:`proxy.rs` 的全部跨 crate 用户

| 用户 | 符号 | 证据 |
|---|---|---|
| conga-ext/src/search.rs | `apply_tool_proxy` | L539 |
| conga-ext/src/search.rs(单测) | `set_tool_proxy` | L815、L837 |
| conga-host/src/mcp.rs | `tool_proxy` | L448 |
| conga-host/src/mcp.rs(单测) | `set_tool_proxy` | L1215、L1223、L1229 |
| web/src-tauri/src/lib.rs | `set_tool_proxy` | L135 |
| web/src-tauri/src/lib.rs | `validate_tool_proxy` | L176 |
| conga/conga/src/tools/fetch.rs(feature 内) | `apply_tool_proxy` / `tool_proxy` / `set_tool_proxy` | L52 / L100 / L396+410 |

无第四个外部 crate;conga-cli 与 conga-gateway 不直接使用 proxy 符号(gateway 经 conga-host)。⇒ proxy.rs 必须无条件(裁决 #2 成立:ext 在 host 下层,不能依赖 host)。

### Q3:`subagent.rs`(trait 文件)的引用者

确认:`types/context.rs:25`(`AgentContext.spawner: Option<Arc<dyn crate::subagent::SubagentSpawner>>`)与 `types/tool.rs:86`(`ToolContext.spawner`,同类型)均引用。其余引用者:(a) `tools/subagent.rs:63`(`NoopSubagentSpawner` 兜底,随 tools 进 feature);(b) conga-host/src/subagent.rs:15-18(`SubagentSpawner`/`SubagentSpawn`/`SubagentResult`/`SubagentEvent` 全量 import)+ :75(`impl SubagentSpawner for HostSubagentSpawner`)+ conga-host/src/lib.rs:90/165(字段与 `with_spawner`);(c) conga-host/src/event_map.rs:70-71(`SubagentEvent`);(d) conga-gateway/src/ws.rs:22(`HostSubagentSpawner`)、317/352-354(`conga::SubagentEvent`);(e) web/src-tauri/src/chat.rs:48/175/352-354(`conga::SubagentEvent`)。⇒ trait 文件留在底座,否则 types 契约(两个 spawner 字段)先碎。

### Q4:是否存在「仅 host/cli/gateway 使用」的非 tools 内核模块?

**没有。**非 tools 模块分两类:(1) 循环内部件,crate 外零使用 —— `guard.rs`(仅 agent_loop.rs:48/226/374)、`providers/sse.rs`(仅 anthropic.rs:123/130、openai_compat.rs:116/123)、`test_util.rs`(cfg(test));(2) 公共契约面,被 conga-ext(host 下层)或多层消费 —— `error`、`types`、`extension`、`proxy`、`storage`、`providers`(host)、`subagent`(trait)、`agent_loop`(host/examples)。没有任何非 tools 模块的用户集合 ⊆ {host, cli, gateway}。⇒ T5 没有可搬的「漏网」模块;重定位的全部搬运面就是 `tools/*`。

### Q5:哪些 workspace 依赖只被 `tools/*` 使用(可转 optional)?

| 依赖 | 非 tools/src 使用点 | 结论 |
|---|---|---|
| `ignore` | 无(list.rs:9、grep.rs:13) | ✓ 已 optional(Cargo.toml:45) |
| `glob` | 无(list.rs:57、grep.rs:77/193) | ✓ 已 optional(:48) |
| `regex` | 无(grep.rs:14,自测 L335/358) | ✓ 已 optional(:51) |
| `dom_query` | 无(fetch.rs:172) | ✓ 已 optional(:53) |
| `url` | 无(fetch.rs:108;providers 不用 url crate) | ✓ 已 optional(:30) |
| `landlock` | 无(sandbox.rs,仅 Linux) | 本就 optional(sandbox-landlock,:57/67) |
| `reqwest` | providers(anthropic.rs:28/36、openai_compat.rs:27/35、mod.rs:35/48/51/101-116)+ proxy.rs:60/79-88 | 不可 optional(裁决 #2) |
| `dirs` | storage/mod.rs:43 | 保留 |
| `futures-util` | agent_loop.rs:6、providers、types/context.rs:253(`StreamFn` 返回类型) | 保留 |
| `async-stream` | anthropic.rs:93、openai_compat.rs:87 | 保留 |
| `uuid` | **src/ 零使用**;唯一使用点是 examples/cli_host.rs:42 | ⚠ 见 §4 意外发现 |
| `anyhow` | **整个 crate 零使用**(grep 仅命中 Cargo.toml:22) | ⚠ 见 §4 意外发现 |

### Q6:`tests/plugins_example.rs` 与 `examples/*` 用不用 `built_in_tools()`?

- `conga/conga/tests/plugins_example.rs`:**不用**。它自带工具:hello 经 `conga_ext::hello::register`(L41,L42 取走 api.tools),bash 为内联 `ToolDefinition`(L87-94)。gate 后无需改此测试(其 conga 依赖经 dev-dependencies,若不开 feature 也不受影响)。
- `conga/conga/examples/plugins.rs`:**不用**。工具全部来自 `conga_ext::register_all`(L22/L27)。
- `conga/conga/examples/cli_host.rs`:**用** —— L39 `tools: conga::built_in_tools()`。⇒ T6 已为它加 `[[example]] required-features = ["built-in-tools"]`(Cargo.toml:69-71),正确。

## 4. 意外发现(超出六问)

1. **`anyhow` 是死依赖**:`conga/conga/Cargo.toml:22` 声明,但整个 crate(src/examples/tests)零 import(grep `anyhow` 仅命中 manifest 本身)。与 feature-gate 无关,但既然在做 manifest 瘦身,T5/控制器可顺手移除(超出本审计范围,仅记录事实)。
2. **`uuid` 只被 example 使用**:运行时依赖(Cargo.toml:37),src/ 零使用,唯一使用点 examples/cli_host.rs:42(`uuid::Uuid::new_v4`)。若追求「底座最小」,可降级为 dev-dependency(同样仅记录,不属 T4/T6 契约)。
3. **`tools/mod.rs` 对 `storage::config_dir` 的引用**(mod.rs:134)是 tools→core 的唯一 storage 纽带,随 tools 整体 gate,方向安全(core 永不反向引用 tools,grep 证实 agent_loop/types/providers/storage/proxy/subagent 中 `crate::tools` 零命中)。
4. T6 与本审计并行进行中:工作区已可见 `built-in-tools` feature(Cargo.toml:59-71)与 lib.rs:30-31 的 cfg gate;本审计的分类与之相互印证,无冲突。
