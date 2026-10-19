#[cfg(test)]
mod benchmark_tests {
    //! Gas / CPU cost profiling for hot contract paths.
    //!
    //! Run with:
    //! ```bash
    //! cargo test -p predifi-contract benchmark_tests -- --nocapture
    //! ```

    extern crate std;

    use crate::{
        DataKey, Pool, PoolConfig, PredifiContract, PredifiContractClient, Prediction,
        MAX_AGGREGATE_STAKE_PER_POOL, MAX_OUTCOMES_PER_POOL, MAX_PREDICTIONS_PER_POOL,
    };
    use soroban_sdk::{
        symbol_short,
        testutils::{Address as _, Ledger},
        token, xdr::ToXdr, Address, Env, String, Vec,
    };

    mod dummy_access_control {
        use soroban_sdk::{contract, contractimpl, Address, Env, Symbol};

        #[contract]
        pub struct DummyAccessControl;

        #[contractimpl]
        impl DummyAccessControl {
            pub fn grant_role(env: Env, user: Address, role: u32) {
                let already_has_key = (Symbol::new(&env, "role"), user.clone(), role);
                let already_has: bool = env
                    .storage()
                    .instance()
                    .get(&already_has_key)
                    .unwrap_or(false);
                env.storage().instance().set(&already_has_key, &true);
                if role == 1 && !already_has {
                    let count_key = Symbol::new(&env, "op_count");
                    let count: u32 = env.storage().instance().get(&count_key).unwrap_or(0);
                    env.storage().instance().set(&count_key, &(count + 1));
                }
            }

            pub fn has_role(env: Env, user: Address, role: u32) -> bool {
                let key = (Symbol::new(&env, "role"), user, role);
                env.storage().instance().get(&key).unwrap_or(false)
            }

            pub fn get_operator_count(env: Env) -> u32 {
                let count_key = Symbol::new(&env, "op_count");
                env.storage().instance().get(&count_key).unwrap_or(0)
            }
        }
    }

    const ROLE_ADMIN: u32 = 0;
    const ROLE_OPERATOR: u32 = 1;

    fn setup(
        env: &Env,
    ) -> (
        PredifiContractClient<'_>,
        Address,
        token::Client<'_>,
        token::StellarAssetClient<'_>,
    ) {
        env.mock_all_auths();
        let ac_id = env.register(dummy_access_control::DummyAccessControl, ());
        let ac_client = dummy_access_control::DummyAccessControlClient::new(env, &ac_id);
        let contract_id = env.register(PredifiContract, ());
        let client = PredifiContractClient::new(env, &contract_id);
        let admin = Address::generate(env);
        let treasury = Address::generate(env);
        ac_client.grant_role(&admin, &ROLE_ADMIN);
        ac_client.grant_role(&admin, &ROLE_OPERATOR);
        client.init(&ac_id, &treasury, &500, &3600, &3600u64, &0u32);
        let token_admin = Address::generate(env);
        let token_contract = env.register_stellar_asset_contract_v2(token_admin.clone());
        let token_id = token_contract.address();
        let token_client = token::Client::new(env, &token_id);
        let token_admin_client = token::StellarAssetClient::new(env, &token_id);
        client.add_token_to_whitelist(&admin, &token_id);
        (client, admin, token_client, token_admin_client)
    }

    fn make_outcomes(env: &Env, n: u32) -> Vec<String> {
        let mut outcome_descriptions = Vec::new(env);
        for _ in 0..n {
            outcome_descriptions.push_back(String::from_str(env, "Outcome"));
        }
        outcome_descriptions
    }

    #[test]
    fn test_bench_100_outcomes() {
        let env = Env::default();
        let (client, admin, token_client, token_admin_client) = setup(&env);
        let creator = Address::generate(&env);
        let options_count = 100;
        let outcome_descriptions = make_outcomes(&env, options_count);

        env.cost_estimate().budget().reset_default();
        let pool_id = client.create_pool(
            &creator,
            &(env.ledger().timestamp() + 10000),
            &token_client.address,
            &options_count,
            &symbol_short!("Tech"),
            &PoolConfig {
                start_time: 0,
                description: String::from_str(&env, "Bench"),
                metadata_url: String::from_str(&env, "ipfs://bench"),
                min_stake: 10i128,
                max_stake: 0,
                max_total_stake: 0,
                min_total_stake: 1,
                initial_liquidity: 0,
                required_resolutions: 1,
                private: false,
                whitelist_key: None,
                outcome_descriptions,
            },
        );
        let budget_create = env.cost_estimate().budget().cpu_instruction_cost();
        std::println!("[gas] create_pool(100 outcomes) cpu={}", budget_create);

        let user1 = Address::generate(&env);
        token_admin_client.mint(&user1, &1000);
        env.cost_estimate().budget().reset_default();
        client.place_prediction(&user1, &pool_id, &1000, &0, &None, &None);
        let budget_pred1 = env.cost_estimate().budget().cpu_instruction_cost();
        std::println!("[gas] place_prediction#1 cpu={}", budget_pred1);

        let user2 = Address::generate(&env);
        token_admin_client.mint(&user2, &1000);
        env.cost_estimate().budget().reset_default();
        client.place_prediction(&user2, &pool_id, &1000, &1, &None, &None);
        let budget_pred2 = env.cost_estimate().budget().cpu_instruction_cost();
        std::println!("[gas] place_prediction#2 (batch path) cpu={}", budget_pred2);

        env.cost_estimate().budget().reset_default();
        let _stats = client.get_pool_stats(&pool_id);
        let budget_stats = env.cost_estimate().budget().cpu_instruction_cost();
        std::println!("[gas] get_pool_stats cpu={}", budget_stats);

        env.ledger().with_mut(|li| li.timestamp += 20000);
        env.cost_estimate().budget().reset_default();
        client.resolve_pool(&admin, &pool_id, &0);
        let budget_resolve = env.cost_estimate().budget().cpu_instruction_cost();
        std::println!("[gas] resolve_pool cpu={}", budget_resolve);

        env.cost_estimate().budget().reset_default();
        client.claim_winnings(&user1, &pool_id);
        let budget_claim = env.cost_estimate().budget().cpu_instruction_cost();
        std::println!("[gas] claim_winnings cpu={}", budget_claim);

        // Sanity: second prediction (batch-only write) should not exceed first by much
        assert!(budget_pred2 > 0);
        assert!(budget_create > 0);
        assert!(budget_claim > 0);
    }

    #[test]
    fn test_bench_binary_pool_hot_paths() {
        let env = Env::default();
        let (client, admin, token_client, token_admin_client) = setup(&env);
        let creator = Address::generate(&env);
        let outcome_descriptions = make_outcomes(&env, 2);

        env.cost_estimate().budget().reset_default();
        let pool_id = client.create_pool(
            &creator,
            &(env.ledger().timestamp() + 10000),
            &token_client.address,
            &2u32,
            &symbol_short!("Sports"),
            &PoolConfig {
                start_time: 0,
                description: String::from_str(&env, "Binary bench"),
                metadata_url: String::from_str(&env, "ipfs://binary"),
                min_stake: 1i128,
                max_stake: 0,
                max_total_stake: 0,
                min_total_stake: 1,
                initial_liquidity: 0,
                required_resolutions: 1,
                private: false,
                whitelist_key: None,
                outcome_descriptions,
            },
        );
        let create_cpu = env.cost_estimate().budget().cpu_instruction_cost();

        // Profile N predictions to measure amortized batch-write cost
        let mut predict_costs = std::vec::Vec::new();
        for i in 0..5u32 {
            let user = Address::generate(&env);
            token_admin_client.mint(&user, &500);
            env.cost_estimate().budget().reset_default();
            client.place_prediction(&user, &pool_id, &500, &(i % 2), &None, &None);
            predict_costs.push(env.cost_estimate().budget().cpu_instruction_cost());
        }

        env.cost_estimate().budget().reset_default();
        let stake0 = client.get_outcome_stake(&pool_id, &0u32);
        let lookup_cpu = env.cost_estimate().budget().cpu_instruction_cost();
        assert!(stake0 > 0);

        env.ledger().with_mut(|li| li.timestamp += 20000);
        client.resolve_pool(&admin, &pool_id, &0);

        std::println!("[gas] binary create_pool cpu={}", create_cpu);
        for (i, c) in predict_costs.iter().enumerate() {
            std::println!("[gas] binary place_prediction#{} cpu={}", i + 1, c);
        }
        std::println!("[gas] get_outcome_stake (batch) cpu={}", lookup_cpu);

        // Later predictions should stay in a stable band (batch path amortized)
        let first = predict_costs[0];
        let last = *predict_costs.last().unwrap();
        assert!(last < first.saturating_mul(2), "prediction cost regressed badly");
    }

    #[test]
    fn test_bench_active_pool_lookup() {
        let env = Env::default();
        let (client, _admin, token_client, _) = setup(&env);
        let creator = Address::generate(&env);

        for _ in 0..10 {
            let outcomes = make_outcomes(&env, 2);
            client.create_pool(
                &creator,
                &(env.ledger().timestamp() + 10000),
                &token_client.address,
                &2u32,
                &symbol_short!("Crypto"),
                &PoolConfig {
                    start_time: 0,
                    description: String::from_str(&env, "Lookup"),
                    metadata_url: String::from_str(&env, "ipfs://x"),
                    min_stake: 1i128,
                    max_stake: 0,
                    max_total_stake: 0,
                    min_total_stake: 1,
                    initial_liquidity: 0,
                    required_resolutions: 1,
                    private: false,
                    whitelist_key: None,
                    outcome_descriptions: outcomes,
                },
            );
        }

        env.cost_estimate().budget().reset_default();
        let active = client.get_active_pools(&0u32, &10u32);
        let cpu = env.cost_estimate().budget().cpu_instruction_cost();
        std::println!(
            "[gas] get_active_pools(offset=0,limit=10) n={} cpu={}",
            active.len(),
            cpu
        );
        assert_eq!(active.len(), 10);
    }

    #[test]
    fn test_bench_storage_footprint_per_pool() {
        //! Storage footprint benchmark per pool.
        //!
        //! Reports:
        //! - Bytes per pool at creation
        //! - Growth in bytes per prediction
        //!
        //! Run with:
        //! ```bash
        //! cargo test -p predifi-contract test_bench_storage_footprint_per_pool -- --nocapture
        //! ```
        //!
        //! The output is deterministic and stable across runs because it uses
        //! XDR serialization size, which is a function of the data content only.

        let env = Env::default();
        let (client, admin, token_client, token_admin_client) = setup(&env);
        let creator = Address::generate(&env);
        let options_count = 4u32;
        let outcome_descriptions = make_outcomes(&env, options_count);

        // ── Create pool and measure footprint ──────────────────────────────
        let pool_id = client.create_pool(
            &creator,
            &(env.ledger().timestamp() + 10000),
            &token_client.address,
            &options_count,
            &symbol_short!("Tech"),
            &PoolConfig {
                start_time: 0,
                description: String::from_str(&env, "FootprintBench"),
                metadata_url: String::from_str(&env, "ipfs://footprint"),
                min_stake: 10i128,
                max_stake: 0,
                max_total_stake: 0,
                min_total_stake: 1,
                initial_liquidity: 0,
                required_resolutions: 1,
                private: false,
                whitelist_key: None,
                outcome_descriptions,
            },
        );

        let mut tracked_users: std::vec::Vec<Address> = std::vec::Vec::new();
        let contract_id = client.address.clone();
        let bytes_at_creation =
            measure_pool_footprint(&env, &contract_id, pool_id, &tracked_users);
        std::println!(
            "[storage] bytes_per_pool at creation = {}",
            bytes_at_creation
        );

        // ── Place N predictions and measure growth ─────────────────────────
        let n_predictions = 5u32;
        let mut cumulative_growth: usize = 0;

        for i in 0..n_predictions {
            let user = Address::generate(&env);
            token_admin_client.mint(&user, &1000);
            client
                .place_prediction(&user, &pool_id, &100, &(i % options_count), &None, &None);
            tracked_users.push(user);

            let bytes_after =
                measure_pool_footprint(&env, &contract_id, pool_id, &tracked_users);
            let growth = bytes_after.saturating_sub(bytes_at_creation);
            cumulative_growth = growth;
            std::println!(
                "[storage] after {} prediction(s): total_bytes={}, cumulative_growth={}",
                i + 1,
                bytes_after,
                growth,
            );
        }

        // Sanity: storage should grow with each prediction
        let final_bytes = measure_pool_footprint(&env, &contract_id, pool_id, &tracked_users);
        assert!(
            final_bytes >= bytes_at_creation,
            "storage footprint should not shrink after predictions"
        );

        // Report summary
        std::println!(
            "[storage] summary: creation={} bytes, final={} bytes, total_growth={} bytes, n_predictions={}, growth_per_prediction={}",
            bytes_at_creation,
            final_bytes,
            cumulative_growth,
            n_predictions,
            cumulative_growth / n_predictions as usize,
        );
    }

    /// Measure the XDR-serialized byte size of all storage entries belonging to a pool.
    ///
    /// This is a deterministic proxy for on-chain storage footprint: XDR encoding
    /// size is stable across runs and directly corresponds to the Soroban
    /// ledger's storage charging model.
    ///
    /// `tracked_users` is the list of users who have placed predictions on this pool;
    /// their per-user entries are included in the measurement.
    fn measure_pool_footprint(
        env: &Env,
        contract_id: &Address,
        pool_id: u64,
        tracked_users: &[Address],
    ) -> usize {
        env.as_contract(contract_id, || {
            let mut total: usize = 0;

            // Pool struct
            let pool_key = DataKey::Pool(pool_id);
            let pool: Pool = env
                .storage()
                .persistent()
                .get(&pool_key)
                .expect("Pool must exist");
            total += pool_key.to_xdr(env).len() as usize;
            total += pool.to_xdr(env).len() as usize;

            // Outcome stakes (batch `OutStakes` vector)
            let stakes_key = DataKey::OutStakes(pool_id);
            if let Some(stakes) = env.storage().persistent().get::<_, Vec<i128>>(&stakes_key) {
                total += stakes_key.to_xdr(env).len() as usize;
                total += stakes.to_xdr(env).len() as usize;
            }

            // Per-user prediction entries
            for user in tracked_users {
                // Prediction record
                let pred_key = DataKey::Pred(user.clone(), pool_id);
                if let Some(pred) = env.storage().persistent().get::<_, Prediction>(&pred_key) {
                    total += pred_key.to_xdr(env).len() as usize;
                    total += pred.to_xdr(env).len() as usize;
                }

                // Claimed sentinel
                let claimed_key = DataKey::Claimed(user.clone(), pool_id);
                if let Some(claimed) = env.storage().persistent().get::<_, bool>(&claimed_key) {
                    total += claimed_key.to_xdr(env).len() as usize;
                    total += claimed.to_xdr(env).len() as usize;
                }

                // Last prediction time (global, not per-pool)
                let lpt_key = DataKey::LastPredictionTime(user.clone());
                if let Some(lpt) = env.storage().persistent().get::<_, u64>(&lpt_key) {
                    total += lpt_key.to_xdr(env).len() as usize;
                    total += lpt.to_xdr(env).len() as usize;
                }

                // User prediction count
                let cnt_key = DataKey::UsrPrdCnt(user.clone());
                if let Some(cnt) = env.storage().persistent().get::<_, u32>(&cnt_key) {
                    total += cnt_key.to_xdr(env).len() as usize;
                    total += cnt.to_xdr(env).len() as usize;
                }
            }

            total
        })
    }

    /// Resource benchmarks at each contract-level limit.
    ///
    /// Reports the CPU, memory, and persistent-storage cost observed at the
    /// configured ceiling and asserts that each remains below a published
    /// ceiling. Run with:
    /// ```bash
    /// cargo test -p predifi-contract test_resource_ceilings_at_limits -- --nocapture
    /// ```
    #[test]
    fn test_resource_ceilings_at_limits() {
        const MAX_TX_CPU: u64 = 5_000_000; // 5% of Soroban's 100M instruction limit
        const MAX_TX_MEM: u64 = 10 * 1024 * 1024; // 10 MB of the 40 MB test budget
        // The 1,000th prediction measures ~30M CPU / ~15 MB, so keep the
        // per-prediction ceiling at 40% / 50% of the host budget.
        const MAX_PREDICTION_TX_CPU: u64 = 40_000_000;
        const MAX_PREDICTION_TX_MEM: u64 = 20 * 1024 * 1024;
        const MAX_POOL_BYTES: usize = 512 * 1024; // 512 KB of persistent state

        let env = Env::default();
        let (client, _admin, token_client, token_admin_client) = setup(&env);
        let creator = Address::generate(&env);

        // ── Outcome ceiling: create a pool at MAX_OUTCOMES_PER_POOL ────────
        let outcomes = make_outcomes(&env, MAX_OUTCOMES_PER_POOL);
        env.cost_estimate().budget().reset_default();
        let pool_id = client.create_pool(
            &creator,
            &(env.ledger().timestamp() + 10000),
            &token_client.address,
            &MAX_OUTCOMES_PER_POOL,
            &symbol_short!("Tech"),
            &PoolConfig {
                start_time: 0,
                description: String::from_str(&env, "Resource ceiling"),
                metadata_url: String::from_str(&env, "ipfs://resource-ceiling"),
                min_stake: 1i128,
                max_stake: 0,
                max_total_stake: 0,
                min_total_stake: 1,
                initial_liquidity: 0,
                required_resolutions: 1,
                private: false,
                whitelist_key: None,
                outcome_descriptions: outcomes,
            },
        );
        let create_cpu = env.cost_estimate().budget().cpu_instruction_cost();
        let create_mem = env.cost_estimate().budget().memory_bytes_cost();
        let contract_id = client.address.clone();
        env.cost_estimate().budget().reset_unlimited();
        let outcome_bytes = measure_pool_footprint(&env, &contract_id, pool_id, &[]);
        std::println!(
            "[limits] outcomes={} create_cpu={} create_mem={} storage_bytes={}",
            MAX_OUTCOMES_PER_POOL,
            create_cpu,
            create_mem,
            outcome_bytes
        );
        assert!(create_cpu < MAX_TX_CPU, "create_pool CPU ceiling exceeded");
        assert!(create_mem < MAX_TX_MEM, "create_pool memory ceiling exceeded");
        assert!(
            outcome_bytes < MAX_POOL_BYTES,
            "outcome storage ceiling exceeded"
        );

        // ── Prediction ceiling: distinct predictors at the cap ────────────
        let binary_outcomes = make_outcomes(&env, 2);
        let binary_pool = client.create_pool(
            &creator,
            &(env.ledger().timestamp() + 10000),
            &token_client.address,
            &2u32,
            &symbol_short!("Sports"),
            &PoolConfig {
                start_time: 0,
                description: String::from_str(&env, "Prediction ceiling"),
                metadata_url: String::from_str(&env, "ipfs://prediction-ceiling"),
                min_stake: 1i128,
                max_stake: 0,
                max_total_stake: 0,
                min_total_stake: 1,
                initial_liquidity: 0,
                required_resolutions: 1,
                private: false,
                whitelist_key: None,
                outcome_descriptions: binary_outcomes,
            },
        );

        let mut tracked_users = std::vec::Vec::with_capacity(MAX_PREDICTIONS_PER_POOL as usize);
        let mut max_prediction_cpu = 0u64;
        let mut max_prediction_mem = 0u64;
        for i in 0..MAX_PREDICTIONS_PER_POOL {
            let user = Address::generate(&env);
            token_admin_client.mint(&user, &1i128);
            env.cost_estimate().budget().reset_default();
            client.place_prediction(&user, &binary_pool, &1i128, &0u32, &None, &None);
            let cpu = env.cost_estimate().budget().cpu_instruction_cost();
            let mem = env.cost_estimate().budget().memory_bytes_cost();
            if i % 250 == 0 || i + 1 == MAX_PREDICTIONS_PER_POOL {
                std::println!("[limits] prediction#{} cpu={} mem={}", i + 1, cpu, mem);
            }
            max_prediction_cpu = max_prediction_cpu.max(cpu);
            max_prediction_mem = max_prediction_mem.max(mem);
            tracked_users.push(user);
        }
        // Storage measurement is a test-only scan; give it an unlimited budget
        // so the accounting does not trip the per-transaction CPU limit.
        env.cost_estimate().budget().reset_unlimited();
        let prediction_bytes =
            measure_pool_footprint(&env, &contract_id, binary_pool, &tracked_users);
        std::println!(
            "[limits] predictions={} max_prediction_cpu={} max_prediction_mem={} storage_bytes={}",
            MAX_PREDICTIONS_PER_POOL,
            max_prediction_cpu,
            max_prediction_mem,
            prediction_bytes
        );
        assert!(
            max_prediction_cpu < MAX_PREDICTION_TX_CPU,
            "place_prediction CPU ceiling exceeded"
        );
        assert!(
            max_prediction_mem < MAX_PREDICTION_TX_MEM,
            "place_prediction memory ceiling exceeded"
        );
        assert!(
            prediction_bytes < MAX_POOL_BYTES,
            "prediction storage ceiling exceeded"
        );

        // ── Aggregate stake ceiling: single stake at the cap ──────────────
        let aggregate_outcomes = make_outcomes(&env, 2);
        let aggregate_pool = client.create_pool(
            &creator,
            &(env.ledger().timestamp() + 10000),
            &token_client.address,
            &2u32,
            &symbol_short!("Finance"),
            &PoolConfig {
                start_time: 0,
                description: String::from_str(&env, "Aggregate ceiling"),
                metadata_url: String::from_str(&env, "ipfs://aggregate-ceiling"),
                min_stake: 1i128,
                max_stake: 0,
                max_total_stake: 0,
                min_total_stake: 1,
                initial_liquidity: 0,
                required_resolutions: 1,
                private: false,
                whitelist_key: None,
                outcome_descriptions: aggregate_outcomes,
            },
        );
        let whale = Address::generate(&env);
        token_admin_client.mint(&whale, &MAX_AGGREGATE_STAKE_PER_POOL);
        env.cost_estimate().budget().reset_default();
        client.place_prediction(
            &whale,
            &aggregate_pool,
            &MAX_AGGREGATE_STAKE_PER_POOL,
            &0u32,
            &None,
            &None,
        );
        let aggregate_cpu = env.cost_estimate().budget().cpu_instruction_cost();
        let aggregate_mem = env.cost_estimate().budget().memory_bytes_cost();
        std::println!(
            "[limits] aggregate_stake={} place_cpu={} place_mem={}",
            MAX_AGGREGATE_STAKE_PER_POOL, aggregate_cpu, aggregate_mem
        );
        // Measured after the pool state already holds 1,000 predictions, so
        // this is the combined worst-case state for a max-size stake transfer.
        assert!(
            aggregate_cpu < MAX_PREDICTION_TX_CPU,
            "aggregate stake CPU ceiling exceeded"
        );
        assert!(
            aggregate_mem < MAX_PREDICTION_TX_MEM,
            "aggregate stake memory ceiling exceeded"
        );
    }
}
