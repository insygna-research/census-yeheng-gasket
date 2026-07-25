//! Example plugin: `hello` tool — the plugin "hello world".
//!
//! Demonstrates the minimum plugin shape: a `register` function that adds one
//! tool to the agent. The tool greets a name passed in `args["name"]`.
//!
//! As a library entry point (used by the integration tests) it exposes
//! [`register`]. As a real cdylib it would also `#[no_mangle]` the same symbol
//! so `loader::load_plugin` can find it — see the bottom of this file.
//!
//! See `docs/plugin-tutorial.md` for a walkthrough.

use std::sync::Arc;

use gasket_core::{ContentBlock, ExtensionApi, ToolDefinition, ToolResult};

/// Register the `hello` tool with the agent.
///
/// A plugin's `register` is its only entry point. It receives the
/// `ExtensionApi` and calls `register_tool` / `register_before_tool_call` /
/// `register_event_handler` as needed. Nothing else.
pub fn register(api: &mut (impl ExtensionApi + ?Sized)) {
    api.register_tool(ToolDefinition {
        name: "hello".into(),
        label: "Hello".into(),
        description: "Say hello to someone. Pass {\"name\": \"...\"}.".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Who to greet" }
            },
            "required": ["name"]
        }),
        execute: Arc::new(|ctx| {
            Box::pin(async move {
                let name = ctx.args["name"].as_str().unwrap_or("world");
                Ok(ToolResult {
                    content: vec![ContentBlock::text(format!("Hello, {}!", name))],
                    details: serde_json::json!({ "greeted": name }),
                    is_error: false,
                })
            })
        }),
    });
}

// ── cdylib entry point (for the standalone-plugin build) ─────────────────
// When this file is the lib of its OWN crate (`crate-type = ["cdylib"]`), the
// cdylib entry point looks like the function below. It is intentionally NOT
// compiled here (this example builds all three plugins into one binary, where
// a `#[no_mangle]` symbol would clash); see `docs/plugin-tutorial.md` for the
// real standalone-crate setup.
//
//     #[no_mangle]
//     pub extern "C" fn register(api: &mut dyn ExtensionApi) {
//         register(api);
//     }
//
// Note: `dyn ExtensionApi` across an FFI boundary is not strictly FFI-safe
// (the loader suppresses that lint), and the plugin must be compiled with the
// same toolchain as the host — see §5.1.1 of the refactor plan. The
// `gasket_abi_version = 1` in the matching `manifest.toml` must equal
// `gasket_core::extension::loader::GASKET_ABI_VERSION`.
