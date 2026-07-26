//! ConfigLoader: 从 env/.env 聚合 ProviderConfig + AgentTunables。
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

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

impl HostConfig {
    /// Assemble the provider `stream_fn` + `ModelSpec` + tunables into an
    /// `AgentLoopConfig`. Every host (CLI, smoke tests, future channels) wires
    /// the loop config identically; centralizing it here stops the assembly
    /// from being copy-pasted at each call site.
    ///
    /// `max_turns` is caller-supplied (smoke tests cap it at 3; the REPL uses
    /// the tunables default). `signal` and `hooks` are host-specific.
    pub fn build_loop_config(
        &self,
        max_turns: usize,
        signal: Option<Arc<AtomicBool>>,
        hooks: Option<Arc<dyn gasket_core::HookChain>>,
    ) -> gasket_core::AgentLoopConfig {
        let stream_fn: Arc<dyn gasket_core::StreamFn> = match self.provider.api {
            gasket_core::ProviderApi::OpenAiCompat => {
                Arc::new(gasket_core::OpenAiCompat::from_config(&self.provider))
            }
            gasket_core::ProviderApi::Anthropic => {
                Arc::new(gasket_core::AnthropicProvider::from_config(&self.provider))
            }
        };
        gasket_core::AgentLoopConfig {
            model: gasket_core::ModelSpec {
                id: self.provider.model.clone(),
                api: self.provider.api,
                max_tokens: self.tunables.max_tokens,
                supports_thinking: self.tunables.thinking_level != gasket_core::ThinkingLevel::Off,
            },
            thinking_level: self.tunables.thinking_level,
            max_turns,
            max_tool_calls_per_turn: self.tunables.max_tool_calls_per_turn,
            signal,
            stream_fn,
            hooks,
            retry: self.tunables.retry.clone(),
        }
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

    #[test]
    fn build_loop_config_wires_provider_and_tunables() {
        let cfg = ConfigLoader::load_with(&fake_env(&[
            ("GASKET_LLM_BASE_URL", "https://api.x.com/v1"),
            ("GASKET_LLM_KEY", "sk-test"),
            ("GASKET_LLM_MODEL", "m"),
            ("GASKET_LLM_API", "anthropic"),
            ("GASKET_MAX_TURNS", "7"),
        ]))
        .unwrap();
        // max_turns is the caller's arg (3), NOT the tunables value (7).
        let lc = cfg.build_loop_config(3, None, None);
        assert_eq!(lc.model.id, "m");
        assert_eq!(lc.model.api, gasket_core::ProviderApi::Anthropic);
        assert_eq!(lc.max_turns, 3);
        assert_eq!(lc.max_tool_calls_per_turn, cfg.tunables.max_tool_calls_per_turn);
        assert!(lc.signal.is_none());
        assert!(lc.hooks.is_none());
    }
}
