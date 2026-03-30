/// Wallet loading — replaces the old WalletContext from sui-sdk.
///
/// Reads `~/.sui/sui_config/client.yaml` for the active address and RPC URL,
/// then `~/.sui/sui_config/sui.keystore` (JSON array of base64-encoded keypairs)
/// to find the matching private key.
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde::Deserialize;
use std::path::PathBuf;
use sui_crypto::{
    ed25519::Ed25519PrivateKey,
    secp256k1::Secp256k1PrivateKey,
    secp256r1::Secp256r1PrivateKey,
    simple::SimpleKeypair,
};
use sui_sdk_types::Address;

// ─────────────────────────────────────────────────────────────────────────────
// Config types
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ClientConfig {
    active_address: Option<String>,
    active_env:     String,
    envs:           Vec<SuiEnv>,
}

#[derive(Deserialize)]
struct SuiEnv {
    alias: String,
    rpc:   String,
}

// ─────────────────────────────────────────────────────────────────────────────
// Wallet
// ─────────────────────────────────────────────────────────────────────────────

/// Loaded wallet — the active address, its keypair, and the RPC endpoint.
pub struct Wallet {
    pub address: Address,
    pub keypair: SimpleKeypair,
    pub rpc_url: String,
}

impl Wallet {
    /// Matches the old `WalletContext::active_address()` signature used in bin files.
    pub fn active_address(&mut self) -> Result<Address> {
        Ok(self.address)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

fn sui_config_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("SUI_CONFIG_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let home = dirs::home_dir().context("Cannot determine home directory")?;
    Ok(home.join(".sui").join("sui_config"))
}

/// Decode one base64 keystore entry (`flag || 32-byte private key`) into a
/// `SimpleKeypair` and the corresponding Sui address.
///
/// Sui keystore flag bytes: 0x00 = Ed25519, 0x01 = Secp256k1, 0x02 = Secp256r1.
fn keypair_from_entry(entry: &str) -> Result<(SimpleKeypair, Address)> {
    let bytes = B64.decode(entry).context("Base64 decode failed")?;
    if bytes.len() < 33 {
        bail!("Keystore entry too short ({} bytes, expected ≥33)", bytes.len());
    }
    let flag    = bytes[0];
    let key_raw = &bytes[1..];

    // Ed25519PublicKey::derive_address() uses blake2b256(flag || pubkey_bytes).
    match flag {
        0x00 => {
            let raw: [u8; 32] = key_raw[..32].try_into().context("Ed25519 key != 32 bytes")?;
            let sk = Ed25519PrivateKey::new(raw);
            let addr = sk.public_key().derive_address();
            Ok((SimpleKeypair::from(sk), addr))
        }
        0x01 => {
            let raw: [u8; 32] = key_raw[..32].try_into().context("Secp256k1 key != 32 bytes")?;
            let sk = Secp256k1PrivateKey::new(raw)
                .context("Invalid Secp256k1 private key")?;
            let addr = sk.public_key().derive_address();
            Ok((SimpleKeypair::from(sk), addr))
        }
        0x02 => {
            let raw: [u8; 32] = key_raw[..32].try_into().context("Secp256r1 key != 32 bytes")?;
            let sk = Secp256r1PrivateKey::new(raw);
            let addr = sk.public_key().derive_address();
            Ok((SimpleKeypair::from(sk), addr))
        }
        _ => bail!("Unsupported signature scheme flag: 0x{flag:02x}"),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Public API
// ─────────────────────────────────────────────────────────────────────────────

/// Load the active wallet from the Sui CLI configuration files.
pub fn load_wallet() -> Result<Wallet> {
    let cfg_dir = sui_config_dir()?;

    // 1. Parse client.yaml
    let yaml = std::fs::read(cfg_dir.join("client.yaml"))
        .context("Cannot read ~/.sui/sui_config/client.yaml")?;
    let config: ClientConfig =
        serde_yaml::from_slice(&yaml).context("Cannot parse client.yaml")?;

    let active_str = config
        .active_address
        .context("No active_address in client.yaml")?;
    let active_address: Address = active_str.parse().context("Cannot parse active_address")?;

    let rpc_url = config
        .envs
        .iter()
        .find(|e| e.alias == config.active_env)
        .with_context(|| format!("Active env '{}' not found in client.yaml", config.active_env))?
        .rpc
        .clone();

    // 2. Parse sui.keystore (JSON array of base64 strings)
    let ks_bytes = std::fs::read(cfg_dir.join("sui.keystore"))
        .context("Cannot read ~/.sui/sui_config/sui.keystore")?;
    let entries: Vec<String> =
        serde_json::from_slice(&ks_bytes).context("Cannot parse sui.keystore")?;

    // 3. Find the keypair whose derived address matches active_address
    for entry in &entries {
        match keypair_from_entry(entry) {
            Ok((keypair, addr)) if addr == active_address => {
                return Ok(Wallet { address: active_address, keypair, rpc_url });
            }
            Ok(_) => {}
            Err(e) => eprintln!("Warning: skipping keystore entry: {e}"),
        }
    }

    bail!("No keypair found in keystore for active address {active_str}")
}

/// Async wrapper kept for compatibility with the bin files that call
/// `utils::get_wallet().await?`.
pub async fn get_wallet() -> Result<Wallet> {
    load_wallet()
}

/// Return any address in the keystore that is not the active address.
/// Used for local two-account testing.
pub fn get_second_address() -> Result<Address> {
    let active = load_wallet()?.address;
    let cfg_dir = sui_config_dir()?;
    let ks_bytes = std::fs::read(cfg_dir.join("sui.keystore"))
        .context("Cannot read sui.keystore")?;
    let entries: Vec<String> = serde_json::from_slice(&ks_bytes)?;

    for entry in &entries {
        if let Ok((_, addr)) = keypair_from_entry(entry) {
            if addr != active {
                return Ok(addr);
            }
        }
    }
    bail!("No second address found in keystore")
}
