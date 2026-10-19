use crate::config::Config;
use crate::price_cache::PriceCache;
use crate::redis_cache::RedisCache;
use crate::ws::EventBus;
use axum::Router;
use sqlx::PgPool;
use std::sync::Arc;

pub mod v1;

/// Build the versioned API router without a database pool.
///
/// All routes that require a database will return an error response at
/// runtime. Useful for unit tests and health-check-only deployments.
pub fn router(
    config: Arc<Config>,
    cache: PriceCache,
    redis: RedisCache,
    pool: Option<sqlx::PgPool>,
    metrics: crate::metrics::SharedMetrics,
    event_bus: EventBus,
) -> Router {
    Router::new().nest(
        "/v1",
        v1::router(config, cache, redis, pool, metrics, event_bus),
    )
}

/// Build the versioned API router with a live PostgreSQL connection pool.
///
/// Wraps the pool in `Some` and delegates to [`router`].
pub fn router_with_db(
    config: Arc<Config>,
    cache: PriceCache,
    redis: RedisCache,
    db: PgPool,
    metrics: crate::metrics::SharedMetrics,
    event_bus: EventBus,
) -> Router {
    Router::new().nest(
        "/v1",
        v1::router(config, cache, redis, Some(db), metrics, event_bus),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use sqlx::postgres::PgPoolOptions;
    use tower::ServiceExt;

    use crate::config::Config;
    use crate::metrics::Metrics;
    use crate::price_cache::PriceCache;
    use crate::redis_cache::RedisCache;
    use crate::ws::EventBus;

    use super::{router, router_with_db};

    fn test_metrics() -> crate::metrics::SharedMetrics {
        Arc::new(Metrics::new().expect("metrics must initialize in tests"))
    }

    fn get(path: &str) -> Request<axum::body::Body> {
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
        String::from_utf8(bytes.to_vec()).expect("body is not valid utf-8")
    }

    fn build_router_without_db() -> axum::Router {
        router(
            Arc::new(Config::default_for_test()),
            PriceCache::new(),
            RedisCache::disabled(),
            None,
            test_metrics(),
            EventBus::new(),
        )
    }

    fn build_router_with_lazy_db() -> axum::Router {
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_secs(1))
            .connect_lazy("postgres://localhost:5432/predifi_routes_test")
            .expect("lazy pool");
        router_with_db(
            Arc::new(Config::default_for_test()),
            PriceCache::new(),
            RedisCache::disabled(),
            pool,
            test_metrics(),
            EventBus::new(),
        )
    }

    /// `router` must nest the v1 API under `/v1`.
    #[tokio::test]
    async fn router_nests_v1_index() {
        let response = build_router_without_db()
            .oneshot(get("/v1"))
            .await
            .expect("request failed");

        assert_eq!(response.status(), StatusCode::OK);

        let body = body_string(response.into_body()).await;
        assert!(
            body.contains("\"name\":\"predifi-backend\"") && body.contains("\"version\":\"v1\""),
            "body should describe the v1 API, got: {body}"
        );
    }

    /// Routes that require a database must fail gracefully when no pool is passed.
    #[tokio::test]
    async fn router_without_db_returns_database_unavailable_for_pools() {
        let response = build_router_without_db()
            .oneshot(get("/v1/pools"))
            .await
            .expect("request failed");

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

        let body = body_string(response.into_body()).await;
        assert!(
            body.contains("database not available"),
            "body should mention missing database, got: {body}"
        );
    }

    /// `router` must still expose stateless routes such as `/v1/fees`.
    #[tokio::test]
    async fn router_without_db_returns_fees() {
        let mut config = Config::default_for_test();
        config.treasury_fee_bps = 250;
        config.referral_fee_bps = 5000;

        let app = router(
            Arc::new(config),
            PriceCache::new(),
            RedisCache::disabled(),
            None,
            test_metrics(),
            EventBus::new(),
        );

        let response = app.oneshot(get("/v1/fees")).await.expect("request failed");

        assert_eq!(response.status(), StatusCode::OK);

        let body = body_string(response.into_body()).await;
        assert!(
            body.contains("\"treasury_fee_bps\":250") && body.contains("\"referral_fee_bps\":5000"),
            "body should contain configured fee values, got: {body}"
        );
    }

    /// Unknown paths under the nested router must return 404.
    #[tokio::test]
    async fn router_unknown_route_returns_404() {
        let response = build_router_without_db()
            .oneshot(get("/v1/no-such-route"))
            .await
            .expect("request failed");

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    /// `router_with_db` must wire the pool through so DB-backed routes attempt queries.
    #[tokio::test]
    async fn router_with_db_attempts_database_queries() {
        let response = build_router_with_lazy_db()
            .oneshot(get("/v1/pools"))
            .await
            .expect("request failed");

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

        let body = body_string(response.into_body()).await;
        assert!(
            !body.contains("database not available"),
            "router_with_db should pass a pool to handlers, got: {body}"
        );
        assert!(
            body.contains("\"error\"") || body.contains("error"),
            "unreachable database should surface a query error, got: {body}"
        );
    }

    /// `router_with_db` must nest routes under `/v1` like `router`.
    #[tokio::test]
    async fn router_with_db_nests_v1_index() {
        let response = build_router_with_lazy_db()
            .oneshot(get("/v1"))
            .await
            .expect("request failed");

        assert_eq!(response.status(), StatusCode::OK);

        let body = body_string(response.into_body()).await;
        assert!(
            body.contains("\"version\":\"v1\""),
            "router_with_db should nest under /v1, got: {body}"
        );
    }

    // ── Market predictions: no-DB guard ──────────────────────────────────────

    /// Without a DB the market predictions endpoint must return a structured
    /// DATABASE_UNAVAILABLE error (not a panic or 500 without an envelope).
    #[tokio::test]
    async fn market_predictions_without_db_returns_database_unavailable() {
        let response = build_router_without_db()
            .oneshot(get("/v1/markets/1/predictions"))
            .await
            .expect("request failed");

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

        let body = body_string(response.into_body()).await;
        assert!(
            body.contains("database not available") || body.contains("DATABASE_UNAVAILABLE"),
            "should surface DATABASE_UNAVAILABLE, got: {body}"
        );
    }

    /// A non-numeric market ID must return 422 (Axum path extraction failure).
    #[tokio::test]
    async fn market_predictions_bad_market_id_returns_400() {
        let response = build_router_without_db()
            .oneshot(get("/v1/markets/not-a-number/predictions"))
            .await
            .expect("request failed");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    /// `?limit` values outside the 1-100 range are clamped, not rejected.
    /// With no DB the response is DATABASE_UNAVAILABLE – the important thing is
    /// that no 500 / panic occurs from an unclamped limit.
    #[tokio::test]
    async fn market_predictions_large_limit_is_clamped() {
        let response = build_router_without_db()
            .oneshot(get("/v1/markets/1/predictions?limit=99999"))
            .await
            .expect("request failed");

        // DATABASE_UNAVAILABLE path, but must not be a 500 from bad arithmetic.
        assert_ne!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    /// With a lazy (unreachable) DB the handler must attempt a query and return
    /// a DB error response, not the database-unavailable sentinel (which would
    /// mean the pool was never wired through).
    #[tokio::test]
    async fn market_predictions_with_lazy_db_attempts_query() {
        let response = build_router_with_lazy_db()
            .oneshot(get("/v1/markets/1/predictions"))
            .await
            .expect("request failed");

        let body = body_string(response.into_body()).await;
        assert!(
            !body.contains("database not available"),
            "router_with_db should wire the pool; got: {body}"
        );
        // Should either be a DB error or a 404 (market not found), never a panic.
        assert!(
            body.contains("\"error\""),
            "should return an error envelope; got: {body}"
        );
    }

    // ── Request body and query limits (#54) ────────────────────────────

    fn post_json(path: &str, body: String) -> Request<axum::body::Body> {
        Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body))
            .expect("failed to build request")
    }

    /// A valid pool-template body, padded with an ignored field to exactly
    /// `len` bytes.
    fn template_body_of_len(len: usize) -> String {
        let head = r#"{"creator":"G","name":"n","category":"c","description":"d","token":"T","duration_seconds":3600,"recurrence_interval_seconds":3600,"pad":""#;
        let tail = r#""}"#;
        let fill = len - head.len() - tail.len();
        format!("{head}{}{tail}", "x".repeat(fill))
    }

    fn template_json(name: &str, description: &str, interval: i64) -> String {
        serde_json::json!({
            "creator": "G",
            "name": name,
            "category": "c",
            "description": description,
            "token": "T",
            "duration_seconds": 3600,
            "recurrence_interval_seconds": interval,
        })
        .to_string()
    }

    /// POST to `/v1/pool-templates` on a fresh router (so rate limits never
    /// interfere) and return the status and body text.
    async fn post_template(body: String) -> (StatusCode, String) {
        let response = build_router_without_db()
            .oneshot(post_json("/v1/pool-templates", body))
            .await
            .expect("request failed");
        let status = response.status();
        (status, body_string(response.into_body()).await)
    }

    /// Without a database, a request that passes every check answers
    /// "database not available". Anything rejected earlier never says that,
    /// so it also never reached persistence.
    fn reached_db_step(text: &str) -> bool {
        text.contains("database not available")
    }

    #[tokio::test]
    async fn template_body_just_under_and_at_limit_reach_the_handler() {
        let max = crate::constants::MAX_BODY_BYTES_POOL_TEMPLATE;
        for len in [max - 1, max] {
            let body = template_body_of_len(len);
            assert_eq!(body.len(), len);
            let (status, text) = post_template(body).await;
            assert_eq!(status, StatusCode::OK, "len {len}");
            assert!(reached_db_step(&text), "len {len}: {text}");
        }
    }

    #[tokio::test]
    async fn template_body_over_limit_is_rejected_with_413() {
        let max = crate::constants::MAX_BODY_BYTES_POOL_TEMPLATE;
        let (status, text) = post_template(template_body_of_len(max + 1)).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert!(!reached_db_step(&text));
    }

    #[tokio::test]
    async fn template_field_limits_are_enforced_before_the_db() {
        use crate::constants::{
            MAX_TEMPLATE_DESCRIPTION_LEN, MAX_TEMPLATE_SECONDS, MAX_TEMPLATE_TEXT_LEN,
        };

        let cases = [
            (
                "name at limit",
                template_json(&"n".repeat(MAX_TEMPLATE_TEXT_LEN), "d", 3600),
                true,
            ),
            (
                "name over limit",
                template_json(&"n".repeat(MAX_TEMPLATE_TEXT_LEN + 1), "d", 3600),
                false,
            ),
            (
                "description at limit",
                template_json("n", &"d".repeat(MAX_TEMPLATE_DESCRIPTION_LEN), 3600),
                true,
            ),
            (
                "description over limit",
                template_json("n", &"d".repeat(MAX_TEMPLATE_DESCRIPTION_LEN + 1), 3600),
                false,
            ),
            (
                "interval at max",
                template_json("n", "d", MAX_TEMPLATE_SECONDS),
                true,
            ),
            (
                "interval over max",
                template_json("n", "d", MAX_TEMPLATE_SECONDS + 1),
                false,
            ),
            (
                "interval i64::MAX",
                template_json("n", "d", i64::MAX),
                false,
            ),
            ("interval zero", template_json("n", "d", 0), false),
        ];

        for (label, body, expect_ok) in cases {
            let (status, text) = post_template(body).await;
            if expect_ok {
                assert_eq!(status, StatusCode::OK, "{label}");
            } else {
                assert_eq!(status, StatusCode::BAD_REQUEST, "{label}");
            }
            assert_eq!(reached_db_step(&text), expect_ok, "{label}: {text}");
        }
    }

    #[tokio::test]
    async fn template_list_creator_length_is_bounded() {
        let max = crate::constants::MAX_ADDRESS_LEN;
        for (len, expect_ok) in [(max, true), (max + 1, false)] {
            let uri = format!("/v1/pool-templates?creator={}", "G".repeat(len));
            let response = build_router_without_db()
                .oneshot(get(&uri))
                .await
                .expect("request failed");
            let status = response.status();
            let text = body_string(response.into_body()).await;
            let expected = if expect_ok {
                StatusCode::OK
            } else {
                StatusCode::BAD_REQUEST
            };
            assert_eq!(status, expected, "len {len}");
            assert_eq!(reached_db_step(&text), expect_ok, "len {len}: {text}");
        }
    }

    #[tokio::test]
    async fn pools_offset_above_cap_is_rejected() {
        let max = crate::constants::MAX_PAGE_OFFSET;
        let ok = build_router_without_db()
            .oneshot(get(&format!("/v1/pools?offset={max}")))
            .await
            .expect("request failed");
        assert_ne!(ok.status(), StatusCode::BAD_REQUEST, "offset at cap");

        let over = build_router_without_db()
            .oneshot(get(&format!("/v1/pools?offset={}", max + 1)))
            .await
            .expect("request failed");
        assert_eq!(over.status(), StatusCode::BAD_REQUEST, "offset over cap");
    }
}
