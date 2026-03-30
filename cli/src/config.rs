use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::PathBuf;
use sui_sdk_types::Address;

// ─────────────────────────────────────────────────────────────────────────────
// Config structs
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Deserialize, Debug, Clone)]
pub struct SuiConfig {
    pub network:    String,
    pub rpc_url:    String,
    pub gas_budget: u64,
    /// Path to the directory containing `sui.keystore` (e.g. `~/.sui/sui_config`).
    pub keystore:   PathBuf,
    /// Signing account address.
    pub address:    Address,
}

#[derive(Deserialize, Debug, Clone)]
pub struct MarketplaceConfig {
    pub package_id:     Address,
    pub marketplace_id: Address,
    pub coin_type:      String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct MarketConfig {
    #[serde(rename = "Sui")]
    pub sui: SuiConfig,
    #[serde(rename = "Marketplace")]
    pub marketplace: MarketplaceConfig,
}

// ─────────────────────────────────────────────────────────────────────────────
// Loading
// ─────────────────────────────────────────────────────────────────────────────

/// Load `market-config.toml` from the current working directory.
pub fn load_config() -> Result<MarketConfig> {
    let path = PathBuf::from("market-config.toml");
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("Cannot read {}", path.display()))?;
    toml::from_str(&content).context("Cannot parse market-config.toml")
}
