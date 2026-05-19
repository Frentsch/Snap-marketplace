module marketplace::escrow {
    use sui::coin::Coin;
    use sui::clock::{Self, Clock};

    // ── Status constants ──────────────────────────────────────────────────────

    const STATUS_PURCHASED: u8 = 0;
    const STATUS_REDEEMED:  u8 = 1;
    const STATUS_DELIVERED: u8 = 2;

    // ── Error codes ───────────────────────────────────────────────────────────

    const ENotSeller:          u64 = 20;
    const ENotBuyer:           u64 = 21;
    const EEscrowNotClaimable: u64 = 22;

    // other constants

    const GRACE_PERIOD:  u64 = 30;

    // ── Struct ────────────────────────────────────────────────────────────────

    /// Shared object that holds payment until the redemption outcome is settled.
    public struct Escrow<phantom COIN> has key {
        id:         UID,
        token_id:   ID,
        buyer:      address,
        seller:     address,
        payment:    Coin<COIN>,
        status:     u8,
        expires_at: u64,   // Unix seconds; copied from the purchased token window
        redeemed_at: u64,
        delivered_at: u64
    }

    // ── Package-internal API (callable only from marketplace.move) ────────────

    public(package) fun new_escrow<COIN>(
        token_id:   ID,
        buyer:      address,
        seller:     address,
        payment:    Coin<COIN>,
        expires_at: u64,
        ctx:        &mut TxContext,
    ): Escrow<COIN> {
        Escrow {
            id: object::new(ctx),
            token_id,
            buyer,
            seller,
            payment,
            status: STATUS_PURCHASED,
            expires_at,
            redeemed_at: 0,
            delivered_at: 0,
        }
    }

    /// Shares the Escrow as a shared object.
    /// Must live here because Escrow has only `key` (no `store`),
    /// restricting transfer::share_object to the defining module.
    public(package) fun share_escrow<COIN>(escrow: Escrow<COIN>) {
        transfer::share_object(escrow);
    }

    public(package) fun set_redeemed<COIN>(e: &mut Escrow<COIN>, clock: &Clock)  { e.status = STATUS_REDEEMED; e.redeemed_at = clock::timestamp_ms(clock) /1000 }
    public(package) fun set_delivered<COIN>(e: &mut Escrow<COIN>, clock: &Clock) { e.status = STATUS_DELIVERED; e.delivered_at = clock::timestamp_ms(clock) / 1000 }

    public(package) fun token_id<COIN>(e: &Escrow<COIN>): ID { e.token_id }
    public(package) fun status<COIN>(e: &Escrow<COIN>): u8   { e.status   }

    public(package) fun status_purchased(): u8 { STATUS_PURCHASED }
    public(package) fun status_redeemed():  u8 { STATUS_REDEEMED  }

    // ── Public claim functions ────────────────────────────────────────────────

    /// Seller claims payment immediately after delivery, or after expiry if the
    /// buyer never attempted to redeem (status still PURCHASED).
    /// Sellers must deliver the redemption within 30 seconds
    public entry fun claim_payment<COIN>(
        escrow: Escrow<COIN>,
        clock:  &Clock,
        ctx:    &TxContext,
    ) {
        assert!(ctx.sender() == escrow.seller, ENotSeller);
        let now = clock::timestamp_ms(clock) / 1000;
        assert!(
            (escrow.status == STATUS_DELIVERED && escrow.delivered_at < escrow.redeemed_at + GRACE_PERIOD) ||
            (escrow.status == STATUS_PURCHASED && now > escrow.expires_at),
            EEscrowNotClaimable,
        );
        let Escrow { id, token_id: _, buyer: _, seller, payment, status: _, expires_at: _, redeemed_at: _, delivered_at: _ } = escrow;
        object::delete(id);
        transfer::public_transfer(payment, seller);
    }

    /// Buyer claims a refund after expiry when the seller failed to call
    /// deliver_redemption despite the token having been redeemed.
    /// The seller has 30 seconds time to deliver the redemption to avoid 
    /// buyers redeeming just before expiry to claim refunds.
    public entry fun claim_refund<COIN>(
        escrow: Escrow<COIN>,
        clock:  &Clock,
        ctx:    &TxContext,
    ) {
        assert!(ctx.sender() == escrow.buyer, ENotBuyer);
        let now = clock::timestamp_ms(clock) / 1000;
        assert!(
            (escrow.status == STATUS_REDEEMED && now > escrow.expires_at && now > escrow.redeemed_at + GRACE_PERIOD) ||
            (escrow.status == STATUS_DELIVERED && escrow.delivered_at >= escrow.redeemed_at + GRACE_PERIOD),
            EEscrowNotClaimable,
        );
        let Escrow { id, token_id: _, buyer, seller: _, payment, status: _, expires_at: _, redeemed_at: _, delivered_at: _ } = escrow;
        object::delete(id);
        transfer::public_transfer(payment, buyer);
    }
}
