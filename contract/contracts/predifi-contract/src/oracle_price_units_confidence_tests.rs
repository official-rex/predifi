//! Oracle price-unit normalization and confidence-bound tests.
//!
//! - Normalization: provider `(price, confidence, expo)` readings are converted
//!   to `PRICE_DECIMALS` fixed point with checked arithmetic and conservative
//!   rounding (`oracle::normalize_price_feed`).
//! - Confidence: resolution is rejected or deferred when the confidence
//!   interval is too wide or crosses the condition boundary.

#![cfg(test)]

use crate::oracle::normalize_price_feed;
use crate::test::ROLE_ADMIN;
use crate::{
    DataKey, MarketState, PoolConfig, PredifiContract, PredifiContractClient, PredifiError,
    MAX_PRICE_EXPONENT, MIN_PRICE_EXPONENT,
};
use proptest::prelude::*;
use soroban_sdk::{
    symbol_short,
    testutils::{Address as _, Ledger},
    Address, Env, String, Symbol,
};

const EQ: u32 = 0;
const GT: u32 = 1;
const LT: u32 = 2;

fn outcome(
    price: i128,
    confidence: i128,
    target: i128,
    op: u32,
    tol: u32,
) -> Result<u32, PredifiError> {
    PredifiContract::price_resolution_outcome(price, confidence, target, op, tol)
}

// ── Normalization: fixtures ──────────────────────────────────────────────────

#[test]
fn test_normalize_canonical_exponent_is_identity() {
    assert_eq!(
        normalize_price_feed(123_456_789, 5, -8),
        Ok((123_456_789, 5))
    );
}

#[test]
fn test_normalize_scales_up_coarser_exponents() {
    // 12.34567 USD at expo -5.
    assert_eq!(
        normalize_price_feed(1_234_567, 5, -5),
        Ok((1_234_567_000, 5_000))
    );
    // 3000 USD at expo 0.
    assert_eq!(
        normalize_price_feed(3_000, 1, 0),
        Ok((300_000_000_000, 100_000_000))
    );
    // Positive exponent: 30 * 10^2 USD.
    assert_eq!(normalize_price_feed(30, 0, 2), Ok((300_000_000_000, 0)));
}

#[test]
fn test_normalize_scales_down_finer_exponents_exactly() {
    assert_eq!(
        normalize_price_feed(12_345_678_900, 100, -10),
        Ok((123_456_789, 1))
    );
}

#[test]
fn test_normalize_rounds_price_down_and_confidence_up() {
    // Confidence 1.01 units rounds up to 2.
    assert_eq!(
        normalize_price_feed(12_345_678_900, 101, -10),
        Ok((123_456_789, 2))
    );
    // Price remainder dropped: price truncates and confidence widens by one unit.
    assert_eq!(
        normalize_price_feed(12_345_678_999, 0, -10),
        Ok((123_456_789, 1))
    );
    // Both inexact: ceil(0.01) + 1.
    assert_eq!(
        normalize_price_feed(12_345_678_999, 1, -10),
        Ok((123_456_789, 2))
    );
}

#[test]
fn test_normalize_exponent_range_boundaries() {
    assert_eq!(
        normalize_price_feed(10_i128.pow(10), 0, MIN_PRICE_EXPONENT),
        Ok((1, 0))
    );
    assert_eq!(
        normalize_price_feed(1, 0, MAX_PRICE_EXPONENT),
        Ok((10_i128.pow(26), 0))
    );
    assert_eq!(
        normalize_price_feed(10_i128.pow(11), 0, MIN_PRICE_EXPONENT - 1),
        Err(PredifiError::UnsupportedPriceExponent)
    );
    assert_eq!(
        normalize_price_feed(1, 0, MAX_PRICE_EXPONENT + 1),
        Err(PredifiError::UnsupportedPriceExponent)
    );
}

#[test]
fn test_normalize_rejects_overflow() {
    assert_eq!(
        normalize_price_feed(i128::MAX / 10, 0, -7),
        Ok((i128::MAX / 10 * 10, 0))
    );
    assert_eq!(
        normalize_price_feed(i128::MAX / 10 + 1, 0, -7),
        Err(PredifiError::ArithmeticError)
    );
    assert_eq!(
        normalize_price_feed(10_i128.pow(13), 0, MAX_PRICE_EXPONENT),
        Err(PredifiError::ArithmeticError)
    );
    // Overflowing confidence is rejected too.
    assert_eq!(
        normalize_price_feed(1, i128::MAX / 10 + 1, -7),
        Err(PredifiError::ArithmeticError)
    );
}

#[test]
fn test_normalize_rejects_zero_negative_and_sub_unit_prices() {
    assert_eq!(
        normalize_price_feed(0, 0, -8),
        Err(PredifiError::InvalidAmount)
    );
    assert_eq!(
        normalize_price_feed(-1, 0, -8),
        Err(PredifiError::InvalidAmount)
    );
    assert_eq!(
        normalize_price_feed(1, -1, -8),
        Err(PredifiError::InvalidAmount)
    );
    // 99e-18 is below one 1e-8 unit: unsupported precision, not a zero price.
    assert_eq!(
        normalize_price_feed(99, 0, -18),
        Err(PredifiError::UnsupportedPriceExponent)
    );
    // Zero confidence is valid.
    assert_eq!(normalize_price_feed(1, 0, -8), Ok((1, 0)));
}

// ── Normalization: properties ────────────────────────────────────────────────

proptest! {
    /// The same economic price published at any finer exponent normalizes to
    /// the same canonical value.
    #[test]
    fn prop_equivalent_finer_exponents_normalize_identically(
        price in 1..=1_000_000_000_000_000_000i128,
        confidence in 0..=1_000_000_000_000i128,
        k in 0u32..=10,
    ) {
        let f = 10_i128.pow(k);
        let expo = -8 - k as i32;
        prop_assert_eq!(
            normalize_price_feed(price * f, confidence * f, expo),
            Ok((price, confidence))
        );
    }

    /// Coarser exponents scale up exactly.
    #[test]
    fn prop_coarser_exponents_scale_up_exactly(
        price in 1..=1_000_000_000_000_000_000i128,
        confidence in 0..=1_000_000_000_000i128,
        k in 0u32..=10,
    ) {
        let f = 10_i128.pow(k);
        prop_assert_eq!(
            normalize_price_feed(price, confidence, -8 + k as i32),
            Ok((price * f, confidence * f))
        );
    }

    /// Equivalent prices compare identically against any condition, whatever
    /// exponent they were published with.
    #[test]
    fn prop_equivalent_prices_compare_identically_across_exponents(
        price in 1..=1_000_000_000_000i128,
        confidence in 0..=1_000_000i128,
        target in 1..=1_000_000_000_000i128,
        op in 0u32..=2,
        tol in 0u32..=1_000,
        k in 0u32..=10,
    ) {
        let f = 10_i128.pow(k);
        let canonical = outcome(price, confidence, target, op, tol);
        let (p_fine, c_fine) = normalize_price_feed(price * f, confidence * f, -8 - k as i32).unwrap();
        prop_assert_eq!(outcome(p_fine, c_fine, target, op, tol), canonical);
        if k <= 8 && price % f == 0 && confidence % f == 0 {
            let (p_coarse, c_coarse) =
                normalize_price_feed(price / f, confidence / f, -8 + k as i32).unwrap();
            prop_assert_eq!(outcome(p_coarse, c_coarse, target, op, tol), canonical);
        }
    }

    /// Lossy normalization never shrinks the uncertainty interval: the
    /// normalized interval always contains the provider's interval.
    #[test]
    fn prop_lossy_normalization_interval_contains_provider_interval(
        raw_price in 1..=100_000_000_000_000_000_000i128,
        raw_confidence in 0..=1_000_000_000_000i128,
        k in 1u32..=10,
    ) {
        let f = 10_i128.pow(k);
        prop_assume!(raw_price >= f);
        let (p, c) = normalize_price_feed(raw_price, raw_confidence, -8 - k as i32).unwrap();
        prop_assert!((p - c) * f <= raw_price - raw_confidence);
        prop_assert!((p + c) * f >= raw_price + raw_confidence);
    }
}

// ── Confidence: boundary cases per operator (target 1000, confidence 10) ────

#[test]
fn test_confidence_greater_than_boundaries() {
    // just inside: whole interval [1001, 1021] > 1000
    assert_eq!(outcome(1_011, 10, 1_000, GT, 0), Ok(1));
    // exact boundary: low == 1000 is not > 1000 while high is
    assert_eq!(
        outcome(1_010, 10, 1_000, GT, 0),
        Err(PredifiError::PriceConfidenceTooWide)
    );
    // just crossing from below: high == 1001
    assert_eq!(
        outcome(991, 10, 1_000, GT, 0),
        Err(PredifiError::PriceConfidenceTooWide)
    );
    // just outside: whole interval [980, 1000] <= 1000
    assert_eq!(outcome(990, 10, 1_000, GT, 0), Ok(0));
}

#[test]
fn test_confidence_less_than_boundaries() {
    // just inside: whole interval [979, 999] < 1000
    assert_eq!(outcome(989, 10, 1_000, LT, 0), Ok(1));
    // exact boundary: high == 1000 is not < 1000 while low is
    assert_eq!(
        outcome(990, 10, 1_000, LT, 0),
        Err(PredifiError::PriceConfidenceTooWide)
    );
    // just crossing from above: low == 999
    assert_eq!(
        outcome(1_009, 10, 1_000, LT, 0),
        Err(PredifiError::PriceConfidenceTooWide)
    );
    // just outside: whole interval [1000, 1020] >= 1000
    assert_eq!(outcome(1_010, 10, 1_000, LT, 0), Ok(0));
}

#[test]
fn test_confidence_equal_band_boundaries() {
    // 100 bps tolerance around 1000: band [990, 1010].
    // exact fit inside the band
    assert_eq!(outcome(1_000, 10, 1_000, EQ, 100), Ok(1));
    // just inside after a shift
    assert_eq!(outcome(1_001, 9, 1_000, EQ, 100), Ok(1));
    // one unit too wide: interval [989, 1011] crosses both edges
    assert_eq!(
        outcome(1_000, 11, 1_000, EQ, 100),
        Err(PredifiError::PriceConfidenceTooWide)
    );
    // touching the lower edge from outside: high == 990
    assert_eq!(
        outcome(980, 10, 1_000, EQ, 100),
        Err(PredifiError::PriceConfidenceTooWide)
    );
    // just outside below: high == 989
    assert_eq!(outcome(979, 10, 1_000, EQ, 100), Ok(0));
    // touching the upper edge from outside: low == 1010
    assert_eq!(
        outcome(1_020, 10, 1_000, EQ, 100),
        Err(PredifiError::PriceConfidenceTooWide)
    );
    // just outside above: low == 1011
    assert_eq!(outcome(1_021, 10, 1_000, EQ, 100), Ok(0));
}

#[test]
fn test_zero_confidence_keeps_point_comparison() {
    assert_eq!(outcome(1_001, 0, 1_000, GT, 0), Ok(1));
    assert_eq!(outcome(1_000, 0, 1_000, GT, 0), Ok(0));
    assert_eq!(outcome(999, 0, 1_000, LT, 0), Ok(1));
    assert_eq!(outcome(1_000, 0, 1_000, LT, 0), Ok(0));
    assert_eq!(outcome(990, 0, 1_000, EQ, 100), Ok(1));
    assert_eq!(outcome(989, 0, 1_000, EQ, 100), Ok(0));
}

// ── End-to-end through resolve_pool_from_price ───────────────────────────────

struct PricePool<'a> {
    client: PredifiContractClient<'a>,
    oracle: Address,
    operator: Address,
    creator: Address,
    token: Address,
}

/// Oracle with max confidence ratio 100 bps (1%) and fresh-price age 10_000s.
fn setup_price_env(env: &Env) -> PricePool<'_> {
    env.mock_all_auths();
    env.ledger().with_mut(|l| l.timestamp = 1_000);
    let (ac_client, client, token, _, _, _, operator, creator) = crate::test::setup(env);
    let admin = Address::generate(env);
    ac_client.grant_role(&admin, &ROLE_ADMIN);
    let oracle = Address::generate(env);
    client.add_oracle(&admin, &oracle);
    client.init_oracle(&admin, &Address::generate(env), &10_000u64, &100u32);
    PricePool {
        client,
        oracle,
        operator,
        creator,
        token,
    }
}

fn create_price_pool(env: &Env, ctx: &PricePool, feed: &Symbol, target: i128, op: u32) -> u64 {
    let pool_id = ctx.client.create_pool(
        &ctx.creator,
        &5_000u64,
        &ctx.token,
        &2u32,
        &symbol_short!("Finance"),
        &PoolConfig {
            start_time: 1_000,
            description: String::from_str(env, "Price pool"),
            metadata_url: String::from_str(env, "ipfs://price"),
            min_stake: 1,
            max_stake: 0,
            max_total_stake: 0,
            min_total_stake: 1,
            initial_liquidity: 0,
            required_resolutions: 1,
            private: false,
            whitelist_key: None,
            outcome_descriptions: soroban_sdk::vec![
                env,
                String::from_str(env, "No"),
                String::from_str(env, "Yes"),
            ],
        },
    );
    ctx.client
        .set_price_condition(&ctx.operator, &pool_id, feed, &target, &op, &0u32);
    pool_id
}

fn stored_feed(env: &Env, ctx: &PricePool, feed: &Symbol) -> (i128, i128, u64, u64) {
    env.as_contract(&ctx.client.address, || {
        env.storage()
            .persistent()
            .get(&DataKey::PriceFeed(feed.clone()))
            .unwrap()
    })
}

#[test]
fn test_feeds_with_different_exponents_are_stored_and_resolved_identically() {
    let env = Env::default();
    let ctx = setup_price_env(&env);
    let feed_a = Symbol::new(&env, "ETHUSD_A");
    let feed_b = Symbol::new(&env, "ETHUSD_B");
    // Target 3000.00 USD in canonical fixed point.
    let pool_a = create_price_pool(&env, &ctx, &feed_a, 300_000_000_000, GT);
    let pool_b = create_price_pool(&env, &ctx, &feed_b, 300_000_000_000, GT);

    // 3010.50 ± 1.25 USD published at expo -2 and at expo -12.
    ctx.client.update_price_feed_with_expo(
        &ctx.oracle,
        &feed_a,
        &301_050,
        &125,
        &-2,
        &999u64,
        &100_000u64,
    );
    ctx.client.update_price_feed_with_expo(
        &ctx.oracle,
        &feed_b,
        &3_010_500_000_000_000,
        &1_250_000_000_000,
        &-12,
        &999u64,
        &100_000u64,
    );
    assert_eq!(
        stored_feed(&env, &ctx, &feed_a),
        (301_050_000_000, 125_000_000, 999, 100_000)
    );
    assert_eq!(
        stored_feed(&env, &ctx, &feed_a),
        stored_feed(&env, &ctx, &feed_b)
    );

    env.ledger().with_mut(|l| l.timestamp = 8_600);
    ctx.client.resolve_pool_from_price(&pool_a);
    ctx.client.resolve_pool_from_price(&pool_b);
    assert_eq!(ctx.client.get_pool(&pool_a).outcome, 1);
    assert_eq!(ctx.client.get_pool(&pool_b).outcome, 1);
}

#[test]
fn test_update_price_feed_with_unsupported_exponent_is_rejected() {
    let env = Env::default();
    let ctx = setup_price_env(&env);
    let feed = Symbol::new(&env, "ETHUSD");
    assert_eq!(
        ctx.client.try_update_price_feed_with_expo(
            &ctx.oracle,
            &feed,
            &1,
            &0,
            &-19,
            &999u64,
            &100_000u64,
        ),
        Err(Ok(PredifiError::UnsupportedPriceExponent))
    );
}

#[test]
fn test_confidence_ratio_policy_boundary() {
    // price 100_000 with 1% policy: confidence 1_000 is the exact limit.
    let env = Env::default();
    let ctx = setup_price_env(&env);
    let at_limit = Symbol::new(&env, "AT_LIMIT");
    let over_limit = Symbol::new(&env, "OVER");
    let pool_ok = create_price_pool(&env, &ctx, &at_limit, 50_000, GT);
    let pool_wide = create_price_pool(&env, &ctx, &over_limit, 50_000, GT);
    ctx.client.update_price_feed(
        &ctx.oracle,
        &at_limit,
        &100_000,
        &1_000,
        &999u64,
        &100_000u64,
    );
    ctx.client.update_price_feed(
        &ctx.oracle,
        &over_limit,
        &100_000,
        &1_001,
        &999u64,
        &100_000u64,
    );

    env.ledger().with_mut(|l| l.timestamp = 8_600);
    ctx.client.resolve_pool_from_price(&pool_ok);
    assert_eq!(ctx.client.get_pool(&pool_ok).outcome, 1);
    assert_eq!(
        ctx.client.try_resolve_pool_from_price(&pool_wide),
        Err(Ok(PredifiError::PriceConfidenceTooWide))
    );
    assert_eq!(ctx.client.get_pool(&pool_wide).state, MarketState::Active);
}

#[test]
fn test_straddling_price_defers_resolution_until_tighter_price() {
    let env = Env::default();
    let ctx = setup_price_env(&env);
    let feed = Symbol::new(&env, "ETHUSD");
    let pool_id = create_price_pool(&env, &ctx, &feed, 100_000, GT);

    // 100_500 ± 600 spans [99_900, 101_100] across the 100_000 target
    // (within the 1% width policy).
    ctx.client
        .update_price_feed(&ctx.oracle, &feed, &100_500, &600, &999u64, &100_000u64);
    env.ledger().with_mut(|l| l.timestamp = 8_600);
    assert_eq!(
        ctx.client.try_resolve_pool_from_price(&pool_id),
        Err(Ok(PredifiError::PriceConfidenceTooWide))
    );
    assert_eq!(ctx.client.get_pool(&pool_id).state, MarketState::Active);

    // A tighter reading [100_400, 100_600] clears the boundary.
    ctx.client
        .update_price_feed(&ctx.oracle, &feed, &100_500, &100, &8_599u64, &100_000u64);
    ctx.client.resolve_pool_from_price(&pool_id);
    let pool = ctx.client.get_pool(&pool_id);
    assert_eq!(pool.state, MarketState::Resolved);
    assert_eq!(pool.outcome, 1);
}
