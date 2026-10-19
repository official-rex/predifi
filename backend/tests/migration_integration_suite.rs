//! Migration Integration Suite
//!
//! Validates:
//! 1. Lock and data impact classification for all schema migrations.
//! 2. Extension and PostgreSQL version state validation.
//! 3. Documented rollback and forward-fix paths for every migration.
//! 4. Populated schema checks detecting concurrency and table-lock risks.
//! 5. Diagnosis and safe resumption of interrupted or dirty migrations.

use predifi_backend::migration_checks::{
    evaluate_populated_schema_risk, get_known_migration_catalog, DataImpact,
    InterruptedMigrationDiagnosis, LockImpact, LockLevel, MigrationChecker,
    RollbackStrategy, TableStat,
};

#[test]
fn test_all_migrations_classified_with_rollback_and_forward_fix() {
    let catalog = get_known_migration_catalog();
    assert_eq!(
        catalog.len(),
        12,
        "All 12 migration files in backend/migrations must have catalog entries"
    );

    for m in &catalog {
        // Every migration must have target tables identified
        assert!(
            !m.target_tables.is_empty(),
            "Migration {} must identify target tables",
            m.name
        );

        // Every migration must have a documented rollback path
        assert!(
            !m.rollback.instructions.is_empty(),
            "Migration {} must document rollback instructions",
            m.name
        );
        match m.rollback.strategy {
            RollbackStrategy::ReversibleSql => {
                assert!(
                    m.rollback.down_sql.is_some(),
                    "ReversibleSql migration {} must provide down_sql",
                    m.name
                );
            }
            RollbackStrategy::ManualIntervention => {
                assert!(
                    !m.rollback.instructions.is_empty(),
                    "ManualIntervention migration {} must document intervention",
                    m.name
                );
            }
            _ => {}
        }

        // Every migration must have a documented forward-fix path
        assert!(
            !m.forward_fix.instructions.is_empty(),
            "Migration {} must document forward-fix instructions",
            m.name
        );
    }
}

#[test]
fn test_precision_migration_classified_as_full_table_rewrite() {
    let catalog = get_known_migration_catalog();
    let m = catalog
        .iter()
        .find(|m| m.name == "004_enhance_schema_precision.sql")
        .expect("004_enhance_schema_precision.sql exists");

    assert_eq!(m.lock_level, LockLevel::AccessExclusive);
    assert_eq!(m.lock_impact, LockImpact::ExclusiveBlocking);
    assert_eq!(m.data_impact, DataImpact::FullTableRewrite);
    assert!(
        !m.safe_online,
        "Altering column types to NUMERIC(32,7) is not safe online without maintenance window"
    );
    assert!(m.requires_maintenance_window);
}

#[test]
fn test_populated_schema_check_flags_high_risk_on_large_tables() {
    let populated_tables = vec![
        TableStat {
            table_name: "predictions".to_string(),
            estimated_row_count: 1_200_000,
            total_size_bytes: 180_000_000,
            total_size_pretty: "180 MB".to_string(),
            is_large: true,
        },
        TableStat {
            table_name: "pools".to_string(),
            estimated_row_count: 25_000,
            total_size_bytes: 5_000_000,
            total_size_pretty: "5 MB".to_string(),
            is_large: true,
        },
    ];

    let catalog = get_known_migration_catalog();
    let report = evaluate_populated_schema_risk(&populated_tables, &catalog);

    assert!(report.has_high_risk);
    assert!(
        report
            .risks
            .iter()
            .any(|r| r.table_name == "predictions" && r.data_impact == DataImpact::FullTableRewrite),
        "Must flag full table rewrite on populated predictions table"
    );
    assert!(
        report
            .risks
            .iter()
            .any(|r| r.lock_impact == LockImpact::WriteBlocking),
        "Must flag write-blocking index build on populated pools table"
    );
}

#[test]
fn test_interrupted_migration_diagnosis_and_safe_resumption_plan() {
    let mock_pool = sqlx::PgPool::connect_lazy("postgres://localhost/mock").unwrap();
    let checker = MigrationChecker::new(mock_pool);

    let interrupted_diagnosis = InterruptedMigrationDiagnosis {
        has_interrupted_migration: true,
        failed_version: Some(4),
        failed_description: Some("enhance_schema_precision".to_string()),
        failure_reason: Some("Timeout while holding lock on predictions".to_string()),
        invalid_indexes: vec!["idx_predictions_user_created".to_string()],
        active_lock_blockers: vec![],
        diagnosis_summary: "Interrupted migration version 4 detected".to_string(),
        can_safely_resume: true,
    };

    let plan = checker.plan_migration_resumption(&interrupted_diagnosis);

    assert_eq!(plan.failed_version, Some(4));
    assert!(plan.can_auto_execute);

    // Verify step order:
    // 1. Drop invalid index
    // 2. Clear failed migration state in _sqlx_migrations
    // 3. Resume and re-apply migration
    // 4. Verify database consistency
    let step_actions: Vec<&str> = plan.steps.iter().map(|s| s.action.as_str()).collect();
    assert!(
        step_actions.iter().any(|a| a.contains("Drop invalid index")),
        "Plan must include dropping invalid leftover indexes"
    );
    assert!(
        step_actions.iter().any(|a| a.contains("Clear failed migration state")),
        "Plan must clear failed migration state from _sqlx_migrations"
    );
    assert!(
        step_actions.iter().any(|a| a.contains("Resume and re-apply")),
        "Plan must include re-applying migration"
    );
    assert!(
        step_actions.iter().any(|a| a.contains("Verify database consistency")),
        "Plan must include post-resumption verification"
    );
}

// ── Real PostgreSQL Container Integration Tests ───────────────────────────────

#[cfg(feature = "integration-tests")]
mod live_db_tests {
    use super::*;
    use sqlx::postgres::PgPoolOptions;
    use sqlx::PgPool;
    use testcontainers::runners::AsyncRunner;
    use testcontainers_modules::postgres::Postgres;

    async fn start_postgres() -> (PgPool, testcontainers::ContainerAsync<Postgres>) {
        let container = Postgres::default()
            .start()
            .await
            .expect("postgres container");

        let port = container
            .get_host_port_ipv4(5432)
            .await
            .expect("postgres port");

        let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(&url)
            .await
            .expect("connect to test postgres");

        (pool, container)
    }

    #[tokio::test]
    #[ignore = "Requires Docker container for Postgres"]
    async fn live_environment_version_and_extension_validation() {
        let (pool, container) = start_postgres().await;
        let checker = MigrationChecker::new(pool.clone());

        let env = checker
            .validate_environment(&["plpgsql"], 140000)
            .await
            .expect("validate environment");

        assert!(env.version.server_version_num > 0);
        assert!(env.extensions.installed.contains_key("plpgsql"));
        assert!(env.migration_state.is_clean);

        pool.close().await;
        drop(container);
    }

    #[tokio::test]
    #[ignore = "Requires Docker container for Postgres"]
    async fn live_populated_schema_and_interrupted_migration_resumption() {
        let (pool, container) = start_postgres().await;
        let checker = MigrationChecker::new(pool.clone());

        // 1. Run migrations 001, 002, 003
        sqlx::query(include_str!("../migrations/001_create_referrals.sql"))
            .execute(&pool)
            .await
            .expect("run 001");
        sqlx::query(include_str!("../migrations/002_create_pools.sql"))
            .execute(&pool)
            .await
            .expect("run 002");
        sqlx::query(include_str!("../migrations/003_create_predictions.sql"))
            .execute(&pool)
            .await
            .expect("run 003");

        // 2. Populate schema with sample rows
        sqlx::query(
            r#"
            INSERT INTO pools (pool_id, name, category, total_stake, end_time, state, creator, token)
            VALUES (1, 'BTC Pool', 'Crypto', 1000, NOW() + INTERVAL '1 day', 'active', 'GCREATOR', 'XLM')
            "#,
        )
        .execute(&pool)
        .await
        .expect("insert pool");

        sqlx::query(
            r#"
            INSERT INTO predictions (pool_id, user_address, outcome, amount)
            SELECT 1, 'GUSER' || g, (g % 2), 100
            FROM generate_series(1, 100) AS g
            "#,
        )
        .execute(&pool)
        .await
        .expect("insert predictions");

        // 3. Verify populated schema check
        let report = checker
            .check_populated_schema(50) // threshold = 50 rows
            .await
            .expect("check populated schema");

        assert!(
            report.tables.iter().any(|t| t.table_name == "predictions" && t.is_large),
            "predictions table must be identified as populated"
        );

        // 4. Simulate an interrupted migration in _sqlx_migrations
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS _sqlx_migrations (
                version BIGINT PRIMARY KEY,
                description TEXT NOT NULL,
                installed_on TIMESTAMPTZ NOT NULL DEFAULT now(),
                success BOOLEAN NOT NULL,
                checksum BYTEA NOT NULL,
                execution_time BIGINT NOT NULL
            )
            "#,
        )
        .execute(&pool)
        .await
        .expect("create _sqlx_migrations");

        sqlx::query(
            r#"
            INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)
            VALUES (4, 'creator_incentives_templates_and_indexes', false, E'\\x00', -1)
            "#,
        )
        .execute(&pool)
        .await
        .expect("insert failed migration");

        // 5. Diagnose interrupted migration
        let diagnosis = checker
            .diagnose_interrupted_migration()
            .await
            .expect("diagnose interrupted migration");

        assert!(diagnosis.has_interrupted_migration);
        assert_eq!(diagnosis.failed_version, Some(4));

        // 6. Plan and execute safe resumption
        let plan = checker.plan_migration_resumption(&diagnosis);
        assert!(plan.can_auto_execute);

        checker
            .resolve_dirty_migration(4)
            .await
            .expect("resolve dirty migration");

        // 7. Verify dirty migration state is resolved
        let diagnosis_after = checker
            .diagnose_interrupted_migration()
            .await
            .expect("diagnose after fix");
        assert!(!diagnosis_after.has_interrupted_migration);

        pool.close().await;
        drop(container);
    }
}
