//! `/evolve` — distill a session transcript into memory insights and
//! skills, admitted one-by-one through the human approver. The read side
//! (`memory.rs`) only ever catalogs; everything here is the write side.

use conga::types::message::{AgentMessage, ContentBlock};

/// Render derived messages to compact extraction input. Oldest messages
/// are dropped first when over budget (the freshest context — where the
/// mistake and its correction live — always survives), and the truncation
/// is flagged so the extractor knows the transcript has a hole.
pub fn render_trajectory(messages: &[AgentMessage], max_chars: usize) -> String {
    let mut blocks: Vec<String> = Vec::with_capacity(messages.len());
    for m in messages {
        match m {
            AgentMessage::User(u) => {
                for b in &u.content {
                    if let ContentBlock::Text { text } = b {
                        blocks.push(format!("## USER\n{text}"));
                    }
                }
            }
            AgentMessage::Assistant(a) => {
                let mut out = String::new();
                for b in &a.content {
                    match b {
                        ContentBlock::Thinking { .. } => {}
                        ContentBlock::Text { text } => {
                            if !text.trim().is_empty() {
                                out.push_str(&format!("## ASSISTANT\n{text}"));
                            }
                        }
                        ContentBlock::ToolCall { tool_call } => {
                            out.push_str(&format!(
                                "\n- tool call: {}({})\n",
                                tool_call.function.name,
                                bound(&tool_call.function.arguments, 200)
                            ));
                        }
                    }
                }
                if !out.trim().is_empty() {
                    blocks.push(out.trim().to_string());
                }
            }
            AgentMessage::ToolResult(r) => {
                for b in &r.content {
                    if let ContentBlock::Text { text } = b {
                        blocks.push(format!(
                            "## TOOL RESULT ({})\n{}",
                            r.tool_name,
                            bound(text, 2_000)
                        ));
                    }
                }
            }
            AgentMessage::Custom(_) => {}
        }
    }
    // Budget: drop whole oldest blocks until the joined text fits.
    let mut start = 0;
    loop {
        let joined = blocks[start..].join("\n\n");
        if joined.chars().count() <= max_chars || start >= blocks.len() {
            if start == 0 {
                return joined;
            }
            return format!("(older messages truncated — {start} blocks dropped)\n\n{joined}");
        }
        start += 1;
    }
}

/// Char-boundary-safe truncation.
fn bound(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut t: String = s.chars().take(max).collect();
    t.push('…');
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use conga::types::message::{FunctionCall, ToolCall};
    use conga::{AgentMessage, ContentBlock, ToolResultMessage};

    #[test]
    fn renders_roles_and_tool_calls() {
        let msgs = vec![
            AgentMessage::user("fix the build"),
            AgentMessage::Assistant(conga::AssistantMessage {
                content: vec![
                    ContentBlock::text("trying a rebuild"),
                    ContentBlock::ToolCall {
                        tool_call: ToolCall {
                            id: "t1".into(),
                            function: FunctionCall {
                                name: "bash".into(),
                                arguments: r#"{"command":"cargo build"}"#.into(),
                            },
                        },
                    },
                ],
                model: "m".into(),
                stop_reason: conga::StopReason::ToolUse,
                usage: None,
                timestamp: 0,
                stream_indices: Vec::new(),
            }),
            AgentMessage::ToolResult(ToolResultMessage {
                tool_call_id: "t1".into(),
                tool_name: "bash".into(),
                content: vec![ContentBlock::text("error: cyclic dependency")],
                is_error: false,
                timestamp: 0,
            }),
        ];
        let out = render_trajectory(&msgs, 10_000);
        assert!(out.contains("## USER\nfix the build"));
        assert!(out.contains("## ASSISTANT\ntrying a rebuild"));
        assert!(out.contains("- tool call: bash("));
        assert!(out.contains("## TOOL RESULT (bash)\nerror: cyclic dependency"));
    }

    #[test]
    fn truncates_oldest_first_and_flags() {
        let mut msgs = Vec::new();
        for i in 0..100 {
            msgs.push(AgentMessage::user(format!(
                "message number {i} with padding padding padding"
            )));
        }
        let out = render_trajectory(&msgs, 2_000);
        assert!(out.starts_with("(older messages truncated"));
        assert!(!out.contains("message number 0"));
        assert!(out.contains("message number 99"));
    }
}
