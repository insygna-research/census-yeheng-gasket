//! gasket-host - 可复用的 host 层（配置/session/权限/事件渲染）。
pub mod config;
pub mod session;
pub mod permission;
pub mod printer;

pub use config::{ConfigLoader, HostConfig};
pub use session::{SessionInfo, SessionManager};
pub use permission::{Mode, PermissionPolicy, RiskLevel};
pub use printer::EventPrinter;

#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("config error: {0}")]
    Config(#[from] gasket_core::ConfigError),
    #[error("session error: {0}")]
    Session(String),
}
