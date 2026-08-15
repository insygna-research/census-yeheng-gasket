//! Optional in-process extensions. Host enables via Cargo feature and calls
//! [`register_all`] (or individual module `register`s); the production-only
//! composition root [`prod_register`] is used by the desktop app.

pub mod hello;
pub mod permission_gate;
pub mod search;
pub mod todo;

#[cfg(feature = "terminal")]
pub mod terminal;

use gasket_core::ExtensionApi;

/// Production extensions only (no demo tools). Hosts whose users did not
/// opt into the demo set (the desktop app) compose from here; the CLI keeps
/// [`register_all`] behind `--features ext`.
pub fn prod_register(api: &mut dyn gasket_core::ExtensionApi) {
    search::register(api);
    #[cfg(feature = "terminal")]
    terminal::register(api);
}

/// Register every extension in this crate.
pub fn register_all(api: &mut dyn ExtensionApi) {
    prod_register(api);
    hello::register(api);
    todo::register(api);
    permission_gate::register(api);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prod_register_has_search_only() {
        let mut api = gasket_core::ExtensionApiImpl::new();
        prod_register(&mut api);
        let names: Vec<_> = api.tools.iter().map(|t| t.name.clone()).collect();
        // `--all-features` (CI) turns the terminal feature on; without it the
        // module is compiled out entirely.
        let expected: Vec<&str> = if cfg!(feature = "terminal") {
            vec!["web_search", "terminal"]
        } else {
            vec!["web_search"]
        };
        assert_eq!(names, expected);
    }
}
