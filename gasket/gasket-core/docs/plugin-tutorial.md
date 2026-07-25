# Writing a gasket Plugin

A **plugin** extends the agent without touching its core. gasket's agent loop
(`agent_loop`) is fixed; everything else — extra tools, command policies,
audit logging — is a plugin that registers capabilities via the `ExtensionApi`.

This tutorial walks through three real plugins (source in `examples/plugins/`):
a tool, a stateful tool, and a policy hook. Each is <100 lines.

---

## What a plugin is

A plugin is a Rust crate compiled as a `cdylib` that exports one entry point:

```rust
#[no_mangle]
pub extern "C" fn register(api: &mut dyn ExtensionApi) {
    // call api.register_tool / api.register_before_tool_call / ...
}
```

The host loads it with `gasket_core::extension::load_plugin`, which:

1. reads the `manifest.toml` next to the `.so`/`.dylib`,
2. checks `gasket_abi_version` matches the host's `GASKET_ABI_VERSION` (currently `1`),
3. calls `register`, letting the plugin wire itself in.

That's the entire plugin protocol. No JSON-RPC, no subprocess, no serialization
of calls — the plugin runs **in-process**, calling the agent's real types
directly. The trade-off (see §5.1.1 of the refactor plan): a plugin must be
compiled with the **same toolchain and dependency versions** as the host. This
is the cdylib honesty contract.

> For development and tests you don't need the cdylib dance — call a plugin's
> `register(&mut api)` directly. The `examples/plugins.rs` host does exactly
> this. The cdylib path is only for distributing a plugin as a loadable file.

---

## The `ExtensionApi` surface

Everything a plugin can do is one of these methods on `&mut impl ExtensionApi`:

| Method | What it does | Example plugin |
|---|---|---|
| `register_tool(ToolDefinition)` | add a tool the LLM may call | `hello`, `todo_list` |
| `register_before_tool_call(handler)` | intercept a call before it runs (block / modify args) | `permission_gate` |
| `register_after_tool_call(handler)` | rewrite a tool's result (redact, compress) | — |
| `register_event_handler(handler)` | observe events (audit, persist) | — |
| `send_message(msg)` | inject a message into the session | — |

**Events vs hooks are type-separated.** `register_event_handler` handlers
return nothing — they observe. `register_before_tool_call` handlers return a
`ToolCallVerdict` (`Allow` / `Block(reason)` / `Modify(args)`) that controls
agent flow. The two cannot be confused at the type level. See §3.2 / §3.5 of
the refactor plan.

---

## Example 1: `hello` — the minimum plugin

Source: `examples/plugins/hello.rs` (~45 lines).

```rust
use std::sync::Arc;
use gasket_core::{ContentBlock, ExtensionApi, ToolDefinition, ToolResult};

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

Notes:

- `parameters` is a raw JSON Schema. The host validates args before calling
  `execute`; your tool gets them already-parsed in `ctx.args`.
- `execute` is `Arc<dyn Fn(ToolCallCtx) -> BoxFuture<ToolResult>>`. The `Arc`
  lets the agent share it across turns.
- `details` is **plugin-private** — the agent never reads it. Use it for data
  only your plugin cares about.

---

## Example 2: `todo_list` — plugin-private state

Source: `examples/plugins/todo_list.rs` (~140 lines).

The key idea: **a plugin keeps its own state in its own files**, not in any
agent-owned shared map. Each tool call receives a `ToolContext` with a private
`state_dir`:

```
~/.gasket/tool_state/<session_id>/<tool_name>/todos.json
```

```rust
fn load(ctx: &gasket_core::ToolContext) -> State {
    std::fs::read(ctx.state_dir.join("todos.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}
```

This replaces the old "shared metadata HashMap" pattern. Benefits:

- **typed**: your `State` struct, not `serde_json::Value` with key collisions.
- **isolated**: no other plugin can clobber your keys.
- **agent-agnostic**: the agent core never touches it.

The full example supports `add` / `list` / `toggle` / `clear` and persists
after each mutation.

---

## Example 3: `permission_gate` — block dangerous commands

Source: `examples/plugins/permission_gate.rs` (~45 lines).

A `before_tool_call` hook runs before **every** tool call and can refuse it:

```rust
impl BeforeToolCallHandler for DangerousCommandGate {
    fn call(&self, _id: &str, tool_name: &str, args: &serde_json::Value, _ctx: &ExtensionContext)
        -> ToolCallVerdict
    {
        if tool_name == "bash" {
            let cmd = args["command"].as_str().unwrap_or("");
            if cmd.contains("rm -rf") || cmd.contains("sudo ") {
                return ToolCallVerdict::Block("Refused: dangerous pattern.".into());
            }
        }
        ToolCallVerdict::Allow
    }
}

pub fn register(api: &mut (impl ExtensionApi + ?Sized)) {
    api.register_before_tool_call(Box::new(DangerousCommandGate));
}
```

When the model asks to run `bash` with `rm -rf`, the agent loop sees `Block`:
it **skips execution entirely** and sends the block reason back to the model as
an error tool result. The model then reacts (asks the user, picks a safer
command). No dangerous command ever runs.

The three verdicts:

- `Allow` — proceed.
- `Block(reason)` — refuse; `reason` becomes the tool result the model sees.
- `Modify(new_args)` — replace the args, then execute.

Multiple `before_tool_call` handlers combine: the first `Block` wins; otherwise
the last `Modify` wins.

---

## Building a plugin as a cdylib

For distribution, a plugin is its own crate:

```toml
# Cargo.toml
[package]
name = "hello-plugin"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
gasket-core = { path = "../../gasket-core" }
serde_json = "1"
```

```toml
# manifest.toml (next to the built .so/.dylib)
name = "hello"
version = "0.1.0"
gasket_abi_version = 1
description = "A greeting plugin"
```

```bash
cargo build --release
# produces target/release/libhello.so (or .dylib / .dll)
```

Drop the built library + its `manifest.toml` into `~/.gasket/plugins/hello/`.
The host's `discover_plugins` finds it on next start, `load_plugin` checks the
ABI version, and calls `register`.

### The ABI honesty contract

`gasket_abi_version` is **independent of the crate semantic version**. It bumps
whenever a struct layout, enum discriminant, or trait vtable changes. A plugin
built against ABI version `1` loads into a host at ABI `1`; against ABI `2` it
is refused. This is the only thing preventing memory corruption from a layout
mismatch — there is no stable Rust cdylib ABI otherwise.

Concretely: **rebuild your plugin whenever you upgrade gasket-core**, with the
same `rustc` and the same major versions of shared dependencies (`tokio`,
`reqwest`, `serde`). If you need cross-language or independently-versioned
plugins, that is the use case for a subprocess + JSON-RPC design (not V0.1).

---

## Running the example plugins

The `plugins` example loads all three in-process (no cdylib) and runs one turn
against a mock provider:

```bash
cargo run --example plugins
```

The integration test `tests/plugins_example.rs` proves the examples are correct
against the real agent loop:

```bash
cargo test --test plugins_example
```

---

## Summary

- A plugin is one `register(&mut ExtensionApi)` function.
- Register tools (`register_tool`), policies (`register_before_tool_call`), or
  observers (`register_event_handler`).
- Keep state in `ToolContext.state_dir`, not in any shared map.
- Distribute as a cdylib + `manifest.toml` with a matching `gasket_abi_version`.

See `gasket-refactor-plan.md` §3.5 / §5 for the full API reference.
