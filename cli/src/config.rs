use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::PathBuf;

// ─────────────────────────────────────────────────────────────────────────────
// Config structs
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Deserialize, Debug, Clone)]
pub struct SuiConfig {
    pub network:    String,
    pub rpc_url:    String,
    pub gas_budget: u64,
    /// Path to the directory containing `sui.keystore` (e.g. `~/.sui/sui_config`).
    pub keystore:   String,
    /// Hex address of the signing account (e.g. `0xf714...`).
    pub address:    String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct MarketplaceConfig {
    pub package_id:     String,
    pub marketplace_id: String,
    pub coin_type:      String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct MarketConfig {
    #[serde(rename = "Sui")]
    pub sui: SuiConfig,
    #[serde(rename = "Marketplace")]
    pub marketplace: MarketplaceConfig,
}

impl MarketConfig {
    pub fn keystore_dir(&self) -> PathBuf {
        PathBuf::from(&self.sui.keystore)
    }
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
