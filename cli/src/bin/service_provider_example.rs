/// service_provider — registers a listing on the marketplace, polls for
/// TokenRedeemed events, and gates TCP connections by the redeemer's IP.
use anyhow::{Context, Result};
use futures::stream::StreamExt;
use clap::Parser;
use move_core_types::identifier::Identifier;
use std::{collections::HashSet, str::FromStr, sync::Arc};
use sui_sdk::{rpc_types::EventFilter, SuiClient, SuiClientBuilder};
use sui_types::base_types::ObjectID;
use tokio::{io::AsyncWriteExt, net::TcpListener, sync::RwLock, time::{sleep, Duration}};

// Mirror the constant from marketplace.rs — keep in sync if the package is re-deployed.
const PACKAGE_ID: &str = "0x7f1c4d5a5a48ec94025deebcff0c570ab086409a4cfd52f4381a1aa08bb7d096";

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
    #[arg(long, default_value = "0.1")]
    price_sui: f64,

    /// Access duration in hours; 0 = perpetual (default)
    #[arg(long, default_value = "0")]
    duration_hours: f64,

    /// TCP address to listen on for authorization checks
    #[arg(long, default_value = "127.0.0.1:8080")]
    listen: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// Event polling loop
// ─────────────────────────────────────────────────────────────────────────────

async fn event_loop(
    client: SuiClient,
    listing_id: ObjectID,
    authorized: Arc<RwLock<HashSet<String>>>,
) -> Result<()>{
    let listing_id_hex = listing_id.to_string();

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

    println!("Polling marketplace events every 3s, watching listing {listing_id_hex}");

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
                    if j["listing_id"].as_str().unwrap_or("") != listing_id_hex {
                        continue;
                    }
                    if let Some(ip) = j["ip_address"].as_str() {
                        println!("Redemption received — authorizing IP: {ip}");
                        authorized.write().await.insert(ip.to_string());
                    }
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

    // 1. Create the marketplace listing with ip_address = listen address.
    println!("Creating marketplace listing '{}'…", args.name);
    let listing_id = cli::marketplace::create_listing(
        args.name.clone(),
        args.listen.clone(),
        args.price_sui,
        args.duration_hours,
    )
    .await?;
    println!("Listing ID: {listing_id}");

    // 2. Build an HTTP Sui client for event polling (no WebSocket needed).
    let wallet = cli::utils::get_wallet().await?;
    let env = wallet.get_active_env()?;
    let client = SuiClientBuilder::default()
        .build(&env.rpc)
        .await
        .context("Failed to build Sui client")?;

    // 3. Shared authorized-IP set.
    let authorized: Arc<RwLock<HashSet<String>>> = Arc::new(RwLock::new(HashSet::new()));

    // 4. Spawn event polling in the background.
    tokio::spawn(event_loop(client, listing_id, authorized.clone()));

    // 5. Run TCP listener (blocks until error).
    tcp_listener_loop(&args.listen, authorized).await
}
