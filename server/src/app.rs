//! Shared state, and getting a post through the cache and its site's fetch budget.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::OwnedSemaphorePermit;
use tracing::warn;

use crate::alerts::Alerts;
use crate::cache::{Cache, Entry};
use crate::canary::Canary;
use crate::client_ip::Resolver;
use crate::config::Config;
use crate::cookies::Cookies;
use crate::db::Db;
use crate::extract::{CookieChoice, ExtractError, Extractor, Post, Source, Stage, Target, Timings};
use crate::limits::{Budgets, IpLimiter, discord_share};
use crate::render::Reader;
use crate::settings::{LiveSettings, Settings};
use crate::stats::Stats;
use crate::telegram_ips::TelegramIps;
use crate::video::Videos;

/// How long a request waits for a free fetch from its site's budget.
const BUDGET_WAIT: Duration = Duration::from_secs(2);

pub struct App {
    pub config: Config,
    pub db: Db,
    pub settings: LiveSettings,
    pub telegram_ips: TelegramIps,
    pub resolver: Resolver,
    pub cache: Cache,
    pub ip_limiter: IpLimiter,
    pub budgets: Budgets,
    pub stats: Stats,
    pub canary: Canary,
    pub alerts: Alerts,
    pub videos: Videos,
    pub mosaics: crate::mosaic::Mosaics,
    /// Whether test links also fetch the start of their media (off in most tests).
    pub check_media: AtomicBool,
    pub cookies: Cookies,
    pub extractor: Arc<dyn Extractor>,
}

/// The result of looking up a post.
#[derive(Debug)]
pub enum Lookup {
    Hit(Arc<Post>),
    Extracted(Arc<Post>),
    /// Extraction failed now.
    Failed(ExtractError),
    /// It failed earlier, and the failure is still cached: no fetch this time.
    CachedFailure(ExtractError),
    /// No fetch budget was free in time. Not cached.
    Busy,
}

/// No fetch budget was free in time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Busy;

impl App {
    pub async fn new(
        config: Config,
        db: Db,
        cookies: Cookies,
        extractor: Arc<dyn Extractor>,
    ) -> anyhow::Result<Arc<Self>> {
        let settings = LiveSettings::load(db.clone()).await?;
        let current = settings.get();
        let telegram_ips = TelegramIps::load(&db).await;
        Ok(Arc::new(Self {
            resolver: Resolver::new(config.client_ip_from, config.trusted_proxies.clone()),
            cache: Cache::new(current.cache_megabytes, crate::cache::id_key(&db).await?),
            ip_limiter: IpLimiter::new(current.ip_requests_per_minute),
            budgets: Budgets::new(&current),
            stats: Stats::new(db.clone()),
            canary: Canary::default(),
            alerts: Alerts::default(),
            videos: Videos::new(config.video_bucket.clone())?,
            mosaics: crate::mosaic::Mosaics::new(),
            check_media: AtomicBool::new(true),
            telegram_ips,
            settings,
            config,
            db,
            cookies,
            extractor,
        }))
    }

    /// Gets a post from the cache, or extracts it once for all concurrent requests.
    pub async fn lookup(&self, target: &Target) -> Lookup {
        self.lookup_for(target, Reader::Telegram).await
    }

    /// [`App::lookup`] for `reader`. Discord gets a post its own lookups cached, or else one fetched
    /// from its share of the site's budget, taken before joining a fetch others may share: a
    /// refusal there must not reach a Telegram request waiting on the same post. What Telegram
    /// cached is fetched again for it, as how fast or whether it's answered would say which links
    /// were previewed, and anyone can claim to be Discord.
    pub async fn lookup_for(&self, target: &Target, reader: Reader) -> Lookup {
        let settings = self.settings.get();
        let _share = match reader {
            Reader::Telegram => None,
            Reader::Discord => {
                if let Some(entry) = self.cache.get(target.key()).await {
                    return match entry.outcome {
                        Ok(post) => Lookup::Hit(post),
                        Err(error) => Lookup::CachedFailure(error),
                    };
                }
                match self.discord_share(target.source, &settings).await {
                    Some(permit) => Some(permit),
                    None => return Lookup::Busy,
                }
            }
        };
        let ran = AtomicBool::new(false);
        let fetch = async {
            ran.store(true, Ordering::Relaxed);
            let extracted = self.extract(target, CookieChoice::Policy, &settings).await?;
            // Alerts go by Telegram's lookups alone: anyone can claim to be Discord.
            if reader == Reader::Telegram {
                self.alerts.extracted(target, extracted.as_ref().err());
            }
            let entry = match extracted {
                Ok((post, _)) => Entry::post(post, Duration::from_secs(settings.cache_minutes * 60)),
                Err(error) => {
                    self.note_failure(target, &error).await;
                    // A timeout or a refusal says nothing about the post: the next look may work.
                    let minutes = if error.stage.is_transient() { 1 } else { settings.failure_cache_minutes };
                    Entry::failure(error, Duration::from_secs(minutes * 60))
                }
            };
            Ok::<_, Busy>(entry)
        };
        match self.cache.get_or_fetch(settings.cache_megabytes, target.key(), fetch).await {
            Err(_) => Lookup::Busy,
            Ok(Entry { outcome: Err(error), .. }) if ran.load(Ordering::Relaxed) => Lookup::Failed(error),
            Ok(Entry { outcome: Err(error), .. }) => Lookup::CachedFailure(error),
            Ok(Entry { outcome: Ok(post), .. }) if ran.load(Ordering::Relaxed) => Lookup::Extracted(post),
            Ok(Entry { outcome: Ok(post), .. }) => Lookup::Hit(post),
        }
    }

    /// A fetch from Discord's share of `source`'s budget, if one is free in time. Held while the
    /// fetch (or a mosaic it asks for) lasts.
    pub async fn discord_share(&self, source: Source, settings: &Settings) -> Option<OwnedSemaphorePermit> {
        let budget = settings.source(source);
        let (rate, at_once) = (discord_share(budget.fetches_per_minute), discord_share(budget.concurrency));
        if rate == 0 || at_once == 0 {
            return None;
        }
        self.budgets.discord(source).acquire(rate, at_once, BUDGET_WAIT).await
    }

    /// Extracts within the site's budget and the overall deadline, without the cache.
    pub async fn extract(
        &self,
        target: &Target,
        cookies: CookieChoice,
        settings: &Settings,
    ) -> Result<Result<(Post, Timings), ExtractError>, Busy> {
        self.extract_waiting(target, cookies, settings, BUDGET_WAIT).await
    }

    /// [`App::extract`], waiting up to `wait` for the site's budget: test links can wait their
    /// turn, where Telegram can't.
    pub async fn extract_waiting(
        &self,
        target: &Target,
        cookies: CookieChoice,
        settings: &Settings,
        wait: Duration,
    ) -> Result<Result<(Post, Timings), ExtractError>, Busy> {
        let budget = settings.source(target.source);
        let Some(_permit) =
            self.budgets.of(target.source).acquire(budget.fetches_per_minute, budget.concurrency, wait).await
        else {
            return Err(Busy);
        };
        let (deadline, start) = (Duration::from_secs(settings.extract_timeout_seconds), std::time::Instant::now());
        let result = match tokio::time::timeout(deadline, self.extractor.extract(target, settings, cookies)).await {
            Ok(result) => result,
            Err(_) => Err(ExtractError::new(Stage::Timeout, format!("no result within {}s", deadline.as_secs()))),
        };
        self.stats.fetched(target, start.elapsed());
        Ok(result)
    }

    /// Logs a failure and adds it to the recent-failures list.
    async fn note_failure(&self, target: &Target, error: &ExtractError) {
        warn!(url = %target.key(), page_type = target.page_type.as_str(), "extraction failed: {error}");
        if let Err(e) = self.stats.failure(target.key(), target.page_type, error.stage, &error.message).await {
            warn!("can't save a failure: {e:#}");
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::extract::Source;
    use crate::extract::fake::FakeExtractor;

    pub async fn app_with(extractor: Arc<FakeExtractor>) -> Arc<App> {
        app_from(extractor, Config::from_lookup(|_| None).unwrap()).await
    }

    /// [`app_with`], keeping joined videos in `bucket`.
    pub async fn app_with_bucket(extractor: Arc<FakeExtractor>, bucket: crate::video::BucketConfig) -> Arc<App> {
        let mut config = Config::from_lookup(|_| None).unwrap();
        config.video_bucket = Some(bucket);
        app_from(extractor, config).await
    }

    pub async fn app_from(extractor: Arc<FakeExtractor>, config: Config) -> Arc<App> {
        let db = Db::memory().await.unwrap();
        let cookies = Cookies::load(&db).await;
        let app = App::new(config, db, cookies, extractor).await.unwrap();
        app.check_media.store(false, Ordering::Relaxed);
        app
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn lookup_extracts_once_then_hits() {
        let extractor = Arc::new(FakeExtractor::default());
        let app = app_with(Arc::clone(&extractor)).await;
        let target = Target::from_request(Source::Facebook, "/reel/1?s=a").unwrap();
        assert!(matches!(app.lookup(&target).await, Lookup::Extracted(..)));
        let same_post = Target::from_request(Source::Facebook, "/reel/1/?mibextid=b").unwrap();
        assert!(matches!(app.lookup(&same_post).await, Lookup::Hit(_)));
        assert_eq!(extractor.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn failures_are_cached_and_recorded() {
        let extractor = Arc::new(FakeExtractor::default());
        *extractor.fail.lock().unwrap() = Some(ExtractError::new(Stage::LoginWall, "login"));
        let app = app_with(Arc::clone(&extractor)).await;
        let target = Target::from_request(Source::Facebook, "/reel/2").unwrap();
        assert!(matches!(app.lookup(&target).await, Lookup::Failed(e) if e.stage == Stage::LoginWall));
        assert!(matches!(app.lookup(&target).await, Lookup::CachedFailure(_)), "no fetch the second time");
        assert_eq!(extractor.calls.load(Ordering::SeqCst), 1);
        assert_eq!(app.stats.failures(10).await.unwrap()[0].stage, "login_wall");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn deadline_and_busy() {
        let extractor = Arc::new(FakeExtractor { delay: Some(Duration::from_secs(3)), ..Default::default() });
        let app = app_with(extractor).await;
        let mut settings = (*app.settings.get()).clone();
        settings.extract_timeout_seconds = 1;
        settings.facebook.concurrency = 1;
        app.settings.set(settings).await.unwrap();

        let (first, second) = (
            Target::from_request(Source::Facebook, "/reel/3").unwrap(),
            Target::from_request(Source::Facebook, "/reel/4").unwrap(),
        );
        let (a, b) = tokio::join!(app.lookup(&first), app.lookup(&second));
        let outcomes = [a, b];
        assert!(outcomes.iter().any(|o| matches!(o, Lookup::Failed(e) if e.stage == Stage::Timeout)));
        // With one slot, the second waited up to 2 s while the first held it for 1 s, so it got a turn too.
        assert!(outcomes.iter().all(|o| matches!(o, Lookup::Failed(_))));
    }
}
