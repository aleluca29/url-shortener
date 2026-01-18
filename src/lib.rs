use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    body::Bytes,
    response::{IntoResponse, Redirect},
    routing::{get, post},
    Json, Router,
};
use rand::{distributions::Alphanumeric, Rng};
use serde::{Deserialize, Serialize};
use sqlx::{Pool, Sqlite};
use std::{collections::HashMap, sync::Arc, time::Duration};
use std::io::Cursor;
use tokio::sync::Mutex;
use time::OffsetDateTime;

#[derive(Clone)]
pub struct AppState {
    pub pool: Pool<Sqlite>,
    pub base_url: String,
    pub rate_limiter: RateLimiter,
}

#[derive(Clone)]
pub struct RateLimiter {
    inner: Arc<Mutex<HashMap<String, Vec<std::time::Instant>>>>,
    limit: usize,
    window: Duration,
}

impl RateLimiter {
    pub fn new(limit: usize, window: Duration) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            limit,
            window,
        }
    }

    pub async fn allow(&self, key: &str) -> bool {
        let mut map = self.inner.lock().await;
        let now = std::time::Instant::now();
        let entry = map.entry(key.to_string()).or_default();
        entry.retain(|t| now.duration_since(*t) < self.window);
        if entry.len() >= self.limit {
            return false;
        }
        entry.push(now);
        true
    }
}

#[derive(Deserialize)]
struct ShortenReq {
    url: String,
    custom_code: Option<String>,
    expires_at: Option<String>,
}

#[derive(Serialize)]
struct ShortenResp {
    code: String,
    short_url: String,
    qr_png_url: String,
    expires_at: Option<String>,
}

fn gen_code() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .map(char::from)
        .take(7)
        .collect()
}

pub fn router(state: AppState) -> Router {
    let rate_limited_shorten = post(shorten)
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            rate_limit_middleware,
        ));

    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/api/shorten", rate_limited_shorten)
        .route("/api/links", get(list_links))
        .route("/:code", get(redirect))
        .route("/api/links/:code/qr", get(qr_png))
        .route("/api/links/:code/stats", get(stats))
        .with_state(state)
}

#[derive(Serialize)]
struct LinkSummary {
    code: String,
    target_url: String,
    created_at: String,
    expires_at: Option<String>,
    expired: bool,
    total_clicks: i64,
    unique_visitors: i64,
}

async fn list_links(State(state): State<AppState>) -> Result<Json<Vec<LinkSummary>>, (StatusCode, String)> {
    let rows: Vec<(String, String, String, Option<String>, i64, i64)> = sqlx::query_as(
        "SELECT u.code, u.target_url, u.created_at, u.expires_at, \
                count(c.id) as total_clicks, count(DISTINCT c.ip) as unique_visitors \
         FROM urls u LEFT JOIN clicks c ON c.code = u.code \
         GROUP BY u.code ORDER BY u.created_at DESC",
    )
    .fetch_all(&state.pool)
    .await
    .map_err(internal)?;

    let out = rows
        .into_iter()
        .map(|(code, target_url, created_at, expires_at, total_clicks, unique_visitors)| {
            let expired = is_expired(expires_at.as_deref());
            LinkSummary {
                code,
                target_url,
                created_at,
                expires_at,
                expired,
                total_clicks,
                unique_visitors,
            }
        })
        .collect();

    Ok(Json(out))
}

async fn rate_limit_middleware(
    State(state): State<AppState>,
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> impl IntoResponse {
    let headers = req.headers();
    let ip = client_ip_from_headers(headers).unwrap_or_else(|| "local".to_string());

    if !state.rate_limiter.allow(&ip).await {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "rate limit exceeded (10 requests/minute)".to_string(),
        )
            .into_response();
    }

    next.run(req).await
}

async fn shorten(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<ShortenReq>,
) -> Result<Json<ShortenResp>, (StatusCode, String)> {
    let target = normalize_url(&payload.url).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            "url must start with http:// or https://".to_string(),
        )
    })?;

    if let Some(exp) = &payload.expires_at {
        time::OffsetDateTime::parse(exp, &time::format_description::well_known::Rfc3339)
            .map_err(|_| {
                (
                    StatusCode::BAD_REQUEST,
                    "expires_at must be RFC3339 (e.g. 2026-01-31T00:00:00Z)".to_string(),
                )
            })?;
    }

    let ip = client_ip_from_headers(&headers);
    let ua = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let code = if let Some(custom) = payload.custom_code.as_deref() {
        validate_custom_code(custom).map_err(|msg| (StatusCode::BAD_REQUEST, msg))?;
        insert_url(
            &state,
            custom,
            &target,
            payload.expires_at.as_deref(),
            ip.as_deref(),
            ua.as_deref(),
        )
        .await
        .map_err(|e| match e {
            InsertUrlError::CodeTaken => (StatusCode::CONFLICT, "code already exists".to_string()),
            InsertUrlError::Other(e) => internal(e),
        })?;
        custom.to_string()
    } else {
        const MAX_ATTEMPTS: usize = 8;
        let mut last_err: Option<anyhow::Error> = None;
        let mut code: Option<String> = None;
        for _ in 0..MAX_ATTEMPTS {
            let candidate = gen_code();
            match insert_url(
                &state,
                &candidate,
                &target,
                payload.expires_at.as_deref(),
                ip.as_deref(),
                ua.as_deref(),
            )
            .await
            {
                Ok(()) => {
                    code = Some(candidate);
                    break;
                }
                Err(InsertUrlError::CodeTaken) => continue,
                Err(InsertUrlError::Other(e)) => {
                    last_err = Some(e);
                    break;
                }
            }
        }
        code.ok_or_else(|| {
            internal(last_err.unwrap_or_else(|| anyhow::anyhow!("failed to generate code")))
        })?
    };

    let short_url = format!("{}/{}", state.base_url, code);
    Ok(Json(ShortenResp {
        qr_png_url: format!("{}/api/links/{}/qr", state.base_url, code),
        code: code.clone(),
        short_url,
        expires_at: payload.expires_at,
    }))
}

async fn qr_png(State(state): State<AppState>, Path(code): Path<String>) -> impl IntoResponse {
    let exists: Option<(i64,)> = sqlx::query_as("SELECT 1 FROM urls WHERE code = ?")
        .bind(&code)
        .fetch_optional(&state.pool)
        .await
        .unwrap();

    if exists.is_none() {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }

    let short_url = format!("{}/{}", state.base_url, code);

    let qr = match qrcode::QrCode::new(short_url.as_bytes()) {
        Ok(qr) => qr,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "qr error").into_response(),
    };

    let img = qr.render::<image::Luma<u8>>().min_dimensions(256, 256).build();
    let mut png_bytes = Vec::new();
    if image::DynamicImage::ImageLuma8(img)
        .write_to(&mut Cursor::new(&mut png_bytes), image::ImageFormat::Png)
        .is_err()
    {
        return (StatusCode::INTERNAL_SERVER_ERROR, "qr encode error").into_response();
    }

    (
        [(header::CONTENT_TYPE, "image/png")],
        Bytes::from(png_bytes),
    )
        .into_response()
}

#[derive(Debug)]
enum InsertUrlError {
    CodeTaken,
    Other(anyhow::Error),
}

async fn insert_url(
    state: &AppState,
    code: &str,
    target_url: &str,
    expires_at: Option<&str>,
    created_ip: Option<&str>,
    created_user_agent: Option<&str>,
) -> Result<(), InsertUrlError> {
    let created_at = OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();

    let res = sqlx::query(
        "INSERT INTO urls (code, target_url, created_at, expires_at, created_ip, created_user_agent) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(code)
    .bind(target_url)
    .bind(created_at)
    .bind(expires_at)
    .bind(created_ip)
    .bind(created_user_agent)
    .execute(&state.pool)
    .await;

    match res {
        Ok(_) => Ok(()),
        Err(e) if is_unique_violation(&e) => Err(InsertUrlError::CodeTaken),
        Err(e) => Err(InsertUrlError::Other(anyhow::Error::new(e))),
    }
}

fn is_unique_violation(e: &sqlx::Error) -> bool {
    match e {
        sqlx::Error::Database(db) => db.is_unique_violation(),
        _ => false,
    }
}

fn validate_custom_code(code: &str) -> Result<(), String> {
    if !(3..=32).contains(&code.len()) {
        return Err("custom_code must be 3-32 characters".to_string());
    }
    if !code
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("custom_code must be alphanumeric (plus - and _)".to_string());
    }
    Ok(())
}

fn normalize_url(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        Some(trimmed.to_string())
    } else {
        None
    }
}

fn client_ip_from_headers(headers: &HeaderMap) -> Option<String> {
    if let Some(v) = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
    {
        let first = v.split(',').next().map(|s| s.trim()).filter(|s| !s.is_empty());
        if let Some(ip) = first {
            return Some(ip.to_string());
        }
    }
    None
}

async fn redirect(
    State(state): State<AppState>,
    Path(code): Path<String>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let row: Option<(String, Option<String>)> =
        sqlx::query_as("SELECT target_url, expires_at FROM urls WHERE code = ?")
        .bind(&code)
        .fetch_optional(&state.pool)
        .await
        .unwrap();

    if let Some((target, expires_at)) = row {
        if is_expired(expires_at.as_deref()) {
            return (StatusCode::GONE, "This link has expired").into_response();
        }

        let ip = client_ip_from_headers(&headers).unwrap_or_else(|| "local".to_string());
        let ua = headers
            .get(header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let referer = headers
            .get(header::REFERER)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let country = country_from_headers(&headers);
        let city = headers
            .get("x-geo-city")
            .or_else(|| headers.get("cf-ipcity"))
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        let now = OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        let _ = sqlx::query(
            "INSERT INTO clicks (code, at, ip, user_agent, referer, country, city) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
            .bind(&code)
            .bind(now)
            .bind(ip)
            .bind(ua)
            .bind(referer)
            .bind(country)
            .bind(city)
            .execute(&state.pool)
            .await;
        Redirect::temporary(&target).into_response()
    } else {
        (StatusCode::NOT_FOUND, "Not found").into_response()
    }
}

fn is_expired(expires_at: Option<&str>) -> bool {
    let Some(exp) = expires_at else { return false };
    let Ok(exp) = OffsetDateTime::parse(exp, &time::format_description::well_known::Rfc3339) else {
        return true;
    };
    OffsetDateTime::now_utc() >= exp
}

fn country_from_headers(headers: &HeaderMap) -> Option<String> {
    let candidates = ["cf-ipcountry", "x-geo-country", "x-country"];
    for key in candidates {
        if let Some(v) = headers.get(key).and_then(|v| v.to_str().ok()) {
            let trimmed = v.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

#[derive(Serialize)]
struct StatsResp {
    code: String,
    target_url: String,
    created_at: String,
    expires_at: Option<String>,

    total_clicks: i64,
    unique_visitors: i64,
    clicks_by_day: Vec<DailyStats>,
    top_countries: Vec<CountryStat>,
    recent_clicks: Vec<RecentClick>,
}

#[derive(Serialize)]
struct DailyStats {
    day: String,
    clicks: i64,
    unique_visitors: i64,
}

#[derive(Serialize)]
struct CountryStat {
    country: String,
    clicks: i64,
}

#[derive(Serialize)]
struct RecentClick {
    at: String,
    ip: Option<String>,
    country: Option<String>,
    user_agent: Option<String>,
    referer: Option<String>,
}

async fn stats(
    State(state): State<AppState>,
    Path(code): Path<String>,
) -> Result<Json<StatsResp>, (StatusCode, String)> {
    let url_row: Option<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT target_url, created_at, expires_at FROM urls WHERE code = ?",
    )
    .bind(&code)
    .fetch_optional(&state.pool)
    .await
    .map_err(internal)?;

    let Some((target_url, created_at, expires_at)) = url_row else {
        return Err((StatusCode::NOT_FOUND, "not found".to_string()));
    };

    let total_clicks: (i64,) = sqlx::query_as("SELECT count(*) FROM clicks WHERE code = ?")
        .bind(&code)
        .fetch_one(&state.pool)
        .await
        .map_err(internal)?;

    let unique_visitors: (i64,) = sqlx::query_as(
        "SELECT count(DISTINCT ip) FROM clicks WHERE code = ? AND ip IS NOT NULL",
    )
    .bind(&code)
    .fetch_one(&state.pool)
    .await
    .map_err(internal)?;

    let daily_rows: Vec<(String, i64, i64)> = sqlx::query_as(
        "SELECT substr(at, 1, 10) as day, count(*) as clicks, count(DISTINCT ip) as unique_visitors \
         FROM clicks WHERE code = ? GROUP BY day ORDER BY day DESC LIMIT 30",
    )
    .bind(&code)
    .fetch_all(&state.pool)
    .await
    .map_err(internal)?;

    let clicks_by_day = daily_rows
        .into_iter()
        .map(|(day, clicks, unique_visitors)| DailyStats {
            day,
            clicks,
            unique_visitors,
        })
        .collect();

    let country_rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT country, count(*) as clicks FROM clicks \
         WHERE code = ? AND country IS NOT NULL \
         GROUP BY country ORDER BY clicks DESC LIMIT 10",
    )
    .bind(&code)
    .fetch_all(&state.pool)
    .await
    .map_err(internal)?;

    let top_countries = country_rows
        .into_iter()
        .map(|(country, clicks)| CountryStat { country, clicks })
        .collect();

    let recent_rows: Vec<(String, Option<String>, Option<String>, Option<String>, Option<String>)> =
        sqlx::query_as(
            "SELECT at, ip, country, user_agent, referer \
             FROM clicks WHERE code = ? ORDER BY at DESC LIMIT 25",
        )
        .bind(&code)
        .fetch_all(&state.pool)
        .await
        .map_err(internal)?;

    let recent_clicks = recent_rows
        .into_iter()
        .map(|(at, ip, country, user_agent, referer)| RecentClick {
            at,
            ip,
            country,
            user_agent,
            referer,
        })
        .collect();

    Ok(Json(StatsResp {
        code,
        target_url,
        created_at,
        expires_at,
        total_clicks: total_clicks.0,
        unique_visitors: unique_visitors.0,
        clicks_by_day,
        top_countries,
        recent_clicks,
    }))
}

fn internal<E: std::fmt::Display>(e: E) -> (StatusCode, String) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("internal error: {}", e),
    )
}
