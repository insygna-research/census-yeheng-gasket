//! CLI 命令模块

mod agent;
mod auth;
mod broker_outbound;
mod channels;
mod command_host;
mod cron;
mod dispatching_handler;
mod embedding;
mod onboard;
mod plugin_commands;
pub mod registry;
mod status;
mod tool;
pub mod vault;
mod wiki;

pub use agent::cmd_agent;
pub use auth::cmd_auth_copilot;
pub use channels::cmd_channels_status;
pub use cron::{
    cmd_cron_add, cmd_cron_disable, cmd_cron_enable, cmd_cron_list, cmd_cron_refresh,
    cmd_cron_remove, cmd_cron_show,
};
pub use embedding::{cmd_embedding_rebuild, cmd_embedding_stats};
pub use onboard::cmd_onboard;
pub use status::{cmd_auth_status, cmd_status};
pub use tool::cmd_tool_execute;
pub use vault::{
    cmd_vault_delete, cmd_vault_export, cmd_vault_get, cmd_vault_import, cmd_vault_list,
    cmd_vault_rekey, cmd_vault_set, cmd_vault_show,
};
pub use wiki::{
    cmd_wiki_delete, cmd_wiki_ingest, cmd_wiki_init, cmd_wiki_lint, cmd_wiki_list,
    cmd_wiki_migrate, cmd_wiki_search, cmd_wiki_stats,
};

/// Show session token usage and cost statistics
pub async fn cmd_stats() -> anyhow::Result<()> {
    println!("📊 Session Token Statistics");
    println!("───────────────────────────");

    println!("\nToken usage and cost statistics are automatically displayed:");
    println!("  • After each LLM response during agent interactions");
    println!("  • At the end of each conversation session");
    println!("\nTip: Configure pricing in ~/.gasket/config.toml to see cost estimates:\n");
    println!("  [providers.openai]");
    println!("  price_input_per_million = 2.5");
    println!("  price_output_per_million = 10.0");
    println!("  currency = \"USD\"");

    Ok(())
}
