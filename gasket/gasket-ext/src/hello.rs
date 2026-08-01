//! `hello` tool — minimum extension shape.

use std::sync::Arc;

use gasket_core::{ContentBlock, ExtensionApi, RiskLevel, ToolDefinition, ToolResult};

pub fn register(api: &mut dyn ExtensionApi) {
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
        risk: RiskLevel::High,
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
