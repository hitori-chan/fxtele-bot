//! What needs attention: one check per site, then test links, videos, Telegram's addresses and
//! alerts. The top bar's status menu, the menu's site dots, the overview and the Alerts page all
//! show these.

use super::*;

/// A check: what it's about, where to fix it, and its [`Status`].
pub struct Check {
    pub name: String,
    pub href: String,
    /// An icon, or a site's mark when `site` is set.
    pub icon: &'static str,
    pub site: Option<&'static str>,
    pub status: Status,
}

/// How bad a level is, for picking the worst.
fn rank(level: &str) -> u8 {
    match level {
        BAD => 3,
        WARN => 2,
        OK => 1,
        _ => 0,
    }
}

pub fn worst<'a>(levels: impl IntoIterator<Item = &'a str>) -> &'a str {
    levels.into_iter().max_by_key(|l| rank(l)).unwrap_or(IDLE)
}

/// A site's previews and cookies in one: failing previews first, then the cookies.
pub fn site_status(app: &App, source: Source) -> Status {
    if let Some(n) = app.alerts.failing(source) {
        return Status::new("Failing", format!("the last {n} previews failed"), BAD);
    }
    let status = app.cookies.of(source).status();
    let cookies = cookie_status(source, &status);
    if cookies.level == BAD {
        return Status::new(format!("Cookies {}", cookies.state.to_lowercase()), cookies.detail, BAD);
    }
    match status.expires_at.filter(|_| status.has_session) {
        Some(at) if at - now() < 7 * 86_400 => Status::new("Cookies expire soon", format!("on {}", day(at)), WARN),
        _ if status.count == 0 => Status::new("Working", "logged out: public posts only", OK),
        _ => Status::new("Working", format!("cookies {}", cookies.detail), OK),
    }
}

pub async fn tests_status(app: &App, settings: &Settings) -> Result<Status> {
    let results = canary::results(app).await?;
    Ok(if settings.test_links.is_empty() {
        Status::new("None", "add some to hear about changes early", IDLE)
    } else if results.is_empty() {
        Status::new("Not run", "the first check runs soon", IDLE)
    } else {
        let ok = results.iter().filter(|r| r.ok).count();
        let last = results.iter().map(|r| r.checked_at).max().unwrap_or_default();
        let level = if ok < results.len() { BAD } else { OK };
        Status::new(format!("{ok} of {} pass", results.len()), format!("checked {}", ago(last)), level)
    })
}

pub async fn videos_status(app: &App, settings: &Settings) -> Status {
    let videos = &app.videos;
    if settings.video_quality == VideoQuality::Standard {
        return Status::new("720p", "the sites' own files", IDLE);
    }
    if !videos.wanted(settings) {
        return Status::new("720p", "1080p needs a bucket, and none is set up", BAD);
    }
    if let Some(failing) = videos.bucket_health().failing {
        let at = failing.at;
        let state = if videos.ready(settings) { "1080p" } else { "720p" };
        return Status::new(state, format!("the bucket failed {}", ago(at)), BAD);
    }
    let usage = videos.usage().await.unwrap_or_default();
    let failed = videos.recent_failures().first().map(|f| f.0).filter(|at| now() - at < 3600);
    let detail = match failed {
        Some(at) => format!("a video failed {}", ago(at)),
        None => format!("{} joined, {} in the bucket", usage.files, size(usage.bytes)),
    };
    Status::new("1080p", detail, if failed.is_some() { BAD } else { OK })
}

fn telegram_status(app: &App) -> Status {
    if app.stats.proxy_warning() {
        return Status::new("Misread", "Telegram looks like a proxy: check CLIENT_IP_FROM", BAD);
    }
    let ranges = app.telegram_ips.current();
    match ranges.refreshed_at {
        Some(at) if now() - at > 3 * 86_400 => {
            Status::new(format!("{} ranges", ranges.nets.len()), format!("last downloaded {}", ago(at)), WARN)
        }
        Some(at) => Status::new(format!("{} ranges", ranges.nets.len()), format!("downloaded {}", ago(at)), OK),
        None => Status::new(format!("{} ranges", ranges.nets.len()), "the bundled copy, until the first download", OK),
    }
}

pub fn alerts_status(app: &App, settings: &Settings) -> Status {
    match (&app.config.telegram_bot_token, settings.alert_chat) {
        (None, _) => Status::new("Off", "TELEGRAM_BOT_TOKEN isn't set", IDLE),
        (Some(_), None) => Status::new("No chat", "choose where alerts go", WARN),
        (Some(_), Some(chat)) => Status::new("On", format!("sent to chat {chat}"), OK),
    }
}

pub async fn checks(app: &App) -> Result<Vec<Check>> {
    let settings = app.settings.get();
    let mut checks: Vec<Check> = Source::ALL
        .into_iter()
        .map(|source| Check {
            name: source.name().to_string(),
            href: format!("/sites/{}", source.as_str()),
            icon: source.as_str(),
            site: Some(source.as_str()),
            status: site_status(app, source),
        })
        .collect();
    let others = [
        ("Test links", "/tests", "tests", tests_status(app, &settings).await?),
        ("Videos", "/videos", "videos", videos_status(app, &settings).await),
        ("Telegram IPs", "/telegram", "telegram", telegram_status(app)),
        ("Alerts", "/alerts", "bell", alerts_status(app, &settings)),
    ];
    checks.extend(others.into_iter().map(|(name, href, icon, status)| Check {
        name: name.into(),
        href: href.into(),
        icon,
        site: None,
        status,
    }));
    Ok(checks)
}

#[derive(Template)]
#[template(path = "dashboard/status.html")]
struct StatusMenu {
    level: &'static str,
    headline: String,
    checks: Vec<Check>,
}

/// The top bar's status menu, and the menu's site dots (swapped in out of band).
pub async fn status(State(d): State<Shared>) -> Response {
    let checks = match checks(&d.app).await {
        Ok(checks) => checks,
        Err(e) => return server_error(&e),
    };
    let level = worst(checks.iter().map(|c| c.status.level));
    let attention = checks.iter().filter(|c| matches!(c.status.level, BAD | WARN)).count();
    let headline = match attention {
        0 => "All good".to_string(),
        1 => "1 needs attention".to_string(),
        n => format!("{n} need attention"),
    };
    render(&StatusMenu { level, headline, checks })
}
