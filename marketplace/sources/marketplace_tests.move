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
    const ONE_HOUR_MS: u64 = 3_600_000;

    /// Bootstrap the shared Marketplace via the module initializer.
    fun setup(scenario: &mut Scenario) {
        marketplace::init_for_testing(test_scenario::ctx(scenario));
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
            let mut mp = test_scenario::take_shared<Marketplace>(&scenario);
            marketplace::create_listing(
                &mut mp,
                b"My Service",
                b"127.0.0.1:8080",
                PRICE_MIST,
                ONE_HOUR_MS,
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
            let mut mp = test_scenario::take_shared<Marketplace>(&scenario);
            let id = marketplace::create_listing(
                &mut mp,
                b"My Service",
                b"127.0.0.1:8080",
                PRICE_MIST,
                ONE_HOUR_MS,
                test_scenario::ctx(&mut scenario),
            );
            test_scenario::return_shared(mp);
            id
        };

        // Buyer purchases
        test_scenario::next_tx(&mut scenario, BUYER);
        {
            let mut mp = test_scenario::take_shared<Marketplace>(&scenario);
            let mut clock = clock::create_for_testing(test_scenario::ctx(&mut scenario));
            clock::set_for_testing(&mut clock, 1_000_000);

            let mut payment = coin::mint_for_testing<SUI>(
                1_000_000_000,
                test_scenario::ctx(&mut scenario),
            );

            marketplace::purchase(
                &mut mp,
                listing_id,
                &mut payment,
                &clock,
                0,
                0,
                test_scenario::ctx(&mut scenario),
            );

            // Remainder after split (1 SUI - 0.5 SUI = 0.5 SUI)
            assert!(coin::value(&payment) == 500_000_000, 0);

            coin::burn_for_testing(payment);
            clock::destroy_for_testing(clock);
            test_scenario::return_shared(mp);
        };

        // Verify buyer owns an AccessToken with correct fields
        test_scenario::next_tx(&mut scenario, BUYER);
        {
            let token = test_scenario::take_from_sender<AccessToken>(&scenario);
            assert!(marketplace::token_listing_id(&token) == listing_id, 0);
            // expires_at = 1_000_000 + 3_600_000 = 4_600_000
            assert!(marketplace::token_expires_at(&token) == 4_600_000, 1);
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
            let mut mp = test_scenario::take_shared<Marketplace>(&scenario);
            let id = marketplace::create_listing(
                &mut mp,
                b"My Service",
                b"127.0.0.1:8080",
                PRICE_MIST,
                ONE_HOUR_MS,
                test_scenario::ctx(&mut scenario),
            );
            test_scenario::return_shared(mp);
            id
        };

        // Seller deactivates the listing
        test_scenario::next_tx(&mut scenario, SELLER);
        {
            let mut mp = test_scenario::take_shared<Marketplace>(&scenario);
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
            let mut mp = test_scenario::take_shared<Marketplace>(&scenario);
            let mut clock = clock::create_for_testing(test_scenario::ctx(&mut scenario));
            let mut payment = coin::mint_for_testing<SUI>(1_000_000_000, test_scenario::ctx(&mut scenario));

            marketplace::purchase(&mut mp, listing_id, &mut payment, &clock, 0, 0, test_scenario::ctx(&mut scenario));

            coin::burn_for_testing(payment);
            clock::destroy_for_testing(clock);
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
            let mut mp = test_scenario::take_shared<Marketplace>(&scenario);
            let id = marketplace::create_listing(
                &mut mp,
                b"My Service",
                b"127.0.0.1:8080",
                PRICE_MIST,
                ONE_HOUR_MS,
                test_scenario::ctx(&mut scenario),
            );
            test_scenario::return_shared(mp);
            id
        };

        test_scenario::next_tx(&mut scenario, BUYER);
        {
            let mut mp = test_scenario::take_shared<Marketplace>(&scenario);
            let mut clock = clock::create_for_testing(test_scenario::ctx(&mut scenario));
            // Only 100 MIST — not enough
            let mut payment = coin::mint_for_testing<SUI>(100, test_scenario::ctx(&mut scenario));

            marketplace::purchase(&mut mp, listing_id, &mut payment, &clock, 0, 0, test_scenario::ctx(&mut scenario));

            coin::burn_for_testing(payment);
            clock::destroy_for_testing(clock);
            test_scenario::return_shared(mp);
        };

        test_scenario::end(scenario);
    }

    // =========================================================
    // Test 5: is_valid reflects expiry
    // =========================================================
    #[test]
    fun test_token_validity() {
        let mut scenario = test_scenario::begin(SELLER);
        setup(&mut scenario);

        test_scenario::next_tx(&mut scenario, SELLER);
        let listing_id = {
            let mut mp = test_scenario::take_shared<Marketplace>(&scenario);
            let id = marketplace::create_listing(
                &mut mp,
                b"My Service",
                b"127.0.0.1:8080",
                PRICE_MIST,
                ONE_HOUR_MS,
                test_scenario::ctx(&mut scenario),
            );
            test_scenario::return_shared(mp);
            id
        };

        // Buyer purchases at t=1_000; expires_at = 1_000 + 3_600_000 = 3_601_000
        test_scenario::next_tx(&mut scenario, BUYER);
        {
            let mut mp = test_scenario::take_shared<Marketplace>(&scenario);
            let mut clock = clock::create_for_testing(test_scenario::ctx(&mut scenario));
            clock::set_for_testing(&mut clock, 1_000);
            let mut payment = coin::mint_for_testing<SUI>(1_000_000_000, test_scenario::ctx(&mut scenario));
            marketplace::purchase(&mut mp, listing_id, &mut payment, &clock, 0, 0, test_scenario::ctx(&mut scenario));
            coin::burn_for_testing(payment);
            clock::destroy_for_testing(clock);
            test_scenario::return_shared(mp);
        };

        test_scenario::next_tx(&mut scenario, BUYER);
        {
            let token = test_scenario::take_from_sender<AccessToken>(&scenario);
            let mut clock = clock::create_for_testing(test_scenario::ctx(&mut scenario));

            clock::set_for_testing(&mut clock, 1_000);
            assert!(marketplace::is_valid(&token, &clock), 0);

            // Advance clock past expiry
            clock::set_for_testing(&mut clock, 4_000_000);
            assert!(!marketplace::is_valid(&token, &clock), 1);

            clock::destroy_for_testing(clock);
            test_scenario::return_to_sender(&scenario, token);
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
            let mut mp = test_scenario::take_shared<Marketplace>(&scenario);
            let id = marketplace::create_listing(
                &mut mp,
                b"My Service",
                b"127.0.0.1:8080",
                PRICE_MIST,
                ONE_HOUR_MS,
                test_scenario::ctx(&mut scenario),
            );
            test_scenario::return_shared(mp);
            id
        };

        test_scenario::next_tx(&mut scenario, BUYER);
        {
            let mut mp = test_scenario::take_shared<Marketplace>(&scenario);
            let mut clock = clock::create_for_testing(test_scenario::ctx(&mut scenario));
            clock::set_for_testing(&mut clock, 1_000);
            let mut payment = coin::mint_for_testing<SUI>(1_000_000_000, test_scenario::ctx(&mut scenario));
            marketplace::purchase(&mut mp, listing_id, &mut payment, &clock, 0, 0, test_scenario::ctx(&mut scenario));
            coin::burn_for_testing(payment);
            clock::destroy_for_testing(clock);
            test_scenario::return_shared(mp);
        };

        // Redeem the token — it is consumed by value and deleted on-chain
        test_scenario::next_tx(&mut scenario, BUYER);
        {
            let token = test_scenario::take_from_sender<AccessToken>(&scenario);
            let clock = clock::create_for_testing(test_scenario::ctx(&mut scenario));
            marketplace::redeem(token, &clock, b"127.0.0.1", test_scenario::ctx(&mut scenario));
            clock::destroy_for_testing(clock);
        };

        test_scenario::end(scenario);
    }

    // =========================================================
    // Test 7: redeeming an expired token fails
    // =========================================================
    #[test]
    #[expected_failure(abort_code = marketplace::ETokenInvalid)]
    fun test_redeem_expired_token_fails() {
        let mut scenario = test_scenario::begin(SELLER);
        setup(&mut scenario);

        test_scenario::next_tx(&mut scenario, SELLER);
        let listing_id = {
            let mut mp = test_scenario::take_shared<Marketplace>(&scenario);
            let id = marketplace::create_listing(
                &mut mp,
                b"My Service",
                b"127.0.0.1:8080",
                PRICE_MIST,
                ONE_HOUR_MS,
                test_scenario::ctx(&mut scenario),
            );
            test_scenario::return_shared(mp);
            id
        };

        // Purchase at t=1_000; expires_at = 3_601_000
        test_scenario::next_tx(&mut scenario, BUYER);
        {
            let mut mp = test_scenario::take_shared<Marketplace>(&scenario);
            let mut clock = clock::create_for_testing(test_scenario::ctx(&mut scenario));
            clock::set_for_testing(&mut clock, 1_000);
            let mut payment = coin::mint_for_testing<SUI>(1_000_000_000, test_scenario::ctx(&mut scenario));
            marketplace::purchase(&mut mp, listing_id, &mut payment, &clock, 0, 0, test_scenario::ctx(&mut scenario));
            coin::burn_for_testing(payment);
            clock::destroy_for_testing(clock);
            test_scenario::return_shared(mp);
        };

        // Attempt to redeem after expiry — should abort with ETokenInvalid
        test_scenario::next_tx(&mut scenario, BUYER);
        {
            let token = test_scenario::take_from_sender<AccessToken>(&scenario);
            let mut clock = clock::create_for_testing(test_scenario::ctx(&mut scenario));
            clock::set_for_testing(&mut clock, 4_000_000); // past expiry
            marketplace::redeem(token, &clock, b"127.0.0.1", test_scenario::ctx(&mut scenario));
            clock::destroy_for_testing(clock);
        };

        test_scenario::end(scenario);
    }

    // =========================================================
    // Test 8: delist prevents purchase
    // =========================================================
    #[test]
    #[expected_failure]
    fun test_delist_prevents_purchase() {
        let mut scenario = test_scenario::begin(SELLER);
        setup(&mut scenario);

        test_scenario::next_tx(&mut scenario, SELLER);
        let listing_id = {
            let mut mp = test_scenario::take_shared<Marketplace>(&scenario);
            let id = marketplace::create_listing(
                &mut mp,
                b"My Service",
                b"127.0.0.1:8080",
                PRICE_MIST,
                ONE_HOUR_MS,
                test_scenario::ctx(&mut scenario),
            );
            test_scenario::return_shared(mp);
            id
        };

        // Seller delists
        test_scenario::next_tx(&mut scenario, SELLER);
        {
            let mut mp = test_scenario::take_shared<Marketplace>(&scenario);
            marketplace::delist(&mut mp, listing_id, test_scenario::ctx(&mut scenario));
            test_scenario::return_shared(mp);
        };

        // Buyer tries to purchase the delisted listing — bag lookup aborts
        test_scenario::next_tx(&mut scenario, BUYER);
        {
            let mut mp = test_scenario::take_shared<Marketplace>(&scenario);
            let mut clock = clock::create_for_testing(test_scenario::ctx(&mut scenario));
            let mut payment = coin::mint_for_testing<SUI>(1_000_000_000, test_scenario::ctx(&mut scenario));
            marketplace::purchase(&mut mp, listing_id, &mut payment, &clock, 0, 0, test_scenario::ctx(&mut scenario));
            coin::burn_for_testing(payment);
            clock::destroy_for_testing(clock);
            test_scenario::return_shared(mp);
        };

        test_scenario::end(scenario);
    }
