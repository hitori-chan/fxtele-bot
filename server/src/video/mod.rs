//! 1080p videos (`docs/server.md`, "1080p videos"): Facebook keeps them as separate DASH video
//! and audio files, so the server downloads both, joins them into one MP4 ([`mp4`]) and uploads it
//! to a bucket with S3's API ([`bucket`]), which Telegram downloads from: the server never sends
//! video itself. Each file is made once, two at most at a time, and kept until it's old or the
//! space runs out, which is looked at whenever a file is made. Off unless the dashboard turns it
//! on, and there's a bucket.

pub mod bucket;
pub mod mp4;

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, ensure};
use arc_swap::ArcSwap;
use futures_util::StreamExt;
use tokio::sync::{OnceCell, Semaphore};
use tracing::{info, warn};

pub use self::bucket::{Bucket, BucketConfig};
use crate::extract::HdVideo;
use crate::extract::facebook::fetch::USER_AGENT;
use crate::settings::{Settings, VideoQuality};
use crate::sync::lock;

/// Joined files older than this are deleted; Telegram fetches a video right after the preview.
const MAX_AGE: Duration = Duration::from_secs(3 * 86_400);
/// A video that couldn't be made isn't tried again sooner than this.
const RETRY_AFTER: Duration = Duration::from_secs(10 * 60);
const BUILDS_AT_ONCE: usize = 2;
const MB: u64 = 1024 * 1024;
/// Failures kept for the dashboard.
const RECENT_FAILURES: usize = 20;

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
                making: moka::future::Cache::builder().max_capacity(1000).build(),
                failed: moka::future::Cache::builder().max_capacity(10_000).time_to_live(RETRY_AFTER).build(),
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
        self.wanted(settings) && !lock(&self.inner.health).failing.as_ref().is_some_and(|f| now() < f.retry_at())
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

    /// Our files in the bucket, by name: size and when they were made.
    async fn list(&self, bucket: &Bucket) -> Result<HashMap<String, (u64, i64)>> {
        let listed = self.noted(bucket.list().await.context("can't list the bucket's files"))?;
        Ok(listed.into_iter().filter(|o| Self::is_ours(&o.key)).map(|o| (o.key, (o.size, o.modified))).collect())
    }

    /// The file for item `index` of the post with media ID `media_id`, or `None` when the ID
    /// isn't one the server hands out (16 hex digits).
    pub fn file_name(media_id: &str, index: usize) -> Option<String> {
        let valid = media_id.len() == 16 && media_id.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
        (valid && index < 1000).then(|| format!("{media_id}-{index}.mp4"))
    }

    /// Whether `name` is a file this server makes; nothing else in a bucket is ever deleted.
    fn is_ours(name: &str) -> bool {
        let parsed = name.strip_suffix(".mp4").and_then(|s| s.split_once('-'));
        parsed.and_then(|(id, n)| Self::file_name(id, n.parse().ok()?)).as_deref() == Some(name)
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
            Some(bucket) => self.prune(bucket, 0).await,
            None => Ok(()),
        }
    }

    /// Starts a cleanup to `max_bytes`, after any that's running.
    fn tidy(&self, max_bytes: u64) {
        let this = self.clone();
        tokio::spawn(async move {
            let _one = this.inner.tidying.lock().await;
            if let Some(bucket) = &this.inner.bucket
                && let Err(e) = this.prune(bucket, max_bytes).await
            {
                warn!("{e:#}");
            }
        });
    }

    /// Deletes files older than a few days, then the oldest until the rest fit in `max_bytes`.
    /// The bucket's other files are never touched.
    async fn prune(&self, bucket: &Bucket, max_bytes: u64) -> Result<()> {
        let files = self.inventory(bucket).await?;
        let listed = lock(files).iter().map(|(name, (size, made))| (name.clone(), *size, *made)).collect();
        for name in doomed(listed, max_bytes) {
            bucket.delete(&name).await.context("can't delete old videos from the bucket")?;
            lock(files).remove(&name);
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
        let _slot = self.inner.slots.acquire().await?;
        if self.size(name).await.is_some() {
            return Ok(bucket.config().link(name)); // made while this one waited
        }
        let started = Instant::now();
        let limit = settings.video_megabytes * MB;
        let client = self.client(settings.proxy.as_deref())?;
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
        self.tidy(settings.video_space_megabytes * MB);
        info!(
            file = name,
            bytes = size,
            ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "made a 1080p video"
        );
        Ok(bucket.config().link(name))
    }
}

/// Which of `files` (id, size, made at) to delete: those older than `MAX_AGE`, then the
/// oldest until the rest fit in `max_bytes`.
fn doomed<T>(mut files: Vec<(T, u64, i64)>, max_bytes: u64) -> Vec<T> {
    files.sort_by_key(|f| f.2);
    let oldest = now() - i64::try_from(MAX_AGE.as_secs()).unwrap_or(i64::MAX);
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

    #[test]
    fn file_names_only_for_our_ids() {
        assert_eq!(Videos::file_name("0123456789abcdef", 2).as_deref(), Some("0123456789abcdef-2.mp4"));
        for bad in ["0123456789ABCDEF", "../../etc/passwd", "0123456789abcde", "0123456789abcdeg"] {
            assert!(Videos::file_name(bad, 0).is_none(), "{bad}");
        }
        assert!(Videos::file_name("0123456789abcdef", 5000).is_none());
        assert!(Videos::is_ours("0123456789abcdef-12.mp4"));
        for other in ["0123456789abcdef-12.mp4.part", "holiday.mp4", "0123456789abcdef-x.mp4"] {
            assert!(!Videos::is_ours(other), "{other}");
        }
    }

    #[test]
    fn old_files_go_first_then_the_oldest_over_budget() {
        let day = 86_400;
        let files = vec![
            ("new", 10, now()),
            ("ancient", 1, now() - 4 * day),
            ("older", 10, now() - day),
            ("old", 10, now() - 60),
        ];
        assert_eq!(doomed(files.clone(), 100), ["ancient"], "only what's too old");
        assert_eq!(doomed(files.clone(), 20), ["ancient", "older"], "then the oldest until it fits");
        assert_eq!(doomed(files, 0), ["ancient", "older", "old", "new"]);
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
        let name = Videos::file_name("0123456789abcdef", 0).unwrap();

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

        let broken = Videos::file_name("0123456789abcdef", 1).unwrap();
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
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn uploads_to_the_bucket_listed_once() {
        let (fake, config, videos) = in_bucket().await;
        // Someone else's file in the same bucket is never touched.
        fake.files.lock().unwrap().insert("holiday.mp4".into(), (vec![0; 10], "video/mp4".into(), 0));
        assert_eq!(videos.bucket_health(), BucketHealth::default(), "nothing asked of it yet");
        let addr = cdn().await;
        let name = Videos::file_name("0123456789abcdef", 0).unwrap();

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
        assert_eq!(fake.lists(), 1, "listed once, then kept up to date");

        // After a restart, the first look lists what's there.
        let restarted = Videos::new(Some(config)).unwrap();
        assert!(restarted.size(&name).await.is_some());
        restarted.delete_all().await.unwrap();
        assert_eq!(fake.files.lock().unwrap().keys().collect::<Vec<_>>(), ["holiday.mp4"]);
        assert_eq!(restarted.usage().await, Some(Usage::default()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failing_bucket_pauses_1080p() {
        let (fake, _, videos) = in_bucket().await;
        let addr = cdn().await;
        let settings = best();
        assert_eq!(videos.usage().await, Some(Usage::default()), "listed, then uploads fail");
        *fake.refuse.lock().unwrap() = Some(axum::http::StatusCode::FORBIDDEN);

        let name = Videos::file_name("0123456789abcdef", 0).unwrap();
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
        let other = Videos::file_name("0123456789abcdef", 1).unwrap();
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
        let name = Videos::file_name("0123456789abcdef", 0).unwrap();
        videos.get(&name, &hd(cdn().await, "video.mp4"), "post", &best()).await.unwrap_err();
        assert!(videos.bucket_health().failing.is_some());

        // Serving files asks nothing of the bucket, so only a look after the pause can see it's back.
        *fake.refuse.lock().unwrap() = None;
        videos.usage().await;
        assert_eq!(fake.lists(), 1, "not during the pause");
        lock(&videos.inner.health).failing.as_mut().unwrap().at -= 3600;
        videos.usage().await;
        assert_eq!(fake.lists(), 2, "listed again once it's over");
        assert_eq!(videos.bucket_health().failing, None);
        videos.usage().await;
        assert_eq!(fake.lists(), 2, "and not again while it works");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn refuses_videos_over_the_limit() {
        let (fake, _, videos) = in_bucket().await;
        let addr = cdn().await;
        let settings = Settings { video_megabytes: 0, ..best() }; // anything is too big
        let name = Videos::file_name("0123456789abcdef", 0).unwrap();
        let error = videos.get(&name, &hd(addr, "video.mp4"), "post", &settings).await.unwrap_err();
        assert!(error.contains("limit"), "{error}");
        assert!(videos.size(&name).await.is_none());
        assert!(fake.files.lock().unwrap().is_empty(), "nothing uploaded");
    }
}
