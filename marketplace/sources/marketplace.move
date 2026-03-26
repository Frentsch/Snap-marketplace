/// marketplace.move
///
/// A marketplace where sellers create service listings and buyers purchase
/// AccessToken NFTs. Buyers later redeem tokens to prove access to off-chain
/// services. Redemption destroys the token on-chain as immutable proof of use.
///
/// All listings are stored inside a shared Marketplace<COIN> object via an
/// ObjectBag. Multiple marketplace instances can coexist for different coin
/// types. create_listing pre-mints an AccessToken inside the listing;
/// purchase mutates the token in-place within the seller's bounds and extracts
/// it — no new object is created on purchase. Sellers may delist a listing
/// before it is purchased (which also deletes the wrapped token).
module marketplace::marketplace;

    // =========================================================
    // Imports
    // =========================================================
    use sui::coin::{Self, Coin};
    use sui::event;
    use sui::clock::{Self, Clock};
    use sui::object_bag::{Self, ObjectBag};
    use std::string::{Self, String};

    // =========================================================
    // Error codes
    // =========================================================
    const EInsufficientPayment:  u64 = 0;
    const EListingNotActive:     u64 = 1;
    const ENotSeller:            u64 = 2;
    const ETokenInvalid:         u64 = 3; // reserved
    const EInvalidInterval:      u64 = 4;
    const EInvalidBandwidth:     u64 = 5;
    const EInvalidMinBandwidth:  u64 = 6;
    const EInvalidGranularity:   u64 = 7;

    // =========================================================
    // Core objects
    // =========================================================

    /// Shared marketplace registry keyed on coin type.
    /// All ServiceListings live inside its ObjectBag.
    /// Multiple instances may exist — one per accepted currency.
    public struct Marketplace<phantom COIN> has key {
        id: UID,
        listings: ObjectBag,
    }

    /// A service listing stored inside the Marketplace ObjectBag.
    /// Contains a pre-minted AccessToken whose valid_from_ms / expires_at_ms
    /// represent the seller's upper bounds. The buyer narrows these in-place
    /// at purchase time; the token is then extracted and transferred.
    public struct ServiceListing has key, store {
        id: UID,
        /// Address of the issuer (seller) — receives payment on purchase
        issuer: address,
        /// Human-readable service name
        name: String,
        /// IP address or host:port of the service endpoint
        ip_address: String,
        /// Price per access grant, in the coin's base unit
        price_mist: u64,
        /// Whether the listing is accepting new purchases
        is_active: bool,
        /// Minimum bandwidth buyers must purchase; 0 = no minimum
        min_bandwidth_bps: u64,
        /// Minimum duration buyers must purchase in ms; 0 = no minimum
        min_duration_ms: u64,
        /// Bandwidth must be a multiple of this value; 0 = any value
        bw_granularity: u64,
        /// Duration must be a multiple of this value in ms; 0 = any value
        time_granularity: u64,
        /// Pre-minted token; valid_from_ms / expires_at_ms are seller's bounds
        token: AccessToken,
    }

    /// An access token NFT owned by the buyer.
    /// Redemption destroys the token — the on-chain deletion is the proof of use.
    public struct AccessToken has key, store {
        id: UID,
        /// The listing this token grants access to
        listing_id: ID,
        /// Copied from the listing at purchase time (convenience)
        service_name: String,
        /// IP address or host:port of the service endpoint (copied from listing)
        ip_address: String,
        /// Unix timestamp (ms) from which the token is valid
        valid_from_ms: u64,
        /// Unix timestamp (ms) after which the token is expired
        expires_at_ms: u64,
        /// Maximum bandwidth in bytes per second
        bandwidth_bps: u64,
        /// Issuer (seller) address at time of purchase
        issuer: address,
    }

    // =========================================================
    // Events
    // =========================================================

    public struct TokenRedeemed has copy, drop {
        token_id: ID,
        issuer: address,
        redeemed_by: address,
        ip_address: String,
        valid_from_ms: u64,
        expires_at_ms: u64,
        bandwidth_bps: u64,
    }

    // =========================================================
    // Marketplace creation
    // =========================================================

    /// Create a new Marketplace for a specific coin type and share it.
    /// Anyone may create a marketplace; each call produces a distinct shared object.
    public entry fun create_marketplace<COIN>(ctx: &mut TxContext) {
        transfer::share_object(Marketplace<COIN> {
            id: object::new(ctx),
            listings: object_bag::new(ctx),
        });
    }

    // =========================================================
    // Seller functions
    // =========================================================

    /// Create a new service listing with a pre-minted AccessToken and store it
    /// in the Marketplace bag.
    ///
    /// `valid_from_ms`: seller's "not before" bound for buyers; 0 = no restriction.
    /// `expires_at_ms`: seller's "not after" bound for buyers; 0 = no expiry.
    public entry fun create_listing<COIN>(
        marketplace: &mut Marketplace<COIN>,
        name: vector<u8>,
        ip_address: vector<u8>,
        price_mist: u64,
        valid_from_ms: u64,
        expires_at_ms: u64,
        max_bandwidth_bps: u64,
        min_bandwidth_bps: u64,
        min_duration_ms: u64,
        bw_granularity: u64,
        time_granularity: u64,
        ctx: &mut TxContext,
    ): ID {
        // Validate granularity and minimum constraints against the seller's own bounds.
        // All constraint values must be non-zero; the client is responsible for resolving defaults.
        let total_duration = expires_at_ms - valid_from_ms;
        assert!(min_bandwidth_bps != 0 && max_bandwidth_bps != 0 && min_bandwidth_bps <= max_bandwidth_bps, EInvalidMinBandwidth);
        assert!(bw_granularity != 0 && max_bandwidth_bps % bw_granularity == 0, EInvalidGranularity);
        assert!(time_granularity != 0 && total_duration % time_granularity == 0,                            EInvalidGranularity);
        assert!(min_duration_ms != 0 && min_duration_ms <= total_duration,                                  EInvalidInterval);

        let listing_uid = object::new(ctx);
        let listing_id  = object::uid_to_inner(&listing_uid);

        let token = AccessToken {
            id: object::new(ctx),
            listing_id,
            service_name: string::utf8(name),
            ip_address:   string::utf8(ip_address),
            valid_from_ms,
            expires_at_ms,
            bandwidth_bps: max_bandwidth_bps,
            issuer: ctx.sender(),
        };

        let listing = ServiceListing {
            id: listing_uid,
            issuer: ctx.sender(),
            name: string::utf8(name),
            ip_address: string::utf8(ip_address),
            price_mist,
            is_active: true,
            min_bandwidth_bps,
            min_duration_ms,
            bw_granularity,
            time_granularity,
            token,
        };

        let id = object::id(&listing);
        object_bag::add(&mut marketplace.listings, id, listing);
        id
    }

    /// Remove a listing from the Marketplace bag before it is purchased.
    /// Also deletes the wrapped AccessToken. Only the original seller may call this.
    public entry fun delist<COIN>(
        marketplace: &mut Marketplace<COIN>,
        listing_id: ID,
        ctx: &TxContext,
    ) {
        let listing = object_bag::borrow<ID, ServiceListing>(&marketplace.listings, listing_id);
        assert!(listing.issuer == ctx.sender(), ENotSeller);

        let listing = object_bag::remove<ID, ServiceListing>(&mut marketplace.listings, listing_id);
        let ServiceListing {
            id,
            issuer: _,
            name: _,
            ip_address: _,
            price_mist: _,
            is_active: _,
            min_bandwidth_bps: _,
            min_duration_ms: _,
            bw_granularity: _,
            time_granularity: _,
            token,
        } = listing;
        let AccessToken {
            id: token_id,
            listing_id: _,
            service_name: _,
            ip_address: _,
            valid_from_ms: _,
            expires_at_ms: _,
            bandwidth_bps: _,
            issuer: _,
        } = token;
        object::delete(token_id);
        object::delete(id);
    }

    /// Update listing price or active status. Only the original seller may call this.
    public entry fun update_listing<COIN>(
        marketplace: &mut Marketplace<COIN>,
        listing_id: ID,
        new_price_mist: u64,
        is_active: bool,
        ctx: &TxContext,
    ) {
        let listing = object_bag::borrow_mut<ID, ServiceListing>(&mut marketplace.listings, listing_id);
        assert!(listing.issuer == ctx.sender(), ENotSeller);
        listing.price_mist = new_price_mist;
        listing.is_active = is_active;
    }

    // =========================================================
    // Primary purchase
    // =========================================================

    /// Purchase access to a service.
    ///
    /// Removes the listing from the Marketplace bag (one-time-use).
    /// `payment` must be a Coin<COIN> with at least `listing.price_mist` base units.
    /// Exactly `price_mist` is extracted and forwarded to the seller.
    ///
    /// The pre-minted AccessToken inside the listing is mutated in-place with
    /// the buyer's desired window (clamped to the seller's bounds) and then
    /// transferred to the buyer — no new object is created.
    ///
    /// Bounds validation (performed by the contract; defaults must be resolved client-side):
    ///   - If seller set valid_from_ms > 0, start_ms must be >= that value.
    ///   - If seller set expires_at_ms > 0 and end_ms > 0, end_ms must be <= expires_at_ms.
    ///   - If seller set bandwidth_bps > 0 and bandwidth_bps arg > 0, it must be <= seller's cap.
    public entry fun purchase<COIN>(
        marketplace: &mut Marketplace<COIN>,
        listing_id: ID,
        payment: &mut Coin<COIN>,
        start_ms: u64,
        end_ms: u64,
        bandwidth_bps: u64,
        ctx: &mut TxContext,
    ): ID {
        let mut listing = object_bag::remove<ID, ServiceListing>(&mut marketplace.listings, listing_id);
        assert!(listing.is_active, EListingNotActive);
        assert!(coin::value(payment) >= listing.price_mist, EInsufficientPayment);

        // Validate buyer's values against seller's bounds.
        // valid_from_ms and expires_at_ms are always concrete timestamps;
        // the client is responsible for resolving any defaults before calling purchase.
        let seller_from  = listing.token.valid_from_ms;
        let seller_until = listing.token.expires_at_ms;
        let seller_bw    = listing.token.bandwidth_bps;
        assert!(start_ms >= seller_from,                                              EInvalidInterval);
        assert!(end_ms > start_ms && end_ms <= seller_until,                         EInvalidInterval);
        assert!(seller_bw == 0 || (bandwidth_bps > 0 && bandwidth_bps <= seller_bw), EInvalidBandwidth);

        let duration_ms = end_ms - start_ms;
        assert!(listing.min_bandwidth_bps != 0 && bandwidth_bps >= listing.min_bandwidth_bps, EInvalidBandwidth);
        assert!(listing.min_duration_ms   != 0 && duration_ms   >= listing.min_duration_ms,   EInvalidInterval);
        assert!(listing.bw_granularity    != 0 && bandwidth_bps % listing.bw_granularity == 0, EInvalidGranularity);
        assert!(listing.time_granularity  != 0 && duration_ms   % listing.time_granularity == 0, EInvalidGranularity);

        // Mutate the pre-minted token in-place with the buyer's values
        listing.token.valid_from_ms = start_ms;
        listing.token.expires_at_ms = end_ms;
        listing.token.bandwidth_bps = bandwidth_bps;

        // Extract exact payment and forward to issuer
        let seller_payment = coin::split(payment, listing.price_mist, ctx);
        transfer::public_transfer(seller_payment, listing.issuer);

        // Destructure listing, extract token, transfer to buyer
        let ServiceListing {
            id,
            issuer: _,
            name: _,
            ip_address: _,
            price_mist: _,
            is_active: _,
            min_bandwidth_bps: _,
            min_duration_ms: _,
            bw_granularity: _,
            time_granularity: _,
            token,
        } = listing;
        object::delete(id);

        let token_id = object::id(&token);
        transfer::public_transfer(token, ctx.sender());
        token_id
    }

    // =========================================================
    // Redemption
    // =========================================================

    /// Redeem a token, emitting a redemption event and destroying the token.
    ///
    /// Only the current owner can call this (the object model guarantees
    /// this — owned objects can only be passed by a transaction from their owner).
    /// The token is deleted on-chain; its deletion is the immutable proof of use.
    public entry fun redeem(
        token: AccessToken,
        ip_address: vector<u8>,
        ctx: &TxContext,
    ) {
        event::emit(TokenRedeemed {
            token_id: object::id(&token),
            issuer: token.issuer,
            redeemed_by: ctx.sender(),
            ip_address: string::utf8(ip_address),
            valid_from_ms: token.valid_from_ms,
            expires_at_ms: token.expires_at_ms,
            bandwidth_bps: token.bandwidth_bps,
        });

        let AccessToken {
            id, listing_id: _, service_name: _, ip_address: _,
            valid_from_ms: _, expires_at_ms: _, bandwidth_bps: _, issuer: _,
        } = token;
        object::delete(id);
    }

    // =========================================================
    // View helpers
    // =========================================================


    public fun listing_price(listing: &ServiceListing): u64         { listing.price_mist }
    public fun listing_active(listing: &ServiceListing): bool        { listing.is_active }
    public fun listing_issuer(listing: &ServiceListing): address     { listing.issuer }
    public fun listing_valid_from(listing: &ServiceListing): u64     { listing.token.valid_from_ms }
    public fun listing_expires_at(listing: &ServiceListing): u64     { listing.token.expires_at_ms }
    public fun listing_bandwidth_bps(listing: &ServiceListing): u64  { listing.token.bandwidth_bps }
    public fun listing_min_bandwidth(listing: &ServiceListing): u64  { listing.min_bandwidth_bps }
    public fun listing_min_duration(listing: &ServiceListing): u64   { listing.min_duration_ms }
    public fun listing_bw_granularity(listing: &ServiceListing): u64 { listing.bw_granularity }
    public fun listing_time_granularity(listing: &ServiceListing): u64 { listing.time_granularity }
    public fun token_listing_id(token: &AccessToken): ID            { token.listing_id }
    public fun token_expires_at(token: &AccessToken): u64           { token.expires_at_ms }
    public fun token_bandwidth_bps(token: &AccessToken): u64        { token.bandwidth_bps }
