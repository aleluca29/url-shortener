use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Redirect},
    routing::{get, post},
    Json, Router,
};
use rand::{distributions::Alphanumeric, Rng};
use serde::{Deserialize, Serialize};
use sqlx::{Pool, Sqlite};
use time::OffsetDateTime;

#[derive(Clone)]
pub struct AppState {
    pub pool: Pool<Sqlite>,
    pub base_url: String,
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
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/api/shorten", post(shorten))
        .route("/:code", get(redirect))
        .route("/api/links/:code/stats", get(stats))
        .with_state(state)
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
        code,
        short_url,
        expires_at: payload.expires_at,
    }))
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
    total_clicks: i64,
}

async fn stats(
    State(state): State<AppState>,
    Path(code): Path<String>,
) -> Result<Json<StatsResp>, (StatusCode, String)> {
    let row: (i64,) = sqlx::query_as("SELECT count(*) FROM clicks WHERE code = ?")
        .bind(&code)
        .fetch_one(&state.pool)
        .await
        .map_err(internal)?;
    Ok(Json(StatsResp { total_clicks: row.0 }))
}

fn internal<E: std::fmt::Display>(e: E) -> (StatusCode, String) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("internal error: {}", e),
    )
}
