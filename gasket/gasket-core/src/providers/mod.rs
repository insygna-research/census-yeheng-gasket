//! LLM providers - OpenAI-compatible + Anthropic.

pub mod anthropic;
pub mod openai_compat;
pub mod sse;

pub use anthropic::AnthropicProvider;
pub use openai_compat::OpenAiCompat;

use crate::types::context::ProviderApi;

/// Connection + model config for an LLM provider, read from environment.
///
/// Recognized env vars (all optional unless noted):
/// - `GASKET_LLM_BASE_URL` - provider base URL (e.g. `https://api.deepseek.com/v1`)
/// - `GASKET_LLM_KEY`      - API key (auth bearer / x-api-key)
/// - `GASKET_LLM_MODEL`    - model id to call
/// - `GASKET_LLM_API`      - `openai` (default) or `anthropic` - wire protocol
/// - `GASKET_LLM_PROXY`    - proxy used for BOTH http and https (fallback)
/// - `GASKET_LLM_HTTP_PROXY`  - proxy for http requests (overrides GASKET_LLM_PROXY for http)
/// - `GASKET_LLM_HTTPS_PROXY` - proxy for https requests (overrides GASKET_LLM_PROXY for https)
///
/// Proxy precedence: per-scheme (`HTTP_PROXY`/`HTTPS_PROXY`) wins; `GASKET_LLM_PROXY`
/// fills in for whichever scheme has no explicit proxy.
#[derive(Debug, Clone)]
pub struct ProviderConfig {
    /// Wire protocol the provider speaks. Determines which provider impl to
    /// build and how auth headers are sent.
    pub api: ProviderApi,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    /// A reqwest client with any configured proxies applied. Built once,
    /// shared by all calls.
    pub client: reqwest::Client,
}

/// Errors from building a [`ProviderConfig`].
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("missing required env var: {0}")]
    Missing(&'static str),
    #[error("invalid proxy URL {var}={url}: {source}")]
    BadProxy {
        var: &'static str,
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("could not build HTTP client: {0}")]
    ClientBuild(reqwest::Error),
}

impl ProviderConfig {
    /// Read config from process environment.
    ///
    /// Requires `GASKET_LLM_BASE_URL`, `GASKET_LLM_KEY`, `GASKET_LLM_MODEL`.
    /// Proxies are optional.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_env_with(&|k: &str| std::env::var(k))
    }

    /// Same as [`from_env`](Self::from_env) but with an injectable lookup -
    /// used by tests to avoid mutating process env.
    pub fn from_env_with(
        lookup: &dyn Fn(&str) -> Result<String, std::env::VarError>,
    ) -> Result<Self, ConfigError> {
        let base_url = lookup("GASKET_LLM_BASE_URL")
            .map_err(|_| ConfigError::Missing("GASKET_LLM_BASE_URL"))?;
        let api_key =
            lookup("GASKET_LLM_KEY").map_err(|_| ConfigError::Missing("GASKET_LLM_KEY"))?;
        let model =
            lookup("GASKET_LLM_MODEL").map_err(|_| ConfigError::Missing("GASKET_LLM_MODEL"))?;
        let api = match lookup("GASKET_LLM_API").ok().as_deref() {
            Some("anthropic") => ProviderApi::Anthropic,
            _ => ProviderApi::OpenAiCompat,
        };

        let generic_proxy = lookup("GASKET_LLM_PROXY").ok();
        let http_proxy = lookup("GASKET_LLM_HTTP_PROXY").ok();
        let https_proxy = lookup("GASKET_LLM_HTTPS_PROXY").ok();

        let client = build_client(&http_proxy, &https_proxy, &generic_proxy)?;

        Ok(Self {
            api,
            base_url,
            api_key,
            model,
            client,
        })
    }
}

/// Build a reqwest client applying proxy precedence:
/// http scheme -> `http_proxy` else `generic`; https scheme -> `https_proxy`
/// else `generic`.
fn build_client(
    http_proxy: &Option<String>,
    https_proxy: &Option<String>,
    generic_proxy: &Option<String>,
) -> Result<reqwest::Client, ConfigError> {
    let mut builder = reqwest::Client::builder();

    let http_url = http_proxy.as_ref().or(generic_proxy.as_ref());
    if let Some(url) = http_url {
        let proxy = reqwest::Proxy::http(url).map_err(|e| ConfigError::BadProxy {
            var: "GASKET_LLM_HTTP_PROXY/GASKET_LLM_PROXY",
            url: url.clone(),
            source: e,
        })?;
        builder = builder.proxy(proxy);
    }

    let https_url = https_proxy.as_ref().or(generic_proxy.as_ref());
    if let Some(url) = https_url {
        let proxy = reqwest::Proxy::https(url).map_err(|e| ConfigError::BadProxy {
            var: "GASKET_LLM_HTTPS_PROXY/GASKET_LLM_PROXY",
            url: url.clone(),
            source: e,
        })?;
        builder = builder.proxy(proxy);
    }

    builder.build().map_err(ConfigError::ClientBuild)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    /// A fake env for tests - maps var name -> value.
    fn fake_env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Result<String, std::env::VarError> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| map.get(k).cloned().ok_or(std::env::VarError::NotPresent)
    }

    #[test]
    fn parses_required_vars() {
        let cfg = ProviderConfig::from_env_with(&fake_env(&[
            ("GASKET_LLM_BASE_URL", "https://api.x.com/v1"),
            ("GASKET_LLM_KEY", "sk-test"),
            ("GASKET_LLM_MODEL", "gpt-4o-mini"),
        ]))
        .unwrap();
        assert_eq!(cfg.base_url, "https://api.x.com/v1");
        assert_eq!(cfg.api_key, "sk-test");
        assert_eq!(cfg.model, "gpt-4o-mini");
        assert_eq!(cfg.api, ProviderApi::OpenAiCompat); // default when GASKET_LLM_API unset
    }

    #[test]
    fn api_selects_anthropic() {
        let cfg = ProviderConfig::from_env_with(&fake_env(&[
            ("GASKET_LLM_BASE_URL", "https://api.anthropic.com/v1"),
            ("GASKET_LLM_KEY", "sk-test"),
            ("GASKET_LLM_MODEL", "claude-sonnet"),
            ("GASKET_LLM_API", "anthropic"),
        ]))
        .unwrap();
        assert_eq!(cfg.api, ProviderApi::Anthropic);
    }

    #[test]
    fn missing_required_var_errors() {
        let r = ProviderConfig::from_env_with(&fake_env(&[
            ("GASKET_LLM_KEY", "sk-test"),
            ("GASKET_LLM_MODEL", "m"),
        ]));
        assert!(matches!(
            r,
            Err(ConfigError::Missing("GASKET_LLM_BASE_URL"))
        ));
    }

    #[test]
    fn generic_proxy_applies_to_both_schemes() {
        // GASKET_LLM_PROXY alone -> client builds (http + https both proxied).
        let cfg = ProviderConfig::from_env_with(&fake_env(&[
            ("GASKET_LLM_BASE_URL", "https://api.x.com/v1"),
            ("GASKET_LLM_KEY", "k"),
            ("GASKET_LLM_MODEL", "m"),
            ("GASKET_LLM_PROXY", "http://localhost:8080"),
        ]))
        .unwrap();
        // No direct way to inspect proxies on a Client; building without error
        // and having a usable client is the contract.
        let _ = cfg.client;
    }

    #[test]
    fn https_proxy_overrides_generic_for_https() {
        // Both generic and https set - both accepted (https wins for https).
        let cfg = ProviderConfig::from_env_with(&fake_env(&[
            ("GASKET_LLM_BASE_URL", "https://api.x.com/v1"),
            ("GASKET_LLM_KEY", "k"),
            ("GASKET_LLM_MODEL", "m"),
            ("GASKET_LLM_PROXY", "http://generic:8080"),
            ("GASKET_LLM_HTTPS_PROXY", "http://specific:9090"),
        ]))
        .unwrap();
        let _ = cfg.client;
    }

    #[test]
    fn bad_proxy_url_errors() {
        let r = ProviderConfig::from_env_with(&fake_env(&[
            ("GASKET_LLM_BASE_URL", "https://api.x.com/v1"),
            ("GASKET_LLM_KEY", "k"),
            ("GASKET_LLM_MODEL", "m"),
            ("GASKET_LLM_PROXY", "not a url %"),
        ]));
        assert!(matches!(r, Err(ConfigError::BadProxy { .. })));
    }

    // Silence unused-import warning for the OnceLock/Mutex shim if not needed.
    #[allow(dead_code)]
    fn _unused() {
        let _ = OnceLock::<()>::new();
        let _ = Mutex::new(());
    }
}
