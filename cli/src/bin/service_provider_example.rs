/// service_provider — registers a listing on the marketplace, subscribes to
/// on-chain checkpoints for TokenRedeemed events, and gates TCP connections
/// by the redeemer's IP.
use anyhow::{Context, Result};
use clap::Parser;
use futures::StreamExt;
use std::{collections::HashSet, sync::Arc};
use sui_rpc::{Client, proto::sui::rpc::v2::SubscribeCheckpointsRequest};
use sui_sdk_types::Address;
use tokio::{io::AsyncWriteExt, net::TcpListener, sync::RwLock};

use cli::models::TokenRedeemed;

// ─────────────────────────────────────────────────────────────────────────────
// CLI args
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "service-provider", about = "Marketplace service provider server")]
struct Args {
    /// Human-readable service name shown in the marketplace listing
    #[arg(long, default_value = "Test Server")]
    name: String,

    /// Price per access grant in SUI (e.g. 0.1)
    #[arg(long, default_value = "0.01")]
    price_sui: f64,

    /// Earliest time buyers may set as their start, as Unix ms timestamp; 0 = now (default)
    #[arg(long, default_value = "0")]
    valid_from_ms: u64,

    /// Latest time buyers may set as their end, as Unix ms timestamp; 0 = now+1h (default)
    #[arg(long, default_value = "0")]
    expires_at_ms: u64,

    /// Maximum bandwidth in bytes per second buyers may request
    #[arg(long, default_value = "10000")]
    max_bandwidth_bps: u64,

    /// Minimum bandwidth buyers must purchase in B/s
    #[arg(long, default_value = "1000")]
    min_bandwidth_bps: u64,

    /// Minimum duration buyers must purchase in ms
    #[arg(long, default_value = "1000")]
    min_duration_ms: u64,

    /// Bandwidth granularity in B/s
    #[arg(long, default_value = "1000")]
    bw_granularity: u64,

    /// Time granularity in ms
    #[arg(long, default_value = "10000")]
    time_granularity: u64,

    /// TCP address to listen on for authorization checks
    #[arg(long, default_value = "127.0.0.1:8080")]
    listen: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// Checkpoint subscription event loop
// ─────────────────────────────────────────────────────────────────────────────

async fn event_loop(
    rpc_url: String,
    issuer_address: Address,
    authorized: Arc<RwLock<HashSet<String>>>,
) -> Result<()> {
    let mut client = Client::new(rpc_url.as_str())
        .map_err(|e| anyhow::anyhow!("Cannot connect to {rpc_url}: {e}"))?;

    let mut stream = client
        .subscription_client()
        .subscribe_checkpoints(SubscribeCheckpointsRequest::default())
        .await
        .context("subscribe_checkpoints failed")?
        .into_inner();

    println!("Subscribed to checkpoints — watching for redemptions issued by {issuer_address}");

    while let Some(item) = stream.next().await {
        let response = item.context("Checkpoint stream error")?;
        let checkpoint = match response.checkpoint {
            Some(c) => c,
            None => continue,
        };

        for tx in &checkpoint.transactions {
            let tx_events = match tx.events.as_ref() {
                Some(e) => e,
                None => continue,
            };

            for event in &tx_events.events {
                let event_type = match event.event_type.as_deref() {
                    Some(t) => t,
                    None => continue,
                };
                if !event_type.contains("::marketplace::TokenRedeemed") {
                    continue;
                }

                let bcs_bytes = match event.contents.as_ref().and_then(|b| b.value.as_ref()) {
                    Some(b) => b,
                    None => {
                        eprintln!("Warning: TokenRedeemed event has no BCS contents");
                        continue;
                    }
                };

                match bcs::from_bytes::<TokenRedeemed>(bcs_bytes.as_ref()) {
                    Ok(redeemed) if Address::new(redeemed.issuer) == issuer_address => {
                        println!(
                            "Redemption received — authorizing IP: {}  ({} bps, {} → {})",
                            redeemed.ip_address,
                            redeemed.bandwidth_bps,
                            redeemed.valid_from_ms,
                            redeemed.expires_at_ms,
                        );
                        authorized.write().await.insert(redeemed.ip_address.clone());
                    }
                    Ok(_) => {} // different issuer — not our token
                    Err(e) => eprintln!("Warning: failed to deserialize TokenRedeemed: {e}"),
                }
            }
        }
    }

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// TCP listener loop
// ─────────────────────────────────────────────────────────────────────────────

async fn tcp_listener_loop(
    addr: &str,
    authorized: Arc<RwLock<HashSet<String>>>,
) -> Result<()> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("Failed to bind to {addr}"))?;
    println!("Listening for authorization checks on {addr}");

    loop {
        let (mut stream, peer) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => { eprintln!("Accept error: {e}"); continue; }
        };
        let ip = peer.ip().to_string();
        let auth = authorized.clone();
        println!("Connection from {ip}");
        tokio::spawn(async move {
            let reply = if auth.read().await.contains(&ip) {
                "authorized\n"
            } else {
                "unauthorized\n"
            };
            if let Err(e) = stream.write_all(reply.as_bytes()).await {
                eprintln!("Write error for {ip}: {e}");
                return;
            }
            let _ = stream.shutdown().await;
        });
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Entry point
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // 1. Resolve 0 → now / now+1h for valid_from_ms / expires_at_ms.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("System clock before UNIX epoch")?
        .as_millis() as u64;
    let valid_from_ms = if args.valid_from_ms == 0 { now_ms } else { args.valid_from_ms };
    let expires_at_ms = if args.expires_at_ms == 0 { now_ms + 3_600_000 } else { args.expires_at_ms };
    let duration_ms   = expires_at_ms - valid_from_ms;

    // Resolve 0 → tenth of max for constraint fields.
    let min_bandwidth_bps = if args.min_bandwidth_bps == 0 { args.max_bandwidth_bps / 10 } else { args.min_bandwidth_bps };
    let bw_granularity    = if args.bw_granularity    == 0 { args.max_bandwidth_bps / 10 } else { args.bw_granularity };
    let min_duration_ms   = if args.min_duration_ms   == 0 { duration_ms / 10 }            else { args.min_duration_ms };
    let time_granularity  = if args.time_granularity  == 0 { duration_ms / 10 }            else { args.time_granularity };

    // 2. Load wallet — we need the address and RPC URL.
    let mut wallet = cli::utils::get_wallet().await?;
    let issuer_address = wallet.active_address()?;
    let rpc_url = wallet.rpc_url.clone();

    // 3. Create the marketplace listing with ip_address = listen address.
    println!(
        "Creating listing '{}' at {} ({} bps, {} → {})",
        args.name, args.listen, args.max_bandwidth_bps, valid_from_ms, expires_at_ms
    );
    let listing_id = cli::marketplace::create_listing(
        args.name.clone(),
        args.listen.clone(),
        args.price_sui,
        valid_from_ms,
        expires_at_ms,
        args.max_bandwidth_bps,
        min_bandwidth_bps,
        min_duration_ms,
        bw_granularity,
        time_granularity,
    )
    .await?;
    println!("Listing ID: {listing_id}");

    // 4. Shared authorized-IP set.
    let authorized: Arc<RwLock<HashSet<String>>> = Arc::new(RwLock::new(HashSet::new()));

    // 5. Spawn checkpoint subscription in the background.
    tokio::spawn(event_loop(rpc_url, issuer_address, authorized.clone()));

    // 6. Run TCP listener (blocks until error).
    tcp_listener_loop(&args.listen, authorized).await
}
