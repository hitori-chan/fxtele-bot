//! Request counts by outcome and page type, kept in memory and written to SQLite once a minute,
//! with how long they took in bands, so percentiles can be estimated; and the latest requests,
//! in memory only, for the dashboard's request log.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Result;
use serde::Serialize;
use tokio::sync::broadcast;

use crate::db::Db;
use crate::extract::{PageType, Source, Stage, Target};
use crate::sync::lock;

const MINUTE_DAYS: i64 = 7;
const HOUR_DAYS: i64 = 90;
const KEEP_FAILURES: i64 = 500;
const KEEP_SUSPECTS: usize = 50;
/// Requests the dashboard's log keeps; they're gone when the server restarts.
pub const KEEP_RECENT: usize = 500;

/// Upper ends of the time bands, in milliseconds; a last band holds the rest. Percentiles are
/// estimated within a band, so these are closest together where Telegram's patience runs out.
const BANDS: [u64; 7] = [100, 250, 500, 1000, 2000, 4000, 8000];
const BAND_COLUMNS: [&str; 7] =
    ["under_100ms", "under_250ms", "under_500ms", "under_1s", "under_2s", "under_4s", "under_8s"];

/// The band columns, each written as `form` says (`{}` is the column).
fn bands(form: &str) -> String {
    BAND_COLUMNS.map(|c| form.replace("{}", c)).join(", ")
}

/// An upsert's `SET`: a row's counts added to the ones already there.
fn added() -> String {
    format!(
        "count = count + excluded.count, total_ms = total_ms + excluded.total_ms, \
         max_ms = max(max_ms, excluded.max_ms), {}",
        bands("{} = {} + excluded.{}")
    )
}

/// What [`bands`]`("sum({})")` sums, read from a row starting at column `at`.
fn agg_at(r: &rusqlite::Row, at: usize) -> rusqlite::Result<Agg> {
    let mut bands = [0; 7];
    for (i, band) in bands.iter_mut().enumerate() {
        *band = to_u64(r.get(at + 3 + i)?);
    }
    Ok(Agg { count: to_u64(r.get(at)?), total_ms: to_u64(r.get(at + 1)?), max_ms: to_u64(r.get(at + 2)?), bands })
}

/// The outcome [`Stats::fetched`] counts under: fetches, beside the requests.
pub const FETCH: &str = "fetch";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum Outcome {
    /// A person (or another app) was sent to the post.
    Redirect,
    /// Telegram got a cached post.
    Hit,
    /// Telegram got a freshly extracted post.
    Extracted,
    /// Extraction failed: a fetch that got nothing.
    Failed(Stage),
    /// A failure still in the cache answered again, with no fetch.
    CachedFailure(Stage),
    /// The per-IP limit refused the request.
    RateLimited,
    /// No fetch budget was free in time.
    Busy,
}

impl Outcome {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Redirect => "redirect",
            Self::Hit => "hit",
            Self::Extracted => "extracted",
            Self::Failed(Stage::Fetch) => "failed:fetch",
            Self::Failed(Stage::Redirect) => "failed:redirect",
            Self::Failed(Stage::LoginWall) => "failed:login_wall",
            Self::Failed(Stage::NotFound) => "failed:not_found",
            Self::Failed(Stage::RateLimited) => "failed:rate_limited",
            Self::Failed(Stage::Timeout) => "failed:timeout",
            Self::CachedFailure(Stage::Fetch) => "failed_cached:fetch",
            Self::CachedFailure(Stage::Redirect) => "failed_cached:redirect",
            Self::CachedFailure(Stage::LoginWall) => "failed_cached:login_wall",
            Self::CachedFailure(Stage::NotFound) => "failed_cached:not_found",
            Self::CachedFailure(Stage::RateLimited) => "failed_cached:rate_limited",
            Self::CachedFailure(Stage::Timeout) => "failed_cached:timeout",
            Self::RateLimited => "rate_limited",
            Self::Busy => "busy",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Agg {
    pub count: u64,
    pub total_ms: u64,
    pub max_ms: u64,
    /// How many took less than each of [`BANDS`] (and no less than the one before).
    pub bands: [u64; 7],
}

impl Agg {
    pub fn one(ms: u64) -> Self {
        let mut bands = [0; 7];
        if let Some(band) = BANDS.iter().position(|&end| ms < end) {
            bands[band] = 1;
        }
        Self { count: 1, total_ms: ms, max_ms: ms, bands }
    }

    pub fn merge(&mut self, other: Self) {
        self.count += other.count;
        self.total_ms += other.total_ms;
        self.max_ms = self.max_ms.max(other.max_ms);
        for (band, n) in self.bands.iter_mut().zip(other.bands) {
            *band += n;
        }
    }

    pub fn mean(&self) -> Option<u64> {
        self.total_ms.checked_div(self.count)
    }

    /// The `p`th percentile in milliseconds, estimated by spreading each band's requests evenly
    /// across it; the last band ends at the slowest.
    pub fn percentile(&self, p: u64) -> Option<u64> {
        if self.count == 0 {
            return None;
        }
        let rank = (self.count * p).div_ceil(100).max(1);
        let mut before = 0;
        for (band, &n) in self.bands.iter().chain(&[self.count - self.bands.iter().sum::<u64>()]).enumerate() {
            if before + n >= rank {
                let start = if band == 0 { 0 } else { BANDS[band - 1] };
                let end = BANDS.get(band).copied().unwrap_or(self.max_ms).min(self.max_ms).max(start);
                // Never past the slowest, even when counts lack their band (rows saved before
                // bands were kept fall in "the rest").
                return Some((start + (end - start) * (rank - before) / n).min(self.max_ms));
            }
            before += n;
        }
        Some(self.max_ms)
    }
}

/// A counted request: sent to live dashboard pages, and kept for the request log.
#[derive(Debug, Clone, Serialize)]
pub struct Event {
    /// Counts up from 1 since startup: the log's older pages stay put as new requests come in.
    pub seq: u64,
    pub at: i64,
    pub outcome: &'static str,
    pub source: &'static str,
    pub page_type: &'static str,
    pub ms: u64,
    /// The post's link (the cache key), or none for requests refused before a page was read.
    pub link: Option<String>,
    /// Who got a card: `telegram`, `discord`, or empty for everyone else.
    pub by: &'static str,
}

/// A request that claimed to be Telegram but came from outside its ranges.
#[derive(Debug, Clone, Serialize)]
pub struct Suspect {
    pub at: i64,
    pub ip: IpAddr,
    pub user_agent: String,
    /// The address was a trusted proxy's own, so CLIENT_IP_FROM is probably wrong.
    pub from_proxy: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Failure {
    pub id: i64,
    pub at: i64,
    pub url: String,
    pub page_type: String,
    pub stage: String,
    pub error: String,
}

/// Minute, source, outcome, page type.
type Key = (i64, &'static str, &'static str, &'static str);
/// A minute, a source (or `-`) and a kind of media.
type MediaKey = (i64, &'static str, Media);

/// What was fetched after a preview, counted apart from the requests for previews.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Media {
    /// A photo, or a video's thumbnail.
    Image,
    /// A video: the site's own file.
    Video,
    /// A video above 720p, from the bucket.
    Video1080,
    /// A 1080p video that couldn't be had in time, so the site's own file went instead.
    Fallback,
    /// A post's photos drawn as one.
    Mosaic,
}

impl Media {
    pub const ALL: [Self; 5] = [Self::Image, Self::Video, Self::Video1080, Self::Fallback, Self::Mosaic];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Video => "video",
            Self::Video1080 => "video_1080",
            Self::Fallback => "fallback",
            Self::Mosaic => "mosaic",
        }
    }

    fn parse(label: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.label() == label)
    }
}

/// Source, outcome, page type.
type TotalKey = (String, String, String);

/// The sums [`agg_at`] reads, before the bands.
const SUMS: &str = "sum(count), sum(total_ms), max(max_ms)";

pub struct Stats {
    db: Db,
    pending: Mutex<HashMap<Key, Agg>>,
    media: Mutex<HashMap<MediaKey, u64>>,
    /// One flush at a time, so a page that flushes before reading waits for one under way and
    /// sees its counts.
    flushing: tokio::sync::Mutex<()>,
    recent: Mutex<VecDeque<Event>>,
    suspects: Mutex<VecDeque<Suspect>>,
    /// When a request with Telegram's user agent last came from a proxy address, and when that
    /// was last logged.
    last_proxy_warning: AtomicI64,
    last_proxy_logged: AtomicI64,
    /// The last request's number in the log.
    seq: AtomicU64,
    events: broadcast::Sender<Event>,
}

fn now() -> i64 {
    jiff::Timestamp::now().as_second()
}

impl Stats {
    pub fn new(db: Db) -> Self {
        Self {
            db,
            pending: Mutex::new(HashMap::new()),
            media: Mutex::new(HashMap::new()),
            flushing: tokio::sync::Mutex::new(()),
            recent: Mutex::new(VecDeque::with_capacity(KEEP_RECENT)),
            suspects: Mutex::new(VecDeque::new()),
            last_proxy_warning: AtomicI64::new(0),
            last_proxy_logged: AtomicI64::new(0),
            seq: AtomicU64::new(0),
            events: broadcast::channel(256).0,
        }
    }

    pub fn record(&self, outcome: Outcome, target: Option<&Target>, elapsed: Duration) {
        self.record_by(outcome, target, elapsed, "");
    }

    /// [`Stats::record`], saying who got the card (`telegram`, `discord`) in the request log.
    pub fn record_by(&self, outcome: Outcome, target: Option<&Target>, elapsed: Duration, by: &'static str) {
        let ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        let source = target.map_or("-", |t| t.source.as_str());
        let page_type = target.map_or("-", |t| t.page_type.as_str());
        let label = outcome.label();
        let at = now();
        lock(&self.pending).entry((at / 60 * 60, source, label, page_type)).or_default().merge(Agg::one(ms));
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let link = target.map(|t| t.key().to_string());
        let event = Event { seq, at, outcome: label, source, page_type, ms, link, by };
        {
            let mut recent = lock(&self.recent);
            if recent.len() == KEEP_RECENT {
                recent.pop_back();
            }
            recent.push_front(event.clone());
        }
        if self.events.receiver_count() > 0 {
            // A page closing in between is fine.
            let _ = self.events.send(event);
        }
    }

    /// Counts a fetch from a site, whoever it was for (Telegram, a test link, the playground, a
    /// media link whose post expired), with how long it took. Not a request: it's left out of the
    /// request log and its chart, and read for fetch times and the sites' busiest minutes.
    pub fn fetched(&self, target: &Target, elapsed: Duration) {
        let ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        let key = (now() / 60 * 60, target.source.as_str(), FETCH, target.page_type.as_str());
        lock(&self.pending).entry(key).or_default().merge(Agg::one(ms));
    }

    /// Counts media answered, of `source`'s posts when known.
    pub fn media(&self, kind: Media, source: Option<Source>) {
        self.media_many(kind, source, 1);
    }

    pub fn media_many(&self, kind: Media, source: Option<Source>, count: u64) {
        if count == 0 {
            return;
        }
        let at = now() / 60 * 60;
        *lock(&self.media).entry((at, source.map_or("-", Source::as_str), kind)).or_default() += count;
    }

    /// Media answered from `since` until before `until`, by kind; only `source`'s when given.
    pub async fn media_totals(
        &self,
        since: i64,
        until: i64,
        source: Option<&'static str>,
    ) -> Result<HashMap<Media, u64>> {
        self.flush().await?;
        let stored: Vec<(String, u64)> = self
            .db
            .call(move |c| {
                c.prepare(
                    "SELECT kind, sum(count) FROM media WHERE bucket >= ?1 AND bucket < ?2 AND (?3 IS NULL OR source = ?3)
                     GROUP BY kind",
                )?
                .query_map((since, until, source), |r| Ok((r.get(0)?, r.get::<_, i64>(1)?.unsigned_abs())))?
                .collect()
            })
            .await?;
        Ok(stored.into_iter().filter_map(|(kind, n)| Some((Media::parse(&kind)?, n))).collect())
    }

    /// The latest requests, newest first.
    pub fn recent(&self) -> Vec<Event> {
        lock(&self.recent).iter().cloned().collect()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// Remembers a request with Telegram's user agent from outside Telegram's ranges.
    pub fn suspect(&self, ip: IpAddr, user_agent: &str, from_proxy: bool) {
        let at = now();
        if from_proxy {
            self.last_proxy_warning.store(at, Ordering::Relaxed);
        }
        if from_proxy && at - self.last_proxy_logged.load(Ordering::Relaxed) > 600 {
            self.last_proxy_logged.store(at, Ordering::Relaxed);
            tracing::warn!(
                %ip,
                "a request with Telegram's user agent came from a proxy address; \
                 CLIENT_IP_FROM is probably wrong, so Telegram gets redirected instead of previews"
            );
        }
        let mut suspects = lock(&self.suspects);
        // One row per address, so one that keeps trying can't push the others out.
        suspects.retain(|s| s.ip != ip);
        if suspects.len() == KEEP_SUSPECTS {
            suspects.pop_front();
        }
        let user_agent = user_agent.chars().take(200).collect();
        suspects.push_back(Suspect { at, ip, user_agent, from_proxy });
    }

    pub fn suspects(&self) -> Vec<Suspect> {
        lock(&self.suspects).iter().rev().cloned().collect()
    }

    /// Whether Telegram-looking requests arrived from a proxy address in the last hour.
    pub fn proxy_warning(&self) -> bool {
        now() - self.last_proxy_warning.load(Ordering::Relaxed) < 3600
    }

    pub async fn failure(&self, url: &str, page_type: PageType, stage: Stage, error: &str) -> Result<()> {
        let (url, error) = (url.to_string(), error.chars().take(500).collect::<String>());
        self.db
            .call(move |c| {
                c.execute(
                    "INSERT INTO failures (at, url, page_type, stage, error) VALUES (?1, ?2, ?3, ?4, ?5)",
                    (now(), url, page_type.as_str(), stage.as_str(), error),
                )?;
                c.execute("DELETE FROM failures WHERE id <= (SELECT max(id) FROM failures) - ?1", [KEEP_FAILURES])
            })
            .await?;
        Ok(())
    }

    /// Drops failures once their issue is fixed; it shows again if the failure comes back.
    pub async fn forget_failures(&self, ids: Vec<i64>) -> Result<()> {
        self.db
            .call(move |c| {
                let mut statement = c.prepare("DELETE FROM failures WHERE id = ?1")?;
                for id in ids {
                    statement.execute([id])?;
                }
                Ok(())
            })
            .await
    }

    pub async fn failures(&self, limit: u32) -> Result<Vec<Failure>> {
        self.db
            .call(move |c| {
                let mut statement =
                    c.prepare("SELECT id, at, url, page_type, stage, error FROM failures ORDER BY id DESC LIMIT ?1")?;
                statement
                    .query_map([limit], |r| {
                        Ok(Failure {
                            id: r.get(0)?,
                            at: r.get(1)?,
                            url: r.get(2)?,
                            page_type: r.get(3)?,
                            stage: r.get(4)?,
                            error: r.get(5)?,
                        })
                    })?
                    .collect()
            })
            .await
    }

    /// Adds the counts gathered since the last flush to the one-minute buckets.
    pub async fn flush(&self) -> Result<()> {
        let _one = self.flushing.lock().await;
        let pending = std::mem::take(&mut *lock(&self.pending));
        let media = std::mem::take(&mut *lock(&self.media));
        if pending.is_empty() && media.is_empty() {
            return Ok(());
        }
        // Kept until the write is in, to put back if it fails (the database busy, the disk full).
        let (kept, kept_media) = (pending.clone(), media.clone());
        let written = self
            .db
            .call(move |c| {
                let tx = c.transaction()?;
                {
                    let mut upsert = tx.prepare(&format!(
                        "INSERT INTO stats (size, bucket, source, outcome, page_type, count, total_ms, max_ms, {})
                         VALUES (60, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
                         ON CONFLICT (bucket, size, source, outcome, page_type) DO UPDATE SET {}",
                        bands("{}"),
                        added()
                    ))?;
                    for ((minute, source, outcome, page_type), agg) in pending {
                        let [b0, b1, b2, b3, b4, b5, b6] = agg.bands.map(to_i64);
                        let (count, total, max) = (to_i64(agg.count), to_i64(agg.total_ms), to_i64(agg.max_ms));
                        upsert.execute(rusqlite::params![
                            minute, source, outcome, page_type, count, total, max, b0, b1, b2, b3, b4, b5, b6
                        ])?;
                    }
                    let mut upsert = tx.prepare(
                        "INSERT INTO media (size, bucket, source, kind, count) VALUES (60, ?1, ?2, ?3, ?4)
                         ON CONFLICT (bucket, size, source, kind) DO UPDATE SET count = count + excluded.count",
                    )?;
                    for ((minute, source, kind), count) in media {
                        upsert.execute((minute, source, kind.label(), to_i64(count)))?;
                    }
                }
                tx.commit()
            })
            .await;
        if written.is_err() {
            let mut pending = lock(&self.pending);
            for (key, agg) in kept {
                pending.entry(key).or_default().merge(agg);
            }
            let mut media = lock(&self.media);
            for (key, n) in kept_media {
                *media.entry(key).or_default() += n;
            }
        }
        written
    }

    /// Rolls minute buckets older than 7 days into hours and drops hours older than 90 days.
    pub async fn rollup(&self) -> Result<()> {
        let minute_cutoff = now() - MINUTE_DAYS * 86_400;
        let hour_cutoff = now() - HOUR_DAYS * 86_400;
        self.db
            .call(move |c| {
                let tx = c.transaction()?;
                tx.execute(
                    &format!(
                        "INSERT INTO stats (size, bucket, source, outcome, page_type, count, total_ms, max_ms, {})
                         SELECT 3600, bucket / 3600 * 3600, source, outcome, page_type, sum(count), sum(total_ms),
                           max(max_ms), {}
                         FROM stats WHERE size = 60 AND bucket < ?1
                         GROUP BY bucket / 3600, source, outcome, page_type
                         ON CONFLICT (bucket, size, source, outcome, page_type) DO UPDATE SET {}",
                        bands("{}"),
                        bands("sum({})"),
                        added()
                    ),
                    [minute_cutoff],
                )?;
                tx.execute("DELETE FROM stats WHERE size = 60 AND bucket < ?1", [minute_cutoff])?;
                tx.execute("DELETE FROM stats WHERE size = 3600 AND bucket < ?1", [hour_cutoff])?;
                tx.execute(
                    "INSERT INTO media (size, bucket, source, kind, count)
                     SELECT 3600, bucket / 3600 * 3600, source, kind, sum(count) FROM media
                     WHERE size = 60 AND bucket < ?1 GROUP BY bucket / 3600, source, kind
                     ON CONFLICT (bucket, size, source, kind) DO UPDATE SET count = count + excluded.count",
                    [minute_cutoff],
                )?;
                tx.execute("DELETE FROM media WHERE size = 60 AND bucket < ?1", [minute_cutoff])?;
                tx.execute("DELETE FROM media WHERE size = 3600 AND bucket < ?1", [hour_cutoff])?;
                tx.commit()
            })
            .await
    }

    /// Totals per (source, outcome, page type) from `since` until before `until`, the latest
    /// counts saved first.
    pub async fn totals(&self, since: i64, until: i64) -> Result<HashMap<TotalKey, Agg>> {
        self.flush().await?;
        self.stored(since, until).await
    }

    /// Hourly and minute buckets never cover the same minutes, so they simply add up.
    async fn stored(&self, since: i64, until: i64) -> Result<HashMap<TotalKey, Agg>> {
        self.db
            .call(move |c| {
                let mut statement = c.prepare(&format!(
                    "SELECT source, outcome, page_type, {SUMS}, {} FROM stats
                     WHERE bucket >= ?1 AND bucket < ?2
                     GROUP BY source, outcome, page_type",
                    bands("sum({})")
                ))?;
                statement
                    .query_map((since, until), |r| Ok(((r.get(0)?, r.get(1)?, r.get(2)?), agg_at(r, 3)?)))?
                    .collect()
            })
            .await
    }

    /// Each source's busiest minute of fetches (whoever they were for, whether they worked or not)
    /// since `since`, against which its budget can be read. Minutes are kept for the last 7 days.
    pub async fn peak_fetches(&self, since: i64) -> Result<HashMap<String, u64>> {
        self.flush().await?;
        self.db
            .call(move |c| {
                c.prepare(
                    "SELECT source, max(n) FROM (
                       SELECT source, sum(count) AS n FROM stats
                       WHERE size = 60 AND bucket >= ?1 AND outcome = 'fetch'
                       GROUP BY source, bucket)
                     GROUP BY source",
                )?
                .query_map([since], |r| Ok((r.get(0)?, r.get::<_, i64>(1)?.unsigned_abs())))?
                .collect()
            })
            .await
    }

    /// Requests per bucket of `step` seconds since `since`, by outcome, for charts; only `source`'s
    /// when given.
    pub async fn series(&self, since: i64, step: i64, source: Option<&'static str>) -> Result<Vec<(i64, String, Agg)>> {
        self.flush().await?;
        self.db
            .call(move |c| {
                let mut statement = c.prepare(&format!(
                    "SELECT ?1 + (bucket - ?1) / ?2 * ?2 AS b, outcome, {SUMS}, {} FROM stats
                     WHERE bucket >= ?1 AND (?3 IS NULL OR source = ?3)
                     GROUP BY b, outcome ORDER BY b",
                    bands("sum({})")
                ))?;
                statement.query_map((since, step, source), |r| Ok((r.get(0)?, r.get(1)?, agg_at(r, 2)?)))?.collect()
            })
            .await
    }
}

fn to_i64(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

fn to_u64(n: i64) -> u64 {
    u64::try_from(n).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_labels_name_their_stage() {
        for stage in Stage::ALL {
            assert_eq!(Outcome::Failed(stage).label(), format!("failed:{}", stage.as_str()));
            assert_eq!(Outcome::CachedFailure(stage).label(), format!("failed_cached:{}", stage.as_str()));
        }
    }

    fn reel() -> Target {
        Target::from_link("https://www.facebook.com/reel/1").unwrap()
    }

    #[tokio::test]
    async fn counts_flush_and_add_up() {
        let stats = Stats::new(Db::memory().await.unwrap());
        let post = Target::from_link("https://www.instagram.com/p/DfSi56IFwZK").unwrap();
        stats.record(Outcome::Hit, Some(&reel()), Duration::from_millis(10));
        stats.record(Outcome::Hit, Some(&reel()), Duration::from_millis(30));
        stats.record(Outcome::Hit, Some(&post), Duration::from_millis(5));
        stats.record(Outcome::Redirect, None, Duration::from_millis(1));
        stats.flush().await.unwrap();
        stats.record(Outcome::Hit, Some(&reel()), Duration::from_millis(50));

        let key = |source: &str, outcome: &str, page: &str| (source.to_string(), outcome.to_string(), page.to_string());
        let totals = stats.totals(0, i64::MAX).await.unwrap();
        let hit = totals[&key("facebook", "hit", "reel")];
        assert_eq!((hit.count, hit.total_ms, hit.max_ms), (3, 90, 50));
        assert_eq!(hit.bands[0], 3, "all under 100 ms");
        assert_eq!(totals[&key("instagram", "hit", "post")].count, 1, "sources apart");
        assert_eq!(totals[&key("-", "redirect", "-")].count, 1);
        assert!(stats.totals(0, 1).await.unwrap().is_empty(), "nothing before the window's end");

        stats.flush().await.unwrap();
        let series = stats.series(0, 3600, None).await.unwrap();
        assert_eq!(series.iter().map(|s| s.2.count).sum::<u64>(), 5);
        let instagram = stats.series(0, 3600, Some("instagram")).await.unwrap();
        assert_eq!(instagram.iter().map(|s| s.2.count).sum::<u64>(), 1);
        let latest = stats.recent();
        assert_eq!((latest.len(), latest[0].ms, latest[0].link.as_deref()), (5, 50, Some(reel().key())));
        assert!(stats.peak_fetches(0).await.unwrap().is_empty(), "cache hits aren't fetches");
        stats.record(Outcome::Extracted, Some(&reel()), Duration::from_millis(900));
        assert!(stats.peak_fetches(0).await.unwrap().is_empty(), "fetches are counted where they happen");
        stats.fetched(&reel(), Duration::from_millis(900));
        stats.fetched(&reel(), Duration::from_millis(700));
        assert_eq!(stats.peak_fetches(0).await.unwrap()["facebook"], 2);
        assert_eq!(stats.recent().len(), 6, "and aren't requests in the log");
    }

    #[tokio::test]
    async fn media_is_counted_apart() {
        let stats = Stats::new(Db::memory().await.unwrap());
        stats.media(Media::Image, Some(Source::Facebook));
        stats.media(Media::Image, Some(Source::Instagram));
        stats.media(Media::Video1080, Some(Source::Facebook));
        stats.flush().await.unwrap();
        stats.media(Media::Image, Some(Source::Facebook));
        let all = stats.media_totals(0, i64::MAX, None).await.unwrap();
        assert_eq!((all[&Media::Image], all[&Media::Video1080]), (3, 1), "saved and not yet saved");
        assert_eq!(stats.media_totals(0, i64::MAX, Some("facebook")).await.unwrap()[&Media::Image], 2);
        assert!(stats.totals(0, i64::MAX).await.unwrap().is_empty() && stats.recent().is_empty(), "not previews");

        // Old minutes become hours, and the total stays. Both minutes in one hour, whenever this runs.
        let old = (now() - 10 * 86_400) / 3600 * 3600;
        stats
            .db
            .call(move |c| {
                c.execute(
                    "INSERT INTO media VALUES (60, ?1, 'facebook', 'mosaic', 2), (60, ?2, 'facebook', 'mosaic', 3)",
                    (old, old + 60),
                )
            })
            .await
            .unwrap();
        stats.rollup().await.unwrap();
        let sizes: Vec<(i64, i64)> = stats
            .db
            .call(|c| {
                c.prepare("SELECT size, count FROM media WHERE kind = 'mosaic'")?
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect()
            })
            .await
            .unwrap();
        assert_eq!(sizes, [(3600, 5)]);
    }

    #[test]
    fn percentiles_come_from_the_bands() {
        let mut agg = Agg::default();
        assert_eq!(agg.percentile(50), None);
        for ms in [50, 60, 70, 80, 300, 400, 1500, 9000, 9500, 12_000] {
            agg.merge(Agg::one(ms));
        }
        assert_eq!(agg.bands, [4, 0, 2, 0, 1, 0, 0]);
        // The 5th of 10 is the first of the two between 250 and 500 ms: halfway up that band.
        assert_eq!(agg.percentile(50), Some(375));
        // The 10th is the slowest band's last, which ends at the slowest request.
        assert_eq!(agg.percentile(95), Some(12_000));
        assert_eq!(agg.percentile(10), Some(25));
        assert_eq!(agg.mean(), Some(3296));
        // Counts without their band can't make a percentile slower than the slowest request.
        let unbanded = Agg { count: 10, total_ms: 19_000, max_ms: 3845, bands: [0, 0, 0, 0, 0, 1, 0] };
        assert_eq!(unbanded.percentile(50), Some(3845));
    }

    #[tokio::test]
    async fn rollup_moves_old_minutes_into_hours() {
        let db = Db::memory().await.unwrap();
        let stats = Stats::new(db.clone());
        let old = now() - 8 * 86_400;
        let old_minute = old / 60 * 60;
        db.call(move |c| {
            c.execute(
                "INSERT INTO stats (size, bucket, source, outcome, page_type, count, total_ms, max_ms, under_100ms)
                 VALUES (60, ?1, 'facebook', 'hit', 'reel', 2, 20, 15, 2), (60, ?2, 'facebook', 'hit', 'reel', 3, 30, 12, 3)",
                (old_minute, old_minute + 60),
            )
        })
        .await
        .unwrap();
        stats.rollup().await.unwrap();
        let rows: Vec<(i64, i64, i64)> = db
            .call(|c| {
                c.prepare("SELECT size, count, max_ms FROM stats")?
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                    .collect()
            })
            .await
            .unwrap();
        // Two minutes in the same hour, or one each side of an hour boundary.
        assert!(rows.iter().all(|r| r.0 == 3600));
        assert_eq!(rows.iter().map(|r| r.1).sum::<i64>(), 5);
        let totals = stats.totals(0, i64::MAX).await.unwrap();
        let rolled = totals[&("facebook".to_string(), "hit".to_string(), "reel".to_string())];
        assert_eq!((rolled.count, rolled.bands[0]), (5, 5));
    }

    #[tokio::test]
    async fn keeps_recent_failures() {
        let stats = Stats::new(Db::memory().await.unwrap());
        for i in 0..3 {
            stats
                .failure(&format!("https://www.facebook.com/reel/{i}"), PageType::Reel, Stage::NotFound, "x")
                .await
                .unwrap();
        }
        let failures = stats.failures(2).await.unwrap();
        assert_eq!(failures.len(), 2);
        assert_eq!(failures[0].url, "https://www.facebook.com/reel/2");
    }

    #[test]
    fn suspects_are_capped() {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let stats = Stats::new(rt.block_on(Db::memory()).unwrap());
        for n in 0..60u8 {
            stats.suspect([10, 0, 0, n].into(), "TelegramBot (like TwitterBot)", false);
        }
        assert_eq!(stats.suspects().len(), KEEP_SUSPECTS);
        for _ in 0..60 {
            stats.suspect([10, 0, 0, 1].into(), "TelegramBot (like TwitterBot)", false);
        }
        assert_eq!(stats.suspects().len(), KEEP_SUSPECTS, "one address, one row");
        let newest: Vec<_> = stats.suspects().iter().take(2).map(|s| s.ip).collect();
        assert_eq!(newest, [IpAddr::from([10, 0, 0, 1]), [10, 0, 0, 59].into()]);
        assert!(!stats.proxy_warning());
        stats.suspect("127.0.0.1".parse().unwrap(), "TelegramBot (like TwitterBot)", true);
        assert!(stats.proxy_warning());
    }
}
