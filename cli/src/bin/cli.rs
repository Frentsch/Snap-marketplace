use cli::config;
use cli::marketplace;
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

        /// Earliest time buyers may set as their start, as Unix ms timestamp
        #[arg(long, default_value = "0")]
        valid_from_ms: u64,

        /// Latest time buyers may set as their end, as Unix ms timestamp
        #[arg(long, default_value = "0")]
        expires_at_ms: u64,

        /// Maximum bandwidth in bytes per second buyers may request
        #[arg(long, default_value = "1000000")]
        max_bandwidth_bps: u64,

        /// Minimum bandwidth buyers must purchase in B/s
        #[arg(long, default_value = "1000")]
        min_bandwidth_bps: u64,

        /// Minimum duration buyers must purchase in ms
        #[arg(long, default_value = "1000")]
        min_duration_ms: u64,

        /// Bandwidth granularity — purchased bandwidth must be a multiple of this
        #[arg(long, default_value = "1000")]
        bw_granularity: u64,

        /// Time granularity — purchased duration must be a multiple of this in ms
        #[arg(long, default_value = "1000")]
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

        /// Only show listings offering at least this bandwidth in B/s (0 = any)
        #[arg(long, default_value = "0")]
        bandwidth_bps: u64,

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

        /// Desired bandwidth in bytes per second; 0 = keep seller's max (default: 0)
        #[arg(long, default_value = "0")]
        bandwidth_bps: u64,
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

/// Parse "YYYY-MM-DD HH:MM:SS" (UTC) into a Unix timestamp in milliseconds.
/// Also accepts the date-only form "YYYY-MM-DD", treating it as 00:00:00 UTC.
fn parse_date_ms(s: &str) -> Result<u64> {
    let dt = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
        .or_else(|_| {
            chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
                .map(|d| d.and_hms_opt(0, 0, 0).unwrap())
        })
        .with_context(|| {
            format!("invalid datetime '{s}' — expected \"YYYY-MM-DD HH:MM:SS\" or \"YYYY-MM-DD\"")
        })?;
    Ok(dt.and_utc().timestamp_millis() as u64)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::CreateMarketplace {} => {
            marketplace::create_marketplace().await?;
        }

        Commands::CreateListing { name, ip_address, price_sui, valid_from_ms, expires_at_ms, max_bandwidth_bps, min_bandwidth_bps, min_duration_ms, bw_granularity, time_granularity } => {
            marketplace::create_listing(name, ip_address, price_sui, valid_from_ms, expires_at_ms, max_bandwidth_bps, min_bandwidth_bps, min_duration_ms, bw_granularity, time_granularity).await?;
            // listing ID is printed inside create_listing
        }

        Commands::GetListings { limit } => {
            marketplace::get_listings(limit).await?;
        }

        Commands::SearchListings { subnet, bandwidth_bps, start_date, end_date } => {
            let start_ms = start_date.as_deref().map(parse_date_ms).transpose()?.unwrap_or(0);
            let end_ms   = end_date.as_deref().map(parse_date_ms).transpose()?.unwrap_or(0);
            marketplace::search_listings(&subnet, bandwidth_bps, start_ms, end_ms).await?;
        }

        Commands::BuyListing { listing_id, start_date, end_date, bandwidth_bps } => {
            let start_ms_given = start_date.as_deref().map(parse_date_ms).transpose()?;
            let end_ms_given   = end_date.as_deref().map(parse_date_ms).transpose()?;

            // Always fetch the listing — needed for defaults and granularity alignment.
            let listing = marketplace::get_listing(&listing_id).await?;
            let now_ms  = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .context("System clock before UNIX epoch")?
                .as_millis() as u64;

            let start_ms  = start_ms_given.unwrap_or(now_ms);
            let end_ms_raw = end_ms_given.unwrap_or(listing.token.expires_at_ms);
            let bw_raw    = if bandwidth_bps == 0 { listing.token.bandwidth_bps } else { bandwidth_bps };

            // Align duration down to the nearest multiple of time_granularity.
            let duration_ms = end_ms_raw.saturating_sub(start_ms);
            let end_ms = if listing.time_granularity > 0 {
                start_ms + (duration_ms / listing.time_granularity) * listing.time_granularity
            } else {
                end_ms_raw
            };

            // Align bandwidth down to the nearest multiple of bw_granularity.
            let bandwidth_bps = if listing.bw_granularity > 0 {
                (bw_raw / listing.bw_granularity) * listing.bw_granularity
            } else {
                bw_raw
            };

            marketplace::buy_listing(listing_id, start_ms, end_ms, bandwidth_bps).await?;
        }

        Commands::Redeem { token_id, ip_address } => {
            marketplace::redeem(token_id, ip_address).await?;
        }

        Commands::GetWallet {} => {
            let cfg = config::load_config()?;
            let w = utils::load_wallet(&cfg)?;
            println!("Active address: {}", w.active_address());
        }
    }

    Ok(())
}
