//! Messaging channel core types for gasket.
//!
//! This crate provides only the core channel abstractions:
//! - Channel types (`events`, `config`, `adapter`, `middleware`, `provider`)
//! - Session addressing (`SessionKey`, `ChannelType`)
//!
//! Platform adapter implementations (Telegram/Discord/Slack/Feishu/WeChat/WebSocket)
//! were removed in the V0.1 refactor — they belong in plugins now.

// Core types
pub mod adapter;
pub mod approval_router;
pub mod config;
pub mod error;
pub mod events;
pub mod middleware;
pub mod provider;

// Platform adapter implementations (telegram/discord/slack/feishu/wechat/websocket)
// were removed in the V0.1 refactor. Only core types remain.

// Convenience re-exports
pub use adapter::ImAdapter;
pub use approval_router::ApprovalRouter;
pub use config::{
    ChannelsConfig, DiscordConfig, FeishuConfig, SlackConfig, TelegramConfig, WebSocketConfig,
    WechatConfig,
};
pub use error::ChannelConfigError;
pub use events::{
    ChannelType, InboundMessage, MediaAttachment, OutboundMessage, SessionKey,
    SessionKeyParseError, WebSocketMessage,
};
pub use middleware::{
    log_inbound, ChannelError, InboundSender, SimpleAuthChecker, SimpleRateLimiter,
};
pub use provider::{ImProvider, ImProviders};
