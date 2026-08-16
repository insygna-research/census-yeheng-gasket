# Tool Proxy (fetch / web_search) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Route `fetch` and `web_search` tool traffic through a configurable proxy (http/https/socks5/socks5h), settable from the desktop app UI (hot-applied by the backend) or via `GASKET_TOOL_PROXY` env; also register `web_search` into the desktop backend.

**Architecture:** A `proxy` module in gasket-core holds a process-global `RwLock<Option<String>>` override with env fallback. `fetch` and `web_search` build their reqwest clients per call through a shared builder hook, so proxy changes apply on the next tool call. The desktop backend installs the override from its `app_config.json` (key `gasket_proxy`, written by the frontend's existing debounced config sync).

**Tech Stack:** Rust (workspace `gasket/` crates + `web/src-tauri`), reqwest with `socks` feature, Vue 3 + TS frontend.

**Spec:** `docs/superpowers/specs/2026-08-14-tool-proxy-design.md`

## Global Constraints

- Workspace root for Rust crates: `/Users/yeheng/workspaces/Github/gasket/gasket` (has workspace `Cargo.toml`). Desktop backend: `/Users/yeheng/workspaces/Github/gasket/web/src-tauri`. Frontend: `/Users/yeheng/workspaces/Github/gasket/web`.
- Proxy schemes allowed: `http`, `https`, `socks5`, `socks5h` (case-insensitive). Anything else is an error.
- Precedence: runtime override > `GASKET_TOOL_PROXY` env > none.
- Env var name: `GASKET_TOOL_PROXY`. Frontend storage key: `gasket_proxy`. Tauri state/commands stay untouched (the override is plain process state in gasket-core).
- LLM provider traffic is NOT modified (keeps `GASKET_LLM_PROXY` et al).
- Rust code style in `gasket/` workspace: 2-space indent; in `web/src-tauri`: 2-space indent. Match existing file conventions.
- Commit style: conventional commits (`feat:`, `test:`, `docs:`, `chore:`), one commit per task, run from repo root `/Users/yeheng/workspaces/Github/gasket`.

---

### Task 1: gasket-core `proxy` module + reqwest socks feature

**Files:**
- Modify: `gasket/Cargo.toml` (workspace dependencies, reqwest line)
- Create: `gasket/gasket-core/src/proxy.rs`
- Modify: `gasket/gasket-core/src/lib.rs` (module decl + re-exports)

**Interfaces:**
- Produces (later tasks rely on these exact signatures):
  - `gasket_core::set_tool_proxy(url: Option<&str>) -> Result<(), String>` — set/clear runtime override; validates eagerly.
  - `gasket_core::tool_proxy() -> Option<String>` — active proxy URL (override > env).
  - `gasket_core::apply_tool_proxy(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder` — attach active proxy to a client builder.
  - `gasket_core::proxy::test_util::LOCK` (`std::sync::Mutex<()>`, `#[cfg(test)]`, `pub(crate)`) — serializes tests that touch the global override across the crate.

- [ ] **Step 1: Enable reqwest socks in the workspace manifest**

In `gasket/Cargo.toml`, change the workspace reqwest entry to add `"socks"`:

```toml
reqwest = { version = "0.13", default-features = false, features = ["json", "rustls", "stream", "socks"] }
```

Without the `socks` feature, `reqwest::Proxy::all("socks5://…")` fails to construct.

- [ ] **Step 2: Write the failing tests (create `gasket/gasket-core/src/proxy.rs` with tests only, module not yet exported)**

Create `gasket/gasket-core/src/proxy.rs` containing the test module and a `test_util` module; implementation fns come in Step 4 (file must compile then, so add them together — Step 2/3 verify via the full test run in Step 5; the "failing first" check here is: leave `set_tool_proxy` etc. unimplemented and `cargo test -p gasket-core proxy` must fail to compile, which is the failing state):

```rust
//! Runtime-configurable proxy for outbound tool HTTP traffic (`fetch`,
//! `web_search`). Precedence: in-process override (desktop app UI) >
//! `GASKET_TOOL_PROXY` env > none. Supported schemes: http, https, socks5,
//! socks5h (with optional `user:pass@` userinfo embedded in the URL).

use std::sync::RwLock;

/// In-process override, set by hosts with a UI (the desktop app). `None`
/// means "fall back to the env var".
static OVERRIDE: RwLock<Option<String>> = RwLock::new(None);

const ENV_VAR: &str = "GASKET_TOOL_PROXY";
const ALLOWED_SCHEMES: &[&str] = &["http", "https", "socks5", "socks5h"];

/// Set (or clear) the runtime override. The URL is validated eagerly so a
/// typo from the UI fails at save time, not at the next tool call. `None`,
/// empty, and blank strings all clear the override.
pub fn set_tool_proxy(url: Option<&str>) -> Result<(), String> {
  let normalized = url.map(str::trim).filter(|s| !s.is_empty());
  match normalized {
    None => {
      *OVERRIDE.write().unwrap() = None;
      Ok(())
    }
    Some(url) => {
      validate(url)?;
      *OVERRIDE.write().unwrap() = Some(url.to_string());
      Ok(())
    }
  }
}

/// The currently active proxy URL (override > env), if any.
pub fn tool_proxy() -> Option<String> {
  resolve_with(&|k| std::env::var(k))
}

/// Same as [`tool_proxy`] with an injectable env lookup — used by tests to
/// avoid mutating process env (mirrors `ProviderConfig::from_env_with`).
fn resolve_with(lookup: &dyn Fn(&str) -> Result<String, std::env::VarError>) -> Option<String> {
  if let Some(o) = OVERRIDE.read().unwrap().clone() {
    return Some(o);
  }
  lookup(ENV_VAR)
    .ok()
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty())
}

fn validate(url: &str) -> Result<(), String> {
  let scheme = url.split("://").next().unwrap_or("").to_ascii_lowercase();
  if !ALLOWED_SCHEMES.contains(&scheme.as_str()) {
    return Err(format!(
      "unsupported proxy scheme '{scheme}' in '{url}' (allowed: http, https, socks5, socks5h)"
    ));
  }
  reqwest::Proxy::all(url)
    .map(|_| ())
    .map_err(|e| format!("invalid proxy url '{url}': {e}"))
}

/// Attach the active proxy (if any) to a client builder. An invalid URL can
/// only come from the env var here (the override is validated at set time);
/// it is warned and skipped — fail-open to a direct connection.
pub fn apply_tool_proxy(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
  apply_proxy_url(builder, &tool_proxy())
}

fn apply_proxy_url(
  mut builder: reqwest::ClientBuilder,
  url: &Option<String>,
) -> reqwest::ClientBuilder {
  if let Some(url) = url {
    match reqwest::Proxy::all(url) {
      Ok(proxy) => builder = builder.proxy(proxy),
      Err(e) => tracing::warn!("ignoring invalid tool proxy '{url}': {e}"),
    }
  }
  builder
}

#[cfg(test)]
pub(crate) mod test_util {
  /// Serializes tests that touch the global override. Shared across this
  /// crate's test modules (proxy.rs, tools/fetch.rs) so parallel #[test]
  /// threads cannot observe each other's override.
  pub static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
}

#[cfg(test)]
mod tests {
  use super::*;

  fn fake_env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Result<String, std::env::VarError> {
    let map: std::collections::HashMap<String, String> = pairs
      .iter()
      .map(|(k, v)| (k.to_string(), v.to_string()))
      .collect();
    move |k: &str| map.get(k).cloned().ok_or(std::env::VarError::NotPresent)
  }

  #[test]
  fn validation_accepts_supported_schemes() {
    for url in [
      "http://127.0.0.1:8080",
      "https://proxy.corp:8443",
      "socks5://127.0.0.1:1080",
      "socks5h://proxy.internal:1080",
      "SOCKS5://127.0.0.1:1080",
      "http://user:pass@proxy:8080",
    ] {
      assert!(validate(url).is_ok(), "should accept {url}");
    }
  }

  #[test]
  fn validation_rejects_bad_input() {
    for url in ["", "  ", "ftp://proxy:21", "127.0.0.1:8080", "http://"] {
      assert!(validate(url).is_err(), "should reject '{url}'");
    }
  }

  #[test]
  fn override_beats_env_and_blank_env_is_none() {
    let _g = test_util::LOCK.lock().unwrap();
    set_tool_proxy(Some("socks5://override:1080")).unwrap();
    assert_eq!(
      resolve_with(&fake_env(&[(ENV_VAR, "http://env:8080")])),
      Some("socks5://override:1080".to_string())
    );

    set_tool_proxy(None).unwrap();
    assert_eq!(
      resolve_with(&fake_env(&[(ENV_VAR, "http://env:8080")])),
      Some("http://env:8080".to_string())
    );
    // env unset and no override -> none
    assert_eq!(resolve_with(&fake_env(&[])), None);
    // blank env value treated as unset
    assert_eq!(resolve_with(&fake_env(&[(ENV_VAR, "  ")])), None);
  }

  #[test]
  fn set_rejects_invalid_and_keeps_previous() {
    let _g = test_util::LOCK.lock().unwrap();
    set_tool_proxy(Some("http://good:8080")).unwrap();
    assert!(set_tool_proxy(Some("garbage")).is_err());
    assert_eq!(resolve_with(&fake_env(&[])), Some("http://good:8080".to_string()));
    // blank input clears
    set_tool_proxy(Some("   ")).unwrap();
    assert_eq!(resolve_with(&fake_env(&[])), None);
  }

  #[test]
  fn apply_builds_client_with_socks5_and_fails_open_on_invalid() {
    let _g = test_util::LOCK.lock().unwrap();
    set_tool_proxy(Some("socks5://127.0.0.1:1080")).unwrap();
    apply_tool_proxy(reqwest::Client::builder()).build().unwrap();
    set_tool_proxy(None).unwrap();
    // invalid URL (as if from a bad env value) must not break client construction
    apply_proxy_url(reqwest::Client::builder(), &Some("ftp://bad".to_string()))
      .build()
      .unwrap();
  }
}
```

- [ ] **Step 3: Export the module**

In `gasket/gasket-core/src/lib.rs`: add `pub mod proxy;` to the module list (alphabetical, between `providers` and `storage`) and a re-export next to the other `pub use` lines:

```rust
pub use proxy::{apply_tool_proxy, set_tool_proxy, tool_proxy};
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p gasket-core proxy` (cwd `gasket/`)
Expected: 5 tests PASS (`validation_accepts_supported_schemes`, `validation_rejects_bad_input`, `override_beats_env_and_blank_env_is_none`, `set_rejects_invalid_and_keeps_previous`, `apply_builds_client_with_socks5_and_fails_open_on_invalid`).

- [ ] **Step 5: Full crate check**

Run: `cargo test -p gasket-core` (cwd `gasket/`)
Expected: all existing tests still PASS.

- [ ] **Step 6: Commit**

```bash
git add gasket/Cargo.toml gasket/Cargo.lock gasket/gasket-core/src/proxy.rs gasket/gasket-core/src/lib.rs
git commit -m "feat(core): tool proxy override module (http/https/socks5/socks5h)"
```

---

### Task 2: `fetch` tool uses the proxy

**Files:**
- Modify: `gasket/gasket-core/src/tools/fetch.rs` (client construction, ~line 45; test module)

**Interfaces:**
- Consumes: `crate::proxy::apply_tool_proxy`, `crate::proxy::test_util::LOCK`, `crate::proxy::set_tool_proxy` (from Task 1).

- [ ] **Step 1: Write the failing test**

Append to the existing `mod tests` in `fetch.rs` (it already imports `super::*`, `ContentBlock`, `Arc`, and has a `ToolCallCtx` construction pattern to copy):

    /// End-to-end wiring proof: with the override set, fetch's request must
    /// hit the proxy, not the target host. A real HTTP proxy in ~25 lines:
    /// read the request head, reply with a canned page.
    #[tokio::test]
    async fn fetch_goes_through_tool_proxy() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let _g = crate::proxy::test_util::LOCK.lock().unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut head = String::new();
            let mut buf = [0u8; 8192];
            loop {
                let n = sock.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                head.push_str(&String::from_utf8_lossy(&buf[..n]));
                if head.contains("\r\n\r\n") {
                    break;
                }
            }
            let body = "<html><body><article>via proxy</article></body></html>";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            head
        });

        crate::proxy::set_tool_proxy(Some(&format!("http://{proxy_addr}"))).unwrap();
        let ctx = ToolCallCtx {
            tool_call_id: "t2".into(),
            args: serde_json::json!({"url": "http://example.test/"}),
            signal: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            ctx: crate::ToolContext {
                cwd: ".".into(),
                env: std::collections::HashMap::new(),
                session_id: "t".into(),
                state_dir: ".".into(),
                spawner: None,
            },
        };
        let result = execute(ctx).await.unwrap();
        crate::proxy::set_tool_proxy(None).unwrap();

        assert!(!result.is_error);
        match &result.content[0] {
            ContentBlock::Text { text } => assert!(text.contains("via proxy")),
            _ => panic!("expected text content"),
        }
        // A proxied http request carries the absolute target URI on the
        // request line — proof the connection went through the proxy.
        let head = server.await.unwrap();
        assert!(head.starts_with("GET http://example.test/"), "proxy saw: {head}");
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p gasket-core fetch_goes_through_tool_proxy` (cwd `gasket/`)
Expected: FAIL — the request goes direct to `example.test`, times out after 30s, and the result is an error (`assert!(!result.is_error)` fails) or the spawned server panics on `accept` (task join error). Either failure mode proves the proxy is not yet used.

- [ ] **Step 3: Wire the proxy into the client build**

In `fetch.rs` `execute()`, change the client construction to route the builder through the proxy hook (keep timeout/UA exactly as-is):

```rust
    let client = crate::proxy::apply_tool_proxy(reqwest::Client::builder())
        .timeout(Duration::from_secs(TIMEOUT_SECS))
        .user_agent("gasket-fetch/1.0")
        .build()
        .map_err(|e| crate::error::ToolError::Message(format!("client build failed: {e}")))?;
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p gasket-core` (cwd `gasket/`)
Expected: ALL PASS including `fetch_goes_through_tool_proxy`.

- [ ] **Step 5: Commit**

```bash
git add gasket/gasket-core/src/tools/fetch.rs
git commit -m "feat(core): fetch tool routes through the tool proxy"
```

---

### Task 3: `web_search` builds its client per call with the proxy

**Files:**
- Modify: `gasket/gasket-ext/src/search.rs` (registration, ~line 514; test module)
- Modify: `gasket/gasket-ext/Cargo.toml` (dev-dependencies)

**Interfaces:**
- Consumes: `gasket_core::apply_tool_proxy`, `gasket_core::set_tool_proxy` (from Task 1).
- Produces: unchanged public surface — `pub fn register(api: &mut dyn ExtensionApi)` registering exactly one tool `web_search` (name `"web_search"`, `ToolDefinition`).

- [ ] **Step 1: Add the tokio dev-dependency**

In `gasket/gasket-ext/Cargo.toml` append:

```toml

[dev-dependencies]
tokio = { workspace = true }
```

(The workspace tokio already enables `rt-multi-thread`, `macros`, `net`, `io-util` — enough for `#[tokio::test]` + `TcpListener`.)

- [ ] **Step 2: Write the failing test**

Append to the existing `mod tests` in `search.rs`:

```rust
    /// Wiring proof: the tool must build its HTTP client at execute time so
    /// the runtime tool proxy applies. A minimal HTTP proxy replies with a
    /// canned DuckDuckGo-shaped results page.
    #[tokio::test]
    async fn web_search_goes_through_tool_proxy() {
        use gasket_core::{ExtensionApiImpl, ToolCallCtx, ToolContext};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut head = String::new();
            let mut buf = [0u8; 8192];
            loop {
                let n = sock.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                head.push_str(&String::from_utf8_lossy(&buf[..n]));
                if head.contains("\r\n\r\n") {
                    break;
                }
            }
            let body = concat!(
                "<html><body>",
                "<div class=\"result\">",
                "<a class=\"result__a\" href=\"//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Frust\">Rust Language</a>",
                "<a class=\"result__snippet\" href=\"#\">A systems programming language</a>",
                "</div>",
                "</body></html>"
            );
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            head
        });

        // Mutates process env deliberately: the provider is selected at
        // execute time. No other test in this crate reads env vars.
        std::env::set_var("GASKET_SEARCH_PROVIDER", "duckduckgo");
        gasket_core::set_tool_proxy(Some(&format!("http://{proxy_addr}"))).unwrap();

        let mut api = ExtensionApiImpl::new();
        super::register(&mut api);
        assert_eq!(api.tools.len(), 1);
        assert_eq!(api.tools[0].name, "web_search");
        let tool = api.tools.remove(0);
        let ctx = ToolCallCtx {
            tool_call_id: "t1".into(),
            args: serde_json::json!({"query": "rust language", "count": 2}),
            signal: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            ctx: ToolContext {
                cwd: ".".into(),
                env: std::collections::HashMap::new(),
                session_id: "t".into(),
                state_dir: ".".into(),
                spawner: None,
            },
        };
        let result = (tool.execute)(ctx).await.unwrap();

        // Cleanup before assertions so a failed assert can't leak state.
        gasket_core::set_tool_proxy(None).unwrap();
        std::env::remove_var("GASKET_SEARCH_PROVIDER");

        assert!(!result.is_error);
        match &result.content[0] {
            gasket_core::ContentBlock::Text { text } => {
                assert!(text.contains("Rust Language"), "got: {text}");
                assert!(text.contains("https://example.com/rust"), "got: {text}");
            }
            _ => panic!("expected text content"),
        }
        let head = server.await.unwrap();
        assert!(
            head.starts_with("GET https://html.duckduckgo.com/")
                || head.starts_with("GET http://html.duckduckgo.com/"),
            "proxy saw: {head}"
        );
    }
```

- [ ] **Step 3: Run it to verify it fails**

Run: `cargo test -p gasket-ext web_search_goes_through_tool_proxy` (cwd `gasket/`)
Expected: FAIL — with the registration-time `Arc<Client>` (no proxy), the request goes direct: DNS for `html.duckduckgo.com` either resolves and returns a real/blocked page (parse yields no hits → `is_error` false but content lacks "Rust Language") or the network is unavailable → `is_error` true. Either way the assertions fail.

- [ ] **Step 4: Build the client per execution**

In `search.rs` `register()`: delete `let client = Arc::new(Client::new());`, delete `let client = client.clone();` inside the closure, and build the client at the top of the async block:

```rust
pub fn register(api: &mut dyn ExtensionApi) {
    api.register_tool(ToolDefinition {
        name: "web_search".into(),
        label: "Web Search".into(),
        description: "Search the web for current information. Supported providers: serper (default), serpapi, duckduckgo, brave, tavily, exa, firecrawl. Configure via GASKET_SEARCH_PROVIDER and the corresponding GASKET_<PROVIDER>_API_KEY environment variable.".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search query" },
                "count": { "type": "number", "description": "Number of results to return (default 5)", "default": 5 }
            },
            "required": ["query"]
        }),
        risk: RiskLevel::High,
        execute: Arc::new(move |ctx| {
            Box::pin(async move {
                // Built per call so the runtime tool proxy (desktop UI /
                // GASKET_TOOL_PROXY) applies without re-registering.
                let client = gasket_core::apply_tool_proxy(Client::builder())
                    .build()
                    .map_err(|e| ToolError::Message(format!("client build failed: {e}")))?;

                let query = ctx.args["query"].as_str().unwrap_or_default();
                // … rest of the existing execute body unchanged …
            })
        }),
    });
}
```

Also update the doc comment at the top of the file's env-var list: add `- GASKET_TOOL_PROXY: optional proxy (http/https/socks5/socks5h) for search traffic`.

- [ ] **Step 5: Run the tests**

Run: `cargo test -p gasket-ext` (cwd `gasket/`)
Expected: ALL PASS (existing serde-shape tests + the new wiring test).

- [ ] **Step 6: Commit**

```bash
git add gasket/gasket-ext/src/search.rs gasket/gasket-ext/Cargo.toml gasket/Cargo.lock
git commit -m "feat(ext): web_search honors the tool proxy via per-call client"
```

---

### Task 4: Desktop backend — link gasket-ext, register `web_search`, install proxy from app config

**Files:**
- Modify: `web/src-tauri/Cargo.toml` (dependencies)
- Modify: `web/src-tauri/src/chat.rs` (`build_session`, after the `mcp_tools` load, ~line 320)
- Modify: `web/src-tauri/src/lib.rs` (`set_app_config`, `run()` setup)

**Interfaces:**
- Consumes: `gasket_ext::search::register` (Task 3 surface), `gasket_core::{ExtensionApiImpl, set_tool_proxy}` (Tasks 1/3), existing `get_app_config` / `app_config_path`.
- Produces: Tauri command `set_app_config(config)` now ALSO validates/installs `gasket_proxy` (rejects the whole call on an invalid proxy URL with `Err(String)`); app startup installs the stored proxy once. Tools available to desktop sessions now include `web_search`.

- [ ] **Step 1: Add the dependency**

In `web/src-tauri/Cargo.toml` `[dependencies]`, after the `gasket-host` line:

```toml
gasket-ext = { path = "../../gasket/gasket-ext" }
```

- [ ] **Step 2: Register `web_search` in `build_session`**

In `web/src-tauri/src/chat.rs`, inside `build_session`, after `let mcp_tools = load_all_mcp().await;` and before `let built_in = built_in_tools();`, add:

```rust
  // web_search from gasket-ext — same registration the CLI performs via
  // register_all, scoped to the search tool only (hello/todo/
  // permission_gate are CLI demos). Its HTTP client honors the runtime
  // tool proxy (gasket_core::set_tool_proxy).
  let search_tools = {
    let mut api = gasket_core::ExtensionApiImpl::new();
    gasket_ext::search::register(&mut api);
    api.tools
  };
```

Then extend the parent's tool set in the existing `let tools = { … }` block:

```rust
  let tools = {
    let mut t = built_in;
    t.extend(extra_tools.iter().cloned());
    t.extend(mcp_tools.iter().cloned());
    t.extend(search_tools);
    t
  };
```

(The sub-agent tool set stays filtered from `built_in` only — unchanged, matching CLI behavior.)

- [ ] **Step 3: Install the proxy from `app_config.json`**

In `web/src-tauri/src/lib.rs`:

Add a helper next to `app_config_path`:

```rust
/// Extract `gasket_proxy` from the app config and install it as the
/// fetch/web_search proxy override. Missing or empty clears the override
/// (direct connection). Values may be raw strings (writeString path — not
/// JSON) or JSON strings; `as_str` covers both.
fn apply_proxy_from_config(config: &serde_json::Value) -> Result<(), String> {
  let url = config
    .get("gasket_proxy")
    .and_then(|v| v.as_str())
    .map(str::trim)
    .filter(|s| !s.is_empty());
  gasket_core::set_tool_proxy(url).map_err(|e| format!("gasket_proxy invalid: {e}"))
}
```

Change `set_app_config` to validate before persisting (fail-loud: a bad value from a stale/hand-edited config must not poison the runtime override, and must not be silently stored):

```rust
#[tauri::command]
fn set_app_config(config: serde_json::Value) -> Result<(), String> {
  apply_proxy_from_config(&config)?;
  let path = app_config_path();
  if let Some(parent) = path.parent() {
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
  }
  let bytes = serde_json::to_vec_pretty(&config).map_err(|e| e.to_string())?;
  let tmp = path.with_extension("json.tmp");
  std::fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
  std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
}
```

In `run()`'s `.setup(|app| { … })`, before the log-plugin block, initialize once from the existing on-disk config (warn-and-skip on invalid — the frontend surfaces config corruption separately via `get_app_config`):

```rust
    .setup(|app| {
      if let Ok(Some(config)) = get_app_config() {
        if let Err(e) = apply_proxy_from_config(&config) {
          log::warn!("skipping invalid stored proxy: {e}");
        }
      }
      if cfg!(debug_assertions) {
        // … existing log plugin block unchanged …
```

- [ ] **Step 4: Compile-check**

Run: `cargo check` (cwd `web/src-tauri`)
Expected: succeeds with no errors. (First build pulls gasket-ext + socks feature; warnings acceptable, errors not.)

- [ ] **Step 5: Commit**

```bash
git add web/src-tauri/Cargo.toml web/src-tauri/Cargo.lock web/src-tauri/src/chat.rs web/src-tauri/src/lib.rs
git commit -m "feat(desktop): register web_search and drive tool proxy from app config"
```

---

### Task 5: Frontend — proxy dialog + header button + storage key

**Files:**
- Modify: `web/src/lib/storage.ts` (`storageKeys`)
- Create: `web/src/components/NetworkProxyDialog.vue`
- Modify: `web/src/components/ChatHeader.vue` (button + dialog state)

**Interfaces:**
- Consumes: `readString` / `writeString` / `storageKeys` from `@/lib/storage`; `isTauri` from `@/lib/platform`; `Input` from `@/components/ui/input`; lucide icons.
- Produces: persisted storage key `gasket_proxy` (raw string, empty string = disabled), synced to the backend by the existing 500 ms debounced `set_app_config` path. No new props/events on ChatHeader's public surface (dialog is internal state).

- [ ] **Step 1: Add the storage key**

In `web/src/lib/storage.ts`:

```ts
export const storageKeys = {
  theme: 'gasket_theme_v2',
  sidebarWidth: 'gasket_sidebar_width',
  sidebarCollapsed: 'gasket_sidebar_collapsed',
  proxy: 'gasket_proxy',
} as const;
```

- [ ] **Step 2: Create `NetworkProxyDialog.vue`**

Styling copied from `ApprovalDialog.vue` (Teleport + Transition + backdrop + panel classes):

```vue
<script setup lang="ts">
import { computed, ref, watch } from 'vue';
import { Ban, Check, Globe, X } from 'lucide-vue-next';
import { Input } from '@/components/ui/input';
import { readString, storageKeys, writeString } from '@/lib/storage';
import { isTauri } from '@/lib/platform';

const props = defineProps<{ open: boolean }>();
const emit = defineEmits<{ (e: 'close'): void }>();

const url = ref('');
const error = ref('');

// Reload the stored value each time the dialog opens.
watch(
  () => props.open,
  (open) => {
    if (open) {
      url.value = readString(storageKeys.proxy, '');
      error.value = '';
    }
  }
);

const PROXY_RE = /^(https?|socks5h?):\/\/\S+$/i;
const trimmed = computed(() => url.value.trim());

const save = () => {
  const value = trimmed.value;
  if (value && !PROXY_RE.test(value)) {
    error.value = 'URL must start with http://, https://, socks5:// or socks5h://';
    return;
  }
  writeString(storageKeys.proxy, value);
  emit('close');
};

const disable = () => {
  writeString(storageKeys.proxy, '');
  emit('close');
};
</script>

<template>
  <Teleport to="body">
    <Transition
      enter-active-class="transition-all duration-200 ease-out"
      leave-active-class="transition-all duration-150 ease-in"
      enter-from-class="opacity-0"
      leave-to-class="opacity-0"
    >
      <div v-if="open" class="fixed inset-0 z-50 flex items-center justify-center p-4">
        <!-- Backdrop -->
        <div class="absolute inset-0 bg-black/50 backdrop-blur-sm" @click="emit('close')" />

        <!-- Dialog -->
        <div
          class="relative w-full max-w-md bg-popover border border-border rounded-2xl shadow-2xl p-6 space-y-4 animate-in zoom-in-95 duration-200"
        >
          <!-- Header -->
          <div class="flex items-center gap-3">
            <div class="w-10 h-10 rounded-xl bg-primary/10 flex items-center justify-center shrink-0">
              <Globe class="w-5 h-5 text-primary" />
            </div>
            <div>
              <h3 class="text-sm font-semibold text-foreground">Network Proxy</h3>
              <p class="text-xs text-muted-foreground">
                Routes fetch / web_search tool traffic
              </p>
            </div>
          </div>

          <!-- Input -->
          <div class="space-y-1.5">
            <Input
              v-model="url"
              placeholder="socks5://127.0.0.1:1080"
              class="font-mono text-xs"
              @keyup.enter="save"
            />
            <p v-if="error" class="text-[11px] text-destructive">{{ error }}</p>
            <p v-else class="text-[11px] text-muted-foreground">
              Schemes: http, https, socks5, socks5h. Credentials: user:pass@host.
            </p>
          </div>

          <p v-if="!isTauri" class="text-[11px] text-amber-500">
            Browser mode: the proxy only takes effect in the desktop app.
          </p>
          <p v-else class="text-[11px] text-muted-foreground">
            Applies to the next tool call — no restart needed.
          </p>

          <!-- Actions -->
          <div class="flex gap-2 pt-1">
            <button
              @click="emit('close')"
              class="flex-1 flex items-center justify-center gap-1.5 px-4 py-2.5 rounded-xl border border-border bg-background text-foreground text-xs font-medium hover:bg-accent transition-colors"
            >
              <X class="w-3.5 h-3.5" />
              Cancel
            </button>
            <button
              @click="disable"
              class="flex-1 flex items-center justify-center gap-1.5 px-4 py-2.5 rounded-xl border border-border bg-background text-foreground text-xs font-medium hover:bg-accent transition-colors"
            >
              <Ban class="w-3.5 h-3.5" />
              Disable
            </button>
            <button
              @click="save"
              class="flex-1 flex items-center justify-center gap-1.5 px-4 py-2.5 rounded-xl bg-primary text-primary-foreground text-xs font-medium hover:bg-primary/90 transition-colors shadow-sm"
            >
              <Check class="w-3.5 h-3.5" />
              Save
            </button>
          </div>
        </div>
      </div>
    </Transition>
  </Teleport>
</template>
```

- [ ] **Step 3: Add the header button**

In `web/src/components/ChatHeader.vue`:

Script: import the dialog, `ref`, and the `Globe` icon (merge into the existing lucide import):

```ts
import { ref } from 'vue';
import { Cpu, Globe, Loader2, Moon, MoreVertical, Palette, RotateCcw, Sun, Trash2, Check } from 'lucide-vue-next';
import NetworkProxyDialog from './NetworkProxyDialog.vue';
```

Add state:

```ts
const showProxyDialog = ref(false);
```

Template: add the trigger button between the session-actions dropdown and the Appearance dropdown (copy the Appearance trigger's classes), and mount the dialog after the Appearance dropdown's closing `</DropdownMenuRoot>`:

```html
      <!-- Network proxy -->
      <button
        class="p-2 rounded-md th-hover th-text-muted hover:th-text transition-colors"
        title="Network proxy"
        @click="showProxyDialog = true"
      >
        <Globe class="w-4 h-4" />
      </button>
```

```html
      <NetworkProxyDialog :open="showProxyDialog" @close="showProxyDialog = false" />
```

- [ ] **Step 4: Type-check and build**

Run: `pnpm build` (cwd `web/`; if pnpm is unavailable or `node_modules` is missing, `pnpm install` first — the lockfile in use is `pnpm-lock.yaml`)
Expected: `vue-tsc -b && vite build` succeeds.

- [ ] **Step 5: Commit**

```bash
git add web/src/lib/storage.ts web/src/components/NetworkProxyDialog.vue web/src/components/ChatHeader.vue
git commit -m "feat(web): network proxy dialog for fetch/web_search egress"
```

---

### Task 6: Docs + full verification

**Files:**
- Modify: `gasket/.env.example` (proxy section)
- Modify: `docs/usage.md` (§3.3, after the LLM proxy paragraph)

- [ ] **Step 1: Document the env var**

In `gasket/.env.example`, after the `GASKET_LLM_HTTPS_PROXY` line, add:

```
# Tool traffic proxy for fetch / web_search (http, https, socks5, socks5h).
# The desktop app configures this via its UI; the UI value wins over this var.
# GASKET_TOOL_PROXY=socks5://127.0.0.1:1080
```

- [ ] **Step 2: Document in usage.md**

In `docs/usage.md` §3.3, after the line "代理优先级:按 scheme 的专用代理…填补缺失的那个 scheme。" append:

```markdown

**工具代理(fetch / web_search)**:设置 `GASKET_TOOL_PROXY` 可让 `fetch` 与 `web_search` 工具的出站流量走代理,支持 `http` / `https` / `socks5` / `socks5h`(带认证的代理把 `user:pass` 写进 URL 即可):

| 变量 | 说明 | 示例 |
|---|---|---|
| `GASKET_TOOL_PROXY` | 工具出站代理 | `socks5://127.0.0.1:1080` |

桌面版在顶栏 Globe 按钮中配置代理,优先级高于该环境变量;保存后下一次工具调用即生效,无需重启。该代理不影响 LLM API 请求(那部分继续用上面的 `GASKET_LLM_PROXY` 系列)。
```

- [ ] **Step 3: Full verification sweep**

Run (cwd `gasket/`): `cargo test --workspace`
Expected: every crate green (gasket-core, gasket-host, gasket-ext, gasket-cli, gasket-gateway).

Run (cwd `web/src-tauri/`): `cargo check`
Expected: success.

Run (cwd `web/`): `pnpm build`
Expected: success.

- [ ] **Step 4: Commit**

```bash
git add gasket/.env.example docs/usage.md
git commit -m "docs: GASKET_TOOL_PROXY and desktop proxy UI usage"
```

- [ ] **Step 5: Report manual verification steps (not executed by this plan)**

Delivery notes must list: run `pnpm tauri:dev`, set a local http or socks5 proxy in the Globe dialog, ask the agent to fetch a URL / run a web search, and confirm egress via the proxy's logs; toggle Disable and confirm direct egress resumes on the next call.
