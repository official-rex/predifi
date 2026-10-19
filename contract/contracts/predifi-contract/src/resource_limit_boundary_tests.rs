//! Contract-level resource limit boundary tests (issue #39).
//!
//! Each limit is exercised at its maximum, one below the maximum, and one
//! above it. Aggregate-stake tests also cover the `i128` overflow path.
//!
//! Limits under test:
//! - `MAX_OUTCOMES_PER_POOL`
//! - `MAX_PREDICTIONS_PER_POOL` (distinct predictors in one pool)
//! - `MAX_AGGREGATE_STAKE_PER_POOL`

#![cfg(test)]

use crate::{
    PoolConfig, PredifiContract, PredifiContractClient, PredifiError, MAX_AGGREGATE_STAKE_PER_POOL,
    MAX_OUTCOMES_PER_POOL, MAX_PREDICTIONS_PER_POOL,
};
use soroban_sdk::{
    symbol_short,
    testutils::{Address as _, Ledger},
    token, vec, Address, Env, String, Vec,
};

mod dummy_ac {
    use soroban_sdk::{contract, contractimpl, Address, Env, Symbol};

    #[contract]
    pub struct DummyAC;

    #[contractimpl]
    impl DummyAC {
        pub fn grant_role(env: Env, user: Address, role: u32) {
            let key = (Symbol::new(&env, "role"), user.clone(), role);
            env.storage().instance().set(&key, &true);
        }

        pub fn has_role(env: Env, user: Address, role: u32) -> bool {
            let key = (Symbol::new(&env, "role"), user, role);
            env.storage().instance().get(&key).unwrap_or(false)
        }

        // `create_pool` queries this to ensure `required_resolutions` can be met.
        pub fn get_operator_count(env: Env) -> u32 {
            env.storage()
                .instance()
                .get(&Symbol::new(&env, "op_count"))
                .unwrap_or(0)
        }
    }
}

struct Ctx<'a> {
    client: PredifiContractClient<'a>,
    token: token::Client<'a>,
    token_admin: token::StellarAssetClient<'a>,
    token_address: Address,
    admin: Address,
}

fn setup(env: &Env) -> Ctx<'_> {
    env.mock_all_auths();
    env.ledger().with_mut(|li| {
        li.protocol_version = 23;
        li.timestamp = 1_000;
    });

    let admin = Address::generate(env);
    let treasury = Address::generate(env);

    let ac_id = env.register(dummy_ac::DummyAC, ());
    let ac_client = dummy_ac::DummyACClient::new(env, &ac_id);
    ac_client.grant_role(&admin, &0u32);

    let contract_id = env.register(PredifiContract, ());
    let client = PredifiContractClient::new(env, &contract_id);
    client.init(&ac_id, &treasury, &0u32, &0u64, &3600u64, &0u32);

    let token_deployer = Address::generate(env);
    let token_contract = env.register_stellar_asset_contract_v2(token_deployer);
    let token_address = token_contract.address();
    let token = token::Client::new(env, &token_address);
    let token_admin = token::StellarAssetClient::new(env, &token_address);

    client.add_token_to_whitelist(&admin, &token_address);

    Ctx {
        client,
        token,
        token_admin,
        token_address,
        admin,
    }
}

fn outcome_descriptions(env: &Env, count: u32) -> Vec<String> {
    let mut descriptions = Vec::new(env);
    for _ in 0..count {
        descriptions.push_back(String::from_str(env, "Outcome"));
    }
    descriptions
}

fn base_config(env: &Env, options_count: u32) -> PoolConfig {
    PoolConfig {
        start_time: 0,
        description: String::from_str(env, "resource limit boundary pool"),
        metadata_url: String::from_str(env, "ipfs://resource-limits"),
        min_stake: 1i128,
        max_stake: 0i128,
        min_total_stake: 1i128,
        max_total_stake: 0i128,
        initial_liquidity: 0i128,
        required_resolutions: 1u32,
        private: false,
        whitelist_key: None,
        outcome_descriptions: outcome_descriptions(env, options_count),
    }
}

fn create_binary_pool(ctx: &Ctx<'_>, env: &Env) -> u64 {
    ctx.client.create_pool(
        &ctx.admin,
        &1_000_000u64,
        &ctx.token_address,
        &2u32,
        &symbol_short!("Sports"),
        &base_config(env, 2),
    )
}

/// Place `count` predictions, each from a fresh address, on outcome 0.
fn place_distinct_predictions(
    ctx: &Ctx<'_>,
    env: &Env,
    pool_id: u64,
    count: u32,
) -> std::vec::Vec<Address> {
    let mut users = std::vec::Vec::with_capacity(count as usize);
    for _ in 0..count {
        let user = Address::generate(env);
        ctx.token_admin.mint(&user, &1i128);
        ctx.client
            .place_prediction(&user, &pool_id, &1i128, &0u32, &None, &None);
        users.push(user);
    }
    users
}

// ═══════════════════════════════════════════════════════════════════════════
// OUTCOME COUNT
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_outcome_limit_one_below_max_succeeds() {
    let env = Env::default();
    let ctx = setup(&env);
    let options = MAX_OUTCOMES_PER_POOL - 1;

    let pool_id = ctx.client.create_pool(
        &ctx.admin,
        &1_000_000u64,
        &ctx.token_address,
        &options,
        &symbol_short!("Tech"),
        &base_config(&env, options),
    );

    assert_eq!(ctx.client.get_pool(&pool_id).options_count, options);
}

#[test]
fn test_outcome_limit_at_max_succeeds() {
    let env = Env::default();
    let ctx = setup(&env);
    let options = MAX_OUTCOMES_PER_POOL;

    let pool_id = ctx.client.create_pool(
        &ctx.admin,
        &1_000_000u64,
        &ctx.token_address,
        &options,
        &symbol_short!("Tech"),
        &base_config(&env, options),
    );

    assert_eq!(ctx.client.get_pool(&pool_id).options_count, options);
}

#[test]
fn test_outcome_limit_one_above_max_is_rejected() {
    let env = Env::default();
    let ctx = setup(&env);
    let options = MAX_OUTCOMES_PER_POOL + 1;

    let result = ctx.client.try_create_pool(
        &ctx.admin,
        &1_000_000u64,
        &ctx.token_address,
        &options,
        &symbol_short!("Tech"),
        &base_config(&env, options),
    );

    assert_eq!(result, Err(Ok(PredifiError::MaxOutcomesExceeded)));
}

// ═══════════════════════════════════════════════════════════════════════════
// PREDICTION COUNT (distinct predictors per pool)
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_prediction_limit_one_below_max_succeeds() {
    let env = Env::default();
    let ctx = setup(&env);
    let pool_id = create_binary_pool(&ctx, &env);

    let count = MAX_PREDICTIONS_PER_POOL - 1;
    place_distinct_predictions(&ctx, &env, pool_id, count);

    let pool = ctx.client.get_pool(&pool_id);
    assert_eq!(pool.participants_count, count);
    assert_eq!(pool.total_stake, count as i128);
}

#[test]
fn test_prediction_limit_at_max_succeeds() {
    let env = Env::default();
    let ctx = setup(&env);
    let pool_id = create_binary_pool(&ctx, &env);

    let count = MAX_PREDICTIONS_PER_POOL;
    place_distinct_predictions(&ctx, &env, pool_id, count);

    let pool = ctx.client.get_pool(&pool_id);
    assert_eq!(pool.participants_count, count);
    assert_eq!(pool.total_stake, count as i128);
}

#[test]
fn test_prediction_limit_one_above_max_is_rejected_before_write() {
    let env = Env::default();
    let ctx = setup(&env);
    let pool_id = create_binary_pool(&ctx, &env);

    place_distinct_predictions(&ctx, &env, pool_id, MAX_PREDICTIONS_PER_POOL);

    let extra = Address::generate(&env);
    ctx.token_admin.mint(&extra, &1i128);
    let result = ctx
        .client
        .try_place_prediction(&extra, &pool_id, &1i128, &0u32, &None, &None);

    assert_eq!(result, Err(Ok(PredifiError::MaxPredictionsPerPoolExceeded)));

    // The rejected prediction must not have mutated pool or user state.
    let pool = ctx.client.get_pool(&pool_id);
    assert_eq!(pool.participants_count, MAX_PREDICTIONS_PER_POOL);
    assert_eq!(pool.total_stake, MAX_PREDICTIONS_PER_POOL as i128);
    assert_eq!(ctx.token.balance(&extra), 1i128);
}

// ═══════════════════════════════════════════════════════════════════════════
// AGGREGATE STAKE
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_aggregate_stake_one_below_max_succeeds() {
    let env = Env::default();
    let ctx = setup(&env);
    let pool_id = create_binary_pool(&ctx, &env);

    let amount = MAX_AGGREGATE_STAKE_PER_POOL - 1;
    let user = Address::generate(&env);
    ctx.token_admin.mint(&user, &amount);
    ctx.client
        .place_prediction(&user, &pool_id, &amount, &0u32, &None, &None);

    assert_eq!(ctx.client.get_pool(&pool_id).total_stake, amount);
}

#[test]
fn test_aggregate_stake_at_max_succeeds() {
    let env = Env::default();
    let ctx = setup(&env);
    let pool_id = create_binary_pool(&ctx, &env);

    let amount = MAX_AGGREGATE_STAKE_PER_POOL;
    let user = Address::generate(&env);
    ctx.token_admin.mint(&user, &amount);
    ctx.client
        .place_prediction(&user, &pool_id, &amount, &0u32, &None, &None);

    assert_eq!(ctx.client.get_pool(&pool_id).total_stake, amount);
}

#[test]
fn test_aggregate_stake_one_above_max_is_rejected_before_write() {
    let env = Env::default();
    let ctx = setup(&env);
    let pool_id = create_binary_pool(&ctx, &env);

    let amount = MAX_AGGREGATE_STAKE_PER_POOL + 1;
    let user = Address::generate(&env);
    ctx.token_admin.mint(&user, &amount);
    let result = ctx
        .client
        .try_place_prediction(&user, &pool_id, &amount, &0u32, &None, &None);

    assert_eq!(result, Err(Ok(PredifiError::MaxAggregateStakeExceeded)));
    assert_eq!(ctx.client.get_pool(&pool_id).total_stake, 0);
    assert_eq!(ctx.token.balance(&user), amount);
}

#[test]
fn test_aggregate_stake_overflowing_addition_is_rejected() {
    let env = Env::default();
    let ctx = setup(&env);
    let pool_id = create_binary_pool(&ctx, &env);

    // Fill the pool to the contract ceiling.
    let first = Address::generate(&env);
    ctx.token_admin.mint(&first, &MAX_AGGREGATE_STAKE_PER_POOL);
    ctx.client.place_prediction(
        &first,
        &pool_id,
        &MAX_AGGREGATE_STAKE_PER_POOL,
        &0u32,
        &None,
        &None,
    );

    // `total_stake + i128::MAX` cannot be represented; the checked addition
    // must surface the typed limit error instead of panicking or wrapping.
    let second = Address::generate(&env);
    ctx.token_admin.mint(&second, &i128::MAX);
    let result =
        ctx.client
            .try_place_prediction(&second, &pool_id, &i128::MAX, &0u32, &None, &None);

    assert_eq!(result, Err(Ok(PredifiError::MaxAggregateStakeExceeded)));
    assert_eq!(
        ctx.client.get_pool(&pool_id).total_stake,
        MAX_AGGREGATE_STAKE_PER_POOL
    );
}

#[test]
fn test_aggregate_stake_cap_cannot_be_raised_above_contract_ceiling() {
    let env = Env::default();
    let ctx = setup(&env);
    let pool_id = create_binary_pool(&ctx, &env);

    let result = ctx.client.try_increase_max_total_stake(
        &ctx.admin,
        &pool_id,
        &(MAX_AGGREGATE_STAKE_PER_POOL + 1),
    );

    assert_eq!(result, Err(Ok(PredifiError::MaxAggregateStakeExceeded)));
}

#[test]
#[should_panic(expected = "Error(Contract, #116)")]
fn test_create_pool_rejects_cap_above_contract_ceiling() {
    let env = Env::default();
    let ctx = setup(&env);
    let mut config = base_config(&env, 2);
    config.max_total_stake = MAX_AGGREGATE_STAKE_PER_POOL + 1;

    ctx.client.create_pool(
        &ctx.admin,
        &1_000_000u64,
        &ctx.token_address,
        &2u32,
        &symbol_short!("Tech"),
        &config,
    );
}

#[test]
fn test_total_stake_never_exceeds_cap_after_boundary_sequence() {
    let env = Env::default();
    let ctx = setup(&env);
    let pool_id = create_binary_pool(&ctx, &env);

    // max - 1, then +1 to exactly fill the cap, then +1 must be rejected.
    let nearly_full = MAX_AGGREGATE_STAKE_PER_POOL - 1;
    let first = Address::generate(&env);
    ctx.token_admin.mint(&first, &nearly_full);
    ctx.client
        .place_prediction(&first, &pool_id, &nearly_full, &0u32, &None, &None);

    let second = Address::generate(&env);
    ctx.token_admin.mint(&second, &2i128);
    ctx.client
        .place_prediction(&second, &pool_id, &1i128, &1u32, &None, &None);
    assert_eq!(
        ctx.client.get_pool(&pool_id).total_stake,
        MAX_AGGREGATE_STAKE_PER_POOL
    );

    let result = ctx
        .client
        .try_place_prediction(&second, &pool_id, &1i128, &1u32, &None, &None);
    assert_eq!(result, Err(Ok(PredifiError::MaxAggregateStakeExceeded)));
    assert_eq!(
        ctx.client.get_pool(&pool_id).total_stake,
        MAX_AGGREGATE_STAKE_PER_POOL
    );
}
