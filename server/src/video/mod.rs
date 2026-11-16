//! 1080p videos (`docs/server.md`, "1080p videos"): Facebook keeps them as separate DASH video
//! and audio files, so the server downloads both, joins them into one MP4 ([`mp4`]) and uploads it
//! to a bucket with S3's API ([`bucket`]), which Telegram downloads from: the server never sends
//! video itself. Each file is made once, two at most at a time, under its source's prefix
//! (`facebook/<id>-<n>.mp4`), and kept for the days the settings say or until the space runs out,
//! which is looked at whenever a file is made and every hour. Off unless the dashboard turns it on,
//! and there's a bucket.

pub mod bucket;
pub mod mp4;

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, ensure};
use arc_swap::ArcSwap;
use futures_util::StreamExt;
use moka::policy::EvictionPolicy;
use tokio::sync::{OnceCell, Semaphore};
use tracing::{info, warn};

pub use self::bucket::{Bucket, BucketConfig};
use crate::extract::facebook::fetch::USER_AGENT;
use crate::extract::{HdVideo, Source};
use crate::settings::{Settings, VideoQuality};
use crate::sync::lock;

/// A video that couldn't be made isn't tried again sooner than this.
const RETRY_AFTER: Duration = Duration::from_secs(10 * 60);
const BUILDS_AT_ONCE: usize = 2;
/// A build that waits longer than this for its turn gives up. Telegram has taken Facebook's file
/// by then, and with videos arriving faster than they're made, old builds would hold up every
/// new one. (A second in tests.)
const TURN_WAIT: Duration = Duration::from_secs(if cfg!(test) { 1 } else { 20 });
const MB: u64 = 1024 * 1024;
/// Failures kept for the dashboard.
const RECENT_FAILURES: usize = 20;

/// Too many videos were being made: not a failure, and the next request tries again.
#[derive(Debug)]
struct Busy;

impl std::fmt::Display for Busy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("busy making other videos")
    }
}

impl std::error::Error for Busy {}

/// A video that couldn't be made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub post: String,
    pub error: String,
}

/// How the bucket has been doing, judged by the requests videos needed: there's no polling.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BucketHealth {
    /// When a request to it last worked.
    pub worked_at: Option<i64>,
    /// The failures since then.
    pub failing: Option<Failing>,
}

/// A run of bucket failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failing {
    pub since: i64,
    /// The latest one; the bucket is tried again `RETRY_AFTER` later.
    pub at: i64,
    pub error: String,
}

impl Failing {
    /// When the next video may try the bucket again.
    pub fn retry_at(&self) -> i64 {
        self.at + i64::try_from(RETRY_AFTER.as_secs()).unwrap_or(i64::MAX)
    }
}

/// How the bucket is doing, for alerts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BucketState {
    /// 1080p is off, or there's no bucket.
    Unused,
    Works,
    Failing(String),
}

/// Joined files in the bucket.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub files: u64,
    pub bytes: u64,
}

/// Joined files by name: size and when they were made.
type Inventory = Mutex<HashMap<String, (u64, i64)>>;

#[derive(Clone)]
pub struct Videos {
    inner: Arc<Inner>,
}

struct Inner {
    bucket: Option<Bucket>,
    /// The bucket's joined files: listed when first needed, then kept up to date, and listed
    /// again when it's time to see whether a failing bucket is back.
    in_bucket: OnceCell<Inventory>,
    relisting: tokio::sync::Mutex<()>,
    health: Mutex<BucketHealth>,
    /// One cleanup at a time.
    tidying: tokio::sync::Mutex<()>,
    /// Builds in progress, shared by everyone waiting for the same file: a link to it.
    making: moka::future::Cache<String, Arc<str>>,
    /// Recent failures, so a broken video isn't downloaded over and over.
    failed: moka::future::Cache<String, Arc<str>>,
    slots: Semaphore,
    client: ArcSwap<(Option<String>, reqwest::Client)>,
    /// Failures the alerts haven't seen yet, by file name.
    unreported: Mutex<HashMap<String, Failure>>,
    /// The latest failures, newest first, with when they happened.
    recent: Mutex<VecDeque<(i64, Failure)>>,
}

fn now() -> i64 {
    jiff::Timestamp::now().as_second()
}

impl Videos {
    /// Without a bucket, there are no 1080p videos.
    pub fn new(bucket: Option<BucketConfig>) -> Result<Self> {
        Ok(Self {
            inner: Arc::new(Inner {
                bucket: bucket.map(Bucket::new).transpose()?,
                in_bucket: OnceCell::new(),
                relisting: tokio::sync::Mutex::new(()),
                health: Mutex::new(BucketHealth::default()),
                tidying: tokio::sync::Mutex::new(()),
                making: moka::future::Cache::builder()
                    .max_capacity(1000)
                    .eviction_policy(EvictionPolicy::lru())
                    .build(),
                failed: moka::future::Cache::builder()
                    .max_capacity(10_000)
                    .eviction_policy(EvictionPolicy::lru())
                    .time_to_live(RETRY_AFTER)
                    .build(),
                slots: Semaphore::new(BUILDS_AT_ONCE),
                client: ArcSwap::from_pointee((None, build_client(None)?)),
                unreported: Mutex::new(HashMap::new()),
                recent: Mutex::new(VecDeque::new()),
            }),
        })
    }

    /// The bucket from `.env`, if there is one.
    pub fn bucket(&self) -> Option<&BucketConfig> {
        self.inner.bucket.as_ref().map(Bucket::config)
    }

    /// Whether the settings ask for 1080p and there's a bucket to keep it in.
    pub fn wanted(&self, settings: &Settings) -> bool {
        settings.video_quality == VideoQuality::Best && self.inner.bucket.is_some()
    }

    /// Whether Telegram gets 1080p now: when it's wanted, except for a while after the bucket
    /// failed, when Telegram gets 720p rather than waiting on a broken bucket.
    pub fn ready(&self, settings: &Settings) -> bool {
        self.wanted(settings) && !self.paused()
    }

    /// Whether the bucket failed and isn't to be tried again yet.
    fn paused(&self) -> bool {
        lock(&self.inner.health).failing.as_ref().is_some_and(|f| now() < f.retry_at())
    }

    /// What's wrong with the bucket, when 1080p is wanted and the bucket hasn't worked since.
    pub fn bucket_problem(&self, settings: &Settings) -> Option<String> {
        match self.bucket_state(settings) {
            BucketState::Failing(error) => Some(error),
            BucketState::Unused | BucketState::Works => None,
        }
    }

    pub fn bucket_state(&self, settings: &Settings) -> BucketState {
        if !self.wanted(settings) {
            return BucketState::Unused;
        }
        match &lock(&self.inner.health).failing {
            Some(f) => BucketState::Failing(f.error.clone()),
            None => BucketState::Works,
        }
    }

    /// Whether the bucket failed and its pause is over, so it's time to try it again.
    fn retry_due(&self) -> bool {
        lock(&self.inner.health).failing.as_ref().is_some_and(|f| now() >= f.retry_at())
    }

    pub fn bucket_health(&self) -> BucketHealth {
        lock(&self.inner.health).clone()
    }

    /// Notes how a bucket request went, for [`Self::ready`] and the dashboard.
    fn noted<T>(&self, result: Result<T>) -> Result<T> {
        let mut health = lock(&self.inner.health);
        let at = now();
        match &result {
            Ok(_) => *health = BucketHealth { worked_at: Some(at), failing: None },
            Err(e) => {
                let since = health.failing.as_ref().map_or(at, |f| f.since);
                health.failing = Some(Failing { since, at, error: format!("{e:#}") });
            }
        }
        result
    }

    /// The bucket's joined files, listed on first use. Serving a file asks nothing of the bucket,
    /// so once a failure's pause is over, the next look lists it again to see whether it's back.
    async fn inventory(&self, bucket: &Bucket) -> Result<&Inventory> {
        let files = self.inner.in_bucket.get_or_try_init(|| async { self.list(bucket).await.map(Mutex::new) }).await?;
        if self.retry_due()
            && let Ok(_one) = self.inner.relisting.try_lock()
        {
            let started = now();
            if let Ok(mut listed) = self.list(bucket).await {
                let mut files = lock(files);
                // A file uploaded while it listed may not be in the list.
                listed.extend(files.drain().filter(|(_, (_, made))| *made >= started));
                *files = listed;
            }
        }
        Ok(files)
    }

    /// Our files in the bucket, by name: size and when they were made. Only the sources' prefixes
    /// are listed, so the bucket's other files aren't even read.
    async fn list(&self, bucket: &Bucket) -> Result<HashMap<String, (u64, i64)>> {
        let mut files = HashMap::new();
        for source in Source::ALL {
            let listed = bucket.list(&format!("{}/", source.as_str())).await;
            let listed = self.noted(listed.context("can't list the bucket's files"))?;
            files.extend(listed.into_iter().filter(|o| Self::is_ours(&o.key)).map(|o| (o.key, (o.size, o.modified))));
        }
        Ok(files)
    }

    /// The file for item `index` of a post from `source` with media ID `media_id`, or `None` when
    /// the ID isn't one the server hands out (16 hex digits).
    pub fn file_name(source: Source, media_id: &str, index: usize) -> Option<String> {
        let valid = media_id.len() == 16 && media_id.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
        (valid && index < 1000).then(|| format!("{}/{media_id}-{index}.mp4", source.as_str()))
    }

    /// Whether `name` is exactly what [`Self::file_name`] gives; nothing else in a bucket is ever
    /// deleted.
    fn is_ours(name: &str) -> bool {
        let parsed = name.split_once('/').and_then(|(prefix, file)| {
            let (id, n) = file.strip_suffix(".mp4")?.split_once('-')?;
            Self::file_name(Source::from_name(prefix)?, id, n.parse().ok()?)
        });
        parsed.as_deref() == Some(name)
    }

    /// The finished file for item `index` of media ID `media_id`, whichever source it's from, so
    /// serving it needs no post.
    pub async fn made(&self, media_id: &str, index: usize) -> Option<String> {
        let files = self.inventory(self.inner.bucket.as_ref()?).await.ok()?;
        let files = lock(files);
        Source::ALL.into_iter().filter_map(|s| Self::file_name(s, media_id, index)).find(|n| files.contains_key(n))
    }

    /// The size of the finished file, if it's been made.
    pub async fn size(&self, name: &str) -> Option<u64> {
        let bucket = self.inner.bucket.as_ref()?;
        let files = self.inventory(bucket).await.ok()?;
        lock(files).get(name).map(|f| f.0)
    }

    /// A fresh download link for `name`, made or not.
    pub fn link(&self, name: &str) -> Option<String> {
        Some(self.inner.bucket.as_ref()?.config().link(name))
    }

    /// A download link for the joined file: made now, or waited for if it's being made. `post`
    /// names the post in alerts. A failure is remembered for a while and returned again without
    /// retrying.
    pub async fn get(&self, name: &str, hd: &HdVideo, post: &str, settings: &Settings) -> Result<Arc<str>, Arc<str>> {
        if !self.ready(settings) {
            return Err("1080p videos are off".into());
        }
        let Some(bucket) = &self.inner.bucket else { return Err("there's no bucket set up".into()) };
        // Without the list, a file made couldn't be recorded, nor ever deleted: nothing is made.
        // That's the bucket failing, which has its own alert, not this video.
        let files = self.inventory(bucket).await.map_err(|e| Arc::<str>::from(format!("{e:#}")))?;
        if lock(files).contains_key(name) {
            return Ok(bucket.config().link(name).into());
        }
        let key = name.to_string();
        if let Some(error) = self.inner.failed.get(&key).await {
            return Err(error);
        }
        let build = {
            let (this, name, hd, settings) = (self.clone(), name.to_string(), hd.clone(), settings.clone());
            // Its own task, so the file is finished even if the request that asked for it is gone.
            async move {
                tokio::spawn(async move { this.make(&name, &hd, &settings).await.map(Arc::from) })
                    .await
                    .map_err(|e| anyhow!("the build stopped: {e}"))?
            }
        };
        let result = self.inner.making.try_get_with(key.clone(), build).await;
        // Done either way: later calls look for the file again.
        self.inner.making.invalidate(&key).await;
        match result {
            Ok(link) => Ok(link),
            Err(error) if error.is::<Busy>() => Err(error.to_string().into()),
            Err(error) => {
                let message: Arc<str> = format!("{error:#}").into();
                // Everyone waiting gets the error; only the first records it.
                if self.inner.failed.entry(key).or_insert(Arc::clone(&message)).await.is_fresh() {
                    warn!(file = name, "couldn't make a 1080p video: {message}");
                    let failure = Failure { post: post.to_string(), error: message.to_string() };
                    lock(&self.inner.unreported).insert(name.to_string(), failure.clone());
                    let mut recent = lock(&self.inner.recent);
                    recent.push_front((now(), failure));
                    recent.truncate(RECENT_FAILURES);
                }
                Err(message)
            }
        }
    }

    /// Starts making the file in the background, so it's ready when Telegram asks for it.
    pub fn prepare(&self, name: String, hd: HdVideo, post: String, settings: Arc<Settings>) {
        let this = self.clone();
        tokio::spawn(async move {
            let _ = this.get(&name, &hd, &post, &settings).await;
        });
    }

    /// Failures since the last call, for the alerts.
    pub fn take_failures(&self) -> Vec<Failure> {
        lock(&self.inner.unreported).drain().map(|(_, failure)| failure).collect()
    }

    /// The latest failures, newest first, with when they happened.
    pub fn recent_failures(&self) -> Vec<(i64, Failure)> {
        lock(&self.inner.recent).iter().cloned().collect()
    }

    /// Joined files in the bucket, or `None` when there's no bucket or it can't be listed. The
    /// first look lists it.
    pub async fn usage(&self) -> Option<Usage> {
        let files = self.inventory(self.inner.bucket.as_ref()?).await.ok()?;
        let files = lock(files);
        Some(Usage { files: files.len() as u64, bytes: files.values().map(|v| v.0).sum() })
    }

    /// Deletes every joined file from the bucket.
    pub async fn delete_all(&self) -> Result<()> {
        let _one = self.inner.tidying.lock().await;
        match &self.inner.bucket {
            Some(bucket) => self.prune(bucket, 0, 0).await,
            None => Ok(()),
        }
    }

    /// Starts a cleanup to the settings' space and days, after any that's running.
    fn tidy(&self, settings: &Settings) {
        let (this, max_bytes, days) = (self.clone(), settings.video_space_megabytes * MB, settings.video_days);
        tokio::spawn(async move {
            let _one = this.inner.tidying.lock().await;
            if let Some(bucket) = &this.inner.bucket
                && let Err(e) = this.prune(bucket, max_bytes, days).await
            {
                warn!("{e:#}");
            }
        });
    }

    /// The hourly cleanup, so files go after their days even when no new video is made, 1080p on
    /// or off. A failing bucket is left alone during its pause; after it, the look here lists it
    /// again like any other, and if it still fails that's already on the Videos page and in the
    /// alerts.
    pub async fn tidy_up(&self, settings: &Settings) -> Result<()> {
        let Some(bucket) = &self.inner.bucket else { return Ok(()) };
        if self.paused() {
            return Ok(());
        }
        let _one = self.inner.tidying.lock().await;
        if self.inventory(bucket).await.is_err() || lock(&self.inner.health).failing.is_some() {
            return Ok(());
        }
        self.prune(bucket, settings.video_space_megabytes * MB, settings.video_days).await
    }

    /// Deletes files older than `days`, then the oldest until the rest fit in `max_bytes`. The
    /// bucket's other files are never touched.
    async fn prune(&self, bucket: &Bucket, max_bytes: u64, days: u64) -> Result<()> {
        let files = self.inventory(bucket).await?;
        let listed = lock(files).iter().map(|(name, (size, made))| (name.clone(), *size, *made)).collect();
        for name in doomed(listed, max_bytes, days) {
            // Only ever our own names, whatever the list held.
            if !Self::is_ours(&name) {
                continue;
            }
            // Out of the list first, so no link is handed out for a file being deleted.
            let Some(entry) = lock(files).remove(&name) else { continue };
            if let Err(e) = bucket.delete(&name).await {
                lock(files).insert(name, entry);
                return Err(e.context("can't delete old videos from the bucket"));
            }
        }
        Ok(())
    }

    /// A client for Facebook's CDN with this proxy setting, built once per setting.
    pub fn client(&self, proxy: Option<&str>) -> Result<reqwest::Client> {
        let current = self.inner.client.load();
        if current.0.as_deref() == proxy {
            return Ok(current.1.clone());
        }
        let client = build_client(proxy)?;
        self.inner.client.store(Arc::new((proxy.map(str::to_string), client.clone())));
        Ok(client)
    }

    async fn make(&self, name: &str, hd: &HdVideo, settings: &Settings) -> Result<String> {
        let bucket = self.inner.bucket.as_ref().context("there's no bucket set up")?;
        let _slot = tokio::time::timeout(TURN_WAIT, self.inner.slots.acquire()).await.map_err(|_| Busy)??;
        if self.size(name).await.is_some() {
            return Ok(bucket.config().link(name)); // made while this one waited
        }
        let started = Instant::now();
        let limit = settings.video_megabytes * MB;
        // Downloaded through the proxy of the site the video is from, named by the file's prefix.
        let source =
            name.split_once('/').and_then(|(prefix, _)| Source::from_name(prefix)).context("not one of our files")?;
        let client = self.client(settings.source(source).proxy.as_deref())?;
        let audio = async {
            match &hd.audio {
                Some(url) => download(&client, url, limit).await.map(Some),
                None => Ok(None),
            }
        };
        let (video, audio) = tokio::try_join!(download(&client, &hd.video, limit), audio)?;
        let joined = tokio::task::spawn_blocking(move || {
            let mut inputs: Vec<&[u8]> = vec![&video];
            inputs.extend(audio.as_deref());
            mp4::join(&inputs)
        })
        .await??;
        let size = joined.len() as u64;
        ensure!(
            size <= limit,
            "the joined video is {} MB, over the {} MB limit",
            size.div_ceil(MB),
            settings.video_megabytes
        );

        let uploaded = bucket.put(name, joined, "video/mp4").await.context("can't upload it to the bucket");
        self.noted(uploaded)?;
        if let Ok(files) = self.inventory(bucket).await {
            lock(files).insert(name.to_string(), (size, now()));
        }
        // Room for the new file is made afterwards, so a failure never costs an older one, and in
        // the background, so Telegram doesn't wait for it.
        self.tidy(settings);
        info!(
            file = name,
            bytes = size,
            ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "made a 1080p video"
        );
        Ok(bucket.config().link(name))
    }
}

/// Which of `files` (id, size, made at) to delete: those older than `days`, then the oldest
/// until the rest fit in `max_bytes`.
fn doomed<T>(mut files: Vec<(T, u64, i64)>, max_bytes: u64, days: u64) -> Vec<T> {
    files.sort_by_key(|f| f.2);
    let oldest = now().saturating_sub(i64::try_from(days.saturating_mul(86_400)).unwrap_or(i64::MAX));
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    let mut doomed = Vec::new();
    for (id, size, made) in files {
        if made < oldest || total > max_bytes {
            total -= size;
            doomed.push(id);
        }
    }
    doomed
}

fn build_client(proxy: Option<&str>) -> Result<reqwest::Client> {
    let mut builder = crate::net::client_builder()?
        .user_agent(USER_AGENT)
        .read_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(60));
    if let Some(proxy) = proxy {
        builder = builder.proxy(reqwest::Proxy::all(proxy)?);
    }
    Ok(builder.build()?)
}

/// Downloads a whole stream, refusing anything over `limit` bytes.
async fn download(client: &reqwest::Client, url: &str, limit: u64) -> Result<Vec<u8>> {
    let response = client.get(url).send().await.map_err(|e| anyhow!("download failed: {}", crate::net::describe(e)))?;
    let status = response.status();
    ensure!(status.is_success(), "Facebook's CDN answered {status}");
    let over = || anyhow!("a stream is over the {} MB limit", limit / MB);
    if response.content_length().is_some_and(|length| length > limit) {
        return Err(over());
    }
    // Sized once from the length Facebook gives, already checked against the limit.
    let mut body = Vec::with_capacity(response.content_length().map_or(0, |n| usize::try_from(n).unwrap_or(0)));
    let mut chunks = response.bytes_stream();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(|e| anyhow!("the download broke off: {}", crate::net::describe(e)))?;
        if (body.len() + chunk.len()) as u64 > limit {
            return Err(over());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[cfg(test)]
pub mod tests {
    use std::net::SocketAddr;

    use axum::Router;
    use axum::routing::get;

    use super::*;

    const VIDEO: &[u8] = include_bytes!("../../tests/fixtures/video/video.mp4");
    const AUDIO: &[u8] = include_bytes!("../../tests/fixtures/video/audio.mp4");

    /// Serves the test streams at `/video.mp4` and `/audio.mp4`, and a 403 at `/denied.mp4`.
    pub async fn cdn() -> SocketAddr {
        let app = Router::new()
            .route("/video.mp4", get(|| async { VIDEO }))
            .route("/audio.mp4", get(|| async { AUDIO }))
            .route("/denied.mp4", get(|| async { (axum::http::StatusCode::FORBIDDEN, "URL signature mismatch") }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        addr
    }

    pub fn hd(addr: SocketAddr, video: &str) -> HdVideo {
        HdVideo {
            video: format!("http://{addr}/{video}"),
            audio: Some(format!("http://{addr}/audio.mp4")),
            width: 96,
            height: 64,
            codec: "avc1".into(),
            bytes: Some(40_000),
        }
    }

    /// 1080p on.
    pub fn best() -> Settings {
        Settings { video_quality: VideoQuality::Best, ..Settings::default() }
    }

    /// A fake bucket, and videos kept in it.
    async fn in_bucket() -> (bucket::tests::FakeS3, BucketConfig, Videos) {
        let (fake, config) = bucket::tests::FakeS3::start().await;
        let videos = Videos::new(Some(config.clone())).unwrap();
        (fake, config, videos)
    }

    /// Item `n` of a Facebook post's videos.
    fn file(n: usize) -> String {
        Videos::file_name(Source::Facebook, "0123456789abcdef", n).unwrap()
    }

    #[test]
    fn file_names_only_for_our_ids() {
        assert_eq!(file(2), "facebook/0123456789abcdef-2.mp4");
        for bad in ["0123456789ABCDEF", "../../etc/passwd", "0123456789abcde", "0123456789abcdeg"] {
            assert!(Videos::file_name(Source::Facebook, bad, 0).is_none(), "{bad}");
        }
        assert!(Videos::file_name(Source::Facebook, "0123456789abcdef", 5000).is_none());
        assert!(Videos::is_ours("facebook/0123456789abcdef-12.mp4"));
        let others = [
            "0123456789abcdef-12.mp4", // the flat names from before sources
            "other/0123456789abcdef-0.mp4",
            "Facebook/0123456789abcdef-0.mp4",
            "facebook/0123456789ABCDEF-0.mp4",
            "facebook/../x",
            "facebook/../0123456789abcdef-0.mp4",
            "facebook/x/0123456789abcdef-0.mp4",
            "facebook//0123456789abcdef-0.mp4",
            "/facebook/0123456789abcdef-0.mp4",
            "facebook/0123456789abcdef-01.mp4",
            "facebook/0123456789abcdef-+1.mp4",
            "facebook/0123456789abcdef-12.mp4.part",
            "facebook/0123456789abcdef-x.mp4",
            "facebook/holiday.mp4",
            "holiday.mp4",
            "",
        ];
        for other in others {
            assert!(!Videos::is_ours(other), "{other}");
        }
    }

    #[tokio::test]
    async fn ready_follows_the_settings() {
        let without = Videos::new(None).unwrap();
        assert!(!without.ready(&Settings::default()), "off by default");
        assert!(!without.ready(&best()), "no bucket set up");
        let (_fake, _, with) = in_bucket().await;
        assert!(!with.ready(&Settings::default()));
        assert!(with.ready(&best()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn makes_a_file_once_and_remembers_failures() {
        let (fake, config, videos) = in_bucket().await;
        let addr = cdn().await;
        let settings = best();
        let name = file(0);

        let good = hd(addr, "video.mp4");
        let (a, b) = tokio::join!(
            videos.get(&name, &good, "https://www.facebook.com/reel/1", &settings),
            videos.get(&name, &good, "https://www.facebook.com/reel/1", &settings)
        );
        let link = a.unwrap();
        assert_eq!(b.unwrap(), link);
        assert!(link.starts_with(&format!("{}/videos/{name}?X-Amz-", config.api.as_str().trim_end_matches('/'))));
        let joined = fake.files.lock().unwrap()[&name].0.clone();
        let (order, _, tracks) = mp4::tests::read(&joined);
        assert_eq!(order, ["ftyp", "moov", "mdat"]);
        assert_eq!(tracks.len(), 2);
        assert!(videos.take_failures().is_empty());

        let broken = file(1);
        let error = videos.get(&broken, &hd(addr, "denied.mp4"), "https://www.facebook.com/reel/2", &settings).await;
        assert!(error.unwrap_err().contains("403"));
        let again = videos.get(&broken, &hd(addr, "video.mp4"), "https://www.facebook.com/reel/2", &settings).await;
        assert!(again.is_err(), "not retried right away");
        let failures = videos.take_failures();
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert_eq!(failures[0].post, "https://www.facebook.com/reel/2");
        assert!(videos.take_failures().is_empty(), "reported once");
        assert_eq!(videos.recent_failures().len(), 1, "the dashboard keeps them");
        assert!(videos.bucket_health().failing.is_none(), "Facebook's fault, not the bucket's");

        let off = videos.get(&name, &good, "post", &Settings::default()).await;
        assert!(off.unwrap_err().contains("off"));

        // With every slot taken past the wait, a build gives up without counting as a failure.
        let _taken = videos.inner.slots.acquire_many(BUILDS_AT_ONCE as u32).await.unwrap();
        let busy = videos.get(&file(2), &good, "https://www.facebook.com/reel/3", &settings).await;
        assert!(busy.unwrap_err().contains("busy"));
        assert!(videos.take_failures().is_empty() && videos.inner.failed.get(&file(2)).await.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn uploads_to_the_bucket_listed_once() {
        let (fake, config, videos) = in_bucket().await;
        // Someone else's files in the same bucket, and flat names from before sources, are never
        // touched.
        let others = ["0123456789abcdef-0.mp4", "holiday.mp4", "other/0123456789abcdef-0.mp4"];
        for other in others {
            fake.files.lock().unwrap().insert(other.into(), (vec![0; 10], "video/mp4".into(), 0));
        }
        assert_eq!(videos.bucket_health(), BucketHealth::default(), "nothing asked of it yet");
        let addr = cdn().await;
        let name = file(0);

        let link = videos.get(&name, &hd(addr, "video.mp4"), "post", &best()).await.unwrap();
        let downloaded = reqwest::get(&*link).await.unwrap().bytes().await.unwrap();
        assert_eq!(downloaded, fake.files.lock().unwrap()[&name].0, "the link downloads it");
        let again = videos.link(&name).unwrap();
        assert_eq!(reqwest::get(&again).await.unwrap().bytes().await.unwrap(), downloaded, "so does a fresh one");
        let (size, kind) = {
            let files = fake.files.lock().unwrap();
            (files[&name].0.len() as u64, files[&name].1.clone())
        };
        assert_eq!(kind, "video/mp4");
        assert_eq!(videos.usage().await, Some(Usage { files: 1, bytes: size }));
        assert_eq!(videos.size(&name).await, Some(size));
        assert!(videos.bucket_health().worked_at.is_some());
        assert_eq!(fake.lists(), Source::ALL.len(), "listed once (a call per site), then kept up to date");

        // After a restart, the first look lists what's there.
        let restarted = Videos::new(Some(config)).unwrap();
        assert_eq!(restarted.made("0123456789abcdef", 0).await, Some(name), "found without the post");
        assert_eq!(restarted.made("0123456789abcdef", 1).await, None);
        restarted.delete_all().await.unwrap();
        assert_eq!(fake.files.lock().unwrap().keys().map(String::as_str).collect::<Vec<_>>(), others);
        assert_eq!(restarted.usage().await, Some(Usage::default()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failing_bucket_pauses_1080p() {
        let (fake, _, videos) = in_bucket().await;
        let addr = cdn().await;
        let settings = best();
        assert_eq!(videos.usage().await, Some(Usage::default()), "listed, then uploads fail");
        *fake.refuse.lock().unwrap() = Some(axum::http::StatusCode::FORBIDDEN);

        let name = file(0);
        let error = videos.get(&name, &hd(addr, "video.mp4"), "post", &settings).await.unwrap_err();
        assert!(error.contains("403"), "{error}");
        let failing = videos.bucket_health().failing.unwrap();
        assert!(failing.error.contains("AccessDenied"), "{failing:?}");
        assert!(!videos.ready(&settings), "720p for a while");
        assert!(videos.wanted(&settings));
        assert_eq!(videos.bucket_problem(&settings), Some(failing.error.clone()));
        assert_eq!(videos.bucket_problem(&Settings::default()), None, "not in use");

        // Later, the next video tries again; a request that works clears it.
        lock(&videos.inner.health).failing.as_mut().unwrap().at -= 3600;
        assert!(videos.ready(&settings));
        *fake.refuse.lock().unwrap() = None;
        videos.get(&name, &hd(addr, "video.mp4"), "post", &settings).await.unwrap_err(); // remembered for this video
        let other = file(1);
        videos.get(&other, &hd(addr, "video.mp4"), "post", &settings).await.unwrap();
        assert_eq!(videos.bucket_health().failing, None);
        assert_eq!(videos.bucket_problem(&settings), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_look_after_the_pause_tries_the_bucket_again() {
        let (fake, _, videos) = in_bucket().await;
        assert_eq!(videos.usage().await, Some(Usage::default()));
        assert_eq!(Videos::new(None).unwrap().usage().await, None, "no bucket");
        *fake.refuse.lock().unwrap() = Some(axum::http::StatusCode::FORBIDDEN);
        let name = file(0);
        videos.get(&name, &hd(cdn().await, "video.mp4"), "post", &best()).await.unwrap_err();
        assert!(videos.bucket_health().failing.is_some());

        // Serving files asks nothing of the bucket, so only a look after the pause can see it's back.
        *fake.refuse.lock().unwrap() = None;
        videos.usage().await;
        // A listing is one call per site's prefix.
        let listing = Source::ALL.len();
        assert_eq!(fake.lists(), listing, "not during the pause");
        lock(&videos.inner.health).failing.as_mut().unwrap().at -= 3600;
        videos.usage().await;
        assert_eq!(fake.lists(), 2 * listing, "listed again once it's over");
        assert_eq!(videos.bucket_health().failing, None);
        videos.usage().await;
        assert_eq!(fake.lists(), 2 * listing, "and not again while it works");
    }

    /// Puts a file of `bytes` made `ago` seconds back straight into the fake bucket.
    fn put(fake: &bucket::tests::FakeS3, name: &str, bytes: usize, ago: i64) {
        fake.files.lock().unwrap().insert(name.into(), (vec![0; bytes], "video/mp4".into(), now() - ago));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_hourly_cleanup_deletes_only_our_old_files() {
        let (fake, _, videos) = in_bucket().await;
        let day = 86_400;
        put(&fake, &file(0), 10, 5 * day);
        put(&fake, &file(1), 10, 2 * day);
        put(&fake, &file(2), 700_000, 3600);
        put(&fake, &file(3), 700_000, 0);
        let others = ["0123456789abcdef-0.mp4", "facebook/holiday.mp4", "other/0123456789abcdef-0.mp4"];
        for other in others {
            put(&fake, other, 10, 30 * day);
        }
        let left = || fake.files.lock().unwrap().keys().cloned().collect::<Vec<_>>();
        let ours = |n: &[usize]| {
            let mut all: Vec<String> = n.iter().map(|&n| file(n)).chain(others.map(String::from)).collect();
            all.sort();
            all
        };
        Videos::new(None).unwrap().tidy_up(&Settings::default()).await.unwrap();

        videos.tidy_up(&Settings { video_days: 7, ..Settings::default() }).await.unwrap();
        assert_eq!(left(), ours(&[0, 1, 2, 3]), "all made within the week");
        // 1080p is off: what's there still goes after its days.
        videos.tidy_up(&Settings::default()).await.unwrap();
        assert_eq!(left(), ours(&[1, 2, 3]), "three days by default");
        videos.tidy_up(&Settings { video_space_megabytes: 1, ..Settings::default() }).await.unwrap();
        assert_eq!(left(), ours(&[3]), "then the oldest until the rest fit");
        assert_eq!(fake.lists(), Source::ALL.len(), "listed once (a call per site), then kept up to date");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_hourly_cleanup_leaves_a_failing_bucket_be() {
        let (fake, _, videos) = in_bucket().await;
        put(&fake, &file(0), 10, 5 * 86_400);
        let failing = Failing { since: now(), at: now(), error: "the bucket answered 403 Forbidden".into() };
        lock(&videos.inner.health).failing = Some(failing);

        videos.tidy_up(&Settings::default()).await.unwrap();
        assert_eq!(fake.lists(), 0, "nothing asked of it during the pause");
        // After the pause it's looked at like any other time, quietly if it still fails.
        *fake.refuse.lock().unwrap() = Some(axum::http::StatusCode::FORBIDDEN);
        lock(&videos.inner.health).failing.as_mut().unwrap().at -= 3600;
        videos.tidy_up(&Settings::default()).await.unwrap();
        assert!(videos.paused(), "a new pause");
        assert!(fake.files.lock().unwrap().contains_key(&file(0)));

        *fake.refuse.lock().unwrap() = None;
        lock(&videos.inner.health).failing.as_mut().unwrap().at -= 3600;
        videos.tidy_up(&Settings::default()).await.unwrap();
        assert!(fake.files.lock().unwrap().is_empty(), "back, and tidied");
        assert_eq!(videos.bucket_health().failing, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn refuses_videos_over_the_limit() {
        let (fake, _, videos) = in_bucket().await;
        let addr = cdn().await;
        let settings = Settings { video_megabytes: 0, ..best() }; // anything is too big
        let name = file(0);
        let error = videos.get(&name, &hd(addr, "video.mp4"), "post", &settings).await.unwrap_err();
        assert!(error.contains("limit"), "{error}");
        assert!(videos.size(&name).await.is_none());
        assert!(fake.files.lock().unwrap().is_empty(), "nothing uploaded");
    }
}
