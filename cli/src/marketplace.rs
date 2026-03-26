use anyhow::{bail, Context, Result};
use move_core_types::identifier::Identifier;
use move_core_types::language_storage::TypeTag;
use std::str::FromStr;
use sui_sdk::rpc_types::{
    ObjectChange, SuiObjectDataOptions, SuiParsedData, SuiTransactionBlockResponse,
    SuiTransactionBlockResponseOptions, SuiTransactionBlockEffectsAPI,
};
use sui_sdk::wallet_context::WalletContext;
use sui_sdk::{SuiClient, SuiClientBuilder};
use sui_types::{
    base_types::{ObjectID, ObjectRef, SequenceNumber},
    object::Owner,
    programmable_transaction_builder::ProgrammableTransactionBuilder,
    transaction::{CallArg, ObjectArg, SharedObjectMutability, TransactionData},
    transaction_driver_types::ExecuteTransactionRequestType,
};
use serde_json::Value;

pub const PACKAGE_ID: &str = "0x16c820f39b159f91a10acb9dcf2e9b12b3d1ba855b7f49d9221574b44fa3cd91";
// Fill in after publishing the updated contract:
const MARKETPLACE_ID: &str = "0x6105d1937ce316c44aa02e2da7a4e6e621605e9b97e2280d0513998a6f540d99";
/// Coin type accepted by MARKETPLACE_ID. Update alongside MARKETPLACE_ID.
const COIN_TYPE: &str = "0x2::sui::SUI";
const GAS_BUDGET: u64 = 50_000_000; // 0.05 SUI

fn coin_type_tag() -> Result<TypeTag> {
    TypeTag::from_str(COIN_TYPE).context("Invalid COIN_TYPE")
}

// ─────────────────────────────────────────────────────────────────────────────
// Client helper
// ─────────────────────────────────────────────────────────────────────────────

async fn get_wallet() -> Result<WalletContext> {
    crate::utils::get_wallet().await
}

/// Build a JSON-RPC SuiClient from the wallet's active environment.
async fn rpc_client(wallet: &WalletContext) -> Result<SuiClient> {
    let rpc_url = &wallet.get_active_env()?.rpc;
    SuiClientBuilder::default()
        .build(rpc_url)
        .await
        .context("Failed to build SuiClient")
}

// ─────────────────────────────────────────────────────────────────────────────
// Private helpers using the JSON-RPC SuiClient
// ─────────────────────────────────────────────────────────────────────────────

/// Fetch the parsed Move fields of an object as a JSON Value.
async fn get_move_fields(client: &SuiClient, id: ObjectID) -> Result<Value> {
    let opts = SuiObjectDataOptions::new().with_content();
    let resp = client
        .read_api()
        .get_object_with_options(id, opts)
        .await?;
    let data = resp.data.context("Object not found")?;
    match data.content.context("No content")? {
        SuiParsedData::MoveObject(obj) => Ok(serde_json::to_value(&obj.fields)?),
        _ => bail!("Object {id} is not a Move object"),
    }
}

/// Return the `initial_shared_version` of a shared object.
async fn get_shared_version(client: &SuiClient, id: ObjectID) -> Result<SequenceNumber> {
    let opts = SuiObjectDataOptions::new().with_owner();
    let resp = client
        .read_api()
        .get_object_with_options(id, opts)
        .await?;
    let data = resp.data.context("Object not found")?;
    match data.owner.context("No owner field")? {
        Owner::Shared { initial_shared_version } => Ok(initial_shared_version),
        _ => bail!("Object {id} is not a shared object"),
    }
}

/// Return the `ObjectRef` of an immutable or owned object.
async fn get_owned_object_ref(client: &SuiClient, id: ObjectID) -> Result<ObjectRef> {
    let resp = client
        .read_api()
        .get_object_with_options(id, SuiObjectDataOptions::new())
        .await?;
    Ok(resp.data.context("Object not found")?.object_ref())
}

/// Find a coin to use as payment — distinct from the gas coin (first coin).
/// Returns its ObjectRef. The wallet must hold ≥2 SUI coins.
async fn find_payment_coin(
    client: &SuiClient,
    owner: sui_types::base_types::SuiAddress,
) -> Result<ObjectRef> {
    let page = client
        .coin_read_api()
        .get_coins(owner, Some(COIN_TYPE.into()), None, Some(50))
        .await?;

    if page.data.is_empty() {
        bail!("No SUI coins found for {owner}. Fund your wallet first.");
    }
    if page.data.len() == 1 {
        bail!(
            "Only one coin found. Split it first:\n  \
             sui client split-coin --coin-id {} --amounts <price>",
            page.data[0].coin_object_id,
        );
    }

    // Skip index 0 (used as gas coin); return the first available payment coin.
    Ok(page.data[1].object_ref())
}

// ─────────────────────────────────────────────────────────────────────────────
// Shared tx helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Wrap a finished PTB into `TransactionData` using the first coin as gas.
async fn build_tx_data(
    client: &SuiClient,
    wallet: &mut WalletContext,
    ptb: ProgrammableTransactionBuilder,
) -> Result<TransactionData> {
    let address = wallet.active_address()?;

    let coins = client
        .coin_read_api()
        .get_coins(address, None, None, None)
        .await
        .context("Failed to fetch gas coins")?;
    let gas_ref = coins
        .data
        .first()
        .context("No gas coin — fund your wallet first")?
        .object_ref();

    let gas_price = client
        .read_api()
        .get_reference_gas_price()
        .await
        .context("Failed to fetch reference gas price")?;

    Ok(TransactionData::new_programmable(
        address,
        vec![gas_ref],
        ptb.finish(),
        GAS_BUDGET,
        gas_price,
    ))
}

/// Sign and execute a transaction, returning the full response.
/// Fails if the transaction was rejected or effects indicate failure.
async fn sign_and_execute(
    client: &SuiClient,
    wallet: &WalletContext,
    tx_data: TransactionData,
) -> Result<SuiTransactionBlockResponse> {
    let tx = wallet.sign_transaction(&tx_data).await;

    let response = client
        .quorum_driver_api()
        .execute_transaction_block(
            tx,
            SuiTransactionBlockResponseOptions::new()
                .with_effects()
                .with_object_changes(),
            Some(ExecuteTransactionRequestType::WaitForLocalExecution),
        )
        .await
        .context("Failed to execute transaction")?;

    let effects = response.effects.as_ref().context("No effects in response")?;
    if !effects.status().is_ok() {
        bail!("Transaction failed on-chain: {:?}", effects.status());
    }

    Ok(response)
}

/// Find a created object by Move type name (e.g. "AccessToken") in object_changes.
/// This is reliable even when seller == buyer (both own created objects in the tx).
fn get_created_object_id_by_type(
    response: &SuiTransactionBlockResponse,
    type_name: &str,
) -> Result<ObjectID> {
    let changes = response
        .object_changes
        .as_deref()
        .context("No object_changes in response")?;
    changes
        .iter()
        .find_map(|c| {
            if let ObjectChange::Created { object_id, object_type, .. } = c {
                if object_type.name.as_str() == type_name {
                    return Some(*object_id);
                }
            }
            None
        })
        .with_context(|| format!("No created object of type '{type_name}' found"))
}

/// Find the AccessToken produced by a `purchase` transaction.
///
/// Two cases:
/// - Token fields were mutated by the buyer → SUI reports it in `object_changes`
///   as `Transferred` or `Mutated`.
/// - Token fields were unchanged (buyer kept seller's values) → SUI skips the
///   object_change entry and only reports it in `effects.unwrapped()`.
fn get_access_token_from_purchase(response: &SuiTransactionBlockResponse) -> Result<ObjectID> {
    // Fast path: token appears in object_changes (mutated or transferred).
    if let Some(changes) = response.object_changes.as_deref() {
        let found = changes.iter().find_map(|c| {
            let (object_id, object_type) = match c {
                ObjectChange::Created    { object_id, object_type, .. } => (object_id, object_type),
                ObjectChange::Transferred{ object_id, object_type, .. } => (object_id, object_type),
                ObjectChange::Mutated    { object_id, object_type, .. } => (object_id, object_type),
                _ => return None,
            };
            if object_type.name.as_str() == "AccessToken" { Some(*object_id) } else { None }
        });
        if let Some(id) = found {
            return Ok(id);
        }
    }

    // Fallback: token was unwrapped without field changes; SUI reports it only in
    // effects.unwrapped(). The AccessToken is the sole wrapped object in a purchase tx.
    let effects = response.effects.as_ref().context("No effects in response")?;
    effects
        .unwrapped()
        .first()
        .map(|o| o.reference.object_id)
        .context("AccessToken not found in object_changes or effects.unwrapped()")
}

/// Find a created object that is NOT address-owned (i.e. stored in an ObjectBag).
fn get_created_bag_object_id(
    response: &SuiTransactionBlockResponse,
) -> Result<ObjectID> {
    let effects = response.effects.as_ref().context("No effects in response")?;
    effects
        .created()
        .iter()
        .find(|o| !matches!(o.owner, Owner::AddressOwner(_)))
        .context("No bag-stored object created in transaction")
        .map(|o| o.reference.object_id)
}

/// Build the Marketplace shared-mutable CallArg.
async fn marketplace_arg(client: &SuiClient) -> Result<CallArg> {
    let marketplace_id = ObjectID::from_str(MARKETPLACE_ID)?;
    let version = get_shared_version(client, marketplace_id).await?;
    Ok(CallArg::Object(ObjectArg::SharedObject {
        id: marketplace_id,
        initial_shared_version: version,
        mutability: SharedObjectMutability::Mutable,
    }))
}

/// Return the `ip_address` stored on a ServiceListing.
/// Must be called BEFORE buying — purchase removes the listing from the bag.
pub async fn get_ip_address(listing_id: &str) -> Result<String> {
    let wallet = get_wallet().await?;
    let client = rpc_client(&wallet).await?;
    let id = ObjectID::from_str(listing_id)?;
    let fields = get_move_fields(&client, id).await?;
    fields["ip_address"]
        .as_str()
        .map(|s| s.to_string())
        .context("No ip_address field on listing")
}

// ─────────────────────────────────────────────────────────────────────────────
// create_marketplace
// ─────────────────────────────────────────────────────────────────────────────

/// Deploy a new Marketplace<COIN_TYPE> shared object and print its ID.
pub async fn create_marketplace() -> Result<ObjectID> {
    let mut wallet = get_wallet().await?;
    let client = rpc_client(&wallet).await?;

    let mut ptb = ProgrammableTransactionBuilder::new();
    ptb.move_call(
        ObjectID::from_str(PACKAGE_ID)?,
        Identifier::new("marketplace")?,
        Identifier::new("create_marketplace")?,
        vec![coin_type_tag()?],
        vec![],
    )?;

    let tx_data = build_tx_data(&client, &mut wallet, ptb).await?;
    let response = sign_and_execute(&client, &wallet, tx_data).await?;
    let marketplace_id = get_created_bag_object_id(&response)?;

    println!("Marketplace created!");
    println!("  Coin type:      {COIN_TYPE}");
    println!("  Marketplace ID: {marketplace_id}");
    println!("  Update MARKETPLACE_ID in marketplace.rs to use it.");
    Ok(marketplace_id)
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
) -> Result<ObjectID> {
    let mut wallet = get_wallet().await?;
    let price_mist = (price_sui * 1_000_000_000.0) as u64;
    let client = rpc_client(&wallet).await?;

    let mut ptb = ProgrammableTransactionBuilder::new();
    ptb.move_call(
        ObjectID::from_str(PACKAGE_ID)?,
        Identifier::new("marketplace")?,
        Identifier::new("create_listing")?,
        vec![coin_type_tag()?],
        vec![
            marketplace_arg(&client).await?,
            CallArg::Pure(bcs::to_bytes(name.as_bytes())?),
            CallArg::Pure(bcs::to_bytes(ip_address.as_bytes())?),
            CallArg::Pure(bcs::to_bytes(&price_mist)?),
            CallArg::Pure(bcs::to_bytes(&valid_from_ms)?),
            CallArg::Pure(bcs::to_bytes(&expires_at_ms)?),
            CallArg::Pure(bcs::to_bytes(&max_bandwidth_bps)?),
            CallArg::Pure(bcs::to_bytes(&min_bandwidth_bps)?),
            CallArg::Pure(bcs::to_bytes(&min_duration_ms)?),
            CallArg::Pure(bcs::to_bytes(&bw_granularity)?),
            CallArg::Pure(bcs::to_bytes(&time_granularity)?),
        ],
    )?;

    let tx_data = build_tx_data(&client, &mut wallet, ptb).await?;
    let response = sign_and_execute(&client, &wallet, tx_data).await?;
    let listing_id = get_created_object_id_by_type(&response, "ServiceListing")?;
    let digest = response.digest.to_string();

    println!("Listing created!");
    println!("  Listing:        {listing_id}");
    println!("  Name:           {name}");
    println!("  Price:          {price_sui} SUI");
    println!("  Valid from:     {valid_from_ms} ms");
    println!("  Expires at:     {expires_at_ms} ms");
    println!("  Max BW:         {}", if max_bandwidth_bps == 0 { "unlimited".into() } else { format!("{max_bandwidth_bps} B/s") });
    println!("  Min BW:         {}", if min_bandwidth_bps == 0 { "none".into() } else { format!("{min_bandwidth_bps} B/s") });
    println!("  Min duration:   {}", if min_duration_ms   == 0 { "none".into() } else { format!("{min_duration_ms} ms") });
    println!("  BW granularity: {}", if bw_granularity    == 0 { "any".into()  } else { format!("{bw_granularity} B/s") });
    println!("  Time gran.:     {}", if time_granularity  == 0 { "any".into()  } else { format!("{time_granularity} ms") });
    println!("  Tx:             {digest}");
    Ok(listing_id)
}

// ─────────────────────────────────────────────────────────────────────────────
// get_listings
// ─────────────────────────────────────────────────────────────────────────────

pub async fn get_listings(limit: u32) -> Result<()> {
    let wallet = get_wallet().await?;
    let client = rpc_client(&wallet).await?;
    let marketplace_id = ObjectID::from_str(MARKETPLACE_ID)?;

    // The listings are stored inside the Marketplace's ObjectBag field, which
    // has its own UID. We must query dynamic fields on the bag's ID, not the
    // Marketplace's ID.
    let marketplace_fields = get_move_fields(&client, marketplace_id).await?;
    println!("{}", marketplace_fields);
    let bag_id_str = marketplace_fields["listings"]["fields"]["id"]["id"]
        .as_str()
        .context("Could not find listings ObjectBag ID in Marketplace fields")?;
    let bag_id = ObjectID::from_str(bag_id_str)?;

    let fields = client
        .read_api()
        .get_dynamic_fields(bag_id, None, Some(limit as usize))
        .await?;

    if fields.data.is_empty() {
        println!("No listings found.");
        return Ok(());
    }

    // Batch-fetch listing objects
    let ids: Vec<ObjectID> = fields.data.iter().map(|f| f.object_id).collect();
    let objects = client
        .read_api()
        .multi_get_object_with_options(ids, SuiObjectDataOptions::new().with_content())
        .await?;

    println!("{:<68}  {:>10}  {}", "Listing ID", "Price SUI", "Name");
    println!("{}", "─".repeat(100));

    let mut shown = 0;
    for obj_resp in &objects {
        let Some(data) = &obj_resp.data else { continue };
        let Some(SuiParsedData::MoveObject(move_obj)) = &data.content else { continue };
        let f = serde_json::to_value(&move_obj.fields)?;
        let id = data.object_id.to_string();
        let name = f["name"].as_str().unwrap_or("?");
        let price: f64 = f["price_mist"]
            .as_str()
            .and_then(|s: &str| s.parse::<u64>().ok())
            .unwrap_or(0) as f64
            / 1e9;
        let expires_at_ms: u64 = f["token"]["fields"]["expires_at_ms"]
            .as_str()
            .and_then(|s: &str| s.parse().ok())
            .unwrap_or(0);
        let bandwidth_bps: u64 = f["token"]["fields"]["bandwidth_bps"]
            .as_str()
            .and_then(|s: &str| s.parse().ok())
            .unwrap_or(0);
        let expires = if expires_at_ms == 0 { "never".to_string() } else { format!("{expires_at_ms} ms") };
        let bw      = if bandwidth_bps  == 0 { "unlimited".to_string() } else { format!("{bandwidth_bps} B/s") };
        println!("{id:<68}  {price:>10.4}  {name}  (expires: {expires}, bw: {bw})");
        shown += 1;
    }

    println!("\n{shown} listing(s) shown.");
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// search_listings
// ─────────────────────────────────────────────────────────────────────────────

/// Search listings by subnet, minimum bandwidth, and time window.
///
/// Fetches all listings from the marketplace, applies filters, then displays
/// results sorted by increasing price.
///
/// - `subnet`:            only show listings whose IP is contained in this subnet
/// - `min_bandwidth_bps`: only show listings offering at least this bandwidth (0 = any)
/// - `start_ms`:          only show listings available at/before this timestamp (0 = skip)
/// - `end_ms`:            only show listings valid at/after this timestamp (0 = skip)
pub async fn search_listings(
    subnet: &str,
    min_bandwidth_bps: u64,
    start_ms: u64,
    end_ms: u64,
) -> Result<()> {
    use ipnet::IpNet;
    use std::net::IpAddr;
    use std::str::FromStr;

    let filter_net: IpNet = subnet.parse()
        .with_context(|| format!("Invalid subnet '{subnet}'"))?;

    let wallet = get_wallet().await?;
    let client = rpc_client(&wallet).await?;
    let marketplace_id = ObjectID::from_str(MARKETPLACE_ID)?;

    let marketplace_fields = get_move_fields(&client, marketplace_id).await?;
    let bag_id_str = marketplace_fields["listings"]["fields"]["id"]["id"]
        .as_str()
        .context("Could not find listings ObjectBag ID")?;
    let bag_id = ObjectID::from_str(bag_id_str)?;

    // Paginate through all listings
    let mut all_field_ids: Vec<ObjectID> = Vec::new();
    let mut cursor = None;
    loop {
        let page = client
            .read_api()
            .get_dynamic_fields(bag_id, cursor, Some(50))
            .await?;
        all_field_ids.extend(page.data.iter().map(|f| f.object_id));
        if page.has_next_page {
            cursor = page.next_cursor;
        } else {
            break;
        }
    }

    if all_field_ids.is_empty() {
        println!("No listings found.");
        return Ok(());
    }

    let objects = client
        .read_api()
        .multi_get_object_with_options(all_field_ids, SuiObjectDataOptions::new().with_content())
        .await?;

    struct ListingRow {
        id: String,
        name: String,
        price_mist: u64,
        price_sui: f64,
        ip_address: String,
        valid_from_ms: u64,
        expires_at_ms: u64,
        bandwidth_bps: u64,
    }

    let mut rows: Vec<ListingRow> = Vec::new();

    for obj_resp in &objects {
        let Some(data) = &obj_resp.data else { continue };
        let Some(SuiParsedData::MoveObject(move_obj)) = &data.content else { continue };
        let f = serde_json::to_value(&move_obj.fields)?;

        let ip_str   = f["ip_address"].as_str().unwrap_or("").to_string();
        let bw: u64  = f["token"]["fields"]["bandwidth_bps"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0);
        let from: u64 = f["token"]["fields"]["valid_from_ms"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0);
        let until: u64 = f["token"]["fields"]["expires_at_ms"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0);
        let price_mist: u64 = f["price_mist"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0);

        // Subnet filter: strip port if present, then check containment
        let host_str = ip_str.rsplit_once(':').map(|(h, _)| h).unwrap_or(&ip_str);
        if let Ok(ip) = IpAddr::from_str(host_str) {
            if !filter_net.contains(&ip) {
                continue;
            }
        }
        // Bandwidth filter
        if min_bandwidth_bps > 0 && bw < min_bandwidth_bps {
            continue;
        }
        // Start filter: listing must be available at or before the requested start
        if start_ms > 0 && from > start_ms {
            continue;
        }
        // End filter: listing must stay valid at or after the requested end
        if end_ms > 0 && until < end_ms {
            continue;
        }

        rows.push(ListingRow {
            id: data.object_id.to_string(),
            name: f["name"].as_str().unwrap_or("?").to_string(),
            price_mist,
            price_sui: price_mist as f64 / 1e9,
            ip_address: ip_str,
            valid_from_ms: from,
            expires_at_ms: until,
            bandwidth_bps: bw,
        });
    }

    if rows.is_empty() {
        println!("No listings match the given filters.");
        return Ok(());
    }

    rows.sort_by_key(|r| r.price_mist);

    println!("{:<68}  {:>10}  {:<20}  {}", "Listing ID", "Price SUI", "IP", "Name");
    println!("{}", "─".repeat(120));
    for r in &rows {
        let bw_str = if r.bandwidth_bps == 0 { "unlimited".to_string() } else { format!("{} B/s", r.bandwidth_bps) };
        println!(
            "{:<68}  {:>10.4}  {:<20}  {}  (from: {}, until: {}, bw: {})",
            r.id, r.price_sui, r.ip_address, r.name, r.valid_from_ms, r.expires_at_ms, bw_str
        );
    }
    println!("\n{} listing(s) found.", rows.len());
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// buy_listing
// ─────────────────────────────────────────────────────────────────────────────

pub async fn buy_listing(
    listing_id: String,
    start_ms: u64,
    end_ms: u64,
    bandwidth_bps: u64,
) -> Result<ObjectID> {
    let mut wallet = get_wallet().await?;
    let client = rpc_client(&wallet).await?;
    let listing_obj_id = ObjectID::from_str(&listing_id)?;
    let address = wallet.active_address()?; // needed for find_payment_coin

    // Resolve 0-defaults client-side: fetch listing bounds before purchasing
    // (purchase removes the listing from the bag, so we must read it first).
    let listing_fields = get_move_fields(&client, listing_obj_id).await?;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis() as u64;
    let resolved_start = if start_ms == 0 { now_ms } else { start_ms };
    let mut resolved_end: u64 = if end_ms == 0 {
        listing_fields["token"]["fields"]["expires_at_ms"]
            .as_str().and_then(|s| s.parse().ok())
            .context("expires_at_ms missing from listing")?
    } else { end_ms };
    //align end_ms to fit granularity
    let granularity: u64 = listing_fields["time_granularity"].as_str().and_then(|s| s.parse().ok()).context("No granularity set")?;
    if (resolved_end-resolved_start)%granularity !=0 {
        resolved_end = resolved_end - ((resolved_end-resolved_start)%granularity);
    }

    let resolved_bw: u64 = if bandwidth_bps == 0 {
        listing_fields["token"]["fields"]["bandwidth_bps"]
            .as_str().and_then(|s| s.parse().ok()).unwrap_or(0)
    } else { bandwidth_bps };

    let payment_ref = find_payment_coin(&client, address).await?;

    let mut ptb = ProgrammableTransactionBuilder::new();
    ptb.move_call(
        ObjectID::from_str(PACKAGE_ID)?,
        Identifier::new("marketplace")?,
        Identifier::new("purchase")?,
        vec![coin_type_tag()?],
        vec![
            // &mut Marketplace (shared mutable)
            marketplace_arg(&client).await?,
            // listing_id: ID (pure)
            CallArg::Pure(bcs::to_bytes(&listing_obj_id)?),
            // &mut Coin<SUI> (owned payment coin)
            CallArg::Object(ObjectArg::ImmOrOwnedObject(payment_ref)),
            CallArg::Pure(bcs::to_bytes(&resolved_start)?),
            CallArg::Pure(bcs::to_bytes(&resolved_end)?),
            CallArg::Pure(bcs::to_bytes(&resolved_bw)?),
        ],
    )?;

    let tx_data = build_tx_data(&client, &mut wallet, ptb).await?;
    let response = sign_and_execute(&client, &wallet, tx_data).await?;
    let object_id = get_access_token_from_purchase(&response)?;
    let digest = response.digest.to_string();

    println!("Access token minted!");
    println!("  Listing:  {listing_id}");
    println!("  Tx:       {digest}");
    println!("  Token_ID {object_id}");
    Ok(object_id)
}

// ─────────────────────────────────────────────────────────────────────────────
// redeem
// ─────────────────────────────────────────────────────────────────────────────

pub async fn redeem(token_id: String, ip_address: String) -> Result<()> {
    let mut wallet = get_wallet().await?;
    let client = rpc_client(&wallet).await?;
    let token_obj_id = ObjectID::from_str(&token_id)?;

    let fields = get_move_fields(&client, token_obj_id).await?;
    let service_name = fields["service_name"].as_str().unwrap_or("?").to_string();
    println!("Redeeming token for '{service_name}'…");

    let token_ref = get_owned_object_ref(&client, token_obj_id).await?;

    let mut ptb = ProgrammableTransactionBuilder::new();
    ptb.move_call(
        ObjectID::from_str(PACKAGE_ID)?,
        Identifier::new("marketplace")?,
        Identifier::new("redeem")?,
        vec![],
        vec![
            CallArg::Object(ObjectArg::ImmOrOwnedObject(token_ref)),
            CallArg::Pure(bcs::to_bytes(ip_address.as_bytes())?),
        ],
    )?;

    let tx_data = build_tx_data(&client, &mut wallet, ptb).await?;
    let response = sign_and_execute(&client, &wallet, tx_data).await?;
    let digest = response.digest.to_string();

    println!("Token redeemed! TokenRedeemed event emitted on-chain.");
    println!("  Service:  {service_name}");
    println!("  Tx:       {digest}");
    Ok(())
}
