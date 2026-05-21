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
    use marketplace::access_token::{Self, AccessToken, RedemptionRequest};
    use marketplace::escrow::{Self, Escrow};
    use sui::clock::{Self,Clock};

    // =========================================================
    // Error codes
    // =========================================================
    const EInsufficientPayment:  u64 = 0;
    const ENotSeller:            u64 = 2;
    const ETokenInvalid:         u64 = 3;
    const EInvalidInterval:      u64 = 4;
    const EInvalidBandwidth:     u64 = 5;
    const EInvalidMinBandwidth:  u64 = 6;
    const EInvalidGranularity:   u64 = 7;
    const ENotIssuer:            u64 = 8;
    const ETiersNotSorted:       u64 = 9;
    const ETierLengthMismatch:   u64 = 10;
    const EEscrowTokenMismatch:  u64 = 11;
    const EInvalidEscrowStatus:  u64 = 12;

    // =========================================================
    // Pricing structs
    // =========================================================

    /// A single tier in a discount schedule.
    /// `fraction_bps` is the price fraction in basis points (10 000 = 100%).
    public struct DiscountTier has store, drop, copy {
        threshold:    u64,  // kB/s (bandwidth) or seconds (duration)
        fraction_bps: u64,  // price fraction in basis points
    }

    /// Pricing policy embedded in every ServiceListing.
    ///
    /// Price formula:
    ///   computed  = base_price_mist × bandwidth × duration
    ///             × bw_fraction(bandwidth)  / 10 000
    ///             × dur_fraction(duration)  / 10 000
    ///   effective = max(min_price, computed)
    ///
    /// Fractions are linearly interpolated from the tier lists.
    /// Empty tier list → fraction is always 10 000 (100%), i.e. no volume discount.
    public struct PricingPolicy has store, drop {
        base_price_mist: u64,                   // price per (kB/s × second) in MIST
        min_price:       u64,                   // absolute price floor in MIST
        bw_tiers:        vector<DiscountTier>,  // sorted ascending by threshold (kB/s)
        dur_tiers:       vector<DiscountTier>,  // sorted ascending by threshold (seconds)
    }

    // =========================================================
    // Core objects
    // =========================================================

    public struct Marketplace<phantom COIN> has key {
        id: UID,
        listings: ObjectBag,
    }

    /// A service listing wraps an AccessToken and its pricing policy.
    public struct ServiceListing has key, store {
        id:               UID,
        issuer:           address,
        pricing_policy:   PricingPolicy,
        min_bandwidth:    u64,    // kB/s
        min_duration:     u64,    // seconds
        bw_granularity:   u64,   // kB/s
        time_granularity: u64,   // seconds
        token:            AccessToken,
    }

    /// Emitted by purchase() so the orchestrator can map token_id → escrow_id.
    public struct PurchaseCompleted has copy, drop {
        token_id:  ID,
        escrow_id: ID,
        buyer:     address,
        seller:    address,
        amount:    u64,
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

    /// Wraps an AccessToken into a ServiceListing with a flat pricing policy
    /// (no tier discounts). Call `set_pricing_policy` afterwards to add tiers.
    public entry fun create_listing<COIN>(
        marketplace:      &mut Marketplace<COIN>,
        token:            AccessToken,
        base_price_mist:  u64,
        min_price:        u64,
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
        let id = object::uid_to_inner(&listing_uid);

        object_bag::add(&mut marketplace.listings, id, ServiceListing {
            id: listing_uid,
            issuer: ctx.sender(),
            pricing_policy: PricingPolicy {
                base_price_mist,
                min_price,
                bw_tiers:  vector::empty(),
                dur_tiers: vector::empty(),
            },
            min_bandwidth,
            min_duration,
            bw_granularity,
            time_granularity,
            token,
        });
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
            id, issuer: _, pricing_policy: _,
            min_bandwidth: _, min_duration: _, bw_granularity: _, time_granularity: _, token,
        } = listing;
        access_token::destroy(token);
        object::delete(id);
    }

    // =========================================================
    // Pricing policy management
    // =========================================================

    /// Replace the pricing policy on a listing the caller owns.
    ///
    /// Pass empty vectors for `bw_thresholds`/`dur_thresholds` to clear tiers
    /// (reverts to flat `base_price_mist` pricing).
    /// Thresholds must be strictly ascending within each list.
    public entry fun set_pricing_policy<COIN>(
        marketplace:       &mut Marketplace<COIN>,
        listing_id:        ID,
        base_price_mist:   u64,
        min_price:         u64,
        bw_thresholds:     vector<u64>,
        bw_fractions_bps:  vector<u64>,
        dur_thresholds:    vector<u64>,
        dur_fractions_bps: vector<u64>,
        ctx:               &TxContext,
    ) {
        let listing = object_bag::borrow_mut<ID, ServiceListing>(&mut marketplace.listings, listing_id);
        assert!(listing.issuer == ctx.sender(), ENotIssuer);
        listing.pricing_policy = PricingPolicy {
            base_price_mist,
            min_price,
            bw_tiers:  build_tiers(bw_thresholds,  bw_fractions_bps),
            dur_tiers: build_tiers(dur_thresholds, dur_fractions_bps),
        };
    }

    // =========================================================
    // Purchase
    // =========================================================

    // returns true if the listing fulfills the minimum bw/duration requirements  
    fun isValidListing(
        listing: &ServiceListing
    ): bool {
        let start =  access_token::valid_from(&listing.token);
        let end =  access_token::expires_at(&listing.token);
        let bw =  access_token::bandwidth(&listing.token);
        bw >= listing.min_bandwidth 
        && bw >= listing.bw_granularity
        && start < end
        && (end-start) >= listing.min_duration
        && (end-start) >= listing.time_granularity
    }

    fun split_bandwidth(
        listing: &mut ServiceListing,
        split_bw: u64,
        ctx: &mut TxContext
    ): ServiceListing{
        let old_bw = access_token::bandwidth(&listing.token);
        assert!(
            split_bw >= listing.min_bandwidth
            && split_bw < old_bw,
            EInvalidBandwidth
        );
        assert!( split_bw % listing.bw_granularity == 0, EInvalidGranularity);
        let new_asset = access_token::split_bandwidth_internal(&mut listing.token, split_bw, ctx);

        let new_listing = ServiceListing{  
            id:               object::new(ctx),
            issuer:           listing.issuer,
            pricing_policy:   clone_pricing_policy(&listing.pricing_policy),
            min_bandwidth:    listing.min_bandwidth,    // kB/s
            min_duration:     listing.min_duration,    // seconds
            bw_granularity:   listing.bw_granularity,   // kB/s
            time_granularity: listing.time_granularity,   // seconds
            token:            new_asset,
        };

        new_listing

    }

    fun split_time (
        listing: &mut ServiceListing,
        split_time: u64,
        ctx:    &mut TxContext
    ): ServiceListing {

        assert!(
            split_time > access_token::valid_from(&listing.token)
            && split_time < access_token::expires_at(&listing.token),
            EInvalidInterval
        );

        let new_asset = access_token::split_time_internal(&mut listing.token, split_time, ctx);
        
        let new_listing = ServiceListing{  
            id:               object::new(ctx),
            issuer:           listing.issuer,
            pricing_policy:   clone_pricing_policy(&listing.pricing_policy),
            min_bandwidth:    listing.min_bandwidth,    // kB/s
            min_duration:     listing.min_duration,    // seconds
            bw_granularity:   listing.bw_granularity,   // kB/s
            time_granularity: listing.time_granularity,   // seconds
            token:            new_asset,
        };

        new_listing
    }

    fun extract_asset_by_time<COIN>(
        market: &mut Marketplace<COIN>,
        mut listing: ServiceListing,
        valid_from: u64,
        expires_at: u64,
        ctx:    &mut TxContext
    ): ServiceListing {
        let old_start  = access_token::valid_from(&listing.token);
        let old_end = access_token::expires_at(&listing.token);
        let duration = expires_at - valid_from;

        assert!(valid_from < expires_at
                && valid_from >=old_start
                && expires_at <= old_end
                && duration >= listing.min_duration, EInvalidInterval);

        assert!(duration % listing.time_granularity == 0, EInvalidGranularity);

        if(valid_from > old_start){
            let new_listing = split_time(&mut listing, valid_from,ctx);
            let new_id = object::id(&listing);
            if(isValidListing(&listing)) {object_bag::add(&mut market.listings, new_id, listing);}
            else {destroy_listing(listing);};
            listing = new_listing;
        };

        if(expires_at < old_end){
            let new_listing = split_time(&mut listing, expires_at, ctx);
            let new_id = object::id(&new_listing);
            if(isValidListing(&new_listing)) {object_bag::add(&mut market.listings, new_id, new_listing);}
            else {destroy_listing(new_listing);}
        };

        listing
    }


    fun purchase_internal<COIN>(
        marketplace: &mut Marketplace<COIN>,
        listing_id:  ID,
        payment:     &mut Coin<COIN>,
        start:       u64,
        end:         u64,
        bandwidth:   u64,
        ctx:         &mut TxContext,
    ): (AccessToken, Escrow<COIN>) {
        let mut listing = object_bag::remove<ID, ServiceListing>(&mut marketplace.listings, listing_id);
        // split asset into desired bounds
        listing = extract_asset_by_time<COIN>(marketplace, listing, start, end, ctx);

        if(bandwidth != access_token::bandwidth(&listing.token)){
            let new_listing = split_bandwidth(&mut listing, bandwidth, ctx);
            let new_id = object::id(&new_listing);
            if(isValidListing(&new_listing)) {object_bag::add(&mut marketplace.listings, new_id, new_listing);}
            else {destroy_listing(new_listing);}
        };
        
        
        // calculate price of asset
        let effective_price = compute_price(&listing);
        assert!(coin::value(payment) >= effective_price, EInsufficientPayment);
        let seller_payment = coin::split(payment, effective_price, ctx);

        let ServiceListing {
            id,
            issuer,
            pricing_policy,
            min_bandwidth,
            min_duration,
            bw_granularity,
            time_granularity,
            token: access_token,
        } = listing;

        // escrow payment
        let token_id = object::id(&access_token);
        let escrow_obj = escrow::new_escrow<COIN>(
            token_id, ctx.sender(), issuer, seller_payment, end, ctx,
        );

        event::emit(PurchaseCompleted {
            token_id,
            escrow_id: object::id(&escrow_obj),
            buyer:     ctx.sender(),
            seller:     issuer,
            amount:    effective_price,
        });
        object::delete(id);
        (access_token, escrow_obj)

    }

    public entry fun purchase<COIN>(
        marketplace: &mut Marketplace<COIN>,
        listing_id:  ID,
        payment:     &mut Coin<COIN>,
        start:       u64,
        end:         u64,
        bandwidth:   u64,
        ctx:         &mut TxContext,
    ): ID {
        let (token, escrow_obj) = purchase_internal(marketplace, listing_id, payment, start, end, bandwidth, ctx);
        let token_id = object::id(&token);
        escrow::share_escrow(escrow_obj);
        transfer::public_transfer(token, ctx.sender());
        token_id
    }

    /// Non-entry version of purchase for PTB chaining.
    /// Returns the AccessToken and unshared Escrow so the caller can pipe them
    /// into a subsequent redeem_and_share call within the same PTB.
    public fun purchase_token<COIN>(
        marketplace: &mut Marketplace<COIN>,
        listing_id:  ID,
        payment:     &mut Coin<COIN>,
        start:       u64,
        end:         u64,
        bandwidth:   u64,
        ctx:         &mut TxContext,
    ): (AccessToken, Escrow<COIN>) {
        purchase_internal(marketplace, listing_id, payment, start, end, bandwidth, ctx)
    }

    // =========================================================
    // Redemption lifecycle 
    // =========================================================

    fun redeem_internal<COIN>(
        escrow:        &mut Escrow<COIN>,
        clock:         &Clock,
        token:         AccessToken,
        client_pubkey: vector<u8>,
        ctx:           &mut TxContext,
    ) {
        assert!(object::id(&token) == escrow::token_id(escrow), EEscrowTokenMismatch);
        assert!(escrow::status(escrow) == escrow::status_purchased(), EInvalidEscrowStatus);

        // Read event fields before token is moved into the request
        let issuer     = access_token::issuer(&token);
        let service_ip = access_token::ip_address(&token);
        let valid_from = access_token::valid_from(&token);
        let expires_at = access_token::expires_at(&token);
        let bandwidth  = access_token::bandwidth(&token);
        let escrow_id  = object::id(escrow);

        escrow::set_redeemed(escrow, clock);

        let (token_id, request_id, request) = access_token::create_redemption_request(
            token, ctx.sender(), client_pubkey, ctx,
        );

        access_token::emit_token_redeemed(
            token_id, request_id, escrow_id, issuer, ctx.sender(),
            service_ip, client_pubkey, valid_from, expires_at, bandwidth,
        );

        access_token::transfer_redemption_request(request, issuer);
    }

    /// Buyer redeems their token, registering their public key for encryption
    /// and advancing the escrow to REDEEMED status.
    public entry fun redeem<COIN>(
        escrow:        &mut Escrow<COIN>,
        clock:         &Clock,
        token:         AccessToken,
        client_pubkey: vector<u8>,
        ctx:           &mut TxContext,
    ) {
        redeem_internal(escrow, clock, token, client_pubkey, ctx);
    }

    /// Non-entry version of redeem for PTB chaining.
    /// Takes the owned Escrow returned by purchase_token, runs redeem logic,
    /// then shares the escrow so the seller can call deliver_redemption later.
    public fun redeem_and_share<COIN>(
        escrow:        Escrow<COIN>,
        clock:         &Clock,
        token:         AccessToken,
        client_pubkey: vector<u8>,
        ctx:           &mut TxContext,
    ) {
        let mut escrow = escrow;
        redeem_internal(&mut escrow, clock, token, client_pubkey, ctx);
        escrow::share_escrow(escrow);
    }

    /// Seller delivers the encrypted auth key, advancing the escrow to DELIVERED
    /// status so they can immediately claim payment.
    public entry fun deliver_redemption<COIN>(
        escrow:             &mut Escrow<COIN>,
        clock:              &Clock,
        request:            RedemptionRequest,
        encrypted_auth_key: vector<u8>,
        ctx:                &mut TxContext,
    ) {
        assert!(access_token::request_token_id(&request) == escrow::token_id(escrow), EEscrowTokenMismatch);
        assert!(escrow::status(escrow) == escrow::status_redeemed(), EInvalidEscrowStatus);

        escrow::set_delivered(escrow, clock);

        let (token_id, redeemed_by, _, token) = access_token::unpack_redemption_request(request);
        let (_, service_name, ip_address, login_server, valid_from, expires_at, bandwidth, issuer) =
            access_token::unpack_token_for_delivery(token);

        access_token::emit_redemption_delivery(token_id, redeemed_by, encrypted_auth_key);

        let access_key = access_token::new_access_key(
            token_id, service_name, ip_address, login_server,
            valid_from, expires_at, bandwidth, issuer, encrypted_auth_key, ctx,
        );
        transfer::public_transfer(access_key, redeemed_by);
    }

    // =========================================================
    // Private helpers
    // =========================================================

    fun destroy_listing(listing: ServiceListing) {
        let ServiceListing {
            id, issuer: _, pricing_policy: _,
            min_bandwidth: _, min_duration: _, bw_granularity: _, time_granularity: _, token,
        } = listing;
        access_token::destroy(token);
        object::delete(id);
    }

    fun clone_pricing_policy(p: &PricingPolicy): PricingPolicy {
        let n_bw  = vector::length(&p.bw_tiers);
        let n_dur = vector::length(&p.dur_tiers);
        let mut bw_tiers  = vector::empty<DiscountTier>();
        let mut dur_tiers = vector::empty<DiscountTier>();
        let mut i = 0;
        while (i < n_bw)  { vector::push_back(&mut bw_tiers,  *vector::borrow(&p.bw_tiers,  i)); i = i + 1; };
        i = 0;
        while (i < n_dur) { vector::push_back(&mut dur_tiers, *vector::borrow(&p.dur_tiers, i)); i = i + 1; };
        PricingPolicy { base_price_mist: p.base_price_mist, min_price: p.min_price, bw_tiers, dur_tiers }
    }

    fun build_tiers(thresholds: vector<u64>, fractions: vector<u64>): vector<DiscountTier> {
        let n = vector::length(&thresholds);
        assert!(n == vector::length(&fractions), ETierLengthMismatch);

        let mut tiers = vector::empty<DiscountTier>();
        let mut i = 0;
        while (i < n) {
            let threshold = *vector::borrow(&thresholds, i);
            if (i > 0) {
                assert!(threshold > *vector::borrow(&thresholds, i - 1), ETiersNotSorted);
            };
            vector::push_back(&mut tiers, DiscountTier {
                threshold,
                fraction_bps: *vector::borrow(&fractions, i),
            });
            i = i + 1;
        };
        tiers
    }

    fun interpolate_fraction(tiers: &vector<DiscountTier>, value: u64): u64 {
        let n = vector::length(tiers);
        if (n == 0) return 10_000;

        let first = vector::borrow(tiers, 0);
        if (value <= first.threshold) return first.fraction_bps;

        let last = vector::borrow(tiers, n - 1);
        if (value >= last.threshold) return last.fraction_bps;

        let mut i = 0;
        while (i < n - 1) {
            let lo = vector::borrow(tiers, i);
            let hi = vector::borrow(tiers, i + 1);
            if (value >= lo.threshold && value < hi.threshold) {
                let span  = hi.threshold - lo.threshold;
                let delta = value        - lo.threshold;
                return if (hi.fraction_bps >= lo.fraction_bps) {
                    lo.fraction_bps + delta * (hi.fraction_bps - lo.fraction_bps) / span
                } else {
                    lo.fraction_bps - delta * (lo.fraction_bps - hi.fraction_bps) / span
                }
            };
            i = i + 1;
        };
        10_000
    }


    /// Compute the effective price for a (bandwidth, duration) purchase.
    ///
    /// Formula:
    ///   computed  = base_price_mist × bandwidth × duration
    ///             × bw_fraction(bandwidth) / 10 000
    ///             × dur_fraction(duration) / 10 000
    ///   effective = max(min_price, computed)
    ///
    /// base_price_mist is in MIST per (kB/s × second), so the result scales
    /// linearly with both dimensions — consistent across remainder listings.
    /// Empty tier lists → fractions are 10 000 → no discount applied.
    fun compute_price(listing: &ServiceListing): u64 {
        let bandwidth = access_token::bandwidth(&listing.token);
        let duration  = access_token::expires_at(&listing.token) - access_token::valid_from(&listing.token);
        let policy    = &listing.pricing_policy;
        let bw_frac   = interpolate_fraction(&policy.bw_tiers,  bandwidth);
        let dur_frac  = interpolate_fraction(&policy.dur_tiers, duration);
        let computed  = (policy.base_price_mist as u128)
            * (bandwidth as u128)
            * (duration  as u128)
            * (bw_frac   as u128) / 10_000
            * (dur_frac  as u128) / 10_000;
        let computed_u64 = (computed as u64);
        if (computed_u64 > policy.min_price) { computed_u64 } else { policy.min_price }
    }
