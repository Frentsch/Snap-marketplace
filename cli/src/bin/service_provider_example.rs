/// service_provider — registers a listing on the marketplace, polls for
/// TokenRedeemed events, and gates TCP connections by the redeemer's IP.
use anyhow::{Context, Result};
use clap::Parser;
use move_core_types::identifier::Identifier;
use std::{collections::HashSet, str::FromStr, sync::Arc};
use sui_sdk::{rpc_types::EventFilter, SuiClient, SuiClientBuilder};
use sui_types::base_types::ObjectID;
use tokio::{io::AsyncWriteExt, net::TcpListener, sync::RwLock, time::{sleep, Duration}};

use cli::marketplace::PACKAGE_ID;

// ─────────────────────────────────────────────────────────────────────────────
// CLI args
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "service-provider", about = "Marketplace service provider server")]
struct Args {
    /// Use secondary (not active) address for local testing
    #[arg(short = 's', long )]
    secondary: bool,

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
// Event polling loop
// ─────────────────────────────────────────────────────────────────────────────

async fn event_loop(
    client: SuiClient,
    issuer: String,
    authorized: Arc<RwLock<HashSet<String>>>,
) -> Result<()>{
    let filter = match (|| -> Result<EventFilter> {
        Ok(EventFilter::MoveEventModule {
            package: ObjectID::from_str(PACKAGE_ID)?,
            module: Identifier::new("marketplace")?,
        })
    })() {
        Ok(f) => f,
        Err(e) => {
            eprintln!("Failed to build event filter: {e}");
            return Ok(());
        }
    };

    println!("Polling marketplace events every 3s, watching redemptions for issuer {issuer}");

    // Manually poll query_events with a moving cursor so we catch events
    // emitted after startup. get_events_stream only drains existing events
    // and terminates — it does not wait for future ones.
    let mut cursor = None;
    loop {
        match client.event_api().query_events(filter.clone(), cursor, Some(100), false).await {
            Ok(page) => {
                for event in &page.data {
                    if event.type_.name.as_str() != "TokenRedeemed" {
                        continue;
                    }
                    let j = &event.parsed_json;
                    if j["issuer"].as_str().unwrap_or("") != issuer {
                        continue;
                    }
                    if let Some(ip) = j["ip_address"].as_str() {
                        println!("Redemption received — authorizing IP: {ip}");
                        authorized.write().await.insert(ip.to_string());
                    }
                    println!("reserved {} bps from {} to {}", j["bandwidth_bps"],j["valid_from_ms"], j["expires_at_ms"]);
                }
                // Advance cursor if the node returned one
                if page.next_cursor.is_some() {
                    cursor = page.next_cursor;
                }
                // If we exhausted all pages, wait before polling again
                if !page.has_next_page {
                    sleep(Duration::from_secs(3)).await;
                }
            }
            Err(e) => {
                eprintln!("Event query error: {e}");
                sleep(Duration::from_secs(5)).await;
            }
        }
    }
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
        println!("received connection from {ip}");
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
            // Explicitly shut down the write half so clients (nc, curl) see EOF.
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
    let valid_from_ms  = if args.valid_from_ms  == 0 { now_ms } else { args.valid_from_ms };
    let expires_at_ms  = if args.expires_at_ms  == 0 { now_ms + 3_600_000 } else { args.expires_at_ms };
    let duration_ms    = expires_at_ms - valid_from_ms;

    // Resolve 0 → tenth of max for the constraint fields.
    let min_bandwidth_bps = if args.min_bandwidth_bps == 0 { args.max_bandwidth_bps / 10 } else { args.min_bandwidth_bps };
    let bw_granularity    = if args.bw_granularity    == 0 { args.max_bandwidth_bps / 10 } else { args.bw_granularity };
    let min_duration_ms   = if args.min_duration_ms   == 0 { duration_ms / 10 }            else { args.min_duration_ms };
    let time_granularity  = if args.time_granularity  == 0 { duration_ms / 10 }            else { args.time_granularity };

    // 2. Create the marketplace listing with ip_address = listen address.
    println!("Creating marketplace listing '{}' advertising {} bps from {} to {}", args.name, args.max_bandwidth_bps, valid_from_ms, expires_at_ms);
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

    // 3. Build an HTTP Sui client for event polling (no WebSocket needed).
    let mut wallet = cli::utils::get_wallet().await?;
    let issuer = wallet.active_address()?.to_string();
    let env = wallet.get_active_env()?;
    let client = SuiClientBuilder::default()
        .build(&env.rpc)
        .await
        .context("Failed to build Sui client")?;

    // 4. Shared authorized-IP set.
    let authorized: Arc<RwLock<HashSet<String>>> = Arc::new(RwLock::new(HashSet::new()));

    // 5. Spawn event polling in the background.
    tokio::spawn(event_loop(client, issuer, authorized.clone()));

    // 6. Run TCP listener (blocks until error).
    tcp_listener_loop(&args.listen, authorized).await
}
