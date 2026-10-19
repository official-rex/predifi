//! # Payout Arithmetic
//!
//! Pure integer arithmetic that mirrors `safe_math.rs` and `payouts.rs` in the
//! Soroban contract. Every function documents its rounding direction so that
//! differential tests can validate it exhaustively.
//!
//! The key invariant preserved by these routines is that no combination of
//! claimants can receive more tokens in total than `pool.total_stake`.
//! This holds because:
//!   - `protocol_fee` is floored (≤ exact value), so `payout_pool` is slightly
//!     generous, but every `winnings` calculation floors again.
//!   - For any partition of `winning_stake` into user stakes
//!     `u₁ + u₂ + … + uₙ = winning_stake`, the sum of floored shares satisfies
//!     `Σ floor(uᵢ * payout_pool / winning_stake) ≤ payout_pool ≤ total_stake`.

/// Maximum basis points (100%).
pub const MAX_BPS: i128 = 10_000;

/// Sentinel value for an unresolved pool outcome (mirrors `UNRESOLVED_OUTCOME`).
pub const UNRESOLVED_OUTCOME: u32 = u32::MAX;

/// Calculate protocol fee from total stake and fee rate.
///
/// # Rounding: floor (matches `RoundingMode::ProtocolFavor`)
/// `result = total_stake * fee_bps / 10_000`
///
/// # Errors
/// Returns `Err(MathError::InvalidFeeBps)` when `fee_bps > 10_000`.
/// Returns `Err(MathError::NegativeInput)` when `total_stake < 0`.
/// Returns `Err(MathError::Overflow)` on i128 overflow (extremely large inputs).
pub fn protocol_fee(total_stake: i128, fee_bps: i128) -> Result<i128, MathError> {
    if total_stake < 0 {
        return Err(MathError::NegativeInput);
    }
    if fee_bps < 0 || fee_bps > MAX_BPS {
        return Err(MathError::InvalidFeeBps);
    }
    if total_stake == 0 || fee_bps == 0 {
        return Ok(0);
    }
    let numerator = total_stake
        .checked_mul(fee_bps)
        .ok_or(MathError::Overflow)?;
    Ok(numerator / MAX_BPS) // floor division
}

/// Amount remaining for winners after protocol fee is deducted.
///
/// # Rounding: exact subtraction (no new rounding)
pub fn payout_pool(total_stake: i128, fee_bps: i128) -> Result<i128, MathError> {
    let fee = protocol_fee(total_stake, fee_bps)?;
    total_stake
        .checked_sub(fee)
        .ok_or(MathError::Overflow)
}

/// Proportional winner payout.
///
/// Returns `0` when `winning_stake == 0` or `user_stake == 0`.
///
/// # Rounding: floor (matches `RoundingMode::ProtocolFavor`)
/// `result = user_stake * pool / winning_stake`
///
/// # Errors
/// Returns `Err(MathError::NegativeInput)` on negative inputs.
/// Returns `Err(MathError::Overflow)` on i128 overflow.
pub fn winner_share(user_stake: i128, winning_stake: i128, pool: i128) -> Result<i128, MathError> {
    if user_stake < 0 || winning_stake < 0 || pool < 0 {
        return Err(MathError::NegativeInput);
    }
    if winning_stake == 0 || user_stake == 0 {
        return Ok(0);
    }
    let numerator = user_stake
        .checked_mul(pool)
        .ok_or(MathError::Overflow)?;
    Ok(numerator / winning_stake) // floor division
}

/// Full payout breakdown for one claim: fee → payout pool → user share.
///
/// This is the reference implementation of `calculate_claim_payout` in the contract.
pub fn claim_payout(
    total_stake: i128,
    fee_bps: i128,
    user_stake: i128,
    winning_stake: i128,
) -> Result<PayoutBreakdown, MathError> {
    if total_stake < 0 || user_stake < 0 || winning_stake < 0 {
        return Err(MathError::NegativeInput);
    }
    if fee_bps < 0 || fee_bps > MAX_BPS {
        return Err(MathError::InvalidFeeBps);
    }

    let fee = protocol_fee(total_stake, fee_bps)?;
    let pool = total_stake
        .checked_sub(fee)
        .ok_or(MathError::Overflow)?;
    let winnings = winner_share(user_stake, winning_stake, pool)?;

    // INV-4: defense in depth
    if winnings > total_stake {
        return Err(MathError::InvariantViolated("INV-4: winnings > total_stake"));
    }

    Ok(PayoutBreakdown {
        protocol_fee: fee,
        payout_pool: pool,
        winnings,
    })
}

/// Referral cut from the protocol fee attributable to a specific claimer.
///
/// # Rounding: nearest, ties round up (matches `RoundingMode::Neutral`)
/// Intentionally more generous than floor because it comes from the treasury
/// share, not from user payouts.
///
/// `result = round_half_up(user_stake / total_stake * protocol_fee * referral_cut_bps / 10_000)`
pub fn referral_amount(
    user_stake: i128,
    total_stake: i128,
    protocol_fee_total: i128,
    referral_cut_bps: i128,
) -> Result<i128, MathError> {
    if protocol_fee_total <= 0 || total_stake <= 0 || user_stake <= 0 {
        return Ok(0);
    }
    // user's proportional share of the protocol fee — round nearest
    let user_fee_num = user_stake
        .checked_mul(protocol_fee_total)
        .ok_or(MathError::Overflow)?;
    let user_fee_share = round_nearest(user_fee_num, total_stake)?;

    // referral cut of that share — round nearest
    let referral_num = user_fee_share
        .checked_mul(referral_cut_bps)
        .ok_or(MathError::Overflow)?;
    round_nearest(referral_num, MAX_BPS)
}

/// Round `numerator / denominator` to nearest integer, ties round up.
fn round_nearest(numerator: i128, denominator: i128) -> Result<i128, MathError> {
    if denominator == 0 {
        return Err(MathError::DivisionByZero);
    }
    // (num + denom/2) / denom  — but we need to avoid overflow from the addition.
    // Use: num/denom + (num%denom*2 >= denom ? 1 : 0)
    let q = numerator / denominator;
    let r = numerator % denominator;
    if r * 2 >= denominator {
        Ok(q + 1)
    } else {
        Ok(q)
    }
}

/// Select the dynamic fee rate for a pool at resolution time.
///
/// Walks `tiers` and picks the highest `stake_threshold ≤ total_stake`.
/// Falls back to `base_fee_bps` if no tier matches.
///
/// This exactly mirrors `calculate_dynamic_fee` in the contract.
pub fn dynamic_fee(total_stake: i128, base_fee_bps: u32, tiers: &[FeeTier]) -> u32 {
    let mut applied = base_fee_bps;
    let mut max_threshold: i128 = -1;
    for tier in tiers {
        if total_stake >= tier.stake_threshold && tier.stake_threshold > max_threshold {
            max_threshold = tier.stake_threshold;
            applied = tier.fee_bps;
        }
    }
    applied
}

// ─────────────────────────────────────────────────────────────────────────────
// Supporting types
// ─────────────────────────────────────────────────────────────────────────────

/// Full payout breakdown returned by [`claim_payout`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PayoutBreakdown {
    pub protocol_fee: i128,
    pub payout_pool: i128,
    pub winnings: i128,
}

/// Arithmetic error variants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MathError {
    Overflow,
    NegativeInput,
    InvalidFeeBps,
    DivisionByZero,
    InvariantViolated(&'static str),
}

/// A single dynamic fee tier (mirrors `FeeTier` in the contract).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FeeTier {
    /// Minimum `total_stake` for this tier to activate.
    pub stake_threshold: i128,
    /// Protocol fee in basis points when this tier is active.
    pub fee_bps: u32,
}

// ─────────────────────────────────────────────────────────────────────────────
// Unit tests for the math primitives
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_fee_floor() {
        // 1% of 999 = 9.99 → floor → 9
        assert_eq!(protocol_fee(999, 100).unwrap(), 9);
    }

    #[test]
    fn protocol_fee_zero_stake() {
        assert_eq!(protocol_fee(0, 500).unwrap(), 0);
    }

    #[test]
    fn protocol_fee_zero_bps() {
        assert_eq!(protocol_fee(1_000_000, 0).unwrap(), 0);
    }

    #[test]
    fn protocol_fee_full_bps() {
        assert_eq!(protocol_fee(1_000, 10_000).unwrap(), 1_000);
    }

    #[test]
    fn protocol_fee_invalid_bps() {
        assert_eq!(protocol_fee(1_000, 10_001), Err(MathError::InvalidFeeBps));
    }

    #[test]
    fn payout_pool_equals_stake_minus_fee() {
        let total = 10_000i128;
        let bps = 250i128;
        let fee = protocol_fee(total, bps).unwrap();
        assert_eq!(payout_pool(total, bps).unwrap(), total - fee);
    }

    #[test]
    fn winner_share_sole_winner() {
        // Sole winner gets the entire payout pool
        assert_eq!(winner_share(500, 500, 990).unwrap(), 990);
    }

    #[test]
    fn winner_share_proportional() {
        // 250 / 500 * 1000 = 500
        assert_eq!(winner_share(250, 500, 1_000).unwrap(), 500);
    }

    #[test]
    fn winner_share_floor() {
        // 1 / 3 * 10 = 3.333… → floor → 3
        assert_eq!(winner_share(1, 3, 10).unwrap(), 3);
    }

    #[test]
    fn winner_share_zero_winning_stake() {
        assert_eq!(winner_share(100, 0, 1_000).unwrap(), 0);
    }

    #[test]
    fn claim_payout_invariant_iv() {
        let out = claim_payout(1_000, 100, 400, 400).unwrap();
        assert_eq!(out.protocol_fee, 10);
        assert_eq!(out.payout_pool, 990);
        assert_eq!(out.winnings, 990); // sole winner
        assert!(out.winnings <= 1_000);
    }

    #[test]
    fn claim_payout_loser_gets_zero() {
        // User is on the losing side — handled by the model, not this function,
        // but we verify a zero winning_stake path returns 0
        let out = claim_payout(1_000, 100, 100, 0).unwrap();
        assert_eq!(out.winnings, 0);
    }

    #[test]
    fn dynamic_fee_no_tiers() {
        assert_eq!(dynamic_fee(999_999, 300, &[]), 300);
    }

    #[test]
    fn dynamic_fee_picks_highest_met_threshold() {
        let tiers = vec![
            FeeTier { stake_threshold: 1_000, fee_bps: 250 },
            FeeTier { stake_threshold: 5_000, fee_bps: 200 },
            FeeTier { stake_threshold: 10_000, fee_bps: 150 },
        ];
        // total_stake = 7_500 → tier at 5_000 matches, tier at 10_000 doesn't
        assert_eq!(dynamic_fee(7_500, 300, &tiers), 200);
    }

    #[test]
    fn dynamic_fee_below_first_threshold() {
        let tiers = vec![FeeTier { stake_threshold: 5_000, fee_bps: 150 }];
        assert_eq!(dynamic_fee(4_999, 300, &tiers), 300);
    }

    #[test]
    fn dynamic_fee_at_exact_threshold() {
        let tiers = vec![FeeTier { stake_threshold: 5_000, fee_bps: 150 }];
        assert_eq!(dynamic_fee(5_000, 300, &tiers), 150);
    }

    #[test]
    fn referral_amount_proportional() {
        // User has 50% of pool; fee total = 100; referral cut = 10% → 5
        let amount = referral_amount(500, 1_000, 100, 1_000).unwrap();
        assert_eq!(amount, 5);
    }

    #[test]
    fn referral_amount_zero_when_no_fee() {
        assert_eq!(referral_amount(500, 1_000, 0, 1_000).unwrap(), 0);
    }

    #[test]
    fn sum_of_floored_winner_shares_never_exceeds_pool() {
        // 3 equal winners sharing a payout_pool that doesn't divide evenly
        let pool = 10i128;
        let winning_stake = 3i128;
        let w1 = winner_share(1, winning_stake, pool).unwrap(); // floor(10/3) = 3
        let w2 = winner_share(1, winning_stake, pool).unwrap();
        let w3 = winner_share(1, winning_stake, pool).unwrap();
        assert!(w1 + w2 + w3 <= pool, "sum of shares must not exceed pool");
    }
}
