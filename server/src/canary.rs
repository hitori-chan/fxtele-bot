//! Scheduled test links. Every 30 minutes each test link is extracted (within the Facebook
//! budget, skipping the cache) and the start of its media fetched, as Telegram would; changes
//! go to the alert chat (`crate::alerts`).

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::Ordering;

use anyhow::Result;
use futures_util::StreamExt;
use serde::Serialize;
use tracing::warn;

use crate::app::{App, Busy};
use crate::extract::facebook::url::Target;
use crate::extract::{CookieChoice, MediaKind, Post};
use crate::settings::{Settings, VideoQuality};
use crate::sync::lock;
use crate::video::mp4;

/// Cookies this close to expiring trigger an alert.
const EXPIRY_WARNING_DAYS: i64 = 7;
/// More budget misses than this between two checks trigger an alert.
const BUSY_ALERT: u64 = 10;

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
    cookies_rejected_at: Option<i64>,
    expiry_warned_day: Option<i64>,
    busy_seen: u64,
}

#[derive(Default)]
pub struct Canary {
    memory: Mutex<Memory>,
}

impl Canary {
    /// Checks every test link, saves the results, and sends alerts for changes.
    pub async fn run(&self, app: &App) -> Result<Vec<TestResult>> {
        let settings = app.settings.get();
        let mut results = Vec::new();
        for link in &settings.test_links {
            let Some(target) = Target::from_link(link) else { continue };
            let (ok, detail) = match app.extract(&target, CookieChoice::Policy, &settings).await {
                Ok(Ok((post, timings))) => {
                    let checked = if app.check_media.load(Ordering::Relaxed) {
                        check_media(app, &post, &settings).await
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
                Err(Busy) => (false, "no Facebook budget free".to_string()),
            };
            results.push(TestResult {
                url: link.clone(),
                page_type: target.page_type.as_str().to_string(),
                checked_at: jiff::Timestamp::now().as_second(),
                ok,
                detail,
            });
        }
        save(app, &results).await?;
        for alert in self.alerts(app, &results) {
            if let Err(e) = crate::alerts::send(app, &alert).await {
                warn!("can't send an alert: {e:#}");
            }
        }
        Ok(results)
    }

    /// What changed since the last check: test links that broke or recovered, cookies, budget.
    fn alerts(&self, app: &App, results: &[TestResult]) -> Vec<String> {
        let mut memory = lock(&self.memory);
        let mut alerts = Vec::new();
        for result in results {
            let before = memory.links.insert(result.url.clone(), result.ok);
            match (before, result.ok) {
                (Some(true) | None, false) => {
                    alerts.push(format!(
                        "Test link for {} is failing: {}\n{}",
                        result.page_type, result.detail, result.url
                    ));
                }
                (Some(false), true) => {
                    alerts.push(format!("Test link for {} works again.\n{}", result.page_type, result.url));
                }
                _ => {}
            }
        }
        memory.links.retain(|url, _| results.iter().any(|r| &r.url == url));
        alerts.extend(Self::watch(&mut memory, app));
        alerts
    }

    /// Alerts that don't depend on test links: cookies and the Facebook budget.
    fn watch(memory: &mut Memory, app: &App) -> Vec<String> {
        let mut alerts = Vec::new();
        let status = app.cookies.status();
        if status.rejected_at.is_some() && status.rejected_at != memory.cookies_rejected_at {
            alerts.push("Facebook rejected the saved cookies. Paste fresh ones in the dashboard.".to_string());
        }
        memory.cookies_rejected_at = status.rejected_at;

        let now = jiff::Timestamp::now().as_second();
        if let Some(expires) = status.expires_at.filter(|_| status.rejected_at.is_none())
            && expires - now < EXPIRY_WARNING_DAYS * 86_400
            && memory.expiry_warned_day != Some(now / 86_400)
        {
            memory.expiry_warned_day = Some(now / 86_400);
            let days = (expires - now).max(0) / 86_400;
            alerts.push(format!("The saved Facebook cookies expire in {days} days."));
        }

        let busy = app.budget.exhausted();
        if busy.saturating_sub(memory.busy_seen) > BUSY_ALERT {
            alerts.push(format!(
                "The Facebook budget ran out {} times in the last half hour; previews failed meanwhile.",
                busy - memory.busy_seen
            ));
        }
        memory.busy_seen = busy;
        alerts
    }
}

async fn save(app: &App, results: &[TestResult]) -> Result<()> {
    let results = results.to_vec();
    app.db
        .call(move |c| {
            let tx = c.transaction()?;
            tx.execute("DELETE FROM canary_results", [])?;
            for r in &results {
                tx.execute(
                    "INSERT INTO canary_results (url, checked_at, ok, page_type, detail) VALUES (?1, ?2, ?3, ?4, ?5)",
                    (&r.url, r.checked_at, r.ok, &r.page_type, &r.detail),
                )?;
            }
            tx.commit()
        })
        .await
}

pub async fn results(app: &App) -> Result<Vec<TestResult>> {
    app.db
        .call(|c| {
            c.prepare("SELECT url, page_type, checked_at, ok, detail FROM canary_results ORDER BY page_type, url")?
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

/// Fetches the start of the post's first media file, as Telegram would, and with 1080p on, the
/// start of both of its streams. A CDN refusing them breaks previews as surely as a changed page.
async fn check_media(app: &App, post: &Post, settings: &Settings) -> Result<(), String> {
    let client = app.videos.client(settings.proxy.as_deref()).map_err(|e| format!("{e:#}"))?;
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
        return Err(format!("Facebook's CDN answered {}", response.status()));
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
        let result = &app.canary.run(&app).await.unwrap()[0];
        assert!(
            !result.ok && result.detail.contains("the video didn't load") && result.detail.contains("403"),
            "{result:?}"
        );

        // 1080p streams are checked only when 1080p is on.
        let mut swapped = crate::video::tests::hd(addr, "audio.mp4");
        swapped.audio = Some(format!("http://{addr}/video.mp4"));
        *extractor.media.lock().unwrap() = Some(video("video.mp4", Some(swapped)));
        assert!(app.canary.run(&app).await.unwrap()[0].ok);
        settings.video_quality = VideoQuality::Best;
        app.settings.set(settings).await.unwrap();
        let result = &app.canary.run(&app).await.unwrap()[0];
        assert!(!result.ok && result.detail.contains("holds a soun track"), "{result:?}");

        *extractor.media.lock().unwrap() = Some(video("video.mp4", Some(crate::video::tests::hd(addr, "video.mp4"))));
        let result = &app.canary.run(&app).await.unwrap()[0];
        assert!(result.ok, "{result:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn alerts_when_a_link_breaks_and_recovers() {
        let extractor = Arc::new(FakeExtractor::default());
        let app = app_with(Arc::clone(&extractor)).await;
        let mut settings = (*app.settings.get()).clone();
        settings.test_links =
            vec!["https://www.facebook.com/reel/1".into(), "https://www.facebook.com/watch?v=2".into()];
        app.settings.set(settings).await.unwrap();

        let checked = app.canary.run(&app).await.unwrap();
        assert!(checked.iter().all(|r| r.ok));
        assert_eq!(results(&app).await.unwrap().len(), 2, "saved");
        assert!(app.canary.alerts(&app, &checked).is_empty(), "still working: nothing to say");

        *extractor.fail.lock().unwrap() = Some(ExtractError::new(Stage::LoginWall, "login"));
        let failing = app.canary.run(&app).await.unwrap();
        assert!(failing.iter().all(|r| !r.ok && r.detail.contains("login")));
        let again = app.canary.alerts(&app, &failing);
        assert!(again.is_empty(), "only the change is reported, not every check: {again:?}");

        *extractor.fail.lock().unwrap() = None;
        let fixed = app.canary.run(&app).await.unwrap();
        app.canary.memory.lock().unwrap().links.insert(fixed[0].url.clone(), false);
        let alerts = app.canary.alerts(&app, &fixed);
        assert_eq!(alerts.len(), 1);
        assert!(alerts[0].contains("works again"));
    }
}
