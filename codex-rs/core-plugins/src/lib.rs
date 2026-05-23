pub mod installed_marketplaces;
pub mod loader;
mod manager;
pub mod manifest;
pub mod marketplace;
pub mod marketplace_add;
pub mod marketplace_remove;
pub mod marketplace_upgrade;
pub mod remote;
pub mod remote_bundle;
pub mod remote_legacy;
pub(crate) mod startup_remote_sync;
pub mod startup_sync;
pub mod store;
#[cfg(test)]
mod test_support;
pub mod toggles;

pub const OPENAI_CURATED_MARKETPLACE_NAME: &str = "openai-curated";
pub const OPENAI_BUNDLED_MARKETPLACE_NAME: &str = "openai-bundled";

pub const TOOL_SUGGEST_DISCOVERABLE_PLUGIN_ALLOWLIST: &[&str] = &[
    "github@openai-curated",
    "notion@openai-curated",
    "slack@openai-curated",
    "gmail@openai-curated",
    "google-calendar@openai-curated",
    "google-drive@openai-curated",
    "openai-developers@openai-curated",
    "canva@openai-curated",
    "teams@openai-curated",
    "sharepoint@openai-curated",
    "outlook-email@openai-curated",
    "outlook-calendar@openai-curated",
    "linear@openai-curated",
    "figma@openai-curated",
    "browser-use@openai-bundled",
    "chrome@openai-bundled",
    "computer-use@openai-bundled",
];

pub type LoadedPlugin = codex_plugin::LoadedPlugin<codex_config::McpServerConfig>;
pub type PluginLoadOutcome = codex_plugin::PluginLoadOutcome<codex_config::McpServerConfig>;

pub use manager::ConfiguredMarketplace;
pub use manager::ConfiguredMarketplaceListOutcome;
pub use manager::ConfiguredMarketplacePlugin;
pub use manager::PluginDetail;
pub use manager::PluginDetailsUnavailableReason;
pub use manager::PluginInstallError;
pub use manager::PluginInstallOutcome;
pub use manager::PluginInstallRequest;
pub use manager::PluginReadOutcome;
pub use manager::PluginReadRequest;
pub use manager::PluginRemoteSyncError;
pub use manager::PluginUninstallError;
pub use manager::PluginsConfigInput;
pub use manager::PluginsManager;
pub use manager::RemotePluginSyncResult;
pub use marketplace_upgrade::ConfiguredMarketplaceUpgradeError as PluginMarketplaceUpgradeError;
pub use marketplace_upgrade::ConfiguredMarketplaceUpgradeOutcome as PluginMarketplaceUpgradeOutcome;

#[cfg(test)]
mod tests {
    use super::TOOL_SUGGEST_DISCOVERABLE_PLUGIN_ALLOWLIST;

    #[test]
    fn bundled_browser_and_computer_use_plugins_are_tool_suggest_discoverable() {
        for plugin_id in [
            "browser-use@openai-bundled",
            "chrome@openai-bundled",
            "computer-use@openai-bundled",
        ] {
            assert!(
                TOOL_SUGGEST_DISCOVERABLE_PLUGIN_ALLOWLIST.contains(&plugin_id),
                "{plugin_id} should be tool-suggest discoverable"
            );
        }
    }
}
