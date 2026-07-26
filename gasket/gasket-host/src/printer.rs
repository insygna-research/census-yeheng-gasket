//! EventPrinter: 消费 AgentEvent，渲染到注入的 writer（可测/可复用）。
use std::io::Write;

use gasket_core::{AgentEvent, ContentDelta};

pub struct EventPrinter<W: Write> {
    out: W,
}

impl<W: Write> EventPrinter<W> {
    pub fn new(out: W) -> Self {
        Self { out }
    }

    pub fn on_event(&mut self, ev: &AgentEvent) {
        match ev {
            AgentEvent::MessageUpdate { delta } => match delta {
                ContentDelta::TextDelta(t) => {
                    let _ = self.out.write_all(t.as_bytes());
                }
                ContentDelta::ThinkingDelta(_) => {}
                ContentDelta::ToolCallDelta { .. } => {}
            },
            AgentEvent::ToolExecutionStart { tool_name, args, .. } => {
                let _ = writeln!(self.out, "\n-> {tool_name} {}", args);
            }
            AgentEvent::ToolExecutionEnd { result, is_error, .. } => {
                let first = result
                    .content
                    .iter()
                    .find_map(|b| match b {
                        gasket_core::ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .unwrap_or("");
                let head = first.lines().next().unwrap_or("");
                let tag = if *is_error { "ERR" } else { "ok" };
                let _ = writeln!(self.out, "   [{tag}] {head}");
            }
            AgentEvent::AfterProviderResponse { response, .. } => {
                if let Some(u) = &response.usage {
                    let _ = writeln!(self.out, "\n[in: {}, out: {}]", u.input_tokens, u.output_tokens);
                }
            }
            AgentEvent::TurnEnd { .. } => {
                let _ = writeln!(self.out);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gasket_core::AssistantMessage;

    #[test]
    fn streams_text_delta() {
        let mut buf: Vec<u8> = Vec::new();
        let mut p = EventPrinter::new(&mut buf);
        p.on_event(&AgentEvent::MessageUpdate {
            delta: ContentDelta::TextDelta("Hi".into()),
        });
        p.on_event(&AgentEvent::MessageUpdate {
            delta: ContentDelta::TextDelta(" there".into()),
        });
        drop(p);
        assert_eq!(String::from_utf8(buf).unwrap(), "Hi there");
    }

    #[test]
    fn prints_usage_after_response() {
        let mut buf: Vec<u8> = Vec::new();
        let mut p = EventPrinter::new(&mut buf);
        let model = "m".to_string();
        let mut msg = AssistantMessage::new(&model);
        msg.usage = Some(gasket_core::types::message::Usage {
            input_tokens: 42,
            output_tokens: 7,
        });
        let _ = msg.stop_reason;
        p.on_event(&AgentEvent::AfterProviderResponse {
            model: "m".into(),
            response: msg,
        });
        drop(p);
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("in: 42") && s.contains("out: 7"));
    }
}
