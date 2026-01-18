use axum::http::{header, Request, StatusCode};
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

async fn req(
    app: axum::Router,
    method: &str,
    uri: &str,
    headers: Vec<(&str, &str)>,
    body: Option<String>,
) -> axum::response::Response {
    let mut builder = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        builder = builder.header(k, v);
    }
    let body = body.unwrap_or_default();
    app.oneshot(builder.body(axum::body::Body::from(body)).unwrap())
        .await
        .unwrap()
}

async fn body_string(response: axum::response::Response) -> (StatusCode, String, axum::http::HeaderMap) {
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).to_string(), headers)
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

#[tokio::test]
async fn can_shorten_and_redirect_and_see_stats() {
    let app = test_app().await;

    let payload = serde_json::json!({"url": "https://example.com/hello"}).to_string();
    let resp = req(
        app.clone(),
        "POST",
        "/api/shorten",
        vec![(header::CONTENT_TYPE.as_str(), "application/json"), ("x-forwarded-for", "1.2.3.4")],
        Some(payload),
    )
    .await;
    let (status, body, _) = body_string(resp).await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    let code = json["code"].as_str().unwrap().to_string();
    assert!((6..=8).contains(&code.len()));

    let resp = req(
        app.clone(),
        "GET",
        &format!("/{code}"),
        vec![("x-forwarded-for", "1.2.3.4"), ("cf-ipcountry", "RO")],
        None,
    )
    .await;
    assert!(resp.status().is_redirection());
    assert_eq!(
        resp.headers().get(header::LOCATION).unwrap().to_str().unwrap(),
        "https://example.com/hello"
    );

    let resp = req(
        app.clone(),
        "GET",
        &format!("/api/links/{code}/stats"),
        vec![],
        None,
    )
    .await;
    let (status, body, _) = body_string(resp).await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["total_clicks"].as_i64().unwrap(), 1);
    assert_eq!(json["unique_visitors"].as_i64().unwrap(), 1);
    let countries = json["top_countries"].as_array().unwrap();
    assert!(countries.iter().any(|c| c["country"] == "RO"));
}

#[tokio::test]
async fn custom_code_conflicts_return_409() {
    let app = test_app().await;

    let payload = serde_json::json!({"url": "https://example.com/a", "custom_code": "mycode"}).to_string();
    let resp = req(
        app.clone(),
        "POST",
        "/api/shorten",
        vec![(header::CONTENT_TYPE.as_str(), "application/json"), ("x-forwarded-for", "9.9.9.9")],
        Some(payload),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let payload = serde_json::json!({"url": "https://example.com/b", "custom_code": "mycode"}).to_string();
    let resp = req(
        app.clone(),
        "POST",
        "/api/shorten",
        vec![(header::CONTENT_TYPE.as_str(), "application/json"), ("x-forwarded-for", "9.9.9.9")],
        Some(payload),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn expired_links_return_410() {
    let app = test_app().await;

    let payload = serde_json::json!({
        "url": "https://example.com/x",
        "custom_code": "exp",
        "expires_at": "2000-01-01T00:00:00Z"
    })
    .to_string();

    let resp = req(
        app.clone(),
        "POST",
        "/api/shorten",
        vec![(header::CONTENT_TYPE.as_str(), "application/json"), ("x-forwarded-for", "2.2.2.2")],
        Some(payload),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = req(app.clone(), "GET", "/exp", vec![], None).await;
    assert_eq!(resp.status(), StatusCode::GONE);
}

#[tokio::test]
async fn qr_endpoint_returns_png() {
    let app = test_app().await;

    let payload = serde_json::json!({"url": "https://example.com/qr", "custom_code": "qr1"}).to_string();
    let resp = req(
        app.clone(),
        "POST",
        "/api/shorten",
        vec![(header::CONTENT_TYPE.as_str(), "application/json"), ("x-forwarded-for", "3.3.3.3")],
        Some(payload),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = req(app.clone(), "GET", "/api/links/qr1/qr", vec![], None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::CONTENT_TYPE).unwrap().to_str().unwrap(),
        "image/png"
    );
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    assert!(bytes.len() > 100);
}

#[tokio::test]
async fn rate_limit_trips_after_10_requests() {
    let app = test_app().await;

    for i in 0..10 {
        let payload = serde_json::json!({"url": format!("https://example.com/{i}")}).to_string();
        let resp = req(
            app.clone(),
            "POST",
            "/api/shorten",
            vec![(header::CONTENT_TYPE.as_str(), "application/json"), ("x-forwarded-for", "4.4.4.4")],
            Some(payload),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    let payload = serde_json::json!({"url": "https://example.com/overflow"}).to_string();
    let resp = req(
        app.clone(),
        "POST",
        "/api/shorten",
        vec![(header::CONTENT_TYPE.as_str(), "application/json"), ("x-forwarded-for", "4.4.4.4")],
        Some(payload),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
}
