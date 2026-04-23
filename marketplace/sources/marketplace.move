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
    use marketplace::access_token::{Self, AccessToken};

    // =========================================================
    // Error codes
    // =========================================================
    const EInsufficientPayment:  u64 = 0;
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

    /// A service listing wraps an AccessToken and adds marketplace metadata.
    /// Name and ip_address are accessed via the embedded token.
    public struct ServiceListing has key, store {
        id: UID,
        issuer: address,
        price_mist: u64,
        min_bandwidth: u64,    // kB/s
        min_duration: u64,     // seconds
        bw_granularity: u64,   // kB/s
        time_granularity: u64, // seconds
        token: AccessToken,
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

    /// Wraps a previously created AccessToken into a ServiceListing.
    /// Only the token's original issuer may list it.
    public entry fun create_listing<COIN>(
        marketplace:      &mut Marketplace<COIN>,
        mut token:        AccessToken,
        price_mist:       u64,
        min_bandwidth:    u64,
        min_duration:     u64,
        bw_granularity:   u64,
        time_granularity: u64,
        ctx:              &mut TxContext,
    ): ID {
        assert!(access_token::issuer(&token) == ctx.sender(), ENotSeller);

        let total_duration = access_token::expires_at(&token) - access_token::valid_from(&token);
        assert!(min_bandwidth != 0 && min_bandwidth <= access_token::bandwidth(&token), EInvalidMinBandwidth);
        assert!(bw_granularity   != 0 && access_token::bandwidth(&token) % bw_granularity   == 0, EInvalidGranularity);
        assert!(time_granularity != 0 && total_duration % time_granularity == 0, EInvalidGranularity);
        assert!(min_duration     != 0 && min_duration <= total_duration,          EInvalidInterval);

        let listing_uid = object::new(ctx);

        let listing = ServiceListing {
            id: listing_uid,
            issuer: ctx.sender(),
            price_mist,
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
        listing_id:  ID,
        ctx:         &TxContext,
    ) {
        let listing = object_bag::borrow<ID, ServiceListing>(&marketplace.listings, listing_id);
        assert!(listing.issuer == ctx.sender(), ENotSeller);

        let listing = object_bag::remove<ID, ServiceListing>(&mut marketplace.listings, listing_id);
        let ServiceListing {
            id, issuer: _, price_mist: _,
            min_bandwidth: _, min_duration: _, bw_granularity: _, time_granularity: _, token,
        } = listing;
        access_token::destroy(token);
        object::delete(id);
    }

    public entry fun update_listing<COIN>(
        marketplace:    &mut Marketplace<COIN>,
        listing_id:     ID,
        new_price_mist: u64,
        ctx:            &TxContext,
    ) {
        let listing = object_bag::borrow_mut<ID, ServiceListing>(&mut marketplace.listings, listing_id);
        assert!(listing.issuer == ctx.sender(), ENotSeller);
        listing.price_mist = new_price_mist;
    }

    // =========================================================
    // Purchase
    // =========================================================

    public entry fun purchase<COIN>(
        marketplace: &mut Marketplace<COIN>,
        listing_id:  ID,
        payment:     &mut Coin<COIN>,
        start:       u64,
        end:         u64,
        bandwidth:   u64,
        ctx:         &mut TxContext,
    ): ID {
        let mut listing = object_bag::remove<ID, ServiceListing>(&mut marketplace.listings, listing_id);
        assert!(coin::value(payment) >= listing.price_mist, EInsufficientPayment);

        let seller_from  = access_token::valid_from(&listing.token);
        let seller_until = access_token::expires_at(&listing.token);
        let seller_bw    = access_token::bandwidth(&listing.token);
        assert!(start >= seller_from,                                          EInvalidInterval);
        assert!(end > start && end <= seller_until,                           EInvalidInterval);
        assert!(seller_bw == 0 || (bandwidth > 0 && bandwidth <= seller_bw), EInvalidBandwidth);

        let duration = end - start;
        assert!(listing.min_bandwidth != 0 && bandwidth >= listing.min_bandwidth,            EInvalidBandwidth);
        assert!(listing.min_duration  != 0 && duration  >= listing.min_duration,             EInvalidInterval);
        assert!(listing.bw_granularity   != 0 && bandwidth % listing.bw_granularity   == 0, EInvalidGranularity);
        assert!(listing.time_granularity != 0 && duration  % listing.time_granularity == 0, EInvalidGranularity);

        access_token::set_valid_from(&mut listing.token, start);
        access_token::set_expires_at(&mut listing.token, end);
        access_token::set_bandwidth(&mut listing.token, bandwidth);

        let seller_payment = coin::split(payment, listing.price_mist, ctx);
        transfer::public_transfer(seller_payment, listing.issuer);

        let ServiceListing {
            id, issuer: _, price_mist: _,
            min_bandwidth: _, min_duration: _, bw_granularity: _, time_granularity: _, token,
        } = listing;
        object::delete(id);

        let token_id = object::id(&token);
        transfer::public_transfer(token, ctx.sender());
        token_id
    }




    fun get_relative_price(price: u64, original_val: u64, new_val: u64){
        price * new_val / original_val;
    }