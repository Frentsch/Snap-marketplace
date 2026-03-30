/// BCS-deserializable Rust mirrors of the on-chain Move structs.
///
/// Move objects are stored as raw BCS bytes. The layout matches the Move struct
/// field order exactly:
///   - `UID`     → 32 bytes  (UID { id: ID { bytes: address } }, all transparent in BCS)
///   - `ID`      → 32 bytes  (same)
///   - `address` → 32 bytes
///   - `String`  → ULEB128 length + UTF-8 bytes
///   - `u64`     → 8 bytes little-endian
///   - `bool`    → 1 byte
///   - nested struct → fields serialised inline (no tag)
use serde::Deserialize;
use sui_sdk::TypeTag;

/// A 32-byte SUI address/object-ID as used in BCS.
/// Covers Move `address`, `ID { bytes: address }`, and `UID { id: ID }`.
pub type RawId = [u8; 32];

// ─────────────────────────────────────────────────────────────────────────────
// On-chain Move objects
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Deserialize, Debug, Clone)]
pub struct AccessToken {
    pub id:            RawId,   // UID → opaque ID bytes
    pub listing_id:    RawId,
    pub service_name:  String,
    pub ip_address:    String,
    pub valid_from_ms: u64,
    pub expires_at_ms: u64,
    pub bandwidth_bps: u64,
    pub issuer:        RawId,
}

#[derive(Deserialize, Debug, Clone)]
pub struct ServiceListing {
    pub id:                RawId,
    pub issuer:            RawId,
    pub name:              String,
    pub ip_address:        String,
    pub price_mist:        u64,
    pub is_active:         bool,
    pub min_bandwidth_bps: u64,
    pub min_duration_ms:   u64,
    pub bw_granularity:    u64,
    pub time_granularity:  u64,
    pub token:             AccessToken, // wrapped inline
}

/// `ObjectBag` layout — only `id` is needed to query its dynamic fields.
#[derive(Deserialize, Debug)]
pub struct ObjectBagRef {
    pub id:   RawId,
    pub size: u64,
}

/// `Marketplace<phantom COIN>` layout.
#[derive(Deserialize, Debug)]
pub struct MarketplaceObject {
    pub id:       RawId,
    pub cointype: TypeTag
    pub listings: ObjectBagRef,
}

// ─────────────────────────────────────────────────────────────────────────────
// Events
// ─────────────────────────────────────────────────────────────────────────────

/// BCS layout of the `TokenRedeemed` Move event, emitted by `redeem()`.
#[derive(Deserialize, Debug)]
pub struct TokenRedeemed {
    pub token_id:      RawId,
    pub issuer:        RawId,
    pub redeemed_by:   RawId,
    pub ip_address:    String,
    pub valid_from_ms: u64,
    pub expires_at_ms: u64,
    pub bandwidth_bps: u64,
}
