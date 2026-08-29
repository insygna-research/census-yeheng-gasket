//! `/evolve` — distill a session transcript into memory insights and
//! skills, admitted one-by-one through the human approver. The read side
//! (`memory.rs`) only ever catalogs; everything here is the write side.

use conga::types::message::{AgentMessage, ContentBlock};
use serde::Deserialize;
use std::path::Path;

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

#[derive(Debug, Default, Deserialize)]
pub struct EvolveProposal {
    #[serde(default)]
    pub insights: Vec<InsightProposal>,
    #[serde(default)]
    pub skills: Vec<SkillProposal>,
    #[serde(default)]
    pub retires: Vec<String>,
    #[serde(default)]
    pub duplicates: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct InsightProposal {
    pub title: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub content: String,
}

#[derive(Debug, Deserialize)]
pub struct SkillProposal {
    pub name: String,
    pub description: String,
    pub body: String,
}

/// The one prompt the extraction sub-agent sees. The quality contract is
/// the point: insights must be root-cause + applied fix + evidence, or
/// the library fills with platitudes that get injected into every future
/// prompt (see docs/evolve.md "Content quality").
pub fn extraction_task_prompt(trajectory: &str, catalog: &str) -> String {
    format!(
        "You are a distillation engine for a coding assistant. You get ONE \
session transcript. Extract reusable knowledge for FUTURE sessions. Reply \
with ONLY a JSON object — no prose, no markdown fences.\n\n\
Rules for \"insights\":\n\
- Only what THIS session proved: each insight must state the root cause, the \
fix actually applied, and the evidence (what happened in the transcript).\n\
- No general advice (\"read errors carefully\"), no restating the task.\n\
- tags: 2-5 lowercase single-word tokens likely to appear in future tasks.\n\
- content: <= 2KB, imperative, self-contained.\n\n\
Rules for \"skills\": repeatable multi-step procedures actually demonstrated \
in the transcript; name = kebab-case; description = one line saying when to \
use it.\n\
Rules for \"retires\": titles from the existing library that this session \
proved obsolete or wrong.\n\
Rules for \"duplicates\": existing titles your new entries would duplicate — \
list them here instead of re-proposing them.\n\n\
Existing library:\n{catalog}\n\n\
Transcript:\n{trajectory}\n\n\
Output schema:\n\
{{\"insights\":[{{\"title\":\"\",\"tags\":[\"\"],\"content\":\"\"}}],\
\"skills\":[{{\"name\":\"\",\"description\":\"\",\"body\":\"\"}}],\
\"retires\":[\"\"],\"duplicates\":[\"\"]}}"
    )
}

/// Parse the extractor's reply: take the outermost {...} span so prose or
/// markdown fences around the JSON are tolerated; fail loud otherwise —
/// a silently empty proposal would look like "nothing to learn".
pub fn parse_proposal(output: &str) -> Result<EvolveProposal, conga::AgentError> {
    let start = output.find('{').ok_or_else(|| {
        conga::AgentError::Tool(format!(
            "extractor output has no JSON object: {}",
            bound(output, 200)
        ))
    })?;
    let end = output
        .rfind('}')
        .ok_or_else(|| conga::AgentError::Tool("extractor output has no closing brace".into()))?;
    let json = &output[start..=end];
    serde_json::from_str(json).map_err(conga::AgentError::Serde)
}

/// Library snapshot for the extractor input: every existing memory entry
/// and skill, so it proposes deltas rather than echoes.
pub fn catalog_snapshot(memory_root: &Path, cwd: &Path, global_root: &Path) -> String {
    let mut out = String::new();
    for e in crate::memory::load_entries(memory_root) {
        out.push_str(&format!("memory: {} [{}]\n", e.title, e.tags.join(", ")));
    }
    for (name, desc) in crate::skills::catalog_entries(cwd, global_root) {
        out.push_str(&format!("skill: {name} — {desc}\n"));
    }
    out
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

    #[test]
    fn parses_proposal_with_surrounding_prose() {
        let raw = "Here you go:\n```json\n{\"insights\":[{\"title\":\"t\",\"tags\":[\"a\"],\"content\":\"c\"}],\"skills\":[],\"retires\":[\"old\"],\"duplicates\":[]}\n```\nthanks";
        let p = parse_proposal(raw).unwrap();
        assert_eq!(p.insights.len(), 1);
        assert_eq!(p.insights[0].title, "t");
        assert_eq!(p.retires, vec!["old".to_string()]);
    }

    #[test]
    fn missing_keys_default_to_empty() {
        let p = parse_proposal("{}").unwrap();
        assert!(p.insights.is_empty() && p.skills.is_empty());
    }

    #[test]
    fn garbage_fails_loud() {
        assert!(parse_proposal("no json at all").is_err());
    }

    #[test]
    fn extraction_prompt_carries_quality_contract() {
        let p = extraction_task_prompt("TRAJ", "CATALOG");
        assert!(p.contains("root cause"));
        assert!(p.contains("evidence"));
        assert!(p.contains("CATALOG"));
        assert!(p.contains("TRAJ"));
        assert!(p.contains("ONLY a JSON object"));
    }
}
