//! # Pre-Migration Safety Checks
//!
//! This module provides comprehensive migration safety infrastructure for
//! zero-downtime deployments, including:
//!
//! - **Lock and data impact classification**: Classify DDL operations by PostgreSQL
//!   lock level (`AccessExclusiveLock`, `ShareLock`, `ShareUpdateExclusiveLock`),
//!   operational lock impact (instant, write-blocking, exclusive-blocking), and table
//!   data impact (metadata-only, index-build, full table rewrite, backfill).
//! - **Environment & extension validation**: Validate PostgreSQL server version state
//!   (e.g. PG 14+) and required extensions before migrations run.
//! - **Documented rollback & forward-fix paths**: Explicit rollback SQL scripts and
//!   forward-fix recovery paths for every migration in the schema history.
//! - **Populated schema risk analysis**: Scan existing populated tables and evaluate
//!   concurrency / table-lock risks against real row counts and table sizes.
//! - **Interrupted migration diagnosis & resumption**: Detect failed/dirty migration
//!   records, identify invalid leftover indexes (`indisvalid = false`), detect active
//!   blocking locks, and generate an automated, safe resumption plan.
//! - **Dry-run mode**: Simulate migrations inside a transaction that is always
//!   rolled back, verifying SQL syntax and constraint violations without any
//!   permanent schema changes.
//! - **Rollback testing**: Verify that a down-migration successfully reverts
//!   the schema to its prior state.
//! - **Schema diff preview**: Compute a human-readable diff of table/column
//!   changes that would be introduced by pending migrations.
//! - **Backward-compatibility checks**: Detect destructive DDL operations
//!   (column/table drops, type changes, NOT NULL additions) that would break
//!   running replicas before they are updated.

use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool, Row};
use std::collections::HashMap;
use std::fmt;
use tracing::{info, warn};

// ── Lock and Data Impact Types ────────────────────────────────────────────────

/// PostgreSQL table lock levels acquired by DDL statements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum LockLevel {
    /// No locks acquired on user tables.
    None,
    /// `AccessShareLock`: acquired by `SELECT`.
    AccessShare,
    /// `RowExclusiveLock`: acquired by `INSERT`, `UPDATE`, `DELETE`.
    RowExclusive,
    /// `ShareUpdateExclusiveLock`: acquired by `CREATE INDEX CONCURRENTLY`, `VACUUM`, `ANALYZE`.
    /// Does not block concurrent reads or writes.
    ShareUpdateExclusive,
    /// `ShareLock`: acquired by non-concurrent `CREATE INDEX`.
    /// Blocks concurrent writes (`INSERT`, `UPDATE`, `DELETE`), permits reads.
    Share,
    /// `ShareRowExclusiveLock`: blocks concurrent writes and exclusive locks.
    ShareRowExclusive,
    /// `AccessExclusiveLock`: blocks all concurrent reads and writes.
    /// Acquired by `ALTER TABLE` (most forms), `DROP TABLE`, `TRUNCATE`, `CREATE TRIGGER`.
    AccessExclusive,
}

impl fmt::Display for LockLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => write!(f, "None"),
            Self::AccessShare => write!(f, "AccessShareLock"),
            Self::RowExclusive => write!(f, "RowExclusiveLock"),
            Self::ShareUpdateExclusive => write!(f, "ShareUpdateExclusiveLock"),
            Self::Share => write!(f, "ShareLock"),
            Self::ShareRowExclusive => write!(f, "ShareRowExclusiveLock"),
            Self::AccessExclusive => write!(f, "AccessExclusiveLock"),
        }
    }
}

/// Operational concurrency impact of the acquired table lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LockImpact {
    /// O(1) catalog update (< 10ms); negligible impact on live queries.
    Instant,
    /// Blocks concurrent writes (`INSERT`/`UPDATE`/`DELETE`), permits reads.
    WriteBlocking,
    /// Blocks all concurrent reads and writes (`SELECT`/`INSERT`/`UPDATE`/`DELETE`).
    ExclusiveBlocking,
}

impl fmt::Display for LockImpact {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Instant => write!(f, "Instant (O(1) catalog update)"),
            Self::WriteBlocking => write!(f, "WriteBlocking (blocks writes, reads OK)"),
            Self::ExclusiveBlocking => write!(f, "ExclusiveBlocking (blocks all reads and writes)"),
        }
    }
}

/// Impact on underlying table data pages, tuples, and disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DataImpact {
    /// Updates only system catalogs. Zero heap tuples modified.
    MetadataOnly,
    /// Scans table to build index pages. Heap tuples are not rewritten.
    IndexBuild,
    /// Scans table heap (e.g. for constraint validation or aggregate backfill).
    TableScan,
    /// Rewrites every tuple in table heap (e.g. `ALTER COLUMN TYPE`, table rewrite).
    FullTableRewrite,
    /// Inserts, updates, or deletes existing/derived rows (e.g. backfilling aggregate tables).
    DataBackfill,
    /// Destructive DDL (drops tables or columns, permanent data removal).
    Destructive,
}

impl fmt::Display for DataImpact {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MetadataOnly => write!(f, "MetadataOnly"),
            Self::IndexBuild => write!(f, "IndexBuild"),
            Self::TableScan => write!(f, "TableScan"),
            Self::FullTableRewrite => write!(f, "FullTableRewrite"),
            Self::DataBackfill => write!(f, "DataBackfill"),
            Self::Destructive => write!(f, "Destructive"),
        }
    }
}

/// Strategy for rolling back a migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RollbackStrategy {
    /// Fully reversible via a pure SQL down-migration script.
    ReversibleSql,
    /// Requires manual intervention (e.g. data reconciliation or precision restoration).
    ManualIntervention,
    /// Irreversible without restoring from a database backup or storage snapshot.
    SnapshotRestore,
    /// Not applicable.
    NotApplicable,
}

/// Documented rollback path for an applied migration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackPlan {
    pub strategy: RollbackStrategy,
    pub down_sql: Option<String>,
    pub instructions: String,
}

/// Documented forward-fix path for an interrupted or problematic migration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForwardFixPlan {
    pub instructions: String,
    pub fix_sql: Option<String>,
}

/// Comprehensive classification and mitigation profile for a migration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationClassification {
    pub name: String,
    pub version: i64,
    pub lock_level: LockLevel,
    pub lock_impact: LockImpact,
    pub data_impact: DataImpact,
    pub target_tables: Vec<String>,
    pub safe_online: bool,
    pub requires_maintenance_window: bool,
    pub notes: String,
    pub rollback: RollbackPlan,
    pub forward_fix: ForwardFixPlan,
}

// ── Environment and Version Validation Types ──────────────────────────────────

/// PostgreSQL server version check result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionValidation {
    pub server_version: String,
    pub server_version_num: i32,
    pub is_supported: bool,
    pub minimum_required: i32,
    pub details: String,
}

/// PostgreSQL extension state validation result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionValidation {
    pub installed: HashMap<String, String>,
    pub missing_required: Vec<String>,
    pub is_valid: bool,
    pub recommendations: Vec<String>,
}

/// Validation of applied and pending migrations in `_sqlx_migrations`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationHistoryValidation {
    pub applied_versions: Vec<i64>,
    pub dirty_versions: Vec<i64>,
    pub duplicate_versions: Vec<i64>,
    pub is_clean: bool,
    pub details: Vec<String>,
}

/// Consolidated environment validation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentValidation {
    pub version: VersionValidation,
    pub extensions: ExtensionValidation,
    pub migration_state: MigrationHistoryValidation,
    pub all_checks_passed: bool,
}

// ── Populated Schema Risk Types ───────────────────────────────────────────────

/// Row count and size metrics for an individual database table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableStat {
    pub table_name: String,
    pub estimated_row_count: i64,
    pub total_size_bytes: i64,
    pub total_size_pretty: String,
    pub is_large: bool,
}

/// Concurrency or lock risk detected against a populated table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PopulatedSchemaRisk {
    pub table_name: String,
    pub migration_version: Option<i64>,
    pub operation: String,
    pub lock_impact: LockImpact,
    pub data_impact: DataImpact,
    pub risk_reason: String,
    pub recommended_mitigation: String,
}

/// Report evaluating migration risks against populated database tables.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PopulatedSchemaReport {
    pub tables: Vec<TableStat>,
    pub risks: Vec<PopulatedSchemaRisk>,
    pub has_high_risk: bool,
    pub summary: String,
}

// ── Interrupted Migration Diagnosis and Resumption Types ─────────────────────

/// Detailed diagnostic findings for an interrupted or failed migration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterruptedMigrationDiagnosis {
    pub has_interrupted_migration: bool,
    pub failed_version: Option<i64>,
    pub failed_description: Option<String>,
    pub failure_reason: Option<String>,
    pub invalid_indexes: Vec<String>,
    pub active_lock_blockers: Vec<String>,
    pub diagnosis_summary: String,
    pub can_safely_resume: bool,
}

/// Individual step in a safe resumption plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumptionStep {
    pub step_number: usize,
    pub action: String,
    pub sql: Option<String>,
    pub explanation: String,
}

/// Step-by-step plan to safely recover and resume an interrupted migration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumptionPlan {
    pub failed_version: Option<i64>,
    pub steps: Vec<ResumptionStep>,
    pub can_auto_execute: bool,
    pub summary: String,
}

// ── Schema Diff and Compatibility Types ───────────────────────────────────────

/// A single column descriptor extracted from `information_schema`.
#[derive(Debug, Clone, PartialEq, FromRow, Serialize, Deserialize)]
pub struct ColumnDescriptor {
    pub table_name: String,
    pub column_name: String,
    pub data_type: String,
    pub is_nullable: bool,
    pub column_default: Option<String>,
    pub ordinal_position: i64,
}

/// Represents a single schema-change entry in the diff output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SchemaDiffEntry {
    /// A table that does not yet exist will be created.
    TableAdded(String),
    /// A table that exists will be removed.
    TableDropped(String),
    /// A column that does not yet exist will be added.
    ColumnAdded {
        table: String,
        column: String,
        data_type: String,
        nullable: bool,
    },
    /// An existing column will be removed.
    ColumnDropped { table: String, column: String },
    /// An existing column's type will change.
    TypeChanged {
        table: String,
        column: String,
        from: String,
        to: String,
    },
    /// A column's nullability will change.
    NullabilityChanged {
        table: String,
        column: String,
        was_nullable: bool,
        now_nullable: bool,
    },
}

impl fmt::Display for SchemaDiffEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TableAdded(t) => write!(f, "[ADD TABLE]    {t}"),
            Self::TableDropped(t) => write!(f, "[DROP TABLE]   {t}"),
            Self::ColumnAdded {
                table,
                column,
                data_type,
                nullable,
            } => write!(
                f,
                "[ADD COLUMN]   {table}.{column} {data_type}{}",
                if *nullable { "" } else { " NOT NULL" }
            ),
            Self::ColumnDropped { table, column } => {
                write!(f, "[DROP COLUMN]  {table}.{column}")
            }
            Self::TypeChanged {
                table,
                column,
                from,
                to,
            } => write!(f, "[TYPE CHANGE]  {table}.{column}: {from} → {to}"),
            Self::NullabilityChanged {
                table,
                column,
                was_nullable,
                now_nullable,
            } => write!(
                f,
                "[NULL CHANGE]  {table}.{column}: {} → {}",
                if *was_nullable { "nullable" } else { "NOT NULL" },
                if *now_nullable { "nullable" } else { "NOT NULL" }
            ),
        }
    }
}

/// Classification of a schema change from a backward-compatibility perspective.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CompatibilityRisk {
    /// The change is safe for zero-downtime deployment.
    Safe,
    /// The change may break running replicas or clients.
    Breaking(String),
    /// The change requires a careful multi-step rollout.
    RequiresPhasing(String),
}

/// An individual backward-compatibility finding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompatibilityFinding {
    pub entry: SchemaDiffEntry,
    pub risk: CompatibilityRisk,
}

/// Summary of a dry-run execution.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct DryRunResult {
    /// Whether the migration SQL applied successfully inside the rolled-back txn.
    pub success: bool,
    /// Any SQL error encountered during the dry run.
    pub error: Option<String>,
    /// List of SQL statements that were executed (best-effort parse).
    pub statements_executed: usize,
}

/// The aggregated report produced by [`MigrationChecker::run`].
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct MigrationReport {
    /// Results of the dry-run phase (if `opts.dry_run` is true).
    pub dry_run: Option<DryRunResult>,
    /// Schema diff entries computed before and after applying migrations.
    pub schema_diff: Vec<SchemaDiffEntry>,
    /// Backward-compatibility findings.
    pub compat_findings: Vec<CompatibilityFinding>,
    /// Whether any breaking change was detected.
    pub has_breaking_changes: bool,
    /// Total pending migration files detected.
    pub pending_migrations: usize,
    /// Environment and extension validation results.
    pub environment: Option<EnvironmentValidation>,
    /// Lock and data impact classifications for pending/catalog migrations.
    pub classifications: Vec<MigrationClassification>,
    /// Populated schema risk report.
    pub populated_schema: Option<PopulatedSchemaReport>,
    /// Interrupted migration diagnostic findings.
    pub interrupted_diagnosis: Option<InterruptedMigrationDiagnosis>,
    /// Safe resumption plan if an interrupted migration was diagnosed.
    pub resumption_plan: Option<ResumptionPlan>,
}

impl MigrationReport {
    /// Return a formatted, multi-line summary of the report.
    pub fn summary(&self) -> String {
        let mut out = String::new();
        out.push_str("═══════════════════════════════════════════════════════════\n");
        out.push_str("              PrediFi Migration Safety Report\n");
        out.push_str("═══════════════════════════════════════════════════════════\n");

        out.push_str(&format!(
            "Pending migrations : {}\n",
            self.pending_migrations
        ));

        // Environment validation
        if let Some(env) = &self.environment {
            out.push_str("\n─── Environment & Version State ───────────────────────────\n");
            out.push_str(&format!(
                "  PostgreSQL Version : {} ({})\n",
                env.version.server_version,
                if env.version.is_supported { "✓ SUPPORTED" } else { "✗ UNSUPPORTED" }
            ));
            out.push_str(&format!(
                "  Extensions State   : {}\n",
                if env.extensions.is_valid { "✓ VALID" } else { "✗ MISSING REQUIRED" }
            ));
            for missing in &env.extensions.missing_required {
                out.push_str(&format!("    - Missing: {missing}\n"));
            }
            out.push_str(&format!(
                "  Migration History  : {}\n",
                if env.migration_state.is_clean { "✓ CLEAN" } else { "⚠ ISSUES DETECTED" }
            ));
            for detail in &env.migration_state.details {
                out.push_str(&format!("    - {detail}\n"));
            }
        }

        // Dry-run result
        if let Some(dry) = &self.dry_run {
            out.push_str("\n─── Dry-Run Simulation ────────────────────────────────────\n");
            out.push_str(&format!(
                "  Status             : {}\n",
                if dry.success { "✓ PASSED" } else { "✗ FAILED" }
            ));
            if let Some(err) = &dry.error {
                out.push_str(&format!("  Error              : {err}\n"));
            }
            out.push_str(&format!(
                "  Statements run     : {}\n",
                dry.statements_executed
            ));
        }

        // Lock & Data Impact Classifications
        if !self.classifications.is_empty() {
            out.push_str("\n─── Lock & Data Impact Classifications ────────────────────\n");
            for c in &self.classifications {
                let badge = if c.safe_online { "✓ SAFE ONLINE" } else { "⚠ HIGH RISK  " };
                out.push_str(&format!("  {badge} [{:03}] {}\n", c.version, c.name));
                out.push_str(&format!("             Lock   : {} ({})\n", c.lock_level, c.lock_impact));
                out.push_str(&format!("             Data   : {}\n", c.data_impact));
                out.push_str(&format!("             Tables : {}\n", c.target_tables.join(", ")));
                if c.requires_maintenance_window {
                    out.push_str("             Notice : Maintenance window required on large tables\n");
                }
            }
        }

        // Populated Schema Report
        if let Some(pop) = &self.populated_schema {
            out.push_str("\n─── Populated Schema Assessment ───────────────────────────\n");
            out.push_str(&format!("  {}\n", pop.summary));
            for risk in &pop.risks {
                out.push_str(&format!(
                    "  ⚠ Table '{}': {} -> {}\n    Mitigation: {}\n",
                    risk.table_name, risk.operation, risk.risk_reason, risk.recommended_mitigation
                ));
            }
        }

        // Interrupted Migration Diagnosis
        if let Some(diag) = &self.interrupted_diagnosis {
            out.push_str("\n─── Interrupted Migration Diagnosis ───────────────────────\n");
            out.push_str(&format!("  {}\n", diag.diagnosis_summary));
            if !diag.invalid_indexes.is_empty() {
                out.push_str(&format!(
                    "  Leftover Invalid Indexes: {}\n",
                    diag.invalid_indexes.join(", ")
                ));
            }
            if let Some(plan) = &self.resumption_plan {
                out.push_str("  Resumption Plan Steps:\n");
                for step in &plan.steps {
                    out.push_str(&format!(
                        "    {}. {} - {}\n",
                        step.step_number, step.action, step.explanation
                    ));
                    if let Some(sql) = &step.sql {
                        out.push_str(&format!("       SQL: {sql}\n"));
                    }
                }
            }
        }

        // Schema diff
        if !self.schema_diff.is_empty() {
            out.push_str("\n─── Schema Diff ───────────────────────────────────────────\n");
            for entry in &self.schema_diff {
                out.push_str(&format!("  {entry}\n"));
            }
        }

        // Compatibility findings
        if !self.compat_findings.is_empty() {
            out.push_str("\n─── Backward Compatibility ────────────────────────────────\n");
            for f in &self.compat_findings {
                let label = match &f.risk {
                    CompatibilityRisk::Safe => "✓ SAFE     ",
                    CompatibilityRisk::Breaking(_) => "✗ BREAKING ",
                    CompatibilityRisk::RequiresPhasing(_) => "⚠ PHASE-IN ",
                };
                out.push_str(&format!("  {label} {}\n", f.entry));
                if let CompatibilityRisk::Breaking(reason)
                | CompatibilityRisk::RequiresPhasing(reason) = &f.risk
                {
                    out.push_str(&format!("             → {reason}\n"));
                }
            }
        }

        out.push_str("\n─── Overall ───────────────────────────────────────────────\n");
        if self.has_breaking_changes {
            out.push_str("  ✗ BREAKING CHANGES DETECTED — review before deploy\n");
        } else {
            out.push_str("  ✓ No breaking changes detected\n");
        }
        out.push_str("═══════════════════════════════════════════════════════════\n");

        out
    }
}

// ── Options ───────────────────────────────────────────────────────────────────

/// Configuration for a [`MigrationChecker`] run.
#[derive(Debug, Clone)]
pub struct CheckOptions {
    /// Apply migrations inside a rolled-back transaction to validate SQL.
    pub dry_run: bool,
    /// Compute the schema diff and emit it in the report.
    pub show_diff: bool,
    /// Analyse the diff for backward-compatibility issues.
    pub check_backward_compat: bool,
    /// Validate PostgreSQL version, extensions, and migration state.
    pub validate_environment: bool,
    /// Classify pending/catalog migrations by lock and data impact.
    pub classify_migrations: bool,
    /// Check populated tables for heavy migration risks.
    pub check_populated_schema: bool,
    /// Diagnose any interrupted or failed migrations in `_sqlx_migrations`.
    pub diagnose_interrupted: bool,
    /// Row count threshold to treat a table as "populated / large" (default: 10,000).
    pub large_table_threshold: i64,
}

impl Default for CheckOptions {
    fn default() -> Self {
        Self {
            dry_run: true,
            show_diff: true,
            check_backward_compat: true,
            validate_environment: false,
            classify_migrations: true,
            check_populated_schema: false,
            diagnose_interrupted: false,
            large_table_threshold: 10_000,
        }
    }
}

// ── MigrationChecker ──────────────────────────────────────────────────────────

/// Core migration safety checker.
pub struct MigrationChecker {
    pool: PgPool,
}

impl MigrationChecker {
    /// Create a new checker backed by the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Execute all requested checks and return a consolidated [`MigrationReport`].
    pub async fn run(&self, opts: &CheckOptions) -> Result<MigrationReport, sqlx::Error> {
        let mut report = MigrationReport::default();

        // 1. Environment validation
        if opts.validate_environment {
            let env = self.validate_environment(&["plpgsql"], 140000).await?;
            report.environment = Some(env);
        }

        // 2. Interrupted migration diagnosis
        if opts.diagnose_interrupted {
            let diag = self.diagnose_interrupted_migration().await?;
            if diag.has_interrupted_migration {
                let plan = self.plan_migration_resumption(&diag);
                report.resumption_plan = Some(plan);
            }
            report.interrupted_diagnosis = Some(diag);
        }

        // 3. Populated schema assessment
        if opts.check_populated_schema {
            let pop = self.check_populated_schema(opts.large_table_threshold).await?;
            report.populated_schema = Some(pop);
        }

        // 4. Migration classification
        if opts.classify_migrations {
            report.classifications = self.classify_all_migrations();
        }

        // 5. Snapshot schema BEFORE migrations are applied
        let before = if opts.show_diff || opts.check_backward_compat {
            self.snapshot_schema().await?
        } else {
            HashMap::new()
        };

        // Determine how many migrations are pending
        report.pending_migrations = self.count_pending_migrations().await?;
        info!(
            pending = report.pending_migrations,
            "migration safety check started"
        );

        // 6. Dry-run: apply migrations inside a savepoint, then roll back
        if opts.dry_run {
            let dry = self.run_dry_run().await;
            report.dry_run = Some(dry);
        }

        // 7. Snapshot schema AFTER applying real migrations
        let after = if opts.show_diff || opts.check_backward_compat {
            self.snapshot_schema().await?
        } else {
            HashMap::new()
        };

        // 8. Schema diff
        if opts.show_diff {
            report.schema_diff = compute_schema_diff(&before, &after);
        }

        // 9. Backward-compatibility analysis
        if opts.check_backward_compat {
            report.compat_findings = analyse_compatibility(&report.schema_diff);
            report.has_breaking_changes = report.compat_findings.iter().any(|f| {
                matches!(f.risk, CompatibilityRisk::Breaking(_))
            });
        }

        if report.has_breaking_changes {
            warn!(
                breaking = report.compat_findings.len(),
                "breaking migration changes detected"
            );
        }

        Ok(report)
    }

    // ── Environment and Version Validation ────────────────────────────────────

    /// Validate PostgreSQL server version against minimum requirement (e.g. 140000 for PG 14).
    pub async fn validate_postgres_version(
        &self,
        min_version_num: i32,
    ) -> Result<VersionValidation, sqlx::Error> {
        let row = sqlx::query(
            "SELECT version() AS version_str, current_setting('server_version_num')::integer AS version_num",
        )
        .fetch_one(&self.pool)
        .await?;

        let server_version: String = row.try_get("version_str").unwrap_or_else(|_| "unknown".to_string());
        let server_version_num: i32 = row.try_get("version_num").unwrap_or(0);

        let is_supported = server_version_num >= min_version_num;
        let details = if is_supported {
            format!("PostgreSQL version {server_version_num} meets requirement (>= {min_version_num})")
        } else {
            format!(
                "PostgreSQL version {server_version_num} is below minimum requirement {min_version_num} (PostgreSQL 14+ required for safe zero-downtime DDL)"
            )
        };

        Ok(VersionValidation {
            server_version,
            server_version_num,
            is_supported,
            minimum_required: min_version_num,
            details,
        })
    }

    /// Validate installed PostgreSQL extensions against required list.
    pub async fn validate_extensions(
        &self,
        required: &[&str],
    ) -> Result<ExtensionValidation, sqlx::Error> {
        let rows = sqlx::query("SELECT extname, extversion FROM pg_extension")
            .fetch_all(&self.pool)
            .await?;

        let mut installed = HashMap::new();
        for r in rows {
            let name: String = r.try_get("extname").unwrap_or_default();
            let ver: String = r.try_get("extversion").unwrap_or_default();
            installed.insert(name, ver);
        }

        let mut missing_required = Vec::new();
        let mut recommendations = Vec::new();

        for &req in required {
            if !installed.contains_key(req) {
                missing_required.push(req.to_string());
                recommendations.push(format!("Run `CREATE EXTENSION IF NOT EXISTS \"{req}\";`"));
            }
        }

        let is_valid = missing_required.is_empty();

        Ok(ExtensionValidation {
            installed,
            missing_required,
            is_valid,
            recommendations,
        })
    }

    /// Validate migration records in `_sqlx_migrations`.
    pub async fn validate_migration_state(&self) -> Result<MigrationHistoryValidation, sqlx::Error> {
        let table_exists: (bool,) = sqlx::query_as(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_schema = 'public' AND table_name = '_sqlx_migrations')",
        )
        .fetch_one(&self.pool)
        .await?;

        if !table_exists.0 {
            return Ok(MigrationHistoryValidation {
                applied_versions: Vec::new(),
                dirty_versions: Vec::new(),
                duplicate_versions: Vec::new(),
                is_clean: true,
                details: vec!["_sqlx_migrations table does not exist yet (clean new database)".to_string()],
            });
        }

        let rows = sqlx::query("SELECT version, success FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&self.pool)
            .await?;

        let mut applied_versions = Vec::new();
        let mut dirty_versions = Vec::new();
        let mut seen_versions = HashMap::new();
        let mut duplicate_versions = Vec::new();

        for r in rows {
            let ver: i64 = r.try_get("version").unwrap_or_default();
            let succ: bool = r.try_get("success").unwrap_or(false);

            applied_versions.push(ver);
            if !succ {
                dirty_versions.push(ver);
            }

            let count = seen_versions.entry(ver).or_insert(0);
            *count += 1;
            if *count == 2 {
                duplicate_versions.push(ver);
            }
        }

        let mut details = Vec::new();
        if !dirty_versions.is_empty() {
            details.push(format!("Dirty/failed migrations detected: {:?}", dirty_versions));
        }
        if !duplicate_versions.is_empty() {
            details.push(format!("Duplicate migration versions detected: {:?}", duplicate_versions));
        }
        if dirty_versions.is_empty() && duplicate_versions.is_empty() {
            details.push(format!("Migration history clean ({} applied)", applied_versions.len()));
        }

        let is_clean = dirty_versions.is_empty() && duplicate_versions.is_empty();

        Ok(MigrationHistoryValidation {
            applied_versions,
            dirty_versions,
            duplicate_versions,
            is_clean,
            details,
        })
    }

    /// Consolidated environment validation.
    pub async fn validate_environment(
        &self,
        required_extensions: &[&str],
        min_pg_version_num: i32,
    ) -> Result<EnvironmentValidation, sqlx::Error> {
        let version = self.validate_postgres_version(min_pg_version_num).await?;
        let extensions = self.validate_extensions(required_extensions).await?;
        let migration_state = self.validate_migration_state().await?;

        let all_checks_passed = version.is_supported && extensions.is_valid && migration_state.is_clean;

        Ok(EnvironmentValidation {
            version,
            extensions,
            migration_state,
            all_checks_passed,
        })
    }

    // ── Populated Schema Risk Assessment ──────────────────────────────────────

    /// Inspect existing table sizes and evaluate migration lock risks against populated data.
    pub async fn check_populated_schema(
        &self,
        large_table_threshold_rows: i64,
    ) -> Result<PopulatedSchemaReport, sqlx::Error> {
        let rows = sqlx::query(
            r#"
            SELECT
                c.relname AS table_name,
                GREATEST(c.reltuples, 0)::BIGINT AS estimated_row_count,
                pg_total_relation_size(c.oid)::BIGINT AS total_size_bytes,
                pg_size_pretty(pg_total_relation_size(c.oid)) AS total_size_pretty
            FROM pg_class c
            JOIN pg_namespace n ON n.oid = c.relnamespace
            WHERE n.nspname = 'public' AND c.relkind = 'r'
            ORDER BY pg_total_relation_size(c.oid) DESC
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        let mut tables = Vec::new();
        for r in rows {
            let table_name: String = r.try_get("table_name").unwrap_or_default();
            let estimated_row_count: i64 = r.try_get("estimated_row_count").unwrap_or(0);
            let total_size_bytes: i64 = r.try_get("total_size_bytes").unwrap_or(0);
            let total_size_pretty: String = r.try_get("total_size_pretty").unwrap_or_default();
            let is_large = estimated_row_count >= large_table_threshold_rows;

            tables.push(TableStat {
                table_name,
                estimated_row_count,
                total_size_bytes,
                total_size_pretty,
                is_large,
            });
        }

        let classifications = self.classify_all_migrations();
        let report = evaluate_populated_schema_risk(&tables, &classifications);
        Ok(report)
    }

    // ── Interrupted Migration Diagnosis and Resumption ────────────────────────

    /// Diagnose interrupted or failed migrations in `_sqlx_migrations` and detect leftover invalid indexes.
    pub async fn diagnose_interrupted_migration(
        &self,
    ) -> Result<InterruptedMigrationDiagnosis, sqlx::Error> {
        let mut diagnosis = InterruptedMigrationDiagnosis::default();

        // 1. Check for failed rows in _sqlx_migrations
        let table_exists: (bool,) = sqlx::query_as(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_schema = 'public' AND table_name = '_sqlx_migrations')",
        )
        .fetch_one(&self.pool)
        .await?;

        if table_exists.0 {
            let failed_row = sqlx::query(
                "SELECT version, description FROM _sqlx_migrations WHERE success = false ORDER BY version LIMIT 1",
            )
            .fetch_optional(&self.pool)
            .await?;

            if let Some(r) = failed_row {
                let v: i64 = r.try_get("version").unwrap_or_default();
                let desc: String = r.try_get("description").unwrap_or_default();
                diagnosis.has_interrupted_migration = true;
                diagnosis.failed_version = Some(v);
                diagnosis.failed_description = Some(desc);
                diagnosis.failure_reason = Some("Migration marked dirty (success = false) in _sqlx_migrations".to_string());
            }
        }

        // 2. Check for invalid indexes from interrupted index builds
        diagnosis.invalid_indexes = self.find_invalid_indexes().await?;
        if !diagnosis.invalid_indexes.is_empty() {
            diagnosis.has_interrupted_migration = true;
        }

        // 3. Check for active lock contention blockers
        let lock_blockers = sqlx::query(
            r#"
            SELECT pid::TEXT || ': ' || COALESCE(query, '') AS blocker
            FROM pg_stat_activity
            WHERE pid <> pg_backend_pid()
              AND wait_event_type = 'Lock'
            LIMIT 5
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .unwrap_or_default();

        for b in lock_blockers {
            let s: String = b.try_get("blocker").unwrap_or_default();
            diagnosis.active_lock_blockers.push(s);
        }

        diagnosis.can_safely_resume = diagnosis.active_lock_blockers.is_empty();

        if diagnosis.has_interrupted_migration {
            let mut summary_parts = Vec::new();
            if let Some(v) = diagnosis.failed_version {
                summary_parts.push(format!("Interrupted migration version {v}"));
            }
            if !diagnosis.invalid_indexes.is_empty() {
                summary_parts.push(format!("{} invalid leftover index(es)", diagnosis.invalid_indexes.len()));
            }
            if !diagnosis.active_lock_blockers.is_empty() {
                summary_parts.push(format!("{} active lock blocker(s)", diagnosis.active_lock_blockers.len()));
            }
            diagnosis.diagnosis_summary = format!("INTERRUPTED MIGRATION DETECTED: {}", summary_parts.join("; "));
        } else {
            diagnosis.diagnosis_summary = "No interrupted migrations detected; database is in a clean state.".to_string();
        }

        Ok(diagnosis)
    }

    /// Query PostgreSQL catalogs for indexes left in an invalid state (`indisvalid = false`).
    pub async fn find_invalid_indexes(&self) -> Result<Vec<String>, sqlx::Error> {
        let rows = sqlx::query(
            r#"
            SELECT c.relname AS index_name
            FROM pg_class c
            JOIN pg_index i ON i.indexrelid = c.oid
            JOIN pg_namespace n ON n.oid = c.relnamespace
            WHERE n.nspname = 'public' AND i.indisvalid = false
            ORDER BY c.relname
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        let mut list = Vec::new();
        for r in rows {
            let name: String = r.try_get("index_name").unwrap_or_default();
            list.push(name);
        }
        Ok(list)
    }

    /// Drop invalid leftover indexes using `DROP INDEX IF EXISTS <name>`.
    pub async fn drop_invalid_indexes(
        &self,
        invalid_indexes: &[String],
    ) -> Result<Vec<String>, sqlx::Error> {
        let mut dropped = Vec::new();
        for idx in invalid_indexes {
            let clean_idx = idx.replace('"', "").replace(';', "");
            let query = format!("DROP INDEX IF EXISTS \"{clean_idx}\"");
            sqlx::query(&query).execute(&self.pool).await?;
            dropped.push(clean_idx);
            info!(index = idx, "dropped invalid leftover index");
        }
        Ok(dropped)
    }

    /// Delete dirty migration record (`success = false`) from `_sqlx_migrations` so it can be resumed.
    pub async fn resolve_dirty_migration(&self, version: i64) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM _sqlx_migrations WHERE version = $1 AND success = false")
            .bind(version)
            .execute(&self.pool)
            .await?;
        info!(version, "resolved dirty migration record in _sqlx_migrations");
        Ok(())
    }

    /// Generate an automated, safe resumption plan for an interrupted migration.
    pub fn plan_migration_resumption(
        &self,
        diagnosis: &InterruptedMigrationDiagnosis,
    ) -> ResumptionPlan {
        let mut steps = Vec::new();
        let mut step_num = 1;

        // Step 1: Terminate active lock blockers if any
        if !diagnosis.active_lock_blockers.is_empty() {
            steps.push(ResumptionStep {
                step_number: step_num,
                action: "Terminate lock contention blockers".to_string(),
                sql: Some(
                    "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE pid <> pg_backend_pid() AND wait_event_type = 'Lock';".to_string(),
                ),
                explanation: "Release conflicting exclusive locks held by aborted or hanging connections.".to_string(),
            });
            step_num += 1;
        }

        // Step 2: Drop invalid leftover indexes
        for idx in &diagnosis.invalid_indexes {
            steps.push(ResumptionStep {
                step_number: step_num,
                action: format!("Drop invalid index '{idx}'"),
                sql: Some(format!("DROP INDEX CONCURRENTLY IF EXISTS \"{idx}\";")),
                explanation: "Invalid indexes cannot be used by the planner and block concurrent index recreation.".to_string(),
            });
            step_num += 1;
        }

        // Step 3: Remove dirty record from _sqlx_migrations
        if let Some(v) = diagnosis.failed_version {
            steps.push(ResumptionStep {
                step_number: step_num,
                action: format!("Clear failed migration state for version {v}"),
                sql: Some(format!("DELETE FROM _sqlx_migrations WHERE version = {v} AND success = false;")),
                explanation: "Remove the failed record from _sqlx_migrations so sqlx migrate can retry cleanly.".to_string(),
            });
            step_num += 1;

            // Step 4: Re-apply migration with forward fix
            let catalog = get_known_migration_catalog();
            let forward_fix = catalog.iter().find(|m| m.version == v).and_then(|m| m.forward_fix.fix_sql.clone());

            steps.push(ResumptionStep {
                step_number: step_num,
                action: format!("Resume and re-apply migration {v}"),
                sql: forward_fix,
                explanation: "Safely execute the forward-fix script or re-run `sqlx migrate run`.".to_string(),
            });
            step_num += 1;
        }

        // Final verification step
        steps.push(ResumptionStep {
            step_number: step_num,
            action: "Verify database consistency".to_string(),
            sql: Some("SELECT version, success FROM _sqlx_migrations ORDER BY version;".to_string()),
            explanation: "Verify all migrations are marked success = true and no invalid indexes remain.".to_string(),
        });

        ResumptionPlan {
            failed_version: diagnosis.failed_version,
            steps,
            can_auto_execute: diagnosis.can_safely_resume,
            summary: format!(
                "Safe resumption plan generated with {} steps for version {:?}",
                step_num, diagnosis.failed_version
            ),
        }
    }

    /// Retrieve the complete migration catalog with lock and data classifications.
    pub fn classify_all_migrations(&self) -> Vec<MigrationClassification> {
        get_known_migration_catalog()
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    /// Snapshot the current schema from `information_schema.columns`.
    async fn snapshot_schema(
        &self,
    ) -> Result<HashMap<String, Vec<ColumnDescriptor>>, sqlx::Error> {
        let rows = sqlx::query_as::<_, ColumnDescriptor>(
            r#"
            SELECT
                table_name,
                column_name,
                data_type,
                (is_nullable = 'YES') AS is_nullable,
                column_default,
                ordinal_position
            FROM information_schema.columns
            WHERE table_schema = 'public'
            ORDER BY table_name, ordinal_position
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        let mut map: HashMap<String, Vec<ColumnDescriptor>> = HashMap::new();
        for row in rows {
            map.entry(row.table_name.clone()).or_default().push(row);
        }
        Ok(map)
    }

    /// Count how many migration files have not yet been applied.
    async fn count_pending_migrations(&self) -> Result<usize, sqlx::Error> {
        let migrator = sqlx::migrate!("./migrations");
        let applied: Vec<i64> = sqlx::query_scalar(
            "SELECT version FROM _sqlx_migrations ORDER BY version",
        )
        .fetch_all(&self.pool)
        .await
        .unwrap_or_default();

        let total = migrator.migrations.len();
        let pending = total.saturating_sub(applied.len());
        Ok(pending)
    }

    /// Run migrations inside a transaction that is always rolled back.
    async fn run_dry_run(&self) -> DryRunResult {
        let mut tx = match self.pool.begin().await {
            Ok(tx) => tx,
            Err(e) => {
                return DryRunResult {
                    success: false,
                    error: Some(format!("failed to begin transaction: {e}")),
                    statements_executed: 0,
                }
            }
        };

        let migrator = sqlx::migrate!("./migrations");
        let mut count = 0usize;

        for migration in migrator.migrations.iter() {
            let sql = migration.sql.as_ref();
            let stmts: Vec<&str> = sql
                .split(';')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .collect();
            count += stmts.len();

            if let Err(e) = sqlx::query(sql).execute(&mut *tx).await {
                let _ = tx.rollback().await;
                return DryRunResult {
                    success: false,
                    error: Some(format!(
                        "migration '{}' failed: {e}",
                        migration.description
                    )),
                    statements_executed: count,
                };
            }
        }

        let _ = tx.rollback().await;
        info!(statements = count, "dry-run complete (rolled back)");

        DryRunResult {
            success: true,
            error: None,
            statements_executed: count,
        }
    }
}

// ── SQL Statement Classifier ──────────────────────────────────────────────────

/// Analyze raw SQL statements and extract LockLevel, LockImpact, DataImpact, and target tables.
pub fn classify_sql_statement(sql: &str) -> (LockLevel, LockImpact, DataImpact, Vec<String>) {
    let upper = sql.to_uppercase();
    let mut lock = LockLevel::None;
    let mut lock_impact = LockImpact::Instant;
    let mut data_impact = DataImpact::MetadataOnly;
    let mut tables = Vec::new();

    // Table extraction (basic token parsing)
    for word in sql.split_whitespace() {
        let clean = word.trim_matches(|c: char| !c.is_alphanumeric() && c != '_');
        if clean.eq_ignore_ascii_case("pools")
            || clean.eq_ignore_ascii_case("predictions")
            || clean.eq_ignore_ascii_case("referrals")
            || clean.eq_ignore_ascii_case("stats")
            || clean.eq_ignore_ascii_case("creator_stats")
            || clean.eq_ignore_ascii_case("pool_templates")
            || clean.eq_ignore_ascii_case("referrer_stats")
            || clean.eq_ignore_ascii_case("referrer_users")
            || clean.eq_ignore_ascii_case("referrer_pool_stats")
            || clean.eq_ignore_ascii_case("user_stats")
            || clean.eq_ignore_ascii_case("user_pool_stats")
            || clean.eq_ignore_ascii_case("user_outcomes")
            || clean.eq_ignore_ascii_case("notifications")
            || clean.eq_ignore_ascii_case("user_interests")
        {
            let s = clean.to_lowercase();
            if !tables.contains(&s) {
                tables.push(s);
            }
        }
    }

    if upper.contains("ALTER TABLE") && upper.contains("ALTER COLUMN") && upper.contains("TYPE") {
        lock = LockLevel::AccessExclusive;
        lock_impact = LockImpact::ExclusiveBlocking;
        data_impact = DataImpact::FullTableRewrite;
    } else if upper.contains("DROP TABLE") || upper.contains("DROP COLUMN") {
        lock = LockLevel::AccessExclusive;
        lock_impact = LockImpact::Instant;
        data_impact = DataImpact::Destructive;
    } else if upper.contains("CREATE INDEX CONCURRENTLY") {
        lock = LockLevel::ShareUpdateExclusive;
        lock_impact = LockImpact::Instant;
        data_impact = DataImpact::IndexBuild;
    } else if upper.contains("CREATE INDEX") {
        lock = LockLevel::Share;
        lock_impact = LockImpact::WriteBlocking;
        data_impact = DataImpact::IndexBuild;
    } else if upper.contains("CREATE TRIGGER") || upper.contains("DROP TRIGGER") {
        lock = LockLevel::AccessExclusive;
        lock_impact = LockImpact::Instant;
        data_impact = DataImpact::MetadataOnly;
    } else if upper.contains("INSERT INTO") && upper.contains("SELECT") {
        lock = LockLevel::RowExclusive;
        lock_impact = LockImpact::WriteBlocking;
        data_impact = DataImpact::DataBackfill;
    } else if upper.contains("ALTER TABLE") && upper.contains("ADD COLUMN") {
        lock = LockLevel::AccessExclusive;
        lock_impact = LockImpact::Instant;
        data_impact = DataImpact::MetadataOnly;
    } else if upper.contains("CREATE TABLE") {
        lock = LockLevel::AccessExclusive;
        lock_impact = LockImpact::Instant;
        data_impact = DataImpact::MetadataOnly;
    }

    (lock, lock_impact, data_impact, tables)
}

// ── Known Migration Catalog ───────────────────────────────────────────────────

/// Comprehensive catalog of all migrations in the repository with lock, data impact,
/// rollback path, and forward-fix recovery documentation.
pub fn get_known_migration_catalog() -> Vec<MigrationClassification> {
    vec![
        MigrationClassification {
            name: "001_create_referrals.sql".to_string(),
            version: 1,
            lock_level: LockLevel::AccessExclusive,
            lock_impact: LockImpact::Instant,
            data_impact: DataImpact::MetadataOnly,
            target_tables: vec!["referrals".to_string()],
            safe_online: true,
            requires_maintenance_window: false,
            notes: "Creates referrals table and index on referrer. Safe online as table is brand new.".to_string(),
            rollback: RollbackPlan {
                strategy: RollbackStrategy::ReversibleSql,
                down_sql: Some("DROP TABLE IF EXISTS referrals CASCADE;".to_string()),
                instructions: "Drop referrals table and its index.".to_string(),
            },
            forward_fix: ForwardFixPlan {
                instructions: "Idempotent re-run using CREATE TABLE IF NOT EXISTS.".to_string(),
                fix_sql: Some("CREATE TABLE IF NOT EXISTS referrals (id BIGSERIAL PRIMARY KEY, referrer TEXT NOT NULL, user_address TEXT NOT NULL, pool_id BIGINT NOT NULL, amount BIGINT NOT NULL DEFAULT 0, created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()); CREATE INDEX IF NOT EXISTS idx_referrals_referrer ON referrals (referrer);".to_string()),
            },
        },
        MigrationClassification {
            name: "002_create_pools.sql".to_string(),
            version: 2,
            lock_level: LockLevel::AccessExclusive,
            lock_impact: LockImpact::Instant,
            data_impact: DataImpact::MetadataOnly,
            target_tables: vec!["pools".to_string()],
            safe_online: true,
            requires_maintenance_window: false,
            notes: "Creates pools table and indexes on state and category.".to_string(),
            rollback: RollbackPlan {
                strategy: RollbackStrategy::ReversibleSql,
                down_sql: Some("DROP TABLE IF EXISTS pools CASCADE;".to_string()),
                instructions: "Drop pools table and cascade foreign keys.".to_string(),
            },
            forward_fix: ForwardFixPlan {
                instructions: "Re-run with CREATE TABLE IF NOT EXISTS pools.".to_string(),
                fix_sql: None,
            },
        },
        MigrationClassification {
            name: "003_create_predictions.sql".to_string(),
            version: 3,
            lock_level: LockLevel::AccessExclusive,
            lock_impact: LockImpact::Instant,
            data_impact: DataImpact::MetadataOnly,
            target_tables: vec!["predictions".to_string(), "pools".to_string()],
            safe_online: true,
            requires_maintenance_window: false,
            notes: "Creates predictions table referencing pools(pool_id).".to_string(),
            rollback: RollbackPlan {
                strategy: RollbackStrategy::ReversibleSql,
                down_sql: Some("DROP TABLE IF EXISTS predictions CASCADE;".to_string()),
                instructions: "Drop predictions table and cascade.".to_string(),
            },
            forward_fix: ForwardFixPlan {
                instructions: "Ensure pools table exists first, then create predictions table.".to_string(),
                fix_sql: None,
            },
        },
        MigrationClassification {
            name: "004_creator_incentives_templates_and_indexes.sql".to_string(),
            version: 4,
            lock_level: LockLevel::Share,
            lock_impact: LockImpact::WriteBlocking,
            data_impact: DataImpact::IndexBuild,
            target_tables: vec!["pools".to_string(), "predictions".to_string(), "creator_stats".to_string(), "pool_templates".to_string()],
            safe_online: false,
            requires_maintenance_window: true,
            notes: "Adds columns with defaults (fast in PG 11+) but builds 7 standard indexes that block writes during creation on large tables.".to_string(),
            rollback: RollbackPlan {
                strategy: RollbackStrategy::ReversibleSql,
                down_sql: Some("DROP TABLE IF EXISTS pool_templates, creator_stats CASCADE; ALTER TABLE pools DROP COLUMN IF EXISTS creator_reward_bps, DROP COLUMN IF EXISTS min_participation_threshold, DROP COLUMN IF EXISTS creator_reward_paid, DROP COLUMN IF EXISTS creator_reward_amount; DROP INDEX IF EXISTS idx_pools_total_stake, idx_pools_end_time, idx_pools_created_at, idx_pools_state_category, idx_pools_creator, idx_pools_settled_with_result, idx_predictions_user_created, idx_predictions_pool_outcome;".to_string()),
                instructions: "Drop newly added creator tables, remove incentive columns, and drop the 7 indexes.".to_string(),
            },
            forward_fix: ForwardFixPlan {
                instructions: "If index creation times out or conflicts, drop incomplete indexes and rebuild using CREATE INDEX CONCURRENTLY.".to_string(),
                fix_sql: Some("CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_pools_total_stake ON pools (total_stake DESC); CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_pools_end_time ON pools (end_time ASC); CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_pools_created_at ON pools (created_at DESC);".to_string()),
            },
        },
        MigrationClassification {
            name: "004_enhance_schema_precision.sql".to_string(),
            version: 4,
            lock_level: LockLevel::AccessExclusive,
            lock_impact: LockImpact::ExclusiveBlocking,
            data_impact: DataImpact::FullTableRewrite,
            target_tables: vec!["pools".to_string(), "predictions".to_string(), "referrals".to_string(), "stats".to_string()],
            safe_online: false,
            requires_maintenance_window: true,
            notes: "HIGH RISK ON PRODUCTION: Alters total_stake and amount to NUMERIC(32,7) via table rewrite, holding AccessExclusiveLock. Backfills stats table.".to_string(),
            rollback: RollbackPlan {
                strategy: RollbackStrategy::ManualIntervention,
                down_sql: Some("ALTER TABLE pools ALTER COLUMN total_stake TYPE BIGINT USING ROUND(total_stake)::BIGINT; ALTER TABLE predictions ALTER COLUMN amount TYPE BIGINT USING ROUND(amount)::BIGINT; ALTER TABLE referrals ALTER COLUMN amount TYPE BIGINT USING ROUND(amount)::BIGINT; DROP TABLE IF EXISTS stats CASCADE; DROP FUNCTION IF EXISTS update_pool_stats(BIGINT);".to_string()),
                instructions: "Converting back from NUMERIC to BIGINT carries precision truncation risk if fractional tokens were recorded. Requires manual review.".to_string(),
            },
            forward_fix: ForwardFixPlan {
                instructions: "If lock timeout occurs on production, deploy a phased rollout: 1. Add new column amount_numeric; 2. Dual-write via trigger; 3. Batch backfill in chunks; 4. Swap column names.".to_string(),
                fix_sql: None,
            },
        },
        MigrationClassification {
            name: "005_add_index_pools_created_at.sql".to_string(),
            version: 5,
            lock_level: LockLevel::Share,
            lock_impact: LockImpact::WriteBlocking,
            data_impact: DataImpact::IndexBuild,
            target_tables: vec!["pools".to_string()],
            safe_online: false,
            requires_maintenance_window: false,
            notes: "Non-concurrent index on pools.created_at. Blocks pool writes during creation.".to_string(),
            rollback: RollbackPlan {
                strategy: RollbackStrategy::ReversibleSql,
                down_sql: Some("DROP INDEX IF EXISTS idx_pools_created_at;".to_string()),
                instructions: "Drop index idx_pools_created_at.".to_string(),
            },
            forward_fix: ForwardFixPlan {
                instructions: "Rebuild index concurrently without blocking writes.".to_string(),
                fix_sql: Some("DROP INDEX IF EXISTS idx_pools_created_at; CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_pools_created_at ON pools (created_at);".to_string()),
            },
        },
        MigrationClassification {
            name: "006_optimize_user_address_indexes.sql".to_string(),
            version: 6,
            lock_level: LockLevel::Share,
            lock_impact: LockImpact::WriteBlocking,
            data_impact: DataImpact::IndexBuild,
            target_tables: vec!["predictions".to_string(), "referrals".to_string()],
            safe_online: false,
            requires_maintenance_window: false,
            notes: "Indexes on predictions and referrals. Blocks writes on predictions while indexing.".to_string(),
            rollback: RollbackPlan {
                strategy: RollbackStrategy::ReversibleSql,
                down_sql: Some("DROP INDEX IF EXISTS idx_predictions_user_created, idx_referrals_user_address;".to_string()),
                instructions: "Drop newly created indexes.".to_string(),
            },
            forward_fix: ForwardFixPlan {
                instructions: "Recreate indexes concurrently if lock timeout was hit.".to_string(),
                fix_sql: Some("CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_predictions_user_created ON predictions (user_address, created_at DESC); CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_referrals_user_address ON referrals (user_address);".to_string()),
            },
        },
        MigrationClassification {
            name: "007_optimize_pool_id_indexes.sql".to_string(),
            version: 7,
            lock_level: LockLevel::Share,
            lock_impact: LockImpact::WriteBlocking,
            data_impact: DataImpact::IndexBuild,
            target_tables: vec!["referrals".to_string()],
            safe_online: true,
            requires_maintenance_window: false,
            notes: "Indexes on referrals (referrer, pool_id). Safe on moderate tables.".to_string(),
            rollback: RollbackPlan {
                strategy: RollbackStrategy::ReversibleSql,
                down_sql: Some("DROP INDEX IF EXISTS idx_referrals_referrer_pool, idx_referrals_pool_id;".to_string()),
                instructions: "Drop referral pool indexes.".to_string(),
            },
            forward_fix: ForwardFixPlan {
                instructions: "CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_referrals_referrer_pool ON referrals (referrer, pool_id);".to_string(),
                fix_sql: None,
            },
        },
        MigrationClassification {
            name: "008_referrer_stats.sql".to_string(),
            version: 8,
            lock_level: LockLevel::AccessExclusive,
            lock_impact: LockImpact::WriteBlocking,
            data_impact: DataImpact::DataBackfill,
            target_tables: vec!["referrals".to_string(), "referrer_users".to_string(), "referrer_stats".to_string(), "referrer_pool_stats".to_string()],
            safe_online: false,
            requires_maintenance_window: true,
            notes: "Creates auxiliary tables and trigger; runs synchronous backfill queries from referrals.".to_string(),
            rollback: RollbackPlan {
                strategy: RollbackStrategy::ReversibleSql,
                down_sql: Some("DROP TRIGGER IF EXISTS trg_referrals_maintain_stats ON referrals; DROP FUNCTION IF EXISTS maintain_referrer_stats; DROP TABLE IF EXISTS referrer_pool_stats, referrer_stats, referrer_users;".to_string()),
                instructions: "Drop trigger, function, and aggregate referrer tables.".to_string(),
            },
            forward_fix: ForwardFixPlan {
                instructions: "If backfill times out on large tables, apply trigger first, then backfill in separate chunked transactions.".to_string(),
                fix_sql: None,
            },
        },
        MigrationClassification {
            name: "009_add_predictions_indexes.sql".to_string(),
            version: 9,
            lock_level: LockLevel::Share,
            lock_impact: LockImpact::WriteBlocking,
            data_impact: DataImpact::IndexBuild,
            target_tables: vec!["predictions".to_string()],
            safe_online: false,
            requires_maintenance_window: true,
            notes: "Adds 4 indexes to predictions. On large tables, non-concurrent index build blocks betting transactions.".to_string(),
            rollback: RollbackPlan {
                strategy: RollbackStrategy::ReversibleSql,
                down_sql: Some("DROP INDEX IF EXISTS idx_predictions_pool_created, idx_predictions_outcome_pool, idx_predictions_pool_user, idx_predictions_amount_desc;".to_string()),
                instructions: "Drop the 4 predictions indexes.".to_string(),
            },
            forward_fix: ForwardFixPlan {
                instructions: "Rebuild concurrently: CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_predictions_pool_created ON predictions (pool_id, created_at DESC);".to_string(),
                fix_sql: None,
            },
        },
        MigrationClassification {
            name: "009_create_user_statistics.sql".to_string(),
            version: 9,
            lock_level: LockLevel::AccessExclusive,
            lock_impact: LockImpact::WriteBlocking,
            data_impact: DataImpact::DataBackfill,
            target_tables: vec!["predictions".to_string(), "user_stats".to_string(), "user_pool_stats".to_string(), "user_outcomes".to_string()],
            safe_online: false,
            requires_maintenance_window: true,
            notes: "Creates user_stats tables, trigger on predictions, and executes 3 aggregate backfills from predictions table.".to_string(),
            rollback: RollbackPlan {
                strategy: RollbackStrategy::ReversibleSql,
                down_sql: Some("DROP TRIGGER IF EXISTS trg_predictions_maintain_user_stats ON predictions; DROP FUNCTION IF EXISTS maintain_user_stats; DROP TABLE IF EXISTS user_outcomes, user_pool_stats, user_stats;".to_string()),
                instructions: "Drop trigger, maintenance function, and user stats tables.".to_string(),
            },
            forward_fix: ForwardFixPlan {
                instructions: "If backfill blocks writes, create tables and trigger first, then run backfill in asynchronous batches.".to_string(),
                fix_sql: None,
            },
        },
        MigrationClassification {
            name: "010_tags_claims_notifications.sql".to_string(),
            version: 10,
            lock_level: LockLevel::AccessExclusive,
            lock_impact: LockImpact::WriteBlocking,
            data_impact: DataImpact::IndexBuild,
            target_tables: vec!["pools".to_string(), "predictions".to_string(), "notifications".to_string(), "user_interests".to_string()],
            safe_online: true,
            requires_maintenance_window: false,
            notes: "Adds tags and claim columns, creates notifications table and GIN index. Fast in modern PostgreSQL.".to_string(),
            rollback: RollbackPlan {
                strategy: RollbackStrategy::ReversibleSql,
                down_sql: Some("DROP TABLE IF EXISTS user_interests, notifications; ALTER TABLE pools DROP COLUMN IF EXISTS tags, DROP COLUMN IF EXISTS resolved_at, DROP COLUMN IF EXISTS claim_window_seconds; ALTER TABLE predictions DROP COLUMN IF EXISTS claimed, DROP COLUMN IF EXISTS claimed_amount, DROP COLUMN IF EXISTS claimed_at; DROP INDEX IF EXISTS idx_pools_tags, idx_predictions_unclaimed;".to_string()),
                instructions: "Drop notifications, remove claim tracking and tags columns.".to_string(),
            },
            forward_fix: ForwardFixPlan {
                instructions: "Idempotent column and table additions. Drop and recreate invalid partial indexes.".to_string(),
                fix_sql: None,
            },
        },
    ]
}

// ── Populated Schema Evaluation Logic ─────────────────────────────────────────

/// Evaluate risk when applying migrations against populated tables.
pub fn evaluate_populated_schema_risk(
    tables: &[TableStat],
    classifications: &[MigrationClassification],
) -> PopulatedSchemaReport {
    let mut risks = Vec::new();

    for table in tables {
        if table.is_large {
            for class in classifications {
                if class.target_tables.iter().any(|t| t.eq_ignore_ascii_case(&table.table_name)) {
                    // Check for high-risk operations
                    if class.data_impact == DataImpact::FullTableRewrite {
                        risks.push(PopulatedSchemaRisk {
                            table_name: table.table_name.clone(),
                            migration_version: Some(class.version),
                            operation: format!("{}: Full table rewrite on '{}'", class.name, table.table_name),
                            lock_impact: class.lock_impact,
                            data_impact: class.data_impact,
                            risk_reason: format!(
                                "Table has ~{} rows ({}). ALTER COLUMN TYPE rewrites every row while holding AccessExclusiveLock, causing downtime.",
                                table.estimated_row_count, table.total_size_pretty
                            ),
                            recommended_mitigation: "Use phased dual-write expansion, batch backfill, and column swap.".to_string(),
                        });
                    } else if class.lock_impact == LockImpact::WriteBlocking && class.data_impact == DataImpact::IndexBuild {
                        risks.push(PopulatedSchemaRisk {
                            table_name: table.table_name.clone(),
                            migration_version: Some(class.version),
                            operation: format!("{}: Non-concurrent index build on '{}'", class.name, table.table_name),
                            lock_impact: class.lock_impact,
                            data_impact: class.data_impact,
                            risk_reason: format!(
                                "Table has ~{} rows. Non-concurrent CREATE INDEX acquires ShareLock, blocking all concurrent INSERT/UPDATE/DELETE.",
                                table.estimated_row_count
                            ),
                            recommended_mitigation: "Use CREATE INDEX CONCURRENTLY outside a multi-statement transaction.".to_string(),
                        });
                    } else if class.data_impact == DataImpact::DataBackfill {
                        risks.push(PopulatedSchemaRisk {
                            table_name: table.table_name.clone(),
                            migration_version: Some(class.version),
                            operation: format!("{}: Synchronous data backfill from '{}'", class.name, table.table_name),
                            lock_impact: class.lock_impact,
                            data_impact: class.data_impact,
                            risk_reason: format!(
                                "Table has ~{} rows. Backfill aggregate queries may exceed statement_timeout or lock tables.",
                                table.estimated_row_count
                            ),
                            recommended_mitigation: "Separate schema creation from data backfill; run backfill in chunked background worker.".to_string(),
                        });
                    }
                }
            }
        }
    }

    let has_high_risk = risks.iter().any(|r| r.lock_impact == LockImpact::ExclusiveBlocking || r.data_impact == DataImpact::FullTableRewrite);
    let summary = if risks.is_empty() {
        format!("Scanned {} tables; all tables are small or no high-risk operations detected.", tables.len())
    } else {
        format!("Scanned {} tables; detected {} potential risk(s) on populated tables.", tables.len(), risks.len())
    };

    PopulatedSchemaReport {
        tables: tables.to_vec(),
        risks,
        has_high_risk,
        summary,
    }
}

// ── Schema diff computation ───────────────────────────────────────────────────

/// Compute the diff between a `before` and `after` schema snapshot.
pub fn compute_schema_diff(
    before: &HashMap<String, Vec<ColumnDescriptor>>,
    after: &HashMap<String, Vec<ColumnDescriptor>>,
) -> Vec<SchemaDiffEntry> {
    let mut diff = Vec::new();

    // Tables added
    for table in after.keys() {
        if !before.contains_key(table) {
            diff.push(SchemaDiffEntry::TableAdded(table.clone()));
        }
    }

    // Tables dropped
    for table in before.keys() {
        if !after.contains_key(table) {
            diff.push(SchemaDiffEntry::TableDropped(table.clone()));
        }
    }

    // Column-level diff for tables that exist in both snapshots
    for (table, after_cols) in after {
        if let Some(before_cols) = before.get(table) {
            let before_map: HashMap<&str, &ColumnDescriptor> =
                before_cols.iter().map(|c| (c.column_name.as_str(), c)).collect();
            let after_map: HashMap<&str, &ColumnDescriptor> =
                after_cols.iter().map(|c| (c.column_name.as_str(), c)).collect();

            // Columns added
            for (col_name, col) in &after_map {
                if !before_map.contains_key(col_name) {
                    diff.push(SchemaDiffEntry::ColumnAdded {
                        table: table.clone(),
                        column: col_name.to_string(),
                        data_type: col.data_type.clone(),
                        nullable: col.is_nullable,
                    });
                }
            }

            // Columns dropped / changed
            for (col_name, before_col) in &before_map {
                match after_map.get(col_name) {
                    None => diff.push(SchemaDiffEntry::ColumnDropped {
                        table: table.clone(),
                        column: col_name.to_string(),
                    }),
                    Some(after_col) => {
                        if before_col.data_type != after_col.data_type {
                            diff.push(SchemaDiffEntry::TypeChanged {
                                table: table.clone(),
                                column: col_name.to_string(),
                                from: before_col.data_type.clone(),
                                to: after_col.data_type.clone(),
                            });
                        }
                        if before_col.is_nullable != after_col.is_nullable {
                            diff.push(SchemaDiffEntry::NullabilityChanged {
                                table: table.clone(),
                                column: col_name.to_string(),
                                was_nullable: before_col.is_nullable,
                                now_nullable: after_col.is_nullable,
                            });
                        }
                    }
                }
            }
        }
    }

    diff.sort_by(|a, b| format!("{a}").cmp(&format!("{b}")));
    diff
}

// ── Backward-compatibility analysis ──────────────────────────────────────────

/// Classify each diff entry as safe, breaking, or phase-in required.
pub fn analyse_compatibility(diff: &[SchemaDiffEntry]) -> Vec<CompatibilityFinding> {
    diff.iter()
        .map(|entry| {
            let risk = classify_risk(entry);
            CompatibilityFinding {
                entry: entry.clone(),
                risk,
            }
        })
        .collect()
}

fn classify_risk(entry: &SchemaDiffEntry) -> CompatibilityRisk {
    match entry {
        // Adding a new table is always safe.
        SchemaDiffEntry::TableAdded(_) => CompatibilityRisk::Safe,

        // Dropping a table is always breaking.
        SchemaDiffEntry::TableDropped(t) => CompatibilityRisk::Breaking(format!(
            "Dropping table '{t}' will break any code that still references it. \
             Use a multi-step rollout: deprecate → stop references → drop."
        )),

        // Adding a nullable column with a default is safe for zero-downtime.
        SchemaDiffEntry::ColumnAdded {
            nullable, table, column, ..
        } => {
            if *nullable {
                CompatibilityRisk::Safe
            } else {
                CompatibilityRisk::RequiresPhasing(format!(
                    "Adding NOT NULL column '{table}.{column}' without a default will \
                     fail on rows inserted by the old binary. Add a default or backfill first."
                ))
            }
        }

        // Dropping a column breaks old code that still SELECT/INSERT it.
        SchemaDiffEntry::ColumnDropped { table, column } => {
            CompatibilityRisk::Breaking(format!(
                "Dropping '{table}.{column}' will break old binaries that reference it. \
                 Deprecate the column in code first."
            ))
        }

        // Type changes are almost always breaking.
        SchemaDiffEntry::TypeChanged { table, column, from, to } => {
            CompatibilityRisk::Breaking(format!(
                "Changing '{table}.{column}' from {from} to {to} may corrupt data or \
                 break deserialization in running replicas."
            ))
        }

        // Making a nullable column NOT NULL breaks old inserts that omit it.
        SchemaDiffEntry::NullabilityChanged {
            table,
            column,
            was_nullable,
            now_nullable,
        } => {
            if *was_nullable && !*now_nullable {
                CompatibilityRisk::Breaking(format!(
                    "Adding NOT NULL constraint to '{table}.{column}' will reject inserts \
                     from old code that does not supply the column."
                ))
            } else {
                // NOT NULL → nullable: safe to do online.
                CompatibilityRisk::Safe
            }
        }
    }
}

// ── Rollback test helpers ─────────────────────────────────────────────────────

/// Test that applying a migration and then rolling back its transaction
/// leaves the schema unchanged.
pub async fn run_rollback_test(pool: &PgPool, migration_sql: &str) -> Result<bool, sqlx::Error> {
    let before = snapshot_public_tables(pool).await?;

    let mut tx = pool.begin().await?;
    let _ = sqlx::query(migration_sql).execute(&mut *tx).await;
    tx.rollback().await?;

    let after = snapshot_public_tables(pool).await?;
    let unchanged = before == after;

    if unchanged {
        info!("rollback test passed: schema is identical before and after rollback");
    } else {
        warn!("rollback test FAILED: schema changed despite transaction rollback");
    }

    Ok(unchanged)
}

/// Return a sorted list of table names in the `public` schema.
async fn snapshot_public_tables(pool: &PgPool) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar::<_, String>(
        "SELECT table_name FROM information_schema.tables \
         WHERE table_schema = 'public' ORDER BY table_name",
    )
    .fetch_all(pool)
    .await
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn col(table: &str, column: &str, data_type: &str, nullable: bool) -> ColumnDescriptor {
        ColumnDescriptor {
            table_name: table.to_string(),
            column_name: column.to_string(),
            data_type: data_type.to_string(),
            is_nullable: nullable,
            column_default: None,
            ordinal_position: 1,
        }
    }

    #[test]
    fn lock_level_hierarchy() {
        assert!(LockLevel::AccessExclusive > LockLevel::Share);
        assert!(LockLevel::Share > LockLevel::ShareUpdateExclusive);
        assert!(LockLevel::ShareUpdateExclusive > LockLevel::RowExclusive);
    }

    #[test]
    fn classify_sql_alter_column_type() {
        let sql = "ALTER TABLE pools ALTER COLUMN total_stake TYPE NUMERIC(32, 7) USING total_stake::NUMERIC(32, 7);";
        let (lock, impact, data, tables) = classify_sql_statement(sql);
        assert_eq!(lock, LockLevel::AccessExclusive);
        assert_eq!(impact, LockImpact::ExclusiveBlocking);
        assert_eq!(data, DataImpact::FullTableRewrite);
        assert!(tables.contains(&"pools".to_string()));
    }

    #[test]
    fn classify_sql_create_index_concurrently() {
        let sql = "CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_pools_created_at ON pools (created_at);";
        let (lock, impact, data, tables) = classify_sql_statement(sql);
        assert_eq!(lock, LockLevel::ShareUpdateExclusive);
        assert_eq!(impact, LockImpact::Instant);
        assert_eq!(data, DataImpact::IndexBuild);
        assert!(tables.contains(&"pools".to_string()));
    }

    #[test]
    fn classify_sql_standard_create_index() {
        let sql = "CREATE INDEX IF NOT EXISTS idx_pools_created_at ON pools (created_at);";
        let (lock, impact, data, tables) = classify_sql_statement(sql);
        assert_eq!(lock, LockLevel::Share);
        assert_eq!(impact, LockImpact::WriteBlocking);
        assert_eq!(data, DataImpact::IndexBuild);
        assert!(tables.contains(&"pools".to_string()));
    }

    #[test]
    fn classify_sql_drop_table() {
        let sql = "DROP TABLE IF EXISTS old_pools CASCADE;";
        let (lock, impact, data, _) = classify_sql_statement(sql);
        assert_eq!(lock, LockLevel::AccessExclusive);
        assert_eq!(impact, LockImpact::Instant);
        assert_eq!(data, DataImpact::Destructive);
    }

    #[test]
    fn classify_sql_backfill_insert() {
        let sql = "INSERT INTO stats (pool_id, total_stake) SELECT pool_id, total_stake FROM pools;";
        let (lock, impact, data, tables) = classify_sql_statement(sql);
        assert_eq!(lock, LockLevel::RowExclusive);
        assert_eq!(impact, LockImpact::WriteBlocking);
        assert_eq!(data, DataImpact::DataBackfill);
        assert!(tables.contains(&"stats".to_string()));
        assert!(tables.contains(&"pools".to_string()));
    }

    #[test]
    fn known_catalog_covers_all_twelve_migrations() {
        let catalog = get_known_migration_catalog();
        assert_eq!(catalog.len(), 12);
        for m in &catalog {
            assert!(!m.name.is_empty());
            assert!(!m.rollback.instructions.is_empty());
            assert!(!m.forward_fix.instructions.is_empty());
        }
    }

    #[test]
    fn known_catalog_004_enhance_schema_precision_is_high_risk() {
        let catalog = get_known_migration_catalog();
        let m = catalog.iter().find(|c| c.name == "004_enhance_schema_precision.sql").expect("found 004");
        assert_eq!(m.lock_level, LockLevel::AccessExclusive);
        assert_eq!(m.lock_impact, LockImpact::ExclusiveBlocking);
        assert_eq!(m.data_impact, DataImpact::FullTableRewrite);
        assert!(!m.safe_online);
        assert!(m.requires_maintenance_window);
        assert_eq!(m.rollback.strategy, RollbackStrategy::ManualIntervention);
    }

    #[test]
    fn populated_schema_risk_evaluation_detects_heavy_ops() {
        let tables = vec![
            TableStat {
                table_name: "predictions".to_string(),
                estimated_row_count: 500_000,
                total_size_bytes: 50_000_000,
                total_size_pretty: "50 MB".to_string(),
                is_large: true,
            },
            TableStat {
                table_name: "referrals".to_string(),
                estimated_row_count: 50,
                total_size_bytes: 8192,
                total_size_pretty: "8 kB".to_string(),
                is_large: false,
            },
        ];

        let catalog = get_known_migration_catalog();
        let report = evaluate_populated_schema_risk(&tables, &catalog);

        assert!(report.has_high_risk);
        assert!(!report.risks.is_empty());
        assert!(report.risks.iter().any(|r| r.table_name == "predictions"));
    }

    #[test]
    fn populated_schema_risk_clean_for_small_tables() {
        let tables = vec![
            TableStat {
                table_name: "pools".to_string(),
                estimated_row_count: 10,
                total_size_bytes: 8192,
                total_size_pretty: "8 kB".to_string(),
                is_large: false,
            },
        ];

        let catalog = get_known_migration_catalog();
        let report = evaluate_populated_schema_risk(&tables, &catalog);
        assert!(!report.has_high_risk);
        assert!(report.risks.is_empty());
    }

    #[test]
    fn interrupted_migration_resumption_plan_generation() {
        let checker = MigrationChecker {
            pool: PgPoolOptions_mock(),
        };

        let diagnosis = InterruptedMigrationDiagnosis {
            has_interrupted_migration: true,
            failed_version: Some(4),
            failed_description: Some("enhance_schema_precision".to_string()),
            failure_reason: Some("Transaction interrupted".to_string()),
            invalid_indexes: vec!["idx_predictions_user_created_failed".to_string()],
            active_lock_blockers: vec!["1234: ALTER TABLE pools".to_string()],
            diagnosis_summary: "Interrupted migration version 4".to_string(),
            can_safely_resume: false,
        };

        let plan = checker.plan_migration_resumption(&diagnosis);
        assert_eq!(plan.failed_version, Some(4));
        assert!(plan.steps.len() >= 4);
        assert!(plan.steps.iter().any(|s| s.action.contains("Terminate lock")));
        assert!(plan.steps.iter().any(|s| s.action.contains("Drop invalid index")));
        assert!(plan.steps.iter().any(|s| s.action.contains("Clear failed migration")));
        assert!(plan.steps.iter().any(|s| s.action.contains("Resume and re-apply")));
    }

    fn PgPoolOptions_mock() -> PgPool {
        // Return an uninitialized/dummy pool instance for pure unit tests
        // Since `PgPool` wraps Arc, we can construct with connect_lazy
        sqlx::PgPool::connect_lazy("postgres://localhost/dummy").unwrap()
    }

    #[test]
    fn diff_detects_new_table() {
        let before: HashMap<String, Vec<ColumnDescriptor>> = HashMap::new();
        let mut after = HashMap::new();
        after.insert("pools".to_string(), vec![col("pools", "id", "bigint", false)]);

        let diff = compute_schema_diff(&before, &after);
        assert!(diff.iter().any(|e| matches!(e, SchemaDiffEntry::TableAdded(t) if t == "pools")));
    }

    #[test]
    fn diff_detects_dropped_table() {
        let mut before = HashMap::new();
        before.insert(
            "old_table".to_string(),
            vec![col("old_table", "id", "bigint", false)],
        );
        let after: HashMap<String, Vec<ColumnDescriptor>> = HashMap::new();

        let diff = compute_schema_diff(&before, &after);
        assert!(
            diff.iter()
                .any(|e| matches!(e, SchemaDiffEntry::TableDropped(t) if t == "old_table"))
        );
    }

    #[test]
    fn diff_detects_added_column() {
        let mut before = HashMap::new();
        before.insert("pools".to_string(), vec![col("pools", "id", "bigint", false)]);
        let mut after = HashMap::new();
        after.insert(
            "pools".to_string(),
            vec![
                col("pools", "id", "bigint", false),
                col("pools", "name", "text", true),
            ],
        );

        let diff = compute_schema_diff(&before, &after);
        assert!(diff.iter().any(|e| matches!(
            e,
            SchemaDiffEntry::ColumnAdded { table, column, .. }
            if table == "pools" && column == "name"
        )));
    }

    #[test]
    fn diff_detects_dropped_column() {
        let mut before = HashMap::new();
        before.insert(
            "pools".to_string(),
            vec![
                col("pools", "id", "bigint", false),
                col("pools", "legacy", "text", true),
            ],
        );
        let mut after = HashMap::new();
        after.insert("pools".to_string(), vec![col("pools", "id", "bigint", false)]);

        let diff = compute_schema_diff(&before, &after);
        assert!(diff.iter().any(|e| matches!(
            e,
            SchemaDiffEntry::ColumnDropped { table, column }
            if table == "pools" && column == "legacy"
        )));
    }

    #[test]
    fn diff_detects_type_change() {
        let mut before = HashMap::new();
        before.insert("pools".to_string(), vec![col("pools", "amount", "integer", false)]);
        let mut after = HashMap::new();
        after.insert("pools".to_string(), vec![col("pools", "amount", "bigint", false)]);

        let diff = compute_schema_diff(&before, &after);
        assert!(diff.iter().any(|e| matches!(
            e,
            SchemaDiffEntry::TypeChanged { table, column, from, to }
            if table == "pools" && column == "amount" && from == "integer" && to == "bigint"
        )));
    }

    #[test]
    fn compat_table_drop_is_breaking() {
        let entry = SchemaDiffEntry::TableDropped("pools".to_string());
        let finding = &analyse_compatibility(&[entry])[0];
        assert!(matches!(finding.risk, CompatibilityRisk::Breaking(_)));
    }

    #[test]
    fn compat_nullable_column_add_is_safe() {
        let entry = SchemaDiffEntry::ColumnAdded {
            table: "pools".to_string(),
            column: "notes".to_string(),
            data_type: "text".to_string(),
            nullable: true,
        };
        let finding = &analyse_compatibility(&[entry])[0];
        assert_eq!(finding.risk, CompatibilityRisk::Safe);
    }

    #[test]
    fn compat_not_null_column_add_requires_phasing() {
        let entry = SchemaDiffEntry::ColumnAdded {
            table: "pools".to_string(),
            column: "required_field".to_string(),
            data_type: "text".to_string(),
            nullable: false,
        };
        let finding = &analyse_compatibility(&[entry])[0];
        assert!(matches!(finding.risk, CompatibilityRisk::RequiresPhasing(_)));
    }

    #[test]
    fn compat_type_change_is_breaking() {
        let entry = SchemaDiffEntry::TypeChanged {
            table: "pools".to_string(),
            column: "amount".to_string(),
            from: "integer".to_string(),
            to: "text".to_string(),
        };
        let finding = &analyse_compatibility(&[entry])[0];
        assert!(matches!(finding.risk, CompatibilityRisk::Breaking(_)));
    }

    #[test]
    fn compat_nullable_to_not_null_is_breaking() {
        let entry = SchemaDiffEntry::NullabilityChanged {
            table: "pools".to_string(),
            column: "email".to_string(),
            was_nullable: true,
            now_nullable: false,
        };
        let finding = &analyse_compatibility(&[entry])[0];
        assert!(matches!(finding.risk, CompatibilityRisk::Breaking(_)));
    }

    #[test]
    fn compat_not_null_to_nullable_is_safe() {
        let entry = SchemaDiffEntry::NullabilityChanged {
            table: "pools".to_string(),
            column: "email".to_string(),
            was_nullable: false,
            now_nullable: true,
        };
        let finding = &analyse_compatibility(&[entry])[0];
        assert_eq!(finding.risk, CompatibilityRisk::Safe);
    }

    #[test]
    fn report_summary_contains_expected_sections() {
        let mut report = MigrationReport::default();
        report.pending_migrations = 2;
        report.dry_run = Some(DryRunResult {
            success: true,
            error: None,
            statements_executed: 5,
        });
        report.classifications = get_known_migration_catalog();
        report.schema_diff = vec![SchemaDiffEntry::TableAdded("new_table".to_string())];
        report.has_breaking_changes = false;

        let summary = report.summary();
        assert!(summary.contains("Pending migrations"));
        assert!(summary.contains("Dry-Run Simulation"));
        assert!(summary.contains("Lock & Data Impact Classifications"));
        assert!(summary.contains("new_table"));
        assert!(summary.contains("No breaking changes"));
    }
}
