# Terminal PTY Tool Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 gasket-ext 新增可选工具 `terminal`(Cargo feature `terminal`,默认关闭):通过 PTY 运行命令,支持 `run` / `read` / `send` 三种动作,输出经 ring buffer 缓冲、按需排空,可驱动交互式程序。gasket-core 零改动 —— 零新增依赖、零进程状态。

**Architecture:** 新文件 `gasket/gasket-ext/src/terminal.rs`,经 `#[cfg(feature = "terminal")] pub mod terminal;` 门控;注册走 ext 的既有模式 —— `pub fn register(api: &mut dyn gasket_core::ExtensionApi)` 内 `api.register_tool(...)`(与 `search.rs::register` 同形),并由两个组合根 `register_all` / `prod_register` 以同样的 cfg 门控调用。进程级 `LazyLock<RwLock<HashMap>>` 会话注册表(key = `<ToolContext session_id>/<session 参数>`)也住在这个 ext 模块里(与 core `proxy.rs` 的全局 override 同一模式哲学);每个 `PtySession` 持有 portable-pty child + writer + 一个 `Arc<Mutex<OutputRing>>`(64KiB 上限的 `VecDeque<String>`),一个阻塞读线程持续从 PTY reader 泵入 ring。`read` 排空 ring 并轮询 child 退出状态,直接返回排空文本 —— 64KiB 环本身就是输出上界(ext 不做落盘 spill,见 Global Constraints);同 key 的新 `run` 会先 kill 存活旧进程。主机按需 opt-in:desktop 在 `web/src-tauri/Cargo.toml` 给 gasket-ext 依赖加 `features = ["terminal"]`;CLI 经其 `ext` feature 转发(`gasket-ext?/terminal`)。

**Tech Stack:** Rust,`portable-pty = { version = "0.8", optional = true }`(仅 gasket-ext,由 feature `terminal` 引入),tokio(ext 已有 dev-dependency)。

## Global Constraints

- Rust 工作区:`/Users/yeheng/workspaces/Github/gasket/gasket`;cargo 命令在该目录运行,git 命令在仓库根 `/Users/yeheng/workspaces/Github/gasket` 运行。
- **gasket-core gains no new dependency and no process-registry state; terminal is opt-in via gasket-ext feature `terminal` (default off)。** 本计划不创建、不修改 gasket-core 下任何文件,也不改 core 的可见性。
- 格式:`gasket/rustfmt.toml` — 4 空格缩进、`max_width = 100`。CI 门禁:`cargo fmt --check`、`cargo clippy --all-features --all-targets -D warnings`、`cargo test --all-features`(注意:`--all-features` 会启用 `terminal`,见下条与 Task 2 的既有测试适配)。
- 工具名 `terminal`,`RiskLevel::Medium`;参数:`action`(必填,"run"|"read"|"send")、`command`(run/send 用)、`session`(默认 "default")。
- portable-pty 是同步 API:阻塞 IO 全部放进 `std::thread`,锁临界区内不得跨 `.await`。
- 输出上界 = 64KiB ring 本身:`read` 直接返回排空文本。core 的 `spill_or_truncate` 是 `pub(crate)`、未对 ext 导出 —— 本计划不改 core 可见性(ext 的 search/web 工具同样不做 spill,此为一致先例);若日后 ext 需要 spill,应由 core 显式导出该能力,不在本计划内。
- 验证命令口径:带 feature 的测试用 `cargo test -p gasket-ext --features terminal`;`cargo check -p gasket-ext`(不带 feature)也必须通过 —— 模块被 cfg 掉,默认构建零影响。
- 已知限制(明确不做):会话移除时的 `reap`/自动回收不在本计划内 —— 孤儿会话由同 key 新 `run` 覆盖时 kill,进程退出后自然消亡;输出不落盘、超 64KiB 丢最旧;仅启用 feature 的主机能看到该工具。
- Commit:conventional commits,每 task 一个。

---

### Task 1: OutputRing 环形缓冲(纯逻辑)+ feature 骨架

**Files:**
- Create: `gasket/gasket-ext/src/terminal.rs`(本 task 只含 `OutputRing` + tests)
- Modify: `gasket/gasket-ext/Cargo.toml`([dependencies] `dom_query = "0.28"` 之后加 `portable-pty = { version = "0.8", optional = true }`;新增 `[features]` 段 `terminal = ["dep:portable-pty"]`;[dev-dependencies] `tokio` 已存在(workspace tokio 自带 `macros`/`rt-multi-thread`/`time`,`#[tokio::test]` 可用),补 `tempfile = "3"` —— Task 2/3 的测试用 tempdir)
- Modify: `gasket/gasket-ext/src/lib.rs`(`pub mod todo;` 之后加 `#[cfg(feature = "terminal")] pub mod terminal;`)

**Interfaces:**
- Produces(Task 2/3 依赖):
  - `struct OutputRing { chunks: std::collections::VecDeque<String>, bytes: usize }`
  - `impl OutputRing { fn push_str(&mut self, s: &str); fn drain(&mut self) -> String; const MAX_BYTES: usize = 64 * 1024; }`

- [ ] **Step 1: Write the failing test**

`terminal.rs`:

```rust
//! `terminal` tool — run commands on a PTY, with run/read/send actions and a
//! per-session output ring buffer. Lives in gasket-ext behind Cargo feature
//! `terminal`; the session registry is process-global within this crate.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_returns_and_clears() {
        let mut r = OutputRing::default();
        r.push_str("hello\n");
        r.push_str("world\n");
        assert_eq!(r.drain(), "hello\nworld\n");
        assert_eq!(r.drain(), "", "second drain is empty");
    }

    #[test]
    fn cap_evicts_oldest_first() {
        let mut r = OutputRing::default();
        // 2 chunks of half the cap -> over cap after the second push.
        r.push_str(&"a".repeat(OutputRing::MAX_BYTES / 2));
        r.push_str(&"b".repeat(OutputRing::MAX_BYTES / 2));
        let out = r.drain();
        assert!(out.starts_with('b'), "oldest chunk evicted first");
        assert!(out.len() <= OutputRing::MAX_BYTES);
    }

    #[test]
    fn oversized_single_chunk_is_truncated_to_cap() {
        let mut r = OutputRing::default();
        r.push_str(&"x".repeat(OutputRing::MAX_BYTES * 2));
        assert!(r.drain().len() <= OutputRing::MAX_BYTES);
    }
}
```

- [ ] **Step 2: Run to verify it fails**

```bash
cd /Users/yeheng/workspaces/Github/gasket/gasket && cargo test -p gasket-ext --features terminal
```

预期:编译错(`OutputRing` 未定义)。同时 `cargo check -p gasket-ext`(不带 feature)必须通过 —— 模块被 cfg 掉,默认构建不受影响。

- [ ] **Step 3: Minimal implementation**

`gasket/gasket-ext/Cargo.toml` 增补(保留既有条目):

```toml
[dependencies]
# ... 既有 gasket-core / serde / serde_json / reqwest / tracing / urlencoding / dom_query ...
portable-pty = { version = "0.8", optional = true }

[features]
terminal = ["dep:portable-pty"]

[dev-dependencies]
tokio = { workspace = true }
tempfile = "3"
```

`gasket/gasket-ext/src/lib.rs` 模块声明(`pub mod todo;` 之后):

```rust
#[cfg(feature = "terminal")]
pub mod terminal;
```

`terminal.rs` 顶部(`#[cfg(test)] mod tests` 之前):

```rust
use std::collections::VecDeque;

/// Rolling output buffer for one PTY session, capped at MAX_BYTES: pushing
/// past the cap evicts whole oldest chunks until back under it.
struct OutputRing {
    chunks: VecDeque<String>,
    bytes: usize,
}

impl OutputRing {
    const MAX_BYTES: usize = 64 * 1024;

    fn push_str(&mut self, s: &str) {
        let mut s = s.to_string();
        if s.len() > Self::MAX_BYTES {
            // Char-safe tail: never slice through a multi-byte char.
            let mut cut = Self::MAX_BYTES;
            while !s.is_char_boundary(cut) {
                cut -= 1;
            }
            s = s[cut..].to_string();
        }
        self.bytes += s.len();
        self.chunks.push_back(s);
        while self.bytes > Self::MAX_BYTES {
            let Some(front) = self.chunks.pop_front() else { break };
            self.bytes -= front.len();
        }
    }

    /// Take everything buffered (empty string when nothing new).
    fn drain(&mut self) -> String {
        let out: String = self.chunks.drain(..).collect();
        self.bytes = 0;
        out
    }
}

impl Default for OutputRing {
    fn default() -> Self {
        Self { chunks: VecDeque::new(), bytes: 0 }
    }
}
```

- [ ] **Step 4: Run to verify it passes**

同 Step 2 命令,预期 `test result: ok`(3 passed)。`cargo check -p gasket-ext`(无 feature)与 `cargo fmt --check` 通过。

- [ ] **Step 5: Commit**

```bash
cd /Users/yeheng/workspaces/Github/gasket
git add gasket/gasket-ext/Cargo.toml gasket/gasket-ext/src/terminal.rs gasket/gasket-ext/src/lib.rs
git commit -m "feat(ext): capped OutputRing for terminal tool sessions (feature \"terminal\")"
```

---

### Task 2: 会话注册表 + run/read 接线 + 组合根注册

**Files:**
- Modify: `gasket/gasket-ext/src/terminal.rs`(加 `PtySession`、`REGISTRY`、`register()`、run/read 分支)
- Modify: 本文件 tests(端到端:`echo hello` 经注册出的 tool `execute` 跑通)
- Modify: `gasket/gasket-ext/src/lib.rs`(`register_all` / `prod_register` 各加 cfg 门控的 `terminal::register(api);`,并让既有测试 `prod_register_has_search_only` 变为 feature 感知 —— CI 的 `cargo test --all-features` 会启用 `terminal`,原断言 `["web_search"]` 会假失败)

**Interfaces:**
- Produces(Task 3 依赖):
  - `pub fn register(api: &mut dyn gasket_core::ExtensionApi)` —— 经 `api.register_tool` 注册,name `"terminal"`,risk `RiskLevel::Medium`(与 `search.rs::register` 同形)
  - `struct PtySession { child: Box<dyn portable_pty::Child + Send>, writer: Box<dyn std::io::Write + Send>, ring: std::sync::Arc<std::sync::Mutex<OutputRing>> }`
  - `static REGISTRY: std::sync::LazyLock<std::sync::RwLock<std::collections::HashMap<String, std::sync::Arc<std::sync::Mutex<PtySession>>>>>`
  - key 规则:`format!("{}/{}", ctx.ctx.session_id, session_arg)`

> **API 核对要求(必做):** 下面 Step 3 的 portable-pty 调用是 0.8 API 的参考代码(`native_pty_system` / `openpty` / `pair.slave.spawn_command` / `pair.master.try_clone_reader` / `pair.master.take_writer`)。实现时先以 `cargo check -p gasket-ext --features terminal` 对照真实 crate 签名核对(尤其 `CommandBuilder` 的 env/cwd 方法与 `Child` 的 `try_wait` 返回形状),再定稿。

- [ ] **Step 1: Write the failing test**

`terminal.rs` tests 追加(工具获取方式镜像 `search.rs` 的 `web_search_goes_through_tool_proxy`:`ExtensionApiImpl::new()` → `register` → `api.tools.remove(0)`):

```rust
    use gasket_core::{ContentBlock, ExtensionApiImpl, ToolCallCtx, ToolContext, ToolResult};
    use std::sync::Arc;

    fn registered_tool() -> gasket_core::ToolDefinition {
        let mut api = ExtensionApiImpl::new();
        super::register(&mut api);
        assert_eq!(api.tools.len(), 1);
        assert_eq!(api.tools[0].name, "terminal");
        api.tools.remove(0)
    }

    async fn exec(args: serde_json::Value, cwd: &std::path::Path, session: &str) -> ToolResult {
        let t = registered_tool();
        (t.execute)(ToolCallCtx {
            tool_call_id: "x".into(),
            args,
            signal: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            ctx: ToolContext {
                cwd: cwd.to_path_buf(),
                env: std::env::vars().collect(),
                session_id: session.into(),
                state_dir: cwd.to_path_buf(),
                spawner: None,
            },
        })
        .await
        .unwrap()
    }

    fn text(r: &ToolResult) -> String {
        match &r.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text"),
        }
    }

    #[tokio::test]
    async fn run_then_read_returns_output_and_exit() {
        let tmp = tempfile::tempdir().unwrap();
        // unique session key so parallel tests never share a registry slot
        let s = format!("run-read-{}", std::process::id());
        let r = exec(serde_json::json!({"action": "run", "command": "echo hello"}), tmp.path(), &s).await;
        assert!(!r.is_error, "spawn failed");
        // Poll read until the child exits and output shows up (pump thread is async).
        let mut got = String::new();
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let r = exec(serde_json::json!({"action": "read"}), tmp.path(), &s).await;
            got = text(&r);
            if got.contains("[exited") {
                break;
            }
        }
        assert!(got.contains("hello"), "got: {got}");
        assert!(got.contains("[exited code 0]"), "got: {got}");
    }

    #[tokio::test]
    async fn read_with_no_session_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let r = exec(serde_json::json!({"action": "read"}), tmp.path(), "never-spawned").await;
        assert!(!r.is_error);
        assert_eq!(text(&r), "no active session");
    }
```

- [ ] **Step 2: Run to verify it fails**

```bash
cd /Users/yeheng/workspaces/Github/gasket/gasket && cargo test -p gasket-ext --features terminal
```

预期:编译错(`register` 未定义)。

- [ ] **Step 3: Minimal implementation**

`terminal.rs` 追加:

```rust
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, LazyLock, Mutex, RwLock};

use gasket_core::{
    ContentBlock, ExtensionApi, RiskLevel, ToolCallCtx, ToolDefinition, ToolError, ToolResult,
};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};

struct PtySession {
    child: Box<dyn portable_pty::Child + Send>,
    writer: Box<dyn Write + Send>,
    ring: Arc<Mutex<OutputRing>>,
}

/// Sessions keyed by `<tool session_id>/<name>`; same global-state pattern
/// as gasket-core's `proxy.rs` override. Known limitation: no reaper — a
/// session is killed only when a new `run` reuses its key.
static REGISTRY: LazyLock<RwLock<HashMap<String, Arc<Mutex<PtySession>>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Same registration shape as `search.rs::register`.
pub fn register(api: &mut dyn ExtensionApi) {
    api.register_tool(ToolDefinition {
        name: "terminal".into(),
        label: "Terminal".into(),
        description: "Run commands on a PTY. action: run (spawn), read (drain new \
                      output + exit status), send (write to stdin)."
            .into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["run", "read", "send"] },
                "command": { "type": "string", "description": "command (run) or input line (send)" },
                "session": { "type": "string", "description": "session name (default \"default\")" }
            },
            "required": ["action"]
        }),
        risk: RiskLevel::Medium,
        execute: Arc::new(|ctx| Box::pin(execute(ctx))),
    });
}

async fn execute(ctx: ToolCallCtx) -> Result<ToolResult, ToolError> {
    let action = ctx.args["action"]
        .as_str()
        .ok_or_else(|| ToolError::Message("action is required".into()))?;
    let session_name = ctx.args["session"].as_str().unwrap_or("default");
    let key = format!("{}/{}", ctx.ctx.session_id, session_name);
    match action {
        "run" => run(&ctx, &key),
        "read" => read(&key).await,
        "send" => send(&ctx, &key),
        other => Ok(ToolResult::error(format!("unknown action: {other}"))),
    }
}

fn run(ctx: &ToolCallCtx, key: &str) -> Result<ToolResult, ToolError> {
    let command = ctx.args["command"]
        .as_str()
        .ok_or_else(|| ToolError::Message("command is required for run".into()))?;

    // Replace any live session under this key (kill + drop).
    if let Some(old) = REGISTRY.write().unwrap().remove(key) {
        let mut s = old.lock().unwrap();
        let _ = s.child.kill();
    }

    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize { rows: 24, cols: 80, ..Default::default() })
        .map_err(|e| ToolError::Message(format!("openpty failed: {e}")))?;
    let mut cmd = CommandBuilder::new(if cfg!(target_os = "windows") { "cmd" } else { "sh" });
    if cfg!(target_os = "windows") {
        cmd.arg("/C");
    } else {
        cmd.arg("-c");
    }
    cmd.arg(command);
    cmd.cwd(&ctx.ctx.cwd);
    // Host env is already scrubbed of GASKET_* secrets by the host; inherit.
    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| ToolError::Message(format!("spawn failed: {e}")))?;
    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| ToolError::Message(format!("pty reader failed: {e}")))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|e| ToolError::Message(format!("pty writer failed: {e}")))?;

    let ring = Arc::new(Mutex::new(OutputRing::default()));
    let pump_ring = Arc::clone(&ring);
    // Blocking reads live on a plain thread — portable-pty is sync IO.
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => pump_ring
                    .lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&buf[..n])),
            }
        }
    });

    REGISTRY.write().unwrap().insert(
        key.to_string(),
        Arc::new(Mutex::new(PtySession { child, writer, ring })),
    );
    Ok(ToolResult {
        content: vec![ContentBlock::text(format!("session `{key}` started"))],
        details: serde_json::json!({"session": key}),
        is_error: false,
    })
}

async fn read(key: &str) -> Result<ToolResult, ToolError> {
    let Some(sess) = REGISTRY.read().unwrap().get(key).cloned() else {
        return Ok(ToolResult {
            content: vec![ContentBlock::text("no active session")],
            details: serde_json::json!({"exited": true}),
            is_error: false,
        });
    };
    // Lock scope ends before any await; child poll is non-blocking. The ring
    // itself bounds output at 64KiB — core's spill_or_truncate is pub(crate)
    // and not exported, so ext returns the drained text directly (consistent
    // with ext's search/web tools, which don't spill either).
    let (mut text, status) = {
        let mut s = sess.lock().unwrap();
        let out = s.ring.lock().unwrap().drain();
        // try_wait: Ok(Some(status)) = exited, Ok(None) = still running.
        let status = s
            .child
            .try_wait()
            .map_err(|e| ToolError::Message(format!("wait failed: {e}")))?;
        (out, status)
    };
    if let Some(code) = status.map(|s| s.exit_code() as i32) {
        text.push_str(&format!("\n[exited code {code}]"));
    }
    Ok(ToolResult {
        content: vec![ContentBlock::text(text.trim())],
        details: serde_json::json!({
            "exited": status.is_some(),
            "exit_code": status.map(|s| s.exit_code() as i32)
        }),
        is_error: false,
    })
}

fn send(ctx: &ToolCallCtx, key: &str) -> Result<ToolResult, ToolError> {
    let _ = ctx;
    let Some(sess) = REGISTRY.read().unwrap().get(key).cloned() else {
        return Ok(ToolResult::error(format!("no session `{key}`")));
    };
    // implemented fully in Task 3 (needs stdin line semantics)
    drop(sess);
    Ok(ToolResult::error("send not yet implemented".into()))
}
```

`gasket/gasket-ext/src/lib.rs` 组合根(两处都加,cfg 门控):

```rust
pub fn prod_register(api: &mut dyn gasket_core::ExtensionApi) {
    search::register(api);
    #[cfg(feature = "terminal")]
    terminal::register(api);
}

pub fn register_all(api: &mut dyn ExtensionApi) {
    prod_register(api);
    hello::register(api);
    todo::register(api);
    permission_gate::register(api);
}
```

既有测试 `prod_register_has_search_only` 改为 feature 感知(否则 CI 的 `--all-features` 下假失败):

```rust
    #[test]
    fn prod_register_has_search_only() {
        let mut api = gasket_core::ExtensionApiImpl::new();
        prod_register(&mut api);
        let names: Vec<_> = api.tools.iter().map(|t| t.name.clone()).collect();
        // `--all-features` (CI) turns the terminal feature on; without it the
        // module is compiled out entirely.
        let expected: Vec<&str> = if cfg!(feature = "terminal") {
            vec!["web_search", "terminal"]
        } else {
            vec!["web_search"]
        };
        assert_eq!(names, expected);
    }
```

> 先 `cargo check -p gasket-ext --features terminal` 核对 `try_wait`/`exit_code`/`PtySize`/`kill` 的真实签名(0.8),有出入以 crate 为准调整,再进 Step 4。

- [ ] **Step 4: Run to verify it passes**

```bash
cd /Users/yeheng/workspaces/Github/gasket/gasket && cargo test -p gasket-ext --features terminal
cargo test -p gasket-ext   # feature off: terminal compiled out, existing suite (incl. adjusted prod_register test) stays green
cargo clippy --all-features --all-targets -D warnings && cargo fmt --check
```

预期:`run_then_read_returns_output_and_exit`、`read_with_no_session_is_empty` 与 Task 1 的 3 个 ring 测试全绿;无 feature 构建与 clippy(`--all-features` 含 `terminal`)全绿。

- [ ] **Step 5: Commit**

```bash
cd /Users/yeheng/workspaces/Github/gasket
git add gasket/gasket-ext/src/terminal.rs gasket/gasket-ext/src/lib.rs
git commit -m "feat(ext): terminal PTY tool with session registry, run/read actions"
```

---

### Task 3: send 动作 + 主机接入(desktop/CLI)+ 文档

**Files:**
- Modify: `gasket/gasket-ext/src/terminal.rs`(`send` 真实现 + 交互测试)
- Modify: `web/src-tauri/Cargo.toml`(line 31 `gasket-ext = { path = "../../gasket/gasket-ext" }` 加 `features = ["terminal"]` —— desktop 走 `chat.rs` 的 `prod_register`,组合根已覆盖)
- Modify: `gasket/gasket-cli/Cargo.toml`(`[features]` 的 `ext` 转发 terminal)
- Modify: `docs/usage.md`(§9.1,line 258 段落末尾加一句)

**Interfaces:**
- Consumes:Task 2 的 `PtySession.writer`、`REGISTRY`、`register()`。
- Produces:完整 `terminal` 工具(ext 可选工具;core 的 8 个内置工具不变 —— terminal 不进 `built_in_tools()`,只在启用 feature 且主机调用 `register_all`/`prod_register` 时注册)。

- [ ] **Step 1: Write the failing test**

`terminal.rs` tests 追加:

```rust
    #[tokio::test]
    async fn send_writes_to_stdin_of_running_child() {
        let tmp = tempfile::tempdir().unwrap();
        let s = format!("send-{}", std::process::id());
        // `read` a line then echo it back — proves stdin round-trip through the PTY.
        let r = exec(
            serde_json::json!({"action": "run", "command": "read line; echo got:$line"}),
            tmp.path(),
            &s,
        ).await;
        assert!(!r.is_error);
        let r = exec(serde_json::json!({"action": "send", "command": "ping"}), tmp.path(), &s).await;
        assert!(!r.is_error, "send failed");
        let mut got = String::new();
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let r = exec(serde_json::json!({"action": "read"}), tmp.path(), &s).await;
            got = text(&r);
            if got.contains("got:ping") {
                break;
            }
        }
        assert!(got.contains("got:ping"), "got: {got}");
    }
```

- [ ] **Step 2: Run to verify it fails**

```bash
cd /Users/yeheng/workspaces/Github/gasket/gasket && cargo test -p gasket-ext --features terminal send_writes
```

预期:失败 —— `send` 仍是 Task 2 的 `"send not yet implemented"` stub,`is_error` 为 true。

- [ ] **Step 3: Minimal implementation**

替换 `send`:

```rust
fn send(ctx: &ToolCallCtx, key: &str) -> Result<ToolResult, ToolError> {
    let input = ctx.args["command"]
        .as_str()
        .ok_or_else(|| ToolError::Message("command is required for send".into()))?;
    let Some(sess) = REGISTRY.read().unwrap().get(key).cloned() else {
        return Ok(ToolResult::error(format!("no session `{key}`")));
    };
    let mut s = sess.lock().unwrap();
    // PTY stdin is line-oriented: always terminate with a newline.
    s.writer
        .write_all(input.as_bytes())
        .and_then(|_| s.writer.write_all(b"\n"))
        .and_then(|_| s.writer.flush())
        .map_err(|e| ToolError::Message(format!("stdin write failed: {e}")))?;
    Ok(ToolResult {
        content: vec![ContentBlock::text("sent")],
        details: serde_json::json!({"session": key}),
        is_error: false,
    })
}
```

主机 opt-in(两处):

`web/src-tauri/Cargo.toml`(desktop):

```toml
gasket-ext = { path = "../../gasket/gasket-ext", features = ["terminal"] }
```

`gasket/gasket-cli/Cargo.toml`(`gasket-ext?/terminal` 是 weak dependency feature:仅在 `ext` 已启用时转发,不单独引入 dep;CLI 的 `load_inprocess_ext` 走 `register_all`,无需改 main.rs):

```toml
[features]
default = []
# Link optional in-process extensions (hello / todo / permission_gate),
# plus the terminal PTY tool (forwarded feature).
ext = ["dep:gasket-ext", "gasket-ext?/terminal"]
```

`docs/usage.md` §9.1(line 258,段末)追加:

```markdown
`terminal` 工具位于 gasket-ext,默认关闭:主机在 gasket-ext 依赖上启用 `terminal` feature 后经其扩展注册入口生效(桌面端已启用;CLI 随 `--features ext` 一并启用)。它通过 PTY 运行命令:action=`run` 启动(同名 session 存活时旧进程被 kill)、`read` 排空新输出并报告退出状态、`send` 向运行中进程的 stdin 写入一行,适合驱动交互式程序;会话按 `session` 参数(默认 `default`)区分,输出经 64KiB 环形缓冲按需排空、超限丢弃最旧输出(与 `bash`/`fetch` 的 200KB 落盘 spill 不同,该工具不落盘)。
```

- [ ] **Step 4: Run to verify it passes**

```bash
cd /Users/yeheng/workspaces/Github/gasket/gasket && cargo test -p gasket-ext --features terminal && cargo test && cargo clippy --all-features --all-targets -D warnings && cargo fmt --check
cargo check -p gasket-ext   # 无 feature 也必须通过(默认构建零影响)
cd /Users/yeheng/workspaces/Github/gasket/web/src-tauri && cargo check   # desktop opt-in 可编译
```

预期:全绿,含 send 测试、run/read 测试与既有套件;`cargo test --all-features` 下 `prod_register_has_search_only` 按 feature 感知断言通过;gasket-core 无任何改动(`git status` 不含 gasket-core 路径)。

- [ ] **Step 5: Commit**

```bash
cd /Users/yeheng/workspaces/Github/gasket
git add gasket/gasket-ext/src/terminal.rs gasket/gasket-cli/Cargo.toml web/src-tauri/Cargo.toml docs/usage.md
git commit -m "feat(ext): terminal send action, host opt-in (desktop/cli), usage docs"
```
