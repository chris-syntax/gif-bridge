use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use bytes::Bytes;

use crate::{giphy::GiphyError, AppState};

#[derive(Clone)]
pub struct CachedMedia {
    pub bytes: Bytes,
    pub content_type: String,
}

// Unauthenticated by design: a gif id is only ever learned by going through an
// authenticated /search first, so there's nothing to gate here — the resource
// actually worth protecting (the Giphy API) is only reachable via /search.
pub async fn media_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, StatusCode> {
    if let Some(cached) = state.media_cache.get(&id).await {
        return Ok(media_response(cached));
    }

    let gif = state.giphy.get_by_id(&id).await.map_err(|e| match e {
        GiphyError::Status(_) => StatusCode::NOT_FOUND,
        GiphyError::Request(_) => StatusCode::BAD_GATEWAY,
    })?;
    let source_url = gif.images.original.url;

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
