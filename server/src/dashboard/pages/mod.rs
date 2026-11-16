//! The dashboard's pages. A change redirects back with `?done=<what>` for the notice; a refused
//! change shows the page again, with what was entered, the error and status 400.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use askama::Template;
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::{Extension, Form, Json};
use ipnet::IpNet;
use serde::Deserialize;
use serde_json::json;
use webauthn_rs::prelude::RegisterPublicKeyCredential;

use super::auth::{self, Identity};
use super::chart::{self, Chart, Range, Spark};
use super::{Dashboard, Shared, json_error, render, server_error, with_cookie};
use crate::app::{App, Busy};
use crate::cache::Entry;
use crate::canary::{self, TestResult};
use crate::cookies::{self, parse_export};
use crate::extract::{CookieChoice, ExtractError, Post, Source, Stage, Target, Timings};
use crate::render::{self as preview, MediaUrl};
use crate::settings::{CookiePolicy, FailureResponse, MediaLinks, Settings, SourceSettings, VideoQuality};
use crate::stats::{Agg, Failure};
use crate::sync::lock;
use crate::video::{BucketHealth, Usage};

mod activity;
mod alerts;
mod backup;
mod cache;
mod health;
mod issues;
mod limits;
mod overview;
mod playground;
mod previews;
mod requests;
mod security;
mod sites;
mod telegram;
mod test_links;
mod videos;

pub use activity::*;
pub use alerts::*;
pub use backup::*;
pub use cache::*;
pub use health::{Check, status};
pub use issues::*;
pub use limits::*;
pub use overview::*;
pub use playground::*;
pub use previews::*;
pub use requests::*;
pub use security::*;
pub use sites::*;
pub use telegram::*;
pub use test_links::*;
pub use videos::*;

// ---- The frame around every page ----

/// A menu entry: path, label, the key pages use to mark themselves current, and its icon.
type Link = (&'static str, &'static str, &'static str, &'static str);

/// The menu, in groups under a heading (none for the first). Sites' entries show the site's mark.
const NAV: [(&str, &[Link]); 5] = [
    (
        "",
        &[
            ("/", "Overview", "overview", "overview"),
            ("/requests", "Requests", "requests", "requests"),
            ("/issues", "Issues", "issues", "failures"),
            ("/playground", "Playground", "playground", "playground"),
        ],
    ),
    (
        "Sites",
        &[
            ("/sites/facebook", "Facebook", "facebook", "facebook"),
            ("/sites/instagram", "Instagram", "instagram", "instagram"),
        ],
    ),
    (
        "Monitoring",
        &[
            ("/tests", "Test links", "tests", "tests"),
            ("/alerts", "Alerts", "alerts", "bell"),
            ("/cache", "Cache", "cache", "cache"),
        ],
    ),
    (
        "Settings",
        &[
            ("/previews", "Previews", "previews", "previews"),
            ("/videos", "Videos", "videos", "videos"),
            ("/limits", "Rate limits", "limits", "limits"),
            ("/telegram", "Telegram IPs", "telegram", "telegram"),
        ],
    ),
    (
        "Admin",
        &[
            ("/security", "Security", "security", "security"),
            ("/activity", "Activity", "activity", "history"),
            ("/backup", "Backup", "backup", "backup"),
        ],
    ),
];

/// When the dashboard started, for the menu's uptime.
pub static STARTED: std::sync::LazyLock<i64> = std::sync::LazyLock::new(now);

pub struct Shell {
    nav: &'static str,
    notice: Option<&'static str>,
    error: Option<String>,
    /// Signed in with a setup link, which reaches only the Security page: no menu.
    setup: bool,
}

impl Shell {
    fn new(nav: &'static str, done: &Done) -> Self {
        Self { nav, notice: done.done.as_deref().and_then(notice), error: None, setup: false }
    }

    fn refused(nav: &'static str, error: impl Into<String>) -> Self {
        Self { nav, notice: None, error: Some(error.into()), setup: false }
    }

    pub const fn nav(&self) -> &'static [(&'static str, &'static [Link])] {
        if self.setup { &[] } else { &NAV }
    }

    pub fn is_current(&self, key: &str) -> bool {
        self.nav == key
    }

    pub fn is_site(&self, key: &str) -> bool {
        Source::from_name(key).is_some()
    }

    fn current(&self) -> Option<(&'static str, &'static str)> {
        NAV.iter().find_map(|(group, links)| links.iter().find(|l| l.2 == self.nav).map(|l| (*group, l.1)))
    }

    /// The breadcrumb's group: "Settings".
    pub fn group(&self) -> &'static str {
        self.current().map_or("", |c| c.0)
    }

    pub fn title(&self) -> &'static str {
        self.current().map_or("Security", |c| c.1)
    }

    /// "Homura 0.1.0 · up 3 h".
    pub fn build(&self) -> String {
        let up = match now() - *STARTED {
            s @ ..3600 => format!("{} min", s / 60),
            s @ ..172_800 => format!("{} h", s / 3600),
            s => format!("{} days", s / 86_400),
        };
        format!("Homura {} · up {up}", env!("CARGO_PKG_VERSION"))
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct Done {
    done: Option<String>,
}

fn notice(code: &str) -> Option<&'static str> {
    Some(match code {
        "saved" => "Saved.",
        "cookies" => "Cookies saved; the next fetch uses them.",
        "cookies_removed" => "Cookies removed.",
        "refreshed" => "Telegram's IP ranges are refreshed.",
        "forgotten" => "That link is out of the cache; its next preview fetches it again.",
        "cleared" => "The cache is empty.",
        "ran" => "Test links checked.",
        "checked" => "Checked.",
        "test_added" => "Added and checked once; from now on it's checked every 30 minutes.",
        "tests_added" => "Added and checked once; from now on they're checked every 30 minutes.",
        "test_removed" => "No longer tested; its history is gone.",
        "videos_deleted" => "Joined videos deleted; they're made again when needed.",
        "password" => "Password saved; other browsers are signed out.",
        "password_removed" => "Password and authenticator app removed.",
        "totp" => "Authenticator app on: the password signs in with a code from it.",
        "totp_removed" => "Authenticator app off: the password can't sign in until it's on again.",
        "passkey_removed" => "Passkey removed.",
        "imported" => "Settings imported.",
        "alert_sent" => "Test alert sent; check the alert chat.",
        "session_ended" => "That browser is signed out.",
        _ => return None,
    })
}

/// What a change did, for the Activity page, from the path it was posted to and its notice.
pub fn audit_text(path: &str, code: &str) -> Option<String> {
    let mut parts = path.trim_start_matches('/').split('/');
    let page = parts.next().unwrap_or_default();
    let site = parts.next().and_then(Source::from_name).map(Source::name);
    let text = match (code, page) {
        ("saved", "sites") => return Some(format!("Changed the {} settings", site?)),
        ("cookies", _) => return Some(format!("Replaced the {} cookies", site?)),
        ("cookies_removed", _) => return Some(format!("Removed the {} cookies", site?)),
        ("saved", "previews") => "Changed the preview settings",
        ("saved", "videos") => "Changed the video settings",
        ("saved", "limits") => "Changed the rate limits",
        ("saved", "cache") => "Changed the cache settings",
        ("saved", "alerts") => "Changed the alert chat",
        ("refreshed", _) => "Refreshed Telegram's IP ranges",
        ("forgotten", _) => "Removed a link from the cache",
        ("cleared", _) => "Cleared the cache",
        ("ran", _) => "Checked the test links",
        ("checked", _) => "Checked a test link",
        ("test_added", _) => "Added a test link",
        ("tests_added", _) => "Added test links",
        ("test_removed", _) => "Removed a test link",
        ("videos_deleted", _) => "Deleted the joined videos",
        ("password", _) => "Set a new password",
        ("password_removed", _) => "Removed the password",
        ("totp", _) => "Turned on the authenticator app",
        ("totp_removed", _) => "Turned off the authenticator app",
        ("passkey_removed", _) => "Removed a passkey",
        ("imported", _) => "Imported settings from a file",
        ("alert_sent", _) => "Sent a test alert",
        ("session_ended", _) => "Signed a browser out",
        _ => return None,
    };
    Some(text.to_string())
}

fn done(path: &str, code: &str) -> Response {
    Redirect::to(&format!("{path}?done={code}")).into_response()
}

fn refused(page: &impl Template) -> Response {
    (StatusCode::BAD_REQUEST, render(page)).into_response()
}

/// A refused change's page with status 400, or the error page if building it failed.
fn refused_page(page: Result<impl Template>) -> Response {
    match page {
        Ok(page) => refused(&page),
        Err(e) => server_error(&e),
    }
}

/// Renders `page`, or the error page if building it failed.
fn show(page: Result<impl Template>) -> Response {
    match page {
        Ok(page) => render(&page),
        Err(e) => server_error(&e),
    }
}

// ---- Formatting ----

fn now() -> i64 {
    jiff::Timestamp::now().as_second()
}

/// A moment in the browser's time zone.
fn when(at: i64) -> String {
    crate::dashboard::local(at).map_or_else(|| "?".into(), |t| t.strftime("%Y-%m-%d %H:%M").to_string())
}

fn day(at: i64) -> String {
    crate::dashboard::local(at).map_or_else(|| "?".into(), |t| t.strftime("%-d %b %Y").to_string())
}

fn ago(at: i64) -> String {
    match (now() - at).max(0) {
        0..10 => "just now".into(),
        s @ 10..60 => format!("{s} s ago"),
        s @ 60..3600 => format!("{} min ago", s / 60),
        s @ 3600..86_400 => format!("{} h ago", s / 3600),
        86_400..172_800 => "a day ago".into(),
        s => format!("{} days ago", s / 86_400),
    }
}

fn size(bytes: u64) -> String {
    match bytes {
        0..1_048_576 => format!("{} KB", bytes.div_ceil(1024)),
        _ => format!("{:.1} MB", bytes as f64 / 1_048_576.0),
    }
}

fn percent(part: u64, whole: u64) -> String {
    (part * 100 + whole / 2).checked_div(whole).map_or_else(|| "–".into(), |p| format!("{p}%"))
}

fn millis(ms: u64) -> String {
    if ms >= 1000 { format!("{:.1} s", ms as f64 / 1000.0) } else { format!("{ms} ms") }
}

fn duration(d: Duration) -> String {
    millis(u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

// ---- Shared by several pages ----

/// A card's headline, the line under it, and its colour: `ok`, `idle` (nothing to do) or `bad`
/// (needs attention).
pub struct Status {
    pub state: String,
    pub detail: String,
    pub level: &'static str,
}

const OK: &str = "ok";
const IDLE: &str = "idle";
const BAD: &str = "bad";
const WARN: &str = "warn";

impl Status {
    fn new(state: impl Into<String>, detail: impl Into<String>, level: &'static str) -> Self {
        Self { state: state.into(), detail: detail.into(), level }
    }
}

fn cookie_status(source: Source, status: &cookies::Status) -> Status {
    if status.count == 0 {
        Status::new("None", "public posts only", IDLE)
    } else if let Some(at) = status.rejected_at {
        Status::new("Rejected", format!("by {}, {}", source.name(), ago(at)), BAD)
    } else if !status.has_session {
        Status::new("Incomplete", format!("{} is missing", cookies::session_cookies(source).join(" or ")), BAD)
    } else if let Some(expires) = status.expires_at {
        Status::new("OK", format!("expire {}", day(expires)), OK)
    } else {
        Status::new("OK", "no expiry date", OK)
    }
}

/// "Firefox on Linux" from a user agent; unknown parts are left out.
pub fn browser(user_agent: &str) -> String {
    let has = |needle: &str| user_agent.contains(needle);
    let name =
        [("Firefox/", "Firefox"), ("Edg/", "Edge"), ("OPR/", "Opera"), ("Chrome/", "Chrome"), ("CriOS/", "Chrome")]
            .into_iter()
            .find(|(needle, _)| has(needle))
            .map(|(_, name)| name)
            .or_else(|| has("Safari/").then_some("Safari"));
    let system = [
        ("Android", "Android"),
        ("iPhone", "iOS"),
        ("iPad", "iOS"),
        ("Macintosh", "macOS"),
        ("Windows", "Windows"),
        ("CrOS", "ChromeOS"),
        ("Linux", "Linux"),
    ]
    .into_iter()
    .find(|(needle, _)| has(needle))
    .map(|(_, system)| system);
    match (name, system) {
        (Some(name), Some(system)) => format!("{name} on {system}"),
        (Some(name), None) => name.into(),
        (None, Some(system)) => format!("A browser on {system}"),
        (None, None) => user_agent.chars().take(60).collect(),
    }
}

fn plural(n: usize, what: &str) -> String {
    if n == 1 { format!("1 {what}") } else { format!("{n} {what}s") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting() {
        assert_eq!(percent(1, 3), "33%");
        assert_eq!(percent(2, 3), "67%");
        assert_eq!(percent(0, 0), "–");
        assert_eq!(millis(950), "950 ms");
        assert_eq!(millis(1250), "1.2 s");
        assert_eq!(size(2048), "2 KB");
        assert_eq!(size(3 * 1_048_576), "3.0 MB");
        assert_eq!(when(0), "1970-01-01 00:00");
        assert_eq!((plural(1, "photo"), plural(3, "video")), ("1 photo".to_string(), "3 videos".to_string()));
        let firefox = "Mozilla/5.0 (X11; Linux x86_64; rv:140.0) Gecko/20100101 Firefox/140.0";
        let chrome =
            "Mozilla/5.0 (Linux; Android 14) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0 Mobile Safari/537.36";
        let safari = "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Mobile/15E148 Safari/604.1";
        assert_eq!(
            [browser(firefox), browser(chrome), browser(safari)],
            ["Firefox on Linux", "Chrome on Android", "Safari on iOS"]
        );
        assert_eq!(browser("curl/8.0"), "curl/8.0");
    }
}
