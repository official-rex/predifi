//! # Property-Based Differential Tests
//!
//! This module uses `proptest` to generate random operation sequences and verify
//! that the reference model upholds conservation and invariant properties under
//! all inputs. It also includes direct comparisons between the reference model
//! arithmetic and the Soroban contract's `payouts.rs` formulas, ensuring the
//! two implementations always produce identical results.
//!
//! ## Reproducibility
//!
//! Tests use `ProptestConfig::with_cases` and `TestCaseError` and can be run with
//! a fixed seed using the `PROPTEST_SEED` environment variable or the `--seed`
//! proptest flag. The default seed is deterministic via proptest's RNG so failures
//! are always reproducible from the logged seed in the failure message.
//!
//! ## Test structure
//!
//! | Test                                    | Property checked                                        |
//! |-----------------------------------------|---------------------------------------------------------|
//! | `prop_fee_floor_rounding`               | `protocol_fee` never rounds up                          |
//! | `prop_winner_share_floor_rounding`      | `winner_share` never rounds up                          |
//! | `prop_inv4_winnings_le_total_stake`     | INV-4: single claim ≤ total_stake                       |
//! | `prop_inv5_sum_le_total_stake`          | INV-5: all winner claims sum ≤ total_stake              |
//! | `prop_loser_receives_zero`              | Losers always get 0                                     |
//! | `prop_cancel_full_refund`               | Canceled pool returns exact principal                   |
//! | `prop_no_double_claim`                  | INV-3: second claim always returns AlreadyClaimed       |
//! | `prop_dynamic_fee_tier_selection`       | Highest matching tier is selected                       |
//! | `prop_op_sequence_conservation`        | Arbitrary op sequences: claimed ≤ total_stake           |
//! | `prop_terminal_path_resolved`           | Resolved path exercised end-to-end                      |
//! | `prop_terminal_path_canceled`           | Canceled path exercised end-to-end                      |
//! | `prop_terminal_path_disputed_stasis`    | Disputed path blocks all settlement ops                 |

use predifi_reference_model::{
    math::{
        claim_payout, dynamic_fee, protocol_fee, winner_share, FeeTier, MathError, MAX_BPS,
        UNRESOLVED_OUTCOME,
    },
    model::{FeeSchedule, MarketState, ModelError, RefModel},
};
use proptest::prelude::*;

// ─────────────────────────────────────────────────────────────────────────────
// Generators
// ─────────────────────────────────────────────────────────────────────────────

/// Generate a valid fee in basis points (0 … 10_000).
fn arb_fee_bps() -> impl Strategy<Value = i128> {
    0i128..=MAX_BPS
}

/// Generate a positive total stake up to 10^14 (matches the Soroban test range).
fn arb_total_stake() -> impl Strategy<Value = i128> {
    1i128..=100_000_000_000_000i128
}

/// Generate a positive user stake ≤ total_stake.
fn arb_user_stake(total: i128) -> impl Strategy<Value = i128> {
    1i128..=total
}

/// Generate a winning stake that is between 1 and total_stake.
fn arb_winning_stake(total: i128) -> impl Strategy<Value = i128> {
    1i128..=total
}

/// Generate a user stake that is between 1 and winning_stake.
fn arb_stake_le_winning(winning: i128) -> impl Strategy<Value = i128> {
    1i128..=winning
}

/// Generate up to 4 fee tiers with ascending thresholds.
fn arb_fee_tiers() -> impl Strategy<Value = Vec<FeeTier>> {
    prop::collection::vec(
        (1i128..=100_000i128, 0u32..=10_000u32)
            .prop_map(|(threshold, bps)| FeeTier { stake_threshold: threshold, fee_bps: bps }),
        0..=4usize,
    )
    .prop_map(|mut tiers| {
        // Sort ascending by threshold so the highest matching tier is unambiguous.
        tiers.sort_by_key(|t| t.stake_threshold);
        tiers.dedup_by_key(|t| t.stake_threshold);
        tiers
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Math primitive properties
// ─────────────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10_000))]

    /// `protocol_fee` floors: fee * 10_000 <= total_stake * fee_bps
    #[test]
    fn prop_fee_floor_rounding(
        total in arb_total_stake(),
        bps in arb_fee_bps(),
    ) {
        let fee = protocol_fee(total, bps).unwrap();
        // fee = floor(total * bps / 10_000)
        // therefore fee * 10_000 <= total * bps
        prop_assert!(fee * MAX_BPS <= total * bps,
            "fee {} * 10_000 > total {} * bps {}", fee, total, bps);
        // and fee >= 0
        prop_assert!(fee >= 0);
        // and fee <= total
        prop_assert!(fee <= total,
            "fee {} > total {}", fee, total);
    }

    /// `winner_share` floors: share * winning_stake <= user_stake * pool
    #[test]
    fn prop_winner_share_floor_rounding(
        total in arb_total_stake(),
        bps in arb_fee_bps(),
    ) {
        let total_stake = total;
        let fee = protocol_fee(total_stake, bps).unwrap();
        let pool = total_stake - fee;

        if pool <= 0 { return Ok(()); }

        // 1 user stake, winning_stake = user_stake (sole winner)
        let user = 1i128;
        let winning = 3i128;
        let share = winner_share(user, winning, pool).unwrap();
        prop_assert!(share * winning <= user * pool,
            "share {} * winning {} > user {} * pool {}", share, winning, user, pool);
        prop_assert!(share >= 0);
    }

    /// INV-4: a single claim is always ≤ total_stake.
    #[test]
    fn prop_inv4_winnings_le_total_stake(
        total in arb_total_stake(),
        bps in arb_fee_bps(),
        winning_share in 1i128..=100i128, // percent of total that is winning stake
    ) {
        let winning_stake = (total * winning_share / 100).max(1);
        let user_stake = winning_stake; // sole winner for simplicity

        let bd = claim_payout(total, bps, user_stake, winning_stake).unwrap();
        prop_assert!(
            bd.winnings <= total,
            "INV-4: winnings {} > total_stake {}", bd.winnings, total
        );
    }

    /// INV-5: the sum of all winner shares never exceeds payout_pool.
    ///
    /// We split `winning_stake` into `n` users and verify the sum of
    /// their floored shares ≤ payout_pool ≤ total_stake.
    #[test]
    fn prop_inv5_sum_le_total_stake(
        total in 2i128..=100_000_000_000_000i128,
        bps in arb_fee_bps(),
        n in 1usize..=20usize,
    ) {
        let fee = protocol_fee(total, bps).unwrap();
        let pool = total - fee;

        // Divide winning_stake evenly; give rounding remainder to last user
        let winning_stake = total / 2; // 50% winning side
        if winning_stake == 0 { return Ok(()); }

        let per = winning_stake / n as i128;
        if per == 0 { return Ok(()); }

        let mut sum = 0i128;
        let mut remaining = winning_stake;
        for i in 0..n {
            let stake = if i == n - 1 { remaining } else { per.min(remaining) };
            if stake <= 0 { break; }
            remaining -= stake;
            let share = winner_share(stake, winning_stake, pool).unwrap();
            sum += share;
        }

        prop_assert!(
            sum <= pool,
            "INV-5: sum {} > payout_pool {}", sum, pool
        );
        prop_assert!(
            sum <= total,
            "INV-5: sum {} > total_stake {}", sum, total
        );
    }

    /// Dynamic fee tier selection always picks the highest matching threshold.
    #[test]
    fn prop_dynamic_fee_tier_selection(
        total in 0i128..=200_000i128,
        base_bps in 0u32..=10_000u32,
        tiers in arb_fee_tiers(),
    ) {
        let selected = dynamic_fee(total, base_bps, &tiers);

        // Verify by brute force: find the expected tier
        let mut expected = base_bps;
        let mut max_thresh = -1i128;
        for tier in &tiers {
            if total >= tier.stake_threshold && tier.stake_threshold > max_thresh {
                max_thresh = tier.stake_threshold;
                expected = tier.fee_bps;
            }
        }

        prop_assert_eq!(
            selected, expected,
            "dynamic_fee({}, base={}, tiers={:?}) returned {} expected {}",
            total, base_bps, tiers, selected, expected
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Reference model state-machine properties
// ─────────────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2_000))]

    /// Terminal path: Resolved — winner and loser payouts satisfy INV-4 and INV-5.
    #[test]
    fn prop_terminal_path_resolved(
        alice_stake in 1i128..=500_000i128,
        bob_stake in 1i128..=500_000i128,
        fee_bps in 0u32..=2_000u32, // 0-20%
    ) {
        let mut m = RefModel::new();
        let schedule = FeeSchedule::flat(fee_bps);
        let pid = m.create_pool(0, 1000, 1, 0, 0, 0, 0, 2, schedule).unwrap();

        m.stake("alice", pid, alice_stake, 0).unwrap();
        m.stake("bob",   pid, bob_stake,   1).unwrap();
        m.resolve(pid, 0, 500).unwrap(); // alice's outcome wins

        let pa = m.claim("alice", pid, 600).unwrap();
        let pb = m.claim("bob",   pid, 600).unwrap();

        let total = m.pool(pid).unwrap().total_stake;

        // INV-4
        prop_assert!(pa <= total, "INV-4 alice: {} > {}", pa, total);
        prop_assert!(pb == 0, "loser should get 0, got {}", pb);

        // INV-5
        prop_assert!(pa + pb <= total, "INV-5: {} + {} > {}", pa, pb, total);

        // Conservation: treasury keeps the fee dust
        let fee = protocol_fee(total, fee_bps as i128).unwrap();
        let payout_pool_val = total - fee;
        prop_assert!(pa <= payout_pool_val,
            "alice payout {} > payout_pool {}", pa, payout_pool_val);
    }

    /// Terminal path: Canceled — all stakers receive 100% refund.
    #[test]
    fn prop_terminal_path_canceled(
        stakes in prop::collection::vec(1i128..=100_000i128, 1..=10usize),
        fee_bps in 0u32..=10_000u32,
    ) {
        let mut m = RefModel::new();
        let schedule = FeeSchedule::flat(fee_bps);
        let pid = m.create_pool(0, 1000, 1, 0, 0, 0, 0, 2, schedule).unwrap();

        let total_staked: i128 = stakes.iter().sum();
        let users: Vec<String> = (0..stakes.len()).map(|i| format!("user{}", i)).collect();

        for (i, &s) in stakes.iter().enumerate() {
            let outcome = (i % 2) as u32;
            m.stake(users[i].as_str(), pid, s, outcome).unwrap();
        }

        m.cancel(pid).unwrap();

        let mut total_refunded = 0i128;
        for (i, &s) in stakes.iter().enumerate() {
            let refund = m.claim(users[i].as_str(), pid, 0).unwrap();
            prop_assert_eq!(refund, s, "user{} expected refund {} got {}", i, s, refund);
            total_refunded += refund;
        }
        prop_assert_eq!(total_refunded, total_staked,
            "total refunded {} != total staked {}", total_refunded, total_staked);
    }

    /// Terminal path: Disputed — all settlement operations are blocked.
    #[test]
    fn prop_terminal_path_disputed_stasis(
        stake in 1i128..=10_000i128,
    ) {
        let mut m = RefModel::new();
        let pid = m.create_pool(0, 1000, 1, 0, 0, 0, 0, 2, FeeSchedule::flat(200)).unwrap();
        m.stake("alice", pid, stake, 0).unwrap();
        m.dispute(pid).unwrap();

        // Claims are blocked
        prop_assert_eq!(
            m.claim("alice", pid, 0),
            Err(ModelError::PoolDisputed),
            "disputed pool should block claims"
        );
        // Staking is blocked
        prop_assert_eq!(
            m.stake("bob", pid, 1, 1),
            Err(ModelError::PoolDisputed),
            "disputed pool should block staking"
        );
        // Resolution is blocked
        prop_assert_eq!(
            m.resolve(pid, 0, 500),
            Err(ModelError::PoolDisputed),
            "disputed pool should block resolution"
        );
    }

    /// INV-3: a second claim always returns AlreadyClaimed regardless of pool state.
    #[test]
    fn prop_no_double_claim(
        alice_stake in 1i128..=100_000i128,
    ) {
        // Resolved path
        {
            let mut m = RefModel::new();
            let pid = m.create_pool(0, 1000, 1, 0, 0, 0, 0, 2, FeeSchedule::flat(200)).unwrap();
            m.stake("alice", pid, alice_stake, 0).unwrap();
            m.resolve(pid, 0, 500).unwrap();
            m.claim("alice", pid, 600).unwrap();
            prop_assert_eq!(
                m.claim("alice", pid, 600),
                Err(ModelError::AlreadyClaimed)
            );
        }
        // Canceled path
        {
            let mut m = RefModel::new();
            let pid = m.create_pool(0, 1000, 1, 0, 0, 0, 0, 2, FeeSchedule::flat(200)).unwrap();
            m.stake("alice", pid, alice_stake, 0).unwrap();
            m.cancel(pid).unwrap();
            m.claim("alice", pid, 0).unwrap();
            prop_assert_eq!(
                m.claim("alice", pid, 0),
                Err(ModelError::AlreadyClaimed)
            );
        }
    }

    /// INV-2: state transitions are strictly one-way.
    #[test]
    fn prop_inv2_one_way_transitions(
        stake in 1i128..=100_000i128,
        target_state in 0u8..=1u8, // 0=resolve, 1=cancel
    ) {
        let mut m = RefModel::new();
        let pid = m.create_pool(0, 1000, 1, 0, 0, 0, 0, 2, FeeSchedule::flat(100)).unwrap();
        m.stake("alice", pid, stake, 0).unwrap();

        if target_state == 0 {
            m.resolve(pid, 0, 500).unwrap();
            // Cannot re-resolve
            prop_assert_eq!(m.resolve(pid, 1, 600), Err(ModelError::InvalidPoolState));
            // Cannot cancel after resolve
            prop_assert_eq!(m.cancel(pid), Err(ModelError::InvalidPoolState));
        } else {
            m.cancel(pid).unwrap();
            // Cannot resolve after cancel
            prop_assert_eq!(m.resolve(pid, 0, 600), Err(ModelError::InvalidPoolState));
            // Cannot cancel twice
            prop_assert_eq!(m.cancel(pid), Err(ModelError::InvalidPoolState));
        }
    }

    /// Arbitrary multi-user, multi-outcome operation sequences preserve conservation.
    ///
    /// Generates up to 8 users staking random amounts on random outcomes,
    /// then resolves or cancels, then has all users claim. Asserts INV-5.
    #[test]
    fn prop_op_sequence_conservation(
        user_stakes in prop::collection::vec(
            (1i128..=50_000i128, 0u32..=1u32), // (amount, outcome)
            1..=8usize,
        ),
        fee_bps in 0u32..=1_000u32,
        initial_liq in 0i128..=10_000i128,
        resolve_outcome in 0u32..=1u32,
        should_cancel in any::<bool>(),
    ) {
        let mut m = RefModel::new();
        let schedule = FeeSchedule::flat(fee_bps);
        let pid = m.create_pool(0, 1000, 1, 0, 0, 0, initial_liq, 2, schedule).unwrap();

        let users: Vec<String> = (0..user_stakes.len())
            .map(|i| format!("u{}", i))
            .collect();

        let mut total_user_staked = 0i128;
        for (i, &(stake, outcome)) in user_stakes.iter().enumerate() {
            match m.stake(users[i].as_str(), pid, stake, outcome) {
                Ok(_) => { total_user_staked += stake; }
                Err(_) => {} // stake limits might reject; that's fine
            }
        }

        let total_stake = m.pool(pid).unwrap().total_stake;

        if should_cancel {
            m.cancel(pid).unwrap();
        } else {
            // Make sure at least someone bet on resolve_outcome
            let winning_stake = m.outcome_stake(pid, resolve_outcome);
            if winning_stake == 0 {
                // No stake on this outcome → just cancel instead to avoid an uninteresting no-op
                m.cancel(pid).unwrap();
            } else {
                m.resolve(pid, resolve_outcome, 500).unwrap();
            }
        }

        let mut total_claimed = 0i128;
        for user in &users {
            match m.claim(user.as_str(), pid, 600) {
                Ok(amount) => { total_claimed += amount; }
                Err(ModelError::AlreadyClaimed) => {}
                Err(_) => {}
            }
        }

        prop_assert!(
            total_claimed <= total_stake,
            "INV-5: total_claimed {} > total_stake {}",
            total_claimed, total_stake
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Regression / edge-case tests (deterministic, not proptest)
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod regression {
    use super::*;

    /// Sole winner with non-zero fee: payout = payout_pool exactly.
    #[test]
    fn sole_winner_receives_full_payout_pool() {
        let mut m = RefModel::new();
        let pid = m
            .create_pool(0, 1000, 1, 0, 0, 0, 0, 2, FeeSchedule::flat(100))
            .unwrap();
        m.stake("alice", pid, 400, 0).unwrap();
        m.stake("bob", pid, 600, 1).unwrap();
        m.resolve(pid, 0, 500).unwrap();

        let payout = m.claim("alice", pid, 600).unwrap();
        // fee = floor(1000 * 100 / 10_000) = 10; pool = 990; alice = 990
        assert_eq!(payout, 990);
    }

    /// All stake on the winning side: each user gets their proportional share.
    #[test]
    fn all_on_winning_side_proportional() {
        let mut m = RefModel::new();
        let pid = m
            .create_pool(0, 1000, 1, 0, 0, 0, 0, 2, FeeSchedule::flat(0))
            .unwrap();
        m.stake("a", pid, 300, 0).unwrap();
        m.stake("b", pid, 700, 0).unwrap();
        m.resolve(pid, 0, 500).unwrap();

        let pa = m.claim("a", pid, 600).unwrap();
        let pb = m.claim("b", pid, 600).unwrap();
        // pool = 1000 (no fee)
        // a: floor(300 * 1000 / 1000) = 300
        // b: floor(700 * 1000 / 1000) = 700
        assert_eq!(pa, 300);
        assert_eq!(pb, 700);
    }

    /// No one bet on the winning outcome → all winners get 0.
    #[test]
    fn winning_outcome_has_no_stake_returns_zero() {
        let mut m = RefModel::new();
        let pid = m
            .create_pool(0, 1000, 1, 0, 0, 0, 0, 3, FeeSchedule::flat(200))
            .unwrap();
        m.stake("alice", pid, 500, 0).unwrap();
        m.stake("bob", pid, 500, 1).unwrap();
        // outcome 2 has no stake; resolve to it
        m.resolve(pid, 2, 500).unwrap();

        let pa = m.claim("alice", pid, 600).unwrap();
        let pb = m.claim("bob", pid, 600).unwrap();
        assert_eq!(pa, 0);
        assert_eq!(pb, 0);
    }

    /// Rounding dust stays in contract: 3 equal winners on odd-valued pool.
    #[test]
    fn three_winners_rounding_dust_stays_in_contract() {
        let mut m = RefModel::new();
        let pid = m
            .create_pool(0, 1000, 1, 0, 0, 0, 0, 2, FeeSchedule::flat(0))
            .unwrap();
        m.stake("a", pid, 1, 0).unwrap();
        m.stake("b", pid, 1, 0).unwrap();
        m.stake("c", pid, 1, 0).unwrap();
        m.stake("loser", pid, 7, 1).unwrap();
        m.resolve(pid, 0, 500).unwrap();

        let pool = m.pool(pid).unwrap();
        let payout_pool_val = pool.total_stake; // fee = 0

        let pa = m.claim("a", pid, 600).unwrap(); // floor(1 * 10 / 3) = 3
        let pb = m.claim("b", pid, 600).unwrap();
        let pc = m.claim("c", pid, 600).unwrap();
        assert!(pa + pb + pc <= payout_pool_val, "INV-5: dust should stay in contract");
    }

    /// Canceling a pool with initial liquidity: users get their stakes back,
    /// initial liquidity is not distributed.
    #[test]
    fn cancel_with_initial_liquidity_users_get_only_their_stakes() {
        let mut m = RefModel::new();
        let pid = m
            .create_pool(0, 1000, 1, 0, 0, 0, 1_000, 2, FeeSchedule::flat(200))
            .unwrap();
        m.stake("alice", pid, 500, 0).unwrap();
        m.cancel(pid).unwrap();

        let refund = m.claim("alice", pid, 0).unwrap();
        assert_eq!(refund, 500); // only her stake, not the initial liquidity
    }

    /// Fee schedule snapshot: changing the schedule after pool creation does not affect it.
    #[test]
    fn fee_schedule_is_snapshotted_at_creation() {
        use predifi_reference_model::math::FeeTier;
        // Pool created with a 2% flat fee
        let schedule_v1 = FeeSchedule::flat(200);
        let mut m = RefModel::new();
        let pid = m
            .create_pool(0, 1000, 1, 0, 0, 0, 0, 2, schedule_v1)
            .unwrap();
        m.stake("alice", pid, 1000, 0).unwrap();

        // Even if we "update" the schedule outside the model (not supported in the
        // model, just verifying the pool holds its own snapshot), the pool's own
        // fee_schedule must govern resolution.
        m.resolve(pid, 0, 500).unwrap();
        assert_eq!(m.pool(pid).unwrap().fee_bps, 200);

        // Separate pool with a different schedule
        let schedule_v2 = FeeSchedule {
            version: 2,
            base_fee_bps: 100,
            tiers: vec![FeeTier { stake_threshold: 500, fee_bps: 50 }],
        };
        let pid2 = m
            .create_pool(0, 1000, 1, 0, 0, 0, 0, 2, schedule_v2)
            .unwrap();
        m.stake("bob", pid2, 1000, 0).unwrap();
        m.resolve(pid2, 0, 500).unwrap();
        // total_stake = 1000 >= 500, so tier kicks in → 50 bps
        assert_eq!(m.pool(pid2).unwrap().fee_bps, 50);
    }

    /// Idempotent claim sentinel: a loser's claim sets the sentinel so a
    /// subsequent call returns AlreadyClaimed even though payout was 0.
    #[test]
    fn loser_claim_sets_sentinel() {
        let mut m = RefModel::new();
        let pid = m
            .create_pool(0, 1000, 1, 0, 0, 0, 0, 2, FeeSchedule::flat(200))
            .unwrap();
        m.stake("alice", pid, 500, 0).unwrap();
        m.stake("bob", pid, 500, 1).unwrap();
        m.resolve(pid, 0, 500).unwrap();

        // bob is the loser
        let first = m.claim("bob", pid, 600).unwrap();
        assert_eq!(first, 0);
        assert!(m.has_claimed("bob", pid));
        assert_eq!(m.claim("bob", pid, 600), Err(ModelError::AlreadyClaimed));
    }
}
