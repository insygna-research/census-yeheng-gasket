//! IM adapter trait — unified inbound/outbound interface for messaging platforms.
//!
//! Replaces the old split design (Channel for inbound + OutboundSender for outbound)
//! with a single trait per platform.

use async_trait::async_trait;

/// Unified adapter for a single messaging platform.
///
/// Each platform implements this trait to handle both message ingestion
/// (inbound) and message delivery (outbound).
#[async_trait]
pub trait ImAdapter: Send + Sync {
    /// Platform name, e.g. "telegram".
    fn name(&self) -> &str;

    /// Start the inbound message loop.
    ///
    /// For bot-based platforms (Telegram, Discord, Slack) this blocks and
    /// pushes incoming messages into the supplied sender.
    /// For webhook-based platforms this is typically a no-op because inbound
    /// messages arrive via HTTP callbacks.
    async fn start(&self, inbound: crate::middleware::InboundSender) -> anyhow::Result<()>;

    /// Send an outbound message.
    async fn send(&self, msg: &crate::events::OutboundMessage) -> anyhow::Result<()>;
}

/// No-op adapter for the local CLI channel.
///
/// CLI sessions don't use a real transport; inbound/outbound go directly through
/// stdin/stdout. This adapter satisfies the `ImAdapter` contract with empty
/// implementations so the provider registry has a placeholder for `ChannelType::Cli`.
#[derive(Clone, Copy)]
pub struct CliAdapter;

#[async_trait]
impl ImAdapter for CliAdapter {
    fn name(&self) -> &str {
        "cli"
    }

    async fn start(&self, _inbound: crate::middleware::InboundSender) -> anyhow::Result<()> {
        Ok(())
    }

    async fn send(&self, _msg: &crate::events::OutboundMessage) -> anyhow::Result<()> {
        Ok(())
    }
}
