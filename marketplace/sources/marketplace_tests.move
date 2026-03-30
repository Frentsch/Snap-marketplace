#[test_only]
module marketplace::marketplace_tests;
    use sui::test_scenario::{Self, Scenario};
    use sui::coin;
    use sui::sui::SUI;
    use marketplace::marketplace::{Self, Marketplace, AccessToken};

    const SELLER: address = @0xA;
    const BUYER:  address = @0xB;

    const PRICE_MIST: u64 = 500_000_000; // 0.5 SUI

    // Seller listing bounds (times in seconds, bandwidth in kB/s):
    const SELLER_EXPIRES_AT:  u64 = 4_000;
    const SELLER_BANDWIDTH:   u64 = 10_000; // 10 MB/s
    const MIN_BANDWIDTH:      u64 = 1_000;  // 1 MB/s
    const BW_GRANULARITY:     u64 = 1_000;  // 1 MB/s steps
    const MIN_DURATION:       u64 = 100;    // 100 s
    const TIME_GRANULARITY:   u64 = 100;    // 100 s steps

    // Buyer purchase values (satisfy all constraints above):
    const BUYER_START:     u64 = 1_000;
    const BUYER_END:       u64 = 4_000;
    const BUYER_BANDWIDTH: u64 = 2_000; // 2 MB/s

    fun setup(scenario: &mut Scenario) {
        marketplace::create_marketplace<SUI>(test_scenario::ctx(scenario));
    }

    fun create_test_listing(mp: &mut Marketplace<SUI>, scenario: &mut Scenario): ID {
        marketplace::create_listing(
            mp,
            b"My Service",
            b"127.0.0.1:8080",
            PRICE_MIST,
            0,
            SELLER_EXPIRES_AT,
            SELLER_BANDWIDTH,
            MIN_BANDWIDTH,
            MIN_DURATION,
            BW_GRANULARITY,
            TIME_GRANULARITY,
            test_scenario::ctx(scenario),
        )
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
            create_test_listing(&mut mp, &mut scenario);
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

        test_scenario::next_tx(&mut scenario, SELLER);
        let listing_id = {
            let mut mp = test_scenario::take_shared<Marketplace<SUI>>(&scenario);
            let id = create_test_listing(&mut mp, &mut scenario);
            test_scenario::return_shared(mp);
            id
        };

        test_scenario::next_tx(&mut scenario, BUYER);
        {
            let mut mp = test_scenario::take_shared<Marketplace<SUI>>(&scenario);
            let mut payment = coin::mint_for_testing<SUI>(1_000_000_000, test_scenario::ctx(&mut scenario));
            marketplace::purchase(&mut mp, listing_id, &mut payment, BUYER_START, BUYER_END, BUYER_BANDWIDTH, test_scenario::ctx(&mut scenario));
            assert!(coin::value(&payment) == 500_000_000, 0);
            coin::burn_for_testing(payment);
            test_scenario::return_shared(mp);
        };

        test_scenario::next_tx(&mut scenario, BUYER);
        {
            let token = test_scenario::take_from_sender<AccessToken>(&scenario);
            assert!(marketplace::token_listing_id(&token) == listing_id, 0);
            assert!(marketplace::token_expires_at(&token) == BUYER_END, 1);
            assert!(marketplace::token_bandwidth(&token) == BUYER_BANDWIDTH, 2);
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
            let id = create_test_listing(&mut mp, &mut scenario);
            test_scenario::return_shared(mp);
            id
        };

        test_scenario::next_tx(&mut scenario, SELLER);
        {
            let mut mp = test_scenario::take_shared<Marketplace<SUI>>(&scenario);
            marketplace::update_listing(&mut mp, listing_id, PRICE_MIST, false, test_scenario::ctx(&mut scenario));
            test_scenario::return_shared(mp);
        };

        test_scenario::next_tx(&mut scenario, BUYER);
        {
            let mut mp = test_scenario::take_shared<Marketplace<SUI>>(&scenario);
            let mut payment = coin::mint_for_testing<SUI>(1_000_000_000, test_scenario::ctx(&mut scenario));
            marketplace::purchase(&mut mp, listing_id, &mut payment, BUYER_START, BUYER_END, BUYER_BANDWIDTH, test_scenario::ctx(&mut scenario));
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
            let id = create_test_listing(&mut mp, &mut scenario);
            test_scenario::return_shared(mp);
            id
        };

        test_scenario::next_tx(&mut scenario, BUYER);
        {
            let mut mp = test_scenario::take_shared<Marketplace<SUI>>(&scenario);
            let mut payment = coin::mint_for_testing<SUI>(100, test_scenario::ctx(&mut scenario));
            marketplace::purchase(&mut mp, listing_id, &mut payment, BUYER_START, BUYER_END, BUYER_BANDWIDTH, test_scenario::ctx(&mut scenario));
            coin::burn_for_testing(payment);
            test_scenario::return_shared(mp);
        };

        test_scenario::end(scenario);
    }

    // =========================================================
    // Test 5: successful redemption destroys the token
    // =========================================================
    #[test]
    fun test_redeem() {
        let mut scenario = test_scenario::begin(SELLER);
        setup(&mut scenario);

        test_scenario::next_tx(&mut scenario, SELLER);
        let listing_id = {
            let mut mp = test_scenario::take_shared<Marketplace<SUI>>(&scenario);
            let id = create_test_listing(&mut mp, &mut scenario);
            test_scenario::return_shared(mp);
            id
        };

        test_scenario::next_tx(&mut scenario, BUYER);
        {
            let mut mp = test_scenario::take_shared<Marketplace<SUI>>(&scenario);
            let mut payment = coin::mint_for_testing<SUI>(1_000_000_000, test_scenario::ctx(&mut scenario));
            marketplace::purchase(&mut mp, listing_id, &mut payment, BUYER_START, BUYER_END, BUYER_BANDWIDTH, test_scenario::ctx(&mut scenario));
            coin::burn_for_testing(payment);
            test_scenario::return_shared(mp);
        };

        test_scenario::next_tx(&mut scenario, BUYER);
        {
            let token = test_scenario::take_from_sender<AccessToken>(&scenario);
            marketplace::redeem(token, b"127.0.0.1", test_scenario::ctx(&mut scenario));
        };

        test_scenario::end(scenario);
    }

    // =========================================================
    // Test 6: delist prevents purchase
    // =========================================================
    #[test]
    #[expected_failure]
    fun test_delist_prevents_purchase() {
        let mut scenario = test_scenario::begin(SELLER);
        setup(&mut scenario);

        test_scenario::next_tx(&mut scenario, SELLER);
        let listing_id = {
            let mut mp = test_scenario::take_shared<Marketplace<SUI>>(&scenario);
            let id = create_test_listing(&mut mp, &mut scenario);
            test_scenario::return_shared(mp);
            id
        };

        test_scenario::next_tx(&mut scenario, SELLER);
        {
            let mut mp = test_scenario::take_shared<Marketplace<SUI>>(&scenario);
            marketplace::delist(&mut mp, listing_id, test_scenario::ctx(&mut scenario));
            test_scenario::return_shared(mp);
        };

        test_scenario::next_tx(&mut scenario, BUYER);
        {
            let mut mp = test_scenario::take_shared<Marketplace<SUI>>(&scenario);
            let mut payment = coin::mint_for_testing<SUI>(1_000_000_000, test_scenario::ctx(&mut scenario));
            marketplace::purchase(&mut mp, listing_id, &mut payment, BUYER_START, BUYER_END, BUYER_BANDWIDTH, test_scenario::ctx(&mut scenario));
            coin::burn_for_testing(payment);
            test_scenario::return_shared(mp);
        };

        test_scenario::end(scenario);
    }

    // =========================================================
    // Test 7: buyer end beyond seller's bound is rejected
    // =========================================================
    #[test]
    #[expected_failure(abort_code = marketplace::EInvalidInterval)]
    fun test_buyer_out_of_bounds() {
        let mut scenario = test_scenario::begin(SELLER);
        setup(&mut scenario);

        test_scenario::next_tx(&mut scenario, SELLER);
        let listing_id = {
            let mut mp = test_scenario::take_shared<Marketplace<SUI>>(&scenario);
            let id = create_test_listing(&mut mp, &mut scenario);
            test_scenario::return_shared(mp);
            id
        };

        test_scenario::next_tx(&mut scenario, BUYER);
        {
            let mut mp = test_scenario::take_shared<Marketplace<SUI>>(&scenario);
            let mut payment = coin::mint_for_testing<SUI>(1_000_000_000, test_scenario::ctx(&mut scenario));
            marketplace::purchase(&mut mp, listing_id, &mut payment, BUYER_START, 5_000, BUYER_BANDWIDTH, test_scenario::ctx(&mut scenario));
            coin::burn_for_testing(payment);
            test_scenario::return_shared(mp);
        };

        test_scenario::end(scenario);
    }

    // =========================================================
    // Test 8: buyer bandwidth beyond seller's bound is rejected
    // =========================================================
    #[test]
    #[expected_failure(abort_code = marketplace::EInvalidBandwidth)]
    fun test_bandwidth_out_of_bounds() {
        let mut scenario = test_scenario::begin(SELLER);
        setup(&mut scenario);

        test_scenario::next_tx(&mut scenario, SELLER);
        let listing_id = {
            let mut mp = test_scenario::take_shared<Marketplace<SUI>>(&scenario);
            let id = create_test_listing(&mut mp, &mut scenario);
            test_scenario::return_shared(mp);
            id
        };

        test_scenario::next_tx(&mut scenario, BUYER);
        {
            let mut mp = test_scenario::take_shared<Marketplace<SUI>>(&scenario);
            let mut payment = coin::mint_for_testing<SUI>(1_000_000_000, test_scenario::ctx(&mut scenario));
            marketplace::purchase(&mut mp, listing_id, &mut payment, BUYER_START, BUYER_END, 20_000, test_scenario::ctx(&mut scenario));
            coin::burn_for_testing(payment);
            test_scenario::return_shared(mp);
        };

        test_scenario::end(scenario);
    }
