//! Background work: statistics, limiter cleanup, alerts, the Telegram IP-range refresh, test links
//! and old joined videos.

use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::app::App;

/// Test links first run a little after startup, then every 30 minutes.
const TESTS_FIRST: Duration = Duration::from_secs(120);
const TESTS_EVERY: Duration = Duration::from_secs(30 * 60);
/// Old videos are first looked for a little after startup, so a bucket that isn't reachable yet
/// right then doesn't count as failing.
const VIDEOS_FIRST: Duration = Duration::from_secs(300);

pub fn spawn(app: &Arc<App>, shutdown: &CancellationToken) {
    every(app, shutdown, Duration::ZERO, Duration::from_secs(60), |app| async move {
        if let Err(e) = app.stats.flush().await {
            warn!("can't save statistics: {e:#}");
        }
        app.ip_limiter.cleanup();
        crate::alerts::check(&app).await;
    });
    every(app, shutdown, Duration::ZERO, Duration::from_secs(3600), |app| async move {
        if let Err(e) = app.stats.rollup().await {
            warn!("can't roll up statistics: {e:#}");
        }
    });
    every(app, shutdown, Duration::ZERO, Duration::from_secs(3600), |app| async move {
        let ranges = app.telegram_ips.current();
        let age = ranges.refreshed_at.map_or(i64::MAX, |t| jiff::Timestamp::now().as_second() - t);
        if age < 86_400 {
            return;
        }
        let result = match crate::net::client() {
            Ok(client) => app.telegram_ips.refresh(&client, &app.db).await,
            Err(e) => Err(e),
        };
        match result {
            Ok(count) => info!(count, "refreshed Telegram's IP ranges"),
            Err(e) => warn!("can't refresh Telegram's IP ranges, keeping the current list: {e:#}"),
        }
    });
    every(app, shutdown, VIDEOS_FIRST, Duration::from_secs(3600), |app| async move {
        if let Err(e) = app.videos.tidy_up(&app.settings.get()).await {
            warn!("{e:#}");
        }
    });
    let now = || jiff::Timestamp::now().as_second();
    app.canary.plan(now() + TESTS_FIRST.as_secs().cast_signed());
    every(app, shutdown, TESTS_FIRST, TESTS_EVERY, move |app| async move {
        app.canary.plan(now() + TESTS_EVERY.as_secs().cast_signed());
        match app.canary.check(&app, None).await {
            Ok(results) if !results.is_empty() => {
                let failing = results.iter().filter(|r| !r.ok).count();
                info!(links = results.len(), failing, "checked the test links");
            }
            Ok(_) => {}
            Err(e) => warn!("can't check the test links: {e:#}"),
        }
    });
}

/// Runs `job` after `first`, then every `period`, until shutdown.
fn every<F, Fut>(app: &Arc<App>, shutdown: &CancellationToken, first: Duration, period: Duration, job: F)
where
    F: Fn(Arc<App>) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send,
{
    let app = Arc::clone(app);
    let shutdown = shutdown.clone();
    tokio::spawn(async move {
        let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + first, period);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = ticks.tick() => job(Arc::clone(&app)).await,
                () = shutdown.cancelled() => break,
            }
        }
    });
}
