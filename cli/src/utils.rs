use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use sui_crypto::{
    ed25519::Ed25519PrivateKey,
    secp256k1::Secp256k1PrivateKey,
    secp256r1::Secp256r1PrivateKey,
    simple::SimpleKeypair,
};
use sui_sdk_types::Address;

use crate::config::MarketConfig;

// ─────────────────────────────────────────────────────────────────────────────
// Wallet
// ─────────────────────────────────────────────────────────────────────────────

/// Loaded wallet — the active address and its keypair.
/// RPC URL and other network settings come from `MarketConfig`.
pub struct Wallet {
    pub address: Address,
    pub keypair: SimpleKeypair,
}

impl Wallet {
    pub fn active_address(&self) -> Address {
        self.address
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Keystore helpers
// ─────────────────────────────────────────────────────────────────────────────

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

/// Load the wallet for the address specified in `market-config.toml`.
/// The keystore file is read from `[Sui] keystore`.
pub fn load_wallet(cfg: &MarketConfig) -> Result<Wallet> {
    let address = cfg.sui.address;

    let ks_path = cfg.sui.keystore.join("sui.keystore");
    let ks_bytes = std::fs::read(&ks_path)
        .with_context(|| format!("Cannot read {}", ks_path.display()))?;
    let entries: Vec<String> =
        serde_json::from_slice(&ks_bytes).context("Cannot parse sui.keystore")?;

    for entry in &entries {
        match keypair_from_entry(entry) {
            Ok((keypair, addr)) if addr == address => {
                return Ok(Wallet { address, keypair });
            }
            Ok(_) => {}
            Err(e) => eprintln!("Warning: skipping keystore entry: {e}"),
        }
    }

    bail!("No keypair found in keystore for address {}", cfg.sui.address)
}
