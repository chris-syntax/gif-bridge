use std::{
    collections::HashMap,
    io,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, StatusCode},
    response::Response,
};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use tokio::{
    fs,
    io::AsyncWriteExt,
    sync::{mpsc, watch},
};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::io::ReaderStream;

use crate::{disk_cache::DiskCache, AppState};

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FetchError {
    NotFound,
    Upstream,
}

type Completion = Result<(), FetchError>;

/// Coalesces concurrent fetches of the same cache key: the first caller
/// becomes the leader and actually downloads; everyone else gets a receiver
/// that resolves when the entry is on disk (or the download failed).
#[derive(Default)]
pub struct InFlight {
    map: Mutex<HashMap<String, watch::Receiver<Option<Completion>>>>,
}

pub enum Role {
    Leader(LeaderGuard),
    Follower(watch::Receiver<Option<Completion>>),
}

impl InFlight {
    pub fn begin(inflight: &Arc<InFlight>, key: &str) -> Role {
        let mut map = inflight.map.lock().expect("inflight lock poisoned");
        if let Some(rx) = map.get(key) {
            return Role::Follower(rx.clone());
        }
        let (tx, rx) = watch::channel(None);
        map.insert(key.to_string(), rx);
        Role::Leader(LeaderGuard {
            inflight: inflight.clone(),
            key: key.to_string(),
            tx: Some(tx),
        })
    }
}

/// The leader's obligation to wake followers. If it's dropped without
/// `finish` — the leader task panicked or was cancelled mid-fetch — followers
/// get an upstream error and the key is released for the next caller instead
/// of wedging every future request behind a dead entry.
pub struct LeaderGuard {
    inflight: Arc<InFlight>,
    key: String,
    tx: Option<watch::Sender<Option<Completion>>>,
}

impl LeaderGuard {
    pub fn finish(mut self, result: Completion) {
        self.complete(result);
    }

    fn complete(&mut self, result: Completion) {
        if let Some(tx) = self.tx.take() {
            // Release the key before waking followers, so a follower that
            // retries becomes a fresh leader rather than re-following.
            self.inflight
                .map
                .lock()
                .expect("inflight lock poisoned")
                .remove(&self.key);
            let _ = tx.send(Some(result));
        }
    }
}

impl Drop for LeaderGuard {
    fn drop(&mut self) {
        self.complete(Err(FetchError::Upstream));
    }
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

    // Two passes: a follower whose leader succeeded loops back to the disk
    // hit; the retry also covers the freak case of eviction in between.
    for _ in 0..2 {
        if let Some((file, size)) = state.disk_cache.get(&cache_key).await {
            return Ok(media_response(
                Body::from_stream(ReaderStream::new(file)),
                Some(size),
            ));
        }

        match InFlight::begin(&state.inflight, &cache_key) {
            Role::Leader(guard) => return lead_fetch(&state, &id, variant, &cache_key, guard).await,
            Role::Follower(mut rx) => {
                let completion = match rx.wait_for(|c| c.is_some()).await {
                    Ok(c) => c.clone().expect("wait_for guarantees Some"),
                    Err(_) => Err(FetchError::Upstream),
                };
                match completion {
                    Ok(()) => continue,
                    Err(FetchError::NotFound) => return Err(StatusCode::NOT_FOUND),
                    Err(FetchError::Upstream) => return Err(StatusCode::BAD_GATEWAY),
                }
            }
        }
    }
    Err(StatusCode::BAD_GATEWAY)
}

/// Starts the CDN download and responds with a stream of its chunks; a
/// spawned task tees the same chunks to a temp file and commits it to the
/// disk cache, running to completion even if this client disconnects.
async fn lead_fetch(
    state: &AppState,
    id: &str,
    variant: Variant,
    cache_key: &str,
    guard: LeaderGuard,
) -> Result<Response, StatusCode> {
    let Some(urls) = state.url_store.get(id).await else {
        guard.finish(Err(FetchError::NotFound));
        return Err(StatusCode::NOT_FOUND);
    };
    let source_url = match variant {
        Variant::Thumb => urls.thumb,
        Variant::Full => urls.full,
    };

    let cdn_resp = match state.http.get(&source_url).send().await {
        Ok(resp) if resp.status().is_success() => resp,
        _ => {
            guard.finish(Err(FetchError::Upstream));
            return Err(StatusCode::BAD_GATEWAY);
        }
    };
    let content_length = cdn_resp.content_length();

    let tmp = state.disk_cache.temp_path();
    let file = match fs::File::create(&tmp).await {
        Ok(file) => file,
        Err(_) => {
            guard.finish(Err(FetchError::Upstream));
            return Err(StatusCode::BAD_GATEWAY);
        }
    };

    let (tx, rx) = mpsc::channel::<Result<Bytes, io::Error>>(8);
    tokio::spawn(download_and_tee(
        state.disk_cache.clone(),
        guard,
        cache_key.to_string(),
        tmp,
        file,
        cdn_resp.bytes_stream(),
        tx,
    ));

    Ok(media_response(
        Body::from_stream(ReceiverStream::new(rx)),
        content_length,
    ))
}

async fn download_and_tee(
    disk_cache: Arc<DiskCache>,
    guard: LeaderGuard,
    key: String,
    tmp: PathBuf,
    mut file: fs::File,
    stream: impl Stream<Item = reqwest::Result<Bytes>>,
    tx: mpsc::Sender<Result<Bytes, io::Error>>,
) {
    tokio::pin!(stream);
    let result: Result<(), String> = async {
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| format!("cdn read: {e}"))?;
            file.write_all(&chunk)
                .await
                .map_err(|e| format!("tmp write: {e}"))?;
            // A closed receiver just means the client went away; the cache
            // still wants the rest of the file.
            let _ = tx.send(Ok(chunk)).await;
        }
        file.flush().await.map_err(|e| format!("tmp flush: {e}"))?;
        drop(file);
        disk_cache
            .commit(&key, &tmp)
            .await
            .map_err(|e| format!("cache commit: {e}"))?;
        Ok(())
    }
    .await;

    match result {
        Ok(()) => guard.finish(Ok(())),
        Err(e) => {
            tracing::warn!(key, error = %e, "media download failed");
            let _ = fs::remove_file(&tmp).await;
            // Abort the client's stream too — a silently truncated gif is
            // worse than a broken transfer the browser can see.
            let _ = tx.send(Err(io::Error::other(e))).await;
            guard.finish(Err(FetchError::Upstream));
        }
    }
}

fn media_response(body: Body, content_length: Option<u64>) -> Response {
    let mut builder = Response::builder().header(header::CONTENT_TYPE, GIF_CONTENT_TYPE);
    if let Some(len) = content_length {
        builder = builder.header(header::CONTENT_LENGTH, len);
    }
    builder.body(body).expect("static headers are valid")
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

    #[tokio::test]
    async fn first_caller_leads_later_callers_follow() {
        let inflight = Arc::new(InFlight::default());
        let Role::Leader(guard) = InFlight::begin(&inflight, "k") else {
            panic!("first caller should lead");
        };
        assert!(matches!(
            InFlight::begin(&inflight, "k"),
            Role::Follower(_)
        ));
        // Distinct keys don't coalesce.
        assert!(matches!(InFlight::begin(&inflight, "j"), Role::Leader(_)));
        drop(guard);
    }

    #[tokio::test]
    async fn followers_wake_on_success_and_key_is_released() {
        let inflight = Arc::new(InFlight::default());
        let Role::Leader(guard) = InFlight::begin(&inflight, "k") else {
            panic!("first caller should lead");
        };
        let Role::Follower(mut rx) = InFlight::begin(&inflight, "k") else {
            panic!("second caller should follow");
        };

        let waiter = tokio::spawn(async move {
            rx.wait_for(|c| c.is_some())
                .await
                .unwrap()
                .clone()
                .expect("wait_for guarantees Some")
        });
        guard.finish(Ok(()));

        assert_eq!(waiter.await.unwrap(), Ok(()));
        assert!(matches!(InFlight::begin(&inflight, "k"), Role::Leader(_)));
    }

    #[tokio::test]
    async fn followers_see_leader_failure() {
        let inflight = Arc::new(InFlight::default());
        let Role::Leader(guard) = InFlight::begin(&inflight, "k") else {
            panic!("first caller should lead");
        };
        let Role::Follower(mut rx) = InFlight::begin(&inflight, "k") else {
            panic!("second caller should follow");
        };
        guard.finish(Err(FetchError::NotFound));

        let completion = rx.wait_for(|c| c.is_some()).await.unwrap().clone().unwrap();
        assert_eq!(completion, Err(FetchError::NotFound));
    }

    #[tokio::test]
    async fn dropped_leader_fails_followers_and_releases_key() {
        let inflight = Arc::new(InFlight::default());
        let Role::Leader(guard) = InFlight::begin(&inflight, "k") else {
            panic!("first caller should lead");
        };
        let Role::Follower(mut rx) = InFlight::begin(&inflight, "k") else {
            panic!("second caller should follow");
        };
        drop(guard);

        let completion = rx.wait_for(|c| c.is_some()).await.unwrap().clone().unwrap();
        assert_eq!(completion, Err(FetchError::Upstream));
        assert!(matches!(InFlight::begin(&inflight, "k"), Role::Leader(_)));
    }
}
