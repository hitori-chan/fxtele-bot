//! Scheduled test links. Every 30 minutes test links are extracted (within their site's budget,
//! skipping the cache) and the start of their media fetched, as Telegram would; changes go to the
//! alert chat (`crate::alerts`). Many links share each site's budget with Telegram: a run checks
//! a share of them, those checked longest ago first, at a pace that leaves Telegram the rest.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use futures_util::StreamExt;
use serde::Serialize;
use tracing::warn;

use crate::app::{App, Busy};
use crate::extract::{CookieChoice, MediaKind, Post, Source, Target};
use crate::settings::{Settings, VideoQuality};
use crate::sync::lock;
use crate::video::mp4;

/// Cookies this close to expiring trigger an alert.
const EXPIRY_WARNING_DAYS: i64 = 7;
/// More budget misses than this between two checks trigger an alert.
const BUSY_ALERT: u64 = 10;
/// A run checks at most this many minutes' worth of each site's budget (120 Facebook links, 40
/// Instagram ones by default); more links take turns, those checked longest ago first.
const RUN_MINUTES: u32 = 2;
/// Checks use at most a share of a site's fetches a minute (a half), so Telegram keeps the rest.
const PACE_SHARE: u32 = 2;
/// How long a check waits for its site's budget, which Telegram's previews may hold for a while.
const BUDGET_WAIT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Serialize)]
pub struct TestResult {
    pub url: String,
    pub page_type: String,
    pub checked_at: i64,
    pub ok: bool,
    pub detail: String,
}

#[derive(Default)]
struct Memory {
    /// Last known state per test link: `true` = working.
    links: HashMap<String, bool>,
    /// Whether `links` holds the saved results yet, so a restart doesn't alert about links that
    /// were already failing.
    recalled: bool,
    sites: HashMap<Source, SiteMemory>,
}

/// What was last seen of a site's cookies and budget.
#[derive(Default)]
struct SiteMemory {
    cookies_rejected_at: Option<i64>,
    expiry_warned_day: Option<i64>,
    busy_seen: u64,
}

#[derive(Default)]
pub struct Canary {
    memory: Mutex<Memory>,
    /// When the next scheduled check runs, in Unix seconds; 0 before it's planned.
    next: AtomicI64,
    /// Several links are checked one run at a time: scheduled, all now, or just added.
    turn: tokio::sync::Mutex<()>,
    /// A run of all of them is going on, or about to.
    full: AtomicBool,
    /// The run going on: links checked, and in all.
    progress: Mutex<Option<(usize, usize)>>,
}

/// How many of a site's links a run checks at most.
pub fn per_run(budget: &crate::settings::SourceSettings) -> usize {
    usize::try_from(budget.fetches_per_minute.saturating_mul(RUN_MINUTES)).unwrap_or(usize::MAX).max(1)
}

/// The `source` links among `links` a run checks: at most `most`, never checked first, then those
/// checked longest ago (`last`).
fn due(links: &[&String], source: Source, last: &HashMap<String, i64>, most: usize) -> Vec<(String, Target)> {
    let mut due: Vec<(String, Target)> = links
        .iter()
        .filter_map(|l| Target::from_link(l).filter(|t| t.source == source).map(|t| ((*l).clone(), t)))
        .collect();
    due.sort_by_key(|(link, _)| last.get(link).copied().unwrap_or(i64::MIN));
    due.truncate(most);
    due
}

/// Marks a run of all the links as going on, until it's dropped.
struct FullRun<'a>(&'a AtomicBool);

impl Drop for FullRun<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

/// Checks `links`, or all of them, in the background after any run going on, so the page that
/// asked answers at once. False when all of them are being checked already.
pub fn start(app: &Arc<App>, links: Option<Vec<String>>) -> bool {
    if links.is_none() && app.canary.full.swap(true, Ordering::Relaxed) {
        return false;
    }
    let app = Arc::clone(app);
    tokio::spawn(async move {
        if let Err(e) = app.canary.check(&app, links.as_deref()).await {
            warn!("can't check the test links: {e:#}");
        }
    });
    true
}

impl Canary {
    /// Checks some test links, or a run's worth of all of them, saves the results, and sends
    /// alerts for changes. One link is checked at once; several wait for the run going on, then
    /// go one site's link after another at that site's pace, the sites side by side. Cookie and
    /// budget alerts wait for a run of all of them, which keeps their half-hour pace.
    pub async fn check(&self, app: &App, only: Option<&[String]>) -> Result<Vec<TestResult>> {
        self.recall(app).await?;
        let settings = app.settings.get();
        let _full = only.is_none().then(|| {
            self.full.store(true, Ordering::Relaxed);
            FullRun(&self.full)
        });
        let asked: Vec<&String> =
            settings.test_links.iter().filter(|l| only.is_none_or(|only| only.contains(l))).collect();
        let several = asked.len() > 1;
        let _turn = if several { Some(self.turn.lock().await) } else { None };
        let last: HashMap<String, i64> = if several {
            results(app).await?.into_iter().map(|r| (r.url, r.checked_at)).collect()
        } else {
            HashMap::new()
        };
        let sites: Vec<(Source, Vec<(String, Target)>)> = Source::ALL
            .into_iter()
            .map(|source| (source, due(&asked, source, &last, per_run(settings.source(source)))))
            .collect();
        if several {
            *lock(&self.progress) = Some((0, sites.iter().map(|(_, links)| links.len()).sum()));
        }
        let runs = sites.into_iter().map(|(source, links)| {
            let settings = &settings;
            async move {
                let gap = Duration::from_secs(60) * PACE_SHARE / settings.source(source).fetches_per_minute.max(1);
                let mut results = Vec::new();
                let mut next = tokio::time::Instant::now();
                for (link, target) in links {
                    if several {
                        tokio::time::sleep_until(next).await;
                        next = tokio::time::Instant::now() + gap;
                    }
                    results.extend(self.check_one(app, link, &target, settings).await);
                    // A link checked on its own isn't part of the run going on.
                    if several && let Some((done, _)) = lock(&self.progress).as_mut() {
                        *done += 1;
                    }
                }
                results
            }
        });
        let mut results: Vec<TestResult> = futures_util::future::join_all(runs).await.into_iter().flatten().collect();
        if several {
            *lock(&self.progress) = None;
        }
        // The links as they are now: some may have been added or removed during a long run.
        let tested = app.settings.get().test_links.clone();
        results.retain(|r| tested.contains(&r.url));
        results.sort_by_key(|r| tested.iter().position(|l| *l == r.url));
        save(app, &results, &tested).await?;
        for alert in self.alerts(app, &results, &tested, only.is_none()) {
            if let Err(e) = crate::alerts::send(app, &alert).await {
                warn!("can't send an alert: {e:#}");
            }
        }
        Ok(results)
    }

    /// Takes in the saved results once, as the last known states.
    async fn recall(&self, app: &App) -> Result<()> {
        if lock(&self.memory).recalled {
            return Ok(());
        }
        let saved = results(app).await?;
        let mut memory = lock(&self.memory);
        if !memory.recalled {
            for result in saved {
                memory.links.entry(result.url).or_insert(result.ok);
            }
            memory.recalled = true;
        }
        Ok(())
    }

    /// The run going on, if any: links checked so far, and in all.
    pub fn progress(&self) -> Option<(usize, usize)> {
        *lock(&self.progress)
    }

    /// One link's check; none when its site's budget stayed taken, which says nothing about the
    /// link.
    async fn check_one(&self, app: &App, link: String, target: &Target, settings: &Settings) -> Option<TestResult> {
        let (ok, detail) = match app.extract_waiting(target, CookieChoice::Policy, settings, BUDGET_WAIT).await {
            Ok(Ok((post, timings))) => {
                let checked = if app.check_media.load(Ordering::Relaxed) {
                    check_media(app, &post, settings).await
                } else {
                    Ok(())
                };
                match checked {
                    Ok(()) => (
                        true,
                        format!(
                            "{} media, {} ms",
                            post.media.len(),
                            (timings.fetch + timings.parse + timings.extract).as_millis()
                        ),
                    ),
                    Err(problem) => (false, problem),
                }
            }
            Ok(Err(error)) => (false, error.to_string()),
            Err(Busy) => {
                warn!(
                    site = target.source.name(),
                    "a test link waited a minute for a fetch and was skipped: Telegram's previews used the whole budget"
                );
                return None;
            }
        };
        Some(TestResult {
            url: link,
            page_type: target.page_type.as_str().to_string(),
            checked_at: jiff::Timestamp::now().as_second(),
            ok,
            detail,
        })
    }

    /// When the scheduled checks will next run.
    pub fn plan(&self, at: i64) {
        self.next.store(at, Ordering::Relaxed);
    }

    pub fn next_run(&self) -> Option<i64> {
        Some(self.next.load(Ordering::Relaxed)).filter(|&at| at > 0)
    }

    /// Forgets a link that's no longer tested, history and all.
    pub async fn forget(&self, app: &App, url: &str) -> Result<()> {
        lock(&self.memory).links.remove(url);
        let url = url.to_string();
        app.db.call(move |c| c.execute("DELETE FROM canary_results WHERE url = ?1", [url])).await?;
        Ok(())
    }

    /// What changed since the last check: test links that broke or recovered, and with `all`,
    /// cookies and budget.
    fn alerts(&self, app: &App, results: &[TestResult], tested: &[String], all: bool) -> Vec<String> {
        let mut memory = lock(&self.memory);
        let mut alerts = Vec::new();
        for result in results {
            let before = memory.links.insert(result.url.clone(), result.ok);
            let site = Source::of_link(&result.url).map_or("", Source::name);
            match (before, result.ok) {
                (Some(true) | None, false) => {
                    alerts.push(format!(
                        "Test link for {site} {} is failing: {}\n{}",
                        result.page_type, result.detail, result.url
                    ));
                }
                (Some(false), true) => {
                    alerts.push(format!("Test link for {site} {} works again.\n{}", result.page_type, result.url));
                }
                _ => {}
            }
        }
        memory.links.retain(|url, _| tested.contains(url));
        if all {
            alerts.extend(Self::watch(&mut memory, app));
        }
        alerts
    }

    /// Alerts that don't depend on test links: each site's cookies and budget.
    fn watch(memory: &mut Memory, app: &App) -> Vec<String> {
        let mut alerts = Vec::new();
        let now = jiff::Timestamp::now().as_second();
        for source in Source::ALL {
            let (seen, site) = (memory.sites.entry(source).or_default(), source.name());
            let status = app.cookies.of(source).status();
            if status.rejected_at.is_some() && status.rejected_at != seen.cookies_rejected_at {
                alerts
                    .push(format!("{site} rejected the saved cookies. Paste fresh ones on its page in the dashboard."));
            }
            seen.cookies_rejected_at = status.rejected_at;

            if let Some(expires) = status.expires_at.filter(|_| status.rejected_at.is_none())
                && expires - now < EXPIRY_WARNING_DAYS * 86_400
                && seen.expiry_warned_day != Some(now / 86_400)
            {
                seen.expiry_warned_day = Some(now / 86_400);
                let days = (expires - now).max(0) / 86_400;
                alerts.push(format!("The saved {site} cookies expire in {days} days."));
            }

            let busy = app.budgets.of(source).exhausted();
            if busy.saturating_sub(seen.busy_seen) > BUSY_ALERT {
                alerts.push(format!(
                    "The {site} budget ran out {} times in the last half hour; previews failed meanwhile. Raise it on the Rate limits page if {site} allows.",
                    busy - seen.busy_seen
                ));
            }
            seen.busy_seen = busy;
        }
        alerts
    }
}

/// Checks kept per link: a day's worth.
pub const HISTORY: usize = 48;

/// Adds the results to each link's history, and forgets links no longer tested.
async fn save(app: &App, results: &[TestResult], tested: &[String]) -> Result<()> {
    let (results, tested) = (results.to_vec(), tested.to_vec());
    app.db
        .call(move |c| {
            let tx = c.transaction()?;
            for r in &results {
                tx.execute(
                    "INSERT OR REPLACE INTO canary_results (url, checked_at, ok, page_type, detail)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    (&r.url, r.checked_at, r.ok, &r.page_type, &r.detail),
                )?;
            }
            let urls: Vec<String> = tx
                .prepare("SELECT DISTINCT url FROM canary_results")?
                .query_map([], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
            for url in urls {
                if !tested.contains(&url) {
                    tx.execute("DELETE FROM canary_results WHERE url = ?1", [&url])?;
                }
            }
            tx.execute(
                "DELETE FROM canary_results WHERE checked_at < (
                   SELECT checked_at FROM canary_results newer WHERE newer.url = canary_results.url
                   ORDER BY checked_at DESC LIMIT 1 OFFSET ?1)",
                [i64::try_from(HISTORY).unwrap_or(48) - 1],
            )?;
            tx.commit()
        })
        .await
}

/// Each tested link's latest result.
pub async fn results(app: &App) -> Result<Vec<TestResult>> {
    app.db
        .call(|c| {
            c.prepare(
                "SELECT url, page_type, checked_at, ok, detail FROM canary_results latest
                 WHERE checked_at = (SELECT max(checked_at) FROM canary_results WHERE url = latest.url)
                 ORDER BY page_type, url",
            )?
            .query_map([], |r| {
                Ok(TestResult {
                    url: r.get(0)?,
                    page_type: r.get(1)?,
                    checked_at: r.get(2)?,
                    ok: r.get(3)?,
                    detail: r.get(4)?,
                })
            })?
            .collect()
        })
        .await
}

/// Each tested link's kept checks, oldest first: when, and whether it worked.
pub async fn history(app: &App) -> Result<HashMap<String, Vec<(i64, bool)>>> {
    let rows: Vec<(String, i64, bool)> = app
        .db
        .call(|c| {
            c.prepare("SELECT url, checked_at, ok FROM canary_results ORDER BY checked_at")?
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect()
        })
        .await?;
    let mut history: HashMap<String, Vec<(i64, bool)>> = HashMap::new();
    for (url, at, ok) in rows {
        history.entry(url).or_default().push((at, ok));
    }
    Ok(history)
}

/// Fetches the start of the post's first media file, as Telegram would, and with 1080p on, the
/// start of both of its streams. A CDN refusing them breaks previews as surely as a changed page.
async fn check_media(app: &App, post: &Post, settings: &Settings) -> Result<(), String> {
    let source = post.source().unwrap_or(Source::Facebook);
    let client = app.videos.client(settings.source(source).proxy.as_deref()).map_err(|e| format!("{e:#}"))?;
    if let Some(media) = post.media.first() {
        let kind = if media.kind == MediaKind::Photo { "photo" } else { "video" };
        start_of(&client, &media.url).await.map_err(|e| format!("the {kind} didn't load: {e}"))?;
    }
    let hd = post.media.iter().find_map(|m| m.hd.as_ref()).filter(|_| settings.video_quality == VideoQuality::Best);
    if let Some(hd) = hd {
        for (stream, url, handler) in [("video", Some(&hd.video), "vide"), ("audio", hd.audio.as_ref(), "soun")] {
            let Some(url) = url else { continue };
            let start = start_of(&client, url).await.map_err(|e| format!("the 1080p {stream} didn't load: {e}"))?;
            match mp4::probe(&start) {
                Ok((found, _)) if found == handler => {}
                Ok((found, _)) => return Err(format!("the 1080p {stream} holds a {found} track")),
                Err(e) => return Err(format!("the 1080p {stream} can't be read: {e:#}")),
            }
        }
    }
    Ok(())
}

/// The first 16 KB of a file.
async fn start_of(client: &reqwest::Client, url: &str) -> Result<Vec<u8>, String> {
    const LENGTH: usize = 16 * 1024;
    let response = client
        .get(url)
        .header(reqwest::header::RANGE, format!("bytes=0-{}", LENGTH - 1))
        .send()
        .await
        .map_err(crate::net::describe)?;
    if !response.status().is_success() {
        return Err(format!("the site's CDN answered {}", response.status()));
    }
    let mut start = Vec::new();
    let mut chunks = response.bytes_stream();
    while start.len() < LENGTH
        && let Some(chunk) = chunks.next().await
    {
        start.extend_from_slice(&chunk.map_err(crate::net::describe)?);
    }
    start.truncate(LENGTH);
    Ok(start)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::app::tests::app_with;
    use crate::extract::fake::FakeExtractor;
    use crate::extract::{ExtractError, HdVideo, Media, Stage};

    #[tokio::test(flavor = "multi_thread")]
    async fn checks_that_media_loads() {
        let addr = crate::video::tests::cdn().await;
        let extractor = Arc::new(FakeExtractor::default());
        let app = app_with(Arc::clone(&extractor)).await;
        app.check_media.store(true, Ordering::Relaxed);
        let mut settings = (*app.settings.get()).clone();
        settings.test_links = vec!["https://www.facebook.com/reel/1".into()];
        app.settings.set(settings.clone()).await.unwrap();
        let video = |progressive: &str, hd: Option<HdVideo>| {
            let mut media = Media::video(format!("http://{addr}/{progressive}"), None);
            media.hd = hd;
            vec![media]
        };

        *extractor.media.lock().unwrap() = Some(video("denied.mp4", None));
        let result = &app.canary.check(&app, None).await.unwrap()[0];
        assert!(
            !result.ok && result.detail.contains("the video didn't load") && result.detail.contains("403"),
            "{result:?}"
        );

        // 1080p streams are checked only when 1080p is on.
        let mut swapped = crate::video::tests::hd(addr, "audio.mp4");
        swapped.audio = Some(format!("http://{addr}/video.mp4"));
        *extractor.media.lock().unwrap() = Some(video("video.mp4", Some(swapped)));
        assert!(app.canary.check(&app, None).await.unwrap()[0].ok);
        settings.video_quality = VideoQuality::Best;
        app.settings.set(settings).await.unwrap();
        let result = &app.canary.check(&app, None).await.unwrap()[0];
        assert!(!result.ok && result.detail.contains("holds a soun track"), "{result:?}");

        *extractor.media.lock().unwrap() = Some(video("video.mp4", Some(crate::video::tests::hd(addr, "video.mp4"))));
        let result = &app.canary.check(&app, None).await.unwrap()[0];
        assert!(result.ok, "{result:?}");
    }

    #[test]
    fn a_run_takes_turns_when_there_are_more_links_than_it_checks() {
        let links: Vec<String> =
            ["reel/1", "reel/2", "reel/3"].iter().map(|p| format!("https://www.facebook.com/{p}")).collect();
        let mut asked: Vec<&String> = links.iter().collect();
        let instagram = "https://www.instagram.com/p/DfahZrMjAqq/".to_string();
        asked.push(&instagram);
        let last = HashMap::from([(links[0].clone(), 300), (links[1].clone(), 100)]);
        let due: Vec<String> = due(&asked, Source::Facebook, &last, 2).into_iter().map(|(l, _)| l).collect();
        assert_eq!(
            due,
            [links[2].clone(), links[1].clone()],
            "never checked first, then the oldest; another site's apart"
        );
        assert_eq!(per_run(&Settings::default().instagram), 40);
    }

    #[tokio::test]
    async fn history_keeps_a_day_per_link() {
        let app = app_with(Arc::new(FakeExtractor::default())).await;
        let check = |url: &str, at: i64| TestResult {
            url: url.into(),
            page_type: "reel".into(),
            checked_at: at,
            ok: at % 2 == 0,
            detail: String::new(),
        };
        let (one, two) = ("https://www.facebook.com/reel/1", "https://www.facebook.com/reel/2");
        let both = [one.to_string(), two.to_string()];
        for at in 0..50 {
            save(&app, &[check(one, at), check(two, at)], &both).await.unwrap();
        }
        let kept = history(&app).await.unwrap().remove(one).unwrap();
        assert_eq!((kept.len(), kept[0], kept[HISTORY - 1]), (HISTORY, (2, true), (49, false)));
        let latest = results(&app).await.unwrap();
        assert_eq!((latest.len(), latest[0].checked_at), (2, 49));

        // Checking one link leaves the other's history alone; a link no longer tested goes.
        save(&app, &[check(two, 50)], &both).await.unwrap();
        assert_eq!(history(&app).await.unwrap().len(), 2);
        save(&app, &[check(two, 51)], &both[1..]).await.unwrap();
        assert_eq!(history(&app).await.unwrap().keys().collect::<Vec<_>>(), [two]);
        app.canary.forget(&app, two).await.unwrap();
        assert!(history(&app).await.unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn alerts_when_a_link_breaks_and_recovers() {
        let extractor = Arc::new(FakeExtractor::default());
        let app = app_with(Arc::clone(&extractor)).await;
        let mut settings = (*app.settings.get()).clone();
        settings.test_links =
            vec!["https://www.facebook.com/reel/1".into(), "https://www.facebook.com/watch?v=2".into()];
        app.settings.set(settings).await.unwrap();
        let tested = app.settings.get().test_links.clone();

        let checked = app.canary.check(&app, None).await.unwrap();
        assert!(checked.iter().all(|r| r.ok));
        assert_eq!(results(&app).await.unwrap().len(), 2, "saved");
        assert!(app.canary.alerts(&app, &checked, &tested, true).is_empty(), "still working: nothing to say");

        *extractor.fail.lock().unwrap() = Some(ExtractError::new(Stage::LoginWall, "login"));
        let failing = app.canary.check(&app, None).await.unwrap();
        assert!(failing.iter().all(|r| !r.ok && r.detail.contains("login")));
        let again = app.canary.alerts(&app, &failing, &tested, true);
        assert!(again.is_empty(), "only the change is reported, not every check: {again:?}");

        *extractor.fail.lock().unwrap() = None;
        let fixed = app.canary.check(&app, None).await.unwrap();
        app.canary.memory.lock().unwrap().links.insert(fixed[0].url.clone(), false);
        let alerts = app.canary.alerts(&app, &fixed, &tested, true);
        assert_eq!(alerts.len(), 1);
        assert!(alerts[0].contains("works again"));

        let one = app.canary.check(&app, Some(&tested[1..2])).await.unwrap();
        assert_eq!((one.len(), one[0].url.as_str()), (1, tested[1].as_str()));
        assert_eq!(history(&app).await.unwrap().len(), 2, "the other link's checks stay");
    }
}
