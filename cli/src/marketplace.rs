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
use sui_transaction_builder::{Argument, Function, ObjectInput, TransactionBuilder};

use crate::config::{load_config, MarketConfig};
use crate::models::{AccessToken, MarketplaceObject, ServiceListing};
use crate::utils::Wallet;

// ─────────────────────────────────────────────────────────────────────────────
// CallArg — describes a single Move call argument as data
// ─────────────────────────────────────────────────────────────────────────────

enum CallArg {
    Object(Address),  // builder.object(ObjectInput::new(addr))
    Gas,              // builder.gas()
    U64(u64),         // builder.pure(&v)  — Move u64
    Bytes(Vec<u8>),   // builder.pure(&v)  — Move vector<u8>
    Id([u8; 32]),     // builder.pure(&v)  — Move ID / address
}

// ─────────────────────────────────────────────────────────────────────────────
// MarketplaceClient
// ─────────────────────────────────────────────────────────────────────────────

pub struct MarketplaceClient {
    cfg:    MarketConfig,
    wallet: Wallet,
    client: Client,
}

impl MarketplaceClient {
    pub fn new() -> Result<Self> {
        let cfg    = load_config()?;
        let wallet = crate::utils::load_wallet(&cfg)?;
        let url    = cfg.sui.rpc_url.as_str();
        let client = Client::new(url)
            .map_err(|e| anyhow::anyhow!("Cannot connect to {url}: {e}"))?;
        Ok(Self { cfg, wallet, client })
    }

    // ── Config helpers ───────────────────────────────────────────────────────

    fn coin_type_tag(&self) -> Result<TypeTag> {
        self.cfg.marketplace.coin_type
            .parse()
            .context("Invalid coin_type in config")
    }

    // ── Object fetch helpers ─────────────────────────────────────────────────

    async fn get_object_bcs<T: DeserializeOwned>(&mut self, id: Address) -> Result<T> {
        let resp = self.client
            .ledger_client()
            .get_object(
                GetObjectRequest::default()
                    .with_object_id(id.to_string())
                    .with_read_mask(FieldMask::from_str("contents")),
            )
            .await
            .context("get_object RPC failed")?
            .into_inner();
        let obj   = resp.object_opt().context("Object not found")?;
        let bytes = obj.contents().value_opt().context("No BCS contents in object")?;
        bcs::from_bytes(bytes).context("BCS deserialisation failed")
    }

    async fn get_bag_id(&mut self) -> Result<Address> {
        let mp: MarketplaceObject =
            self.get_object_bcs(self.cfg.marketplace.marketplace_id).await?;
        Ok(Address::new(mp.listings.id))
    }

    async fn list_listing_ids(&self, bag_id: Address) -> Result<Vec<Address>> {
        let stream = self.client.list_dynamic_fields(
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

    // ── Transaction helpers ──────────────────────────────────────────────────

    async fn execute(
        &mut self,
        fn_name:   &str,
        type_args: Vec<TypeTag>,
        call_args: Vec<CallArg>,
    ) -> Result<TransactionEffects> {
        let mut builder = TransactionBuilder::new();
        builder.set_sender(self.wallet.address);
        builder.set_gas_budget(self.cfg.sui.gas_budget);

        let args: Vec<Argument> = call_args.into_iter().map(|a| match a {
            CallArg::Object(addr) => builder.object(ObjectInput::new(addr)),
            CallArg::Gas          => builder.gas(),
            CallArg::U64(v)       => builder.pure(&v),
            CallArg::Bytes(v)     => builder.pure(&v),
            CallArg::Id(v)        => builder.pure(&v),
        }).collect();

        builder.move_call(
            Function::new(
                self.cfg.marketplace.package_id,
                Identifier::new("marketplace").unwrap(),
                Identifier::new(fn_name).unwrap(),
            )
            .with_type_args(type_args),
            args,
        );

        let tx = builder
            .build(&mut self.client)
            .await
            .map_err(|e| anyhow::anyhow!("Transaction build failed: {e:?}"))?;

        let sig = self.wallet.keypair
            .sign_transaction(&tx)
            .map_err(|e| anyhow::anyhow!("Signing failed: {e}"))?;

        let resp = self.client
            .execute_transaction_and_wait_for_checkpoint(
                ExecuteTransactionRequest::new(tx.into())
                    .with_signatures(vec![sig.into()]),
                Duration::from_secs(60),
            )
            .await
            .map_err(|e| anyhow::anyhow!("Transaction execution failed: {e:?}"))?
            .into_inner();

        let executed = resp.transaction.context("No transaction in response")?;
        let effects  = executed.effects.context("No effects in response")?;

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

    // ── Public API ───────────────────────────────────────────────────────────

    pub async fn create_marketplace(&mut self) -> Result<Address> {
        let type_args = vec![self.coin_type_tag()?];
        let effects = self.execute("create_marketplace", type_args, vec![]).await?;
        let mp_id   = find_created(&effects, "::marketplace::Marketplace")?;

        println!("Marketplace created!");
        println!("  Coin type:      {}", self.cfg.marketplace.coin_type);
        println!("  Marketplace ID: {mp_id}");
        println!("  Set marketplace_id = \"{mp_id}\" in market-config.toml");
        Ok(mp_id)
    }

    pub async fn create_listing(
        &mut self,
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
        let price_mist = (price_sui * 1_000_000_000.0) as u64;
        let mp_id      = self.cfg.marketplace.marketplace_id;
        let type_args  = vec![self.coin_type_tag()?];
        let effects = self.execute("create_listing", type_args, vec![
            CallArg::Object(mp_id),
            CallArg::Bytes(name.into_bytes()),
            CallArg::Bytes(ip_address.into_bytes()),
            CallArg::U64(price_mist),
            CallArg::U64(valid_from),
            CallArg::U64(expires_at),
            CallArg::U64(max_bandwidth),
            CallArg::U64(min_bandwidth),
            CallArg::U64(min_duration),
            CallArg::U64(bw_granularity),
            CallArg::U64(time_granularity),
        ]).await?;
        let listing_id = find_created(&effects, "::marketplace::ServiceListing")?;

        println!("Listing created!");
        println!("  Listing:        {listing_id}");
        println!("  Price:          {price_sui} SUI");
        println!("  Valid from:     {valid_from} s");
        println!("  Expires at:     {expires_at} s");
        println!("  Max BW:         {max_bandwidth} kB/s");
        println!("  Min BW:         {min_bandwidth} kB/s");
        println!("  Min duration:   {min_duration} s");
        println!("  BW granularity: {bw_granularity} kB/s");
        println!("  Time gran.:     {time_granularity} s");
        Ok(listing_id)
    }

    pub async fn get_listings(&mut self, limit: u32) -> Result<()> {
        let bag_id = self.get_bag_id().await?;
        let mut ids = self.list_listing_ids(bag_id).await?;
        ids.truncate(limit as usize);

        if ids.is_empty() {
            println!("No listings found.");
            return Ok(());
        }

        let mut listings: Vec<(Address, ServiceListing)> = Vec::new();
        for id in ids {
            match self.get_object_bcs::<ServiceListing>(id).await {
                Ok(l)  => listings.push((id, l)),
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

    pub async fn search_listings(
        &mut self,
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

        let bag_id = self.get_bag_id().await?;
        let ids    = self.list_listing_ids(bag_id).await?;

        if ids.is_empty() {
            println!("No listings found.");
            return Ok(());
        }

        struct Row {
            id:         Address,
            name:       String,
            price_mist: u64,
            ip_address: String,
            valid_from: u64,
            expires_at: u64,
            bandwidth:  u64,
        }

        let mut rows: Vec<Row> = Vec::new();
        for id in ids {
            let l = match self.get_object_bcs::<ServiceListing>(id).await {
                Ok(l)  => l,
                Err(e) => { eprintln!("Warning: {e}"); continue; }
            };

            let host = l.ip_address.rsplit_once(':').map(|(h, _)| h).unwrap_or(&l.ip_address);
            if let Ok(ip) = IpAddr::from_str(host) {
                if !filter_net.contains(&ip) { continue; }
            }
            if min_bandwidth > 0 && l.token.bandwidth < min_bandwidth { continue; }
            if start > 0 && l.token.valid_from > start { continue; }
            if end   > 0 && l.token.expires_at < end   { continue; }

            rows.push(Row {
                id,
                name:       l.name.clone(),
                price_mist: l.price_mist,
                ip_address: l.ip_address.clone(),
                valid_from: l.token.valid_from,
                expires_at: l.token.expires_at,
                bandwidth:  l.token.bandwidth,
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

    pub async fn get_listing(&mut self, listing_id: &str) -> Result<ServiceListing> {
        let id: Address = listing_id.parse().context("Invalid listing ID")?;
        self.get_object_bcs(id).await
    }

    pub async fn buy_listing(
        &mut self,
        listing_id: String,
        start: u64,
        end: u64,
        bandwidth: u64,
    ) -> Result<Address> {
        let listing_obj_id: Address = listing_id.parse().context("Invalid listing ID")?;

        anyhow::ensure!(start > 0,     "start must be non-zero; resolve to current time before calling buy_listing");
        anyhow::ensure!(end > start,   "end must be greater than start");
        anyhow::ensure!(bandwidth > 0, "bandwidth must be non-zero; resolve to seller's bound before calling buy_listing");

        let listing: ServiceListing = self.get_object_bcs(listing_obj_id).await?;
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

        let mp_id     = self.cfg.marketplace.marketplace_id;
        let id_bytes  = listing_obj_id.into_inner();
        let type_args = vec![self.coin_type_tag()?];
        let effects = self.execute("purchase", type_args, vec![
            CallArg::Object(mp_id),
            CallArg::Id(id_bytes),
            CallArg::Gas,
            CallArg::U64(start),
            CallArg::U64(end),
            CallArg::U64(bandwidth),
        ]).await?;
        let token_id = find_access_token(&effects)?;

        println!("Access token minted!");
        println!("  Listing:  {listing_id}");
        println!("  Token ID: {token_id}");
        Ok(token_id)
    }

    pub async fn redeem(&mut self, token_id: String, ip_address: String) -> Result<()> {
        let token_addr: Address = token_id.parse().context("Invalid token ID")?;

        let token: AccessToken = self.get_object_bcs(token_addr).await?;
        let service_name = token.service_name.clone();
        println!("Redeeming token for '{service_name}'…");

        self.execute("redeem", vec![], vec![
            CallArg::Object(token_addr),
            CallArg::Bytes(ip_address.into_bytes()),
        ]).await?;

        println!("Token redeemed! TokenRedeemed event emitted on-chain.");
        println!("  Service: {service_name}");
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Pure helpers (no client state needed)
// ─────────────────────────────────────────────────────────────────────────────

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
