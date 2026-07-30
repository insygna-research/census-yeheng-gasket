# Writing a gasket Extension Crate

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

Official examples: workspace crate **`gasket-ext`** (`hello`, `todo`,
`permission_gate`). CLI: `cargo run -p gasket-cli --features ext`.

Built-in tools (`read` / `write` / `edit` / `bash` / `list` / `grep`) live in
`gasket-core` and are not extension crates.

---

## Host wiring

```rust
let mut api = ExtensionApiImpl::new();
gasket_ext::register_all(&mut api); // or hello::register / todo::register

let mut tools = gasket_core::built_in_tools();
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
| `register_tool(ToolDefinition)` | add a tool the LLM may call | `hello`, `todo_list` |
| `register_before_tool_call(handler)` | block / modify args before run | `permission_gate` |
| `register_after_tool_call(handler)` | rewrite tool result | — |
| `register_event_handler(handler)` | observe events | — |
| `send_message(msg)` | queue a message for the host | — |
| `current_messages()` | read session snapshot (host-filled) | — |

**Events vs hooks are type-separated.** Event handlers only observe.
`before_tool_call` returns `ToolCallVerdict` (`Allow` / `Block` / `Modify`).

---

## Example 1: `hello` — minimum extension

Source: `gasket-ext/src/hello.rs`.

```rust
pub fn register(api: &mut (impl ExtensionApi + ?Sized)) {
    api.register_tool(ToolDefinition {
        name: "hello".into(),
        label: "Hello".into(),
        description: "Say hello to someone.".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": { "name": { "type": "string" } },
            "required": ["name"]
        }),
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
- `details` is extension-private; the agent never reads it.

---

## Example 2: `todo` — private state files

Source: `gasket-ext/src/todo.rs`.

State lives under `ToolContext.state_dir`
(`~/.gasket/tool_state/<session_id>/<tool_name>/`), not in a shared map.

---

## Example 3: `permission_gate` — policy hook

Source: `gasket-ext/src/permission_gate.rs`.

`before_tool_call` can `Block` dangerous `bash` patterns; the loop skips
execution and returns the reason to the model as an error tool result.

Note: production CLI already uses `gasket_host::PermissionPolicy` as a
`HookChain`. This example shows the same idea via `ExtensionApi`.

---

## Optional Cargo feature pattern

```toml
# gasket-cli already wires this:
gasket-ext = { workspace = true, optional = true }
[features]
ext = ["dep:gasket-ext"]
```

```rust
#[cfg(feature = "ext")]
{
    gasket_ext::hello::register(&mut api);
    gasket_ext::todo::register(&mut api);
}
```

Do **not** split built-in tools into per-tool features.

---

## Run the examples

```bash
cargo run -p gasket-core --example plugins
cargo test -p gasket-core --test plugins_example
```

---

## External tools (non-Rust)

For any language, host spawns a long-lived process and speaks JSONL on stdio
(`gasket_host::ExternalToolBridge`). Example: `examples/external_echo.py`.

```bash
export GASKET_EXTERNAL_TOOLS="python3 path/to/external_echo.py"
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
