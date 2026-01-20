use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    body::Bytes,
    response::{Html, IntoResponse, Redirect},
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
        .route("/", get(dashboard_index))
        .route("/links/:code", get(dashboard_link))
        .route("/health", get(|| async { "ok" }))
        .route("/api/shorten", rate_limited_shorten)
        .route("/api/links", get(list_links))
        .route("/:code", get(redirect))
        .route("/api/links/:code/qr", get(qr_png))
        .route("/api/links/:code/stats", get(stats))
        .with_state(state)
}

async fn dashboard_index(State(state): State<AppState>) -> Result<Html<String>, (StatusCode, String)> {
    let links = query_link_summaries(&state).await.map_err(internal)?;

    let mut rows = String::new();
    for l in links {
        let status = if l.expired { "expired" } else { "active" };
        rows.push_str(&format!(
            "<tr><td><a href=\"/links/{code}\">{code}</a></td><td class=\"mono\">{target}</td><td>{created}</td><td>{expires}</td><td>{status}</td><td>{clicks}</td><td>{uv}</td></tr>",
            code = html_escape(&l.code),
            target = html_escape(&l.target_url),
            created = html_escape(&l.created_at),
            expires = html_escape(l.expires_at.as_deref().unwrap_or("-")),
            status = status,
            clicks = l.total_clicks,
            uv = l.unique_visitors,
        ));
    }

    let page = layout(
        "URL Shortener Dashboard",
        &format!(
            r#"
<h1>URL Shortener</h1>

<div class="card">
  <h2>Create a short link</h2>
  <form id="shorten-form">
    <label>Long URL</label>
    <input name="url" placeholder="https://example.com/very/long" required />

    <label>Custom code (optional)</label>
    <input name="custom_code" placeholder="my-link" />

    <label>Expires at (optional, RFC3339)</label>
    <input name="expires_at" placeholder="2026-01-31T00:00:00Z" />

    <button type="submit">Shorten</button>
  </form>
  <div id="result" class="result"></div>
</div>

<div class="card">
  <h2>All links</h2>
  <table>
    <thead>
      <tr><th>Code</th><th>Target</th><th>Created</th><th>Expires</th><th>Status</th><th>Clicks</th><th>Unique</th></tr>
    </thead>
    <tbody>
      {rows}
    </tbody>
  </table>
</div>

<script>
  const form = document.getElementById('shorten-form');
  const result = document.getElementById('result');

  form.addEventListener('submit', async (e) => {{
    e.preventDefault();
    result.textContent = 'Working...';

    const data = Object.fromEntries(new FormData(form));
    if (!data.custom_code) delete data.custom_code;
    if (!data.expires_at) delete data.expires_at;

    const resp = await fetch('/api/shorten', {{
      method: 'POST',
      headers: {{ 'Content-Type': 'application/json' }},
      body: JSON.stringify(data)
    }});

    const text = await resp.text();
    if (!resp.ok) {{
      result.textContent = 'Error: ' + text;
      return;
    }}
    const json = JSON.parse(text);
    result.innerHTML = `Short URL: <a href="${{json.short_url}}" target="_blank">${{json.short_url}}</a>
      <br/>QR: <a href="${{json.qr_png_url}}" target="_blank">${{json.qr_png_url}}</a>`;
    form.reset();
  }});
</script>
"#,
            rows = rows
        ),
    );
    Ok(Html(page))
}

async fn dashboard_link(
    State(state): State<AppState>,
    Path(code): Path<String>,
) -> Result<Html<String>, (StatusCode, String)> {
    let stats = query_stats(&state, &code).await?;

    let mut countries = String::new();
    for c in &stats.top_countries {
        countries.push_str(&format!(
            "<li><span class=\"mono\">{country}</span> — {clicks}</li>",
            country = html_escape(&c.country),
            clicks = c.clicks
        ));
    }
    if countries.is_empty() {
        countries.push_str("<li>-</li>");
    }

    let mut recent = String::new();
    for r in &stats.recent_clicks {
        recent.push_str(&format!(
            "<tr><td>{at}</td><td class=\"mono\">{ip}</td><td>{country}</td><td class=\"mono\">{ua}</td></tr>",
            at = html_escape(&r.at),
            ip = html_escape(r.ip.as_deref().unwrap_or("-")),
            country = html_escape(r.country.as_deref().unwrap_or("-")),
            ua = html_escape(r.user_agent.as_deref().unwrap_or("-")),
        ));
    }
    if recent.is_empty() {
        recent.push_str("<tr><td colspan=\"4\">-</td></tr>");
    }

    let page = layout(
        &format!("Stats for {}", html_escape(&code)),
        &format!(
            r#"
<a href="/">← Back</a>

<h1>Link <span class="mono">/{code}</span></h1>

<div class="grid">
  <div class="card">
    <h2>Link</h2>
    <p><strong>Target</strong><br/><span class="mono">{target}</span></p>
    <p><strong>Short URL</strong><br/><a href="{short_url}" target="_blank">{short_url}</a></p>
    <p><strong>Created</strong><br/>{created}</p>
    <p><strong>Expires</strong><br/>{expires}</p>
  </div>

  <div class="card">
    <h2>QR</h2>
    <img class="qr" src="/api/links/{code}/qr" alt="QR code" />
  </div>

  <div class="card">
    <h2>Totals</h2>
    <p class="big">{clicks} clicks</p>
    <p class="big">{unique} unique visitors</p>
  </div>

  <div class="card">
    <h2>Top countries</h2>
    <ul>{countries}</ul>
  </div>
</div>

<div class="card">
  <h2>Recent clicks</h2>
  <table>
    <thead><tr><th>At</th><th>IP</th><th>Country</th><th>User-Agent</th></tr></thead>
    <tbody>{recent}</tbody>
  </table>
</div>
"#,
            code = html_escape(&stats.code),
            target = html_escape(&stats.target_url),
            short_url = html_escape(&format!("{}/{}", state.base_url, stats.code)),
            created = html_escape(&stats.created_at),
            expires = html_escape(stats.expires_at.as_deref().unwrap_or("-")),
            clicks = stats.total_clicks,
            unique = stats.unique_visitors,
            countries = countries,
            recent = recent,
        ),
    );
    Ok(Html(page))
}

fn layout(title: &str, body: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <title>{title}</title>
    <style>
      body {{ font-family: ui-sans-serif, system-ui, -apple-system, Segoe UI, Roboto, Arial; margin: 24px; line-height: 1.35; }}
      h1 {{ margin: 0 0 12px 0; }}
      h2 {{ margin: 0 0 12px 0; font-size: 18px; }}
      a {{ color: #0b62d6; }}
      table {{ width: 100%; border-collapse: collapse; }}
      th, td {{ border-bottom: 1px solid #ddd; padding: 8px; vertical-align: top; }}
      th {{ text-align: left; }}
      .card {{ border: 1px solid #e5e5e5; border-radius: 12px; padding: 16px; margin: 16px 0; }}
      .grid {{ display: grid; gap: 16px; grid-template-columns: repeat(auto-fit, minmax(260px, 1fr)); }}
      .mono {{ font-family: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, 'Liberation Mono', 'Courier New', monospace; }}
      input {{ width: 100%; padding: 10px; border: 1px solid #ccc; border-radius: 10px; margin-bottom: 10px; }}
      button {{ padding: 10px 14px; border-radius: 10px; border: 1px solid #0b62d6; background: #0b62d6; color: white; cursor: pointer; }}
      .result {{ margin-top: 10px; }}
      .big {{ font-size: 22px; margin: 8px 0; }}
      .qr {{ width: 240px; height: 240px; image-rendering: pixelated; }}
    </style>
  </head>
  <body>
    {body}
  </body>
</html>"#,
        title = title,
        body = body
    )
}

fn html_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
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

async fn query_link_summaries(state: &AppState) -> Result<Vec<LinkSummary>, sqlx::Error> {
    let rows: Vec<(String, String, String, Option<String>, i64, i64)> = sqlx::query_as(
        "SELECT u.code, u.target_url, u.created_at, u.expires_at, \
                count(c.id) as total_clicks, count(DISTINCT c.ip) as unique_visitors \
         FROM urls u LEFT JOIN clicks c ON c.code = u.code \
         GROUP BY u.code ORDER BY u.created_at DESC",
    )
    .fetch_all(&state.pool)
    .await?;

    Ok(rows
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
        .collect())
}

async fn list_links(
    State(state): State<AppState>,
) -> Result<Json<Vec<LinkSummary>>, (StatusCode, String)> {
    let out = query_link_summaries(&state).await.map_err(internal)?;
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

#[cfg(not(test))]
fn is_private_or_local_ip(ip: &str) -> bool {
    ip == "127.0.0.1"
        || ip == "::1"
        || ip.starts_with("10.")
        || ip.starts_with("192.168.")
        || ip.starts_with("172.16.")
        || ip.starts_with("172.17.")
        || ip.starts_with("172.18.")
        || ip.starts_with("172.19.")
        || ip.starts_with("172.2")
        || ip.starts_with("172.30.")
        || ip.starts_with("172.31.")
}

#[cfg(not(test))]
async fn geo_country_lookup(ip: &str) -> Option<String> {
    if is_private_or_local_ip(ip) {
        return None;
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .ok()?;

    let url = format!("https://ipapi.co/{}/country/", ip);
    let text = client
    .get(url)
    .header(reqwest::header::USER_AGENT, "url-shortener/1.0")
    .send()
    .await
    .ok()?
    .text()
    .await
    .ok()?;
    let code = text.trim();

    if code.len() == 2 {
        Some(code.to_string())
    } else {
        None
    }
}

#[cfg(test)]
async fn geo_country_lookup(_ip: &str) -> Option<String> {
    None
}

async fn country_from_headers_or_ip(headers: &HeaderMap) -> Option<String> {
    if let Some(c) = country_from_headers(headers) {
        return Some(c);
    }

    let ip = client_ip_from_headers(headers)?;
    geo_country_lookup(&ip).await
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

        let ip_opt = client_ip_from_headers(&headers);
        let ip = ip_opt.clone().unwrap_or_else(|| "local".to_string());

        let ua = headers
            .get(header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let referer = headers
            .get(header::REFERER)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        let country = country_from_headers_or_ip(&headers).await;

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
    let stats = query_stats(&state, &code).await?;
    Ok(Json(stats))
}

async fn query_stats(state: &AppState, code: &str) -> Result<StatsResp, (StatusCode, String)> {
    let url_row: Option<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT target_url, created_at, expires_at FROM urls WHERE code = ?",
    )
    .bind(code)
    .fetch_optional(&state.pool)
    .await
    .map_err(internal)?;

    let Some((target_url, created_at, expires_at)) = url_row else {
        return Err((StatusCode::NOT_FOUND, "not found".to_string()));
    };

    let total_clicks: (i64,) = sqlx::query_as("SELECT count(*) FROM clicks WHERE code = ?")
        .bind(code)
        .fetch_one(&state.pool)
        .await
        .map_err(internal)?;

    let unique_visitors: (i64,) = sqlx::query_as(
        "SELECT count(DISTINCT ip) FROM clicks WHERE code = ? AND ip IS NOT NULL",
    )
    .bind(code)
    .fetch_one(&state.pool)
    .await
    .map_err(internal)?;

    let daily_rows: Vec<(String, i64, i64)> = sqlx::query_as(
        "SELECT substr(at, 1, 10) as day, count(*) as clicks, count(DISTINCT ip) as unique_visitors \
         FROM clicks WHERE code = ? GROUP BY day ORDER BY day DESC LIMIT 30",
    )
    .bind(code)
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
    .bind(code)
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
        .bind(code)
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

    Ok(StatsResp {
        code: code.to_string(),
        target_url,
        created_at,
        expires_at,
        total_clicks: total_clicks.0,
        unique_visitors: unique_visitors.0,
        clicks_by_day,
        top_countries,
        recent_clicks,
    })
}

fn internal<E: std::fmt::Display>(e: E) -> (StatusCode, String) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("internal error: {}", e),
    )
}
