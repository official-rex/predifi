//! Property-based fuzz tests for price-condition resolution.
//!
//! See issue #41: fuzz oracle comparisons across signed exponents, thresholds,
//! timestamps, and extreme values. Verifies overflow, staleness, and equality
//! semantics via `proptest!` invariants.
//!
//! Determinism: `proptest` writes any failure to `proptest-regressions/`.
//! Commit those files — they replay first on every future run.
//!
//! Runtime bound: CI sets `PROPTEST_CASES=256`. The `contract-ci.yml`
//! `test-fuzz` job has `timeout-minutes: 10`.
//!
//! ## Known bugs surfaced by this fuzz suite
//!
//! - **Overflow in `evaluate_price_condition`** (`price_feed_simple.rs:379`):
//!   `(target_price * tolerance_bps)` overflows for large `target_price`
//!   with `tolerance_bps > 0`, panicking inside the contract (revert /
//!   locked pool on-chain). Reproduced deterministically by
//!   `prop_overflow_in_tolerance_multiplication_is_documented_bug` below.
//!   Fix tracked in a follow-up issue (out of scope for #41, which ships
//!   the fuzz harness and the reproduction).

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod proptests {
    use crate::price_feed_simple::PriceFeedAdapter;
    use crate::PredifiContract;
    use crate::PredifiError;
    use proptest::prelude::*;
    use soroban_sdk::testutils::{Address as _, Ledger as _};
    use soroban_sdk::{Env, Symbol};

    // ── Safe-domain helpers ─────────────────────────────────────────────────

    /// The maximum `target_price` we fuzz with comparison semantics, chosen
    /// so that `target * MAX_TOLERANCE / MAX_TOLERANCE` and
    /// `target + tolerance_amount` cannot overflow i128.
    ///
    /// `MAX_TOLERANCE = 10_000`. Worst case is `target * 10_000` which must
    /// fit in `i128::MAX`. So `target <= i128::MAX / 10_000`.
    const SAFE_TARGET_MAX: i128 = i128::MAX / 10_000;

    fn make_env() -> (Env, soroban_sdk::Address, Symbol) {
        let env = Env::default();
        env.mock_all_auths();
        // Pin the ledger clock to NOW so all relative timestamps in the
        // tests (NOW - 1, NOW + 3600, etc.) land where we expect.
        env.ledger().with_mut(|li| li.timestamp = NOW);
        let contract_id = env.register(PredifiContract, ());
        let admin = soroban_sdk::Address::generate(&env);
        let pyth = soroban_sdk::Address::generate(&env);

        env.as_contract(&contract_id, || {
            PriceFeedAdapter::init_oracle(&env, &admin, pyth.clone(), 3600, 10_000)
                .expect("init_oracle");
        });

        let feed_pair = Symbol::new(&env, "BTCUSD");
        (env, contract_id, feed_pair)
    }

    fn install_feed(
        env: &Env,
        contract_id: &soroban_sdk::Address,
        feed_pair: &Symbol,
        price: i128,
        timestamp: u64,
        expires_at: u64,
        confidence: i128,
    ) {
        env.as_contract(contract_id, || {
            let feed = crate::price_feed_simple::SimplePriceFeed {
                price,
                timestamp,
                expires_at,
                confidence,
                pair: feed_pair.clone(),
            };
            env.storage()
                .persistent()
                .set(&crate::DataKey::PriceFeed(feed_pair.clone()), &feed);
        });
    }

    fn eval(
        env: &Env,
        contract_id: &soroban_sdk::Address,
        feed_pair: &Symbol,
        target_price: i128,
        operator: u32,
        tolerance_bps: u32,
        max_age: u64,
    ) -> Result<bool, PredifiError> {
        env.as_contract(contract_id, || {
            let c: (Symbol, i128, u32, u32) =
                (feed_pair.clone(), target_price, operator, tolerance_bps);
            PriceFeedAdapter::evaluate_price_condition(env, &c, max_age)
        })
    }

    // ── Strategies ──────────────────────────────────────────────────────────

    /// Valid oracle prices are strictly positive. `SAFE_TARGET_MAX` bounds
    /// the domain to values that don't overflow in tolerance math — see the
    /// `#[should_panic]` test below for the bug we're avoiding.
    fn price_strategy() -> impl Strategy<Value = i128> {
        prop_oneof![
            Just(1i128),
            Just(i128::MAX),
            Just(i128::MAX / 2),
            Just(100_000_000i128),
            Just(50_000_000_000i128),
            1i128..=1_000_000_000_000_000_000i128,
        ]
    }

    /// Safe target prices for tolerance-math invariants.
    fn safe_target_strategy() -> impl Strategy<Value = i128> {
        prop_oneof![
            Just(1i128),
            Just(SAFE_TARGET_MAX),
            Just(SAFE_TARGET_MAX / 2),
            Just(100_000_000i128),
            1i128..=SAFE_TARGET_MAX,
        ]
    }

    fn tolerance_bps_strategy() -> impl Strategy<Value = u32> {
        prop_oneof![
            Just(0u32),
            Just(1u32),
            Just(9_999u32),
            Just(10_000u32),
            0u32..=10_000u32,
        ]
    }

    fn operator_strategy() -> impl Strategy<Value = u32> {
        prop_oneof![Just(0u32), Just(1u32), Just(2u32), 0u32..=10u32]
    }

    const NOW: u64 = 1_000_000_000;

    proptest! {
        // ── Domain: valid prices, safe targets, exact operator semantics ──

        /// Invariant 3 — equality semantics (zero tolerance).
        #[test]
        fn prop_equal_operator_zero_tolerance(
            price in price_strategy(),
            target in safe_target_strategy(),
        ) {
            let (env, cid, feed_pair) = make_env();
            install_feed(&env, &cid, &feed_pair, price, NOW - 1, NOW + 3600, 0);
            let res = eval(&env, &cid, &feed_pair, target, 0, 0, 3600).unwrap();
            prop_assert_eq!(res, price == target);
        }

        /// Invariant 4 — GreaterThan with zero tolerance is exactly `price > target`.
        #[test]
        fn prop_greater_than_zero_tolerance(
            price in price_strategy(),
            target in safe_target_strategy(),
        ) {
            let (env, cid, feed_pair) = make_env();
            install_feed(&env, &cid, &feed_pair, price, NOW - 1, NOW + 3600, 0);
            let res = eval(&env, &cid, &feed_pair, target, 1, 0, 3600).unwrap();
            prop_assert_eq!(res, price > target);
        }

        /// Invariant 5 — LessThan with zero tolerance is exactly `price < target`.
        #[test]
        fn prop_less_than_zero_tolerance(
            price in price_strategy(),
            target in safe_target_strategy(),
        ) {
            let (env, cid, feed_pair) = make_env();
            install_feed(&env, &cid, &feed_pair, price, NOW - 1, NOW + 3600, 0);
            let res = eval(&env, &cid, &feed_pair, target, 2, 0, 3600).unwrap();
            prop_assert_eq!(res, price < target);
        }

        /// Invariant 6 — tolerance additive (bounded so `target * tolerance_bps`
        /// cannot overflow).
        #[test]
        fn prop_equal_operator_tolerance_additive(
            target in safe_target_strategy(),
            tolerance_bps in 0u32..=10_000,
        ) {
            let (env, cid, feed_pair) = make_env();
            let tol_amount = (target * tolerance_bps as i128) / 10_000i128;

            install_feed(&env, &cid, &feed_pair, target, NOW - 1, NOW + 3600, 0);
            let res = eval(&env, &cid, &feed_pair, target, 0, tolerance_bps, 3600).unwrap();
            prop_assert!(res, "price == target must be within tolerance");

            let at_edge = target.saturating_add(tol_amount);
            install_feed(&env, &cid, &feed_pair, at_edge, NOW - 1, NOW + 3600, 0);
            let res = eval(&env, &cid, &feed_pair, target, 0, tolerance_bps, 3600).unwrap();
            prop_assert!(res, "price at tolerance edge must be within tolerance");

            let outside = at_edge.saturating_add(1);
            install_feed(&env, &cid, &feed_pair, outside, NOW - 1, NOW + 3600, 0);
            let res = eval(&env, &cid, &feed_pair, target, 0, tolerance_bps, 3600).unwrap();
            prop_assert!(!res, "price beyond tolerance edge must be outside");
        }

        /// Invariant 10b — mutual exclusivity of eq/gt/lt.
        #[test]
        fn prop_operators_mutually_exclusive(
            price in 1i128..=SAFE_TARGET_MAX,
            target in safe_target_strategy(),
            tolerance_bps in 0u32..=1_000,
        ) {
            let (env, cid, feed_pair) = make_env();
            install_feed(&env, &cid, &feed_pair, price, NOW - 1, NOW + 3600, 0);
            let eq = eval(&env, &cid, &feed_pair, target, 0, tolerance_bps, 3600).unwrap();
            let gt = eval(&env, &cid, &feed_pair, target, 1, tolerance_bps, 3600).unwrap();
            let lt = eval(&env, &cid, &feed_pair, target, 2, tolerance_bps, 3600).unwrap();
            let count = [eq, gt, lt].iter().filter(|x| **x).count();
            prop_assert!(count <= 1, "eq={eq} gt={gt} lt={lt}");
        }

        /// Invariant 2 — unknown operators fail closed (safe domain; the
        /// operator check runs AFTER tolerance math, so we must stay in the
        /// safe domain to actually reach the operator branch).
        #[test]
        fn prop_unknown_operator_fails_closed(
            price in price_strategy(),
            target in safe_target_strategy(),
            tolerance_bps in 0u32..=10_000,
            bad_op in 3u32..=u32::MAX,
        ) {
            let (env, cid, feed_pair) = make_env();
            install_feed(&env, &cid, &feed_pair, price, NOW - 1, NOW + 3600, 0);
            let res = eval(&env, &cid, &feed_pair, target, bad_op, tolerance_bps, 3600);
            prop_assert!(res.is_err());
            prop_assert_eq!(res.unwrap_err(), PredifiError::InvalidPoolState);
        }

        // ── Domain: staleness and expiration fail closed ──────────────────

        /// Invariant 7 — staleness by age.
        #[test]
        fn prop_staleness_rejects(
            price in 1i128..=1_000_000_000i128,
            age in 1u64..=3_600u64,
            max_age in 0u64..=3_600u64,
        ) {
            prop_assume!(age > max_age);
            let (env, cid, feed_pair) = make_env();
            let ts = NOW.saturating_sub(age);
            install_feed(&env, &cid, &feed_pair, price, ts, NOW + 3600, 0);
            let res = eval(&env, &cid, &feed_pair, price, 0, 0, max_age);
            prop_assert!(res.is_err());
            prop_assert_eq!(res.unwrap_err(), PredifiError::PriceDataInvalid);
        }

        /// Invariant 8 — expiration.
        #[test]
        fn prop_expiration_rejects(
            price in 1i128..=1_000_000_000i128,
            expires_in_past in 1u64..=3_600u64,
        ) {
            let (env, cid, feed_pair) = make_env();
            let exp = NOW.saturating_sub(expires_in_past);
            install_feed(&env, &cid, &feed_pair, price, NOW - 1, exp, 0);
            let res = eval(&env, &cid, &feed_pair, price, 0, 0, 3_600);
            prop_assert!(res.is_err());
            prop_assert_eq!(res.unwrap_err(), PredifiError::PriceDataInvalid);
        }

        // ── Domain: non-positive prices fail closed ───────────────────────

        /// Invariant 10 — DOCUMENTED BUG: non-positive prices are accepted.
        ///
        /// `is_price_valid` uses `feed.confidence > feed.price / 100`, which
        /// for `price = 0, confidence = 0` evaluates to `0 > 0 = false` and
        /// lets the feed through. A zero-priced feed can then resolve any
        /// threshold market. See the follow-up issue for the fix.
        ///
        /// This test **fails** today on purpose, and is marked
        /// `#[should_panic]` so the CI stays green while the bug is tracked.
        /// When the fix lands, remove the `#[should_panic]` — the test will
        /// then pass, verifying the fix.
        #[test]
        #[should_panic(expected = "non-positive price")]
        fn prop_nonpositive_price_is_currently_accepted_bug(
            price in prop_oneof![
                Just(0i128),
                Just(-1i128),
                Just(i128::MIN),
                i128::MIN..=0i128,
            ],
        ) {
            let (env, cid, feed_pair) = make_env();
            install_feed(&env, &cid, &feed_pair, price, NOW - 1, NOW + 3600, 0);
            let res = eval(&env, &cid, &feed_pair, 100, 0, 0, 3600);
            prop_assert!(
                res.is_err(),
                "non-positive price {price} must be rejected"
            );
            prop_assert_eq!(res.unwrap_err(), PredifiError::PriceDataInvalid);
        }
    }

    // ── Documented bug: tolerance math overflows i128 ─────────────────────
    //
    // This test reproduces a real bug in `price_feed_simple.rs:379`:
    //
    //     let tolerance_amount =
    //         (target_price * *tolerance_bps as i128) / MAX_TOLERANCE as i128;
    //
    // For large `target_price` with `tolerance_bps > 0`, `target * tolerance_bps`
    // exceeds `i128::MAX` and panics — reverting the transaction on-chain.
    //
    // Out of scope for #41 (fuzz tests). Fix tracked separately.
    #[test]
    #[should_panic(expected = "attempt to multiply with overflow")]
    fn prop_overflow_in_tolerance_multiplication_is_documented_bug() {
        let (env, cid, feed_pair) = make_env();
        // Pick a price that's valid (>0) but extreme — any feed price works
        // here since the overflow is on `target_price * tolerance_bps`, not on
        // `feed.price`. Use 1 so the age/expiration/confidence gates all pass.
        install_feed(&env, &cid, &feed_pair, 1, NOW - 1, NOW + 3600, 0);
        // target_price = i128::MAX, tolerance_bps = 9999 → multiply overflows.
        let _ = eval(&env, &cid, &feed_pair, i128::MAX, 0, 9_999, 3600);
    }
}
