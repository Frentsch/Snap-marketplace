module marketplace::user_secret {
    use sui::bcs;
    use sui::transfer;
    use sui::tx_context::{Self, TxContext};
    use sui::object::{Self, UID};

    public struct UserSecret has key {
        id:               UID,
        // X25519 public key (32 bytes) stored in the clear so redeemers can read it
        // without decrypting the master secret.
        public_key:       vector<u8>,
        // Seal-IBE-encrypted master secret bytes (AES-256-GCM ciphertext + IBE KEM).
        // Small enough (~300 bytes) to store directly on-chain instead of on Walrus.
        encrypted_secret: vector<u8>,
    }

    // Called by Seal key servers via dry_run_transaction_block to authorise decryption.
    // `id` must equal the BCS serialisation of the sender's address (32 raw bytes).
    public fun seal_approve(id: vector<u8>, ctx: &TxContext) {
        assert!(id == bcs::to_bytes(&tx_context::sender(ctx)), 0);
    }

    // Creates a UserSecret owned by the caller.
    // Invoke once after generating and Seal-encrypting the master secret.
    public entry fun register_secret(
        public_key:       vector<u8>,
        encrypted_secret: vector<u8>,
        ctx:              &mut TxContext,
    ) {
        transfer::transfer(
            UserSecret { id: object::new(ctx), public_key, encrypted_secret },
            tx_context::sender(ctx),
        );
    }

    // Replaces the stored secret (e.g. on key rotation).
    public entry fun update_secret(
        self:                 &mut UserSecret,
        new_public_key:       vector<u8>,
        new_encrypted_secret: vector<u8>,
    ) {
        self.public_key       = new_public_key;
        self.encrypted_secret = new_encrypted_secret;
    }
}
