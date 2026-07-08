use std::{
    io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::UNIX_EPOCH,
};

use lru::LruCache;
use moka::future::Cache;
use tokio::{fs, sync::Mutex};

use crate::giphy::GifUrls;

/// Byte-capped LRU cache of media files.
///
/// The index lives in memory; the bytes live as one file per entry under
/// `dir`. Commits are atomic (temp write + rename on the same filesystem), so
/// a crash mid-download can never leave a truncated entry looking valid.
/// Recency survives restarts only approximately: the startup scan orders by
/// mtime, i.e. by when each entry was written, not last read.
pub struct DiskCache {
    dir: PathBuf,
    max_bytes: u64,
    tmp_counter: AtomicU64,
    index: Mutex<Index>,
}

struct Index {
    lru: LruCache<String, u64>,
    total: u64,
}

impl DiskCache {
    pub async fn open(dir: PathBuf, max_bytes: u64) -> io::Result<Self> {
        fs::create_dir_all(&dir).await?;

        let mut entries = Vec::new();
        let mut read_dir = fs::read_dir(&dir).await?;
        while let Some(entry) = read_dir.next_entry().await? {
            let meta = entry.metadata().await?;
            if !meta.is_file() {
                continue;
            }
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if name.ends_with(".tmp") {
                let _ = fs::remove_file(entry.path()).await;
                continue;
            }
            let mtime = meta.modified().unwrap_or(UNIX_EPOCH);
            entries.push((name, meta.len(), mtime));
        }
        entries.sort_by_key(|&(_, _, mtime)| mtime);

        let mut index = Index {
            lru: LruCache::unbounded(),
            total: 0,
        };
        for (name, size, _) in entries {
            index.total += size;
            index.lru.put(name, size);
        }

        let cache = Self {
            dir,
            max_bytes,
            tmp_counter: AtomicU64::new(0),
            index: Mutex::new(index),
        };
        cache.evict_over_cap().await;
        Ok(cache)
    }

    fn entry_path(&self, key: &str) -> PathBuf {
        self.dir.join(key)
    }

    /// Opens a cached entry, promoting it in the LRU. Returns the open file
    /// and its size.
    pub async fn get(&self, key: &str) -> Option<(fs::File, u64)> {
        let size = { self.index.lock().await.lru.get(key).copied() }?;
        match fs::File::open(self.entry_path(key)).await {
            Ok(file) => Some((file, size)),
            Err(_) => {
                // The file vanished out from under the index; drop the entry.
                let mut index = self.index.lock().await;
                if let Some(size) = index.lru.pop(key) {
                    index.total -= size;
                }
                None
            }
        }
    }

    /// A unique temp path inside the cache dir, so the final rename in
    /// `commit` stays on one filesystem and therefore atomic.
    pub fn temp_path(&self) -> PathBuf {
        let n = self.tmp_counter.fetch_add(1, Ordering::Relaxed);
        self.dir.join(format!("{}-{n}.tmp", std::process::id()))
    }

    /// Atomically promotes a fully-written temp file to a cache entry.
    pub async fn commit(&self, key: &str, tmp: &Path) -> io::Result<()> {
        let size = fs::metadata(tmp).await?.len();
        fs::rename(tmp, self.entry_path(key)).await?;
        {
            let mut index = self.index.lock().await;
            if let Some(old) = index.lru.put(key.to_string(), size) {
                index.total -= old;
            }
            index.total += size;
        }
        self.evict_over_cap().await;
        Ok(())
    }

    pub async fn insert(&self, key: &str, bytes: &[u8]) -> io::Result<()> {
        let tmp = self.temp_path();
        fs::write(&tmp, bytes).await?;
        self.commit(key, &tmp).await
    }

    async fn evict_over_cap(&self) {
        let victims = {
            let mut index = self.index.lock().await;
            let mut victims = Vec::new();
            while index.total > self.max_bytes {
                let Some((key, size)) = index.lru.pop_lru() else {
                    break;
                };
                index.total -= size;
                victims.push(key);
            }
            victims
        };
        for key in victims {
            tracing::info!(key, "evicting cached media");
            let _ = fs::remove_file(self.entry_path(&key)).await;
        }
    }
}

const MAX_URL_RECORDS: usize = 100_000;

/// Persistent `id -> CDN urls` records, fronted by an in-memory cache.
///
/// Written once per id at search time (~200 bytes each). These records are
/// what let /media serve a gif from an old message after a restart without
/// ever asking the Giphy API for its url again.
pub struct UrlStore {
    dir: PathBuf,
    tmp_counter: AtomicU64,
    mem: Cache<String, GifUrls>,
}

impl UrlStore {
    pub async fn open(dir: PathBuf) -> io::Result<Self> {
        fs::create_dir_all(&dir).await?;
        prune_oldest(&dir, MAX_URL_RECORDS).await?;
        Ok(Self {
            dir,
            tmp_counter: AtomicU64::new(0),
            mem: Cache::builder().max_capacity(50_000).build(),
        })
    }

    fn record_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    pub async fn put(&self, id: &str, urls: &GifUrls) -> io::Result<()> {
        if self.mem.contains_key(id) {
            return Ok(());
        }
        let json = serde_json::to_vec(urls)?;
        let n = self.tmp_counter.fetch_add(1, Ordering::Relaxed);
        let tmp = self.dir.join(format!("{}-{n}.tmp", std::process::id()));
        fs::write(&tmp, json).await?;
        fs::rename(&tmp, self.record_path(id)).await?;
        self.mem.insert(id.to_string(), urls.clone()).await;
        Ok(())
    }

    pub async fn get(&self, id: &str) -> Option<GifUrls> {
        if let Some(urls) = self.mem.get(id).await {
            return Some(urls);
        }
        let bytes = fs::read(self.record_path(id)).await.ok()?;
        let urls: GifUrls = serde_json::from_slice(&bytes).ok()?;
        self.mem.insert(id.to_string(), urls.clone()).await;
        Some(urls)
    }
}

/// Startup-only bound on url records: drop the oldest by mtime once over cap.
async fn prune_oldest(dir: &Path, keep: usize) -> io::Result<()> {
    let mut records = Vec::new();
    let mut read_dir = fs::read_dir(dir).await?;
    while let Some(entry) = read_dir.next_entry().await? {
        let meta = entry.metadata().await?;
        if !meta.is_file() {
            continue;
        }
        records.push((meta.modified().unwrap_or(UNIX_EPOCH), entry.path()));
    }
    if records.len() <= keep {
        return Ok(());
    }
    records.sort_by_key(|&(mtime, _)| mtime);
    for (_, path) in records.iter().take(records.len() - keep) {
        let _ = fs::remove_file(path).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    async fn read_entry(cache: &DiskCache, key: &str) -> Option<Vec<u8>> {
        let (mut file, size) = cache.get(key).await?;
        let mut buf = Vec::with_capacity(size as usize);
        file.read_to_end(&mut buf).await.unwrap();
        Some(buf)
    }

    #[tokio::test]
    async fn insert_get_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cache = DiskCache::open(dir.path().join("media"), 1024).await.unwrap();

        cache.insert("abc.full", b"gif bytes").await.unwrap();
        assert_eq!(read_entry(&cache, "abc.full").await.unwrap(), b"gif bytes");
        assert!(cache.get("missing.full").await.is_none());
    }

    #[tokio::test]
    async fn evicts_least_recently_used_over_byte_cap() {
        let dir = tempfile::tempdir().unwrap();
        let cache = DiskCache::open(dir.path().join("media"), 100).await.unwrap();

        cache.insert("a", &[0u8; 40]).await.unwrap();
        cache.insert("b", &[0u8; 40]).await.unwrap();
        // Promote a, so b is the LRU victim when c pushes total to 120.
        assert!(cache.get("a").await.is_some());
        cache.insert("c", &[0u8; 40]).await.unwrap();

        assert!(cache.get("a").await.is_some());
        assert!(cache.get("b").await.is_none());
        assert!(cache.get("c").await.is_some());
        assert!(!dir.path().join("media").join("b").exists());
    }

    #[tokio::test]
    async fn overwriting_a_key_does_not_double_count() {
        let dir = tempfile::tempdir().unwrap();
        let cache = DiskCache::open(dir.path().join("media"), 100).await.unwrap();

        cache.insert("a", &[0u8; 60]).await.unwrap();
        cache.insert("a", &[0u8; 60]).await.unwrap();
        // 60 live bytes, not 120 — nothing should have been evicted.
        assert!(cache.get("a").await.is_some());
    }

    #[tokio::test]
    async fn index_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("media");
        {
            let cache = DiskCache::open(path.clone(), 1024).await.unwrap();
            cache.insert("abc.full", b"persisted").await.unwrap();
        }
        let cache = DiskCache::open(path, 1024).await.unwrap();
        assert_eq!(read_entry(&cache, "abc.full").await.unwrap(), b"persisted");
    }

    #[tokio::test]
    async fn restart_scan_evicts_oldest_when_over_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("media");
        {
            let cache = DiskCache::open(path.clone(), 1024).await.unwrap();
            cache.insert("old", &[0u8; 40]).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            cache.insert("new", &[0u8; 40]).await.unwrap();
        }
        let cache = DiskCache::open(path, 50).await.unwrap();
        assert!(cache.get("old").await.is_none());
        assert!(cache.get("new").await.is_some());
    }

    #[tokio::test]
    async fn cleans_up_leftover_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("media");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("123-0.tmp"), b"partial download").unwrap();

        let cache = DiskCache::open(path.clone(), 1024).await.unwrap();
        assert!(!path.join("123-0.tmp").exists());
        assert!(cache.get("123-0.tmp").await.is_none());
    }

    #[tokio::test]
    async fn url_store_roundtrip_and_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("urls");
        let urls = GifUrls {
            full: "https://media.giphy.com/media/abc/giphy.gif".to_string(),
            thumb: "https://media.giphy.com/media/abc/200w.gif".to_string(),
        };
        {
            let store = UrlStore::open(path.clone()).await.unwrap();
            store.put("abc", &urls).await.unwrap();
            assert_eq!(store.get("abc").await.unwrap().thumb, urls.thumb);
        }
        let store = UrlStore::open(path).await.unwrap();
        let read_back = store.get("abc").await.unwrap();
        assert_eq!(read_back.full, urls.full);
        assert_eq!(read_back.thumb, urls.thumb);
        assert!(store.get("unknown").await.is_none());
    }
}
