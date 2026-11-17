//! Alerts to the Telegram chat chosen in the dashboard, sent with TELEGRAM_BOT_TOKEN. The test
//! links' alerts come from `canary.rs`; this watches real traffic once a minute: a site's previews
//! failing in a row, 1080p videos that couldn't be made, the video bucket failing, and Telegram's
//! requests arriving from the front end's address (a wrong CLIENT_IP_FROM). Every alert is also
//! noted on the dashboard's Activity page, sent or not.

use std::collections::HashMap;
use std::sync::Mutex;

use anyhow::{Result, bail};
use tracing::{info, warn};

use crate::activity::{self, Kind};
use crate::app::App;
use crate::extract::{ExtractError, Source, Target};
use crate::sync::lock;
use crate::video::{BucketState, Failure};

/// This many failed extractions in a row, with no success between, trigger an alert.
const FAILING_IN_A_ROW: u32 = 5;
/// At most one alert about failed videos per hour; the next one counts what was held back.
const VIDEO_ALERT_EVERY: i64 = 3600;
/// At most one alert a day about Telegram's requests coming from the front end's address.
const PROXY_ALERT_EVERY: i64 = 86_400;

#[derive(Default)]
pub struct Alerts {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    sites: HashMap<Source, Streak>,
    /// 1080p videos that failed since the last alert about them: how many, and the latest.
    videos: (usize, Option<Failure>),
    video_alerted_at: Option<i64>,
    bucket_alerted: bool,
    proxy_alerted_at: Option<i64>,
}

/// A site's extractions for Telegram since its last success.
#[derive(Default)]
struct Streak {
    in_a_row: u32,
    latest_failure: Option<String>,
    alerted: bool,
    recovered: bool,
}

impl Alerts {
    /// Notes the outcome of an extraction made for Telegram.
    pub fn extracted(&self, target: &Target, failure: Option<&ExtractError>) {
        let mut state = lock(&self.state);
        let streak = state.sites.entry(target.source).or_default();
        match failure {
            None => {
                streak.recovered |= streak.alerted;
                streak.alerted = false;
                streak.in_a_row = 0;
            }
            Some(error) => {
                streak.in_a_row += 1;
                streak.latest_failure = Some(format!("{error}\n{}", target.key()));
            }
        }
    }

    /// How many of the site's latest extractions failed in a row, once that's enough to alert.
    pub fn failing(&self, source: Source) -> Option<u32> {
        let state = lock(&self.state);
        state.sites.get(&source).map(|s| s.in_a_row).filter(|&n| n >= FAILING_IN_A_ROW)
    }

    /// The alerts due now, given the videos that failed since the last call, what's wrong with
    /// the video bucket, and whether Telegram's requests are arriving from a proxy's address.
    pub fn due(&self, videos: Vec<Failure>, bucket: BucketState, proxy_warning: bool, now: i64) -> Vec<String> {
        let mut state = lock(&self.state);
        let mut alerts = Vec::new();
        for source in Source::ALL {
            let Some(streak) = state.sites.get_mut(&source) else { continue };
            let site = source.name();
            if streak.in_a_row >= FAILING_IN_A_ROW && !streak.alerted {
                streak.alerted = true;
                alerts.push(format!(
                    "{site} previews are failing: the last {} {site} links all failed. {site} may have changed its \
                     pages or blocked the server; try one in the dashboard's playground.\nLatest: {}",
                    streak.in_a_row,
                    streak.latest_failure.as_deref().unwrap_or("?")
                ));
            }
            if streak.recovered {
                streak.recovered = false;
                alerts.push(format!("{site} previews work again."));
            }
        }

        state.videos.0 += videos.len();
        if let Some(latest) = videos.into_iter().last() {
            state.videos.1 = Some(latest);
        }
        if state.video_alerted_at.is_none_or(|at| now - at >= VIDEO_ALERT_EVERY)
            && let (count, Some(latest)) = std::mem::take(&mut state.videos)
        {
            let what = if count == 1 { "a 1080p video".to_string() } else { format!("{count} 1080p videos") };
            alerts.push(format!(
                "Couldn't make {what}, so Telegram got the 720p file.\nLatest: {}\n{}",
                latest.error, latest.post
            ));
            state.video_alerted_at = Some(now);
        }

        match (bucket, state.bucket_alerted) {
            (BucketState::Failing(problem), false) => {
                state.bucket_alerted = true;
                alerts.push(format!(
                    "The video bucket doesn't work, so Telegram gets 720p videos. It's tried again 10 minutes \
                     later; the dashboard's Videos page shows how it's doing.\n{problem}"
                ));
            }
            (BucketState::Works, true) => {
                state.bucket_alerted = false;
                alerts.push("The video bucket works again: 1080p videos are back.".to_string());
            }
            // 1080p turned off, or the bucket removed: nothing came back, so nothing to say.
            (BucketState::Unused, _) => state.bucket_alerted = false,
            _ => {}
        }

        if proxy_warning && state.proxy_alerted_at.is_none_or(|at| now - at >= PROXY_ALERT_EVERY) {
            state.proxy_alerted_at = Some(now);
            alerts.push(
                "Requests with Telegram's user agent are coming from your front end's address, so Telegram gets \
                 redirects instead of previews. Set CLIENT_IP_FROM to the header your front end adds."
                    .to_string(),
            );
        }
        alerts
    }
}

/// Sends what's due now; run once a minute.
pub async fn check(app: &App) {
    let now = jiff::Timestamp::now().as_second();
    let bucket = app.videos.bucket_state(&app.settings.get());
    for alert in app.alerts.due(app.videos.take_failures(), bucket, app.stats.proxy_warning(), now) {
        if let Err(e) = send(app, &alert).await {
            warn!("can't send an alert: {e:#}");
        }
    }
}

/// Sends a message to the alert chat, and notes it on the Activity page. Does nothing more (but
/// log) without the token or a chat.
pub async fn send(app: &App, text: &str) -> Result<()> {
    activity::record(&app.db, Kind::Alert, "alerts", text).await;
    let (Some(token), Some(chat)) = (&app.config.telegram_bot_token, app.settings.get().alert_chat) else {
        info!("alert (no alert chat or token set): {text}");
        return Ok(());
    };
    let client = crate::net::client()?;
    let response = client
        .post(format!("https://api.telegram.org/bot{token}/sendMessage"))
        .json(&serde_json::json!({ "chat_id": chat, "text": text, "link_preview_options": { "is_disabled": true } }))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("{}", crate::net::describe(e)))?;
    if !response.status().is_success() {
        let body: serde_json::Value = response.json().await.unwrap_or_default();
        bail!("Telegram refused: {}", body["description"].as_str().unwrap_or("unknown error"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::Stage;

    fn reel() -> Target {
        Target::from_link("https://www.facebook.com/reel/1").unwrap()
    }

    fn fail(alerts: &Alerts, n: u32) {
        let error = ExtractError::new(Stage::NotFound, "nothing for this reel");
        for _ in 0..n {
            alerts.extracted(&reel(), Some(&error));
        }
    }

    #[test]
    fn failing_previews_alert_once_then_recover() {
        let alerts = Alerts::default();
        fail(&alerts, FAILING_IN_A_ROW - 1);
        assert!(alerts.due(Vec::new(), BucketState::Unused, false, 0).is_empty(), "not yet");
        alerts.extracted(&reel(), None);
        fail(&alerts, FAILING_IN_A_ROW - 1);
        assert!(alerts.due(Vec::new(), BucketState::Unused, false, 0).is_empty(), "a success in between starts over");

        fail(&alerts, 1);
        let due = alerts.due(Vec::new(), BucketState::Unused, false, 0);
        assert_eq!(due.len(), 1);
        assert!(
            due[0].contains("last 5 Facebook") && due[0].contains("not_found") && due[0].contains("reel/1"),
            "{due:?}"
        );
        assert_eq!((alerts.failing(Source::Facebook), alerts.failing(Source::Instagram)), (Some(5), None));
        fail(&alerts, 3);
        assert!(alerts.due(Vec::new(), BucketState::Unused, false, 0).is_empty(), "once per streak");

        let post = Target::from_link("https://www.instagram.com/p/DfSi56IFwZK").unwrap();
        alerts.extracted(&post, None);
        assert!(alerts.due(Vec::new(), BucketState::Unused, false, 0).is_empty(), "another site's success");
        alerts.extracted(&reel(), None);
        assert_eq!(alerts.due(Vec::new(), BucketState::Unused, false, 0), ["Facebook previews work again."]);
        assert!(alerts.due(Vec::new(), BucketState::Unused, false, 0).is_empty());
    }

    #[test]
    fn video_failures_at_most_hourly() {
        let alerts = Alerts::default();
        let failure = |n: u8| Failure {
            post: format!("https://www.facebook.com/reel/{n}"),
            error: "Facebook's CDN answered 403".into(),
        };
        let due = alerts.due(vec![failure(1)], BucketState::Unused, false, 1000);
        assert!(due[0].starts_with("Couldn't make a 1080p video") && due[0].contains("403"), "{due:?}");
        assert!(alerts.due(vec![failure(2), failure(3)], BucketState::Unused, false, 2000).is_empty(), "held back");
        let due = alerts.due(Vec::new(), BucketState::Unused, false, 1000 + VIDEO_ALERT_EVERY);
        assert!(due[0].starts_with("Couldn't make 2 1080p videos") && due[0].contains("reel/3"), "{due:?}");
    }

    #[test]
    fn bucket_alerts_when_it_breaks_and_recovers() {
        let alerts = Alerts::default();
        let broken = || BucketState::Failing("can't upload: the bucket answered 403 Forbidden".to_string());
        let due = alerts.due(Vec::new(), broken(), false, 0);
        assert!(due[0].starts_with("The video bucket doesn't work") && due[0].contains("403"), "{due:?}");
        assert!(alerts.due(Vec::new(), broken(), false, 3600).is_empty(), "once until it works");
        let works = alerts.due(Vec::new(), BucketState::Works, false, 7200);
        assert_eq!(works, ["The video bucket works again: 1080p videos are back."]);
        assert!(alerts.due(Vec::new(), BucketState::Works, false, 7300).is_empty());

        // Turning 1080p off while it's failing isn't a recovery.
        assert_eq!(alerts.due(Vec::new(), broken(), false, 8000).len(), 1);
        assert!(alerts.due(Vec::new(), BucketState::Unused, false, 8100).is_empty());
        assert!(alerts.due(Vec::new(), BucketState::Works, false, 8200).is_empty(), "and no recovery later");
    }

    #[test]
    fn proxy_address_alert_at_most_daily() {
        let alerts = Alerts::default();
        assert!(alerts.due(Vec::new(), BucketState::Unused, false, 0).is_empty());
        assert!(alerts.due(Vec::new(), BucketState::Unused, true, 10)[0].contains("CLIENT_IP_FROM"));
        assert!(alerts.due(Vec::new(), BucketState::Unused, true, 20).is_empty());
        assert_eq!(alerts.due(Vec::new(), BucketState::Unused, true, 10 + PROXY_ALERT_EVERY).len(), 1);
    }
}
