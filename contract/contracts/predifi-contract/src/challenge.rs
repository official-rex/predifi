//! Resolution challenge window: a bounded period after resolution during which
//! pool participants can dispute the result before payouts become final.
//!
//! See `docs/resolution-challenge-window.md` for the state diagram.
//!
//! - **Policy** — `set_challenge_config` (Admin) sets `window_seconds`
//!   (`0..=MAX_CHALLENGE_WINDOW`, 0 disables challenges) and `bond_amount`.
//! - **Opening** — every resolution path (`resolve_pool`, `oracle_resolve`,
//!   `resolve_pool_from_price`) calls `open_challenge_window`, which snapshots
//!   the policy into a per-pool [`ResolutionChallenge`] with
//!   `deadline = resolved_at + window_seconds`.
//! - **Challenging** — any address holding a prediction in the pool may call
//!   `challenge_resolution` until `deadline` (inclusive), posting the bond in
//!   the pool token and a 32-byte evidence commitment plus an evidence URI.
//!   Only one challenge per pool.
//! - **Adjudication** — a Moderator (role 2) calls `adjudicate_challenge`.
//!   Upheld: the outcome is replaced and the bond is returned to the
//!   challenger. Rejected: the outcome stands and the bond goes to the treasury.
//! - **Finalization** — adjudication finalizes immediately; an unchallenged
//!   resolution is final once `deadline` has passed (`finalize_resolution`
//!   records it and is idempotent). Claims are blocked until final, and the
//!   claim window starts when the resolution becomes final.

use soroban_sdk::{contractimpl, token, Address, BytesN, Env, String};

use crate::{
    ChallengeAdjudicatedEvent, ChallengeConfig, ChallengeConfigUpdateEvent, ChallengeStatus,
    ChallengeWindowOpenedEvent, DataKey, MarketState, Pool, PredifiContract, PredifiContractArgs,
    PredifiContractClient, PredifiError, ResolutionChallenge, ResolutionChallengedEvent,
    ResolutionFinalizedEvent, MAX_CHALLENGE_WINDOW, MAX_EVIDENCE_URI_LEN,
};

#[contractimpl]
impl PredifiContract {
    /// Set the protocol challenge policy. Caller must have Admin role (0).
    ///
    /// Applies to pools resolved after the call; open windows keep the policy
    /// snapshotted at their resolution.
    ///
    /// # Errors
    /// - `InvalidTimestamp` — `window_seconds > MAX_CHALLENGE_WINDOW`.
    /// - `InvalidAmount` — `bond_amount < 0`, or `bond_amount == 0` while the
    ///   window is enabled (a free challenge could freeze payouts at no cost).
    pub fn set_challenge_config(
        env: Env,
        admin: Address,
        window_seconds: u64,
        bond_amount: i128,
    ) -> Result<(), PredifiError> {
        Self::require_not_paused(&env)?;
        admin.require_auth();
        Self::require_admin_role(&env, &admin, "set_challenge_config")?;

        if window_seconds > MAX_CHALLENGE_WINDOW {
            return Err(PredifiError::InvalidTimestamp);
        }
        if bond_amount < 0 || (window_seconds > 0 && bond_amount == 0) {
            return Err(PredifiError::InvalidAmount);
        }

        env.storage().instance().set(
            &DataKey::ChallengeConfig,
            &ChallengeConfig {
                window_seconds,
                bond_amount,
            },
        );
        Self::extend_instance(&env);

        ChallengeConfigUpdateEvent {
            admin,
            window_seconds,
            bond_amount,
        }
        .publish(&env);
        Ok(())
    }

    /// Current challenge policy. Defaults to a disabled window (0 seconds).
    pub fn get_challenge_config(env: Env) -> ChallengeConfig {
        env.storage()
            .instance()
            .get(&DataKey::ChallengeConfig)
            .unwrap_or(ChallengeConfig {
                window_seconds: 0,
                bond_amount: 0,
            })
    }

    /// Challenge record for a resolved pool, or `None` when the pool resolved
    /// with the window disabled (its resolution was final immediately).
    pub fn get_resolution_challenge(env: Env, pool_id: u64) -> Option<ResolutionChallenge> {
        env.storage()
            .persistent()
            .get(&DataKey::ResolutionChallenge(pool_id))
    }

    /// Challenge a pool's resolution. `challenger` must hold a prediction in the
    /// pool and posts `bond_amount` of the pool token.
    ///
    /// Evidence: `evidence_hash` is a non-zero 32-byte commitment (e.g. SHA-256)
    /// to the evidence bundle stored at `evidence_uri` (1..=512 bytes), such as
    /// a signed provider price update for the resolution timestamp.
    ///
    /// # Errors
    /// - `PoolNotFound` / `InvalidPoolState` — pool missing or not `Resolved`.
    /// - `TimeConstraintError` — no window was opened, it is finalized, or
    ///   `now > deadline`.
    /// - `PoolDisputed` — a challenge is already open.
    /// - `Unauthorized` — `challenger` has no prediction in the pool.
    /// - `InvalidData` — zero evidence hash or invalid evidence URI length.
    pub fn challenge_resolution(
        env: Env,
        challenger: Address,
        pool_id: u64,
        evidence_hash: BytesN<32>,
        evidence_uri: String,
    ) -> Result<(), PredifiError> {
        Self::require_not_paused(&env)?;
        challenger.require_auth();

        let pool = Self::load_resolved_pool(&env, pool_id)?;
        let key = DataKey::ResolutionChallenge(pool_id);
        let mut record: ResolutionChallenge = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(PredifiError::TimeConstraintError)?;

        match record.status {
            ChallengeStatus::Challenged => return Err(PredifiError::PoolDisputed),
            ChallengeStatus::Finalized => return Err(PredifiError::TimeConstraintError),
            ChallengeStatus::Open => {}
        }
        if env.ledger().timestamp() > record.deadline {
            return Err(PredifiError::TimeConstraintError);
        }
        if !env
            .storage()
            .persistent()
            .has(&DataKey::Pred(challenger.clone(), pool_id))
        {
            return Err(PredifiError::Unauthorized);
        }
        if evidence_hash.to_array() == [0u8; 32]
            || evidence_uri.is_empty()
            || evidence_uri.len() > MAX_EVIDENCE_URI_LEN
        {
            return Err(PredifiError::InvalidData);
        }

        if record.bond_amount > 0 {
            token::Client::new(&env, &pool.token).transfer(
                &challenger,
                env.current_contract_address(),
                &record.bond_amount,
            );
        }

        record.status = ChallengeStatus::Challenged;
        record.challenger = Some(challenger.clone());
        record.evidence_hash = Some(evidence_hash.clone());
        record.evidence_uri = Some(evidence_uri.clone());
        env.storage().persistent().set(&key, &record);
        Self::extend_persistent(&env, &key);

        ResolutionChallengedEvent {
            pool_id,
            challenger,
            bond_amount: record.bond_amount,
            evidence_hash,
            evidence_uri,
        }
        .publish(&env);
        Ok(())
    }

    /// Settle an open challenge. Caller must have Moderator role (2).
    ///
    /// - `upheld == true`: the pool outcome becomes `corrected_outcome` (must be
    ///   a valid outcome different from the original) and the bond is returned
    ///   to the challenger.
    /// - `upheld == false`: the original outcome stands, `corrected_outcome`
    ///   is ignored and the bond is transferred to the treasury.
    ///
    /// Either way the resolution becomes final and payouts are enabled.
    ///
    /// # Errors
    /// - `Unauthorized` — caller is not a Moderator.
    /// - `InvalidPoolState` — pool not `Resolved` or has no open challenge.
    /// - `InvalidOutcome` — upheld with an invalid or unchanged outcome.
    pub fn adjudicate_challenge(
        env: Env,
        moderator: Address,
        pool_id: u64,
        upheld: bool,
        corrected_outcome: u32,
    ) -> Result<(), PredifiError> {
        Self::require_not_paused(&env)?;
        moderator.require_auth();
        Self::require_role(&env, &moderator, 2)?;

        let mut pool = Self::load_resolved_pool(&env, pool_id)?;
        let key = DataKey::ResolutionChallenge(pool_id);
        let mut record: ResolutionChallenge = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(PredifiError::InvalidPoolState)?;
        if record.status != ChallengeStatus::Challenged {
            return Err(PredifiError::InvalidPoolState);
        }
        let challenger = record
            .challenger
            .clone()
            .ok_or(PredifiError::InvalidPoolState)?;

        let bond_recipient = if upheld {
            if corrected_outcome >= pool.options_count || corrected_outcome == pool.outcome {
                return Err(PredifiError::InvalidOutcome);
            }
            pool.outcome = corrected_outcome;
            let pool_key = DataKey::Pool(pool_id);
            env.storage().persistent().set(&pool_key, &pool);
            Self::extend_persistent(&env, &pool_key);
            challenger
        } else {
            Self::get_config(&env).treasury
        };

        let now = env.ledger().timestamp();
        record.status = ChallengeStatus::Finalized;
        record.finalized_at = Some(now);
        env.storage().persistent().set(&key, &record);
        Self::extend_persistent(&env, &key);

        if record.bond_amount > 0 {
            token::Client::new(&env, &pool.token).transfer(
                &env.current_contract_address(),
                &bond_recipient,
                &record.bond_amount,
            );
        }

        ChallengeAdjudicatedEvent {
            pool_id,
            moderator,
            upheld,
            original_outcome: record.original_outcome,
            final_outcome: pool.outcome,
            bond_recipient,
        }
        .publish(&env);
        ResolutionFinalizedEvent {
            pool_id,
            outcome: pool.outcome,
            timestamp: now,
        }
        .publish(&env);
        Ok(())
    }

    /// Record that an unchallenged resolution is final. Permissionless and
    /// idempotent: returns `Ok(())` without re-emitting when already final
    /// (including pools resolved with the window disabled).
    ///
    /// Calling it is optional — claims are enabled as soon as `deadline`
    /// passes without a challenge.
    ///
    /// # Errors
    /// - `PoolNotFound` / `InvalidPoolState` — pool missing or not `Resolved`.
    /// - `TimeConstraintError` — still inside the challenge window.
    /// - `PoolDisputed` — a challenge awaits adjudication.
    pub fn finalize_resolution(env: Env, pool_id: u64) -> Result<(), PredifiError> {
        Self::require_not_paused(&env)?;
        let pool = Self::load_resolved_pool(&env, pool_id)?;
        let key = DataKey::ResolutionChallenge(pool_id);
        let mut record: ResolutionChallenge = match env.storage().persistent().get(&key) {
            Some(record) => record,
            None => return Ok(()),
        };
        match record.status {
            ChallengeStatus::Finalized => return Ok(()),
            ChallengeStatus::Challenged => return Err(PredifiError::PoolDisputed),
            ChallengeStatus::Open => {}
        }
        let now = env.ledger().timestamp();
        if now <= record.deadline {
            return Err(PredifiError::TimeConstraintError);
        }

        record.status = ChallengeStatus::Finalized;
        record.finalized_at = Some(now);
        env.storage().persistent().set(&key, &record);
        Self::extend_persistent(&env, &key);

        ResolutionFinalizedEvent {
            pool_id,
            outcome: pool.outcome,
            timestamp: now,
        }
        .publish(&env);
        Ok(())
    }

    // ── Internal helpers ─────────────────────────────────────────────────────

    /// Open the challenge window for a just-resolved pool when the policy
    /// enables one. No-op when `window_seconds == 0`.
    pub(crate) fn open_challenge_window(env: &Env, pool_id: u64, outcome: u32) {
        let config = Self::get_challenge_config(env.clone());
        if config.window_seconds == 0 {
            return;
        }
        let deadline = env
            .ledger()
            .timestamp()
            .saturating_add(config.window_seconds);
        let key = DataKey::ResolutionChallenge(pool_id);
        env.storage().persistent().set(
            &key,
            &ResolutionChallenge {
                deadline,
                bond_amount: config.bond_amount,
                status: ChallengeStatus::Open,
                original_outcome: outcome,
                challenger: None,
                evidence_hash: None,
                evidence_uri: None,
                finalized_at: None,
            },
        );
        Self::extend_persistent(env, &key);

        ChallengeWindowOpenedEvent {
            pool_id,
            outcome,
            deadline,
            bond_amount: config.bond_amount,
        }
        .publish(env);
    }

    /// Payout gate for resolved pools.
    ///
    /// Returns `Ok(None)` when the pool has no challenge record (final at
    /// resolution), or `Ok(Some(t))` where `t` is when the resolution became
    /// final: the deadline for an unchallenged window, the adjudication time
    /// for a challenged one.
    ///
    /// # Errors
    /// - `PoolDisputed` — a challenge awaits adjudication.
    /// - `TimeConstraintError` — still inside the challenge window.
    pub(crate) fn resolution_final_since(
        env: &Env,
        pool_id: u64,
    ) -> Result<Option<u64>, PredifiError> {
        let record: ResolutionChallenge = match env
            .storage()
            .persistent()
            .get(&DataKey::ResolutionChallenge(pool_id))
        {
            Some(record) => record,
            None => return Ok(None),
        };
        match record.status {
            ChallengeStatus::Challenged => Err(PredifiError::PoolDisputed),
            ChallengeStatus::Finalized if record.challenger.is_some() => Ok(record.finalized_at),
            _ if env.ledger().timestamp() > record.deadline => Ok(Some(record.deadline)),
            _ => Err(PredifiError::TimeConstraintError),
        }
    }

    fn load_resolved_pool(env: &Env, pool_id: u64) -> Result<Pool, PredifiError> {
        let pool: Pool = env
            .storage()
            .persistent()
            .get(&DataKey::Pool(pool_id))
            .ok_or(PredifiError::PoolNotFound)?;
        if pool.state != MarketState::Resolved {
            return Err(PredifiError::InvalidPoolState);
        }
        Ok(pool)
    }
}
