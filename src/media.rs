use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use tokio::io::AsyncReadExt;

use crate::AppState;

// Both rendition urls come out of Giphy's `images` block as .gif files, so
// the content type is static rather than stored per entry.
const GIF_CONTENT_TYPE: &str = "image/gif";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Variant {
    Thumb,
    Full,
}

impl Variant {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "thumb" => Some(Self::Thumb),
            "full" => Some(Self::Full),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Thumb => "thumb",
            Self::Full => "full",
        }
    }
}

/// Giphy ids are alphanumeric (occasionally with `-`/`_`); anything else is
/// rejected before the id can reach a cache key or the filesystem.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

// Unauthenticated by design, and it never calls api.giphy.com: CDN bytes are
// unmetered, and the only ids worth serving are the ones /search already
// recorded urls for — anything else is a 404, not an API lookup.
pub async fn media_handler(
    State(state): State<AppState>,
    Path((id, variant)): Path<(String, String)>,
) -> Result<Response, StatusCode> {
    let variant = Variant::parse(&variant).ok_or(StatusCode::NOT_FOUND)?;
    if !valid_id(&id) {
        return Err(StatusCode::NOT_FOUND);
    }

    let cache_key = format!("{id}.{}", variant.as_str());
    if let Some((mut file, size)) = state.disk_cache.get(&cache_key).await {
        let mut buf = Vec::with_capacity(size as usize);
        file.read_to_end(&mut buf)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        return Ok(media_response(buf.into()));
    }

    let urls = state.url_store.get(&id).await.ok_or(StatusCode::NOT_FOUND)?;
    let source_url = match variant {
        Variant::Thumb => urls.thumb,
        Variant::Full => urls.full,
    };

    let media_resp = state
        .http
        .get(&source_url)
        .send()
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    if !media_resp.status().is_success() {
        return Err(StatusCode::BAD_GATEWAY);
    }
    let bytes = media_resp.bytes().await.map_err(|_| StatusCode::BAD_GATEWAY)?;

    // A failed cache write shouldn't fail the request the bytes are for.
    if let Err(e) = state.disk_cache.insert(&cache_key, &bytes).await {
        tracing::warn!(cache_key, error = %e, "failed to cache media on disk");
    }

    Ok(media_response(bytes))
}

fn media_response(bytes: Bytes) -> Response {
    ([(header::CONTENT_TYPE, GIF_CONTENT_TYPE)], bytes).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_variants() {
        assert_eq!(Variant::parse("thumb"), Some(Variant::Thumb));
        assert_eq!(Variant::parse("full"), Some(Variant::Full));
        assert_eq!(Variant::parse("original"), None);
        assert_eq!(Variant::parse(""), None);
    }

    #[test]
    fn accepts_giphy_shaped_ids() {
        assert!(valid_id("3o7aD2saalBwwftBIY"));
        assert!(valid_id("l0HlBO7eyXzSZkJri"));
        assert!(valid_id("abc-123_XYZ"));
    }

    #[test]
    fn rejects_path_traversal_shaped_ids() {
        assert!(!valid_id(""));
        assert!(!valid_id(".."));
        assert!(!valid_id("../../etc/passwd"));
        assert!(!valid_id("abc/def"));
        assert!(!valid_id("abc.gif"));
    }
}
