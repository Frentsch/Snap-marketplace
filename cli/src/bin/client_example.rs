/// client — buys a marketplace listing, redeems the access token, then
/// connects to the service address in metadata_uri and prints the response.
use anyhow::Result;
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

    // 1. Fetch server address BEFORE buying — purchase removes the listing.
    println!("Fetching listing metadata…");
    let server_addr = cli::marketplace::get_ip_address(&args.listing_id).await?;
    println!("Service address: {server_addr}");

    // 2. Buy the listing → receive an AccessToken.
    println!("Buying listing {}…", args.listing_id);
    let token_id = cli::marketplace::buy_listing(args.listing_id.clone(), 0, 0).await?;

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
