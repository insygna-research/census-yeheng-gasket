# gasket 深度审查报告

> 审查者视角：Linus Torvalds 风格——技术直白、不堆客套、只看代码与数据。
> 审查对象：gasket workspace `2.0.0`（commit `997acdd`，分支 `v3`）
> 审查方法：通读 5 个 crate 全部源码 + docs/superpowers 全部设计文档 + 横向对比 6 个主流竞品 + MCP 生态最新规范

---

## 第一部分：gasket 到底是个什么东西（先把定位说清楚）

先别急着夸或骂，得先搞清楚这玩意儿想干嘛。

读完 `docs/architecture.md` 和代码，gasket 的自我定位是 **"轻量级、可自托管的个人 AI 助手框架"**。关键词是 **框架**——它不是一个开箱即用的产品（像 Cursor 那样），而是一套给"想自己搭一个 AI agent 的人"用的 Rust 工作区。

这个定位决定了它的两个根本约束：

1. **它必须比"自己从零写一个 agent"更省事**，否则没有存在理由。
2. **它不能变成另一个"大而全的 AI 平台"**，否则违背 "lightweight" 的承诺，且会在功能广度上输给 Open Interpreter（67k stars）、Cline（4M+ 用户）这些有团队支撑的项目。

**Linus 第一问：这是真问题还是臆想的问题？**

是真问题。2025 年的开源 AI agent 生态有明显的空白：Open Interpreter 是产品不是框架（你很难拿它做二次开发的底座）；Aider 是 CLI 工具，强绑定 Git 工作流；Continue/Cline 是 IDE 扩展，强绑定编辑器；OpenHands 是研究向的自主 agent。**"一个干净的、可复用的、Rust 写的 agent 内核 + 可插拔 host"** 这个生态位是真实存在的空缺。Rig、swiftide 这类库偏 SDK 层，不是 agent 框架。所以 gasket 的定位站得住。

但定位站得住不等于执行到位。下面拆开看。

---

## 第二部分：架构审查——哪些是真正的"好品味"

Linus 说："Bad programmers worry about the code. Good programmers worry about data structures and their relationships." gasket 的数据结构设计有几个地方体现了好品味：

### 2.1 三层分层是干净的（核心加分项）

```
gasket-core (无状态内核) → gasket-host (有状态驱动) → gasket-cli / gasket-gateway (前端壳)
```

这个分层的**接缝**选得对。`core` 不知道"谁调用、配置从哪来、结果怎么渲染"——这些都甩给 `host`。`host` 的 `run_turn` 通过 `on_event` 回调交出全部事件，自己不持有 printer/writer。这意味着 CLI 的终端渲染和 gateway 的 WebSocket 推流**共用同一份驱动代码**，零分支。

**为什么这是好品味**：因为接缝切在了"状态边界"上。agent loop 是纯函数（输入 context + config，输出 new_messages + 事件流），状态全部由上层持有。这让内核可测试（注入 mock `StreamFn`）、可复用（同一套 host 驱动两种前端）、可扩展（加工具不改内核）。这不是过度设计——这是正确的抽象边界。

### 2.2 `StreamFn` 依赖注入是教科书级的

```rust
pub trait StreamFn: Send + Sync {
    fn stream(&self, model, messages, system_prompt, tools, signal)
        -> Pin<Box<dyn Stream<Item = StreamChunk> + Send>>;
}
```

内核只认这个 trait，不认 OpenAI、不认 Anthropic。测试用 `FakeStream` 注入 canned chunk 序列，`agent_loop.rs` 有 14 个测试覆盖流式/工具/中止/重试/usage 合并——**全离线、全确定性、CI 必跑**。

这是对的。很多人写 agent 测试靠 mock HTTP server，又慢又脆。gasket 在 trait 层注入，测试直接喂 `Vec<StreamChunk>`，快且确定。`FakeStream` 脚本耗尽时 `panic` 而非静默 `Done`——这个细节尤其好，因为静默 fallback 会把"测试写错"变成"测试假阳性通过"。

### 2.3 原子组压缩（atomic_groups）解决了一个真 bug

`compact.rs` 的 `atomic_groups` 把消息切成 `[Assistant + 其后续 ToolResult]` 的不可分割组，压缩只按组丢弃。这解决了一个**真实的、会触发 provider 400 错误的 bug**：如果把 `Assistant(tool_call)` 和它的 `ToolResult` 拆开，LLM 会收到孤儿 tool_result 而报协议错误。

压缩是"只缩内存不改盘"——磁盘 JSONL 永远是 append-only 全量真相源。这个不变量是对的。带滞后（threshold 80% → target 50%）防抖也是对的。

### 2.4 JSONL torn-tail 自愈

`storage/mod.rs` 区分"末行坏=截断它（崩溃产物）"和"中行坏=报错带行号（真实损坏）"。有 11 个测试覆盖。这是**工程成熟度**的标志——大多数 agent 项目根本不考虑进程崩溃后留下半截 JSONL 怎么办。

### 2.5 双通道取消

`AtomicBool`（驱动 loop 退出）+ `watch` channel（解锁挂起在审批上的 future）。三路 `select`（oneshot 决策 / cancel / 超时）。`approval.rs` 有 9 个测试覆盖闩锁毒化场景。这个设计是对的——单通道取消会在"审批挂起时取消"死锁。

### 2.6 子 agent 的 AbortOnDrop

`host/src/subagent.rs` 的 `AbortOnDrop` guard：spawn future 被 drop 时中止所有子任务，不留脱离任务（detached task）。这是对的——否则用户取消或连接断开后，子 agent 还在后台烧 token。

### 2.7 测试覆盖的诚实

约 183 个测试。关键路径覆盖扎实：agent_loop 14 个、compact 11 个、approval 9 个、mcp 10 个、storage torn-tail 11 个、subagent 6 个。生产代码路径几乎没有 `unwrap()`（只在 `main.rs:96-97` 的 `expect("failed to bind")`，服务器启动失败直接退出是合理的）。`panic!` 全在测试里。

**小节结论**：gasket 的内核架构是**经过深思的**。这不是 "vibe coding" 出来的项目。分层、注入、不变量、测试——每一项都体现了作者理解"如何写可维护的 Rust"。这是我要先夸的部分，因为后面要骂的不少。

---

## 第三部分：什么坏了或在说谎（文档与代码的脱节）

这部分让我很不爽。一个项目最大的罪不是"缺功能"，是**对自己说谎**——文档说有，代码说没有；或者反过来。

### 3.1 致命脱节：Subagent "M2 预留" 实际已完全实现

`docs/architecture.md` 在三个地方（§7.3、§10.7、§13）明确写：

> subagent_*（10 种）| S->C | ⏳ **M2 预留**：前端已有处理器，网关暂不发送

**这是假的。** 实际代码：
- `gasket-core/src/subagent.rs`（89 行）：`SubagentSpawner` trait + `SubagentEvent`（10 变体）+ `NoopSubagentSpawner`
- `gasket-host/src/subagent.rs`（**805 行**）：`HostSubagentSpawner` 完整实现，并行 `tokio::spawn`、事件转发、AbortOnDrop、max_turns 上限
- `gasket-core/src/tools/subagent.rs`（201 行）：`spawn_subagents` 工具，maxItems 5 强制
- `gasket-gateway/src/event_map.rs:59-97`：`subagent_event_to_ws` 全 10 变体映射
- `gasket-gateway/src/ws.rs:38`：`WireEvent::Subagent` 变体接通
- 前端 `SubagentGridPanel.vue` / `SubagentThoughtsPanel.vue` 已接线渲染

**git log 证实**：`daafa9e feat(host,gateway): subagent 并行编排实现 (M2)` 已经提交了。但文档没更新。`gasket-gateway/src/main.rs:36` 的注释还写着 "⏳ M2 规划"。`web/src/components/MessageThoughtsPanel.vue:281` 还写着 "M2：subagent_* 协议尚未实现，subagents 恒为空"——**这行注释是错的，当 `spawn_subagents` 被调用时这些组件会渲染**。

### 3.2 工具数量说谎

`architecture.md` §5.2 说 "built_in_tools() 返回 6 个内置工具"，表格只列 read/write/edit/bash/grep/list。

实际 `tools/mod.rs` 的 `built_in_tools()` 返回 **8 个**：上述 6 个 + `fetch`（214 行，HTTP GET + HTML→markdown）+ `spawn_subagents`（201 行）。`tools/mod.rs:1` 的文档注释也过时了。

### 3.3 Dockerfile 是坏的

根 `Dockerfile` 引用 `gasket/types/`、`gasket/storage/`、`gasket/engine/` 等**已不存在的旧 crate 路径**。`EXPOSE 18790` 对不上 gateway 默认端口 3000。`ENTRYPOINT ["gasket"]` 跑的是 CLI 不是网关。

`docs/usage.md §7` 自己都承认 "⚠️ 仓库根 Dockerfile 当前已过时，无法直接使用"。**那就删了它或修了它**。留一个坏掉的 Dockerfile 在仓库根，是给每个新用户挖坑。

### 3.4 .env.example 不完整

`gasket/.env.example` 只文档了 LLM 连接 + 几个 loop tunables。**缺**：所有 gateway 变量（端口、静态目录、模式、审批超时）、所有压缩变量、MCP 配置、外部工具、搜索 provider。`usage.md §10` 说 "以本表为准"——但用户拷贝的是 `.env.example`，不是文档。

### 3.5 版本号三处不一致

workspace `2.0.0` vs `web/package.json` `0.0.0` vs `tauri.conf.json` `0.0.0`。release.yml 打 tag `v2.0.0`，但桌面端产物版本是 0.0.0。

### 3.6 没有顶层 README

仓库根没有 README.md。`docs/` 下只有 `architecture.md`、`usage.md` 和 `docs/superpowers/`（内部设计文档，中文，带日期状态元信息）。一个开源项目没有 README，GitHub 首页就是空的——这直接影响项目能不能被人发现和使用。

### 3.7 CI 不覆盖前端

`.github/workflows/ci.yml` 的 `paths` 只触发 `gasket/**`。**`web/**` 的改动完全绕过 CI**——没有前端 lint、没有 `vue-tsc` 类型检查、没有 `vite build`。这意味着前端可以静默坏掉（类型错误、构建失败），CI 全绿。

**Linus 的话**：文档与代码脱节是**项目腐化的第一个信号**。代码说一套，文档说另一套，新人来了信文档，踩坑，然后再也不信文档。这比没有文档更糟。subagent 已经实现了却不更新架构文档，说明**文档维护没有进入工作流**——这不是一次性疏忽，是流程缺陷。

**立即行动项**（不需要讨论，今天就该做）：
1. 更新 `architecture.md`：subagent 已实现，工具数 8，fetch 已存在。
2. 删除或修复根 `Dockerfile`。
3. 补全 `.env.example` 与 `usage.md §10` 对齐。
4. 统一版本号到 `2.0.0`（web/package.json + tauri.conf.json）。
5. 写一个顶层 README.md。
6. 修 CI：`paths` 加 `web/**`，加 `pnpm install && pnpm build` job。
7. 清理过时代码注释：`main.rs:36`、`MessageThoughtsPanel.vue:281`、`tools/mod.rs:1`。

---

## 第四部分：横向对比——gasket 在 2025-2026 的 agent 生态里站在哪

我搜了当前主流的开源 AI coding agent，做一张诚实的对比表：

| 维度 | gasket | Open Interpreter | Aider | Continue | Cline | OpenHands |
|---|---|---|---|---|---|---|
| 形态 | 框架(lib+bin) | 产品(CLI) | 产品(CLI) | IDE 扩展 | IDE 扩展 | 自主 agent |
| 语言 | Rust | Rust(重写) | Python | TypeScript | TypeScript | Python |
| 定位 | 可复用内核 | 终端 agent | Git 结对 | IDE 助手 | VS Code agent | 项目级自主 |
| Stars | ~新 | 67k | 高 | 20k+ | 4M 用户 | 研究向 |
| MCP | ✅ stdio+legacy | ✅ | ❌ | ✅ | ✅ | ✅ |
| Git 集成 | ❌(仅尊重.gitignore) | ❌ | ✅(核心) | 扩展 | ✅ | ✅ |
| 浏览器自动化 | ❌ | ✅ | ❌ | ❌ | ✅ | ✅ |
| 沙箱隔离 | ❌(仅权限提示) | ✅ | ❌ | ❌ | ❌ | ✅ |
| 多 agent | ✅(subagent) | ❌ | ❌ | ❌ | ❌ | ✅ |
| Plan 模式 | ❌ | ❌ | ❌ | ❌ | ✅(Plan/Act) | ✅ |
| 模型路由 | ❌(单模型) | ✅ | ✅ | ✅ | ✅ | ✅ |
| Repo 索引/RAG | ❌ | ❌ | ✅(repo map) | ✅ | ✅ | ✅ |
| 桌面端 | ✅(Tauri) | ❌ | ❌ | ❌ | ❌ | ❌ |
| 自托管 | ✅ | ✅ | ✅(本地) | ✅ | ✅(本地) | ✅ |

### 4.1 gasket 的真实优势（不是自我吹捧的那种）

1. **它是框架不是产品**——上述竞品全是产品。你想 fork Open Interpreter 做自己的 agent？祝你好运。gasket 的 `gasket-core` + `gasket-host` 是真的可以当库用的。这是它的生态位护城河。
2. **Rust + 类型安全**——Open Interpreter 重写版也是 Rust，但 Aider/Continue/Cline/OpenHands 全是 Python/TS。Rust 的内存安全和性能对长跑 agent 是真优势（无 GC 抖动、低内存）。
3. **Tauri 桌面端**——唯一一个原生桌面形态。Open Interpreter/Aider 是终端，Continue/Cline 绑 IDE。gasket 的 "PWA 壳 + 远端 gateway" 模型虽然不是纯离线，但部署形态独特。
4. **子 agent 并行**——这是 2025 的趋势（multi-agent orchestration）。Open Interpreter/Aider 没有。gasket 的实现（并发 tokio::spawn + AbortOnDrop + 事件流转发）在技术上是扎实的。
5. **MCP 客户端**——接入了生态最大公约数。虽然只支持 stdio+legacy，但 Claude-Desktop 格式配置可复用。

### 4.2 gasket 的真实劣势（数据说话）

1. **没有 Git 集成**——这是 Aider 的杀手锏，gasket 完全没有。一个 coding agent 不懂 Git，就像一个厨师不用刀。`list`/`grep` 尊重 `.gitignore` 是一回事，能 `git diff` / `git commit` / 理解 repo 结构是另一回事。
2. **没有浏览器自动化**——Cline 的浏览器工具是 2025 的差异化特性。gasket 的 `fetch` 是只读 HTTP GET，不能 JS 渲染，不能交互。大量现代文档/应用是 SPA，fetch 抓不到。
3. **没有沙箱**——`bash` 工具以完整用户权限运行，只靠权限提示。Open Interpreter 有沙箱隔离。一个 `full-auto` 模式下 `rm -rf /` 没有 guard。
4. **没有 Plan 模式**——Cline 的 Plan/Act 是 2025 的 UX 共识。gasket 的 Suggest/AutoEdit/FullAuto 是**权限**模式，不是**规划**模式。没有"先想后做"的阶段。
5. **单模型**——一次运行只有一个 `GASKET_LLM_MODEL`。不能给子 agent 用便宜模型、主 agent 用贵模型。竞品全支持模型路由。
6. **没有 repo 索引/RAG**——`grep` 是盲搜。Aider 有 repo map，Continue/Cline 有 embeddings。gasket 没有任何代码库语义理解。
7. **压缩是暴力丢弃**——`compact_by_count` 丢最旧的整组 + 前置通知。Claude Code / Cursor 用 LLM 摘要。gasket 明确说 "无 LLM 摘要"——这是有意的简化，但意味着**信息有损**。

### 4.3 MCP 生态对比（这是 gasket 最该补的地方）

当前 MCP 规范最新版是 **2025-11-25**（gasket 用的 `2025-06-18` 是 legacy era）。我查了官方架构文档和生态分析：

| MCP 能力 | gasket | 规范最新 | 生态主流 |
|---|---|---|---|
| stdio 传输 | ✅ | ✅ | ✅ |
| Streamable HTTP 传输 | ❌ | ✅(推荐) | ✅(远程 server 必需) |
| tools 原语 | ✅ | ✅ | ✅ |
| resources 原语 | ❌ | ✅ | 部分 |
| prompts 原语 | ❌ | ✅ | 部分 |
| sampling(server→client LLM) | ❌ | ✅ | 部分 |
| elicitation(server 问用户) | ❌ | ✅(2025-06-18) | 新 |
| roots(文件系统边界) | ❌ | ✅ | 部分 |
| Tasks(长任务) | ❌ | ✅(2025-11-25) | 新 |
| 自动重连/健康检查 | ❌ | — | 主流客户端有 |
| annotations→风险映射 | ❌(全 High) | ✅ | 部分 |

**关键差距**：gasket 不支持 Streamable HTTP。这意味着**所有远程 MCP server（Sentry、GitHub、Slack 等官方 server）都用不了**——它们都是 HTTP server，不是 stdio 子进程。gasket 只能用 `npx -y @modelcontextprotocol/server-xxx` 这类本地 stdio server。这把 MCP 生态砍掉了一半以上。

---

## 第五部分：真正的差距——按 Linus 三问筛选

Linus 三问：1) 这是真问题还是臆想？2) 有没有更简单的办法？3) 这会破坏什么？

我把所有"能做的功能"过一遍这三问，过滤掉投机性需求，留下真问题：

### 真问题（必须解决，否则项目没竞争力）

| ID | 差距 | 为什么是真问题 | 严重度 |
|---|---|---|---|
| **G1** | 文档/代码脱节 | 项目可信度的根本。连自己有什么都说不清，没人敢用做底座 | 🔴 致命 |
| **G2** | 无 Git 集成 | coding agent 不懂 Git = 厨师不用刀。Aider 全靠这个吃饭 | 🔴 致命 |
| **G3** | MCP 无 Streamable HTTP | 砍掉一半 MCP 生态（所有远程 server）。2025 远程 server 是主流 | 🔴 高 |
| **G4** | 无沙箱/隔离 | `full-auto` + `bash` = 灾难。安全是 agent 的底线 | 🔴 高 |
| **G5** | bash 工具串行执行 tool calls | `agent_loop.rs:90` 注释 "serial in V0.1"。LLM 越来越多并行调用，串行浪费延迟 | 🟡 中 |
| **G6** | CI 不覆盖前端 | 前端可静默坏掉。生产质量的基本要求 | 🟡 中 |
| **G7** | 无 README | 开源项目被发现的前提 | 🟡 中 |

### 投机问题（暂不做，等真实用户驱动）

| ID | 差距 | 为什么暂不做 |
|---|---|---|
| Plan 模式 | gasket 是框架，Plan/Act 是产品决策。host 层加一个 `plan_turn` 容易，但没消费者驱动就是投机 |
| 模型路由 | 单 provider 够用 90% 场景。多模型路由是 Cline 那种产品的需求，框架层不该硬编码 |
| RAG/repo 索引 | 重依赖（embeddings + 向量库），违背 lightweight。留给 ext crate 或 MCP server |
| LLM 摘要压缩 | 当前暴力丢弃 + provider 真实 token 触发，实测够用。摘要压缩是优化，不是 bug |
| 多文件原子编辑 | 单文件 edit 够用。跨文件事务是数据库问题，agent 不该背 |
| 跨会话记忆 | WorkBuddy 式记忆系统是产品特性，不是框架特性 |

---

## 第六部分：下一步该做什么（按优先级 + 可验证目标）

### P0：诚实化（1-2 天，零功能开发，纯修文档/CI）

这是最重要的。不是因为难，是因为**不做这个，后面所有功能开发都建在沙子上**。

```
1. 更新 architecture.md：subagent 已实现、工具 8 个、fetch 已存在
   → verify: architecture.md §5.2/§7.3/§10.7/§13 与代码一致
2. 删除或重写根 Dockerfile（指向当前 5-crate 结构，EXPOSE 3000，ENTRYPOINT gasket-gateway）
   → verify: docker build -t gasket . 成功
3. 补全 .env.example 与 usage.md §10 完全对齐
   → verify: diff .env.example <(usage.md §10 的表)
4. 统一版本号：web/package.json + tauri.conf.json → 2.0.0
   → verify: grep -r "0.0.0" web/ 无残留
5. 写顶层 README.md（项目是什么、5 分钟上手、链接到 docs/）
   → verify: GitHub 首页不再空
6. 修 CI：paths 加 web/**，加 frontend job（pnpm install + vue-tsc + vite build）
   → verify: 改 web/ 触发 CI
7. 清理过时代码注释：main.rs:36、MessageThoughtsPanel.vue:281、tools/mod.rs:1
   → verify: grep "M2" 无虚假残留
```

**Linus 的话**：这 7 件事没有一件是"功能"，但每一件都比加新功能重要。因为它们决定"这个项目能不能被信任"。

### P1：Git 集成（1-2 周，gasket 作为 coding agent 的入场券）

这是 gasket 从"通用 chat agent"升级到"coding agent"的转折点。

**为什么是真问题**：Aider 60% 的价值来自 Git 集成。gasket 的 `read`/`edit`/`bash` 能改文件，但 agent 不知道改了什么、不知道 diff、不能 commit、不能回滚。这让它无法做"安全的代码修改"。

**最小实现（YAGNI 边界）**：

新增内置工具（`gasket-core/src/tools/`）：
- `git_status`（Low）：`git status --porcelain` → 文本
- `git_diff`（Low）：`git diff [path]` → 文本，支持 `--staged`
- `git_commit`（High）：`git commit -m <msg>`，**不自动 stage**（agent 必须显式 git_add）
- `git_add`（Medium）：`git add <paths>`
- `git_log`（Low）：`git log --oneline -n <n>`

**不做**：`git push`（网络副作用，留给 bash + 审批）、`git reset --hard`（破坏性，留给 bash + 审批）、merge/rebase（复杂状态机，YAGNI）。

**系统提示增强**：cwd 在 git repo 内时，system_prompt 自动追加 "You are in a git repository. Prefer git_commit to save changes; use git_diff to review before committing."

**验收**：
```
1. agent 能 git_diff 看自己 edit 的结果
2. agent 能 git_add + git_commit 保存工作
3. full-auto 模式下 git_commit 仍需审批（High risk）
4. 非 git repo 下这些工具返回明确错误
5. cargo test 新增 5 个工具的单测
```

**为什么不直接用 bash 跑 git**：因为 git 操作是**结构化**的（有明确的成功/失败语义、有 diff 输出格式），用 bash 包一层会把结构丢掉。专用工具能让 agent 更可靠地理解 git 状态。Aider 证明了这一点。

### P2：MCP Streamable HTTP 传输（1-2 周，解锁远程 server 生态）

当前 `McpBridge` 只 spawn stdio 子进程。Streamable HTTP 是独立的传输实现，不污染 stdio 路径。

**为什么是真问题**：Sentry、GitHub、Slack、Notion 等官方 MCP server 都是 HTTP server。gasket 用户想接这些，现在做不到。这把 MCP 生态砍掉一半。

**设计要点**：
- 新增 `McpHttpBridge`（与 `McpBridge` 平行，都实现一个内部 `McpTransport` trait）
- 配置格式扩展：`mcp.json` 的 server 项加 `"url": "https://..."` 字段（有 `command` = stdio，有 `url` = HTTP）
- Streamable HTTP：POST JSON-RPC 请求，响应可能是单 JSON 或 SSE 流
- OAuth 2.1 bearer token 支持（`"auth": {"type": "bearer", "token": "..."}` 或 env 引用）
- 不做：sampling/elicitation/roots/Tasks 原语（YAGNI，等真实需求）

**验收**：
```
1. 配置一个 HTTP MCP server，agent 能 list + call 其工具
2. OAuth bearer token 正确注入 Authorization header
3. stdio 和 HTTP server 可在同一个 mcp.json 里混用
4. HTTP server 超时/断开 → 该 server 工具缺失，其余继续
5. cargo test 新增 HTTP transport 的纯函数测试（mock HTTP）
```

### P3：并行工具调用（3-5 天，性能优化）

`agent_loop.rs:90` 注释 "Execute tool calls (serial in V0.1)"。

**为什么是真问题**：现代 LLM（Claude 3.5+、GPT-4o+）会在一个 turn 里请求多个独立工具调用。串行执行意味着 3 个独立的 `read` 要顺序等 3 次 IO。并行 `tokio::join_all` 能把延迟砍到 max(单个)。

**实现**：`execute_tool_calls` 里把 `for tc in tool_calls` 改成 ` futures::future::join_all`。但**有约束**：
- `before_tool_call` hook 仍需串行（审批要逐个确认）
- 同名工具的多次调用可能写同一文件（需检测冲突，串行化冲突项）
- 工具结果顺序必须与 tool_call 顺序一致（LLM 依赖 id 匹配）

**Linus 警告**：并行化是最容易引入 bug 的优化。必须有测试覆盖：并行 read（安全）、并行 write 同一文件（必须串行）、并行 bash（必须串行——shell 副作用不可并行）。

**验收**：
```
1. 3 个独立 read 工具并行执行，总延迟 ≈ max(单个)
2. 2 个 write 同一文件 → 串行执行，结果确定性
3. tool_result 顺序与 tool_call 顺序一致
4. 既有 agent_loop 测试全绿
```

### P4：bash 沙箱（可选，1-2 周，安全加固）

**为什么列 P4 而非 P2**：沙箱实现复杂（Linux namespace / macOS sandbox-exec / Docker），跨平台地狱。对"个人自托管"场景，权限提示 + 非 full-auto 默认已经够用。但如果 gasket 想进团队/企业场景，这是必做的。

**YAGNI 边界**：
- Linux：用 `bubblewrap`（bwrap）做 namespace 隔离（只读 /、读写 cwd、禁网络可选）
- macOS：用 `sandbox-exec`（系统自带，profile 文件）
- Windows：暂不支持沙箱（文档化），或用 Docker
- 不做：自实现 namespace（重造轮子）、seccomp 过滤（YAGNI）

**验收**：
```
1. full-auto 模式下 bash 默认在沙箱内运行
2. 沙箱内 rm -rf / 失败（只读根）
3. 沙箱内可读写 cwd
4. GASKET_SANDBOX=off 可禁用（向后兼容）
5. 非 Linux/macOS 平台沙箱降级为权限提示 + warn
```

---

## 第七部分：代码级 nitpick（细节里的魔鬼）

这些不是 P0-P4，但是 review 必须指出：

### 7.1 `tools/mod.rs` 的 `resolve_within_cwd` 是好的，但不够

它防 `..` 和绝对路径逃逸，含 symlink 解析检查。**但** `bash` 工具绕过了它——`bash` 直接 `Command::new("sh")` 执行，agent 可以 `cat /etc/passwd` 或 `cd / && rm -rf *`。`bash` 的风险靠权限提示，不靠路径约束。这是**设计选择**（bash 本就该能做任何事），但要文档化这个边界。

### 7.2 `McpBridge::call` 的超时是固定的 60s

`DEFAULT_TIMEOUT: Duration = Duration::from_secs(60)`。`recv_response` 用 `deadline` 一次性，**不因 `notifications/progress` 刷新**（设计文档明说 "首版不做"）。长任务 MCP server（如大文件处理、长查询）会被误杀。

**建议**：加 `GASKET_MCP_CALL_TIMEOUT_S` 环境变量，默认 60s，允许调大。progress 通知刷新超时留作 P2+。

### 7.3 子 agent 的 max_turns 硬编码 10

`SUBAGENT_MAX_TURNS: usize = 10`。这是对的（子 agent 不该长跑），但**不可配置**。复杂子任务（如"重构这个模块"）10 轮可能不够。`sub_config.max_turns = SUBAGENT_MAX_TURNS.min(spawner.loop_config.max_turns)`——只取 min，父 agent 设 5 轮时子 agent 也 5 轮，合理；但父设 100 轮时子还是 10 轮，没法调。

**建议**：加 `GASKET_SUBAGENT_MAX_TURNS` 环境变量，默认 10。

### 7.4 `approval_memory` 不持久化

gateway `WsSession.approval_memory` 是 `HashMap`，在内存里。**gateway 重启后 "remember" 决策全丢**。用户记得"允许 read"，重启后又要重新审批。

**建议**：要么文档化"remember 是 per-session 的"，要么持久化到 `~/.gasket/approval_memory.json`。前者更简单，符合 lightweight。

### 7.5 `system_prompt` 硬编码

`gasket-gateway/src/ws.rs:104`：
```rust
let system_prompt = "You are a helpful, concise assistant.".to_string();
```

CLI 也是类似硬编码。**没有 `GASKET_SYSTEM_PROMPT` 环境变量**。用户想定制 agent 人格，只能改代码重编译。这对"框架"定位是硬伤——框架应该让用户不改代码就能定制。

**建议**：加 `GASKET_SYSTEM_PROMPT`（或 `GASKET_SYSTEM_PROMPT_FILE`）环境变量，默认值保留当前字符串。

### 7.6 压缩通知是英文 `[compacted N earlier messages]`

`compact.rs` 硬编码英文。中文用户看到英文通知略突兀。这不是 bug，是 i18n 缺失。**YAGNI**——等真实用户反馈再 i18n。

### 7.7 `tracing` 用了但没配 formatter

`tracing-subscriber` 在依赖里，但 `main.rs` 里没看到 `tracing_subscriber::fmt::init()` 或 env filter 配置。日志可能默认不输出。**建议**：CLI/gateway 启动时初始化 `EnvFilter` 从 `RUST_LOG`。

### 7.8 `futures-util` 是 dev-dependency 还是 dependency？

`gasket-host` 的 `tests/common/mod.rs` 用 `futures_util::stream::iter`。设计文档说放 dev-dependency。但 `host/src/subagent.rs` 用了 `futures`（workspace dependency）。**确认** `futures-util` 不进 release 依赖树——否则是为测试引入的全局依赖。

---

## 第八部分：最终裁决

### 8.1 代码质量：B+

内核架构是 A。分层、注入、不变量、测试——这些是对的。扣分在文档脱节（致命）和几个硬编码（system_prompt、max_turns、timeout）。

### 8.2 工程成熟度：C+

没有 README、CI 不覆盖前端、Dockerfile 坏的、版本号不一致、.env.example 不完整。这些不是"不会写代码"，是"没把项目当项目对待"。一个 67k stars 的项目和一个 5 stars 的项目，代码质量差距可能不大，差距在工程纪律。

### 8.3 生态竞争力：C

作为"框架"有定位护城河（Rust + 可复用内核 + Tauri）。但作为"coding agent"缺 Git 集成是硬伤，作为"MCP 客户端"缺 Streamable HTTP 砍掉一半生态。在 2025-2026 的 agent 涌潮里，gasket 目前是"技术上有趣但功能上不全"的状态。

### 8.4 下一步的优先级裁决

**如果只能做一件事**：P0（诚实化）。因为不修这个，P1-P4 都是在沙子上盖楼——新功能加了，文档还是说旧的，新人来了继续踩坑。

**如果能做三件事**：P0 → P1（Git）→ P2（MCP HTTP）。这三件做完，gasket 从"有趣的框架"变成"可用的 coding agent 框架"。

**如果有人想现在就用 gasket**：可以，但只作为"个人 chat agent + 工具调用"用。别指望它做严肃的代码重构（没 Git）、别指望接远程 MCP server（没 HTTP 传输）、别在 full-auto 下跑不可信任务（没沙箱）。

### 8.5 给作者的话

你的内核写得比大多数开源 AI 项目好。分层、注入、不变量、测试——这些不是堆出来的，是想出来的。**但项目的下半场是工程纪律，不是代码才华**。文档与代码同步、CI 覆盖全栈、版本号一致、README 存在——这些无聊的事决定项目能不能活过一年。

subagent 已经实现了却不说，比"没实现却说实现了"好，但仍然是失职。修了 P0，你的内核才华才有人看得见。

---

## 附录 A：竞品数据来源

- Open Interpreter：67k stars，Rust 重写版，终端 + MCP + 沙箱（tycp.xyz 2026-07 评测）
- Aider：CLI，Git 原生，多模型，94% 重构准确率（techbullion 2025 评测）
- Continue：20k+ stars，VS Code/JetBrains，本地优先（Ollama/LM Studio）
- Cline：4M+ 用户，VS Code，Plan/Act 双模式，MCP，浏览器自动化
- OpenHands（原 OpenDevin）：研究向自主 agent，项目级编排
- MCP 规范：最新 2025-11-25，Streamable HTTP 替代 HTTP+SSE，新增 Tasks/Elicitation/Extensions

## 附录 B：gasket 代码体量（实测）

| Crate | src 行数 | tests 行数 | 测试函数数 |
|---|---|---|---|
| gasket-core | 5,885 | 141 | 91 |
| gasket-host | 3,785 | 500 | 55 |
| gasket-cli | 246 | 0 | 0 |
| gasket-ext | 954 | 0 | 11 |
| gasket-gateway | 1,471 | 0 | 18 |
| **合计** | **12,341** | **641** | **175** |

最大文件：`agent_loop.rs`（1,462 行）、`host/subagent.rs`（805 行）、`ext/search.rs`（763 行）、`host/mcp.rs`（762 行）。

## 附录 C：审查覆盖的文件清单

- 全部 5 crate 的 src/ 下所有 .rs 文件（通读或抽样）
- `docs/architecture.md`、`docs/usage.md`（全读）
- `docs/superpowers/specs/` 7 个设计文档（全读）
- `docs/superpowers/plans/` 2 个实施计划（全读）
- `.github/workflows/ci.yml`、`release.yml`（全读）
- `web/package.json`、`web/src/` 关键组件（抽样）
- `gasket/.env.example`、根 `Dockerfile`、`LICENSE`（全读）
- git log（最近 20 提交）
- 网络搜索：6 个竞品 + MCP 规范最新版 + 生态分析
