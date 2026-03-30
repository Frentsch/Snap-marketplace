/// marketplace.move
///
/// A marketplace where sellers create service listings and buyers purchase
/// AccessToken NFTs. Buyers later redeem tokens to prove access to off-chain
/// services. Redemption destroys the token on-chain as immutable proof of use.
///
/// Time values are Unix timestamps / durations in **seconds**.
/// Bandwidth values are in **kB/s** (kilobytes per second).
module marketplace::marketplace;

    use sui::coin::{Self, Coin};
    use sui::event;
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

    public struct Marketplace<phantom COIN> has key {
        id: UID,
        listings: ObjectBag,
    }

    public struct ServiceListing has key, store {
        id: UID,
        issuer: address,
        name: String,
        ip_address: String,
        price_mist: u64,
        is_active: bool,
        min_bandwidth: u64,  // kB/s
        min_duration: u64,   // seconds
        bw_granularity: u64, // kB/s
        time_granularity: u64, // seconds
        token: AccessToken,
    }

    public struct AccessToken has key, store {
        id: UID,
        listing_id: ID,
        service_name: String,
        ip_address: String,
        valid_from: u64,  // Unix seconds
        expires_at: u64,  // Unix seconds
        bandwidth: u64,   // kB/s
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
        valid_from: u64,
        expires_at: u64,
        bandwidth: u64,
    }

    // =========================================================
    // Marketplace creation
    // =========================================================

    public entry fun create_marketplace<COIN>(ctx: &mut TxContext) {
        transfer::share_object(Marketplace<COIN> {
            id: object::new(ctx),
            listings: object_bag::new(ctx),
        });
    }

    // =========================================================
    // Seller functions
    // =========================================================

    /// `valid_from`: seller's "not before" bound in Unix seconds; 0 = no restriction.
    /// `expires_at`: seller's "not after" bound in Unix seconds; 0 = no expiry.
    public entry fun create_listing<COIN>(
        marketplace: &mut Marketplace<COIN>,
        name: vector<u8>,
        ip_address: vector<u8>,
        price_mist: u64,
        valid_from: u64,
        expires_at: u64,
        max_bandwidth: u64,
        min_bandwidth: u64,
        min_duration: u64,
        bw_granularity: u64,
        time_granularity: u64,
        ctx: &mut TxContext,
    ): ID {
        let total_duration = expires_at - valid_from;
        assert!(min_bandwidth != 0 && max_bandwidth != 0 && min_bandwidth <= max_bandwidth, EInvalidMinBandwidth);
        assert!(bw_granularity   != 0 && max_bandwidth   % bw_granularity   == 0, EInvalidGranularity);
        assert!(time_granularity != 0 && total_duration  % time_granularity == 0, EInvalidGranularity);
        assert!(min_duration     != 0 && min_duration    <= total_duration,        EInvalidInterval);

        let listing_uid = object::new(ctx);
        let listing_id  = object::uid_to_inner(&listing_uid);

        let token = AccessToken {
            id: object::new(ctx),
            listing_id,
            service_name: string::utf8(name),
            ip_address:   string::utf8(ip_address),
            valid_from,
            expires_at,
            bandwidth: max_bandwidth,
            issuer: ctx.sender(),
        };

        let listing = ServiceListing {
            id: listing_uid,
            issuer: ctx.sender(),
            name: string::utf8(name),
            ip_address: string::utf8(ip_address),
            price_mist,
            is_active: true,
            min_bandwidth,
            min_duration,
            bw_granularity,
            time_granularity,
            token,
        };

        let id = object::id(&listing);
        object_bag::add(&mut marketplace.listings, id, listing);
        id
    }

    public entry fun delist<COIN>(
        marketplace: &mut Marketplace<COIN>,
        listing_id: ID,
        ctx: &TxContext,
    ) {
        let listing = object_bag::borrow<ID, ServiceListing>(&marketplace.listings, listing_id);
        assert!(listing.issuer == ctx.sender(), ENotSeller);

        let listing = object_bag::remove<ID, ServiceListing>(&mut marketplace.listings, listing_id);
        let ServiceListing {
            id, issuer: _, name: _, ip_address: _, price_mist: _, is_active: _,
            min_bandwidth: _, min_duration: _, bw_granularity: _, time_granularity: _, token,
        } = listing;
        let AccessToken {
            id: token_id, listing_id: _, service_name: _, ip_address: _,
            valid_from: _, expires_at: _, bandwidth: _, issuer: _,
        } = token;
        object::delete(token_id);
        object::delete(id);
    }

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
    // Purchase
    // =========================================================

    public entry fun purchase<COIN>(
        marketplace: &mut Marketplace<COIN>,
        listing_id: ID,
        payment: &mut Coin<COIN>,
        start: u64,
        end: u64,
        bandwidth: u64,
        ctx: &mut TxContext,
    ): ID {
        let mut listing = object_bag::remove<ID, ServiceListing>(&mut marketplace.listings, listing_id);
        assert!(listing.is_active, EListingNotActive);
        assert!(coin::value(payment) >= listing.price_mist, EInsufficientPayment);

        let seller_from  = listing.token.valid_from;
        let seller_until = listing.token.expires_at;
        let seller_bw    = listing.token.bandwidth;
        assert!(start >= seller_from,                                              EInvalidInterval);
        assert!(end > start && end <= seller_until,                               EInvalidInterval);
        assert!(seller_bw == 0 || (bandwidth > 0 && bandwidth <= seller_bw),     EInvalidBandwidth);

        let duration = end - start;
        assert!(listing.min_bandwidth != 0 && bandwidth  >= listing.min_bandwidth,                EInvalidBandwidth);
        assert!(listing.min_duration  != 0 && duration   >= listing.min_duration,                 EInvalidInterval);
        assert!(listing.bw_granularity   != 0 && bandwidth % listing.bw_granularity   == 0,       EInvalidGranularity);
        assert!(listing.time_granularity != 0 && duration  % listing.time_granularity == 0,       EInvalidGranularity);

        listing.token.valid_from = start;
        listing.token.expires_at = end;
        listing.token.bandwidth  = bandwidth;

        let seller_payment = coin::split(payment, listing.price_mist, ctx);
        transfer::public_transfer(seller_payment, listing.issuer);

        let ServiceListing {
            id, issuer: _, name: _, ip_address: _, price_mist: _, is_active: _,
            min_bandwidth: _, min_duration: _, bw_granularity: _, time_granularity: _, token,
        } = listing;
        object::delete(id);

        let token_id = object::id(&token);
        transfer::public_transfer(token, ctx.sender());
        token_id
    }

    // =========================================================
    // Redemption
    // =========================================================

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
            valid_from: token.valid_from,
            expires_at: token.expires_at,
            bandwidth:  token.bandwidth,
        });

        let AccessToken {
            id, listing_id: _, service_name: _, ip_address: _,
            valid_from: _, expires_at: _, bandwidth: _, issuer: _,
        } = token;
        object::delete(id);
    }

    // =========================================================
    // View helpers
    // =========================================================

    public fun listing_price(listing: &ServiceListing): u64          { listing.price_mist }
    public fun listing_active(listing: &ServiceListing): bool         { listing.is_active }
    public fun listing_issuer(listing: &ServiceListing): address      { listing.issuer }
    public fun listing_valid_from(listing: &ServiceListing): u64      { listing.token.valid_from }
    public fun listing_expires_at(listing: &ServiceListing): u64      { listing.token.expires_at }
    public fun listing_bandwidth(listing: &ServiceListing): u64       { listing.token.bandwidth }
    public fun listing_min_bandwidth(listing: &ServiceListing): u64   { listing.min_bandwidth }
    public fun listing_min_duration(listing: &ServiceListing): u64    { listing.min_duration }
    public fun listing_bw_granularity(listing: &ServiceListing): u64  { listing.bw_granularity }
    public fun listing_time_granularity(listing: &ServiceListing): u64 { listing.time_granularity }
    public fun token_listing_id(token: &AccessToken): ID              { token.listing_id }
    public fun token_expires_at(token: &AccessToken): u64             { token.expires_at }
    public fun token_bandwidth(token: &AccessToken): u64              { token.bandwidth }
