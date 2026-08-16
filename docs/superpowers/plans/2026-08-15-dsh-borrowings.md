# dsh 功能借鉴落地计划(P0 可行性 + 实施任务)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 从 deepseek-harness 借鉴四项 P0 功能(loop 重复调用防护、工具结果外溢、MCP 代理统一、composition root 收敛),并以可行性结论界定 P1/P2(sandbox、PTY、skills、session-query)的后续独立 plan。

**Architecture:** 全部改动落在既有数据结构上,不新增抽象层:RepeatGuard 是 `agent_loop` 内的局部状态;spill 复用 `MAX_OUTPUT_BYTES` 阈值与 per-tool `state_dir`;MCP 代理换成 `apply_tool_proxy` 优先 + 旧 env 链兜底;composition root 收敛为一个 `prod` 注册函数。零 wire 协议变更、零破坏性。

**Tech Stack:** Rust workspace(gasket-core / gasket-host / gasket-ext / web/src-tauri),std `DefaultHasher`,已有 `tempfile` dev-dep。

## Global Constraints

- 不引入新的外部依赖(RepeatGuard / spill / MCP 代理统一全部 std 实现)。
- 不新增环境变量、不新增配置项(阈值复用 `MAX_OUTPUT_BYTES = 200_000`,重复阈值硬编码 3)。
- 不修改 wire 协议(`SessionEvent` / `wire.rs` 字段不动)。
- 每个任务以 `cargo test -p gasket-core`(或对应 crate)通过为完成标准;提交遵循 conventional commits,英文。
- P1(sandbox)与 P2(PTY / skills / session-query)**不在本 plan 内实施**,只交付可行性结论与接口契约,各自另立 plan。

---

## 背景:为什么是这四项

对比 review 的判定:dsh 的 Cordis/"Everything is a Plugin" 架构**不借鉴**;以下功能按性价比入选 P0:

| 借鉴项 | dsh 对应包 | gasket 痛点 | 量级 |
|---|---|---|---|
| 重复调用防护 | `guard/` | 模型可连续同参调用同一工具直到 `MAX_TOOL_CALLS` 熔断,烧 token | ~40 行 |
| 结果外溢 | `spill/` | 大输出现在直接截断丢信息(`truncate_output` @ 200KB) | ~80 行 |
| MCP 代理统一 | `web/` seam 思路 | `mcp.rs` 自读 `GASKET_LLM_PROXY`/`HTTPS_PROXY`,绕开 `apply_tool_proxy` | ~30 行 |
| composition root 收敛 | — | 桌面端手挑 `search::register`,CLI 用 `register_all`,组装根分叉 | ~15 行 |

## 文件结构总览

| 文件 | 动作 | 职责 |
|---|---|---|
| `gasket/gasket-core/src/guard.rs` | 新建 | `RepeatGuard` 纯逻辑 + advisory 文案 |
| `gasket/gasket-core/src/agent_loop.rs` | 修改 | 接线 guard(`execute_tool_calls` 增参) |
| `gasket/gasket-core/src/tools/mod.rs` | 修改 | `spill_or_truncate` 函数 |
| `gasket/gasket-core/src/tools/bash.rs` `fetch.rs` | 修改 | 调用点改用 `spill_or_truncate` |
| `gasket/gasket-host/src/mcp.rs` | 修改 | `pick_mcp_proxy` 统一代理选择 |
| `gasket/gasket-ext/src/lib.rs` | 修改 | `prod_register` 函数 |
| `web/src-tauri/src/chat.rs` | 修改 | 桌面组装根改用 `prod_register` |
| `docs/usage.md` | 修改 | 补 spill 行为说明一句 |

---

### Task 1: RepeatGuard — 连续同参重复调用 advisory

**Files:**
- Create: `gasket/gasket-core/src/guard.rs`
- Modify: `gasket/gasket-core/src/agent_loop.rs:50`(`for turn` 前)、`:105`(调用点)、`:220`(`execute_tool_calls` 签名)、`:360-365`(结果落地前)
- Modify: `gasket/gasket-core/src/lib.rs`(`pub mod guard;` 按字母序插入 `pub mod extension;` 之后)

**Interfaces:**
- Produces: `pub struct RepeatGuard`、`RepeatGuard::new() -> Self`、`(&mut self, tool: &str, args_key: &str) -> u32`(返回连续重复计数,首次为 1)、`pub fn repeat_advisory(count: u32) -> Option<String>`(仅 count == 3 时返回 Some)。

**设计要点(消除特殊情况):** 只跟踪"上一个调用"的 `(tool, args)` 二元组 —— 换任何一侧即重置计数。不需要环形缓冲、不需要 HashMap、不需要窗口大小配置。advisory 只在连续第 3 次注入一次,不反复唠叨。

- [ ] **Step 1: 写失败测试**

```rust
// gasket/gasket-core/src/guard.rs 底部
#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn counts_consecutive_identical_calls() {
    let mut g = RepeatGuard::new();
    assert_eq!(g.observe("bash", r#"{"command":"ls"}"#), 1);
    assert_eq!(g.observe("bash", r#"{"command":"ls"}"#), 2);
    assert_eq!(g.observe("bash", r#"{"command":"ls"}"#), 3);
    // 任一侧变化 -> 重置
    assert_eq!(g.observe("bash", r#"{"command":"pwd"}"#), 1);
    assert_eq!(g.observe("read", r#"{"command":"pwd"}"#), 1);
    // 回到原始组合也是新 streak
    assert_eq!(g.observe("bash", r#"{"command":"ls"}"#), 1);
  }

  #[test]
  fn advisory_fires_only_at_three() {
    assert!(repeat_advisory(1).is_none());
    assert!(repeat_advisory(2).is_none());
    let msg = repeat_advisory(3).unwrap();
    assert!(msg.contains("identical"), "{msg}");
    assert!(repeat_advisory(4).is_none());
    assert!(repeat_advisory(9).is_none());
  }
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cd gasket && cargo test -p gasket-core guard`
Expected: FAIL(模块不存在)

- [ ] **Step 3: 最小实现**

```rust
//! Loop-hygiene guard (borrowed from dsh `guard/`): advisory note when the
//! model makes the exact same tool call three times in a row.

/// Tracks only the previous call's (tool, args) — any change resets the
/// streak. No window, no map: the simplest structure that answers "is this
/// the third identical call in a row?".
pub struct RepeatGuard {
  last: Option<(String, String)>,
  count: u32,
}

impl RepeatGuard {
  pub fn new() -> Self {
    Self { last: None, count: 0 }
  }

  /// Record a call; returns the current consecutive-repeat count (1 = first).
  pub fn observe(&mut self, tool: &str, args_key: &str) -> u32 {
    let same = self
      .last
      .as_ref()
      .is_some_and(|(t, a)| t == tool && a == args_key);
    self.count = if same { self.count + 1 } else { 1 };
    self.last = Some((tool.to_string(), args_key.to_string()));
    self.count
  }
}

/// The note appended to the tool result, fired exactly once per streak.
pub fn repeat_advisory(count: u32) -> Option<String> {
  (count == 3).then(|| {
    "note: this is the third identical call in a row — the result is unlikely \
     to change. If it failed twice, change the arguments or approach."
      .to_string()
  })
}
```

同时在 `lib.rs` 注册模块,并把 `agent_loop.rs` 接线(见 Step 4)。

- [ ] **Step 4: 接线 `execute_tool_calls`**

`agent_loop.rs` 三处修改(全部私有,单调用方):

1. `agent_loop()` 内、`for turn in 0..config.max_turns` 之前:

```rust
let mut guard = crate::guard::RepeatGuard::new();
```

2. `execute_tool_calls` 签名加参(放在 `config` 之后):

```rust
async fn execute_tool_calls<E>(
    context: &AgentContext,
    assistant: &AssistantMessage,
    config: &AgentLoopConfig,
    guard: &mut crate::guard::RepeatGuard,
    emit: &mut E,
) -> Result<Vec<ToolResultMessage>, AgentError>
```

调用点 `:105` 相应传 `&mut guard`。

3. 工具执行段:`args` 被 move 进 `ToolCallCtx` 之前先留键(`args.to_string()` 是 serde_json 的规范化序列化,等价于比较语义):

```rust
// after ToolCallVerdict match, before `emit(AgentEvent::ToolExecutionStart`:
let args_key = args.to_string();
```

在 after_tool_call hook 之后、`let is_error = result.is_error;` 之前插入:

```rust
if let Some(note) = crate::guard::repeat_advisory(guard.observe(&tc.function.name, &args_key)) {
  if let Some(crate::ContentBlock::Text { text }) = result.content.first_mut() {
    text.push_str("\n\n[");
    text.push_str(&note);
    text.push(']');
  }
}
```

- [ ] **Step 5: 测试通过 + 全量回归**

Run: `cd gasket && cargo test -p gasket-core`
Expected: PASS(新增 2 个测试,既有 124 个不变绿→绿)

- [ ] **Step 6: 提交**

```bash
git add gasket/gasket-core/src/guard.rs gasket/gasket-core/src/agent_loop.rs gasket/gasket-core/src/lib.rs
git commit -m "feat(core): advisory on third identical consecutive tool call (dsh guard)"
```

---

### Task 2: Spill — 大结果外溢到 state_dir,上下文留句柄

**Files:**
- Modify: `gasket/gasket-core/src/tools/mod.rs`(`truncate_output` 旁新增 `spill_or_truncate`)
- Modify: `gasket/gasket-core/src/tools/bash.rs:72-73`、`gasket/gasket-core/src/tools/fetch.rs:88`(调用点切换)
- Test: 就地 `#[cfg(test)]`(mod.rs 已有 truncate 测试区)

**Interfaces:**
- Consumes: `ToolCallCtx`(bash/fetch 的 `execute(ctx)` 已持有;`ctx.ctx.state_dir` 为 `<config_dir>/tool_state/<session_id>/<tool_name>/`)
- Produces: `pub(crate) fn spill_or_truncate(ctx: &ToolCallCtx, s: &str) -> String` — 超过 `MAX_OUTPUT_BYTES` 时把完整原文写入 `state_dir/spill/<12-hex-hash>.txt`,返回头部预览 + 文件路径的 stub;未超限原样返回;**写盘失败时回退到既有 `truncate_output`**(工具永远不因 spill 失败而失败)。

**设计要点:** 阈值复用 `MAX_OUTPUT_BYTES`(不造第二个阈值概念);文件名用 `DefaultHasher` 哈希(内容寻址,同输出天然去重,零依赖);bash 的 stdout/stderr 分开调用、各自 spill。模型拿到路径后可用既有 `read` 工具按 offset 分段读回 —— 这是"大结果不进上下文但仍可达"的完整闭环,不需要新工具。

- [ ] **Step 1: 写失败测试**

```rust
// gasket/gasket-core/src/tools/mod.rs tests 模块内
#[test]
fn spill_writes_full_output_and_returns_stub() {
  let tmp = tempfile::tempdir().unwrap();
  let ctx = ToolCallCtx {
    tool_call_id: "t".into(),
    args: serde_json::json!({}),
    signal: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    ctx: crate::ToolContext {
      cwd: ".".into(),
      env: std::collections::HashMap::new(),
      session_id: "s".into(),
      state_dir: tmp.path().to_path_buf(),
      spawner: None,
    },
  };
  let big = "x".repeat(MAX_OUTPUT_BYTES + 1000);
  let out = spill_or_truncate(&ctx, &big);
  assert!(out.len() < big.len());
  assert!(out.contains("full output saved to"), "{out}");
  // stub 指向的文件确实包含完整原文
  let path = out.lines().find(|l| l.contains("saved to")).unwrap()
    .rsplit(' ').next().unwrap().trim();
  let on_disk = std::fs::read_to_string(path).unwrap();
  assert_eq!(on_disk.len(), big.len());
}

#[test]
fn spill_small_output_passthrough() {
  let tmp = tempfile::tempdir().unwrap();
  // (同上构造 ctx,state_dir: tmp.path())
  let out = spill_or_truncate(&ctx, "small");
  assert_eq!(out, "small");
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cd gasket && cargo test -p gasket-core spill`
Expected: FAIL(`spill_or_truncate` 未定义)

- [ ] **Step 3: 最小实现**

```rust
/// Spill threshold-sharing wrapper around [`truncate_output`]: content over
/// [`MAX_OUTPUT_BYTES`] is written whole to `<state_dir>/spill/` and replaced
/// in-context by a head preview + file path (the model can `read` it back
/// with offsets). Falls back to plain truncation if the disk write fails —
/// a spill problem must never fail the tool.
pub(crate) fn spill_or_truncate(ctx: &crate::types::tool::ToolCallCtx, s: &str) -> String {
  if s.len() <= MAX_OUTPUT_BYTES {
    return s.to_string();
  }
  use std::hash::{Hash, Hasher};
  let mut h = std::collections::hash_map::DefaultHasher::new();
  s.hash(&mut h);
  let name = format!("{:012x}.txt", h.finish());
  let dir = ctx.ctx.state_dir.join("spill");
  let path = dir.join(&name);
  match std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&path, s)) {
    Ok(()) => {
      let head: String = s.chars().take(4000).collect();
      format!(
        "[output too large for context ({} bytes); full output saved to {}; head preview follows]\n{}\n[...preview ends — use `read` with offset on that path for the rest]",
        s.len(),
        path.display(),
        head
      )
    }
    Err(e) => {
      tracing::warn!(error = %e, "spill write failed; falling back to truncation");
      truncate_output(s)
    }
  }
}
```

- [ ] **Step 4: 切换调用点**

```rust
// bash.rs:72-73
let stdout = super::spill_or_truncate(&ctx, &String::from_utf8_lossy(&output.stdout));
let stderr = super::spill_or_truncate(&ctx, &String::from_utf8_lossy(&output.stderr));
// fetch.rs:88
let text = super::spill_or_truncate(&ctx, &text);
```

注意 bash 的 ctx 在函数早段可用;fetch 的 `execute(ctx)` 同理。`truncate_output` 保留(spill 的 fallback + 既有测试仍引用)。

- [ ] **Step 5: 测试通过 + 全量回归**

Run: `cd gasket && cargo test -p gasket-core`
Expected: PASS

- [ ] **Step 6: 文档一句 + 提交**

`docs/usage.md` §9 工具节追加一行:`bash`/`fetch` 超过 200KB 的输出会完整落盘到 `~/.gasket/tool_state/<会话>/<工具>/spill/`,上下文中只保留头部预览与文件路径。

```bash
git add gasket/gasket-core/src/tools/ docs/usage.md
git commit -m "feat(core): spill oversized tool output to state_dir, keep head preview in context (dsh spill)"
```

---

### Task 3: MCP 代理统一 — pick_mcp_proxy 接入 apply_tool_proxy 体系

**Files:**
- Modify: `gasket/gasket-host/src/mcp.rs:461-473`(`McpHttpClient::connect` 内的 builder 段)
- Test: `mcp.rs` 底部 tests 模块(若无需新建)

**Interfaces:**
- Consumes: `gasket_core::tool_proxy()`(override > `GASKET_TOOL_PROXY`)
- Produces: `fn pick_mcp_proxy(lookup: &dyn Fn(&str) -> Result<String, std::env::VarError>) -> Option<String>` — 优先级:tool 代理体系 > `GASKET_LLM_PROXY` > `HTTPS_PROXY` > `https_proxy`(后三个为既有行为,零破坏)。

**设计要点:** 现状是 MCP 自读 LLM 代理 env、绕开工具代理 —— 在"桌面 UI 配了 Globe 代理"的心智模型下,远程 MCP 流量竟然不走它,这是真 bug 不是假想敌。旧 env 链保留为兜底:依赖 `GASKET_LLM_PROXY` 的既有用户(never break userspace)零感知。

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod proxy_tests {
  fn fake_env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Result<String, std::env::VarError> {
    let map: std::collections::HashMap<String, String> =
      pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    move |k: &str| map.get(k).cloned().ok_or(std::env::VarError::NotPresent)
  }

  #[test]
  fn tool_proxy_wins_over_legacy_env() {
    assert_eq!(
      super::pick_mcp_proxy(&fake_env(&[
        ("GASKET_TOOL_PROXY", "socks5://tool:1080"),
        ("GASKET_LLM_PROXY", "http://llm:8080"),
      ])),
      Some("socks5://tool:1080".to_string())
    );
  }

  #[test]
  fn legacy_llm_proxy_still_works() {
    assert_eq!(
      super::pick_mcp_proxy(&fake_env(&[("GASKET_LLM_PROXY", "http://llm:8080")])),
      Some("http://llm:8080".to_string())
    );
    assert_eq!(super::pick_mcp_proxy(&fake_env(&[])), None);
  }
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cd gasket && cargo test -p gasket-host pick_mcp`
Expected: FAIL(函数不存在)

- [ ] **Step 3: 实现 + 替换接线**

```rust
/// Proxy for remote MCP traffic: the tool-proxy system first (runtime
/// override > GASKET_TOOL_PROXY), then the legacy LLM-proxy env chain for
/// backward compatibility. Direct connection when none is set.
fn pick_mcp_proxy(
    lookup: &dyn Fn(&str) -> Result<String, std::env::VarError>,
) -> Option<String> {
  if let Some(p) = gasket_core::tool_proxy() {
    return Some(p);
  }
  ["GASKET_LLM_PROXY", "HTTPS_PROXY", "https_proxy"]
    .iter()
    .find_map(|k| lookup(k).ok().map(|s| s.trim().to_string()))
    .filter(|s| !s.is_empty())
}
```

`connect()` 内把现有 461-473 的 env 读取块整体替换为:

```rust
if let Some(proxy_url) = pick_mcp_proxy(&|k| std::env::var(k)) {
  if let Ok(proxy) = reqwest::Proxy::all(&proxy_url) {
    builder = builder.proxy(proxy);
  }
}
```

注意:`gasket_core::tool_proxy()` 直接读全局 override + 真实 env(它内部就是),fake_env 只覆盖 legacy 段 —— 测试一(tool wins)需在无 override 干扰下运行;若同机并行测试可能设置 override,复用 `gasket_core::proxy` 的测试锁思路,在测试开头 `gasket_core::set_tool_proxy(None).unwrap()`(gasket-host 已依赖 gasket-core)。

- [ ] **Step 4: 测试通过 + 回归**

Run: `cd gasket && cargo test -p gasket-host`
Expected: PASS

- [ ] **Step 5: 文档 + 提交**

`docs/usage.md` §3.3 工具代理段落补一句:远程 MCP server 的 HTTP 流量同样遵循 `GASKET_TOOL_PROXY`(兜底 `GASKET_LLM_PROXY`/`HTTPS_PROXY`)。

```bash
git add gasket/gasket-host/src/mcp.rs docs/usage.md
git commit -m "feat(host): route remote MCP traffic through the tool proxy system with legacy env fallback"
```

---

### Task 4: composition root 收敛 — gasket_ext::prod_register

**Files:**
- Modify: `gasket/gasket-ext/src/lib.rs`
- Modify: `web/src-tauri/src/chat.rs:321-329`
- Test: `gasket-ext/src/lib.rs` tests(或既有 tests)

**Interfaces:**
- Produces: `pub fn prod_register(api: &mut dyn ExtensionApi)` — 只注册生产扩展(当前仅 `search`),供桌面端等非 demo 宿主使用。`register_all`(含 hello/todo/permission_gate demo)保持不变,CLI 继续用它。

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn prod_register_has_search_only() {
  let mut api = gasket_core::ExtensionApiImpl::new();
  prod_register(&mut api);
  let names: Vec<_> = api.tools.iter().map(|t| t.name.clone()).collect();
  assert_eq!(names, vec!["web_search".to_string()]);
}
```

(gasket-ext dev-deps 已含 tokio;此测试不需要,直接可跑。)

- [ ] **Step 2: 运行确认失败**

Run: `cd gasket && cargo test -p gasket-ext prod_register`
Expected: FAIL

- [ ] **Step 3: 实现**

```rust
/// Production extensions only (no demo tools). Hosts whose users did not
/// opt into the demo set (the desktop app) compose from here; the CLI keeps
/// [`register_all`] behind `--features ext`.
pub fn prod_register(api: &mut dyn gasket_core::ExtensionApi) {
  search::register(api);
}
```

`register_all` 改为在 prod 之上追加 demo(`hello`/`todo`/`permission_gate`),保持单一事实源:demo 清单只出现在一处。

- [ ] **Step 4: 桌面端切换**

`web/src-tauri/src/chat.rs` 中手工的 `let mut api = ...; gasket_ext::search::register(&mut api); api.tools` 整块替换为:

```rust
let search_tools = {
  let mut api = gasket_core::ExtensionApiImpl::new();
  gasket_ext::prod_register(&mut api);
  api.tools
};
```

- [ ] **Step 5: 测试 + 编译验证**

Run: `cd gasket && cargo test -p gasket-ext && cargo check` 及 `cd web/src-tauri && cargo check`
Expected: PASS / 无错误

- [ ] **Step 6: 提交**

```bash
git add gasket/gasket-ext/src/lib.rs web/src-tauri/src/chat.rs
git commit -m "refactor(ext): prod_register as the non-demo composition root, desktop uses it"
```

---

## P1 / P2 可行性结论(本 plan 不实施,另立 plan)

按 scope-check 原则,以下各项相互独立、各自成 plan。此处只锁定可行性判定与接口契约,防止将来立 plan 时重新争论。

### P1: bash sandbox(dsh `sandbox/`)

**可行性:可行,工程量最大的借鉴项。**
- macOS:`sandbox-exec`(Seatbelt)生成 profile 限制 fs 写入范围(cwd + tmp)与进程派生;系统自带,零依赖。注意 Apple 已标记 deprecated 但仍可用。
- Linux:`landlock` crate(纯 Rust,unprivileged)在 `pre_exec` 中施加 FS 隔离。
- Windows:无等价物 → 不做,维持审批防线。
- **接入点**:`bash.rs` 的 `tokio::process::Command` 构造段(`cmd.env_clear()` 附近),opt-in 开关 `GASKET_SANDBOX=1`(这是唯一允许的新 env)。
- **契约**:`fn confine(cmd: &mut tokio::process::Command, cwd: &Path) -> Result<(), String>`,失败即拒绝执行(fail-closed,与工具代理的 fail-open 相反 —— 安全边界不静默降级)。
- **风险**:profile 过严导致正常命令失败,需要真实使用迭代;这正是它需要独立 plan 的原因。

### P2: 持久 PTY(dsh `terminal/`)

**可行性:可行,但触碰进程生命周期管理。**
- 库:`portable-pty`(wezterm 系,跨平台)。
- **契约**:`static PTYS: RwLock<HashMap<String /* session_id */, PtyHandle>>`(与 `proxy::OVERRIDE` 同型的进程内全局);工具名 `terminal`,`{ "session": "default", "action": "run|read|send", "command": "..." }`。
- **风险**:子进程收割、超时后 PTY 泄漏、并发读写。独立 plan 先解决生命周期,再谈工具面。

### P2: skills(dsh `skill/`)

**可行性:低成本,推荐早做。**
- 扫描 `~/.gasket/skills/*.md` 与 `<cwd>/.gasket/skills/*.md`,frontmatter 取 name/description,目录 + 指令注入 system prompt。
- **接入点**:system prompt 组装处(host 侧);不新增工具、不加注册表抽象 —— 就是文件扫描 + 字符串拼接。

### P2: session-query(dsh `session-query/`)

**可行性:可行,依赖 SQLite 引入,最后做。**
- events.jsonl 天然可索引;SQLite FTS5 sidecar(`~/.gasket/index.db`)。
- **前置条件**:会话量先成为真实痛点(个人使用 <1000 会话时,YAGNI)。

---

## 验收清单(P0 完成的定义)

- [ ] `cargo test --all-features` 全绿(core/host/ext)
- [ ] `cargo clippy --all-features -- -D warnings` 干净
- [ ] 桌面端 `cd web/src-tauri && cargo check` 通过
- [ ] 手工冒烟:CLI `--mode=full-auto` 连续三次同参 `bash` 调用,第三次结果尾部出现 advisory;`bash "cat 大文件"` 输出超 200KB 时收到 spill stub 且文件可 `read` 回
- [ ] `docs/usage.md` 三处追加(spill 一句、MCP 代理一句)已合入

## Self-Review 记录

- 覆盖检查:review 提出的 6 项借鉴中 4 项入 P0 任务,2 项(sandbox、PTY)+ 2 项轻量项(skills、session-query)有可行性结论与接口契约 ✓
- 类型一致性:`RepeatGuard::observe(&str, &str) -> u32`、`spill_or_truncate(&ToolCallCtx, &str) -> String`、`pick_mcp_proxy(&dyn Fn) -> Option<String>`、`prod_register(&mut dyn ExtensionApi)` 在任务间无交叉消费,签名各任务内自洽 ✓
- 占位符扫描:P1/P2 段为显式 scope-check 延后(非 TBD),P0 四任务每步有可执行代码 ✓
