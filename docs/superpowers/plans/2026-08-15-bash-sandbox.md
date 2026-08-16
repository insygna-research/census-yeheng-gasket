# Bash Sandbox (GASKET_SANDBOX) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `bash` 工具在 `GASKET_SANDBOX=1` 时获得 OS 级文件系统隔离(写操作仅限 cwd / TMPDIR / var/tmp),默认关闭、行为零变化,无法施加隔离时拒绝执行(fail-closed)。

**Architecture:** 新增 `tools/sandbox.rs`,暴露唯一入口 `confine(cmd, cwd)`;`bash.rs` 在构建完 program/args 之后、设置 `current_dir` 之前调用它(重写 argv 时不能丢失 cwd/env 配置)。macOS 用 `sandbox-exec -p <profile>` 重写 argv,profile 由纯函数 `seatbelt_profile` 生成;Linux 在 `pre_exec` 里施加 Landlock(全盘只读 + cwd/tmp 可写);其余平台直接报错。开关从 `ToolContext.env` 读取(宿主从进程 env 注入),`bash.rs` 现有的 `GASKET_*` 过滤恰好阻止该变量泄漏给子进程。后端按平台 opt-in:macOS 后端零依赖,Linux Landlock 后端藏在 `sandbox-landlock` feature 之后(默认关闭),构建缺失该后端时 fail-closed。

**Tech Stack:** Rust(tokio process),macOS `sandbox-exec`(系统自带、零依赖),`landlock = "0.4"`(仅 Linux target、`sandbox-landlock` feature 下的可选依赖)。

## Global Constraints

- Rust 工作区:`/Users/yeheng/workspaces/Github/gasket/gasket`;cargo 命令在该目录运行,git 命令在仓库根 `/Users/yeheng/workspaces/Github/gasket` 运行。
- 格式:`gasket/rustfmt.toml` — 4 空格缩进、`max_width = 100`。CI 门禁:`cargo fmt --check`、`cargo clippy --all-features --all-targets -D warnings`、`cargo test --all-features`。
- 唯一新增旋钮:env 变量 `GASKET_SANDBOX=1`。默认关闭,关闭时零行为变化。
- landlock 是 opt-in、target-scoped 的可选依赖(feature `sandbox-landlock`,默认关闭);唯一运行时旋钮仍是 `GASKET_SANDBOX=1`;后端缺失或失败一律 fail-closed。
- Fail-closed:隔离无法施加 → 命令被拒绝,返回 error `ToolResult`(对比:HTTP 代理 fail-open;安全边界 fail-closed)。
- 测试:plain `#[tokio::test]` / `#[test]`,in-module,`tempfile`(已有 dev-dep)。
- Commit:conventional commits(`feat:` / `test:` / `docs:`),每 task 一个。

---

### Task 1: macOS seatbelt profile 纯函数

**Files:**
- Create: `gasket/gasket-core/src/tools/sandbox.rs`(本 task 只含 cfg(target_os = "macos") 的 `seatbelt_profile` + tests)
- Modify: `gasket/gasket-core/src/tools/mod.rs`(line 9 `pub mod subagent;` 之后插入 `pub mod sandbox;`)

**Interfaces:**
- Produces(Task 2 依赖):`#[cfg(target_os = "macos")] fn seatbelt_profile(cwd: &str, tmp: &str) -> String` — 纯函数、无 IO;cwd/tmp 由调用方 canonicalize 后传入。

- [ ] **Step 1: Write the failing test**

在新建的 `sandbox.rs` 末尾写测试(函数体暂用 `unimplemented!()`,编译通过、运行失败即为 failing 状态):

```rust
//! Filesystem confinement for the `bash` tool, enabled by GASKET_SANDBOX=1.
//! Fail-closed: if confinement cannot be applied, the command is refused.

/// Generate a Seatbelt (sandbox-exec) SBPL profile: allow everything broadly,
/// deny file writes everywhere except cwd / tmp / var/tmp. Pure function.
#[cfg(target_os = "macos")]
fn seatbelt_profile(cwd: &str, tmp: &str) -> String {
    unimplemented!()
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn profile_allows_read_execute_and_denies_write_by_default() {
        let p = seatbelt_profile("/tmp/cwd", "/tmp/dir");
        assert!(p.contains("(version 1)"), "{p}");
        assert!(p.contains("(allow default)"), "read/exec broadly: {p}");
        assert!(p.contains("(deny file-write*)"), "deny writes: {p}");
        assert!(p.contains("(allow file-write* (subpath \"/tmp/cwd\"))"), "{p}");
        assert!(p.contains("(allow file-write* (subpath \"/tmp/dir\"))"), "{p}");
    }

    #[test]
    fn profile_includes_var_tmp_unconditionally() {
        let p = seatbelt_profile("/x", "/y");
        assert!(p.contains("(allow file-write* (subpath \"/var/tmp\"))"), "{p}");
    }
}
```

- [ ] **Step 2: Run to verify it fails**

```bash
cd /Users/yeheng/workspaces/Github/gasket/gasket && cargo test -p gasket-core sandbox
```

预期:macOS 上两个测试 panic(`not implemented`)。注意 `mod.rs` 忘加 `pub mod sandbox;` 会报 "file not found" 编译错,同样算失败态。

- [ ] **Step 3: Minimal implementation**

```rust
#[cfg(target_os = "macos")]
fn seatbelt_profile(cwd: &str, tmp: &str) -> String {
    format!(
        "(version 1)\n\
         (allow default)\n\
         (deny file-write*)\n\
         (allow file-write* (subpath \"{cwd}\"))\n\
         (allow file-write* (subpath \"{tmp}\"))\n\
         (allow file-write* (subpath \"/var/tmp\"))\n"
    )
}
```

`(allow default)` 保持读/执行/网络宽松(只做文件系统写隔离);SBPL 中 subpath 规则比泛化规则更具体,不受声明顺序影响。

- [ ] **Step 4: Run to verify it passes**

同 Step 2 命令,预期 `test result: ok`(2 passed)。再跑 `cargo clippy --all-features --all-targets -D warnings`,预期零告警。

- [ ] **Step 5: Commit**

```bash
cd /Users/yeheng/workspaces/Github/gasket
git add gasket/gasket-core/src/tools/sandbox.rs gasket/gasket-core/src/tools/mod.rs
git commit -m "feat(core): pure seatbelt profile generator for bash sandbox"
```

---

### Task 2: confine() 分发 + bash 接线 + 集成测试

**Files:**
- Modify: `gasket/gasket-core/src/tools/sandbox.rs`(加 `confine` + `sandbox_enabled`)
- Modify: `gasket/gasket-core/src/tools/bash.rs`(execute() 内接线,line 47 `};` 之后、line 48 `cmd.current_dir(...)` 之前;tests 末尾加集成测试)

**Interfaces:**
- Produces:
  - `pub(crate) fn confine(cmd: &mut tokio::process::Command, cwd: &std::path::Path) -> Result<(), String>` — 唯一入口;`Err` = 拒绝执行。
  - `pub(crate) fn sandbox_enabled(env: &std::collections::HashMap<String, String>) -> bool` — 从 `ToolContext.env` 读开关。
  - 调用契约:`confine` 必须在设置 `current_dir`/`env` **之前**调用(macOS 分支整体重写 Command)。

- [ ] **Step 1: Write the failing tests**

在 `bash.rs` tests 模块(现 line 96 起)末尾追加;同时在 `sandbox.rs` 加 `sandbox_enabled` 测试:

```rust
// sandbox.rs
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_enabled_only_on_exact_flag() {
        let mut env = std::collections::HashMap::new();
        assert!(!sandbox_enabled(&env));
        env.insert("GASKET_SANDBOX".to_string(), "0".to_string());
        assert!(!sandbox_enabled(&env));
        env.insert("GASKET_SANDBOX".to_string(), "1".to_string());
        assert!(sandbox_enabled(&env));
    }
}
```

```rust
// bash.rs tests(复用现有 run() 辅助不合适——它用进程 env;新写一个带 env 的)
async fn run_with_env(args: serde_json::Value, cwd: &std::path::Path, env: std::collections::HashMap<String, String>) -> ToolResult {
    let t = tool();
    (t.execute)(ToolCallCtx {
        tool_call_id: "x".into(),
        args,
        signal: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        ctx: ToolContext { cwd: cwd.to_path_buf(), env, session_id: "s".into(), state_dir: cwd.to_path_buf(), spawner: None },
    }).await.unwrap()
}

/// Sandbox lets us write inside cwd but not outside it. Only meaningful where
/// confinement is real (macOS seatbelt); Linux lands in Task 3, Windows refuses.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn sandbox_blocks_writes_outside_cwd() {
    let cwd = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let mut env: std::collections::HashMap<_, _> = std::env::vars().collect();
    env.insert("GASKET_SANDBOX".to_string(), "1".to_string());

    // write inside cwd -> allowed
    let r = run_with_env(serde_json::json!({"command": "echo x > inside.txt"}), cwd.path(), env.clone()).await;
    assert!(!r.is_error, "write in cwd must pass: {:?}", r.details);
    assert!(cwd.path().join("inside.txt").exists());

    // write outside cwd -> refused by the seatbelt profile
    let target = outside.path().join("f.txt");
    let r = run_with_env(
        serde_json::json!({"command": format!("echo x > {}", target.display())}),
        cwd.path(),
        env,
    ).await;
    assert!(r.is_error, "write outside cwd must fail");
    assert!(!target.exists(), "sandbox did not contain the write");
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn no_sandbox_flag_no_behavior_change() {
    let cwd = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("f.txt");
    let env: std::collections::HashMap<_, _> = std::env::vars().collect(); // no GASKET_SANDBOX
    let r = run_with_env(
        serde_json::json!({"command": format!("echo x > {}", target.display())}),
        cwd.path(),
        env,
    ).await;
    assert!(!r.is_error, "sandbox off -> old behavior");
    assert!(target.exists());
}
```

- [ ] **Step 2: Run to verify it fails**

```bash
cd /Users/yeheng/workspaces/Github/gasket/gasket && cargo test -p gasket-core sandbox
```

预期:编译错(`confine`/`sandbox_enabled` 未定义)。`sandbox_blocks_writes_outside_cwd` 在实现后、接线前仍失败(沙箱未生效)。

- [ ] **Step 3: Minimal implementation**

`sandbox.rs`:

```rust
/// Read the sandbox flag from the ToolContext env map (host-populated from
/// the process env). Exact "1" only — no truthy-string guessing.
pub(crate) fn sandbox_enabled(env: &std::collections::HashMap<String, String>) -> bool {
    env.get("GASKET_SANDBOX").map(String::as_str) == Some("1")
}

/// Apply filesystem confinement to `cmd`. MUST be called before cwd/env are
/// set on `cmd` (the macOS branch rewrites program+args wholesale).
/// Err = fail-closed: the caller must refuse to run the command.
pub(crate) fn confine(
    cmd: &mut tokio::process::Command,
    cwd: &std::path::Path,
) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let cwd_c = cwd
            .canonicalize()
            .map_err(|e| format!("sandbox: cwd not accessible: {e}"))?;
        let tmp = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
        let profile = seatbelt_profile(&cwd_c.display().to_string(), &tmp);
        let std_cmd = cmd.as_std_mut();
        let program = std_cmd.get_program().to_os_string();
        let args: Vec<_> = std_cmd.get_args().map(std::ffi::OsString::from).collect();
        *cmd = tokio::process::Command::new("sandbox-exec");
        cmd.arg("-p").arg(&profile).arg(program).args(args);
        Ok(())
    }
    #[cfg(all(target_os = "linux", feature = "sandbox-landlock"))]
    {
        confine_landlock(cmd, cwd) // added in Task 3; for now: Ok(()) placeholder is NOT allowed —
        // in this task, the linux branch returns Err until Task 3:
        // Err("sandbox: landlock support not yet built".into())
    }
    #[cfg(all(target_os = "linux", not(feature = "sandbox-landlock")))]
    {
        let _ = (cmd, cwd);
        Err("GASKET_SANDBOX=1 but this build lacks the landlock backend; rebuild gasket-core with --features sandbox-landlock".to_string())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (cmd, cwd);
        Err("sandbox unsupported on this platform".into())
    }
}
```

> 注意:本 task 里带 feature 的 linux 分支写 `Err("sandbox: landlock support not yet built".into())`;`not(feature)` 分支永远返回 rebuild-hint 错误(fail-closed)。Task 3 将带 feature 的分支替换为 `return confine_landlock(cmd, cwd);` 真实实现。`as_std_mut()`/`get_program()`/`get_args()` 为 tokio 1.x / std 1.57+ 已有 API。若 `sandbox-exec` 不存在或 profile 非法,spawn 会失败并落进 bash.rs 现有 `failed to spawn` 错误分支 —— 即 fail-closed。

`bash.rs` execute() 内,line 47 `};`(if/else 构建 program/args 结束)与 line 48 `cmd.current_dir(&ctx.ctx.cwd);` 之间插入:

```rust
    if sandbox_enabled(&ctx.ctx.env) {
        if let Err(e) = super::sandbox::confine(&mut cmd, &ctx.ctx.cwd) {
            return Ok(ToolResult::error(e));
        }
    }
```

- [ ] **Step 4: Run to verify it passes**

```bash
cd /Users/yeheng/workspaces/Github/gasket/gasket && cargo test -p gasket-core
cargo clippy --all-features --all-targets -D warnings
cargo fmt --check
```

预期:全部通过,含新增 `sandbox_blocks_writes_outside_cwd`、`no_sandbox_flag_no_behavior_change`、既有 bash 测试(`runs_echo` 等不受影响,证明默认关闭零变化)。

- [ ] **Step 5: Commit**

```bash
cd /Users/yeheng/workspaces/Github/gasket
git add gasket/gasket-core/src/tools/sandbox.rs gasket/gasket-core/src/tools/bash.rs
git commit -m "feat(core): GASKET_SANDBOX opt-in confinement for bash (macOS, fail-closed)"
```

---

### Task 3: Linux Landlock 路径 + 文档

**Files:**
- Modify: `gasket/gasket-core/Cargo.toml`(line 52 `dom_query` 行后、`[dev-dependencies]` 之前,加 target-scoped 可选依赖 + `[features]`)
- Modify: `gasket/gasket-core/src/tools/sandbox.rs`(替换 linux 分支为真实实现 + 测试)
- Modify: `docs/usage.md`(§9 新增小节;§10 "工具 / 搜索 / MCP" 表加一行)
- Modify: `gasket/.env.example`(line 55 `GASKET_FETCH_ALLOW_PRIVATE_NET` 注释附近加注释行)

**Interfaces:**
- Consumes:Task 2 的 `confine()` 分发骨架。
- Produces:无新接口;`confine` 在 Linux 上生效。

- [ ] **Step 1: Write the failing test / dep**

`gasket/gasket-core/Cargo.toml`(`[dependencies]` 之后、`[dev-dependencies]` 之前,即 line 52 `dom_query = "0.28"` 行后插入两个新 section):

```toml
[target.'cfg(target_os = "linux")'.dependencies]
# Opt-in Landlock backend for the bash tool (GASKET_SANDBOX=1).
landlock = { version = "0.4", optional = true }

[features]
# Opt-in Linux sandbox backend; macOS (sandbox-exec) needs no dependency.
sandbox-landlock = ["dep:landlock"]
```

`sandbox.rs` 加测试(仅 Linux 编译;本机为 macOS,用 `cargo check --target` 交叉验证编译,运行时验证留给 CI/Linux):

```rust
#[cfg(all(test, target_os = "linux", feature = "sandbox-landlock"))]
mod landlock_tests {
    use super::*;

    #[test]
    fn landlock_ruleset_builds_for_existing_paths() {
        let cwd = tempfile::tempdir().unwrap();
        // Building + restricting in the *test process* would sandbox the test
        // runner itself (Landlock applies to the calling thread's children);
        // so only assert construction of the rule set here.
        assert!(landlock_ruleset(cwd.path(), std::path::Path::new("/tmp")).is_ok());
    }
}

#[cfg(all(test, target_os = "linux", not(feature = "sandbox-landlock")))]
mod no_landlock_tests {
    use super::*;

    #[test]
    fn confine_without_feature_fails_closed_with_hint() {
        let mut cmd = tokio::process::Command::new("true");
        let err = confine(&mut cmd, std::path::Path::new("/tmp")).unwrap_err();
        assert!(err.contains("--features sandbox-landlock"), "{err}");
    }
}
```

- [ ] **Step 2: Run to verify it fails**

```bash
cd /Users/yeheng/workspaces/Github/gasket/gasket && cargo test -p gasket-core sandbox
```

预期:编译错(`landlock_ruleset` 未定义);macOS 本机不编译 linux 测试,故同时用 `rustup target add x86_64-unknown-linux-gnu && cargo check -p gasket-core --features sandbox-landlock --target x86_64-unknown-linux-gnu`(无 linker 需求,check 即可)验证目标侧编译(不带 feature 时 landlock 代码根本不编译,交叉 check 必须带 feature);若无法安装 target,注明留给 CI。

- [ ] **Step 3: Minimal implementation**

`sandbox.rs`(linux cfg 段):

```rust
#[cfg(all(target_os = "linux", feature = "sandbox-landlock"))]
fn confine_landlock(
    cmd: &mut tokio::process::Command,
    cwd: &std::path::Path,
) -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    let cwd = cwd
        .canonicalize()
        .map_err(|e| format!("sandbox: cwd not accessible: {e}"))?;
    let tmp = std::path::PathBuf::from(std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into()));
    // pre_exec runs between fork and exec: Landlock is inherited by the
    // exec'd child and its whole process tree. Owned paths (no borrows) so
    // the closure is Send + 'static.
    unsafe {
        cmd.as_std_mut().pre_exec(move || {
            landlock_ruleset(&cwd, &tmp).map_err(std::io::Error::other)
        });
    }
    Ok(())
}

/// Read-only filesystem everywhere except cwd/tmp (+ /var/tmp via a third
/// rule). Errors (unsupported kernel, missing paths) reach pre_exec and fail
/// the spawn -> fail-closed.
#[cfg(all(target_os = "linux", feature = "sandbox-landlock"))]
fn landlock_ruleset(cwd: &std::path::Path, tmp: &std::path::Path) -> Result<(), String> {
    use landlock::{ABI, AccessFs, Compatible, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreated};
    let abi = ABI::V5;
    Ruleset::default()
        .handle_access(AccessFs::from_all(abi))
        .map_err(|e| e.to_string())?
        .create()
        .map_err(|e| e.to_string())?
        .add_rules(PathBeneath::new(PathFd::new("/")?, AccessFs::from_read(abi)))
        .and_then(|r| r.add_rules(PathBeneath::new(PathFd::new(cwd)?, AccessFs::from_all(abi))))
        .and_then(|r| r.add_rules(PathBeneath::new(PathFd::new(tmp)?, AccessFs::from_all(abi))))
        .and_then(|r| {
            r.add_rules(PathBeneath::new(PathFd::new("/var/tmp")?, AccessFs::from_all(abi)))
        })
        .map_err(|e| e.to_string())?
        .set_no_new_privs(true)
        .restrict_self()
        .map_err(|e| e.to_string())
}
```

> 对照 landlock 0.4 crate 文档核对 `add_rules` 的参数形状(单 rule vs iterator)与 `Compatible` trait 方法名后再定稿;`cargo check` 即为验证。

然后替换 Task 2 带 feature 的 linux 分支占位为 `return confine_landlock(cmd, cwd);`(`not(feature)` 分支保持不变,继续 fail-closed)。

文档 —— `docs/usage.md` §9.5 之后新增:

```markdown
### 9.6 bash 沙箱(GASKET_SANDBOX)

设置 `GASKET_SANDBOX=1` 后,`bash` 工具的命令在操作系统级文件系统沙箱中执行:macOS 使用 `sandbox-exec`(Seatbelt,系统自带);Linux 使用 Landlock,要求以 `--features sandbox-landlock` 构建否则 `GASKET_SANDBOX=1` fail-closed 并附带 rebuild 提示。写操作仅允许在当前工作目录、`TMPDIR` 与 `/var/tmp` 内,其余路径只读。未设置时行为完全不变。沙箱是 fail-closed 的:如果隔离无法施加(如 Windows、或不支持 Landlock 的内核),命令会被直接拒绝并返回错误,而不是降级放行。
```

§10 "工具 / 搜索 / MCP" 表(line ~391 起)加一行 `| GASKET_SANDBOX | 置 1 时 bash 工具启用文件系统沙箱(见 §9.6) |`。`gasket/.env.example` line 55 附近加:

```bash
# Set to 1 to run bash tool commands in a filesystem sandbox (writes confined
# to cwd/TMPDIR/var/tmp; macOS sandbox-exec / Linux landlock -- Linux requires
# building with --features sandbox-landlock, otherwise fail-closed).
# GASKET_SANDBOX=1
```

- [ ] **Step 4: Run to verify it passes**

```bash
cd /Users/yeheng/workspaces/Github/gasket/gasket
cargo test -p gasket-core                                        # 不带 feature 也必须全绿(feature 默认关闭)
cargo check -p gasket-core --features sandbox-landlock           # feature 侧编译通过
cargo clippy --all-features --all-targets -D warnings            # landlock 代码全部 linux-cfg'd,macOS 上开启 feature 是 inert,保持零告警
cargo fmt --check
```

预期:全绿(macOS 本机跑 macOS 路径 + no-feature fail-closed 逻辑;linux landlock 代码由 `cargo check --features sandbox-landlock --target x86_64-unknown-linux-gnu` 或 CI 验证)。

- [ ] **Step 5: Commit**

```bash
cd /Users/yeheng/workspaces/Github/gasket
git add gasket/gasket-core/Cargo.toml gasket/gasket-core/src/tools/sandbox.rs docs/usage.md gasket/.env.example
git commit -m "feat(core): landlock sandbox path on linux + GASKET_SANDBOX docs"
```
