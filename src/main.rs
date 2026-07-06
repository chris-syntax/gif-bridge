use axum::{
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use bytes::Bytes;
use moka::future::Cache;
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use std::{env, sync::Arc, time::Duration};

#[derive(Clone)]
struct AppState {
    http: Client,
    giphy_api_key: Arc<String>,
    openid_verify_base: Arc<String>,
    media_cache: Cache<String, CachedMedia>,
    // Every request re-verifies its OpenID token against the homeserver; this only
    // dedupes bursts (e.g. a room render pulling in many gifs at once) within a
    // short window, it is not a substitute for per-request verification.
    verified_token_cache: Cache<String, ()>,
}

#[derive(Clone)]
struct CachedMedia {
    bytes: Bytes,
    content_type: String,
}

#[derive(Deserialize)]
struct SearchParams {
    q: String,
    limit: Option<u32>,
    openid_token: Option<String>,
}

#[derive(Deserialize)]
struct MediaParams {
    openid_token: Option<String>,
}

#[derive(Serialize)]
struct GifSearchResult {
    id: String,
    width: Option<u32>,
    height: Option<u32>,
    mimetype: String,
    size: Option<u64>,
    title: String,
    #[serde(rename = "thumbUrl")]
    thumb_url: String,
    #[serde(rename = "fullUrl")]
    full_url: String,
    #[serde(rename = "pageUrl")]
    page_url: String,
}

#[derive(Deserialize)]
struct GiphySearchResponse {
    data: Vec<GiphyGif>,
}

#[derive(Deserialize)]
struct GiphyGetResponse {
    data: GiphyGif,
}

#[derive(Deserialize)]
struct GiphyGif {
    id: String,
    title: String,
    url: String,
    images: GiphyImages,
}

#[derive(Deserialize)]
struct GiphyImages {
    original: GiphyImage,
}

#[derive(Deserialize)]
struct GiphyImage {
    url: String,
    width: Option<String>,
    height: Option<String>,
    size: Option<String>,
}

async fn verify_openid_token(state: &AppState, token: &str) -> bool {
    if state.verified_token_cache.get(token).await.is_some() {
        return true;
    }

    let Ok(mut url) = Url::parse(&state.openid_verify_base) else {
        return false;
    };
    url.query_pairs_mut().append_pair("access_token", token);

    match state.http.get(url).send().await {
        Ok(resp) if resp.status().is_success() => {
            state.verified_token_cache.insert(token.to_string(), ()).await;
            true
        }
        _ => false,
    }
}

async fn search_handler(
    State(state): State<AppState>,
    Query(params): Query<SearchParams>,
) -> Result<Json<Vec<GifSearchResult>>, StatusCode> {
    let token = params.openid_token.as_deref().ok_or(StatusCode::UNAUTHORIZED)?;
    if !verify_openid_token(&state, token).await {
        return Err(StatusCode::UNAUTHORIZED);
    }

    if params.q.trim().is_empty() {
        return Ok(Json(vec![]));
    }

    let limit = params.limit.unwrap_or(24).min(50);
    let mut url = Url::parse("https://api.giphy.com/v1/gifs/search")
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    url.query_pairs_mut()
        .append_pair("api_key", &state.giphy_api_key)
        .append_pair("q", &params.q)
        .append_pair("limit", &limit.to_string());

    let resp = state
        .http
        .get(url)
        .send()
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    if !resp.status().is_success() {
        return Err(StatusCode::BAD_GATEWAY);
    }
    let parsed: GiphySearchResponse = resp.json().await.map_err(|_| StatusCode::BAD_GATEWAY)?;

    let results = parsed
        .data
        .into_iter()
        .map(|gif| {
            let width = gif.images.original.width.as_deref().and_then(|w| w.parse().ok());
            let height = gif.images.original.height.as_deref().and_then(|h| h.parse().ok());
            let size = gif.images.original.size.as_deref().and_then(|s| s.parse().ok());
            // A single cached rendition serves both the picker thumbnail and the
            // sent message's full image — keeps the bridge to one cache entry per
            // gif instead of juggling multiple Giphy renditions.
            let media_url = format!("/api/gif/media/{}", gif.id);
            GifSearchResult {
                id: gif.id,
                width,
                height,
                mimetype: "image/gif".to_string(),
                size,
                title: gif.title,
                thumb_url: media_url.clone(),
                full_url: media_url,
                page_url: gif.url,
            }
        })
        .collect();

    Ok(Json(results))
}

async fn media_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<MediaParams>,
) -> Result<Response, StatusCode> {
    let token = params.openid_token.as_deref().ok_or(StatusCode::UNAUTHORIZED)?;
    if !verify_openid_token(&state, token).await {
        return Err(StatusCode::UNAUTHORIZED);
    }

    if let Some(cached) = state.media_cache.get(&id).await {
        return Ok(media_response(cached));
    }

    let lookup_url = format!(
        "https://api.giphy.com/v1/gifs/{}?api_key={}",
        id, state.giphy_api_key
    );
    let resp = state
        .http
        .get(&lookup_url)
        .send()
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    if !resp.status().is_success() {
        return Err(StatusCode::NOT_FOUND);
    }
    let parsed: GiphyGetResponse = resp.json().await.map_err(|_| StatusCode::BAD_GATEWAY)?;
    let source_url = parsed.data.images.original.url;

    let media_resp = state
        .http
        .get(&source_url)
        .send()
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    if !media_resp.status().is_success() {
        return Err(StatusCode::BAD_GATEWAY);
    }
    let content_type = media_resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("image/gif")
        .to_string();
    let bytes = media_resp.bytes().await.map_err(|_| StatusCode::BAD_GATEWAY)?;

    let cached = CachedMedia { bytes, content_type };
    state.media_cache.insert(id, cached.clone()).await;

    Ok(media_response(cached))
}

fn media_response(cached: CachedMedia) -> Response {
    ([(header::CONTENT_TYPE, cached.content_type)], cached.bytes).into_response()
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

    let state = AppState {
        http: Client::new(),
        giphy_api_key: Arc::new(giphy_api_key),
        openid_verify_base: Arc::new(openid_verify_base),
        media_cache: Cache::builder()
            .time_to_live(Duration::from_secs(7 * 24 * 60 * 60))
            .max_capacity(2_000)
            .build(),
        verified_token_cache: Cache::builder()
            .time_to_live(Duration::from_secs(60))
            .max_capacity(10_000)
            .build(),
    };

    let app = Router::new()
        .route("/search", get(search_handler))
        .route("/media/:id", get(media_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .expect("failed to bind listener");
    tracing::info!("gif-bridge listening on :{port}");
    axum::serve(listener, app).await.expect("server error");
}
