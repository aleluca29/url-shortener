use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use sqlx::{sqlite::SqlitePoolOptions, Pool, Sqlite};
use std::time::Duration;
use tower::ServiceExt;

use url_shortener::{router, AppState, RateLimiter};

async fn test_app() -> axum::Router {
    let pool: Pool<Sqlite> = SqlitePoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect("sqlite::memory:")
        .await
        .unwrap();

    sqlx::migrate!("./migrations").run(&pool).await.unwrap();

    let state = AppState {
        pool,
        base_url: "http://localhost:3000".to_string(),
        rate_limiter: RateLimiter::new(10, Duration::from_secs(60)),
    };

    router(state)
}

#[tokio::test]
async fn health_check_works() {
    let app = test_app().await;

    let response = app
        .oneshot(Request::builder().uri("/health").body(axum::body::Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], b"ok");
}
