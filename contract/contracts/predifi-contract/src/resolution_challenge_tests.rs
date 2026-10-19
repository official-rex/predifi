//! Resolution challenge window lifecycle tests.
//!
//! Timeline used throughout: pool ends at `POOL_END`, is resolved at
//! `RESOLVED_AT` with outcome 1, and the window closes at
//! `DEADLINE = RESOLVED_AT + WINDOW` (inclusive).

#![cfg(test)]

use crate::test::ROLE_ADMIN;
use crate::{
    ChallengeConfig, ChallengeStatus, MarketState, PoolConfig, PredifiContractClient, PredifiError,
    MAX_CHALLENGE_WINDOW,
};
use soroban_sdk::{
    symbol_short,
    testutils::{Address as _, Events, Ledger},
    token, vec, Address, BytesN, Env, String, Symbol, TryFromVal,
};

const ROLE_MODERATOR: u32 = 2;
const POOL_END: u64 = 10_000;
const RESOLVED_AT: u64 = 20_000;
const WINDOW: u64 = 3_600;
const DEADLINE: u64 = RESOLVED_AT + WINDOW;
const BOND: i128 = 50;
const STAKE: i128 = 100;
const MINTED: i128 = 1_000;

struct Ctx<'a> {
    client: PredifiContractClient<'a>,
    token: token::Client<'a>,
    admin: Address,
    moderator: Address,
    operator: Address,
    treasury: Address,
    /// Staked on outcome 1 (the initially resolved outcome).
    yes: Address,
    /// Staked on outcome 0.
    no: Address,
    pool_id: u64,
}

/// Pool with one stake on each side, challenge window enabled (`window`
/// seconds, `BOND`), not yet resolved.
fn setup_with_window(env: &Env, window: u64) -> Ctx<'_> {
    env.mock_all_auths();
    env.ledger().with_mut(|l| l.timestamp = 1_000);
    let (ac_client, client, token_address, token, token_admin, treasury, operator, creator) =
        crate::test::setup(env);
    let admin = Address::generate(env);
    let moderator = Address::generate(env);
    ac_client.grant_role(&admin, &ROLE_ADMIN);
    ac_client.grant_role(&moderator, &ROLE_MODERATOR);
    client.set_challenge_config(&admin, &window, &if window > 0 { BOND } else { 0 });

    let pool_id = client.create_pool(
        &creator,
        &POOL_END,
        &token_address,
        &2u32,
        &symbol_short!("Tech"),
        &PoolConfig {
            start_time: 0,
            description: String::from_str(env, "Challenge window pool"),
            metadata_url: String::from_str(env, "ipfs://challenge"),
            min_stake: 1,
            max_stake: 0,
            max_total_stake: 0,
            min_total_stake: 1,
            initial_liquidity: 0,
            required_resolutions: 1,
            private: false,
            whitelist_key: None,
            outcome_descriptions: vec![
                env,
                String::from_str(env, "No"),
                String::from_str(env, "Yes"),
            ],
        },
    );
    let yes = Address::generate(env);
    let no = Address::generate(env);
    token_admin.mint(&yes, &MINTED);
    token_admin.mint(&no, &MINTED);
    client.place_prediction(&yes, &pool_id, &STAKE, &1, &None, &None);
    client.place_prediction(&no, &pool_id, &STAKE, &0, &None, &None);

    Ctx {
        client,
        token,
        admin,
        moderator,
        operator,
        treasury,
        yes,
        no,
        pool_id,
    }
}

/// `setup_with_window(WINDOW)` and resolve to outcome 1 at `RESOLVED_AT`.
fn setup_resolved(env: &Env) -> Ctx<'_> {
    let ctx = setup_with_window(env, WINDOW);
    set_time(env, RESOLVED_AT);
    ctx.client.resolve_pool(&ctx.operator, &ctx.pool_id, &1);
    ctx
}

fn set_time(env: &Env, t: u64) {
    env.ledger().with_mut(|l| l.timestamp = t);
}

fn evidence(env: &Env) -> (BytesN<32>, String) {
    (
        BytesN::from_array(env, &[7u8; 32]),
        String::from_str(env, "ipfs://evidence-bundle"),
    )
}

fn challenge(env: &Env, ctx: &Ctx, who: &Address) -> Result<(), PredifiError> {
    let (hash, uri) = evidence(env);
    match ctx
        .client
        .try_challenge_resolution(who, &ctx.pool_id, &hash, &uri)
    {
        Ok(_) => Ok(()),
        Err(Ok(e)) => Err(e),
        Err(Err(e)) => panic!("unexpected invoke error: {:?}", e),
    }
}

fn claim(ctx: &Ctx, who: &Address) -> Result<i128, PredifiError> {
    match ctx.client.try_claim_winnings(who, &ctx.pool_id) {
        Ok(Ok(v)) => Ok(v),
        Err(Ok(e)) => Err(e),
        other => panic!("unexpected claim result: {:?}", other),
    }
}

fn status(ctx: &Ctx) -> ChallengeStatus {
    ctx.client
        .get_resolution_challenge(&ctx.pool_id)
        .unwrap()
        .status
}

fn emitted(env: &Env, topic: &str) -> bool {
    let topic = Symbol::new(env, topic);
    env.events().all().iter().any(|e| {
        e.1.get(0)
            .and_then(|v| Symbol::try_from_val(env, &v).ok())
            .is_some_and(|s| s == topic)
    })
}

// ── Configuration ────────────────────────────────────────────────────────────

#[test]
fn test_challenge_config_validation() {
    let env = Env::default();
    let ctx = setup_with_window(&env, 0);
    let c = &ctx.client;

    assert_eq!(
        c.get_challenge_config(),
        ChallengeConfig {
            window_seconds: 0,
            bond_amount: 0
        }
    );
    assert_eq!(
        c.try_set_challenge_config(&ctx.admin, &(MAX_CHALLENGE_WINDOW + 1), &BOND),
        Err(Ok(PredifiError::InvalidTimestamp))
    );
    assert_eq!(
        c.try_set_challenge_config(&ctx.admin, &WINDOW, &0),
        Err(Ok(PredifiError::InvalidAmount))
    );
    assert_eq!(
        c.try_set_challenge_config(&ctx.admin, &WINDOW, &-1),
        Err(Ok(PredifiError::InvalidAmount))
    );
    assert_eq!(
        c.try_set_challenge_config(&ctx.moderator, &WINDOW, &BOND),
        Err(Ok(PredifiError::Unauthorized))
    );
    c.set_challenge_config(&ctx.admin, &MAX_CHALLENGE_WINDOW, &BOND);
    assert_eq!(
        c.get_challenge_config(),
        ChallengeConfig {
            window_seconds: MAX_CHALLENGE_WINDOW,
            bond_amount: BOND
        }
    );
}

#[test]
fn test_disabled_window_keeps_immediate_finality() {
    let env = Env::default();
    let ctx = setup_with_window(&env, 0);
    set_time(&env, RESOLVED_AT);
    ctx.client.resolve_pool(&ctx.operator, &ctx.pool_id, &1);

    assert_eq!(ctx.client.get_resolution_challenge(&ctx.pool_id), None);
    assert_eq!(
        challenge(&env, &ctx, &ctx.no),
        Err(PredifiError::TimeConstraintError)
    );
    ctx.client.finalize_resolution(&ctx.pool_id);
    assert_eq!(claim(&ctx, &ctx.yes), Ok(2 * STAKE));
}

// ── Opening & timing boundaries ──────────────────────────────────────────────

#[test]
fn test_resolution_opens_window_with_snapshotted_policy() {
    let env = Env::default();
    let ctx = setup_resolved(&env);
    assert!(emitted(&env, "challenge_window_opened"));

    // A later policy change does not alter the open window.
    ctx.client.set_challenge_config(&ctx.admin, &60, &1);
    let record = ctx.client.get_resolution_challenge(&ctx.pool_id).unwrap();
    assert_eq!(record.deadline, DEADLINE);
    assert_eq!(record.bond_amount, BOND);
    assert_eq!(record.status, ChallengeStatus::Open);
    assert_eq!(record.original_outcome, 1);
}

#[test]
fn test_challenge_accepted_at_exact_deadline() {
    let env = Env::default();
    let ctx = setup_resolved(&env);
    set_time(&env, DEADLINE);
    assert_eq!(challenge(&env, &ctx, &ctx.no), Ok(()));
    assert_eq!(status(&ctx), ChallengeStatus::Challenged);
    assert_eq!(ctx.token.balance(&ctx.no), MINTED - STAKE - BOND);
}

#[test]
fn test_challenge_rejected_one_second_after_deadline() {
    let env = Env::default();
    let ctx = setup_resolved(&env);
    set_time(&env, DEADLINE + 1);
    assert_eq!(
        challenge(&env, &ctx, &ctx.no),
        Err(PredifiError::TimeConstraintError)
    );
    assert_eq!(status(&ctx), ChallengeStatus::Open);
}

#[test]
fn test_challenge_rejected_before_resolution() {
    let env = Env::default();
    let ctx = setup_with_window(&env, WINDOW);
    set_time(&env, RESOLVED_AT);
    assert_eq!(
        challenge(&env, &ctx, &ctx.no),
        Err(PredifiError::InvalidPoolState)
    );
}

// ── Rejected challenge attempts ──────────────────────────────────────────────

#[test]
fn test_challenge_eligibility_and_evidence_validation() {
    let env = Env::default();
    let ctx = setup_resolved(&env);

    // Not a participant.
    let outsider = Address::generate(&env);
    assert_eq!(
        challenge(&env, &ctx, &outsider),
        Err(PredifiError::Unauthorized)
    );

    // Zero evidence hash.
    let uri = String::from_str(&env, "ipfs://evidence");
    assert_eq!(
        ctx.client.try_challenge_resolution(
            &ctx.no,
            &ctx.pool_id,
            &BytesN::from_array(&env, &[0u8; 32]),
            &uri
        ),
        Err(Ok(PredifiError::InvalidData))
    );
    // Empty and oversized evidence URI.
    let hash = BytesN::from_array(&env, &[7u8; 32]);
    assert_eq!(
        ctx.client.try_challenge_resolution(
            &ctx.no,
            &ctx.pool_id,
            &hash,
            &String::from_str(&env, "")
        ),
        Err(Ok(PredifiError::InvalidData))
    );
    let long = String::from_str(&env, &"a".repeat(513));
    assert_eq!(
        ctx.client
            .try_challenge_resolution(&ctx.no, &ctx.pool_id, &hash, &long),
        Err(Ok(PredifiError::InvalidData))
    );
    assert_eq!(status(&ctx), ChallengeStatus::Open);

    // Only one challenge per pool.
    assert_eq!(challenge(&env, &ctx, &ctx.no), Ok(()));
    assert_eq!(
        challenge(&env, &ctx, &ctx.yes),
        Err(PredifiError::PoolDisputed)
    );
}

// ── Payout prevention ────────────────────────────────────────────────────────

#[test]
fn test_payouts_blocked_during_window_and_while_challenged() {
    let env = Env::default();
    let ctx = setup_resolved(&env);

    // Open window: not final yet.
    assert_eq!(
        claim(&ctx, &ctx.yes),
        Err(PredifiError::TimeConstraintError)
    );

    challenge(&env, &ctx, &ctx.no).unwrap();

    // Challenged: blocked even after the deadline.
    set_time(&env, DEADLINE + 1);
    assert_eq!(claim(&ctx, &ctx.yes), Err(PredifiError::PoolDisputed));
    assert_eq!(
        ctx.client.try_finalize_resolution(&ctx.pool_id),
        Err(Ok(PredifiError::PoolDisputed))
    );

    // A batch claim skips the pool without consuming the user's claim.
    let batch = ctx
        .client
        .batch_claim_winnings(&ctx.yes, &vec![&env, ctx.pool_id]);
    assert_eq!(batch.get(ctx.pool_id), Some(0));
    assert_eq!(ctx.token.balance(&ctx.yes), MINTED - STAKE);

    ctx.client
        .adjudicate_challenge(&ctx.moderator, &ctx.pool_id, &false, &0);
    assert_eq!(claim(&ctx, &ctx.yes), Ok(2 * STAKE));
}

// ── Adjudication outcomes ────────────────────────────────────────────────────

#[test]
fn test_rejected_challenge_forfeits_bond_and_keeps_outcome() {
    let env = Env::default();
    let ctx = setup_resolved(&env);
    challenge(&env, &ctx, &ctx.no).unwrap();

    let treasury_before = ctx.token.balance(&ctx.treasury);
    ctx.client
        .adjudicate_challenge(&ctx.moderator, &ctx.pool_id, &false, &0);
    assert!(emitted(&env, "challenge_adjudicated"));
    assert!(emitted(&env, "resolution_finalized"));

    assert_eq!(ctx.token.balance(&ctx.treasury), treasury_before + BOND);
    assert_eq!(ctx.token.balance(&ctx.no), MINTED - STAKE - BOND);
    assert_eq!(ctx.client.get_pool(&ctx.pool_id).outcome, 1);
    assert_eq!(status(&ctx), ChallengeStatus::Finalized);

    // Final before the deadline: payouts open immediately.
    assert_eq!(claim(&ctx, &ctx.yes), Ok(2 * STAKE));
    assert_eq!(claim(&ctx, &ctx.no), Ok(0));
}

#[test]
fn test_successful_challenge_corrects_outcome_and_refunds_bond() {
    let env = Env::default();
    let ctx = setup_resolved(&env);
    challenge(&env, &ctx, &ctx.no).unwrap();
    let record = ctx.client.get_resolution_challenge(&ctx.pool_id).unwrap();
    assert_eq!(record.challenger, Some(ctx.no.clone()));
    assert_eq!(record.evidence_hash, Some(evidence(&env).0));
    assert_eq!(record.evidence_uri, Some(evidence(&env).1));

    let treasury_before = ctx.token.balance(&ctx.treasury);
    ctx.client
        .adjudicate_challenge(&ctx.moderator, &ctx.pool_id, &true, &0);

    let pool = ctx.client.get_pool(&ctx.pool_id);
    assert_eq!(pool.state, MarketState::Resolved);
    assert_eq!(pool.outcome, 0);
    assert_eq!(ctx.token.balance(&ctx.treasury), treasury_before);
    assert_eq!(ctx.token.balance(&ctx.no), MINTED - STAKE);

    assert_eq!(claim(&ctx, &ctx.no), Ok(2 * STAKE));
    assert_eq!(claim(&ctx, &ctx.yes), Ok(0));
    assert_eq!(ctx.token.balance(&ctx.no), MINTED + STAKE);
}

#[test]
fn test_adjudication_permissions_and_preconditions() {
    let env = Env::default();
    let ctx = setup_resolved(&env);

    // Nothing to adjudicate yet.
    assert_eq!(
        ctx.client
            .try_adjudicate_challenge(&ctx.moderator, &ctx.pool_id, &false, &0),
        Err(Ok(PredifiError::InvalidPoolState))
    );
    challenge(&env, &ctx, &ctx.no).unwrap();

    // Only moderators adjudicate; admins and participants cannot.
    for who in [&ctx.admin, &ctx.yes, &ctx.no] {
        assert_eq!(
            ctx.client
                .try_adjudicate_challenge(who, &ctx.pool_id, &true, &0),
            Err(Ok(PredifiError::Unauthorized))
        );
    }
    // Upheld must name a different, valid outcome.
    for bad in [1u32, 2u32] {
        assert_eq!(
            ctx.client
                .try_adjudicate_challenge(&ctx.moderator, &ctx.pool_id, &true, &bad),
            Err(Ok(PredifiError::InvalidOutcome))
        );
    }
    ctx.client
        .adjudicate_challenge(&ctx.moderator, &ctx.pool_id, &true, &0);

    // Second adjudication is rejected; finalization stays a no-op.
    assert_eq!(
        ctx.client
            .try_adjudicate_challenge(&ctx.moderator, &ctx.pool_id, &false, &0),
        Err(Ok(PredifiError::InvalidPoolState))
    );
    ctx.client.finalize_resolution(&ctx.pool_id);
    assert_eq!(ctx.client.get_pool(&ctx.pool_id).outcome, 0);
}

// ── Finalization ─────────────────────────────────────────────────────────────

#[test]
fn test_finalize_unchallenged_resolution_is_idempotent() {
    let env = Env::default();
    let ctx = setup_resolved(&env);

    set_time(&env, DEADLINE);
    assert_eq!(
        ctx.client.try_finalize_resolution(&ctx.pool_id),
        Err(Ok(PredifiError::TimeConstraintError))
    );

    set_time(&env, DEADLINE + 1);
    ctx.client.finalize_resolution(&ctx.pool_id);
    assert!(emitted(&env, "resolution_finalized"));
    let record = ctx.client.get_resolution_challenge(&ctx.pool_id).unwrap();
    assert_eq!(record.status, ChallengeStatus::Finalized);
    assert_eq!(record.finalized_at, Some(DEADLINE + 1));

    ctx.client.finalize_resolution(&ctx.pool_id);
    assert!(!emitted(&env, "resolution_finalized"));
    assert_eq!(
        ctx.client.get_resolution_challenge(&ctx.pool_id).unwrap(),
        record
    );

    assert_eq!(
        challenge(&env, &ctx, &ctx.no),
        Err(PredifiError::TimeConstraintError)
    );
    assert_eq!(claim(&ctx, &ctx.yes), Ok(2 * STAKE));
}

#[test]
fn test_claims_open_after_deadline_without_explicit_finalize() {
    let env = Env::default();
    let ctx = setup_resolved(&env);
    set_time(&env, DEADLINE + 1);
    assert_eq!(claim(&ctx, &ctx.yes), Ok(2 * STAKE));
}

#[test]
fn test_claim_window_starts_when_resolution_becomes_final() {
    let env = Env::default();
    let ctx = setup_resolved(&env);
    let claim_window = 2_592_000; // default 30 days

    // Measured from resolution this would already be expired.
    set_time(&env, DEADLINE + claim_window);
    assert_eq!(claim(&ctx, &ctx.yes), Ok(2 * STAKE));

    set_time(&env, DEADLINE + claim_window + 1);
    assert_eq!(claim(&ctx, &ctx.no), Err(PredifiError::InvalidTimestamp));
}

#[test]
fn test_oracle_resolution_path_opens_window() {
    let env = Env::default();
    let ctx = setup_with_window(&env, WINDOW);
    let oracle = Address::generate(&env);
    ctx.client.add_oracle(&ctx.admin, &oracle);
    ctx.client
        .init_oracle(&ctx.admin, &Address::generate(&env), &100_000u64, &100u32);
    let feed = Symbol::new(&env, "ETHUSD");
    ctx.client
        .set_price_condition(&ctx.operator, &ctx.pool_id, &feed, &1_000, &1u32, &0u32);
    ctx.client
        .update_price_feed(&oracle, &feed, &2_000, &1, &999u64, &1_000_000u64);

    set_time(&env, RESOLVED_AT);
    ctx.client.resolve_pool_from_price(&ctx.pool_id);
    assert_eq!(
        ctx.client
            .get_resolution_challenge(&ctx.pool_id)
            .unwrap()
            .deadline,
        DEADLINE
    );
    assert_eq!(
        claim(&ctx, &ctx.yes),
        Err(PredifiError::TimeConstraintError)
    );
}
