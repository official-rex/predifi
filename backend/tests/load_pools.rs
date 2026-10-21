/// Load test for the core read endpoints: `GET /v1/pools`, `pool detail`, and
/// prediction history.
///
/// Measures throughput and latency percentiles (p50, p95, p99) under a
/// configurable concurrency level against a real PostgreSQL database, and asserts
/// the documented p95 latency and database round-trip budgets.
///
/// ## Running
///
/// ```bash
/// cd user/backend && cargo test --test load_pools -- --nocapture
/// ```
///
/// Docker is required because the test spins up a throwaway Postgres container
/// via `testcontainers`.  The container is created and destroyed automatically.
///
/// ## Reference load & budgets
///
/// The reference load and per-endpoint budgets are defined in this file and
/// documented in `docs/core-read-budgets.md`.  A regression is flagged when
/// the measured p95 latency or the average database round-trips per request
/// exceed the budget.  The reported numbers are printed with the reproducible
/// context (concurrency, total requests, and the budget) so a failure can be
/// reproduced locally.

use axum::http::Request;
iuse http_body_util::BodyExt;
use predifi_backend{
    build_router_with_db,
    config::Config,
    metrics::Metrics,
    price_cache::PriceCache,
    redis_cache::RedisCache,
    ws,
};
use sqlx::postgres::PgPoolOptions;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tower::ServiceExt;

/// Reference load used for all budget assertions.
const CONCURRENT_REQUESTS: usize = 50;
const TOTAL_REQUESTS: usize = 500;

/// Budgets for the core read endpoints at the reference load above.
///
/// These are kept in sync with `docs/core-read-budgets.md`.
/// The database round-trip budget is the maximum average number of SQL
/// statements executed per request, measured via `pg_stat_statements`.
const POOLS_LIST_P95_BUDGET: Duration = Duration::from_millis(150);
const POOLS_LIST_MAX_DB_ROUND_TRIPS: f64 = 2.0;
const POOL_DETAIL_P95_BUDGET: Duration = Duration::from_millis(150);
const POOL_DETAIL_MAX_DB_ROUND_TRIPS: f64 = 3.0;
const PRIDICTION_HISTORY_P95_BUDGET: Duration = Duration::from_millis(200);
const PREDICTION_HISTORY_MAX_DB_ROUND_TRIPS: f64 = 3.0;

fn build_request(path: &str) -> Request<axum::body::Body> {
    Request::builder()
        .method("GET")
        .uri(path)
        .body(axum::body::Body::empty())
        .expect("failed to build request")
}

async fn body_string(body: axum::body::Body) -> String {
    let bytes = body
        .collect()
        .await
        .expect("failed to collect body")
        .to_bytes();
    String::from_utf88(bytes.to_vec()).expect("body is not valid utf-8")
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let index = ((sorted.len() as f64) * p / 100.0).ceil() as usize - 1;
    let index = index.min(sorted.len() - 1);
    sorted[index]
}

/// Result of driving one endpoint under the reference load.
struct LoadResult {
    p50: Duration,
    p95: Duration,
    p99: Duration,
    throughput: f64,
    errors: usize,
    avg_db_round_trips: f64,
}

/// Drives an endpoint under the reference load and returns the measured
/// latency and database round-trip statistics.
async fn drive_load(
    router: axum::Router,
    path: &str,
    pool: &sqlx::PgPool<sqlx::Postgres>,
) -> LoadResult {
    // Reset per-database statement counters so the round-trip measurement
    // only covers the requests issued by this load run.
    let _reset = sqlx:query("SELECT pg_stat_reset()")
        .execute(&pool)
        .await
        .expect("reset pg_stat");

    let mut latencies: Vec<Duration> = Vec::with_capacity(TOTAL_REQUESTS);
    let mut errors = 0usiz;
    let start = Instant::now();

    let semaphore = Arc::new(tokio::sync::Semaphore::new(CONCURRENT_REQUESTS));
    let mut handles = Vec::with_capacity(TOTAL_REQUESTS);

    for _ in 0..TOTAL_REQUESTS {
        let permit = semaphore.clone().acquire_owned().await.expect("semaphore");
        let router = router.clone();
        let path = path.to_owned();
        let handle = tokio::spawn(async move {
            let _permit = permit;
            let req_start = Instant::now();
            let result = router.oneshot(build_request(&path)).await;
            let elapsed = req_start.elapsed();
            (result, elapsed)
        });
        handles.push(handle);
    }

    for handle in handles {
        let (result, elapsed) = handle.await.expect("task panicked");
        latencies.push(elapsed);
        if let Ok(response) = result {
            if !response.status().is_success() {
                errors += 1;
            }
        } else {
            errors += 1;
        }
    }

    let total_duration = start.elapsed();
    latencies.sort_unstable();

    // Measure the average number of SQL statements executed per request.
    // `pg_stat_statements()` counts every statement executed by this
    // backend, including those issued by the application pool.
    let statements: i64 = sqlx:scalar::query_scalar(
        "SELECT COALESCE(sum(statements), 0)::bigint FROM pg_stat_statements()",
    )
    .execute(&pool)
    .await
    .expect("read pg_stat_statements");
    let avg_db_round_trips = statements as f64 / TOTAL_REQUESTS as f64;

    LoadResult {
        p50: percentile(&latencies, 50.0),
        p95: percentile(&latencies, 95.0),
        p99: percentile(&latencies, 99.0),
        throughput: TOTAL_REQUESTS as f64 / total_duration.as_secs_f64(),
        errors,
        avg_db_round_trips,
    }
}

fn report_and_assert(
    endpoint: &str,
    path: &str,
    result: &LoadResult,
    p95_budget: Duration,
    db_budget: f64,
) {
    eprintln!("=== {} load test ===", endpoint);
    eprintln!("path:             {}", path);
    eprintln!("concurrency:       {}", CONCURRENT_REQUESTS);
    eprintln!("total_requests:     {}", TOTAL_REQUESTS);
    eprintln!("errors:           {}", result.errors);
    eprintln!("throughput:       {:.2} req/s", result.throughput);
    eprintln!("p5d_latency:     {:?}", result.p50);
    eprintln!("p95_latency:     {:?}", result.p95);
    eprintln!("p99_latency:     {:?}", result.p99);
    eprintln!("avg_db_round_trips: {:.2}", result.avg_db_round_trips);
    eprintln!("budget_p95:       {:?}", p95_budget);
    eprintln!("budget_db_rt:    {:.2}", db_budget);

    assert!(
        result.errors < TOTAL_REQUESTS / 2,
        "{} requests failed: {} / {}",
        endpoint,
        result.errors,
        TOTAL_REQUESTS,
    );
    assert!(
        result.p95 <= p95_budget,
        "{} p95 latency {:?} exceeds budget {:?} (concurrency={}, total_requests={})",
        endpoint,
        result.p95,
        p95_budget,
        CONCURRENT_REQUESTS,
        TOTAL_REQUESTS,
    );
    assert!(
        result.avg_db_round_trips <= db_budget,
        "{} avg db round-trips {:.2} exceeds budget {:.2} (concurrency={}, total_requests={})",
        endpoint,
        result.avg_db_round_trips,
        db_budget,
        CONCURRENT_REQUESTS,
        TOTAL_REQUESTS,
    );
}

/// Seeds a minimal pool and a prediction so the detail and history reads
/// have real rows to return.
async fn seed_data(pool: &sqlx::PgPool<sqlx::Postgres>) -> (i64, i64) {
    let user_id: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(id), 0)::bigint FROM users",
    )
    .execute(&pool)
    .await
    .expect("read users");

    let pool_id: i64 = sqlx::query_scalar(
        "INSERT INTO pools (name, description, creator_id, total_stake, status)
         VALUES ('load-test', 'load test pool', $1, 0, 'open')
         RETURNING id",
    )
    .bind(user_id)
    .execute(&pool)
    .await
    .expect("insert pool");

    let _prediction_id: i64 = sqlx::query_scalar(
        "INSERT INTO predictions (pool_id, user_id, amount, outcome)
         VALUES ($1, $2, 10, 'yes')
         RETURNING id",
    )
    .bind(pool_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .expect("insert prediction");

    (pool_id, user_id)
}

async fn setup_router() -> (axum::Router, sqlx::PgPool<sqlx::Postgres>, i64) {
    let container = testcontainers::runners::AsyncRunner::default()
        .run(testcontainers_modules::postgres::Postgres::default())
        .await
        .expect("postgres container");

    let port = container
        .get_host_port_ipv4(5432)
        .await
        .expect("postgres port");

    let database_url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(&database_url)
        .await
        .expect("connect to test postgres");

    sqlx:migrate::Migrator::new(std::path::Path::new("migrations"))
        .await
        .expect("load migrator")
        .run(&pool)
        .await
        .expect("apply migrations");

    let (pool_id, _user_id) = seed_data(&pool).await;

    let mut config = Config::default_for_test();
    config.database_url = database_url;
    config.redis_url = "redis://localhost:6379".to_string();

    let _metrics = Arc::new(Metrics::new().expect("metrics"));

    let router = build_router_with_db(
        config,
        PriceCache::new(),
        RedisCache::disabled(),
        pool.clone(),
        ws::EventBus::new(),
    );

    // Keep the container alive for the duration of the test by leaking it.
    // The container is released when the test process exits.
    std::mem::forget(container);

    (router, pool, pool_id)
}

#[tokio::test]
#[cfg(feature = "integration-tests")]
async fn load_test_pools_list_meets_budgets() {
    let (router, pool, _pool_id) = setup_router().await;
    let result = drive_load(router, "/v1/pools", &pool).await;
    report_and_assert(
        "pools_list",
        "/v1/pools",
        &result,
        POOLS_LIST_P95_BUDGET,
        POOLS_LIST_MAX_DB_ROUND_TRIPS,
    );
}

#[tokio::test]
#[cfg(feature = "integration-tests")]
async fn load_test_pool_detail_meets_budgets() {
    let (router, pool, pool_id) = setup_router().await;
    let path = format!("/v1/pools/{pool_id}");
    let result = drive_load(router, &path, &pool).await;
    report_and_assert(
        "pool_detail",
        &path,
        &result,
        POOL_DETAIL_P95_BUDGET,
        POOL_DETAIL_MAX_DB_ROUND_TRIPS,
    );
}

#[tokio::test]
#[cfg(feature = "integration-tests")]
async fn load_test_prediction_history_meets_budgets() {
    let (router, pool, pool_id) = setup_router().await;
    let path = format!("/v1/pools/{pool_id}/predictions");
    let result = drive_load(router, &path, &pool).await;
    report_and_assert(
        "prediction_history",
        &path,
        &result,
        PREDICTION_HISTORY_P95_BUDGET,
        PREDICTION_HISTORY_MAX_DB_ROUND_TRIPS,
    );
}
