//! Extracted posts and failures, bounded by size. Concurrent requests for one post share a fetch.

use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use hmac::{Hmac, KeyInit, Mac};
use moka::Expiry;
use moka::future::Cache as Moka;
use sha2::Sha256;

use crate::db::Db;
use crate::extract::{ExtractError, Post};

/// Where the media ID key is kept.
const ID_KEY: &str = "media_id_key";

/// One cached outcome, success or failure, with how long to keep it.
#[derive(Debug, Clone)]
pub struct Entry {
    pub outcome: Result<Arc<Post>, ExtractError>,
    ttl: Duration,
}

impl Entry {
    /// Kept for `ttl`, or until a minute before a media URL expires, whichever is first.
    pub fn post(post: Post, ttl: Duration) -> Self {
        let ttl = match post.expires_at() {
            Some(expires) => {
                let left = expires - jiff::Timestamp::now().as_second() - 60;
                ttl.min(Duration::from_secs(u64::try_from(left).unwrap_or(0).max(1)))
            }
            None => ttl,
        };
        Self { outcome: Ok(Arc::new(post)), ttl }
    }

    pub const fn failure(error: ExtractError, ttl: Duration) -> Self {
        Self { outcome: Err(error), ttl }
    }

    fn weight(&self) -> u32 {
        let bytes = match &self.outcome {
            Ok(post) => post.weight(),
            Err(e) => 128 + e.message.len(),
        };
        u32::try_from(bytes).unwrap_or(u32::MAX)
    }
}

struct PerEntryTtl;

impl Expiry<String, Entry> for PerEntryTtl {
    fn expire_after_create(&self, _: &String, entry: &Entry, _: Instant) -> Option<Duration> {
        Some(entry.ttl)
    }
}

pub struct Cache {
    inner: ArcSwap<(u64, Moka<String, Entry>)>,
    /// Short IDs for `/_/m/` media links → cache key. Never holds URLs taken from requests.
    media_ids: Moka<String, String>,
    /// What media IDs are made with, so nobody can work out a post's ID from its link and ask
    /// whether it was previewed, or for its video.
    id_key: Vec<u8>,
}

/// The media ID key: made on first run and kept in the database, so IDs, and the bucket's file
/// names, stay the same across restarts.
pub async fn id_key(db: &Db) -> anyhow::Result<Vec<u8>> {
    if let Some(saved) = db.get_json::<String>(ID_KEY).await?
        && let Ok(key) = data_encoding::HEXLOWER.decode(saved.as_bytes())
    {
        return Ok(key);
    }
    let mut key = vec![0; 32];
    getrandom::fill(&mut key)?;
    db.set_json(ID_KEY, &data_encoding::HEXLOWER.encode(&key)).await?;
    Ok(key)
}

impl Cache {
    pub fn new(megabytes: u64, id_key: Vec<u8>) -> Self {
        let media_ids = Moka::builder().max_capacity(50_000).time_to_live(Duration::from_secs(6 * 3600)).build();
        Self { inner: ArcSwap::from_pointee((megabytes, build(megabytes))), media_ids, id_key }
    }

    fn current(&self, megabytes: u64) -> Moka<String, Entry> {
        let inner = self.inner.load();
        if inner.0 == megabytes {
            return inner.1.clone();
        }
        // The size changed in the dashboard: start a fresh cache with the new budget.
        let fresh = build(megabytes);
        self.inner.store(Arc::new((megabytes, fresh.clone())));
        fresh
    }

    /// Returns the cached entry, or runs `init` once for all concurrent callers of the same key.
    /// `Err` from `init` (for example no Facebook budget) is returned but not cached.
    pub async fn get_or_fetch<F, E>(&self, megabytes: u64, key: &str, init: F) -> Result<Entry, Arc<E>>
    where
        F: Future<Output = Result<Entry, E>>,
        E: Send + Sync + 'static,
    {
        self.current(megabytes).try_get_with_by_ref(key, init).await
    }

    pub async fn get(&self, key: &str) -> Option<Entry> {
        self.inner.load().1.get(key).await
    }

    pub async fn insert(&self, megabytes: u64, key: String, entry: Entry) {
        self.current(megabytes).insert(key, entry).await;
    }

    pub async fn remove(&self, key: &str) {
        self.inner.load().1.invalidate(key).await;
    }

    pub fn clear(&self) {
        self.inner.load().1.invalidate_all();
    }

    /// Drops cached failures, for when what caused them may have changed (new cookies).
    pub async fn forget_failures(&self) {
        let cache = &self.inner.load().1;
        for (key, entry) in cache {
            if entry.outcome.is_err() {
                cache.invalidate(&*key).await;
            }
        }
    }

    /// Entry count and estimated bytes.
    pub async fn usage(&self) -> (u64, u64) {
        let cache = self.inner.load().1.clone();
        cache.run_pending_tasks().await;
        (cache.entry_count(), cache.weighted_size())
    }

    /// The short ID used in `/_/m/{id}/{n}` links for a post, remembered so the link resolves later.
    pub async fn media_id(&self, key: &str) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.id_key).expect("HMAC takes keys of any length");
        mac.update(key.as_bytes());
        let id = data_encoding::HEXLOWER.encode(&mac.finalize().into_bytes()[..8]);
        self.media_ids.insert(id.clone(), key.to_string()).await;
        id
    }

    pub async fn key_for_media_id(&self, id: &str) -> Option<String> {
        self.media_ids.get(id).await
    }
}

fn build(megabytes: u64) -> Moka<String, Entry> {
    Moka::builder()
        .max_capacity(megabytes.saturating_mul(1024 * 1024))
        .weigher(|key: &String, entry: &Entry| entry.weight().saturating_add(u32::try_from(key.len()).unwrap_or(0)))
        .expire_after(PerEntryTtl)
        .build()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use crate::extract::facebook::url::Target;
    use crate::extract::fake::FakeExtractor;
    use crate::extract::{Media, Stage};

    fn post() -> Post {
        FakeExtractor::post(&Target::from_request("/reel/1").unwrap())
    }

    #[tokio::test]
    async fn media_ids_need_the_key() {
        let key = "https://www.facebook.com/reel/1";
        let id = Cache::new(8, vec![7; 32]).media_id(key).await;
        assert_eq!(id.len(), 16);
        assert_eq!(Cache::new(8, vec![7; 32]).media_id(key).await, id, "the same key, the same ID");
        assert_ne!(Cache::new(8, vec![8; 32]).media_id(key).await, id, "another server's key, another ID");

        let db = Db::memory().await.unwrap();
        let made = id_key(&db).await.unwrap();
        assert_eq!(made.len(), 32);
        assert_eq!(id_key(&db).await.unwrap(), made, "kept across restarts");
    }

    #[tokio::test]
    async fn concurrent_requests_share_one_fetch() {
        let cache = Cache::new(8, vec![7; 32]);
        let calls = AtomicU32::new(0);
        let fetch = || async {
            calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok::<_, ()>(Entry::post(post(), Duration::from_secs(60)))
        };
        let (a, b) = tokio::join!(cache.get_or_fetch(8, "k", fetch()), cache.get_or_fetch(8, "k", fetch()));
        assert!(a.is_ok() && b.is_ok());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(cache.get("k").await.is_some());
    }

    #[tokio::test]
    async fn errors_from_init_are_not_cached() {
        let cache = Cache::new(8, vec![7; 32]);
        assert!(cache.get_or_fetch(8, "k", async { Err::<Entry, _>("busy") }).await.is_err());
        assert!(cache.get("k").await.is_none());
        let failure = Entry::failure(ExtractError::new(Stage::NotFound, "nothing"), Duration::from_secs(60));
        cache.get_or_fetch(8, "k", async { Ok::<_, ()>(failure) }).await.unwrap();
        assert!(cache.get("k").await.unwrap().outcome.is_err(), "failures are cached");
    }

    #[test]
    fn ttl_stops_before_media_expires() {
        let mut p = post();
        let soon = jiff::Timestamp::now().as_second() + 600;
        p.media = vec![Media::photo(format!("https://x.fbcdn.net/a.jpg?oe={soon:X}"))];
        let entry = Entry::post(p, Duration::from_secs(3600));
        assert!(entry.ttl <= Duration::from_secs(540) && entry.ttl > Duration::from_secs(500));

        let mut p = post();
        p.media = vec![Media::photo("https://x.fbcdn.net/a.jpg?oe=10".into())];
        assert_eq!(Entry::post(p, Duration::from_secs(3600)).ttl, Duration::from_secs(1));
    }

    #[tokio::test]
    async fn media_ids_resolve_and_clear_works() {
        let cache = Cache::new(8, vec![7; 32]);
        let id = cache.media_id("https://www.facebook.com/reel/1").await;
        assert_eq!(id.len(), 16);
        assert_eq!(cache.key_for_media_id(&id).await.as_deref(), Some("https://www.facebook.com/reel/1"));
        assert_eq!(cache.key_for_media_id("0000000000000000").await, None);

        cache.insert(8, "k".into(), Entry::post(post(), Duration::from_secs(60))).await;
        cache.clear();
        assert!(cache.get("k").await.is_none());
    }
}
