/// client — buys a marketplace listing, redeems the access token, then
/// connects to the service address in metadata_uri and prints the response.
use anyhow::{Context, Result};
use clap::Parser;
use tokio::{net::TcpStream, time::{sleep, Duration}};

#[derive(Parser)]
#[command(name = "client", about = "Marketplace access client")]
struct Args {
    /// Object ID of the ServiceListing to purchase
    listing_id: String,

    /// IP address to record in the redemption event (must match your TCP peer IP)
    #[arg(long, default_value = "127.0.0.1")]
    ip: String,

    /// Desired access start time as Unix seconds timestamp; 0 = now (default)
    #[arg(long, default_value = "0")]
    start: u64,

    /// Desired access end time as Unix seconds timestamp; 0 = seller's bound (default)
    #[arg(long, default_value = "0")]
    end: u64,

    /// Desired bandwidth in kB/s; 0 = seller's bound (default)
    #[arg(long, default_value = "0")]
    bandwidth: u64,
}

async fn try_connect(addr: &str) -> Result<String> {
    use tokio::io::AsyncReadExt;
    let mut stream = TcpStream::connect(addr).await?;
    let mut response = String::new();
    stream.read_to_string(&mut response).await?;
    Ok(response)
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // 1. Fetch listing BEFORE buying — purchase removes it from the marketplace.
    //    Also resolve any 0-defaults using the seller's bounds.
    println!("Fetching listing metadata…");
    let listing = cli::marketplace::get_listing(&args.listing_id).await?;
    let server_addr = listing.ip_address.clone();
    println!("Service address: {server_addr}");

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("System clock before UNIX epoch")?
        .as_secs();

    let start   = if args.start     == 0 { now }                        else { args.start };
    let end_raw = if args.end       == 0 { listing.token.expires_at }   else { args.end };
    let bw_raw  = if args.bandwidth == 0 { listing.token.bandwidth }    else { args.bandwidth };

    // Align duration down to the nearest multiple of time_granularity.
    let duration = end_raw.saturating_sub(start);
    let end = if listing.time_granularity > 0 {
        start + (duration / listing.time_granularity) * listing.time_granularity
    } else {
        end_raw
    };

    // Align bandwidth down to the nearest multiple of bw_granularity.
    let bw = if listing.bw_granularity > 0 {
        (bw_raw / listing.bw_granularity) * listing.bw_granularity
    } else {
        bw_raw
    };
    // 2. Buy the listing → receive an AccessToken.
    println!("Buying listing {}…", args.listing_id);
    let token_id = cli::marketplace::buy_listing(args.listing_id.clone(), start, end, bw).await?;

    // 3. Redeem the token, recording our IP so the server will authorize us.
    println!("Redeeming token {token_id} with IP {}…", args.ip);
    cli::marketplace::redeem(token_id.to_string(), args.ip.clone()).await?;

    // 4. Retry until the service_provider's event poller has processed the
    //    redemption and authorized our IP (poller runs every 3s).
    println!("Waiting for server to process redemption event…");
    for attempt in 1..=10 {
        sleep(Duration::from_secs(4)).await;
        match try_connect(&server_addr).await {
            Ok(response) => {
                println!("Server says: {}", response.trim());
                if response.trim() == "authorized" {
                    break;
                }
            }
            Err(e) => eprintln!("Attempt {attempt}/10: {e}"),
        }
    }

    Ok(())
}
