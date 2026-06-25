//! Provider 构建和查找逻辑

use std::sync::Arc;

use anyhow::Result;
use gasket_engine::config::{Config, ProviderType};
use gasket_engine::providers::{LlmProvider, ModelSpec};
use gasket_engine::vault::{contains_placeholders, VaultStore};

use crate::commands::vault::ensure_unlocked_non_interactive;

/// Check config for vault placeholders and unlock the vault if needed.
///
/// Returns `Some(Arc<VaultStore>)` if the config contains vault placeholders
/// and the vault was successfully unlocked, or `None` if no placeholders
/// were found (no vault needed).
///
/// This function uses non-interactive mode - it only reads from environment
/// variables and does not prompt for password input.
pub fn setup_vault(config: &Config) -> anyhow::Result<Option<std::sync::Arc<VaultStore>>> {
    // Serialize config to JSON and scan for placeholders
    let config_str = serde_json::to_string(config).unwrap_or_default();
    if !contains_placeholders(&config_str) {
        return Ok(None);
    }

    tracing::info!("[Vault] Detected vault placeholders in config — unlocking vault");
    let mut store = VaultStore::new()?;
    ensure_unlocked_non_interactive(&mut store)?;
    Ok(Some(std::sync::Arc::new(store)))
}

/// Provider information returned by find_provider
pub struct ProviderInfo {
    /// The provider instance
    pub provider: Arc<dyn LlmProvider>,
    /// The model name to use
    pub model: String,
    /// Provider name (e.g., "zhipu", "deepseek")
    pub provider_name: String,
    /// Whether this provider supports thinking/reasoning mode for the selected model
    pub supports_thinking: bool,
    /// Pricing configuration (if configured)
    pub pricing: Option<(f64, f64, String)>, // (input_price, output_price, currency)
}

/// Build a provider instance from its name and config.
///
/// Delegates to `gasket_engine::providers::build_provider` after validating
/// that `api_base` is configured.
pub fn build_provider(
    name: &str,
    api_key: &str,
    provider_config: &gasket_engine::config::ProviderConfig,
    model: &str,
) -> Result<Arc<dyn LlmProvider>> {
    if provider_config.api_base.is_empty() {
        anyhow::bail!(
            "Provider '{}' is missing required 'api_base' configuration. \
             Please add 'api_base' to your provider config in ~/.gasket/config.yaml",
            name
        );
    }
    gasket_engine::providers::build_provider(name, api_key, provider_config, model)
}

/// Infer `ProviderType` from a provider name (used when no config entry exists).
fn infer_provider_type(name: &str) -> ProviderType {
    match name {
        "minimax" | "minimaxi" => ProviderType::Minimax,
        "gemini" => ProviderType::Gemini,
        "moonshot" | "kimi" => ProviderType::Moonshot,
        "anthropic" | "claude" => ProviderType::Anthropic,
        _ => ProviderType::Openai,
    }
}

/// Default API base URL for known providers (used when no config entry exists).
fn get_default_api_base_for_provider(name: &str) -> &'static str {
    match name {
        "minimax" | "minimaxi" => "https://api.minimaxi.com/v1",
        "moonshot" | "kimi" => "https://api.moonshot.cn/v1",
        "anthropic" | "claude" => "https://api.anthropic.com/v1",
        "gemini" => "https://generativelanguage.googleapis.com/v1beta",
        "copilot" => "https://api.githubcopilot.com",
        _ => "",
    }
}

/// Find a provider config entry, trying aliases when the exact name is missing.
///
/// Returns the actual config key and the config. For example, if the user
/// requests `minimax` but only `minimaxi` is configured, this returns
/// `("minimaxi", &config)`.
fn find_provider_config_entry<'a>(
    config: &'a Config,
    name: &'a str,
) -> Option<(&'a str, &'a gasket_engine::config::ProviderConfig)> {
    // Exact match first
    if let Some(cfg) = config.providers.get(name) {
        return Some((name, cfg));
    }

    // Alias fallback
    let aliases: &[&str] = match name {
        "minimax" => &["minimaxi"],
        "kimi" => &["moonshot"],
        "moonshot" => &["kimi"],
        "claude" => &["anthropic"],
        "anthropic" => &["claude"],
        _ => &[],
    };

    for alias in aliases {
        if let Some(cfg) = config.providers.get(*alias) {
            return Some((alias, cfg));
        }
    }

    None
}

/// Get the default model name for a provider.
pub fn get_default_model_for_provider(name: &str) -> &'static str {
    match name {
        "deepseek" => "deepseek-chat",
        "openrouter" => "anthropic/claude-4.5-sonnet",
        "anthropic" => "claude-4-6-sonnet",
        "zhipu" => "glm-5",
        "dashscope" => "Qwen/Qwen3.5-397B-A17B",
        "moonshot" | "kimi" => "kimi-k2.5",
        "minimax" => "MiniMax-M2.5",
        "ollama" => "llama3",
        "litellm" => "gpt-4o", // LiteLLM proxies to configured models
        "copilot" => "gpt-4o",
        "gemini" => "gemini-2.0-flash",
        _ => "gpt-4o",
    }
}

/// Default provider search order when no explicit provider is specified.
const DEFAULT_PROVIDER_ORDER: &[&str] = &[
    "openrouter",
    "deepseek",
    "openai",
    "anthropic",
    "minimax",
    "kimi",
    "moonshot",
    "litellm",
    "ollama",
];

/// Find an available provider from the configuration.
///
/// Searches in order of preference defined by `DEFAULT_PROVIDER_ORDER`,
/// then falls back to any other configured provider.
fn find_default_provider(config: &Config) -> Option<String> {
    // First, try providers in the preferred order
    for &name in DEFAULT_PROVIDER_ORDER {
        if let Some(cfg) = config.providers.get(name) {
            if cfg.is_available(name) {
                return Some(name.to_string());
            }
        }
    }

    // Fallback: scan all providers for any available one
    config
        .providers
        .iter()
        .find(|(_, cfg)| cfg.is_available(""))
        .map(|(name, _)| name.clone())
}

/// Find a configured provider.
///
/// The model field supports `provider_id/model_id` format (parsed via
/// `ModelSpec`) to select a specific provider. For example:
///   - `"deepseek/deepseek-chat"` → use the deepseek provider with model deepseek-chat
///   - `"zhipu/glm-4"`           → use the zhipu provider with model glm-4
///   - `"deepseek-chat"`          → legacy behaviour, use default provider
///
/// If `vault` is provided, `{{vault:key}}` placeholders in API keys are
/// resolved JIT through the vault store.
pub fn find_provider(config: &Config, vault: Option<&VaultStore>) -> Result<ProviderInfo> {
    let raw_model = config
        .agents
        .defaults
        .model
        .clone()
        .unwrap_or_else(|| "gpt-4o".to_string());

    // Parse once into a strongly-typed ModelSpec
    let spec: ModelSpec = raw_model
        .parse()
        .expect("ModelSpec::from_str is infallible");

    // Determine provider name: explicit in spec, or find default
    let (provider_name, provider_config) = match spec.provider() {
        Some(name) => {
            // Try exact match + aliases; if still missing, fall back to a default config
            if let Some((found_name, cfg)) = find_provider_config_entry(config, name) {
                if !cfg.is_available(found_name) {
                    anyhow::bail!(
                        "Provider '{}' is configured but missing API key",
                        found_name
                    );
                }
                (found_name.to_string(), cfg.clone())
            } else {
                // Provider not explicitly configured — build a minimal default config
                // so that build_provider can still route to the correct native
                // implementation.  api_base / api_key will be validated later.
                let api_base = get_default_api_base_for_provider(name);
                let default_config = gasket_engine::config::ProviderConfig {
                    provider_type: infer_provider_type(name),
                    api_base: api_base.to_string(),
                    ..Default::default()
                };
                (name.to_string(), default_config)
            }
        }
        None => {
            // No explicit provider — find an available default
            let name = find_default_provider(config).ok_or_else(|| {
                anyhow::anyhow!(
                    "No available provider configured. Run 'gasket onboard' and add your API key to ~/.gasket/config.yaml"
                )
            })?;
            let cfg = config
                .providers
                .get(&name)
                .expect("provider should exist after validation")
                .clone();
            (name, cfg)
        }
    };

    // Resolve API key — JIT vault resolution if needed
    let raw_api_key = provider_config.api_key.as_deref().unwrap_or("");
    let api_key = resolve_secret(raw_api_key, vault)?;

    // Resolve model name
    let default_model = get_default_model_for_provider(&provider_name);
    let model = if spec.model().is_empty() {
        default_model.to_string()
    } else {
        spec.model().to_string()
    };

    let provider = build_provider(&provider_name, &api_key, &provider_config, &model)?;

    // Check thinking support.
    // agents.defaults.thinking_enabled takes highest priority and overrides
    // any per-model or per-provider configuration.
    let supports_thinking = if config.agents.defaults.thinking_enabled {
        true
    } else {
        provider_config.thinking_enabled_for_model(&model) || provider.supports_thinking()
    };

    // Get pricing configuration if available (model-level overrides provider-level)
    let pricing = provider_config.get_pricing_for_model(&model).map(|p| {
        (
            p.price_input_per_million,
            p.price_output_per_million,
            p.currency,
        )
    });

    Ok(ProviderInfo {
        provider,
        model,
        provider_name,
        supports_thinking,
        pricing,
    })
}

/// Resolve a raw secret string through the vault (JIT).
///
/// - No placeholders → returns the string as-is
/// - Has placeholders & vault available → resolves via vault
/// - Has placeholders & no vault → falls back to env var, then error
pub fn resolve_secret(raw: &str, vault: Option<&VaultStore>) -> Result<String> {
    if !contains_placeholders(raw) {
        return Ok(raw.to_string());
    }

    // Try vault first
    if let Some(v) = vault {
        return v
            .resolve_text(raw)
            .map_err(|e| anyhow::anyhow!("Vault error: {}", e));
    }

    // Fall back to environment variables
    let placeholders = gasket_engine::vault::scan_placeholders(raw);
    let mut replacements = std::collections::HashMap::new();
    for p in &placeholders {
        let env_key = p.key.to_uppercase();
        if let Ok(value) = std::env::var(&env_key) {
            replacements.insert(p.key.clone(), value);
        } else {
            anyhow::bail!(
                "Cannot resolve '{{{{vault:{}}}}}' — no vault available and env var {} not set. \
                 Set GASKET_VAULT_PASSWORD to unlock the vault.",
                p.key,
                env_key
            );
        }
    }

    Ok(gasket_engine::vault::replace_placeholders(
        raw,
        &replacements,
    ))
}
