mod auth;
mod disk_cache;
mod giphy;
mod media;
mod search;

use std::{env, path::PathBuf, sync::Arc, time::Duration};

use axum::{routing::get, Router};
use moka::future::Cache;
use reqwest::Client;
use tower_http::cors::{Any, CorsLayer};

use auth::OpenIdVerifier;
use disk_cache::{DiskCache, UrlStore};
use giphy::GiphyClient;
use search::GifSearchResult;

#[derive(Clone)]
pub struct AppState {
    pub http: Client,
    pub giphy: Arc<GiphyClient>,
    pub verifier: Arc<OpenIdVerifier>,
    pub disk_cache: Arc<DiskCache>,
    pub url_store: Arc<UrlStore>,
    pub search_cache: Cache<(String, u32), Arc<Vec<GifSearchResult>>>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let giphy_api_key = env::var("GIPHY_API_KEY").expect("GIPHY_API_KEY must be set");
    let openid_verify_base = env::var("OPENID_VERIFY_URL").unwrap_or_else(|_| {
        "https://matrix.loaf.moe/_matrix/federation/v1/openid/userinfo".to_string()
    });
    let port: u16 = env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);
    let cache_dir = PathBuf::from(env::var("CACHE_DIR").unwrap_or_else(|_| "./cache".to_string()));
    let cache_max_bytes: u64 = env::var("CACHE_MAX_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2 * 1024 * 1024 * 1024);

    let disk_cache = DiskCache::open(cache_dir.join("media"), cache_max_bytes)
        .await
        .expect("failed to open media disk cache");
    let url_store = UrlStore::open(cache_dir.join("urls"))
        .await
        .expect("failed to open url store");

    let http = Client::new();
    let state = AppState {
        giphy: Arc::new(GiphyClient::new(http.clone(), giphy_api_key)),
        verifier: Arc::new(OpenIdVerifier::new(
            http.clone(),
            openid_verify_base,
            Duration::from_secs(15 * 60),
        )),
        disk_cache: Arc::new(disk_cache),
        url_store: Arc::new(url_store),
        search_cache: Cache::builder()
            .time_to_live(Duration::from_secs(60 * 60))
            .max_capacity(1_000)
            .build(),
        http,
    };

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        .route("/search", get(search::search_handler))
        .route("/media/:id/:variant", get(media::media_handler))
        .layer(cors)
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .expect("failed to bind listener");
    tracing::info!("gif-bridge listening on :{port}");
    axum::serve(listener, app).await.expect("server error");
}
