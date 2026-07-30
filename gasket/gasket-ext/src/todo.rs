//! `todo` tool — private state under `ToolContext.state_dir`.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use gasket_core::{ContentBlock, ExtensionApi, ToolCallCtx, ToolDefinition, ToolError, ToolResult};

#[derive(Default, Serialize, Deserialize, Clone)]
struct Todo {
    id: u64,
    text: String,
    done: bool,
}

#[derive(Default, Serialize, Deserialize, Clone)]
struct State {
    todos: Vec<Todo>,
    next_id: u64,
}

fn load(ctx: &gasket_core::ToolContext) -> State {
    let path = ctx.state_dir.join("todos.json");
    std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save(ctx: &gasket_core::ToolContext, state: &State) {
    let _ = std::fs::create_dir_all(&ctx.state_dir);
    if let Ok(bytes) = serde_json::to_vec(state) {
        let _ = std::fs::write(ctx.state_dir.join("todos.json"), bytes);
    }
}

pub fn register(api: &mut dyn ExtensionApi) {
    api.register_tool(ToolDefinition {
        name: "todo".into(),
        label: "Todo".into(),
        description: "Manage a todo list. action: add|list|toggle|clear.".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["add", "list", "toggle", "clear"] },
                "text": { "type": "string", "description": "for add" },
                "id": { "type": "integer", "description": "for toggle" }
            },
            "required": ["action"]
        }),
        execute: Arc::new(|ctx: ToolCallCtx| Box::pin(async move { execute(&ctx).await })),
    });
}

async fn execute(ctx: &ToolCallCtx) -> Result<ToolResult, ToolError> {
    let action = ctx.args["action"].as_str().unwrap_or("list");
    let mut state = load(&ctx.ctx);

    let (text, is_error) = match action {
        "add" => {
            let t = ctx.args["text"].as_str().unwrap_or("").to_string();
            let id = state.next_id;
            state.next_id += 1;
            state.todos.push(Todo {
                id,
                text: t,
                done: false,
            });
            save(&ctx.ctx, &state);
            (format!("Added #{}", id), false)
        }
        "toggle" => {
            let id = ctx.args["id"].as_u64().unwrap_or(0);
            if let Some(t) = state.todos.iter_mut().find(|t| t.id == id) {
                t.done = !t.done;
            }
            save(&ctx.ctx, &state);
            (format!("Toggled #{}", id), false)
        }
        "clear" => {
            state.todos.clear();
            save(&ctx.ctx, &state);
            ("Cleared all todos.".into(), false)
        }
        _ => {
            if state.todos.is_empty() {
                ("(no todos)".into(), false)
            } else {
                let body = state
                    .todos
                    .iter()
                    .map(|t| format!("{} [{}] {}", t.id, if t.done { "x" } else { " " }, t.text))
                    .collect::<Vec<_>>()
                    .join("\n");
                (body, false)
            }
        }
    };

    Ok(ToolResult {
        content: vec![ContentBlock::text(text)],
        details: serde_json::to_value(&state).unwrap_or_default(),
        is_error,
    })
}
