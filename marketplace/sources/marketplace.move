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
    const ETokenInvalid:         u64 = 3; // reserved
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
    ///   computed  = base_price_mist
    ///             × bw_fraction(bandwidth)  / 10 000
    ///             × dur_fraction(duration)  / 10 000
    ///   effective = max(min_price, computed)
    ///
    /// Fractions are linearly interpolated from the tier lists.
    /// Empty tier list → fraction is always 10 000 (100%), i.e. no volume discount.
    public struct PricingPolicy has store, drop {
        base_price_mist: u64,                   // reference price at full usage
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

        let max_dur        = seller_until - seller_from;
        let computed       = compute_price(&listing.pricing_policy, bandwidth, duration, seller_bw, max_dur);
        let min_price      = listing.pricing_policy.min_price;
        let effective_price = if (computed > min_price) computed else min_price;
        assert!(coin::value(payment) >= effective_price, EInsufficientPayment);

        access_token::set_valid_from(&mut listing.token, start);
        access_token::set_expires_at(&mut listing.token, end);
        access_token::set_bandwidth(&mut listing.token, bandwidth);

        let seller         = listing.issuer;
        let seller_payment = coin::split(payment, effective_price, ctx);

        let ServiceListing {
            id, issuer: _, pricing_policy: _,
            min_bandwidth: _, min_duration: _, bw_granularity: _, time_granularity: _, token,
        } = listing;
        object::delete(id);

        let token_id = object::id(&token);

        // Hold payment in escrow until the buyer redeems and the seller delivers.
        let escrow_obj = escrow::new_escrow<COIN>(
            token_id, ctx.sender(), seller, seller_payment, end, ctx,
        );
        let escrow_id = object::id(&escrow_obj);
        escrow::share_escrow(escrow_obj);

        event::emit(PurchaseCompleted { token_id, escrow_id, buyer: ctx.sender(), seller, amount: effective_price });

        transfer::public_transfer(token, ctx.sender());
        token_id
    }

    // =========================================================
    // Redemption lifecycle (moved from access_token.move)
    // =========================================================

    /// Buyer redeems their token, registering their public key for encryption
    /// and advancing the escrow to REDEEMED status.
    public entry fun redeem<COIN>(
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
    // Pricing helpers (private)
    // =========================================================

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

    /// Compute the price for a given (bandwidth, duration) purchase.
    ///
    /// Formula:
    ///   linear = base_price_mist × (bandwidth / max_bw) × (duration / max_dur)
    ///   price  = linear × bw_fraction(bandwidth) / 10 000
    ///                   × dur_fraction(duration) / 10 000
    ///
    /// Empty tier lists → fractions are 10 000 → pure linear scaling.
    /// Tier fractions < 10 000 → volume discounts on top of linear.
    fun compute_price(
        policy:   &PricingPolicy,
        bandwidth: u64,
        duration:  u64,
        max_bw:    u64,
        max_dur:   u64,
    ): u64 {
        let bw_frac  = interpolate_fraction(&policy.bw_tiers,  bandwidth);
        let dur_frac = interpolate_fraction(&policy.dur_tiers, duration);
        // Linear base scales with what the buyer actually purchases.
        let linear = (policy.base_price_mist as u128)
            * (bandwidth as u128) / (max_bw  as u128)
            * (duration  as u128) / (max_dur as u128);
        // Apply tier discount multipliers.
        let price = linear;
        //    * (bw_frac  as u128) / 10_000
        //    * (dur_frac as u128) / 10_000;
        (price as u64)
    }
