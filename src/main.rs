mod cache;
mod proxy;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::routing::get;
use axum::{extract::State, Json, Router};
use clap::Parser;
use serde::Serialize;
use tokio::time::Instant;

use cache::ProxyCache;

#[derive(Parser)]
#[command(name = "solcast-proxy", about = "Caching reverse proxy for Solcast API")]
struct Cli {
    /// Listen port
    #[arg(short, long, default_value = "8888")]
    port: u16,

    /// Cache directory
    #[arg(short, long, default_value = "./data")]
    cache_dir: PathBuf,

    /// Cache TTL in seconds
    #[arg(long, default_value = "7200")]
    ttl: u64,

    /// Minimum seconds between upstream calls per endpoint
    #[arg(long, default_value = "9000")]
    rate_limit: u64,
}

pub struct AppState {
    pub cache: ProxyCache,
    pub upstream_url: String,
    pub client: reqwest::Client,
    pub start_time: Instant,
    pub ttl: u64,
    pub rate_limit: u64,
}

#[derive(Serialize)]
struct HealthResponse {
    status: String,
    cache_entries: usize,
    uptime_secs: u64,
}

async fn health(State(state): State<Arc<AppState>>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".to_string(),
        cache_entries: state.cache.entry_count().await,
        uptime_secs: state.start_time.elapsed().as_secs(),
    })
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_target(false)
        .init();

    // Ensure cache directory exists
    if let Err(e) = std::fs::create_dir_all(&cli.cache_dir) {
        tracing::error!("Failed to create cache dir {}: {}", cli.cache_dir.display(), e);
        std::process::exit(1);
    }

    let state = Arc::new(AppState {
        cache: ProxyCache::new(&cli.cache_dir),
        upstream_url: "https://api.solcast.com.au".to_string(),
        client: reqwest::Client::new(),
        start_time: Instant::now(),
        ttl: cli.ttl,
        rate_limit: cli.rate_limit,
    });

    let app = Router::new()
        .route("/rooftop_sites/{rooftop_id}/{endpoint}", get(proxy::proxy_handler))
        .route("/health", get(health))
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], cli.port));
    tracing::info!(
        "Solcast proxy listening on {} (ttl={}s, rate_limit={}s, cache_dir={})",
        addr,
        cli.ttl,
        cli.rate_limit,
        cli.cache_dir.display()
    );

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
