use anyhow::{bail, Context, Result};
use move_core_types::identifier::Identifier;
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

const PACKAGE_ID: &str = "0x7f1c4d5a5a48ec94025deebcff0c570ab086409a4cfd52f4381a1aa08bb7d096";
// Fill in after publishing the updated contract:
const MARKETPLACE_ID: &str = "0x192e5278c0eba145ecf3dbc09bd1b3716fd74e22cfa022090d04bc2d70f30f32";
const CLOCK_ID: &str = "0x6";
const CLOCK_INITIAL_SHARED_VERSION: u64 = 1;
const GAS_BUDGET: u64 = 50_000_000; // 0.05 SUI

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
        .get_coins(owner, Some("0x2::sui::SUI".into()), None, Some(50))
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
// create_listing
// ─────────────────────────────────────────────────────────────────────────────

pub async fn create_listing(
    name: String,
    ip_address: String,
    price_sui: f64,
    duration_hours: f64,
) -> Result<ObjectID> {
    let mut wallet = get_wallet().await?;
    let price_mist = (price_sui * 1_000_000_000.0) as u64;
    let duration_ms = (duration_hours * 3_600_000.0) as u64;
    let client = rpc_client(&wallet).await?;

    let mut ptb = ProgrammableTransactionBuilder::new();
    ptb.move_call(
        ObjectID::from_str(PACKAGE_ID)?,
        Identifier::new("marketplace")?,
        Identifier::new("create_listing")?,
        vec![],
        vec![
            // &mut Marketplace (shared mutable)
            marketplace_arg(&client).await?,
            CallArg::Pure(bcs::to_bytes(name.as_bytes())?),
            CallArg::Pure(bcs::to_bytes(ip_address.as_bytes())?),
            CallArg::Pure(bcs::to_bytes(&price_mist)?),
            CallArg::Pure(bcs::to_bytes(&duration_ms)?),
        ],
    )?;

    let tx_data = build_tx_data(&client, &mut wallet, ptb).await?;
    let response = sign_and_execute(&client, &wallet, tx_data).await?;
    // ServiceListing is stored in the ObjectBag (not address-owned).
    let listing_id = get_created_bag_object_id(&response)?;
    let digest = response.digest.to_string();

    println!("Listing created!");
    println!("  Listing:  {listing_id}");
    println!("  Name:     {name}");
    println!("  Price:    {price_sui} SUI");
    println!(
        "  Duration: {}",
        if duration_hours == 0.0 { "perpetual".into() } else { format!("{duration_hours}h") }
    );
    println!("  Tx:       {digest}");
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
        let dur_ms: u64 = f["duration_ms"]
            .as_str()
            .and_then(|s: &str| s.parse().ok())
            .unwrap_or(0);
        let duration = if dur_ms == 0 {
            "perpetual".to_string()
        } else {
            format!("{:.1}h", dur_ms as f64 / 3_600_000.0)
        };
        println!("{id:<68}  {price:>10.4}  {name}  ({duration})");
        shown += 1;
    }

    println!("\n{shown} listing(s) shown.");
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// buy_listing
// ─────────────────────────────────────────────────────────────────────────────

pub async fn buy_listing(
    listing_id: String,
    start_ms: u64,
    end_ms: u64,
) -> Result<ObjectID> {
    let mut wallet = get_wallet().await?;
    let client = rpc_client(&wallet).await?;
    let listing_obj_id = ObjectID::from_str(&listing_id)?;
    let address = wallet.active_address()?; // needed for find_payment_coin

    let payment_ref = find_payment_coin(&client, address).await?;

    let mut ptb = ProgrammableTransactionBuilder::new();
    ptb.move_call(
        ObjectID::from_str(PACKAGE_ID)?,
        Identifier::new("marketplace")?,
        Identifier::new("purchase")?,
        vec![],
        vec![
            // &mut Marketplace (shared mutable)
            marketplace_arg(&client).await?,
            // listing_id: ID (pure)
            CallArg::Pure(bcs::to_bytes(&listing_obj_id)?),
            // &mut Coin<SUI> (owned payment coin)
            CallArg::Object(ObjectArg::ImmOrOwnedObject(payment_ref)),
            // &Clock (shared immutable)
            CallArg::Object(ObjectArg::SharedObject {
                id: ObjectID::from_str(CLOCK_ID)?,
                initial_shared_version: SequenceNumber::from(CLOCK_INITIAL_SHARED_VERSION),
                mutability: SharedObjectMutability::Immutable,
            }),
            CallArg::Pure(bcs::to_bytes(&start_ms)?),
            CallArg::Pure(bcs::to_bytes(&end_ms)?),
        ],
    )?;

    let tx_data = build_tx_data(&client, &mut wallet, ptb).await?;
    let response = sign_and_execute(&client, &wallet, tx_data).await?;
    // purchase creates two objects: AccessToken and a split payment coin.
    // Filter by type name — owner-based filtering breaks when seller == buyer.
    let object_id = get_created_object_id_by_type(&response, "AccessToken")?;
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

    // Pre-flight checks (AccessToken is a regular owned object, directly accessible)
    let fields = get_move_fields(&client, token_obj_id).await?;
    let service_name = fields["service_name"].as_str().unwrap_or("?").to_string();

    let expires_at: u64 = fields["expires_at_ms"]
        .as_str()
        .and_then(|s: &str| s.parse().ok())
        .unwrap_or(0);
    if expires_at > 0 {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis() as u64;
        if now_ms > expires_at {
            bail!("Token for '{service_name}' expired at {expires_at} ms.");
        }
    }
    println!("Redeeming token for '{service_name}'…");

    let token_ref = get_owned_object_ref(&client, token_obj_id).await?;

    let mut ptb = ProgrammableTransactionBuilder::new();
    ptb.move_call(
        ObjectID::from_str(PACKAGE_ID)?,
        Identifier::new("marketplace")?,
        Identifier::new("redeem")?,
        vec![],
        vec![
            // &mut AccessToken (owned by caller)
            CallArg::Object(ObjectArg::ImmOrOwnedObject(token_ref)),
            // &Clock (shared immutable)
            CallArg::Object(ObjectArg::SharedObject {
                id: ObjectID::from_str(CLOCK_ID)?,
                initial_shared_version: SequenceNumber::from(CLOCK_INITIAL_SHARED_VERSION),
                mutability: SharedObjectMutability::Immutable,
            }),
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
