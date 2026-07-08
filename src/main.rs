mod auth;
mod giphy;
mod media;
mod search;

use std::{env, sync::Arc, time::Duration};

use axum::{routing::get, Router};
use moka::future::Cache;
use reqwest::Client;
use tower_http::cors::{Any, CorsLayer};

use auth::OpenIdVerifier;
use giphy::{GifUrls, GiphyClient};
use media::CachedMedia;
use search::GifSearchResult;

#[derive(Clone)]
pub struct AppState {
    pub http: Client,
    pub giphy: Arc<GiphyClient>,
    pub verifier: Arc<OpenIdVerifier>,
    pub media_cache: Cache<String, CachedMedia>,
    pub search_cache: Cache<(String, u32), Arc<Vec<GifSearchResult>>>,
    pub url_map: Cache<String, GifUrls>,
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

    let http = Client::new();
    let state = AppState {
        giphy: Arc::new(GiphyClient::new(http.clone(), giphy_api_key)),
        verifier: Arc::new(OpenIdVerifier::new(
            http.clone(),
            openid_verify_base,
            Duration::from_secs(15 * 60),
        )),
        media_cache: Cache::builder()
            .time_to_live(Duration::from_secs(7 * 24 * 60 * 60))
            .max_capacity(2_000)
            .build(),
        search_cache: Cache::builder()
            .time_to_live(Duration::from_secs(60 * 60))
            .max_capacity(1_000)
            .build(),
        url_map: Cache::builder()
            .time_to_live(Duration::from_secs(7 * 24 * 60 * 60))
            .max_capacity(50_000)
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
