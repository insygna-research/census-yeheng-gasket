//! Built-in tools: read / write / edit / bash / list / grep.
//!
//! See `gasket-refactor-plan.md` §7.

pub mod bash;
pub mod edit;
pub mod grep;
pub mod list;
pub mod read;
pub mod write;

use crate::types::tool::ToolDefinition;

/// The 6 built-in tools, ready to drop into `AgentContext.tools`.
pub fn built_in_tools() -> Vec<ToolDefinition> {
    vec![
        read::tool(),
        write::tool(),
        edit::tool(),
        bash::tool(),
        list::tool(),
        grep::tool(),
    ]
}
