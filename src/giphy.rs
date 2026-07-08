use reqwest::{Client, StatusCode, Url};
use serde::Deserialize;

/// The only place outbound `api.giphy.com` calls are made; every call is
/// logged at info level so quota consumption stays observable.
pub struct GiphyClient {
    http: Client,
    api_key: String,
}

#[derive(Debug)]
pub enum GiphyError {
    Request(reqwest::Error),
    Status(StatusCode),
}

impl GiphyClient {
    pub fn new(http: Client, api_key: String) -> Self {
        Self { http, api_key }
    }

    pub async fn search(&self, q: &str, limit: u32) -> Result<Vec<GiphyGif>, GiphyError> {
        let mut url =
            Url::parse("https://api.giphy.com/v1/gifs/search").expect("static url parses");
        url.query_pairs_mut()
            .append_pair("api_key", &self.api_key)
            .append_pair("q", q)
            .append_pair("limit", &limit.to_string());

        tracing::info!(q, limit, "giphy api call: search");
        let resp = self.http.get(url).send().await.map_err(GiphyError::Request)?;
        if !resp.status().is_success() {
            return Err(GiphyError::Status(resp.status()));
        }
        let parsed: GiphySearchResponse = resp.json().await.map_err(GiphyError::Request)?;
        Ok(parsed.data)
    }

    pub async fn get_by_id(&self, id: &str) -> Result<GiphyGif, GiphyError> {
        let lookup_url = format!(
            "https://api.giphy.com/v1/gifs/{}?api_key={}",
            id, self.api_key
        );

        tracing::info!(id, "giphy api call: get by id");
        let resp = self
            .http
            .get(&lookup_url)
            .send()
            .await
            .map_err(GiphyError::Request)?;
        if !resp.status().is_success() {
            return Err(GiphyError::Status(resp.status()));
        }
        let parsed: GiphyGetResponse = resp.json().await.map_err(GiphyError::Request)?;
        Ok(parsed.data)
    }
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
pub struct GiphyGif {
    pub id: String,
    pub title: String,
    pub url: String,
    pub images: GiphyImages,
}

#[derive(Deserialize)]
pub struct GiphyImages {
    pub original: GiphyImage,
}

#[derive(Deserialize)]
pub struct GiphyImage {
    pub url: String,
    pub width: Option<String>,
    pub height: Option<String>,
    pub size: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_search_response() {
        let json = r#"{
            "data": [{
                "id": "abc123",
                "title": "Cat GIF",
                "url": "https://giphy.com/gifs/abc123",
                "images": {
                    "original": {
                        "url": "https://media.giphy.com/media/abc123/giphy.gif",
                        "width": "480",
                        "height": "270",
                        "size": "1048576"
                    }
                }
            }]
        }"#;

        let parsed: GiphySearchResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.data.len(), 1);
        assert_eq!(parsed.data[0].id, "abc123");
        assert_eq!(parsed.data[0].images.original.width.as_deref(), Some("480"));
    }

    #[test]
    fn deserializes_image_with_missing_dimensions() {
        let json = r#"{"url": "https://example.com/x.gif"}"#;
        let parsed: GiphyImage = serde_json::from_str(json).unwrap();
        assert!(parsed.width.is_none());
        assert!(parsed.height.is_none());
        assert!(parsed.size.is_none());
    }
}
