use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Redirect},
    routing::{get, post},
    Json, Router,
};
use rand::{distributions::Alphanumeric, Rng};
use serde::{Deserialize, Serialize};
use sqlx::{sqlite::SqlitePoolOptions, Pool, Sqlite};
use std::{net::SocketAddr, time::Duration};
use time::OffsetDateTime;
use tower_http::trace::TraceLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Clone)]
struct AppState {
    pool: Pool<Sqlite>,
    base_url: String,
}

#[derive(Deserialize)]
struct ShortenReq {
    url: String,
}

#[derive(Serialize)]
struct ShortenResp {
    code: String,
    short_url: String,
}

fn gen_code() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .map(char::from)
        .take(7)
        .collect()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {

    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new("info"))
        .with(tracing_subscriber::fmt::layer())
        .init();


    let db_url = "sqlite://dev.db";
    let pool = SqlitePoolOptions::new()
        .acquire_timeout(Duration::from_secs(5))
        .max_connections(5)
        .connect(db_url)
        .await?;

    // run migrations
    sqlx::migrate!("./migrations").run(&pool).await?;

    // shared state
    let state = AppState {
        pool,
        base_url: "http://localhost:3000".to_string(),
    };


    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/api/shorten", post(shorten))
        .route("/:code", get(redirect))
        .route("/api/links/:code/stats", get(stats))
        .with_state(state)
        .layer(TraceLayer::new_for_http());

    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    tracing::info!("listening on {}", addr);

    axum::serve(tokio::net::TcpListener::bind(addr).await?, app)
        .await
        .unwrap();

    Ok(())
}

async fn shorten(
    State(state): State<AppState>,
    Json(payload): Json<ShortenReq>,
) -> Result<Json<ShortenResp>, (StatusCode, String)> {
    let code = gen_code();
    let now = OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();

    sqlx::query("INSERT INTO urls (code, target_url, created_at) VALUES (?, ?, ?)")
        .bind(&code)
        .bind(&payload.url)
        .bind(now)
        .execute(&state.pool)
        .await
        .map_err(internal)?;

    let short_url = format!("{}/{}", state.base_url, code);
    Ok(Json(ShortenResp { code, short_url }))
}

async fn redirect(State(state): State<AppState>, Path(code): Path<String>) -> impl IntoResponse {
    let row: Option<(String,)> = sqlx::query_as("SELECT target_url FROM urls WHERE code = ?")
        .bind(&code)
        .fetch_optional(&state.pool)
        .await
        .unwrap();

    if let Some((target,)) = row {
        let now = OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        let _ = sqlx::query("INSERT INTO clicks (code, at, ip) VALUES (?, ?, ?)")
            .bind(&code)
            .bind(now)
            .bind("local")
            .execute(&state.pool)
            .await;
        Redirect::temporary(&target).into_response()
    } else {
        (StatusCode::NOT_FOUND, "Not found").into_response()
    }
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
    (StatusCode::INTERNAL_SERVER_ERROR, format!("internal error: {}", e))
}
