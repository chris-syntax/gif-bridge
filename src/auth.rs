use std::time::Duration;

use axum::http::{header, HeaderMap};
use moka::future::Cache;
use reqwest::{Client, Url};

/// Verifies Matrix OpenID tokens against the homeserver's federation
/// `openid/userinfo` endpoint.
///
/// A given client mints one OpenID token and reuses it for a while (see
/// cinny's getCachedOpenIdToken), so the success cache is what actually saves
/// the repeat homeserver round-trip rather than just deduping a burst.
pub struct OpenIdVerifier {
    http: Client,
    verify_base: String,
    verified: Cache<String, ()>,
}

impl OpenIdVerifier {
    pub fn new(http: Client, verify_base: String, ttl: Duration) -> Self {
        Self {
            http,
            verify_base,
            verified: Cache::builder()
                .time_to_live(ttl)
                .max_capacity(10_000)
                .build(),
        }
    }

    pub async fn verify(&self, token: &str) -> bool {
        if self.verified.get(token).await.is_some() {
            return true;
        }

        let Ok(mut url) = Url::parse(&self.verify_base) else {
            return false;
        };
        url.query_pairs_mut().append_pair("access_token", token);

        match self.http.get(url).send().await {
            Ok(resp) if resp.status().is_success() => {
                self.verified.insert(token.to_string(), ()).await;
                true
            }
            _ => false,
        }
    }
}

pub fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers_with_auth(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, HeaderValue::from_str(value).unwrap());
        headers
    }

    #[test]
    fn extracts_bearer_token() {
        assert_eq!(
            bearer_token(&headers_with_auth("Bearer abc123")),
            Some("abc123")
        );
    }

    #[test]
    fn rejects_missing_header() {
        assert_eq!(bearer_token(&HeaderMap::new()), None);
    }

    #[test]
    fn rejects_non_bearer_scheme() {
        assert_eq!(bearer_token(&headers_with_auth("Basic abc123")), None);
    }

    #[test]
    fn bearer_prefix_is_case_sensitive() {
        assert_eq!(bearer_token(&headers_with_auth("bearer abc123")), None);
    }
}
