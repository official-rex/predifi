//! # PrediFi Off-Chain Reference Model
//!
//! This crate provides a pure-Rust, no-dependency reference model for the PrediFi
//! Soroban smart contract. It mirrors the contract's pool state machine, stake
//! accounting, payout arithmetic, and invariants exactly, without any on-chain
//! primitives, so that:
//!
//! 1. The semantics of each operation are specified independently of the
//!    contract implementation (acceptance criteria A).
//! 2. Property-based differential tests can run the same operation sequences
//!    against both this model and the Soroban test harness and assert identical
//!    outcomes (acceptance criteria B + C).
//!
//! ## Rounding contract (critical — do not change without updating the contract)
//!
//! | Calculation                 | Direction | Formula                                      |
//! |-----------------------------|-----------|----------------------------------------------|
//! | `protocol_fee`              | Floor ↓   | `total_stake * fee_bps / 10_000`             |
//! | `payout_pool`               | Exact     | `total_stake - protocol_fee`                 |
//! | `winnings`                  | Floor ↓   | `user_stake * payout_pool / winning_stake`   |
//! | `referral_amount`           | Nearest ½↑| `user_share_of_fee * referral_cut_bps / 10_000` |
//!
//! All floor divisions use Rust's built-in integer truncation (which floors for
//! non-negative operands). No floating-point arithmetic is used anywhere.
//!
//! ## Invariants enforced after every mutating operation
//!
//! | ID    | Statement                                                          |
//! |-------|--------------------------------------------------------------------|
//! | INV-1 | `pool.total_stake == sum(outcome_stakes)`                         |
//! | INV-2 | State transitions are one-way: Active → {Resolved, Canceled, Disputed} |
//! | INV-3 | `claimed[user, pool]` is write-once (no double-claim)            |
//! | INV-4 | `winnings <= pool.total_stake`                                    |
//! | INV-5 | `sum(all claimed_winnings for pool) <= pool.total_stake`          |
//! | INV-6 | `fee_bps <= 10_000`                                               |
//! | INV-7 | `prediction.amount > 0`                                           |
//! | INV-8 | `pool.end_time > pool.creation_time`                              |
//!
//! ## Terminal paths
//!
//! ```text
//! Active ─┬─→ Resolved : winners claim proportional share minus protocol fee;
//!         │              losers receive 0.
//!         ├─→ Canceled : all stakers receive 100% principal refund (no fee).
//!         └─→ Disputed : stasis — no claims, no refunds until adjudicated.
//! ```

pub mod model;
pub mod math;

pub use model::*;
pub use math::*;
