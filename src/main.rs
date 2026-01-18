use sqlx::{sqlite::SqlitePoolOptions, Pool, Sqlite};
use std::{net::SocketAddr, time::Duration};
use tower_http::trace::TraceLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use url_shortener::{router, AppState, RateLimiter};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            tracing_subscriber::EnvFilter::new("info")
        }))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let db_url = std::env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite://dev.db".to_string());
    let base_url = std::env::var("BASE_URL").unwrap_or_else(|_| "http://localhost:3000".to_string());
    let listen = std::env::var("LISTEN_ADDR").unwrap_or_else(|_| "127.0.0.1:3000".to_string());
    let pool: Pool<Sqlite> = SqlitePoolOptions::new()
        .acquire_timeout(Duration::from_secs(5))
        .max_connections(5)
        .connect(&db_url)
        .await?;

    // run migrations
    sqlx::migrate!("./migrations").run(&pool).await?;

    // shared state
    let state = AppState {
        pool,
        base_url,
        rate_limiter: RateLimiter::new(10, Duration::from_secs(60)),
    };

    let app = router(state).layer(TraceLayer::new_for_http());

    let addr: SocketAddr = listen.parse()?;
    tracing::info!("listening on {}", addr);

    axum::serve(tokio::net::TcpListener::bind(addr).await?, app)
        .await
        .unwrap();

    Ok(())
}
