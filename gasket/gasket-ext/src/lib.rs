//! Optional in-process extensions. Host enables via Cargo feature and calls
//! [`register_all`] (or individual module `register`s).

pub mod hello;
pub mod permission_gate;
pub mod search;
pub mod todo;

use gasket_core::ExtensionApi;

/// Register every extension in this crate.
pub fn register_all(api: &mut dyn ExtensionApi) {
    hello::register(api);
    todo::register(api);
    permission_gate::register(api);
    search::register(api);
}
