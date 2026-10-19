# PrediFi Database Migration Deployment Guide

## Overview

This guide establishes the production deployment procedures, lock-impact classifications, rollback strategies, and recovery runbooks for all PostgreSQL schema migrations in PrediFi.

The primary objective is to **reduce deployment risk** from schema changes that are incompatible with running applications or unsafe on production-sized tables.

---

## 1. PostgreSQL Locking and Concurrency Model

PostgreSQL acquires table-level locks for all Data Definition Language (DDL) operations. The concurrency impact depends on the specific lock level and table volume:

| Lock Level | Acquired By | Conflicts With | Concurrency Impact |
| :--- | :--- | :--- | :--- |
| **`AccessShareLock`** | `SELECT` | `AccessExclusiveLock` | Live reads and writes continue unimpeded. |
| **`RowExclusiveLock`** | `INSERT`, `UPDATE`, `DELETE` | `ShareLock`, `AccessExclusiveLock` | Standard data modification lock. |
| **`ShareUpdateExclusiveLock`** | `CREATE INDEX CONCURRENTLY`, `VACUUM`, `ANALYZE` | Other DDL / concurrent index builds | **Safe Online**: Concurrent reads and writes continue without blocking. |
| **`ShareLock`** | Standard `CREATE INDEX` | `INSERT`, `UPDATE`, `DELETE`, `AccessExclusiveLock` | **Write-Blocking**: Live reads succeed; all write transactions stall until the index build completes. |
| **`AccessExclusiveLock`** | `ALTER TABLE` (most forms), `DROP TABLE`, `TRUNCATE`, `CREATE TRIGGER` | All locks (reads and writes) | **Exclusive-Blocking**: Halts **all** reads and writes on the target table. |

### Operational Duration and Impact Classifications

1. **`Instant` (O(1) Catalog Update)**:
   - Takes `< 10ms`.
   - Modifies system catalogs (`pg_class`, `pg_attribute`) only.
   - Examples: `CREATE TABLE`, `ADD COLUMN` with constant default in PostgreSQL 11+.
   - Concurrency risk: negligible when protected by `lock_timeout`.

2. **`WriteBlocking`**:
   - Holds `ShareLock` or blocks modifications during scans.
   - Examples: Standard `CREATE INDEX` without `CONCURRENTLY`, synchronous data backfills.
   - Concurrency risk: Can queue writes behind the lock, exhausting connection pools.

3. **`ExclusiveBlocking`**:
   - Holds `AccessExclusiveLock` while scanning or rewriting the entire heap.
   - Examples: `ALTER TABLE ... ALTER COLUMN TYPE ... USING ...`, adding constraints without `NOT VALID`.
   - Concurrency risk: Total application stall for the duration of the migration.

### Data Impact Classifications

- **`MetadataOnly`**: Modifies pg_catalog only. 0 heap tuples modified.
- **`IndexBuild`**: Reads table pages, constructs B-Tree/GIN index pages.
- **`TableScan`**: Full table scan to validate constraints or calculate initial aggregates.
- **`FullTableRewrite`**: Rewrites every tuple in the table heap on disk.
- **`DataBackfill`**: Inserts, updates, or populates auxiliary rows.
- **`Destructive`**: Drops tables or columns, causing permanent data removal.

---

## 2. Comprehensive Migration Classification Matrix

The following table documents every migration in `backend/migrations/` (migrations `001` through `010`), detailing lock levels, concurrency risks, rollback paths, and forward-fix recovery procedures.

| Migration File | Version | Target Tables | Lock Level | Lock Impact | Data Impact | Safe Online? | Maint. Window? |
| :--- | :---: | :--- | :--- | :--- | :--- | :---: | :---: |
| `001_create_referrals.sql` | `1` | `referrals` | `AccessExclusiveLock` (new table) | `Instant` | `MetadataOnly` | **Yes** | No |
| `002_create_pools.sql` | `2` | `pools` | `AccessExclusiveLock` (new table) | `Instant` | `MetadataOnly` | **Yes** | No |
| `003_create_predictions.sql` | `3` | `predictions`, `pools` (FK) | `AccessExclusiveLock` (new table) | `Instant` | `MetadataOnly` | **Yes** | No |
| `004_creator_incentives_templates_and_indexes.sql` | `4` | `pools`, `predictions`, `creator_stats`, `pool_templates` | `ShareLock` (7 indexes), `AccessExclusiveLock` (columns) | `WriteBlocking` | `IndexBuild` | **No** on large tables | **Yes** (> 100k rows) |
| `004_enhance_schema_precision.sql` | `4` | `pools`, `predictions`, `referrals`, `stats` | `AccessExclusiveLock` (type alter), `RowExclusiveLock` (stats) | `ExclusiveBlocking` | `FullTableRewrite` | **No** | **Yes** (or phased dual-write) |
| `005_add_index_pools_created_at.sql` | `5` | `pools` | `ShareLock` | `WriteBlocking` | `IndexBuild` | **No** on high-write | No (if concurrent) |
| `006_optimize_user_address_indexes.sql` | `6` | `predictions`, `referrals` | `ShareLock` | `WriteBlocking` | `IndexBuild` | **No** on large tables | No (if concurrent) |
| `007_optimize_pool_id_indexes.sql` | `7` | `referrals` | `ShareLock` | `WriteBlocking` | `IndexBuild` | **Yes** | No |
| `008_referrer_stats.sql` | `8` | `referrals`, `referrer_users`, `referrer_stats`, `referrer_pool_stats` | `AccessExclusiveLock` (trigger), `RowExclusiveLock` (backfill) | `WriteBlocking` | `DataBackfill` | **No** on large tables | **Yes** (if backfill exceeds timeout) |
| `009_add_predictions_indexes.sql` | `9` | `predictions` | `ShareLock` (4 indexes) | `WriteBlocking` | `IndexBuild` | **No** on large tables | **Yes** (if non-concurrent) |
| `009_create_user_statistics.sql` | `9` | `predictions`, `pools`, `user_stats`, `user_pool_stats`, `user_outcomes` | `AccessExclusiveLock` (trigger), `RowExclusiveLock` (backfill) | `WriteBlocking` | `DataBackfill` | **No** on large tables | **Yes** |
| `010_tags_claims_notifications.sql` | `10` | `pools`, `predictions`, `notifications`, `user_interests` | `AccessExclusiveLock` (columns), `ShareLock` (GIN/partial index) | `WriteBlocking` | `IndexBuild` | **Yes** (moderate size) | No |

---

## 3. Rollback and Forward-Fix Paths for Each Migration

### Migration `001_create_referrals.sql`
- **Rollback Path**:
  ```sql
  DROP TABLE IF EXISTS referrals CASCADE;
  ```
- **Forward-Fix Path**:
  Re-run migration idempotently. If index creation was interrupted, execute:
  ```sql
  CREATE INDEX IF NOT EXISTS idx_referrals_referrer ON referrals (referrer);
  ```

---

### Migration `002_create_pools.sql`
- **Rollback Path**:
  ```sql
  DROP TABLE IF EXISTS pools CASCADE;
  ```
- **Forward-Fix Path**:
  Re-run with `CREATE TABLE IF NOT EXISTS pools`. If state check constraint failed due to invalid legacy values, fix or normalize existing rows before applying constraint.

---

### Migration `003_create_predictions.sql`
- **Rollback Path**:
  ```sql
  DROP TABLE IF EXISTS predictions CASCADE;
  ```
- **Forward-Fix Path**:
  Ensure the parent `pools` table exists and foreign key constraints can resolve. Re-run with `CREATE TABLE IF NOT EXISTS`.

---

### Migration `004_creator_incentives_templates_and_indexes.sql`
- **Rollback Path**:
  ```sql
  DROP TABLE IF EXISTS pool_templates, creator_stats CASCADE;
  ALTER TABLE pools 
      DROP COLUMN IF EXISTS creator_reward_bps,
      DROP COLUMN IF EXISTS min_participation_threshold,
      DROP COLUMN IF EXISTS creator_reward_paid,
      DROP COLUMN IF EXISTS creator_reward_amount;
  DROP INDEX IF EXISTS 
      idx_pools_total_stake,
      idx_pools_end_time,
      idx_pools_created_at,
      idx_pools_state_category,
      idx_pools_creator,
      idx_pools_settled_with_result,
      idx_predictions_user_created,
      idx_predictions_pool_outcome;
  ```
- **Forward-Fix Path**:
  If non-concurrent index creation times out or conflicts with client queries, drop any invalid indexes and create them concurrently:
  ```sql
  CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_pools_total_stake ON pools (total_stake DESC);
  CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_pools_end_time ON pools (end_time ASC);
  CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_pools_created_at ON pools (created_at DESC);
  CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_pools_state_category ON pools (state, category);
  CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_pools_creator ON pools (creator);
  CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_pools_settled_with_result ON pools (pool_id) WHERE state = 'settled' AND result IS NOT NULL;
  CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_predictions_user_created ON predictions (user_address, created_at DESC);
  CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_predictions_pool_outcome ON predictions (pool_id, outcome);
  ```

---

### Migration `004_enhance_schema_precision.sql`
- **Rollback Path (Manual Intervention Required)**:
  > [!WARNING]
  > Converting `NUMERIC(32, 7)` back to `BIGINT` will truncate any fractional decimal digits. Review live data for non-zero fractional values before attempting reversal.
  ```sql
  ALTER TABLE pools ALTER COLUMN total_stake TYPE BIGINT USING ROUND(total_stake)::BIGINT;
  ALTER TABLE predictions ALTER COLUMN amount TYPE BIGINT USING ROUND(amount)::BIGINT;
  ALTER TABLE referrals ALTER COLUMN amount TYPE BIGINT USING ROUND(amount)::BIGINT;
  DROP TABLE IF EXISTS stats CASCADE;
  DROP FUNCTION IF EXISTS update_pool_stats(BIGINT);
  ```
- **Forward-Fix Path**:
  If the `ALTER COLUMN TYPE` query times out or exhausts lock wait limits on large tables, execute a zero-downtime phased expansion:
  1. Add temporary column: `ALTER TABLE predictions ADD COLUMN IF NOT EXISTS amount_numeric NUMERIC(32, 7);`
  2. Register dual-write trigger syncing inserts/updates.
  3. Backfill rows in background batches: `UPDATE predictions SET amount_numeric = amount WHERE amount_numeric IS NULL AND id BETWEEN $start AND $end;`
  4. Swap column names in a brief transaction.

---

### Migration `005_add_index_pools_created_at.sql`
- **Rollback Path**:
  ```sql
  DROP INDEX IF EXISTS idx_pools_created_at;
  ```
- **Forward-Fix Path**:
  ```sql
  DROP INDEX IF EXISTS idx_pools_created_at;
  CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_pools_created_at ON pools (created_at);
  ```

---

### Migration `006_optimize_user_address_indexes.sql`
- **Rollback Path**:
  ```sql
  DROP INDEX IF EXISTS idx_predictions_user_created, idx_referrals_user_address;
  ```
- **Forward-Fix Path**:
  ```sql
  DROP INDEX IF EXISTS idx_predictions_user_created, idx_referrals_user_address;
  CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_predictions_user_created ON predictions (user_address, created_at DESC);
  CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_referrals_user_address ON referrals (user_address);
  ```

---

### Migration `007_optimize_pool_id_indexes.sql`
- **Rollback Path**:
  ```sql
  DROP INDEX IF EXISTS idx_referrals_referrer_pool, idx_referrals_pool_id;
  ```
- **Forward-Fix Path**:
  ```sql
  DROP INDEX IF EXISTS idx_referrals_referrer_pool, idx_referrals_pool_id;
  CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_referrals_referrer_pool ON referrals (referrer, pool_id);
  CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_referrals_pool_id ON referrals (pool_id);
  ```

---

### Migration `008_referrer_stats.sql`
- **Rollback Path**:
  ```sql
  DROP TRIGGER IF EXISTS trg_referrals_maintain_stats ON referrals;
  DROP FUNCTION IF EXISTS maintain_referrer_stats;
  DROP TABLE IF EXISTS referrer_pool_stats, referrer_stats, referrer_users CASCADE;
  ```
- **Forward-Fix Path**:
  If the synchronous backfill query times out on large `referrals` tables:
  1. Truncate incomplete auxiliary tables: `TRUNCATE referrer_pool_stats, referrer_stats, referrer_users;`
  2. Create trigger first so new events are captured.
  3. Run historical aggregate backfills in small chunked transactions via an offline script.

---

### Migration `009_add_predictions_indexes.sql`
- **Rollback Path**:
  ```sql
  DROP INDEX IF EXISTS 
      idx_predictions_pool_created,
      idx_predictions_outcome_pool,
      idx_predictions_pool_user,
      idx_predictions_amount_desc;
  ```
- **Forward-Fix Path**:
  Rebuild each index concurrently one by one:
  ```sql
  CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_predictions_pool_created ON predictions (pool_id, created_at DESC);
  CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_predictions_outcome_pool ON predictions (outcome, pool_id);
  CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_predictions_pool_user ON predictions (pool_id, user_address);
  CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_predictions_amount_desc ON predictions (amount DESC);
  ```

---

### Migration `009_create_user_statistics.sql`
- **Rollback Path**:
  ```sql
  DROP TRIGGER IF EXISTS trg_predictions_maintain_user_stats ON predictions;
  DROP FUNCTION IF EXISTS maintain_user_stats;
  DROP TABLE IF EXISTS user_outcomes, user_pool_stats, user_stats CASCADE;
  ```
- **Forward-Fix Path**:
  If backfills hit statement timeout:
  1. Clear partial aggregate rows: `TRUNCATE user_outcomes, user_pool_stats, user_stats;`
  2. Install `maintain_user_stats()` trigger on `predictions`.
  3. Backfill aggregates in user-address ranges asynchronously.

---

### Migration `010_tags_claims_notifications.sql`
- **Rollback Path**:
  ```sql
  DROP TABLE IF EXISTS user_interests, notifications CASCADE;
  ALTER TABLE pools DROP COLUMN IF EXISTS tags, DROP COLUMN IF EXISTS resolved_at, DROP COLUMN IF EXISTS claim_window_seconds;
  ALTER TABLE predictions DROP COLUMN IF EXISTS claimed, DROP COLUMN IF EXISTS claimed_amount, DROP COLUMN IF EXISTS claimed_at;
  DROP INDEX IF EXISTS idx_pools_tags, idx_predictions_unclaimed;
  ```
- **Forward-Fix Path**:
  Idempotent re-run. If GIN index `idx_pools_tags` or partial index `idx_predictions_unclaimed` failed, drop invalid versions and recreate concurrently:
  ```sql
  DROP INDEX IF EXISTS idx_pools_tags, idx_predictions_unclaimed;
  CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_pools_tags ON pools USING GIN (tags);
  CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_predictions_unclaimed ON predictions (pool_id, user_address) WHERE NOT claimed;
  ```

---

## 4. Environment and Extension Validation

Before applying migrations, execute the validation queries below or invoke `MigrationChecker::validate_environment`:

### 1. PostgreSQL Version Validation
- **Requirement**: PostgreSQL 14 or higher (`server_version_num >= 140000`).
- **Validation SQL**:
  ```sql
  SELECT version(), current_setting('server_version_num')::integer AS version_num;
  ```
- **Rationale**: PostgreSQL 14+ supports fast `ADD COLUMN` with constant defaults without table rewrites, as well as enhanced lock management and concurrent index recovery.

### 2. Extension State Validation
- **Requirement**: `plpgsql` (installed by default) and optional indexing helpers (`btree_gist` / `pgcrypto`).
- **Validation SQL**:
  ```sql
  SELECT extname, extversion FROM pg_extension;
  ```

### 3. Duplicate Version Prefix Handling (004 and 009)
The repository contains duplicate version prefixes:
- `004_creator_incentives_templates_and_indexes.sql` and `004_enhance_schema_precision.sql`
- `009_add_predictions_indexes.sql` and `009_create_user_statistics.sql`

Because `_sqlx_migrations` enforces `version` as the primary key:
- Existing databases that already recorded one of each pair must **not** renumber them.
- In production deployments, ensure migrations are tracked with unique version keys or execute them through controlled release pipelines.

---

## 5. Populated Schema Checks & Large Table Deployment Strategy

When deploying against production databases where tables exceed **10,000 rows**:

### Pre-Deployment Table Inspection
Run the following query to measure row counts and table footprints:
```sql
SELECT
    c.relname AS table_name,
    GREATEST(c.reltuples, 0)::BIGINT AS estimated_rows,
    pg_total_relation_size(c.oid)::BIGINT AS total_bytes,
    pg_size_pretty(pg_total_relation_size(c.oid)) AS total_size
FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = 'public' AND c.relkind = 'r'
ORDER BY pg_total_relation_size(c.oid) DESC;
```

### Safety Rules for Populated Tables:
1. **Never alter column types directly with `ALTER COLUMN TYPE` on populated tables**. Use the 4-phase expand/contract pattern.
2. **Never run non-concurrent `CREATE INDEX` on live tables**. Always specify `CONCURRENTLY`.
3. **Always configure lock and statement timeouts**:
   ```sql
   SET lock_timeout = '3s';
   SET statement_timeout = '60s';
   ```
   If a migration cannot acquire its lock within 3 seconds, it will abort rather than queue client transactions and cause connection exhaustion.

---

## 6. Interrupted Migration Diagnosis and Safe Resumption Runbook

If a migration is interrupted (e.g. timeout, process restart, or network interruption), follow this runbook to diagnose and safely resume.

### Step 1: Diagnose Active Lock Blockers
Inspect whether lingering transactions are holding locks on target tables:
```sql
SELECT pid, usename, state, wait_event_type, wait_event, query
FROM pg_stat_activity
WHERE pid <> pg_backend_pid()
  AND wait_event_type = 'Lock';
```
If an aborted migration session is holding conflicting locks, terminate it:
```sql
SELECT pg_terminate_backend(<pid>);
```

### Step 2: Detect Leftover Invalid Indexes
If an interrupted index creation failed, PostgreSQL leaves behind an index marked `indisvalid = false`. The planner cannot use it, but it continues to consume write overhead:
```sql
SELECT c.relname AS invalid_index_name
FROM pg_class c
JOIN pg_index i ON i.indexrelid = c.oid
JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = 'public' AND i.indisvalid = false;
```
Drop any invalid indexes:
```sql
DROP INDEX CONCURRENTLY IF EXISTS <invalid_index_name>;
```

### Step 3: Inspect `_sqlx_migrations` State
Check for failed or uncommitted migration records:
```sql
SELECT version, description, installed_on, success, execution_time
FROM _sqlx_migrations
WHERE success = false;
```

### Step 4: Clean Dirty Migration State
To allow `sqlx migrate run` to retry the failed migration, remove the dirty record:
```sql
DELETE FROM _sqlx_migrations WHERE version = <failed_version> AND success = false;
```

### Step 5: Resume and Verify
1. Apply the forward-fix path for the failed migration.
2. Re-run `sqlx migrate run --source ./migrations`.
3. Confirm all migrations are marked `success = true`:
   ```sql
   SELECT version, description, success, installed_on FROM _sqlx_migrations ORDER BY version;
   ```
4. Verify application read/write health via `/health` endpoint.
