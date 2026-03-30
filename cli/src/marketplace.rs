use anyhow::{Context, Result};
use futures::StreamExt;
use serde::de::DeserializeOwned;
use std::time::Duration;
use sui_crypto::SuiSigner;
use sui_rpc::{
    Client,
    field::{FieldMask, FieldMaskUtil},
    proto::sui::rpc::v2::{
        changed_object, ExecuteTransactionRequest, GetObjectRequest, ListDynamicFieldsRequest,
        TransactionEffects,
    },
};
use sui_sdk_types::{Address, Identifier, TypeTag};
use sui_transaction_builder::{Function, ObjectInput, TransactionBuilder};

use crate::config::{load_config, MarketConfig};
use crate::models::{AccessToken, MarketplaceObject, ServiceListing};
use crate::utils::Wallet;

// ─────────────────────────────────────────────────────────────────────────────
// Config accessors
// ─────────────────────────────────────────────────────────────────────────────

fn coin_type_tag(cfg: &MarketConfig) -> Result<TypeTag> {
    cfg.marketplace.coin_type.parse().context("Invalid coin_type in config")
}

// ─────────────────────────────────────────────────────────────────────────────
// Client construction
// ─────────────────────────────────────────────────────────────────────────────

fn build_client(cfg: &MarketConfig) -> Result<Client> {
    let url = cfg.sui.rpc_url.as_str();
    Client::new(url).map_err(|e| anyhow::anyhow!("Cannot connect to {url}: {e}"))
}

// ─────────────────────────────────────────────────────────────────────────────
// Load both config and wallet in one call
// ─────────────────────────────────────────────────────────────────────────────

fn load_ctx() -> Result<(MarketConfig, Wallet)> {
    let cfg = load_config()?;
    let wallet = crate::utils::load_wallet(&cfg)?;
    Ok((cfg, wallet))
}

// ─────────────────────────────────────────────────────────────────────────────
// Object fetch helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Fetch a Move object by ID and BCS-deserialise its content as `T`.
/// Takes `&mut Client` because `ledger_client()` requires mutable access.
async fn get_object_bcs<T: DeserializeOwned>(client: &mut Client, id: Address) -> Result<T> {
    let resp = client
        .ledger_client()
        .get_object(
            GetObjectRequest::default()
                .with_object_id(id.to_string())
                .with_read_mask(FieldMask::from_str("contents")),
        )
        .await
        .context("get_object RPC failed")?
        .into_inner();
    let obj = resp.object_opt().context("Object not found")?;
    let bytes = obj
        .contents()
        .value_opt()
        .context("No BCS contents in object")?;
    bcs::from_bytes(bytes).context("BCS deserialisation failed")
}

/// Return the ObjectBag ID that stores all listings.
async fn get_bag_id(client: &mut Client, cfg: &MarketConfig) -> Result<Address> {
    let mp: MarketplaceObject = get_object_bcs(client, cfg.marketplace.marketplace_id).await?;
    Ok(Address::new(mp.listings.id))
}

/// List the ObjectIDs of every ServiceListing in the marketplace bag.
///
/// ObjectBag stores each listing as an object-type dynamic field;
/// `DynamicField.child_id` gives the stored object's ID directly.
async fn list_listing_ids(client: &Client, bag_id: Address) -> Result<Vec<Address>> {
    let stream = client.list_dynamic_fields(
        ListDynamicFieldsRequest::default()
            .with_parent(bag_id.to_string())
            .with_read_mask(FieldMask::from_str("child_id")),
    );
    tokio::pin!(stream);

    let mut ids = Vec::new();
    while let Some(item) = stream.next().await {
        let field = item.map_err(|e| anyhow::anyhow!("Dynamic field stream error: {e}"))?;
        if let Some(id_str) = field.child_id_opt() {
            ids.push(id_str.parse::<Address>()
                .with_context(|| format!("Cannot parse child_id '{id_str}'"))?);
        }
    }
    Ok(ids)
}

// ─────────────────────────────────────────────────────────────────────────────
// Transaction helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Sign and execute a finished TransactionBuilder, returning the effects.
async fn execute(
    client: &mut Client,
    wallet: &Wallet,
    builder: TransactionBuilder,
) -> Result<TransactionEffects> {
    let tx = builder
        .build(client)
        .await
        .map_err(|e| anyhow::anyhow!("Transaction build failed: {e:?}"))?;

    let sig = wallet
        .keypair
        .sign_transaction(&tx)
        .map_err(|e| anyhow::anyhow!("Signing failed: {e}"))?;

    let resp = client
        .execute_transaction_and_wait_for_checkpoint(
            ExecuteTransactionRequest::new(tx.into())
                .with_signatures(vec![sig.into()]),
            Duration::from_secs(60),
        )
        .await
        .map_err(|e| anyhow::anyhow!("Transaction execution failed: {e:?}"))?
        .into_inner();

    let executed = resp.transaction.context("No transaction in response")?;
    let effects = executed.effects.context("No effects in response")?;

    // Surface Move abort codes as readable errors
    if let Some(status) = &effects.status {
        if !status.success() {
            let msg = status.error.as_ref()
                .map(|e| format!("{e:?}"))
                .unwrap_or_else(|| "unknown error".to_string());
            anyhow::bail!("Transaction failed on-chain: {msg}");
        }
    }

    Ok(effects)
}

/// Find the ID of a newly created on-chain object by its Move type suffix.
fn find_created(effects: &TransactionEffects, type_suffix: &str) -> Result<Address> {
    effects
        .changed_objects
        .iter()
        .find(|co| {
            co.object_type_opt()
                .map_or(false, |t: &str| t.ends_with(type_suffix))
                && co.id_operation
                    .and_then(|v| changed_object::IdOperation::try_from(v).ok())
                    == Some(changed_object::IdOperation::Created)
        })
        .and_then(|co| co.object_id_opt())
        .context("Object not found in transaction effects")
        .and_then(|s| s.parse::<Address>().context("Cannot parse object ID"))
}

/// Find the AccessToken produced by a `purchase` transaction.
/// The token is extracted from a wrapped state so it may not appear as Created.
fn find_access_token(effects: &TransactionEffects) -> Result<Address> {
    effects
        .changed_objects
        .iter()
        .find(|co| {
            co.object_type_opt()
                .map_or(false, |t: &str| t.ends_with("::marketplace::AccessToken"))
        })
        .and_then(|co| co.object_id_opt())
        .context("AccessToken not found in transaction effects")
        .and_then(|s| s.parse::<Address>().context("Cannot parse AccessToken ID"))
}

// ─────────────────────────────────────────────────────────────────────────────
// create_marketplace
// ─────────────────────────────────────────────────────────────────────────────

pub async fn create_marketplace() -> Result<Address> {
    let (cfg, wallet) = load_ctx()?;
    let mut client = build_client(&cfg)?;

    let mut builder = TransactionBuilder::new();
    builder.set_sender(wallet.address);
    builder.set_gas_budget(cfg.sui.gas_budget);

    builder.move_call(
        Function::new(
            cfg.marketplace.package_id,
            Identifier::new("marketplace").unwrap(),
            Identifier::new("create_marketplace").unwrap(),
        )
        .with_type_args(vec![coin_type_tag(&cfg)?]),
        vec![],
    );

    let effects = execute(&mut client, &wallet, builder).await?;
    let mp_id = find_created(&effects, "::marketplace::Marketplace")?;

    println!("Marketplace created!");
    println!("  Coin type:      {}", cfg.marketplace.coin_type);
    println!("  Marketplace ID: {mp_id}");
    println!("  Set marketplace_id = \"{mp_id}\" in market-config.toml");
    Ok(mp_id)
}

// ─────────────────────────────────────────────────────────────────────────────
// create_listing
// ─────────────────────────────────────────────────────────────────────────────

pub async fn create_listing(
    name: String,
    ip_address: String,
    price_sui: f64,
    valid_from: u64,
    expires_at: u64,
    max_bandwidth: u64,
    min_bandwidth: u64,
    min_duration: u64,
    bw_granularity: u64,
    time_granularity: u64,
) -> Result<Address> {
    let (cfg, wallet) = load_ctx()?;
    let mut client = build_client(&cfg)?;
    let price_mist = (price_sui * 1_000_000_000.0) as u64;

    let mut builder = TransactionBuilder::new();
    builder.set_sender(wallet.address);
    builder.set_gas_budget(cfg.sui.gas_budget);

    let mp  = builder.object(ObjectInput::new(cfg.marketplace.marketplace_id));
    let a0  = builder.pure(&name.into_bytes());
    let a1  = builder.pure(&ip_address.into_bytes());
    let a2  = builder.pure(&price_mist);
    let a3  = builder.pure(&valid_from);
    let a4  = builder.pure(&expires_at);
    let a5  = builder.pure(&max_bandwidth);
    let a6  = builder.pure(&min_bandwidth);
    let a7  = builder.pure(&min_duration);
    let a8  = builder.pure(&bw_granularity);
    let a9  = builder.pure(&time_granularity);

    builder.move_call(
        Function::new(
            cfg.marketplace.package_id,
            Identifier::new("marketplace").unwrap(),
            Identifier::new("create_listing").unwrap(),
        )
        .with_type_args(vec![coin_type_tag(&cfg)?]),
        vec![mp, a0, a1, a2, a3, a4, a5, a6, a7, a8, a9],
    );

    let effects = execute(&mut client, &wallet, builder).await?;
    let listing_id = find_created(&effects, "::marketplace::ServiceListing")?;

    println!("Listing created!");
    println!("  Listing:        {listing_id}");
    println!("  Price:          {price_sui} SUI");
    println!("  Valid from:     {} s", valid_from);
    println!("  Expires at:     {} s", expires_at);
    println!("  Max BW:         {} kB/s", max_bandwidth);
    println!("  Min BW:         {} kB/s", min_bandwidth);
    println!("  Min duration:   {} s", min_duration);
    println!("  BW granularity: {} kB/s", bw_granularity);
    println!("  Time gran.:     {} s", time_granularity);
    Ok(listing_id)
}

// ─────────────────────────────────────────────────────────────────────────────
// get_listings
// ─────────────────────────────────────────────────────────────────────────────

pub async fn get_listings(limit: u32) -> Result<()> {
    let (cfg, _wallet) = load_ctx()?;
    let mut client = build_client(&cfg)?;

    let bag_id = get_bag_id(&mut client, &cfg).await?;
    let mut ids = list_listing_ids(&client, bag_id).await?;
    ids.truncate(limit as usize);

    if ids.is_empty() {
        println!("No listings found.");
        return Ok(());
    }

    let mut listings: Vec<(Address, ServiceListing)> = Vec::new();
    for id in ids {
        match get_object_bcs::<ServiceListing>(&mut client, id).await {
            Ok(l) => listings.push((id, l)),
            Err(e) => eprintln!("Warning: cannot fetch listing {id}: {e}"),
        }
    }

    listings.sort_by_key(|(_, l)| l.price_mist);

    println!("{:<68}  {:>10}  {}", "Listing ID", "Price SUI", "Name");
    println!("{}", "─".repeat(100));
    for (id, l) in &listings {
        let price = l.price_mist as f64 / 1e9;
        let bw = if l.token.bandwidth == 0 {
            "unlimited".to_string()
        } else {
            format!("{} kB/s", l.token.bandwidth)
        };
        println!(
            "{id:<68}  {price:>10.4}  {}  (expires: {} s, bw: {bw})",
            l.name, l.token.expires_at
        );
    }
    println!("\n{} listing(s) shown.", listings.len());
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// search_listings
// ─────────────────────────────────────────────────────────────────────────────

pub async fn search_listings(
    subnet: &str,
    min_bandwidth: u64,
    start: u64,
    end: u64,
) -> Result<()> {
    use ipnet::IpNet;
    use std::net::IpAddr;
    use std::str::FromStr;

    let filter_net: IpNet = subnet
        .parse()
        .with_context(|| format!("Invalid subnet '{subnet}'"))?;

    let (cfg, _wallet) = load_ctx()?;
    let mut client = build_client(&cfg)?;

    let bag_id = get_bag_id(&mut client, &cfg).await?;
    let ids = list_listing_ids(&client, bag_id).await?;

    if ids.is_empty() {
        println!("No listings found.");
        return Ok(());
    }

    struct Row {
        id:            Address,
        name:          String,
        price_mist:    u64,
        ip_address:    String,
        valid_from: u64,
        expires_at: u64,
        bandwidth: u64,
    }

    let mut rows: Vec<Row> = Vec::new();

    for id in ids {
        let l = match get_object_bcs::<ServiceListing>(&mut client, id).await {
            Ok(l) => l,
            Err(e) => { eprintln!("Warning: {e}"); continue; }
        };

        // Subnet filter — strip port if present
        let host = l.ip_address.rsplit_once(':').map(|(h, _)| h).unwrap_or(&l.ip_address);
        if let Ok(ip) = IpAddr::from_str(host) {
            if !filter_net.contains(&ip) { continue; }
        }
        if min_bandwidth > 0 && l.token.bandwidth < min_bandwidth { continue; }
        if start > 0 && l.token.valid_from > start { continue; }
        if end   > 0 && l.token.expires_at < end   { continue; }

        rows.push(Row {
            id,
            name:          l.name.clone(),
            price_mist:    l.price_mist,
            ip_address:    l.ip_address.clone(),
            valid_from: l.token.valid_from,
            expires_at: l.token.expires_at,
            bandwidth: l.token.bandwidth,
        });
    }

    if rows.is_empty() {
        println!("No listings match the given filters.");
        return Ok(());
    }

    rows.sort_by_key(|r| r.price_mist);

    println!("{:<68}  {:>10}  {:<22}  {}", "Listing ID", "Price SUI", "IP", "Name");
    println!("{}", "─".repeat(120));
    for r in &rows {
        let bw = if r.bandwidth == 0 { "unlimited".into() } else { format!("{} kB/s", r.bandwidth) };
        println!(
            "{:<68}  {:>10.4}  {:<22}  {}  (from: {} s, until: {} s, bw: {bw})",
            r.id, r.price_mist as f64 / 1e9, r.ip_address, r.name, r.valid_from, r.expires_at,
        );
    }
    println!("\n{} listing(s) found.", rows.len());
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// get_listing / get_ip_address
// ─────────────────────────────────────────────────────────────────────────────

/// Fetch and BCS-deserialise a single ServiceListing by its object ID.
/// Callers use this to inspect seller bounds before calling `buy_listing`.
pub async fn get_listing(listing_id: &str) -> Result<ServiceListing> {
    let (cfg, _wallet) = load_ctx()?;
    let mut client = build_client(&cfg)?;
    let id: Address = listing_id.parse().context("Invalid listing ID")?;
    get_object_bcs(&mut client, id).await
}

pub async fn get_ip_address(listing_id: &str) -> Result<String> {
    Ok(get_listing(listing_id).await?.ip_address)
}

// ─────────────────────────────────────────────────────────────────────────────
// buy_listing
// ─────────────────────────────────────────────────────────────────────────────

pub async fn buy_listing(
    listing_id: String,
    start: u64,
    end: u64,
    bandwidth: u64,
) -> Result<Address> {
    let (cfg, wallet) = load_ctx()?;
    let mut client = build_client(&cfg)?;
    let listing_obj_id: Address = listing_id.parse().context("Invalid listing ID")?;

    // Callers must resolve defaults before calling; zeros are rejected by the contract.
    anyhow::ensure!(start > 0,        "start must be non-zero; resolve to current time before calling buy_listing");
    anyhow::ensure!(end > start,   "end must be greater than start");
    anyhow::ensure!(bandwidth > 0,   "bandwidth must be non-zero; resolve to seller's bound before calling buy_listing");

    // Fetch listing to validate granularity alignment before submitting the transaction.
    let listing: ServiceListing = get_object_bcs(&mut client, listing_obj_id).await?;
    let duration = end - start;
    let tg = listing.time_granularity;
    let bg = listing.bw_granularity;
    anyhow::ensure!(
        tg == 0 || duration % tg == 0,
        "duration {duration} s is not aligned to listing time_granularity {tg} s \
         (nearest lower end: {})",
        start + (duration / tg) * tg
    );
    anyhow::ensure!(
        bg == 0 || bandwidth % bg == 0,
        "bandwidth {bandwidth} kB/s is not aligned to listing bw_granularity {bg} kB/s \
         (nearest lower value: {})",
        (bandwidth / bg) * bg
    );

    // Pass gas coin as payment — move_call will split price_mist from it.
    let mut builder = TransactionBuilder::new();
    builder.set_sender(wallet.address);
    builder.set_gas_budget(cfg.sui.gas_budget);

    let gas_coin  = builder.gas();
    let mp        = builder.object(ObjectInput::new(cfg.marketplace.marketplace_id));
    let id_arg    = builder.pure(&listing_obj_id.into_inner()); // ID = [u8;32]
    let start_arg = builder.pure(&start);
    let end_arg   = builder.pure(&end);
    let bw_arg    = builder.pure(&bandwidth);

    builder.move_call(
        Function::new(
            cfg.marketplace.package_id,
            Identifier::new("marketplace").unwrap(),
            Identifier::new("purchase").unwrap(),
        )
        .with_type_args(vec![coin_type_tag(&cfg)?]),
        vec![mp, id_arg, gas_coin, start_arg, end_arg, bw_arg],
    );

    let effects = execute(&mut client, &wallet, builder).await?;
    let token_id = find_access_token(&effects)?;

    println!("Access token minted!");
    println!("  Listing:  {listing_id}");
    println!("  Token ID: {token_id}");
    Ok(token_id)
}

// ─────────────────────────────────────────────────────────────────────────────
// redeem
// ─────────────────────────────────────────────────────────────────────────────

pub async fn redeem(token_id: String, ip_address: String) -> Result<()> {
    let (cfg, wallet) = load_ctx()?;
    let mut client = build_client(&cfg)?;
    let token_addr: Address = token_id.parse().context("Invalid token ID")?;

    // Fetch service name for display before the token is destroyed.
    let token: AccessToken = get_object_bcs(&mut client, token_addr).await?;
    let service_name = token.service_name.clone();
    println!("Redeeming token for '{service_name}'…");

    let mut builder = TransactionBuilder::new();
    builder.set_sender(wallet.address);
    builder.set_gas_budget(cfg.sui.gas_budget);

    let token_arg = builder.object(ObjectInput::new(token_addr));
    let ip_arg    = builder.pure(&ip_address.into_bytes());

    builder.move_call(
        Function::new(
            cfg.marketplace.package_id,
            Identifier::new("marketplace").unwrap(),
            Identifier::new("redeem").unwrap(),
        )
        .with_type_args(vec![]),
        vec![token_arg, ip_arg],
    );

    let _ = execute(&mut client, &wallet, builder).await?;

    println!("Token redeemed! TokenRedeemed event emitted on-chain.");
    println!("  Service: {service_name}");
    Ok(())
}
