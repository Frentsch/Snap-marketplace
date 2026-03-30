/// BCS-deserializable Rust mirrors of the on-chain Move structs.
/// Time fields are in seconds; bandwidth fields are in kB/s.
use serde::Deserialize;

pub type RawId = [u8; 32];

#[derive(Deserialize, Debug, Clone)]
pub struct AccessToken {
    pub id:          RawId,
    pub listing_id:  RawId,
    pub service_name: String,
    pub ip_address:  String,
    pub valid_from:  u64,
    pub expires_at:  u64,
    pub bandwidth:   u64,
    pub issuer:      RawId,
}

#[derive(Deserialize, Debug, Clone)]
pub struct ServiceListing {
    pub id:               RawId,
    pub issuer:           RawId,
    pub name:             String,
    pub ip_address:       String,
    pub price_mist:       u64,
    pub is_active:        bool,
    pub min_bandwidth:    u64,
    pub min_duration:     u64,
    pub bw_granularity:   u64,
    pub time_granularity: u64,
    pub token:            AccessToken,
}

#[derive(Deserialize, Debug)]
pub struct ObjectBagRef {
    pub id:   RawId,
    pub size: u64,
}

#[derive(Deserialize, Debug)]
pub struct MarketplaceObject {
    pub id:       RawId,
    pub listings: ObjectBagRef,
}

#[derive(Deserialize, Debug)]
pub struct TokenRedeemed {
    pub token_id:    RawId,
    pub issuer:      RawId,
    pub redeemed_by: RawId,
    pub ip_address:  String,
    pub valid_from:  u64,
    pub expires_at:  u64,
    pub bandwidth:   u64,
}
