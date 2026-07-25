//! LLM providers — OpenAI-compatible + Anthropic.
//!
//! See `gasket-refactor-plan.md` §8.

pub mod anthropic;
pub mod openai_compat;
pub mod sse;

pub use anthropic::AnthropicProvider;
pub use openai_compat::OpenAiCompat;
