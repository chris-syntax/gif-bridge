use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::{Deserialize, Serialize};

use crate::{auth, giphy::GiphyGif, AppState};

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

    if params.q.trim().is_empty() {
        return Ok(Json(vec![]));
    }

    let limit = params.limit.unwrap_or(24).min(50);
    let gifs = state
        .giphy
        .search(&params.q, limit)
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;

    Ok(Json(to_results(gifs)))
}

fn to_results(gifs: Vec<GiphyGif>) -> Vec<GifSearchResult> {
    gifs.into_iter().map(to_result).collect()
}

fn to_result(gif: GiphyGif) -> GifSearchResult {
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
        assert_eq!(result.thumb_url, "/api/gif/media/abc123");
        assert_eq!(result.full_url, "/api/gif/media/abc123");
    }

    #[test]
    fn unparseable_dimensions_become_none() {
        let result = to_result(gif(Some("wide"), None, Some("-3")));
        assert_eq!(result.width, None);
        assert_eq!(result.height, None);
        assert_eq!(result.size, None);
    }
}
