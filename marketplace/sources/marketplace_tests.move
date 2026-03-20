#[test_only]
module marketplace::marketplace_tests;
    use sui::test_scenario::{Self, Scenario};
    use sui::coin;
    use sui::sui::SUI;
    use sui::clock;
    use marketplace::marketplace::{Self, Marketplace, AccessToken};

    const SELLER: address = @0xA;
    const BUYER:  address = @0xB;

    const PRICE_MIST: u64 = 500_000_000; // 0.5 SUI

    // Seller bounds used across tests:
    // valid_from_ms  = 0           (no "not before" restriction)
    // expires_at_ms  = 4_000_000   (fixed future ceiling)
    // bandwidth_bps  = 10_000_000  (10 MB/s ceiling; 0 in some tests = unlimited)
    const SELLER_EXPIRES_AT:   u64 = 4_000_000;
    const SELLER_BANDWIDTH:    u64 = 10_000_000; // 10 MB/s

    /// Create a SUI-denominated marketplace for testing.
    fun setup(scenario: &mut Scenario) {
        marketplace::create_marketplace<SUI>(test_scenario::ctx(scenario));
    }

    // =========================================================
    // Test 1: create a listing
    // =========================================================
    #[test]
    fun test_create_listing() {
        let mut scenario = test_scenario::begin(SELLER);
        setup(&mut scenario);

        test_scenario::next_tx(&mut scenario, SELLER);
        {
            let mut mp = test_scenario::take_shared<Marketplace<SUI>>(&scenario);
            marketplace::create_listing(
                &mut mp,
                b"My Service",
                b"127.0.0.1:8080",
                PRICE_MIST,
                0,
                SELLER_EXPIRES_AT,
                0, // max_bandwidth_bps: 0 = unlimited
                test_scenario::ctx(&mut scenario),
            );
            test_scenario::return_shared(mp);
        };

        test_scenario::end(scenario);
    }

    // =========================================================
    // Test 2: successful purchase
    // =========================================================
    #[test]
    fun test_purchase() {
        let mut scenario = test_scenario::begin(SELLER);
        setup(&mut scenario);

        // Seller creates listing
        test_scenario::next_tx(&mut scenario, SELLER);
        let listing_id = {
            let mut mp = test_scenario::take_shared<Marketplace<SUI>>(&scenario);
            let id = marketplace::create_listing(
                &mut mp,
                b"My Service",
                b"127.0.0.1:8080",
                PRICE_MIST,
                0,
                SELLER_EXPIRES_AT,
                0, // max_bandwidth_bps: 0 = unlimited
                test_scenario::ctx(&mut scenario),
            );
            test_scenario::return_shared(mp);
            id
        };

        // Buyer purchases with resolved values: start=1_000_000, end=SELLER_EXPIRES_AT, bw=0
        test_scenario::next_tx(&mut scenario, BUYER);
        {
            let mut mp = test_scenario::take_shared<Marketplace<SUI>>(&scenario);
            let mut payment = coin::mint_for_testing<SUI>(
                1_000_000_000,
                test_scenario::ctx(&mut scenario),
            );

            marketplace::purchase(
                &mut mp,
                listing_id,
                &mut payment,
                1_000_000,
                SELLER_EXPIRES_AT,
                0,
                test_scenario::ctx(&mut scenario),
            );

            // Remainder after split (1 SUI - 0.5 SUI = 0.5 SUI)
            assert!(coin::value(&payment) == 500_000_000, 0);

            coin::burn_for_testing(payment);
            test_scenario::return_shared(mp);
        };

        // Verify buyer owns an AccessToken with correct fields.
        test_scenario::next_tx(&mut scenario, BUYER);
        {
            let token = test_scenario::take_from_sender<AccessToken>(&scenario);
            assert!(marketplace::token_listing_id(&token) == listing_id, 0);
            assert!(marketplace::token_expires_at(&token) == SELLER_EXPIRES_AT, 1);
            test_scenario::return_to_sender(&scenario, token);
        };

        test_scenario::end(scenario);
    }

    // =========================================================
    // Test 3: purchase fails when listing is inactive
    // =========================================================
    #[test]
    #[expected_failure(abort_code = marketplace::EListingNotActive)]
    fun test_purchase_inactive_listing() {
        let mut scenario = test_scenario::begin(SELLER);
        setup(&mut scenario);

        test_scenario::next_tx(&mut scenario, SELLER);
        let listing_id = {
            let mut mp = test_scenario::take_shared<Marketplace<SUI>>(&scenario);
            let id = marketplace::create_listing(
                &mut mp,
                b"My Service",
                b"127.0.0.1:8080",
                PRICE_MIST,
                0,
                SELLER_EXPIRES_AT,
                0, // max_bandwidth_bps: 0 = unlimited
                test_scenario::ctx(&mut scenario),
            );
            test_scenario::return_shared(mp);
            id
        };

        // Seller deactivates the listing
        test_scenario::next_tx(&mut scenario, SELLER);
        {
            let mut mp = test_scenario::take_shared<Marketplace<SUI>>(&scenario);
            marketplace::update_listing(
                &mut mp,
                listing_id,
                PRICE_MIST,
                false,
                test_scenario::ctx(&mut scenario),
            );
            test_scenario::return_shared(mp);
        };

        // Buyer tries to purchase — should abort with EListingNotActive
        test_scenario::next_tx(&mut scenario, BUYER);
        {
            let mut mp = test_scenario::take_shared<Marketplace<SUI>>(&scenario);
            let mut payment = coin::mint_for_testing<SUI>(1_000_000_000, test_scenario::ctx(&mut scenario));

            marketplace::purchase(&mut mp, listing_id, &mut payment, 0, SELLER_EXPIRES_AT, 0, test_scenario::ctx(&mut scenario));

            coin::burn_for_testing(payment);
            test_scenario::return_shared(mp);
        };

        test_scenario::end(scenario);
    }

    // =========================================================
    // Test 4: purchase fails with insufficient payment
    // =========================================================
    #[test]
    #[expected_failure(abort_code = marketplace::EInsufficientPayment)]
    fun test_purchase_insufficient_payment() {
        let mut scenario = test_scenario::begin(SELLER);
        setup(&mut scenario);

        test_scenario::next_tx(&mut scenario, SELLER);
        let listing_id = {
            let mut mp = test_scenario::take_shared<Marketplace<SUI>>(&scenario);
            let id = marketplace::create_listing(
                &mut mp,
                b"My Service",
                b"127.0.0.1:8080",
                PRICE_MIST,
                0,
                SELLER_EXPIRES_AT,
                0, // max_bandwidth_bps: 0 = unlimited
                test_scenario::ctx(&mut scenario),
            );
            test_scenario::return_shared(mp);
            id
        };

        test_scenario::next_tx(&mut scenario, BUYER);
        {
            let mut mp = test_scenario::take_shared<Marketplace<SUI>>(&scenario);
            // Only 100 MIST — not enough
            let mut payment = coin::mint_for_testing<SUI>(100, test_scenario::ctx(&mut scenario));

            marketplace::purchase(&mut mp, listing_id, &mut payment, 0, SELLER_EXPIRES_AT, 0, test_scenario::ctx(&mut scenario));

            coin::burn_for_testing(payment);
            test_scenario::return_shared(mp);
        };

        test_scenario::end(scenario);
    }

    // =========================================================
    // Test 6: successful redemption destroys the token
    // =========================================================
    #[test]
    fun test_redeem() {
        let mut scenario = test_scenario::begin(SELLER);
        setup(&mut scenario);

        test_scenario::next_tx(&mut scenario, SELLER);
        let listing_id = {
            let mut mp = test_scenario::take_shared<Marketplace<SUI>>(&scenario);
            let id = marketplace::create_listing(
                &mut mp,
                b"My Service",
                b"127.0.0.1:8080",
                PRICE_MIST,
                0,
                SELLER_EXPIRES_AT,
                0, // max_bandwidth_bps: 0 = unlimited
                test_scenario::ctx(&mut scenario),
            );
            test_scenario::return_shared(mp);
            id
        };

        test_scenario::next_tx(&mut scenario, BUYER);
        {
            let mut mp = test_scenario::take_shared<Marketplace<SUI>>(&scenario);
            let mut payment = coin::mint_for_testing<SUI>(1_000_000_000, test_scenario::ctx(&mut scenario));
            marketplace::purchase(&mut mp, listing_id, &mut payment, 1_000, SELLER_EXPIRES_AT, 0, test_scenario::ctx(&mut scenario));
            coin::burn_for_testing(payment);
            test_scenario::return_shared(mp);
        };

        // Redeem the token — it is consumed by value and deleted on-chain
        test_scenario::next_tx(&mut scenario, BUYER);
        {
            let token = test_scenario::take_from_sender<AccessToken>(&scenario);
            marketplace::redeem(token, b"127.0.0.1", test_scenario::ctx(&mut scenario));
        };

        test_scenario::end(scenario);
    }

    // =========================================================
    // Test 7: delist prevents purchase
    // =========================================================
    #[test]
    #[expected_failure]
    fun test_delist_prevents_purchase() {
        let mut scenario = test_scenario::begin(SELLER);
        setup(&mut scenario);

        test_scenario::next_tx(&mut scenario, SELLER);
        let listing_id = {
            let mut mp = test_scenario::take_shared<Marketplace<SUI>>(&scenario);
            let id = marketplace::create_listing(
                &mut mp,
                b"My Service",
                b"127.0.0.1:8080",
                PRICE_MIST,
                0,
                SELLER_EXPIRES_AT,
                0, // max_bandwidth_bps: 0 = unlimited
                test_scenario::ctx(&mut scenario),
            );
            test_scenario::return_shared(mp);
            id
        };

        // Seller delists
        test_scenario::next_tx(&mut scenario, SELLER);
        {
            let mut mp = test_scenario::take_shared<Marketplace<SUI>>(&scenario);
            marketplace::delist(&mut mp, listing_id, test_scenario::ctx(&mut scenario));
            test_scenario::return_shared(mp);
        };

        // Buyer tries to purchase the delisted listing — bag lookup aborts
        test_scenario::next_tx(&mut scenario, BUYER);
        {
            let mut mp = test_scenario::take_shared<Marketplace<SUI>>(&scenario);
            let mut payment = coin::mint_for_testing<SUI>(1_000_000_000, test_scenario::ctx(&mut scenario));
            marketplace::purchase(&mut mp, listing_id, &mut payment, 0, SELLER_EXPIRES_AT, 0, test_scenario::ctx(&mut scenario));
            coin::burn_for_testing(payment);
            test_scenario::return_shared(mp);
        };

        test_scenario::end(scenario);
    }

    // =========================================================
    // Test 9: buyer out-of-bounds end_ms is rejected
    // =========================================================
    #[test]
    #[expected_failure(abort_code = marketplace::EBuyerOutOfBounds)]
    fun test_buyer_out_of_bounds() {
        let mut scenario = test_scenario::begin(SELLER);
        setup(&mut scenario);

        // Seller creates listing that expires at 4_000_000 ms
        test_scenario::next_tx(&mut scenario, SELLER);
        let listing_id = {
            let mut mp = test_scenario::take_shared<Marketplace<SUI>>(&scenario);
            let id = marketplace::create_listing(
                &mut mp,
                b"My Service",
                b"127.0.0.1:8080",
                PRICE_MIST,
                0,
                SELLER_EXPIRES_AT, // bound: expires at 4_000_000
                0,                 // max_bandwidth_bps: 0 = unlimited
                test_scenario::ctx(&mut scenario),
            );
            test_scenario::return_shared(mp);
            id
        };

        // Buyer attempts to purchase with end_ms beyond seller's bound — should abort
        test_scenario::next_tx(&mut scenario, BUYER);
        {
            let mut mp = test_scenario::take_shared<Marketplace<SUI>>(&scenario);
            let mut payment = coin::mint_for_testing<SUI>(1_000_000_000, test_scenario::ctx(&mut scenario));

            marketplace::purchase(
                &mut mp,
                listing_id,
                &mut payment,
                0,
                5_000_000, // exceeds seller's expires_at_ms of 4_000_000
                0,
                test_scenario::ctx(&mut scenario),
            );

            coin::burn_for_testing(payment);
            test_scenario::return_shared(mp);
        };

        test_scenario::end(scenario);
    }

    // =========================================================
    // Test 10: buyer out-of-bounds bandwidth is rejected
    // =========================================================
    #[test]
    #[expected_failure(abort_code = marketplace::EBandwidthOutOfBounds)]
    fun test_bandwidth_out_of_bounds() {
        let mut scenario = test_scenario::begin(SELLER);
        setup(&mut scenario);

        // Seller creates listing with 10 MB/s ceiling
        test_scenario::next_tx(&mut scenario, SELLER);
        let listing_id = {
            let mut mp = test_scenario::take_shared<Marketplace<SUI>>(&scenario);
            let id = marketplace::create_listing(
                &mut mp,
                b"My Service",
                b"127.0.0.1:8080",
                PRICE_MIST,
                0,
                SELLER_EXPIRES_AT,
                SELLER_BANDWIDTH, // 10 MB/s ceiling
                test_scenario::ctx(&mut scenario),
            );
            test_scenario::return_shared(mp);
            id
        };

        // Buyer attempts to purchase with bandwidth exceeding seller's bound — should abort
        test_scenario::next_tx(&mut scenario, BUYER);
        {
            let mut mp = test_scenario::take_shared<Marketplace<SUI>>(&scenario);
            let mut payment = coin::mint_for_testing<SUI>(1_000_000_000, test_scenario::ctx(&mut scenario));

            marketplace::purchase(
                &mut mp,
                listing_id,
                &mut payment,
                0,
                SELLER_EXPIRES_AT,
                20_000_000, // exceeds seller's 10 MB/s ceiling
                test_scenario::ctx(&mut scenario),
            );

            coin::burn_for_testing(payment);
            test_scenario::return_shared(mp);
        };

        test_scenario::end(scenario);
    }
