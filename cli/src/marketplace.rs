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

use crate::models::{AccessToken, MarketplaceObject, ServiceListing};
use crate::utils::Wallet;

// ─────────────────────────────────────────────────────────────────────────────
// Constants — update after each `sui client publish`
// ─────────────────────────────────────────────────────────────────────────────

pub const PACKAGE_ID: &str =
    "0x16c820f39b159f91a10acb9dcf2e9b12b3d1ba855b7f49d9221574b44fa3cd91";
const MARKETPLACE_ID: &str =
    "0x6105d1937ce316c44aa02e2da7a4e6e621605e9b97e2280d0513998a6f540d99";
const COIN_TYPE: &str  = "0x2::sui::SUI";
const GAS_BUDGET: u64 = 50_000_000; // 0.05 SUI

// ─────────────────────────────────────────────────────────────────────────────
// Parse helpers
// ─────────────────────────────────────────────────────────────────────────────

fn package_addr() -> Result<Address> {
    PACKAGE_ID.parse().context("Invalid PACKAGE_ID")
}
fn marketplace_addr() -> Result<Address> {
    MARKETPLACE_ID.parse().context("Invalid MARKETPLACE_ID")
}
fn coin_type_tag() -> Result<TypeTag> {
    COIN_TYPE.parse().context("Invalid COIN_TYPE")
}

// ─────────────────────────────────────────────────────────────────────────────
// Client construction
// ─────────────────────────────────────────────────────────────────────────────

fn build_client(wallet: &Wallet) -> Result<Client> {
    Client::new(wallet.rpc_url.as_str())
        .map_err(|e| anyhow::anyhow!("Cannot connect to {}: {e}", wallet.rpc_url))
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
async fn get_bag_id(client: &mut Client) -> Result<Address> {
    let mp: MarketplaceObject = get_object_bcs(client, marketplace_addr()?).await?;
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
    let wallet = crate::utils::load_wallet()?;
    let mut client = build_client(&wallet)?;

    let mut builder = TransactionBuilder::new();
    builder.set_sender(wallet.address);
    builder.set_gas_budget(GAS_BUDGET);

    builder.move_call(
        Function::new(
            package_addr()?,
            Identifier::new("marketplace").unwrap(),
            Identifier::new("create_marketplace").unwrap(),
        )
        .with_type_args(vec![coin_type_tag()?]),
        vec![],
    );

    let effects = execute(&mut client, &wallet, builder).await?;
    let mp_id = find_created(&effects, "::marketplace::Marketplace")?;

    println!("Marketplace created!");
    println!("  Coin type:      {COIN_TYPE}");
    println!("  Marketplace ID: {mp_id}");
    println!("  Update MARKETPLACE_ID in marketplace.rs to use it.");
    Ok(mp_id)
}

// ─────────────────────────────────────────────────────────────────────────────
// create_listing
// ─────────────────────────────────────────────────────────────────────────────

pub async fn create_listing(
    name: String,
    ip_address: String,
    price_sui: f64,
    valid_from_ms: u64,
    expires_at_ms: u64,
    max_bandwidth_bps: u64,
    min_bandwidth_bps: u64,
    min_duration_ms: u64,
    bw_granularity: u64,
    time_granularity: u64,
) -> Result<Address> {
    let wallet = crate::utils::load_wallet()?;
    let mut client = build_client(&wallet)?;
    let price_mist = (price_sui * 1_000_000_000.0) as u64;

    let mut builder = TransactionBuilder::new();
    builder.set_sender(wallet.address);
    builder.set_gas_budget(GAS_BUDGET);

    let mp  = builder.object(ObjectInput::new(marketplace_addr()?));
    let a0  = builder.pure(&name.into_bytes());
    let a1  = builder.pure(&ip_address.into_bytes());
    let a2  = builder.pure(&price_mist);
    let a3  = builder.pure(&valid_from_ms);
    let a4  = builder.pure(&expires_at_ms);
    let a5  = builder.pure(&max_bandwidth_bps);
    let a6  = builder.pure(&min_bandwidth_bps);
    let a7  = builder.pure(&min_duration_ms);
    let a8  = builder.pure(&bw_granularity);
    let a9  = builder.pure(&time_granularity);

    builder.move_call(
        Function::new(
            package_addr()?,
            Identifier::new("marketplace").unwrap(),
            Identifier::new("create_listing").unwrap(),
        )
        .with_type_args(vec![coin_type_tag()?]),
        vec![mp, a0, a1, a2, a3, a4, a5, a6, a7, a8, a9],
    );

    let effects = execute(&mut client, &wallet, builder).await?;
    let listing_id = find_created(&effects, "::marketplace::ServiceListing")?;

    println!("Listing created!");
    println!("  Listing:        {listing_id}");
    println!("  Price:          {price_sui} SUI");
    println!("  Valid from:     {valid_from_ms} ms");
    println!("  Expires at:     {expires_at_ms} ms");
    println!("  Max BW:         {max_bandwidth_bps} B/s");
    println!("  Min BW:         {min_bandwidth_bps} B/s");
    println!("  Min duration:   {min_duration_ms} ms");
    println!("  BW granularity: {bw_granularity} B/s");
    println!("  Time gran.:     {time_granularity} ms");
    Ok(listing_id)
}

// ─────────────────────────────────────────────────────────────────────────────
// get_listings
// ─────────────────────────────────────────────────────────────────────────────

pub async fn get_listings(limit: u32) -> Result<()> {
    let wallet = crate::utils::load_wallet()?;
    let mut client = build_client(&wallet)?;

    let bag_id = get_bag_id(&mut client).await?;
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
        let bw = if l.token.bandwidth_bps == 0 {
            "unlimited".to_string()
        } else {
            format!("{} B/s", l.token.bandwidth_bps)
        };
        println!(
            "{id:<68}  {price:>10.4}  {}  (expires: {}, bw: {bw})",
            l.name, l.token.expires_at_ms
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
    min_bandwidth_bps: u64,
    start_ms: u64,
    end_ms: u64,
) -> Result<()> {
    use ipnet::IpNet;
    use std::net::IpAddr;
    use std::str::FromStr;

    let filter_net: IpNet = subnet
        .parse()
        .with_context(|| format!("Invalid subnet '{subnet}'"))?;

    let wallet = crate::utils::load_wallet()?;
    let mut client = build_client(&wallet)?;

    let bag_id = get_bag_id(&mut client).await?;
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
        valid_from_ms: u64,
        expires_at_ms: u64,
        bandwidth_bps: u64,
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
        if min_bandwidth_bps > 0 && l.token.bandwidth_bps < min_bandwidth_bps { continue; }
        if start_ms > 0 && l.token.valid_from_ms > start_ms { continue; }
        if end_ms   > 0 && l.token.expires_at_ms < end_ms   { continue; }

        rows.push(Row {
            id,
            name:          l.name.clone(),
            price_mist:    l.price_mist,
            ip_address:    l.ip_address.clone(),
            valid_from_ms: l.token.valid_from_ms,
            expires_at_ms: l.token.expires_at_ms,
            bandwidth_bps: l.token.bandwidth_bps,
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
        let bw = if r.bandwidth_bps == 0 { "unlimited".into() } else { format!("{} B/s", r.bandwidth_bps) };
        println!(
            "{:<68}  {:>10.4}  {:<22}  {}  (from: {}, until: {}, bw: {bw})",
            r.id, r.price_mist as f64 / 1e9, r.ip_address, r.name, r.valid_from_ms, r.expires_at_ms,
        );
    }
    println!("\n{} listing(s) found.", rows.len());
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// get_ip_address  (called by client_example before purchasing)
// ─────────────────────────────────────────────────────────────────────────────

pub async fn get_ip_address(listing_id: &str) -> Result<String> {
    let wallet = crate::utils::load_wallet()?;
    let mut client = build_client(&wallet)?;
    let id: Address = listing_id.parse().context("Invalid listing ID")?;
    let l: ServiceListing = get_object_bcs(&mut client, id).await?;
    Ok(l.ip_address)
}

// ─────────────────────────────────────────────────────────────────────────────
// buy_listing
// ─────────────────────────────────────────────────────────────────────────────

pub async fn buy_listing(
    listing_id: String,
    start_ms: u64,
    end_ms: u64,
    bandwidth_bps: u64,
) -> Result<Address> {
    let wallet = crate::utils::load_wallet()?;
    let mut client = build_client(&wallet)?;
    let listing_obj_id: Address = listing_id.parse().context("Invalid listing ID")?;

    // Pass gas coin as payment — move_call will split price_mist from it.
    let mut builder = TransactionBuilder::new();
    builder.set_sender(wallet.address);
    builder.set_gas_budget(GAS_BUDGET);

    let gas_coin  = builder.gas();
    let mp        = builder.object(ObjectInput::new(marketplace_addr()?));
    let id_arg    = builder.pure(&listing_obj_id.into_inner()); // ID = [u8;32]
    let start_arg = builder.pure(&start_ms);
    let end_arg   = builder.pure(&end_ms);
    let bw_arg    = builder.pure(&bandwidth_bps);

    builder.move_call(
        Function::new(
            package_addr()?,
            Identifier::new("marketplace").unwrap(),
            Identifier::new("purchase").unwrap(),
        )
        .with_type_args(vec![coin_type_tag()?]),
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
    let wallet = crate::utils::load_wallet()?;
    let mut client = build_client(&wallet)?;
    let token_addr: Address = token_id.parse().context("Invalid token ID")?;

    // Fetch service name for display before the token is destroyed.
    let token: AccessToken = get_object_bcs(&mut client, token_addr).await?;
    let service_name = token.service_name.clone();
    println!("Redeeming token for '{service_name}'…");

    let mut builder = TransactionBuilder::new();
    builder.set_sender(wallet.address);
    builder.set_gas_budget(GAS_BUDGET);

    let token_arg = builder.object(ObjectInput::new(token_addr));
    let ip_arg    = builder.pure(&ip_address.into_bytes());

    builder.move_call(
        Function::new(
            package_addr()?,
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
