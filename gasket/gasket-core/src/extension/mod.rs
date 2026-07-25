//! Extension API + cdylib plugin loader.
//!
//! See `gasket-refactor-plan.md` §3.5 / §5.

pub mod api;
pub mod loader;

pub use api::{
    AfterToolCallHandler, BeforeToolCallHandler, ExtensionApi, ExtensionApiImpl, ExtensionContext,
    EventHandler,
};
pub use loader::{discover_plugins, load_plugin, Plugin, PluginManifest};
