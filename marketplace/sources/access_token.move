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
        login_server: String,
        valid_from:   u64,     // Unix seconds
        expires_at:   u64,     // Unix seconds
        bandwidth:    u64,     // kB/s
        issuer:       address,
    }

    /// Capability object transferred to the issuer on redemption.
    /// The original AccessToken is wrapped inside so all its metadata is
    /// preserved until deliver_redemption, where it is unpacked and deleted.
    public struct RedemptionRequest has key {
        id:            UID,
        token_id:      ID,           // kept for backward-compatible event matching
        redeemed_by:   address,
        client_pubkey: vector<u8>,   // flag_byte || raw_pubkey_bytes
        token:         AccessToken,  // wrapped here; deleted in deliver_redemption
    }

    public struct TokenRedeemed has copy, drop {
        token_id:      ID,
        request_id:    ID,           // ID of the RedemptionRequest object
        escrow_id:     ID,           // ID of the shared Escrow object
        issuer:        address,
        redeemed_by:   address,
        service_ip:    String,
        client_pubkey: vector<u8>,   // flag_byte || raw_pubkey_bytes
        valid_from:    u64,
        expires_at:    u64,
        bandwidth:     u64,
    }

    public struct RedemptionDelivery has copy, drop {
        token_id:           ID,
        redeemed_by:        address,
        encrypted_auth_key: vector<u8>,
    }

    /// Persistent proof of a completed redemption, held by the buyer.
    /// Contains all AccessToken service fields so the front end has full
    /// context without needing to scrape past events.
    public struct AccessKey has key, store {
        id:           UID,
        token_id:     ID,
        service_name: String,
        ip_address:   String,
        login_server: String,
        valid_from:   u64,
        expires_at:   u64,
        bandwidth:    u64,
        issuer:       address,
        auth_key:     vector<u8>,
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
        login_server:  vector<u8>,
        valid_from:    u64,
        expires_at:    u64,
        max_bandwidth: u64,
        ctx:           &mut TxContext,
    ): ID {
        let token = create_access_token_obj(name, ip_address, login_server, valid_from, expires_at, max_bandwidth, ctx);
        let token_id = object::id(&token);
        transfer::public_transfer(token, ctx.sender());
        token_id
    }

    /// PTB-chainable variant: returns the `AccessToken` by value instead of
    /// transferring it, so it can be passed directly to `marketplace::create_listing`
    /// in the same transaction block.
    public fun create_access_token_obj(
        name:          vector<u8>,
        ip_address:    vector<u8>,
        login_server:  vector<u8>,
        valid_from:    u64,
        expires_at:    u64,
        max_bandwidth: u64,
        ctx:           &mut TxContext,
    ): AccessToken {
        assert!(expires_at > valid_from, EInvalidInterval);
        assert!(max_bandwidth > 0,       EInvalidBandwidth);

        AccessToken {
            id:           object::new(ctx),
            service_name: string::utf8(name),
            ip_address:   string::utf8(ip_address),
            login_server: string::utf8(login_server),
            valid_from,
            expires_at,
            bandwidth:    max_bandwidth,
            issuer:       ctx.sender(),
        }
    }

    // =========================================================
    // Package-internal getters
    // =========================================================

    public(package) fun valid_from(t: &AccessToken): u64      { t.valid_from  }
    public(package) fun expires_at(t: &AccessToken): u64      { t.expires_at  }
    public(package) fun bandwidth(t: &AccessToken): u64       { t.bandwidth   }
    public(package) fun issuer(t: &AccessToken): address      { t.issuer      }
    public(package) fun ip_address(t: &AccessToken): String   { t.ip_address  }

    public(package) fun request_token_id(req: &RedemptionRequest): ID { req.token_id }

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
        let AccessToken { id, service_name: _, ip_address, login_server: _, valid_from, expires_at, bandwidth, issuer } = token;
        object::delete(id);
        (token_id, issuer, ip_address, valid_from, expires_at, bandwidth)
    }

    /// Destroys the token without returning anything (used by `delist`).
    public(package) fun destroy(token: AccessToken) {
        let AccessToken { id, service_name: _, ip_address: _, login_server: _, valid_from: _, expires_at: _, bandwidth: _, issuer: _ } = token;
        object::delete(id);
    }

    // =========================================================
    // Split and fuse
    // =========================================================

    fun new_token(
        service_name: String,
        ip_address:   String,
        login_server: String,
        valid_from:   u64,
        expires_at:   u64,
        bandwidth:    u64,
        issuer:       address,
        ctx:          &mut TxContext,
    ): AccessToken {
        AccessToken { id: object::new(ctx), service_name, ip_address, login_server, valid_from, expires_at, bandwidth, issuer }
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
        let remainder = new_token(token.service_name, token.ip_address, token.login_server, token.valid_from, token.expires_at, token.bandwidth - bw_a, token.issuer, ctx);
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
        let remainder = new_token(token.service_name, token.ip_address, token.login_server, split_at, token.expires_at, token.bandwidth, token.issuer, ctx);
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
        assert!(token_a.issuer       == token_b.issuer,       EIncompatibleTokens);
        assert!(token_a.ip_address   == token_b.ip_address,   EIncompatibleTokens);
        assert!(token_a.login_server == token_b.login_server, EIncompatibleTokens);
        let from  = if (token_a.valid_from > token_b.valid_from) { token_a.valid_from } else { token_b.valid_from };
        let until = if (token_a.expires_at < token_b.expires_at) { token_a.expires_at } else { token_b.expires_at };
        assert!(from < until, ENoOverlap);
        let bandwidth    = token_a.bandwidth + token_b.bandwidth;
        let service_name = token_a.service_name;
        let ip_address   = token_a.ip_address;
        let login_server = token_a.login_server;
        let issuer       = token_a.issuer;
        let AccessToken { id: id_a, service_name: _, ip_address: _, login_server: _, valid_from: _, expires_at: _, bandwidth: _, issuer: _ } = token_a;
        object::delete(id_a);
        let AccessToken { id: id_b, service_name: _, ip_address: _, login_server: _, valid_from: _, expires_at: _, bandwidth: _, issuer: _ } = token_b;
        object::delete(id_b);
        transfer::public_transfer(new_token(service_name, ip_address, login_server, from, until, bandwidth, issuer, ctx), ctx.sender());
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
        assert!(token_a.issuer       == token_b.issuer,       EIncompatibleTokens);
        assert!(token_a.ip_address   == token_b.ip_address,   EIncompatibleTokens);
        assert!(token_a.login_server == token_b.login_server, EIncompatibleTokens);
        let max_from  = if (token_a.valid_from > token_b.valid_from) { token_a.valid_from } else { token_b.valid_from };
        let min_until = if (token_a.expires_at < token_b.expires_at) { token_a.expires_at } else { token_b.expires_at };
        assert!(max_from <= min_until, ENoOverlap);
        let from      = if (token_a.valid_from < token_b.valid_from) { token_a.valid_from } else { token_b.valid_from };
        let until     = if (token_a.expires_at > token_b.expires_at) { token_a.expires_at } else { token_b.expires_at };
        let bandwidth = if (token_a.bandwidth  < token_b.bandwidth)  { token_a.bandwidth  } else { token_b.bandwidth  };
        let service_name = token_a.service_name;
        let ip_address   = token_a.ip_address;
        let login_server = token_a.login_server;
        let issuer       = token_a.issuer;
        let AccessToken { id: id_a, service_name: _, ip_address: _, login_server: _, valid_from: _, expires_at: _, bandwidth: _, issuer: _ } = token_a;
        object::delete(id_a);
        let AccessToken { id: id_b, service_name: _, ip_address: _, login_server: _, valid_from: _, expires_at: _, bandwidth: _, issuer: _ } = token_b;
        object::delete(id_b);
        transfer::public_transfer(new_token(service_name, ip_address, login_server, from, until, bandwidth, issuer, ctx), ctx.sender());
    }


    // =========================================================
    // Redemption — package-internal helpers
    // (redeem() and deliver_redemption() entry fns live in marketplace.move
    //  so they can also update the Escrow object atomically.)
    // =========================================================

    /// Creates a RedemptionRequest wrapping the token; returns (token_id, request_id, request).
    public(package) fun create_redemption_request(
        token:         AccessToken,
        redeemed_by:   address,
        client_pubkey: vector<u8>,
        ctx:           &mut TxContext,
    ): (ID, ID, RedemptionRequest) {
        let token_id    = object::id(&token);
        let request_uid = object::new(ctx);
        let request_id  = object::uid_to_inner(&request_uid);
        (token_id, request_id, RedemptionRequest { id: request_uid, token_id, redeemed_by, client_pubkey, token })
    }

    /// Destructures a RedemptionRequest; deletes its UID; returns (token_id, redeemed_by, client_pubkey, token).
    public(package) fun unpack_redemption_request(
        request: RedemptionRequest,
    ): (ID, address, vector<u8>, AccessToken) {
        let RedemptionRequest { id, token_id, redeemed_by, client_pubkey, token } = request;
        object::delete(id);
        (token_id, redeemed_by, client_pubkey, token)
    }

    /// Destructures an AccessToken for use in deliver_redemption; deletes its UID.
    /// Returns (token_id, service_name, ip_address, login_server, valid_from, expires_at, bandwidth, issuer).
    public(package) fun unpack_token_for_delivery(
        token: AccessToken,
    ): (ID, String, String, String, u64, u64, u64, address) {
        let token_id = object::id(&token);
        let AccessToken { id, service_name, ip_address, login_server, valid_from, expires_at, bandwidth, issuer } = token;
        object::delete(id);
        (token_id, service_name, ip_address, login_server, valid_from, expires_at, bandwidth, issuer)
    }

    /// Constructs an AccessKey (used by marketplace::deliver_redemption).
    public(package) fun new_access_key(
        token_id:     ID,
        service_name: String,
        ip_address:   String,
        login_server: String,
        valid_from:   u64,
        expires_at:   u64,
        bandwidth:    u64,
        issuer:       address,
        auth_key:     vector<u8>,
        ctx:          &mut TxContext,
    ): AccessKey {
        AccessKey { id: object::new(ctx), token_id, service_name, ip_address, login_server, valid_from, expires_at, bandwidth, issuer, auth_key }
    }

    /// Emits the TokenRedeemed event (called from marketplace::redeem).
    public(package) fun emit_token_redeemed(
        token_id:      ID,
        request_id:    ID,
        escrow_id:     ID,
        issuer:        address,
        redeemed_by:   address,
        service_ip:    String,
        client_pubkey: vector<u8>,
        valid_from:    u64,
        expires_at:    u64,
        bandwidth:     u64,
    ) {
        event::emit(TokenRedeemed {
            token_id, request_id, escrow_id, issuer, redeemed_by,
            service_ip, client_pubkey, valid_from, expires_at, bandwidth,
        });
    }

    /// Transfers a RedemptionRequest to the given recipient.
    /// Must live here because RedemptionRequest has only `key` (no `store`),
    /// restricting transfer::transfer to the defining module.
    public(package) fun transfer_redemption_request(req: RedemptionRequest, recipient: address) {
        transfer::transfer(req, recipient);
    }

    /// Emits the RedemptionDelivery event (called from marketplace::deliver_redemption).
    public(package) fun emit_redemption_delivery(
        token_id:           ID,
        redeemed_by:        address,
        encrypted_auth_key: vector<u8>,
    ) {
        event::emit(RedemptionDelivery { token_id, redeemed_by, encrypted_auth_key });
    }