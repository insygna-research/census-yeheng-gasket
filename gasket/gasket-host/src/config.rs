//! ConfigLoader: 从 env/.env 聚合 ProviderConfig + AgentTunables。
use gasket_core::{AgentTunables, ProviderConfig};

#[derive(Debug, Clone)]
pub struct HostConfig {
    pub provider: ProviderConfig,
    pub tunables: AgentTunables,
}

pub struct ConfigLoader;

impl ConfigLoader {
    pub fn load() -> Result<HostConfig, crate::HostError> {
        let _ = dotenvy::dotenv(); // best-effort；env 已有的值优先
        Self::load_with(&|k: &str| std::env::var(k))
    }

    pub fn load_with(
        lookup: &dyn Fn(&str) -> Result<String, std::env::VarError>,
    ) -> Result<HostConfig, crate::HostError> {
        let provider = ProviderConfig::from_env_with(lookup)?;
        let tunables = AgentTunables::from_env_with(lookup);
        Ok(HostConfig { provider, tunables })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn fake_env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Result<String, std::env::VarError> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| map.get(k).cloned().ok_or(std::env::VarError::NotPresent)
    }

    #[test]
    fn loads_provider_and_tunables() {
        let cfg = ConfigLoader::load_with(&fake_env(&[
            ("GASKET_LLM_BASE_URL", "https://api.x.com/v1"),
            ("GASKET_LLM_KEY", "sk-test"),
            ("GASKET_LLM_MODEL", "m"),
            ("GASKET_LLM_API", "anthropic"),
            ("GASKET_MAX_TURNS", "7"),
        ]))
        .unwrap();
        assert_eq!(cfg.provider.model, "m");
        assert_eq!(cfg.provider.api, gasket_core::ProviderApi::Anthropic);
        assert_eq!(cfg.tunables.max_turns, 7);
    }

    #[test]
    fn missing_llm_config_errors() {
        let r = ConfigLoader::load_with(&fake_env(&[]));
        assert!(matches!(r, Err(crate::HostError::Config(_))));
    }
}
