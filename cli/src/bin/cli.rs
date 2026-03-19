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
    /// Create a new service listing on the marketplace
    CreateListing {
        /// Human-readable service name
        #[arg(long)]
        name: String,

        /// URI pointing to off-chain metadata (IPFS, HTTPS, etc.)
        #[arg(long)]
        ip_address: String,

        /// Price per access grant in SUI (e.g. 0.5)
        #[arg(long)]
        price_sui: f64,

        /// Access duration in hours; use 0 for perpetual
        #[arg(long, default_value = "0")]
        duration_hours: f64,
    },

    /// Display existing service listings
    GetListings {
        /// Maximum number of listings to show
        #[arg(long, default_value = "20")]
        limit: u32,
    },

    /// Purchase a service listing
    BuyListing {
        /// Object ID of the ServiceListing to purchase
        listing_id: String,

        /// Datetime when access begins, "YYYY-MM-DD HH:MM:SS" UTC (default: now)
        #[arg(long)]
        start_date: Option<String>,

        /// Datetime when access expires, "YYYY-MM-DD HH:MM:SS" UTC (default: derived from listing duration)
        #[arg(long)]
        end_date: Option<String>,
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
        Commands::CreateListing { name, ip_address, price_sui, duration_hours } => {
            marketplace::create_listing(name, ip_address, price_sui, duration_hours).await?;
            // listing ID is printed inside create_listing
        }

        Commands::GetListings { limit } => {
            marketplace::get_listings(limit).await?;
        }

        Commands::BuyListing { listing_id, start_date, end_date } => {
            let start_ms = start_date.as_deref().map(parse_date_ms).transpose()?.unwrap_or(0);
            let end_ms   = end_date.as_deref().map(parse_date_ms).transpose()?.unwrap_or(0);
            marketplace::buy_listing(listing_id, start_ms, end_ms).await?;
        }

        Commands::Redeem { token_id, ip_address } => {
            marketplace::redeem(token_id, ip_address).await?;
        }

        Commands::GetWallet {} => {
            let mut w = utils::get_wallet().await?;
            println!("Active address: {}", w.active_address()?);
        }
    }

    Ok(())
}
