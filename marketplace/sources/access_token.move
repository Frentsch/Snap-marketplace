/// access_token.move
///
/// Defines the AccessToken NFT that grants access to a service.
/// Tokens are created by sellers via `create_access_token`, then wrapped
/// into a ServiceListing. On purchase the listing is destroyed and the
/// token is transferred to the buyer. The buyer later redeems it to
/// prove access, which destroys the token on-chain.
module marketplace::access_token;

    use std::string::{Self, String};
    use sui::event;

    // =========================================================
    // Error codes (values must match marketplace.move)
    // =========================================================
    const EInvalidInterval:    u64 = 4;
    const EInvalidBandwidth:   u64 = 5;
    const EInvalidSplit:       u64 = 8;  // split point is out of the token's bounds
    const EIncompatibleTokens: u64 = 9;  // tokens belong to different service endpoints
    const ENoOverlap:          u64 = 10; // time intervals don't overlap or aren't contiguous

    // =========================================================
    // Struct
    // =========================================================

    public struct AccessToken has key, store {
        id:           UID,
        service_name: String,
        ip_address:   String,
        valid_from:   u64,     // Unix seconds
        expires_at:   u64,     // Unix seconds
        bandwidth:    u64,     // kB/s
        issuer:       address,
    }

        public struct TokenRedeemed has copy, drop {
        token_id:    ID,
        issuer:      address,
        redeemed_by: address,
        service_ip:  String, // seller's service endpoint — identifies which server this token is for
        client_ip:   String, // buyer's IP address recorded at redemption time
        valid_from:  u64,
        expires_at:  u64,
        bandwidth:   u64,
    }

    // =========================================================
    // Creation
    // =========================================================

    /// Creates an AccessToken for a service and transfers it to the caller.
    /// The token must subsequently be passed to `marketplace::create_listing`.
    ///
    /// `valid_from`: seller's "not before" bound in Unix seconds.
    /// `expires_at`: seller's "not after" bound; must be > valid_from.
    public entry fun create_access_token(
        name:          vector<u8>,
        ip_address:    vector<u8>,
        valid_from:    u64,
        expires_at:    u64,
        max_bandwidth: u64,
        ctx:           &mut TxContext,
    ): ID {
        assert!(expires_at > valid_from, EInvalidInterval);
        assert!(max_bandwidth > 0,       EInvalidBandwidth);

        let token_uid = object::new(ctx);
        let token_id  = object::uid_to_inner(&token_uid);
        let token = AccessToken {
            id:           token_uid,
            service_name: string::utf8(name),
            ip_address:   string::utf8(ip_address),
            valid_from,
            expires_at,
            bandwidth: max_bandwidth,
            issuer: ctx.sender(),
        };
        transfer::public_transfer(token, ctx.sender());
        token_id
    }

    // =========================================================
    // Package-internal getters
    // =========================================================

    public(package) fun valid_from(t: &AccessToken): u64     { t.valid_from }
    public(package) fun expires_at(t: &AccessToken): u64     { t.expires_at }
    public(package) fun bandwidth(t: &AccessToken): u64      { t.bandwidth }
    public(package) fun issuer(t: &AccessToken): address     { t.issuer }

    // =========================================================
    // Package-internal setters
    // =========================================================

    public(package) fun set_valid_from(t: &mut AccessToken, v: u64) { t.valid_from = v }
    public(package) fun set_expires_at(t: &mut AccessToken, v: u64) { t.expires_at = v }
    public(package) fun set_bandwidth(t: &mut AccessToken, v: u64)  { t.bandwidth = v }

    // =========================================================
    // Lifecycle
    // =========================================================

    /// Destroys the token and returns the fields needed by the `TokenRedeemed` event:
    /// `(token_id, issuer, ip_address, valid_from, expires_at, bandwidth)`.
    public(package) fun burn(token: AccessToken): (ID, address, String, u64, u64, u64) {
        let token_id = object::id(&token);
        let AccessToken { id, service_name: _, ip_address, valid_from, expires_at, bandwidth, issuer } = token;
        object::delete(id);
        (token_id, issuer, ip_address, valid_from, expires_at, bandwidth)
    }

    /// Destroys the token without returning anything (used by `delist`).
    public(package) fun destroy(token: AccessToken) {
        let AccessToken { id, service_name: _, ip_address: _, valid_from: _, expires_at: _, bandwidth: _, issuer: _ } = token;
        object::delete(id);
    }

    // =========================================================
    // Split and fuse
    // =========================================================

    fun new_token(
        service_name: String,
        ip_address:   String,
        valid_from:   u64,
        expires_at:   u64,
        bandwidth:    u64,
        issuer:       address,
        ctx:          &mut TxContext,
    ): AccessToken {
        AccessToken { id: object::new(ctx), service_name, ip_address, valid_from, expires_at, bandwidth, issuer }
    }

    /// Split a token along the bandwidth axis.
    /// The original token's bandwidth is set to `bw_a`; a new token with the
    /// remainder `bandwidth - bw_a` is created. Both share the original time interval.
    public entry fun split_bandwidth(
        mut token: AccessToken,
        bw_a:      u64,
        ctx:       &mut TxContext,
    ) {
        assert!(bw_a > 0 && bw_a < token.bandwidth, EInvalidSplit);
        let remainder = new_token(token.service_name, token.ip_address, token.valid_from, token.expires_at, token.bandwidth - bw_a, token.issuer, ctx);
        token.bandwidth = bw_a;
        transfer::public_transfer(token, ctx.sender());
        transfer::public_transfer(remainder, ctx.sender());
    }

    /// Split a token along the time axis at `split_at`.
    /// The original token's interval is shortened to `[valid_from, split_at]`; a new
    /// token covering `[split_at, expires_at]` is created. Both keep the original bandwidth.
    public entry fun split_time(
        mut token: AccessToken,
        split_at:  u64,
        ctx:       &mut TxContext,
    ) {
        assert!(split_at > token.valid_from && split_at < token.expires_at, EInvalidSplit);
        let remainder = new_token(token.service_name, token.ip_address, split_at, token.expires_at, token.bandwidth, token.issuer, ctx);
        token.expires_at = split_at;
        transfer::public_transfer(token, ctx.sender());
        transfer::public_transfer(remainder, ctx.sender());
    }

    /// Fuse two tokens along the bandwidth axis.
    /// Asserts both tokens belong to the same service (same issuer and ip_address)
    /// and that their time intervals overlap. The output covers the overlapping
    /// interval with the sum of both bandwidths.
    public entry fun fuse_bandwidth(
        token_a: AccessToken,
        token_b: AccessToken,
        ctx:     &mut TxContext,
    ) {
        assert!(token_a.issuer     == token_b.issuer,     EIncompatibleTokens);
        assert!(token_a.ip_address == token_b.ip_address, EIncompatibleTokens);
        let from  = if (token_a.valid_from > token_b.valid_from) { token_a.valid_from } else { token_b.valid_from };
        let until = if (token_a.expires_at < token_b.expires_at) { token_a.expires_at } else { token_b.expires_at };
        assert!(from < until, ENoOverlap);
        let bandwidth    = token_a.bandwidth + token_b.bandwidth;
        let service_name = token_a.service_name;
        let ip_address   = token_a.ip_address;
        let issuer       = token_a.issuer;
        let AccessToken { id: id_a, service_name: _, ip_address: _, valid_from: _, expires_at: _, bandwidth: _, issuer: _ } = token_a;
        object::delete(id_a);
        let AccessToken { id: id_b, service_name: _, ip_address: _, valid_from: _, expires_at: _, bandwidth: _, issuer: _ } = token_b;
        object::delete(id_b);
        transfer::public_transfer(new_token(service_name, ip_address, from, until, bandwidth, issuer, ctx), ctx.sender());
    }

    /// Fuse two tokens along the time axis.
    /// Asserts both tokens belong to the same service and that their intervals
    /// overlap or are contiguous. The output covers the union of both intervals
    /// with the minimum of their bandwidths.
    public entry fun fuse_time(
        token_a: AccessToken,
        token_b: AccessToken,
        ctx:     &mut TxContext,
    ) {
        assert!(token_a.issuer     == token_b.issuer,     EIncompatibleTokens);
        assert!(token_a.ip_address == token_b.ip_address, EIncompatibleTokens);
        let max_from  = if (token_a.valid_from > token_b.valid_from) { token_a.valid_from } else { token_b.valid_from };
        let min_until = if (token_a.expires_at < token_b.expires_at) { token_a.expires_at } else { token_b.expires_at };
        assert!(max_from <= min_until, ENoOverlap);
        let from      = if (token_a.valid_from < token_b.valid_from) { token_a.valid_from } else { token_b.valid_from };
        let until     = if (token_a.expires_at > token_b.expires_at) { token_a.expires_at } else { token_b.expires_at };
        let bandwidth = if (token_a.bandwidth  < token_b.bandwidth)  { token_a.bandwidth  } else { token_b.bandwidth  };
        let service_name = token_a.service_name;
        let ip_address   = token_a.ip_address;
        let issuer       = token_a.issuer;
        let AccessToken { id: id_a, service_name: _, ip_address: _, valid_from: _, expires_at: _, bandwidth: _, issuer: _ } = token_a;
        object::delete(id_a);
        let AccessToken { id: id_b, service_name: _, ip_address: _, valid_from: _, expires_at: _, bandwidth: _, issuer: _ } = token_b;
        object::delete(id_b);
        transfer::public_transfer(new_token(service_name, ip_address, from, until, bandwidth, issuer, ctx), ctx.sender());
    }


    // =========================================================
    // Redemption
    // =========================================================

    public entry fun redeem(
        token:      AccessToken,
        ip_address: vector<u8>,
        ctx:        &TxContext,
    ) {
        let (token_id, issuer, service_ip, valid_from, expires_at, bandwidth) =
            burn(token);

        event::emit(TokenRedeemed {
            token_id,
            issuer,
            redeemed_by: ctx.sender(),
            service_ip,
            client_ip: string::utf8(ip_address),
            valid_from,
            expires_at,
            bandwidth,
        });
}