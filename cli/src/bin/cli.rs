use cli::config;
use cli::marketplace::MarketplaceClient;
use cli::utils;
use anyhow::{Context, Result};
use chrono::NaiveDateTime;
use clap::{Parser, Subcommand};

/// CLI for the SUI Service Marketplace
#[derive(Parser)]
#[command(name = "marketplace-cli", version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Deploy a new Marketplace shared object for the configured coin type
    CreateMarketplace {},

    /// Create a new service listing on the marketplace
    CreateListing {
        /// Human-readable service name
        #[arg(long)]
        name: String,

        /// IP address or host:port of the service endpoint
        #[arg(long)]
        ip_address: String,

        /// Price per access grant in SUI (e.g. 0.5)
        #[arg(long)]
        price_sui: f64,

        /// Earliest time buyers may set as their start, as Unix seconds timestamp; 0 = now
        #[arg(long, default_value = "0")]
        valid_from: u64,

        /// Latest time buyers may set as their end, as Unix seconds timestamp; 0 = now+1h
        #[arg(long, default_value = "0")]
        expires_at: u64,

        /// Maximum bandwidth buyers may request in kB/s
        #[arg(long, default_value = "1000")]
        max_bandwidth: u64,

        /// Minimum bandwidth buyers must purchase in kB/s
        #[arg(long, default_value = "1")]
        min_bandwidth: u64,

        /// Minimum duration buyers must purchase in seconds
        #[arg(long, default_value = "1")]
        min_duration: u64,

        /// Bandwidth granularity — purchased bandwidth must be a multiple of this (kB/s)
        #[arg(long, default_value = "1")]
        bw_granularity: u64,

        /// Time granularity — purchased duration must be a multiple of this (seconds)
        #[arg(long, default_value = "1")]
        time_granularity: u64,
    },

    /// Display existing service listings
    GetListings {
        /// Maximum number of listings to show
        #[arg(long, default_value = "20")]
        limit: u32,
    },

    /// Search listings by subnet, bandwidth, and time window (results sorted by price)
    SearchListings {
        /// Filter to listings whose IP is contained in this subnet (CIDR notation)
        #[arg(long, default_value = "0.0.0.0/0")]
        subnet: String,

        /// Only show listings offering at least this bandwidth in kB/s (0 = any)
        #[arg(long, default_value = "0")]
        bandwidth: u64,

        /// Only show listings available at or before this datetime, "YYYY-MM-DD HH:MM:SS" UTC
        #[arg(long)]
        start_date: Option<String>,

        /// Only show listings valid at or after this datetime, "YYYY-MM-DD HH:MM:SS" UTC
        #[arg(long)]
        end_date: Option<String>,
    },

    /// Purchase a service listing
    BuyListing {
        /// Object ID of the ServiceListing to purchase
        listing_id: String,

        /// Datetime when access begins, "YYYY-MM-DD HH:MM:SS" UTC (default: now)
        #[arg(long)]
        start_date: Option<String>,

        /// Datetime when access expires, "YYYY-MM-DD HH:MM:SS" UTC (default: seller's bound)
        #[arg(long)]
        end_date: Option<String>,

        /// Desired bandwidth in kB/s; 0 = seller's max (default: 0)
        #[arg(long, default_value = "0")]
        bandwidth: u64,
    },

    /// Redeem an access token
    Redeem {
        /// Object ID of the AccessToken to redeem
        token_id: String,

        /// Client IP address to record in the redemption event
        #[arg(long)]
        ip_address: String,
    },

    /// Print the active wallet address
    GetWallet {},
}

/// Parse "YYYY-MM-DD HH:MM:SS" (UTC) into a Unix timestamp in seconds.
/// Also accepts the date-only form "YYYY-MM-DD", treating it as 00:00:00 UTC.
fn parse_date(s: &str) -> Result<u64> {
    let dt = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
        .or_else(|_| {
            chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
                .map(|d| d.and_hms_opt(0, 0, 0).unwrap())
        })
        .with_context(|| {
            format!("invalid datetime '{s}' — expected \"YYYY-MM-DD HH:MM:SS\" or \"YYYY-MM-DD\"")
        })?;
    Ok(dt.and_utc().timestamp() as u64)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::CreateMarketplace {} => {
            MarketplaceClient::new()?.create_marketplace().await?;
        }

        Commands::CreateListing { name, ip_address, price_sui, valid_from, expires_at, max_bandwidth, min_bandwidth, min_duration, bw_granularity, time_granularity } => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .context("System clock before UNIX epoch")?
                .as_secs();
            MarketplaceClient::new()?.create_listing(
                name,
                ip_address,
                price_sui,
                if valid_from == 0 { now }         else { valid_from },
                if expires_at == 0 { now + 3_600 } else { expires_at },
                max_bandwidth,
                min_bandwidth,
                min_duration,
                bw_granularity,
                time_granularity,
            ).await?;
        }

        Commands::GetListings { limit } => {
            MarketplaceClient::new()?.get_listings(limit).await?;
        }

        Commands::SearchListings { subnet, bandwidth, start_date, end_date } => {
            let start = start_date.as_deref().map(parse_date).transpose()?.unwrap_or(0);
            let end   = end_date.as_deref().map(parse_date).transpose()?.unwrap_or(0);
            MarketplaceClient::new()?.search_listings(&subnet, bandwidth, start, end).await?;
        }

        Commands::BuyListing { listing_id, start_date, end_date, bandwidth } => {
            let start_given = start_date.as_deref().map(parse_date).transpose()?;
            let end_given   = end_date.as_deref().map(parse_date).transpose()?;

            let mut mc = MarketplaceClient::new()?;

            // Fetch listing for defaults and granularity alignment.
            let listing = mc.get_listing(&listing_id).await?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .context("System clock before UNIX epoch")?
                .as_secs();

            let start   = start_given.unwrap_or(now);
            let end_raw = end_given.unwrap_or(listing.token.expires_at);
            let bw_raw  = if bandwidth == 0 { listing.token.bandwidth } else { bandwidth };

            let duration = end_raw.saturating_sub(start);
            let end = if listing.time_granularity > 0 {
                start + (duration / listing.time_granularity) * listing.time_granularity
            } else {
                end_raw
            };
            let bw = if listing.bw_granularity > 0 {
                (bw_raw / listing.bw_granularity) * listing.bw_granularity
            } else {
                bw_raw
            };

            mc.buy_listing(listing_id, start, end, bw).await?;
        }

        Commands::Redeem { token_id, ip_address } => {
            MarketplaceClient::new()?.redeem(token_id, ip_address).await?;
        }

        Commands::GetWallet {} => {
            let cfg = config::load_config()?;
            let w = utils::load_wallet(&cfg)?;
            println!("Active address: {}", w.active_address());
        }
    }

    Ok(())
}
