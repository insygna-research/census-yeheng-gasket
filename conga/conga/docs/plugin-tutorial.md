# Writing a conga Extension Crate

An **extension** adds tools or hooks without changing `agent_loop`. It is a
normal Rust crate (workspace / path dependency) that exports:

```rust
pub fn register(api: &mut dyn ExtensionApi) {
    // api.register_tool / register_before_tool_call / ...
}
```

The **host binary** is the composition root: it calls each linked crate's
`register` at startup, often behind Cargo features. There is **no** `.so`
loading, no ABI version, no hot-unload.

Official examples: workspace crate **`conga-ext`** (`hello`, `todo`,
`search`, `permission_gate`). CLI: `cargo run -p conga-cli --features ext`.

Built-in tools (`read` / `write` / `edit` / `bash` / `list` / `grep` /
`fetch`) live in `conga` and are not extension crates.

---

## Host wiring

```rust
let mut api = ExtensionApiImpl::new();
conga_ext::register_all(&mut api); // or hello::register / todo::register

let mut tools = conga::built_in_tools();
tools.extend(std::mem::take(&mut api.tools));

let config = AgentLoopConfig {
    hooks: Some(Arc::new(api)), // if hooks were registered
    // ...
};
```

Optional capabilities = optional **dependencies + features**, then recompile.
That is the static-world substitute for a plugin marketplace.

---

## The `ExtensionApi` surface

| Method | What it does | Example |
|---|---|---|
| `register_tool(ToolDefinition)` | add a tool the LLM may call | `hello`, `todo` |
| `register_before_tool_call(handler)` | block / modify args before run | `permission_gate` |
| `register_after_tool_call(handler)` | rewrite tool result | — |

`before_tool_call` returns `ToolCallVerdict` (`Allow` / `Block` / `Modify`).
Extensions do **not** observe events or read session state through this
trait — event observation is the host's job (`HookChain`, storage).

---

## Example 1: `hello` — minimum extension

Source: `conga-ext/src/hello.rs`.

```rust
pub fn register(api: &mut dyn ExtensionApi) {
    api.register_tool(ToolDefinition {
        name: "hello".into(),
        label: "Hello".into(),
        description: "Say hello to someone.".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": { "name": { "type": "string" } },
            "required": ["name"]
        }),
        risk: RiskLevel::High,
        execute: Arc::new(|ctx| Box::pin(async move {
            let name = ctx.args["name"].as_str().unwrap_or("world");
            Ok(ToolResult {
                content: vec![ContentBlock::text(format!("Hello, {}!", name))],
                details: serde_json::json!({ "greeted": name }),
                is_error: false,
            })
        })),
    });
}
```

- `parameters` is JSON Schema.
- `risk: RiskLevel` is **required** (no default) — set it honestly; the
  host's permission matrix keys off it.
- `details` is extension-private; the agent never reads it.

---

## Example 2: `todo` — private state files

Source: `conga-ext/src/todo.rs`.

State lives under `ToolContext.state_dir`
(`~/.conga/tool_state/<session_id>/<tool_name>/`), not in a shared map.

---

## Example 3: `permission_gate` — policy hook

Source: `conga-ext/src/permission_gate.rs`.

`before_tool_call` can `Block` dangerous `bash` patterns; the loop skips
execution and returns the reason to the model as an error tool result.

Note: production CLI already uses `conga_host::PermissionPolicy` as a
`HookChain`. This example shows the same idea via `ExtensionApi`.

---

## Optional Cargo feature pattern

```toml
# conga-cli already wires this:
conga-ext = { workspace = true, optional = true }
[features]
ext = ["dep:conga-ext"]
```

```rust
#[cfg(feature = "ext")]
{
    conga_ext::hello::register(&mut api);
    conga_ext::todo::register(&mut api);
}
```

Do **not** split built-in tools into per-tool features.

---

## Run the examples

```bash
cargo run -p conga --example plugins
cargo test -p conga --test plugins_example
```

---

## External tools (non-Rust)

For any language, host spawns a long-lived process and speaks JSONL on stdio
(`conga_host::ExternalToolBridge`). Example: `examples/external_echo.py`.

```bash
export CONGA_EXTERNAL_TOOLS="python3 path/to/external_echo.py"
# in REPL: /reload-tools  # kill + re-list (in-process Rust extensions do not reload)
```

Protocol: `{"op":"list"}` / `{"op":"call",...}` — one JSON object per line.
Does **not** expose `ExtensionApi` over the wire.

---

## Summary

- Extension = `pub fn register(&mut dyn ExtensionApi)` in a normal Rust crate.
- Host links crates and calls `register` (features optional).
- Built-ins stay in core; no cdylib, no ABI handshake, no unload.
- Non-Rust tools: stdio JSONL external process + optional `/reload-tools`.
