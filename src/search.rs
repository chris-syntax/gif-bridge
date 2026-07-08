use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::{Deserialize, Serialize};

use crate::{
    auth,
    giphy::{self, GiphyError, GiphyGif},
    AppState,
};

#[derive(Deserialize)]
pub struct SearchParams {
    q: String,
    limit: Option<u32>,
}

#[derive(Clone, Serialize)]
pub struct GifSearchResult {
    pub id: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub mimetype: String,
    pub size: Option<u64>,
    pub title: String,
    #[serde(rename = "thumbUrl")]
    pub thumb_url: String,
    #[serde(rename = "fullUrl")]
    pub full_url: String,
    #[serde(rename = "pageUrl")]
    pub page_url: String,
}

pub async fn search_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<SearchParams>,
) -> Result<Json<Vec<GifSearchResult>>, StatusCode> {
    let token = auth::bearer_token(&headers).ok_or(StatusCode::UNAUTHORIZED)?;
    if !state.verifier.verify(token).await {
        return Err(StatusCode::UNAUTHORIZED);
    }

    let (q, limit) = cache_key(&params.q, params.limit);
    if q.is_empty() {
        return Ok(Json(vec![]));
    }

    let results = state
        .search_cache
        .try_get_with((q.clone(), limit), async {
            let gifs = state.giphy.search(&q, limit).await?;
            // Record CDN urls now: search is the only place ids are born, and
            // /media resolves against these records instead of the Giphy API.
            // A failed write is only a log line — the gif itself can still be
            // served for as long as the in-memory side of the store holds it.
            for gif in &gifs {
                if let Err(e) = state.url_store.put(&gif.id, &giphy::gif_urls(gif)).await {
                    tracing::warn!(id = gif.id, error = %e, "failed to persist url record");
                }
            }
            Ok::<_, GiphyError>(Arc::new(to_results(gifs)))
        })
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;

    Ok(Json(results.as_ref().clone()))
}

/// Normalized cache key: Giphy search is case-insensitive, so "Cat" and
/// "cat " share an entry instead of costing two API calls.
fn cache_key(q: &str, limit: Option<u32>) -> (String, u32) {
    (q.trim().to_lowercase(), limit.unwrap_or(24).min(50))
}

fn to_results(gifs: Vec<GiphyGif>) -> Vec<GifSearchResult> {
    gifs.into_iter().map(to_result).collect()
}

fn to_result(gif: GiphyGif) -> GifSearchResult {
    let width = gif.images.original.width.as_deref().and_then(|w| w.parse().ok());
    let height = gif.images.original.height.as_deref().and_then(|h| h.parse().ok());
    let size = gif.images.original.size.as_deref().and_then(|s| s.parse().ok());
    GifSearchResult {
        thumb_url: format!("/api/gif/media/{}/thumb", gif.id),
        full_url: format!("/api/gif/media/{}/full", gif.id),
        id: gif.id,
        width,
        height,
        mimetype: "image/gif".to_string(),
        size,
        title: gif.title,
        page_url: gif.url,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::giphy::{GiphyImage, GiphyImages};

    fn gif(width: Option<&str>, height: Option<&str>, size: Option<&str>) -> GiphyGif {
        GiphyGif {
            id: "abc123".to_string(),
            title: "Cat GIF".to_string(),
            url: "https://giphy.com/gifs/abc123".to_string(),
            images: GiphyImages {
                original: GiphyImage {
                    url: "https://media.giphy.com/media/abc123/giphy.gif".to_string(),
                    width: width.map(String::from),
                    height: height.map(String::from),
                    size: size.map(String::from),
                },
                fixed_width: None,
            },
        }
    }

    #[test]
    fn maps_numeric_strings() {
        let result = to_result(gif(Some("480"), Some("270"), Some("1048576")));
        assert_eq!(result.width, Some(480));
        assert_eq!(result.height, Some(270));
        assert_eq!(result.size, Some(1_048_576));
        assert_eq!(result.mimetype, "image/gif");
        assert_eq!(result.thumb_url, "/api/gif/media/abc123/thumb");
        assert_eq!(result.full_url, "/api/gif/media/abc123/full");
    }

    #[test]
    fn unparseable_dimensions_become_none() {
        let result = to_result(gif(Some("wide"), None, Some("-3")));
        assert_eq!(result.width, None);
        assert_eq!(result.height, None);
        assert_eq!(result.size, None);
    }

    #[test]
    fn cache_key_normalizes_query() {
        assert_eq!(cache_key("  Cat GIF ", None), ("cat gif".to_string(), 24));
    }

    #[test]
    fn cache_key_clamps_limit() {
        assert_eq!(cache_key("cat", Some(500)).1, 50);
        assert_eq!(cache_key("cat", Some(10)).1, 10);
    }

    #[test]
    fn cache_key_blank_query_is_empty() {
        assert_eq!(cache_key("   ", None).0, "");
    }
}
