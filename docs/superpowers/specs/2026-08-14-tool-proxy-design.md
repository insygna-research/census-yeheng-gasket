# Tool Proxy Design (fetch / web_search)

Date: 2026-08-14
Status: Approved

## Goal

Give the `fetch` and `web_search` tools proxy support (http / https / socks5 / socks5h).
In the desktop app the proxy is configured in the frontend and applied by the backend at
runtime (hot update, no restart). CLI / gateway get the same support via an environment
variable.

Scope decisions (confirmed by user):

- The desktop backend links `gasket-ext` and registers `web_search` (today only the CLI
  has it) so both tools are proxy-capable in the desktop app.
- LLM provider traffic is NOT touched — it keeps using `GASKET_LLM_PROXY` et al.
- No `no_proxy` bypass list; authenticated proxies use `user:pass@host` embedded in the URL.

## Chosen approach: runtime global override with env fallback

A new `proxy` module in `gasket-core` holds a `RwLock<Option<String>>` global override
plus an env fallback (`GASKET_TOOL_PROXY`). fetch/search clients are built per call and
read the current value → changes apply on the next tool call, and subagents inherit it
automatically.

Rejected alternatives:

- **Explicit parameter threading** (`built_in_tools(proxy)`): forces signature changes
  through every host, `HostSubagentSpawner`, and all tests for one optional knob.
- **`std::env::set_var` only**: unsafe in multithreaded processes (2024 edition), and the
  search tool's registration-time `Arc<Client>` would not see updates anyway.

Precedence: runtime override > `GASKET_TOOL_PROXY` env > none. This mirrors the existing
`GASKET_LLM_PROXY` naming convention.

## Changes

### 1. `gasket/Cargo.toml` (workspace)

reqwest features add `"socks"`. Without it reqwest fails to parse `socks5://` proxy URLs.

### 2. `gasket-core/src/proxy.rs` (new)

```rust
static OVERRIDE: RwLock<Option<String>> = ...;

pub fn set_tool_proxy(url: Option<&str>) -> Result<(), String>; // validates via reqwest::Proxy::all
pub fn tool_proxy() -> Option<String>;                          // override > env
pub fn apply_tool_proxy(b: ClientBuilder) -> ClientBuilder;     // shared assembly point
```

- Validation: `reqwest::Proxy::all(url)` must succeed and the scheme must be
  http/https/socks5/socks5h (case-insensitive). Bad runtime value → `Err`; bad env
  value → `warn!` and ignored (fail-open to direct connection).
- Export from `lib.rs`.

### 3. `gasket-core/src/tools/fetch.rs`

Build the client through `apply_tool_proxy`. Already builds per call → hot update free.

### 4. `gasket-ext/src/search.rs`

Drop the registration-time `Arc<Client>`; build a proxy-aware client per execution.
Search is low-frequency; client-build cost is negligible.

### 5. Desktop backend (`web/src-tauri`)

- `Cargo.toml`: add `gasket-ext` dependency.
- `chat.rs` `build_session`: after assembling built-in tools, register `web_search`
  from `gasket_ext::search` (only web_search — not hello/todo demos). Subagent tool set
  unchanged (matches CLI behavior).
- `lib.rs`:
  - `set_app_config`: after persisting, read `gasket_proxy` from the config map and call
    `set_tool_proxy`; a value that fails validation returns `Err` to the frontend.
  - `run()` setup: initialize the override once from the existing `app_config.json`
    (storage syncs only on writes).

### 6. Frontend

- `src/lib/storage.ts`: `storageKeys.proxy = 'gasket_proxy'`.
- `src/components/NetworkProxyDialog.vue` (new): Teleport + overlay pattern copied from
  `ApprovalDialog.vue`. URL input with scheme validation
  (`^(https?|socks5h?)://`), Save / Disable buttons; empty string disables.
  Browser-mode note: "only takes effect in the desktop app".
- `src/components/ChatHeader.vue`: Globe icon button opens the dialog. Self-contained:
  the dialog only needs `storage.ts`, so it lives inside ChatHeader — no parent event
  wiring through ChatArea.

Data flow: `writeString('gasket_proxy', url)` → 500 ms debounce → `set_app_config` →
backend `set_tool_proxy` → next fetch/search call uses the proxy.

### 7. Docs

- `gasket/.env.example`: document `GASKET_TOOL_PROXY`.
- `docs/usage.md` §3.3: tool proxy section (env var for CLI/gateway, UI for desktop).

## Verification

1. `proxy.rs` unit tests: scheme validation (accepts http/https/socks5/socks5h, rejects
   ftp/empty/garbage), override > env precedence, socks5 URL constructs.
2. `cargo test -p gasket-core -p gasket-ext` in the workspace.
3. `cargo check` in `web/src-tauri` (links gasket-ext).
4. `npm run build` in `web/` (vue-tsc + vite).
5. Manual (listed at delivery): local http/socks5 proxy + `tauri dev`, confirm fetch and
   web_search egress through it and that toggling the UI applies without restart.
