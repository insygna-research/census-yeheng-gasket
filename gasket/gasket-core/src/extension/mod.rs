//! Extension API + cdylib plugin loader.
//!
//! See `gasket-refactor-plan.md` §3.5 / §5.

pub mod api;
pub mod loader;

pub use api::{
    AfterToolCallHandler, BeforeToolCallHandler, EventHandler, ExtensionApi, ExtensionApiImpl,
    ExtensionContext,
};
pub use loader::{discover_plugins, load_plugin, Plugin, PluginManifest};
