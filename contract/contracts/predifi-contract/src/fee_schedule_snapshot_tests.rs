//! Fee schedule snapshot tests: each pool settles with the fee schedule in
//! force when it was created, and legacy pools keep their original behaviour.

#![cfg(test)]

use crate::test::ROLE_ADMIN;
use crate::{
    DataKey, FeeSchedule, FeeTier, PoolConfig, PredifiContractClient, FEE_CHANGE_TIMELOCK_SECONDS,
};
use soroban_sdk::{
    symbol_short,
    testutils::{Address as _, Events, Ledger},
    token, vec, Address, Env, IntoVal, Map, String, Symbol, TryFromVal, Val,
};

const POOL_END: u64 = 1_000_000;

fn create_pool(
    env: &Env,
    client: &PredifiContractClient,
    creator: &Address,
    token: &Address,
) -> u64 {
    client.create_pool(
        creator,
        &POOL_END,
        token,
        &2u32,
        &symbol_short!("Tech"),
        &PoolConfig {
            start_time: 0,
            description: String::from_str(env, "Fee snapshot pool"),
            metadata_url: String::from_str(env, "ipfs://fee-snapshot"),
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
    )
}

fn apply_base_fee(env: &Env, client: &PredifiContractClient, admin: &Address, fee_bps: u32) {
    client.set_fee_bps(admin, &fee_bps);
    env.ledger()
        .with_mut(|l| l.timestamp += FEE_CHANGE_TIMELOCK_SECONDS);
    client.apply_fee_bps(admin);
}

/// Winner stakes 100 on outcome 1, loser 100 on outcome 0.
fn stake_both_sides(
    client: &PredifiContractClient,
    token_admin: &token::StellarAssetClient,
    pool_id: u64,
    winner: &Address,
    loser: &Address,
) {
    token_admin.mint(winner, &100);
    token_admin.mint(loser, &100);
    client.place_prediction(winner, &pool_id, &100, &1, &None, &None);
    client.place_prediction(loser, &pool_id, &100, &0, &None, &None);
}

fn event_data(env: &Env, topic: &str) -> Option<Map<Symbol, Val>> {
    let topic = Symbol::new(env, topic);
    env.events().all().iter().find_map(|e| {
        let first = e.1.get(0)?;
        (Symbol::try_from_val(env, &first).ok()? == topic).then(|| e.2.clone().into_val(env))
    })
}

#[test]
fn test_fee_schedule_version_bumps_on_every_schedule_change() {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().with_mut(|l| l.timestamp = 1_000);
    let (ac_client, client, token_address, _, _, _, _, creator) = crate::test::setup(&env);
    let admin = Address::generate(&env);
    ac_client.grant_role(&admin, &ROLE_ADMIN);

    let p0 = create_pool(&env, &client, &creator, &token_address);
    apply_base_fee(&env, &client, &admin, 300);
    let p1 = create_pool(&env, &client, &creator, &token_address);
    let tiers = vec![
        &env,
        FeeTier {
            stake_threshold: 150,
            fee_bps: 50,
        },
    ];
    client.set_fee_tiers(&admin, &tiers);
    let p2 = create_pool(&env, &client, &creator, &token_address);

    assert_eq!(
        client.get_pool_fee_schedule(&p0),
        Some(FeeSchedule {
            version: 1,
            base_fee_bps: 0,
            tiers: vec![&env],
        })
    );
    assert_eq!(
        client.get_pool_fee_schedule(&p1),
        Some(FeeSchedule {
            version: 2,
            base_fee_bps: 300,
            tiers: vec![&env],
        })
    );
    assert_eq!(
        client.get_pool_fee_schedule(&p2),
        Some(FeeSchedule {
            version: 3,
            base_fee_bps: 300,
            tiers,
        })
    );
}

#[test]
fn test_pool_creation_emits_fee_schedule_snapshot_event() {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().with_mut(|l| l.timestamp = 1_000);
    let (_, client, token_address, _, _, _, _, creator) = crate::test::setup(&env);

    let pool_id = create_pool(&env, &client, &creator, &token_address);

    let data = event_data(&env, "fee_schedule_snapshot").expect("snapshot event");
    let get = |k: &str| data.get(Symbol::new(&env, k)).unwrap();
    assert_eq!(u64::try_from_val(&env, &get("pool_id")).unwrap(), pool_id);
    assert_eq!(u32::try_from_val(&env, &get("version")).unwrap(), 1);
    assert_eq!(u32::try_from_val(&env, &get("base_fee_bps")).unwrap(), 0);
    assert_eq!(u32::try_from_val(&env, &get("tiers_count")).unwrap(), 0);
}

/// Pool A is created under base fee 300 bps with no tiers. The treasury then
/// raises the base fee to 800 bps and adds a 50 bps tier, and pool B is created.
/// Both settle after all changes; each must apply its own creation-time schedule.
#[test]
fn test_fee_changes_between_creation_and_settlement_do_not_apply_retroactively() {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().with_mut(|l| l.timestamp = 1_000);
    let (ac_client, client, token_address, token, token_admin, _, operator, creator) =
        crate::test::setup(&env);
    let admin = Address::generate(&env);
    ac_client.grant_role(&admin, &ROLE_ADMIN);

    apply_base_fee(&env, &client, &admin, 300);
    let pool_a = create_pool(&env, &client, &creator, &token_address);
    let (winner_a, loser_a) = (Address::generate(&env), Address::generate(&env));
    stake_both_sides(&client, &token_admin, pool_a, &winner_a, &loser_a);

    apply_base_fee(&env, &client, &admin, 800);
    client.set_fee_tiers(
        &admin,
        &vec![
            &env,
            FeeTier {
                stake_threshold: 150,
                fee_bps: 50,
            },
        ],
    );
    let pool_b = create_pool(&env, &client, &creator, &token_address);
    let (winner_b, loser_b) = (Address::generate(&env), Address::generate(&env));
    stake_both_sides(&client, &token_admin, pool_b, &winner_b, &loser_b);

    // A further change after both pools exist must affect neither.
    apply_base_fee(&env, &client, &admin, 2_000);

    env.ledger().with_mut(|l| l.timestamp = POOL_END + 1);
    client.resolve_pool(&operator, &pool_a, &1);
    client.resolve_pool(&operator, &pool_b, &1);

    // Pool A: base 300 bps of 200 = 6 fee; winner receives 194.
    assert_eq!(client.get_pool(&pool_a).fee_bps, 300);
    assert_eq!(client.claim_winnings(&winner_a, &pool_a), 194);
    assert_eq!(token.balance(&winner_a), 194);

    // Pool B: total stake 200 >= 150 tier, 50 bps of 200 = 1 fee; winner receives 199.
    assert_eq!(client.get_pool(&pool_b).fee_bps, 50);
    assert_eq!(client.claim_winnings(&winner_b, &pool_b), 199);
    assert_eq!(token.balance(&winner_b), 199);
}

/// Legacy fixture: a pool without a stored snapshot (created before the
/// upgrade) keeps the pre-snapshot behaviour and settles on the live schedule.
#[test]
fn test_legacy_pool_without_snapshot_uses_live_schedule() {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().with_mut(|l| l.timestamp = 1_000);
    let (ac_client, client, token_address, _, token_admin, _, operator, creator) =
        crate::test::setup(&env);
    let admin = Address::generate(&env);
    ac_client.grant_role(&admin, &ROLE_ADMIN);

    let legacy = create_pool(&env, &client, &creator, &token_address);
    env.as_contract(&client.address, || {
        env.storage()
            .persistent()
            .remove(&DataKey::PoolFeeSchedule(legacy));
    });
    assert_eq!(client.get_pool_fee_schedule(&legacy), None);

    let (winner, loser) = (Address::generate(&env), Address::generate(&env));
    stake_both_sides(&client, &token_admin, legacy, &winner, &loser);
    apply_base_fee(&env, &client, &admin, 500);

    env.ledger().with_mut(|l| l.timestamp = POOL_END + 1);
    client.resolve_pool(&operator, &legacy, &1);

    // Live base fee 500 bps of 200 = 10 fee; winner receives 190.
    assert_eq!(client.get_pool(&legacy).fee_bps, 500);
    assert_eq!(client.claim_winnings(&winner, &legacy), 190);
}
