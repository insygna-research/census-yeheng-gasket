# gasket 重定位设计：极简 Agent Loop 底座

Date: 2026-08-16
Status: **Draft — 待 review**

## 0. 本文档的用途

把「gasket 从『个人 AI 助手框架』重定位为『极简 agent loop 底座』」的全部问题、证据、
决策与任务规划固化为一份可 review 的 spec。**本文只做决策，不含实现级步骤**；
批准后按 `writing-plans` 流程逐阶段转成实现 plan（保存至 `docs/superpowers/plans/`）。

**待用户裁决的开放决策点集中在 §8**，每项带推荐选项。

---

## 1. 目标

把 gasket 的定位从「轻量级、可自托管的个人 AI 助手框架」改为：

> **极简 agent loop 底座（Rust）**——上层用最少代码（目标：50 行）实现自己的 agent：
自定义工具、自定义策略、事件流回调、可注入任意 LLM。

产品（CLI / gateway / web 前端）不删除、不降功能，仅在仓库叙事与分层上降级为
**参考实现（reference apps）**。

一句话：底座发到 crates.io 当库卖，产品留在仓里当范例养。

---

## 2. 市场证据（已核实，2026-08-16）

| # | 证据 | 事实 | 含义 |
|---|---|---|---|
| E1 | [earendil-works/pi](https://github.com/badlogic/pi-mono)（原 badlogic/pi-mono） | 90,891 stars，MIT；分层 `pi-ai`（统一 LLM API）→ `pi-agent-core`（agent 运行时）→ `pi-coding-agent`（CLI 产品）→ `pi-tui` | 「极简 agent loop 底座」在 TS 生态已被验证到极限 |
| E2 | npm API 实测 | `@mariozechner/pi-agent-core` 近一月下载 **2,784,249** 次 | 需求量级证据 |
| E3 | [rig](https://rig.rs) / [swiftide](https://swiftide.rs) | rig 8k+ stars、2.1M+ 下载（production 框架路线：RAG、20+ provider）；swiftide ~715 stars（RAG 索引管线路线） | **Rust 生态「极简 loop 底座」生态位空缺**；rig/swiftide 均不构成直接竞争 |
| E4 | smolagents（Hugging Face，2025-01） | Python 侧 minimal-loop 路线起量 | 跨语言验证需求真实 |
| E5 | [crates.io/gasket](https://crates.io/crates/gasket) | 名字已被占用：actor 模型数据管线库 | 发布必须换名或用 `gasket-core` 词干 |
| E6 | [GoDaddy Gasket](https://gasket.dev)（v7） | JS「framework-maker」，语义恰为「造框架的框架」 | 搜索发现性撞车，持续给对方引流 |

**结论**：定位不是想象出来的问题——三个语言生态验证过，Rust 生态位空缺，且有先发窗口。

---

## 3. 现状事实核查（代码级，2026-08-16 复核）

### 3.1 体量 vs 定位

- 全仓 Rust **20,439 行** + 完整 Vue 3 / Vite / Tauri 前端 + WS 网关 + MCP + subagent 编排。
- loop 内核（`agent_loop.rs` 实现部分）约 **620 行**（其余 1,057 行是测试），加 `types/` 约 900 行。
- **底座 ≈ 1.5k 行，产品重力 ≈ 19k 行。** 当前仓库挂「极简」招牌，事实与宣称不符。

### 3.2 `review.md`（2026-08-16 早前审查）致命问题复核

| review.md # | 问题 | 现状 | 证据 |
|---|---|---|---|
| 1 | Anthropic 角色不交替 → HTTP 400 | **已修复** | `anthropic.rs:268-274` 同角色折叠归一化；4 个测试覆盖多工具调用 / compaction 插入 / 连续 assistant / 单轮回路 |
| 2 | `run_engine` 嵌套 runtime | **已修复** | `session_index.rs:107` `reindex` 已退化为纯同步函数；`api.rs:208/223` 标准 `spawn_blocking` 调用 |
| 3 | `terminal.rs` REGISTRY 泄漏 | **已修复** | `terminal.rs:305` `reap_dead_sessions()`（run 前扫除）+ read 时回收（`:272-274`）+ 两个专项测试 |
| 4 | `append_tool_call` 盲匹配 | **仍开放** | `message.rs:103-113`：无 id 时 `rev().find_map` 挂最后一条 ToolCall；并发/交替流式分片会把 B 工具参数追加到 A 工具上。**静默数据污染，比崩溃更糟** |

### 3.3 结构缺陷（对照 pi 源码发现）

**D1：loop 缺少每次 LLM 调用前的变换接缝。**
compaction 只在 `host::run_turn` 开头跑一次；loop 内部（上限 50 轮 × 20 工具调用/轮）
期间无任何压缩时机，**长 run 会中途撑爆上下文**。pi 的 `transformContext` 正是此接缝
（见 §5 裁决表 R3）。

**D2：core 混入产品重力。**
`subagent.rs`、`proxy.rs`、`tools/sandbox.rs`、权限语义、8 个内置工具（约 3,400 行）
都在 `gasket-core`。底座每一行都是 API 承诺——不切，产品每次演进都是潜在 semver 破坏。

---

## 4. 问题清单（按严重度排序）

| # | 严重度 | 问题 | 对应任务 |
|---|---|---|---|
| F1 | 阻断发布 | `append_tool_call` 无 id 分片盲匹配（§3.2 #4） | T2 |
| F2 | 阻断发布 | 命名未定：crates.io `gasket` 已被占（E5），GoDaddy 撞名（E6） | T1 |
| F3 | 阻断定位 | loop 无 per-LLM-call 变换接缝（D1） | T3 |
| F4 | 阻断定位 | core 混入产品重力（D2） | T4-T6 |
| F5 | 叙事债务 | README 首屏是网关启动命令，不是库 Quick Start | T9 |
| F6 | 质量信号 | 底座无独立 CI 质量门 | T8 |

---

## 5. 决策记录：参考 pi 的裁决（核心决策，需 review）

gasket 本就自称 "pi-style"（`gasket-core/src/lib.rs:1`）。裁决原则：**抄接缝与分层纪律，
不抄 provider 帝国与包数量。**

| # | pi 机制 | gasket 现状 | 裁决 | 理由 |
|---|---|---|---|---|
| R1 | `AgentMessage` 贯穿全程，只在 LLM 边界转 wire 格式 | 已有，同源设计 | ✅ 保持 | — |
| R2 | loop 不管持久化（harness 管） | **gasket 领先**：`persist` 回调 + crash-safe 顺序（Assistant 先于工具执行落盘） | ✅ 保持 | 这是 gasket 对 pi 的差异点，不回退 |
| R3 | `transformContext`：每次 LLM 调用前 `AgentMessage[]→AgentMessage[]` | **缺失**（D1） | ✅ **抄**（T3） | 结构特例根源：compaction 从「host 的特殊时机」变「上层的普通回调」；MCP/脱敏/审计可挂同一接缝 |
| R4 | `getSteeringMessages` / `getFollowUpMessages`（运行中插话 / 停前续跑） | 缺失，gateway 靠 abort+重发绕 | ⏸️ **缓抄**（§8-D2） | YAGNI：等参考应用真需要再加；加则两个一起（同一机制两面） |
| R5 | `prepareNextTurn` 轮间换 model/thinking | 缺失 | ❌ 不抄 | 两 provider 的底座无此压力 |
| R6 | `shouldStopAfterTurn` 谓词式停止 | 用 max_turns / max_tool_calls 硬顶 | ❌ 暂不抄 | 硬顶更笨更清晰 |
| R7 | provider 广度（30+） | 2 个 + `StreamFn` trait | ❌ 不抄 | 「统一 LLM API」是 rig 的地盘（8k stars）；拼广度必死，拼「loop + 事件日志底座」才空 |
| R8 | 独立 `pi-ai` 包 | providers 在 core 内 | ❌ **不拆** `gasket-ai` | pi 拆 `pi-ai` 因 30+ provider 自成产品；2 个 provider 拆出去是空壳，纯增发布负担 |
| R9 | TS 运行时动态扩展加载 | 编译期 in-process crate | ✅ gasket 的对 | Rust 动态加载是自虐 |
| R10 | 权限系统不进 core | 权限语义散在 core 工具层 | ✅ **抄此原则** | 即 T4-T6 瘦身方向 |
| R11 | usage + cost 双追踪 | 有 usage 无 cost | ⏸️ 可选，不入主线（§8-D5） | 加一列成本表的事，不是结构问题 |

---

## 6. 规划任务列表

依赖链：T1 ∥ T2 → T3 → T4 → T5/T6 → T7/T8 → T9/T10。

### T1 命名决策（P0）

> **已完成（2026-08-16）**：定名 `conga` 并全局执行完毕（见 §8-D1 执行记录）。
> crates.io 占位注册随 D4（暂不发布）延后——FREE 状态窗口风险自担。


- **目标**：敲定 crates.io 发布名，一次到位。发布后改名 = 破坏 userspace；发布前 = 免费。
- **范围**：workspace `Cargo.toml` package name、README/docs/CI badge、Docker 镜像名。
  **不动** `events.jsonl` 格式、`~/.gasket/` 目录、gateway WS/REST 协议。
- **产出**：≤10 行决策记录合入 docs（候选、取舍、结论）。
- **验收**：目标名确认可注册（crates.io 页面 404 或 `cargo publish --dry-run` 通过）。
- **开放选项**：见 §8-D1。

### T2 修复 `append_tool_call` 盲匹配（P1 地基）
> **已完成（2026-08-16）**：TDD 执行，11 套件全绿（conga lib 156→165），fmt/clippy/
> src-tauri check 全绿。实施超出原案——排查中发现 **Anthropic 流式工具调用本就是坏的**：
> `parse_anthropic_chunk` 没有 `content_block_start` 分支，`tool_use` 的 id/name 从未
> 进入累积消息（每次工具调用都执行为 unknown tool）。同一接缝一并修复。
> 最终方案比原案「打开指针」更对：捡回 OpenAI 协议自带的 `tool_calls[].index`
> 路由键——`StreamChunk::ToolCallDelta` 增加 `index: Option<u32>`，
> `AssistantMessage` 增加 `#[serde(skip)] stream_indices` 路由表（磁盘格式零变化，
> 有序列化不变量测试锁定）。三条路由统一为一张查找表：index（OpenAI 并行）→
> id（Anthropic start）→ last（顺序续片，行为不变有回归锁定）。
> 变异测试证明新测试咬得住旧 bug（临时回退旧路由 → `two calls, not one merged blob` 红）。

- **目标**：无 id 的 tool 参数 delta 永远路由到正确的 ToolCall。
- **方案**：统一「按 id 找」与「按序找」两个特例——维护「当前打开的 ToolCall」指针
  （最后一条 `opened` 且未 closed 的），无 id delta 归属之；新 tool call 分片开启新指针。
  若 provider 层能回填 id（OpenAI 流式首片带 id），优先 provider 层回填，内核保持简单。
- **测试**：单工具多片（回归）；双工具交错出片 `A1 B1 A2 B2`（断言各自参数完整）；
  无 id 首片行为固化。`cargo test -p gasket-core` 全绿。
- **红线**：不动 `AgentMessage`/`SessionEvent` 序列化格式；内核不引入 per-provider 特例。

### T3 loop 增加 `transform_context` 接缝（P1 地基，R3 的兑现）

> **已完成（2026-08-16）**：TDD 执行，全绿（conga lib +3 测试；host integration +1；
> 11 套件全绿，fmt/clippy/src-tauri check 绿）。
> - 核心：`AgentLoopConfig.transform_context: Option<Arc<dyn Fn(&[AgentMessage]) -> Result<Vec<AgentMessage>, AgentError>>>`；
>   在**每次逻辑 LLM 调用前**于重试循环外计算一次（重试复用同一视图，避免非确定）；
>   `Err` 经新 `AgentError::ContextTransform` 变体 fail-loud。
> - **关键不变量（有测试锁定）**：transform 只是 **wire view**——loop 累积器、返回值、
>   persist 落盘事件永远是全量未压缩历史（`transform_context_never_touches_accumulator_or_persisted_events`）。
> - host 迁移：`run_turn` 的一次性 `budget.compact(&history)` **已删除**，
>   compaction 走接缝在每次 LLM 调用前执行（`config.transform_context` 由 run_turn 注入）。
>   中途增长也被约束（`run_turn_compacts_before_every_llm_call`：同轮第 3 次调用
>   wire ≤ 预算上限 + `[compacted N]` 通知首条 + 日志全量 assistant）。
> - 变异测试：闭包换 no-op → `third call must be compacted, got 5` 红；恢复绿。
> - D2 裁决遵守：未加 steering/follow-up 回调。


- **目标**：`AgentLoopConfig` 新增可选回调，loop 在**每次 LLM 调用前**应用。
- **签名方向**（最终以实现 plan 为准）：
  ```rust
  // AgentLoopConfig 新增字段
  pub transform_context: Option<Arc<dyn Fn(&[AgentMessage]) -> Result<Vec<AgentMessage>, AgentError> + Send + Sync>>,
  ```
- **不变量**：回调不得破坏 tool_call/tool_result 配对——配对完整性由既有
  `repair_unanswered_tool_calls` 防线兜底；`Err` 沿用 fail-loud。
- **消灭特例，不是加一个再留一个**：host 的 compaction 迁移到此接缝后，
  **删除** `run_turn` 开头的一次性 compaction。
- **测试**：mock provider 下构造超预算长历史，断言每次 LLM 调用前消息被压回预算内；
  无回调时行为与现状逐字节一致（零破坏回归）。

### T4 依赖审计：core 产品重力清单（P2）

- **目标**：产出基于引用事实的模块归属表：core 每个 pub 项 →「底座必需 / host 专属 / 待裁决」。
- **方法**：`cargo tree` + 跨 crate 引用统计（host/cli/gateway/ext 各 import 什么）。
  「待裁决」项列移动成本与建议。
- **产出**：文档合入 `docs/`。**审计期间不改代码。**

### T5 core 瘦身：产品重力移出（P2，R10 的兑现）

- **目标**：`gasket-core` 只留 loop、types、providers（`StreamFn` + OpenAI 兼容 + Anthropic）、
  storage（`EventStorage`）、extension API。`subagent`/`proxy`/`tools/sandbox` 按 T4 结论
  移入 `gasket-host`（R8：**不拆新 crate**）。
- **方案**：纯移动 + 可见性调整，不改逻辑。内部仓库无外部用户，调用方直接改干净，
  不留 re-export 过渡层。
- **验收**：`cargo build -p gasket-core --no-default-features` 独立编译通过；
  `cargo tree -p gasket-core` 无纯产品重力依赖；全量测试绿。
- **红线**：纯移动重构，禁止顺手改逻辑/重命名 pub 符号；`HookChain` 留在 core
  （loop 的接缝，不是产品重力）；`EventStorage::open_or_migrate` 保留（保护已有磁盘数据）。

### T6 内置工具集降级为参考层（P2）

- **目标**：core 默认编译面 = loop + types + providers + storage；8 个内置工具
  （约 3,400 行）收进 `built-in-tools` feature，host 开启。
- **方案**：`gasket-core` `default = []`；`ignore`/`glob`/`regex` 等重依赖挂 feature 下；
  留一个 ~20 行 mock 工具作文档示例。
- **验收**：无 feature 的 core 编译通过且依赖树最小；host/cli/gateway 行为零变化。
- **红线**：不删任何工具实现，只改归属；不改 `ToolDefinition` pub API。

### T7 双示例（P3）——发布部分已冻结（D4，2026-08-16）

> **D4 裁决影响**：crates.io 发布、semver、`cargo add` 实测验收**冻结**，解冻由用户定。
> 本任务只保留示例部分。注意：crate 名已因 D1 全局改名为 `conga`，下文 `gasket-core` 字样
> 均指 `conga`（底座 crate）。
- **示例**（同时是重定位的**硬验收标准**与 dogfood）：
  - `examples/minimal_agent.rs`：自定义 1-2 个工具 + provider，**≤50 行**跑通完整 loop 含事件流。
    写示例时暴露的 API 别扭处**回修 core**，不在示例里绕。
  - `examples/compacting_agent.rs`（~30 行）：用 `transform_context` 实现自定义压缩策略——
    T3 新接缝的活文档。
  - `examples/mock.rs`：CI 可跑版本（无网络）。
- **验收**：`cargo add <name>` + README 示例在**干净临时目录实测**编译运行成功（不是「应该可以」）；
  README Quick Start 与示例代码逐字一致；`cargo publish --dry-run` 零 warning。
- **红线**：T2/T3/T5/T6 未完成不发布；不带 TODO/占位 API 发布；
  不为示例顺手加「便利 API」。

### T8 CI 拆分：底座独立质量门（P3）

- **目标**：新增底座独立 job：`cargo test -p <core> --all-features` +
  `cargo clippy -p <core> --all-features -- -D warnings`，不依赖 host/gateway/web。
- **验收**：底座 job 绿；故意引入编译错误能使其红（实测，非推断）。
- **红线**：不动现有全仓 CI 触发条件。

### T9 README 与架构文档重写叙事（P4）

- **结构**：一句话定位（pi-agent-core 的 Rust 对应物）→ 50 行 Quick Start（T7 示例）→
  crate 三档分层表（底座 / 带电池 host / 参考应用）→ **拒绝清单**（不做 RAG、不做
  orchestration 框架、不做 UI、不做 20 provider——rig/swiftide 的地盘）→ 参考应用一节
  （cli/gateway/web 标注 *reference apps*）。
- **红线**：「极简」宣称必须能被 T6 后的依赖树与代码行数**支撑**；不删参考应用文档，
  只降级位置。

### T10 参考应用降级标注与收尾（P4）

- **目标**：cli/gateway/ext/web 在目录说明、docs/usage.md、README、Dockerfile 注释里
  统一呈现为参考实现。
- **红线**：不删参考应用任何功能；不改 gateway WS 协议与 REST API；
  Docker 部署路径实测可用。

---

## 7. 不变量与红线（全程有效）

1. **零破坏**：`events.jsonl` 格式、`~/.gasket/` 路径、gateway WS/REST 协议、
   参考应用全部功能、Docker 部署路径——一个不动。
2. **每步验证**：任何移动/重构后 `cargo test --all-features` + clippy `-D warnings` 绿，
   才进下一任务。
3. **事实领先叙事**：T2/T3 未修、T5/T6 未切，不挂牌（T7/T9）。
4. **范围枪毙清单**：多 provider 抽象层、RAG、编排 DSL、插件动态加载、
   「为未来预留」的配置项——规划期内一律禁止。

---

## 8. 决策点（D1-D5 已全部裁决，2026-08-16）

### D1 命名（阻塞 T1，其余不受阻）

> **已裁决（2026-08-16）：用户选择选项 b 的变体——全局改名 `conga`（底座 + 全部参考应用）。**
> **已执行完毕，验证全绿。** 执行记录见下。

**名字：`conga`**（2026-08-16 实测核查通过后由用户拍板）。

语义（为什么是它）——5 个字母，三重循环意象：
- **conga line（康加舞队）**：一个接一个加入的舞队，绕场循环——每个工具调用加入队列，
  历史在身后越排越长。「join the loop」就是 conga line 的字面玩法，与「上层往循环上
  挂自己的 agent/工具」的定位叙事完全同构。
- **conga 鼓的 tumbao 节型**：拉丁音乐里那条**反复循环的低音节奏型**（ostinato），
  其余乐器都挂在它上面——底座即节型，上层即变奏。
- 全球通识词，发音 KONG-gah，中文谐音「康嘎」，朗朗上口。

核查明细（六轮实测 ~120 个候选，非猜测）：
- **crates.io API 精确查询**：`conga` = **FREE（404）**。（搜索摘要曾声称存在
  "CongaAI/conga" Rust agent crate v0.1.0——GitHub API 直接验尸 404 + crates.io 404，
  二次证伪搜索引擎摘要幻觉，以注册表为准。）
- **npm**：被一个 v0.0.17 的死包（2013 年代 Node MVC 框架）占用——对 Rust crate 无实质
  影响；若未来发 JS 绑定再议。
- **跨生态 web 撞名**：仅 Conga Inc（Salesforce 生态商业软件公司，非开发者工具）与
  康加鼓本体。Rust/agent/LLM 领域零撞名。

淘汰记录（全部实测，两轮搜索摘要幻觉均被注册表/GitHub API 证伪）：
- ~~`refrain`~~（用户否决）、~~`chaconne`~~（用户否决：不够短、不上口）。
- ~~`dacapo`~~：撞 DaCapo Benchmark Suite（Java 基准测试标准，受众正面撞车）。
- ~~`waltz`~~/~~`tango`~~/~~`ostinato`~~/~~`rondo`~~/~~`metronome`~~/~~`bolero`~~
  （bolero 500 万下载的模糊测试框架）/~~`clave`~~/~~`pulse`~~/~~`relay`~~：crates.io TAKEN。
- ~~`mambo`~~/~~`cumbia`~~：crates.io FREE，npm 被活跃包占用；~~`verse`~~：撞 Epic Verse 语言；
  ~~`cycle`~~：撞 Cycle.js；~~`mobius`~~/~~`lfo`~~/~~`libretto`~~/~~`reprise`~~：npm 撞车。
- ~~`whirl`~~/~~`swirl`~~：crates.io 被 Reserved 囤名——短词空间洗劫的直接证据。
- 备胎：`vuelta`（crates.io+npm 双 FREE，「一圈/一转」语义最精确，但 V 开头对中文
  使用者不够上口）；`bongo`（crates.io FREE）。

| 选项 | 内容 | 代价 |
|---|---|---|
| a | 保留 `gasket-core` 词干：底座叫 `gasket-core`，参考应用沿用 `gasket`/`gasket-gateway` 二进制名 | 接受 E5/E6 撞车，搜索持续引流给重名库；零迁移成本，`~/.gasket/` 完全不动 |
| b | 整体换名（底座 + 产品全换 `conga`） | 词干最整齐；但二进制名、Docker 镜像、`~/.gasket/` 全要动，迁移面最大 |
| **c（推荐）** | 底座 = `conga`，参考应用保留 gasket 词干 | `~/.gasket/`、二进制名、Docker 全不动；叙事天然成立：「gasket，一个构建在 conga 上的参考 agent」。代价：两套词干并存，host 层归属（独立 crate 名或并入 conga 作 feature 层）由实现 plan 定 |

**执行记录（2026-08-16，已完成）：**
- `git mv`：workspace 目录 `gasket/` → `conga/`；5 个 crate → `conga` / `conga-host` /
  `conga-cli`（bin 仍叫 `conga`）/ `conga-ext` / `conga-gateway`。
- 全局文本替换（165 个 git-tracked 文件 + `.env` + 无扩展名文件 Dockerfile/.gitignore）：
  crate 名、标识符（`gasket_core::`→`conga::`）、env 前缀（`GASKET_*`→`CONGA_*`）、
  二进制名、Docker 路径、文档、Tauri 配置。历史文档（`docs/superpowers/` 旧 specs/plans、
  `review.md`）**有意保留原文**，不重写历史。
- **数据兼容（never break userspace）**：`config_dir()` 增加 legacy fallback——`~/.conga`
  不存在而 `~/.gasket` 存在时继续读旧目录（本机有真实 sessions 数据），新增契约测试
  `config_dir_prefers_new_root_and_falls_back_to_legacy`。
- 附带修复：`terminal.rs` 一处新 clippy 触发的 `map_or`→`is_ok_and`（与改名无关的
  既有 lint，1 行）。
- **验证**：`cargo test --all-features` 11 套件全绿（含新契约测试）；clippy `-D warnings`
  绿；fmt 绿；`web/` vue-tsc + vite 生产构建绿；`web/src-tauri` cargo check 绿；
  `./target/debug/conga` 冒烟（REPL 启动 + EOF 干净退出；CLI 无 clap `--help` 为既有行为）。
  **未验证**：Docker 镜像构建（本机未跑，路径已静态核对）；GitHub 仓库改名
  `YeHeng/gasket`→`YeHeng/conga` 为用户侧动作（Cargo.toml repository 字段已指向新地址）。

### D2 `getSteeringMessages` / `getFollowUpMessages`（R4）

> **已裁决（2026-08-16）：不加。** T3 范围保持单回调（`transform_context`）。
> 触发条件（重新评估时机）：参考应用出现真实的「运行中插话」需求——届时两个一起加。


### D3 `web/` 前端去留

> **已裁决（2026-08-16）：保留在仓里**，叙事降级为参考应用（T9/T10 照此执行）。不拆独立仓库。

### D4 发布节奏

> **已裁决（2026-08-16）：暂不发布。** 影响：
> - **T7 冻结发布部分**（crates.io publish、semver、`cargo add` 实测验收），解冻时机由用户定；
> - **T7 保留示例部分**（`minimal_agent.rs` ≤50 行 / `compacting_agent.rs` / `mock.rs`）——
>   它是重定位的硬验收与 dogfood，与发布无关；
> - crates.io 占位注册随之延后。风险自担声明：`conga` 目前 FREE，囤名前科
>   （`bobbin` Reserved、`whirl`/`swirl` Reserved）说明窗口不等人；
> - §9 验证缺口第 1 条（`cargo add` 实测）随发布冻结一并挂起，第 2-5 条不受影响。

### D5 cost 追踪（R11）

> **已裁决（2026-08-16）：暂不做。** backlog。`Usage` 不加 `cost` 字段；
> 触发条件：出现真实用户要求。

**D1-D5 全部裁决完毕，无开放决策点。** 规划可执行态：T2 → T3 → T4 → T5/T6 → T7(仅示例) / T8 → T9/T10。
---

## 9. 验证缺口（若跳过本规划直接挂牌，以下即谎言清单）

1. `cargo add <core>` 后 50 行内跑通自定义 agent 的实测记录。
2. `transform_context` 接缝的对抗性测试：超预算长 run、压缩后 tool 配对完整性。
3. `append_tool_call` 交错分片测试（T2）。
4. 无 feature core 的依赖树快照（支撑「极简」宣称）。
5. 底座独立 CI 的红/绿双向实测。

---

## 10. 参考

- pi（earendil-works/pi，原 badlogic/pi-mono）：`packages/agent/src/agent-loop.ts`（本 spec R3-R6 的源码依据）
- Mario Zechner, [What I learned building an opinionated and minimal coding agent](https://mariozechner.at/posts/2025-11-30-pi-coding-agent)
- rig（rig.rs）、swiftide（swiftide.rs）、smolagents —— 竞争格局与生态位证据（§2）
- 本仓 `review.md`（2026-08-16 早前 Linus 审查；其致命问题清单现状见 §3.2）
