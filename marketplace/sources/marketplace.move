/// marketplace.move
///
/// A SUI marketplace where sellers create service listings and buyers purchase
/// AccessToken NFTs. Buyers later redeem tokens to prove access to off-chain
/// services. Redemption destroys the token on-chain as immutable proof of use.
///
/// All listings are stored inside a single shared Marketplace object via an
/// ObjectBag. create_listing adds to the bag; purchase removes and destroys
/// the listing (one-time-use semantics). Sellers may delist a listing before
/// it is purchased.
module marketplace::marketplace;

    // =========================================================
    // Imports
    // =========================================================
    use sui::coin::{Self, Coin};
    use sui::sui::SUI;
    use sui::event;
    use sui::clock::{Self, Clock};
    use sui::object_bag::{Self, ObjectBag};
    use std::string::{Self, String};

    // =========================================================
    // Error codes
    // =========================================================
    const EInsufficientPayment: u64 = 0;
    const EListingNotActive:    u64 = 1;
    const ENotSeller:           u64 = 2;
    const ETokenInvalid:        u64 = 3;

    // =========================================================
    // Core objects
    // =========================================================

    /// Singleton shared registry. All ServiceListings live inside its ObjectBag.
    public struct Marketplace has key {
        id: UID,
        listings: ObjectBag,
    }

    /// A service listing stored inside the Marketplace ObjectBag.
    /// `price_mist` is denominated in MIST (1 SUI = 1_000_000_000 MIST).
    /// `duration_ms` is how long a purchased AccessToken remains valid in
    /// milliseconds. Use 0 for perpetual (never expires) access.
    public struct ServiceListing has key, store {
        id: UID,
        /// Address of the seller — receives payment on purchase
        seller: address,
        /// Human-readable service name
        name: String,
        /// IP address or host:port of the service endpoint
        ip_address: String,
        /// Price per access grant, in MIST
        price_mist: u64,
        /// Access duration in milliseconds; 0 = perpetual
        duration_ms: u64,
        /// Whether the listing is accepting new purchases
        is_active: bool,
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
        /// Unix timestamp (ms) from which the token is valid; 0 = immediately
        valid_from_ms: u64,
        /// Unix timestamp (ms) after which the token is expired; 0 = never
        expires_at_ms: u64,
        /// Seller address at time of purchase
        seller: address,
    }

    // =========================================================
    // Events
    // =========================================================

    public struct TokenRedeemed has copy, drop {
        token_id: ID,
        listing_id: ID,
        redeemed_by: address,
        ip_address: String,
        valid_from_ms: u64,
        expires_at_ms: u64,
    }

    // =========================================================
    // Module initializer
    // =========================================================

    /// Called once on publish. Creates and shares the singleton Marketplace.
    fun init(ctx: &mut TxContext) {
        transfer::share_object(Marketplace {
            id: object::new(ctx),
            listings: object_bag::new(ctx),
        });
    }

    // =========================================================
    // Seller functions
    // =========================================================

    /// Create a new service listing and store it in the Marketplace bag.
    public entry fun create_listing(
        marketplace: &mut Marketplace,
        name: vector<u8>,
        ip_address: vector<u8>,
        price_mist: u64,
        duration_ms: u64,
        ctx: &mut TxContext,
    ):ID {
        let listing = ServiceListing {
            id: object::new(ctx),
            seller: ctx.sender(),
            name: string::utf8(name),
            ip_address: string::utf8(ip_address),
            price_mist,
            duration_ms,
            is_active: true,
        };

        let listing_id = object::id(&listing);
        object_bag::add(&mut marketplace.listings, listing_id, listing);
        listing_id
    }

    /// Remove a listing from the Marketplace bag before it is purchased.
    /// Only the original seller may call this.
    public entry fun delist(
        marketplace: &mut Marketplace,
        listing_id: ID,
        ctx: &TxContext,
    ) {
        let listing = object_bag::borrow<ID, ServiceListing>(&marketplace.listings, listing_id);
        assert!(listing.seller == ctx.sender(), ENotSeller);

        let listing = object_bag::remove<ID, ServiceListing>(&mut marketplace.listings, listing_id);
        let seller = listing.seller;

        let ServiceListing { id, seller: _, name: _, ip_address: _, price_mist: _, duration_ms: _, is_active: _ } = listing;
        object::delete(id);
    }

    /// Update listing price or active status. Only the original seller may call this.
    public entry fun update_listing(
        marketplace: &mut Marketplace,
        listing_id: ID,
        new_price_mist: u64,
        is_active: bool,
        ctx: &TxContext,
    ) {
        let listing = object_bag::borrow_mut<ID, ServiceListing>(&mut marketplace.listings, listing_id);
        assert!(listing.seller == ctx.sender(), ENotSeller);
        listing.price_mist = new_price_mist;
        listing.is_active = is_active;

    }

    // =========================================================
    // Primary purchase
    // =========================================================

    /// Purchase access to a service.
    ///
    /// Removes the listing from the Marketplace bag (one-time-use).
    /// `payment` must be a Coin<SUI> with at least `listing.price_mist` MIST.
    /// Exactly `price_mist` is extracted and forwarded to the seller.
    ///
    /// `start_ms`: Unix timestamp (ms) when access should begin; 0 = now.
    /// `end_ms`:   Unix timestamp (ms) when access expires; 0 = use listing's
    ///             duration_ms from start (or perpetual if duration_ms == 0).
    public entry fun purchase(
        marketplace: &mut Marketplace,
        listing_id: ID,
        payment: &mut Coin<SUI>,
        clock: &Clock,
        start_ms: u64,
        end_ms: u64,
        ctx: &mut TxContext,
    ) : ID{
        let listing = object_bag::remove<ID, ServiceListing>(&mut marketplace.listings, listing_id);
        assert!(listing.is_active, EListingNotActive);

        let price = listing.price_mist;
        assert!(coin::value(payment) >= price, EInsufficientPayment);

        // Extract exact payment and forward to seller
        let seller_payment = coin::split(payment, price, ctx);
        transfer::public_transfer(seller_payment, listing.seller);

        let now_ms = clock::timestamp_ms(clock);
        let valid_from_ms = if (start_ms == 0) { now_ms } else { start_ms };
        let expires_at_ms = if (end_ms > 0) {
            end_ms
        } else if (listing.duration_ms == 0) {
            0
        } else {
            valid_from_ms + listing.duration_ms
        };

        let token = AccessToken {
            id: object::new(ctx),
            listing_id,
            service_name: listing.name,
            ip_address: listing.ip_address,
            valid_from_ms,
            expires_at_ms,
            seller: listing.seller,
        };

        let token_id = object::id(&token);
        transfer::public_transfer(token, ctx.sender());

        // Destroy the listing — it has been consumed by this purchase
        let ServiceListing { id, seller: _, name: _, ip_address: _, price_mist: _, duration_ms: _, is_active: _ } = listing;
        object::delete(id);
        token_id
    }

    // =========================================================
    // Redemption
    // =========================================================

    /// Redeem a token, emitting a redemption event and destroying the token.
    ///
    /// Only the current owner can call this (SUI's object model guarantees
    /// this — owned objects can only be passed by a transaction from their owner).
    /// The token is deleted on-chain; its deletion is the immutable proof of use.
    public entry fun redeem(
        token: AccessToken,
        clock: &Clock,
        ip_address: vector<u8>,
        ctx: &TxContext,
    ) {
        assert!(is_valid(&token, clock), ETokenInvalid);

        event::emit(TokenRedeemed {
            token_id: object::id(&token),
            listing_id: token.listing_id,
            redeemed_by: ctx.sender(),
            ip_address: string::utf8(ip_address),
            valid_from_ms: token.valid_from_ms,
            expires_at_ms: token.expires_at_ms,
        });

        let AccessToken {
            id, listing_id: _, service_name: _, ip_address: _,
            valid_from_ms: _, expires_at_ms: _, seller: _,
        } = token;
        object::delete(id);
    }

    // =========================================================
    // View helpers
    // =========================================================

    /// Returns true only if the token is within its valid time window.
    public fun is_valid(token: &AccessToken, clock: &Clock): bool {
        let now = clock::timestamp_ms(clock);
        if (now < token.valid_from_ms) { return false };
        if (token.expires_at_ms == 0) { return true };
        now < token.expires_at_ms
    }

    // =========================================================
    // Test helpers
    // =========================================================

    #[test_only]
    public fun init_for_testing(ctx: &mut TxContext) { init(ctx); }

    public fun listing_price(listing: &ServiceListing): u64    { listing.price_mist }
    public fun listing_active(listing: &ServiceListing): bool   { listing.is_active }
    public fun listing_seller(listing: &ServiceListing): address { listing.seller }
    public fun token_listing_id(token: &AccessToken): ID       { token.listing_id }
    public fun token_expires_at(token: &AccessToken): u64      { token.expires_at_ms }
