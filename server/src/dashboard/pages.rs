//! The dashboard's pages. A change redirects back with `?done=<what>` for the notice; a refused
//! change shows the page again, with what was entered, the error and status 400.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::response::{IntoResponse, Redirect, Response};
use axum::{Extension, Form, Json};
use ipnet::IpNet;
use serde::Deserialize;
use serde_json::json;
use webauthn_rs::prelude::RegisterPublicKeyCredential;

use super::auth::{self, Identity};
use super::chart::{self, Chart, Range, Spark};
use super::{Shared, json_error, render, server_error, with_cookie};
use crate::app::{App, Busy};
use crate::cache::Entry;
use crate::canary::{self, TestResult};
use crate::cookies::{self, parse_export};
use crate::extract::facebook::url::{PageType, Target};
use crate::extract::{CookieChoice, ExtractError, Post, Stage, Timings};
use crate::render::{self as preview, MediaUrl};
use crate::settings::{CookiePolicy, FailureResponse, MediaLinks, Settings, VideoQuality};
use crate::stats::{Agg, Failure};
use crate::video::{BucketHealth, Usage};

// ---- The frame around every page ----

/// A menu entry: path, label, and the key pages use to mark themselves current (also its icon's
/// name).
type Link = (&'static str, &'static str, &'static str);

/// The menu, in groups under a heading (none for the first).
const NAV: [(&str, &[Link]); 4] = [
    ("", &[("/", "Overview", "overview"), ("/playground", "Playground", "playground")]),
    (
        "Monitoring",
        &[
            ("/failures", "Failures", "failures"),
            ("/tests", "Test links and alerts", "tests"),
            ("/cache", "Cache", "cache"),
        ],
    ),
    (
        "Settings",
        &[
            ("/facebook", "Facebook", "facebook"),
            ("/previews", "Previews", "previews"),
            ("/videos", "Videos", "videos"),
            ("/limits", "Rate limits", "limits"),
            ("/telegram", "Telegram IPs", "telegram"),
        ],
    ),
    ("Admin", &[("/security", "Security", "security"), ("/backup", "Backup", "backup")]),
];

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

fn done(path: &str, code: &str) -> Response {
    Redirect::to(&format!("{path}?done={code}")).into_response()
}

fn refused(page: &impl Template) -> Response {
    (StatusCode::BAD_REQUEST, render(page)).into_response()
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

fn when(at: i64) -> String {
    jiff::Timestamp::from_second(at).map_or_else(|_| "?".into(), |t| t.strftime("%Y-%m-%d %H:%M UTC").to_string())
}

fn day(at: i64) -> String {
    jiff::Timestamp::from_second(at).map_or_else(|_| "?".into(), |t| t.strftime("%-d %b %Y").to_string())
}

fn ago(at: i64) -> String {
    match (now() - at).max(0) {
        0..10 => "just now".into(),
        s @ 10..60 => format!("{s} s ago"),
        s @ 60..3600 => format!("{} min ago", s / 60),
        s @ 3600..86_400 => format!("{} h ago", s / 3600),
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

// ---- Overview ----

/// Request counts over a window, split the way the overview shows them.
#[derive(Debug, Default, Clone, Copy)]
struct Counts {
    all: u64,
    telegram: u64,
    hits: u64,
    success: u64,
    redirects: u64,
    extracted: Agg,
    slowest: u64,
}

impl Counts {
    fn add(&mut self, outcome: &str, agg: Agg) {
        self.all += agg.count;
        match outcome {
            "redirect" => self.redirects += agg.count,
            "rate_limited" => {}
            _ => {
                self.telegram += agg.count;
                self.slowest = self.slowest.max(agg.max_ms);
                match outcome {
                    "hit" => {
                        self.hits += agg.count;
                        self.success += agg.count;
                    }
                    "extracted" => {
                        self.success += agg.count;
                        self.extracted.count += agg.count;
                        self.extracted.total_ms += agg.total_ms;
                    }
                    _ => {}
                }
            }
        }
    }
}

async fn counts(app: &App, seconds: i64) -> Result<(Counts, BTreeMap<String, Counts>)> {
    let mut total = Counts::default();
    let mut by_type: BTreeMap<String, Counts> = BTreeMap::new();
    for ((outcome, page_type), agg) in app.stats.totals(now() - seconds).await? {
        total.add(&outcome, agg);
        by_type.entry(page_type).or_default().add(&outcome, agg);
    }
    Ok((total, by_type))
}

/// The overview numbers that live pages refresh every few seconds.
pub async fn snapshot(app: &App) -> Result<serde_json::Value> {
    let (hour, _) = counts(app, 3600).await?;
    let (day, _) = counts(app, 86_400).await?;
    let (entries, _) = app.cache.usage().await;
    Ok(json!({
        "hour_total": hour.all,
        "day_telegram": day.telegram,
        "cache_entries": entries,
        "budget_in_use": app.budget.in_use(),
    }))
}

struct Row {
    page_type: String,
    telegram: u64,
    hit_rate: String,
    success_rate: String,
    typical: String,
    slowest: String,
    redirects: u64,
    spark: Spark,
}

/// A card on the overview that links to the page about it.
struct Health {
    href: &'static str,
    name: &'static str,
    icon: &'static str,
    status: Status,
}

#[derive(Template)]
#[template(path = "dashboard/overview.html")]
pub struct Overview {
    shell: Shell,
    proxy_warning: bool,
    range: Range,
    ranges: [Range; 5],
    hour_total: u64,
    day_telegram: u64,
    day_success: String,
    success_level: &'static str,
    cache_entries: u64,
    cache_size: String,
    day_hit_rate: String,
    budget_in_use: u32,
    budget_exhausted: u64,
    health: Vec<Health>,
    chart: Chart,
    rows: Vec<Row>,
}

/// A card's headline, the line under it, and its colour: `ok`, `idle` (nothing to do) or `bad`
/// (needs attention).
struct Status {
    state: String,
    detail: String,
    level: &'static str,
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

fn cookie_status(status: &cookies::Status) -> Status {
    if status.count == 0 {
        Status::new("None", "public posts only", IDLE)
    } else if let Some(at) = status.rejected_at {
        Status::new("Rejected", format!("by Facebook, {}", ago(at)), BAD)
    } else if !status.has_session {
        Status::new("Incomplete", "c_user or xs is missing", BAD)
    } else if let Some(expires) = status.expires_at {
        Status::new("OK", format!("expire {}", day(expires)), OK)
    } else {
        Status::new("OK", "no expiry date", OK)
    }
}

async fn tests_status(app: &App, settings: &Settings) -> Result<Status> {
    let results = canary::results(app).await?;
    Ok(if settings.test_links.is_empty() {
        Status::new("None", "add some under Test links", IDLE)
    } else if results.is_empty() {
        Status::new("Not run", "the first check runs soon", IDLE)
    } else {
        let ok = results.iter().filter(|r| r.ok).count();
        let last = results.iter().map(|r| r.checked_at).max().unwrap_or_default();
        let level = if ok < results.len() { BAD } else { OK };
        Status::new(format!("{ok}/{} OK", results.len()), format!("checked {}", ago(last)), level)
    })
}

async fn videos_status(app: &App, settings: &Settings) -> Status {
    let videos = &app.videos;
    if settings.video_quality == VideoQuality::Standard {
        return Status::new("720p", "Facebook's own file", IDLE);
    }
    if !videos.wanted(settings) {
        return Status::new("720p", "1080p needs a bucket, and none is set up", BAD);
    }
    if videos.bucket_problem(settings).is_some() {
        let at = videos.bucket_health().failing.map_or(0, |f| f.at);
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

/// `?range=` for the overview's chart and table, and the CSV export.
#[derive(Debug, Default, Deserialize)]
pub struct RangeQuery {
    #[serde(default)]
    range: String,
    done: Option<String>,
}

impl RangeQuery {
    fn range(&self) -> Range {
        Range::parse(&self.range).unwrap_or_default()
    }
}

pub async fn overview(State(d): State<Shared>, Query(q): Query<RangeQuery>) -> Response {
    let shell = Shell::new("overview", &Done { done: q.done.clone() });
    show(overview_page(&d.app, shell, q.range()).await)
}

/// Green from 90% success, amber from 75%: login walls and deleted posts always fail some.
fn success_level(success: u64, of: u64) -> &'static str {
    match (success * 100).checked_div(of) {
        None => IDLE,
        Some(90..) => OK,
        Some(75..) => WARN,
        Some(_) => BAD,
    }
}

async fn overview_page(app: &App, shell: Shell, range: Range) -> Result<Overview> {
    let settings = app.settings.get();
    let now = now();
    let (hour, _) = counts(app, 3600).await?;
    let (day, _) = counts(app, 86_400).await?;
    let (_, by_type) = counts(app, range.seconds()).await?;
    let (cache_entries, cache_bytes) = app.cache.usage().await;
    let health = vec![
        Health {
            href: "/facebook#cookies",
            name: "Cookies",
            icon: "cookie",
            status: cookie_status(&app.cookies.status()),
        },
        Health { href: "/tests", name: "Test links", icon: "tests", status: tests_status(app, &settings).await? },
        Health { href: "/videos", name: "Videos", icon: "videos", status: videos_status(app, &settings).await },
    ];
    let chart = chart::requests(range, &app.stats.series(range.since(now), range.step()).await?, now);
    let sparks = chart::per_key(range, &app.stats.series_by_type(range.since(now), range.step()).await?, now);
    let rows = by_type
        .into_iter()
        .filter(|(page_type, c)| page_type != "-" && (c.telegram > 0 || c.redirects > 0))
        .map(|(page_type, c)| Row {
            spark: chart::spark(sparks.get(&page_type).map_or(&[][..], Vec::as_slice)),
            page_type,
            telegram: c.telegram,
            hit_rate: percent(c.hits, c.telegram),
            success_rate: percent(c.success, c.telegram),
            typical: match c.extracted.count {
                0 => "–".into(),
                n => millis(c.extracted.total_ms / n),
            },
            slowest: if c.telegram == 0 { "–".into() } else { millis(c.slowest) },
            redirects: c.redirects,
        })
        .collect();
    Ok(Overview {
        shell,
        proxy_warning: app.stats.proxy_warning(),
        range,
        ranges: Range::ALL,
        hour_total: hour.all,
        day_telegram: day.telegram,
        day_success: percent(day.success, day.telegram),
        success_level: success_level(day.success, day.telegram),
        cache_entries,
        cache_size: size(cache_bytes),
        day_hit_rate: percent(day.hits, day.telegram),
        budget_in_use: app.budget.in_use(),
        budget_exhausted: app.budget.exhausted(),
        health,
        chart,
        rows,
    })
}

/// The chart alone, which the overview reloads every minute.
pub async fn overview_chart(State(d): State<Shared>, Query(q): Query<RangeQuery>) -> Response {
    let (range, now) = (q.range(), now());
    match d.app.stats.series(range.since(now), range.step()).await {
        Ok(rows) => render(&chart::requests(range, &rows, now)),
        Err(e) => server_error(&e),
    }
}

/// Requests per period and outcome, as CSV.
pub async fn export_requests(State(d): State<Shared>, Query(q): Query<RangeQuery>) -> Response {
    let (range, now) = (q.range(), now());
    let rows = match d.app.stats.series(range.since(now), range.step()).await {
        Ok(rows) => rows,
        Err(e) => return server_error(&e),
    };
    let mut csv = String::from("start_utc,outcome,requests\n");
    for (bucket, outcome, count) in rows {
        let start = jiff::Timestamp::from_second(bucket).map(|t| t.to_string()).unwrap_or_default();
        csv.push_str(&format!("{start},{outcome},{count}\n"));
    }
    let name = format!("attachment; filename=\"requests-{}.csv\"", range.as_str());
    ([(CONTENT_TYPE, "text/csv; charset=utf-8")], [(CONTENT_DISPOSITION, name)], csv).into_response()
}

// ---- Facebook: fetching and cookies ----

#[derive(Template)]
#[template(path = "dashboard/facebook.html")]
pub struct FacebookPage {
    shell: Shell,
    s: Settings,
    policy: &'static str,
    cookies: Status,
    cookie_summary: String,
    /// The saved cookies' names and expiry dates; never their values.
    saved: Vec<(cookies::Listed, String)>,
}

fn facebook_page(app: &App, shell: Shell, s: Settings) -> FacebookPage {
    let status = app.cookies.status();
    let cookie_summary = if status.count == 0 {
        "No cookies saved, so only public posts work.".to_string()
    } else {
        let saved = status.saved_at.map(|at| format!("Saved {}.", when(at))).unwrap_or_default();
        let state = if let Some(at) = status.rejected_at {
            format!(" Facebook rejected them {}; add fresh ones.", ago(at))
        } else if !status.has_session {
            " c_user or xs is missing, so they aren't used.".to_string()
        } else if let Some(expires) = status.expires_at {
            format!(" The session expires {}.", day(expires))
        } else {
            String::new()
        };
        format!("{saved}{state}").trim_start().to_string()
    };
    FacebookPage {
        policy: match s.cookie_policy {
            CookiePolicy::Fallback => "fallback",
            CookiePolicy::Always => "always",
            CookiePolicy::Never => "never",
        },
        saved: app
            .cookies
            .list()
            .into_iter()
            .map(|c| {
                let expires = c.expires_at.map_or_else(|| "when the browser closes".into(), day);
                (c, expires)
            })
            .collect(),
        cookies: cookie_status(&status),
        cookie_summary,
        shell,
        s,
    }
}

pub async fn facebook(State(d): State<Shared>, Query(q): Query<Done>) -> Response {
    render(&facebook_page(&d.app, Shell::new("facebook", &q), (*d.app.settings.get()).clone()))
}

#[derive(Deserialize)]
pub struct FacebookForm {
    proxy: String,
    cookie_policy: CookiePolicy,
    extract_timeout_seconds: u64,
}

pub async fn save_facebook(State(d): State<Shared>, Form(f): Form<FacebookForm>) -> Response {
    let mut s = (*d.app.settings.get()).clone();
    let proxy = f.proxy.trim();
    s.proxy = (!proxy.is_empty()).then(|| proxy.to_string());
    s.cookie_policy = f.cookie_policy;
    s.extract_timeout_seconds = f.extract_timeout_seconds;
    match d.app.settings.set(s.clone()).await {
        Ok(()) => done("/facebook", "saved"),
        Err(e) => refused(&facebook_page(&d.app, Shell::refused("facebook", format!("{e:#}")), s)),
    }
}

#[derive(Deserialize)]
pub struct CookiesForm {
    cookies: String,
}

pub async fn save_cookies(State(d): State<Shared>, Form(f): Form<CookiesForm>) -> Response {
    let saved = match parse_export(&f.cookies) {
        Ok(cookies) => d.app.cookies.save(cookies).await,
        Err(e) => Err(e),
    };
    match saved {
        Ok(()) => {
            // A post behind a login wall shouldn't wait out its cached failure.
            d.app.cache.forget_failures().await;
            done("/facebook", "cookies")
        }
        Err(e) => refused(&facebook_page(
            &d.app,
            Shell::refused("facebook", format!("{e:#}")),
            (*d.app.settings.get()).clone(),
        )),
    }
}

pub async fn clear_cookies(State(d): State<Shared>) -> Response {
    match d.app.cookies.clear().await {
        Ok(()) => done("/facebook", "cookies_removed"),
        Err(e) => server_error(&e),
    }
}

// ---- Previews ----

#[derive(Template)]
#[template(path = "dashboard/previews.html")]
pub struct PreviewsPage {
    shell: Shell,
    s: Settings,
    failure: &'static str,
    media_links: &'static str,
}

const fn previews_page(shell: Shell, s: Settings) -> PreviewsPage {
    PreviewsPage {
        failure: match s.failure_response {
            FailureResponse::Redirect => "redirect",
            FailureResponse::Page => "page",
            FailureResponse::Unavailable => "unavailable",
        },
        media_links: match s.media_links {
            MediaLinks::Direct => "direct",
            MediaLinks::Redirect => "redirect",
        },
        shell,
        s,
    }
}

pub async fn previews(State(d): State<Shared>, Query(q): Query<Done>) -> Response {
    render(&previews_page(Shell::new("previews", &q), (*d.app.settings.get()).clone()))
}

#[derive(Deserialize)]
pub struct PreviewsForm {
    site_name: String,
    description_length: usize,
    failure_response: FailureResponse,
    media_links: MediaLinks,
    iv_video: Option<String>,
    iv_photo: Option<String>,
    iv_album: Option<String>,
    iv_text: Option<String>,
    iv_profile: Option<String>,
    iv_counts: Option<String>,
    author_pictures: Option<String>,
}

pub async fn save_previews(State(d): State<Shared>, Form(f): Form<PreviewsForm>) -> Response {
    let mut s = (*d.app.settings.get()).clone();
    s.site_name = f.site_name.trim().to_string();
    s.description_length = f.description_length;
    s.failure_response = f.failure_response;
    s.media_links = f.media_links;
    s.instant_view.video = f.iv_video.is_some();
    s.instant_view.photo = f.iv_photo.is_some();
    s.instant_view.album = f.iv_album.is_some();
    s.instant_view.text = f.iv_text.is_some();
    s.instant_view.profile = f.iv_profile.is_some();
    s.instant_view_counts = f.iv_counts.is_some();
    s.author_pictures = f.author_pictures.is_some();
    match d.app.settings.set(s.clone()).await {
        Ok(()) => done("/previews", "saved"),
        Err(e) => refused(&previews_page(Shell::refused("previews", format!("{e:#}")), s)),
    }
}

// ---- Videos ----

struct BucketView {
    api: String,
    name: String,
    region: String,
    /// An address Telegram can't reach, so it can't download the videos.
    private: bool,
    /// Not R2, so possibly served through Cloudflare's proxy.
    self_hosted: bool,
}

/// The bucket's state, from the requests videos made of it.
struct HealthView {
    /// `Some(true)` works, `Some(false)` failing, `None` not used yet.
    ok: Option<bool>,
    detail: String,
    error: Option<String>,
}

#[derive(Template)]
#[template(path = "dashboard/videos.html")]
pub struct VideosPage {
    shell: Shell,
    s: Settings,
    best: bool,
    bucket: Option<BucketView>,
    health: HealthView,
    /// `None` when the bucket can't be listed.
    stored: Option<Usage>,
    stored_size: String,
    /// The space setting, for the meter.
    space_bytes: u64,
    failures: Vec<(String, crate::video::Failure)>,
}

async fn videos_page(app: &App, shell: Shell, s: Settings) -> VideosPage {
    let videos = &app.videos;
    let bucket = videos.bucket().map(|b| BucketView {
        api: b.api.as_str().trim_end_matches('/').to_string(),
        name: b.bucket.clone(),
        region: b.region.clone(),
        private: b.is_private(),
        self_hosted: !b.api.host_str().is_some_and(|h| h.ends_with(".r2.cloudflarestorage.com")),
    });
    let stored = videos.usage().await;
    // After the usage above, whose first look at the bucket lists it.
    let health = match videos.bucket_health() {
        BucketHealth { failing: Some(f), .. } => HealthView {
            ok: Some(false),
            detail: format!("since {}", ago(f.since)),
            error: Some(format!(
                "{} Meanwhile Telegram gets 720p; the first look after {} (a video, a preview, this page) tries the bucket again.",
                f.error,
                when(f.retry_at())
            )),
        },
        BucketHealth { worked_at: Some(at), .. } => {
            HealthView { ok: Some(true), detail: format!("last request {}", ago(at)), error: None }
        }
        BucketHealth { .. } => HealthView { ok: None, detail: "not used yet".into(), error: None },
    };
    VideosPage {
        best: s.video_quality == VideoQuality::Best,
        health,
        failures: videos.recent_failures().into_iter().map(|(at, f)| (ago(at), f)).collect(),
        bucket,
        stored_size: stored.map(|u| size(u.bytes)).unwrap_or_default(),
        stored,
        space_bytes: s.video_space_megabytes * 1024 * 1024,
        shell,
        s,
    }
}

pub async fn videos(State(d): State<Shared>, Query(q): Query<Done>) -> Response {
    render(&videos_page(&d.app, Shell::new("videos", &q), (*d.app.settings.get()).clone()).await)
}

#[derive(Deserialize)]
pub struct VideosForm {
    video_quality: VideoQuality,
    video_megabytes: u64,
    video_space_megabytes: u64,
    video_days: u64,
}

pub async fn save_videos(State(d): State<Shared>, Form(f): Form<VideosForm>) -> Response {
    let app = &d.app;
    let mut s = (*app.settings.get()).clone();
    s.video_quality = f.video_quality;
    s.video_megabytes = f.video_megabytes;
    s.video_space_megabytes = f.video_space_megabytes;
    s.video_days = f.video_days;
    let saved = if s.video_quality == VideoQuality::Best && app.videos.bucket().is_none() {
        Err(anyhow::anyhow!(
            "1080p videos need a bucket, and none is set up: set VIDEO_BUCKET and the other VIDEO_BUCKET_ variables in .env (docs/deployment.md), then restart the server."
        ))
    } else {
        app.settings.set(s.clone()).await
    };
    match saved {
        Ok(()) => done("/videos", "saved"),
        Err(e) => refused(&videos_page(app, Shell::refused("videos", format!("{e:#}")), s).await),
    }
}

pub async fn delete_videos(State(d): State<Shared>) -> Response {
    match d.app.videos.delete_all().await {
        Ok(()) => done("/videos", "videos_deleted"),
        Err(e) => refused(
            &videos_page(
                &d.app,
                Shell::refused("videos", format!("Couldn't delete the bucket's videos: {e:#}")),
                (*d.app.settings.get()).clone(),
            )
            .await,
        ),
    }
}

// ---- Rate limits ----

#[derive(Template)]
#[template(path = "dashboard/limits.html")]
pub struct LimitsPage {
    shell: Shell,
    s: Settings,
    in_use: u32,
    exhausted: u64,
}

fn limits_page(app: &App, shell: Shell, s: Settings) -> LimitsPage {
    LimitsPage { shell, s, in_use: app.budget.in_use(), exhausted: app.budget.exhausted() }
}

pub async fn limits(State(d): State<Shared>, Query(q): Query<Done>) -> Response {
    render(&limits_page(&d.app, Shell::new("limits", &q), (*d.app.settings.get()).clone()))
}

#[derive(Deserialize)]
pub struct LimitsForm {
    ip_requests_per_minute: u32,
    facebook_fetches_per_minute: u32,
    facebook_concurrency: u32,
}

pub async fn save_limits(State(d): State<Shared>, Form(f): Form<LimitsForm>) -> Response {
    let mut s = (*d.app.settings.get()).clone();
    s.ip_requests_per_minute = f.ip_requests_per_minute;
    s.facebook_fetches_per_minute = f.facebook_fetches_per_minute;
    s.facebook_concurrency = f.facebook_concurrency;
    match d.app.settings.set(s.clone()).await {
        Ok(()) => done("/limits", "saved"),
        Err(e) => refused(&limits_page(&d.app, Shell::refused("limits", format!("{e:#}")), s)),
    }
}

// ---- Telegram IP ranges ----

#[derive(Template)]
#[template(path = "dashboard/telegram.html")]
pub struct TelegramPage {
    shell: Shell,
    proxy_warning: bool,
    client_ip_from: &'static str,
    trusted: String,
    nets: Vec<IpNet>,
    refreshed: String,
    suspects: Vec<(String, std::net::IpAddr, String, bool)>,
}

fn telegram_page(app: &App, shell: Shell) -> TelegramPage {
    let ranges = app.telegram_ips.current();
    TelegramPage {
        shell,
        proxy_warning: app.stats.proxy_warning(),
        client_ip_from: app.resolver.source().as_str(),
        trusted: app.config.trusted_proxies.iter().map(ToString::to_string).collect::<Vec<_>>().join(", "),
        nets: ranges.nets.clone(),
        refreshed: match ranges.refreshed_at {
            Some(at) => format!("downloaded {}", ago(at)),
            None => "the copy bundled with this build; the first download is due".into(),
        },
        suspects: app.stats.suspects().into_iter().map(|s| (when(s.at), s.ip, s.user_agent, s.from_proxy)).collect(),
    }
}

pub async fn telegram(State(d): State<Shared>, Query(q): Query<Done>) -> Response {
    render(&telegram_page(&d.app, Shell::new("telegram", &q)))
}

pub async fn refresh_telegram(State(d): State<Shared>) -> Response {
    let refreshed = match crate::net::client() {
        Ok(client) => d.app.telegram_ips.refresh(&client, &d.app.db).await,
        Err(e) => Err(e),
    };
    match refreshed {
        Ok(_) => done("/telegram", "refreshed"),
        Err(e) => refused(&telegram_page(&d.app, Shell::refused("telegram", format!("Couldn't refresh: {e:#}")))),
    }
}

// ---- Playground ----

struct Mock {
    site: String,
    title: String,
    description: String,
    image: Option<String>,
    video: bool,
    instant_view: bool,
    tags: String,
}

struct Tried {
    page_type: &'static str,
    outcome: &'static str,
    key: String,
    timings: String,
    error: Option<String>,
    preview: Option<Mock>,
    /// The whole page Telegram would get, for the sandboxed Instant View frame.
    html: String,
    json: String,
}

/// The article page with a stylesheet that keeps its media inside the playground's frame.
fn framed(html: &str) -> String {
    let link = format!(r#"<link rel="stylesheet" href="{}">"#, super::asset("iv.css"));
    match html.find("</head>") {
        Some(at) => format!("{}{link}{}", &html[..at], &html[at..]),
        None => format!("{link}{html}"),
    }
}

#[derive(Template)]
#[template(path = "dashboard/playground.html")]
pub struct PlaygroundPage {
    shell: Shell,
    url: String,
    fresh: bool,
    cookies: bool,
    result: Option<Tried>,
}

#[derive(Debug, Default, Deserialize)]
pub struct PlaygroundQuery {
    #[serde(default)]
    url: String,
    done: Option<String>,
}

pub async fn playground(Query(q): Query<PlaygroundQuery>) -> Response {
    let shell = Shell::new("playground", &Done { done: q.done });
    render(&PlaygroundPage { shell, url: q.url, fresh: true, cookies: false, result: None })
}

#[derive(Deserialize)]
pub struct PlaygroundForm {
    url: String,
    fresh: Option<String>,
    cookies: Option<String>,
}

pub async fn run_playground(State(d): State<Shared>, Form(f): Form<PlaygroundForm>) -> Response {
    let (fresh, cookies) = (f.fresh.is_some(), f.cookies.is_some());
    let page = |shell, result| PlaygroundPage { shell, url: f.url.clone(), fresh, cookies, result };
    let Some(target) = Target::from_link(f.url.trim()) else {
        return refused(&page(Shell::refused("playground", "That isn't a facebook.com link."), None));
    };
    match try_link(&d.app, &target, fresh, cookies).await {
        Ok(tried) => render(&page(Shell::new("playground", &Done::default()), Some(tried))),
        Err(Busy) => refused(&page(
            Shell::refused("playground", "No Facebook fetch was free within 2 seconds; try again in a moment."),
            None,
        )),
    }
}

/// A post, and how long extracting it took (`None` when it came from the cache).
type Found = (Arc<Post>, Option<Timings>);

async fn try_link(app: &App, target: &Target, fresh: bool, cookies: bool) -> Result<Tried, Busy> {
    let settings = app.settings.get();
    let cached = if fresh { None } else { app.cache.get(target.key()).await };
    let (outcome, result): (_, Result<Found, ExtractError>) = match cached {
        Some(Entry { outcome, .. }) => ("from the cache", outcome.map(|post| (post, None))),
        None => {
            let choice = if cookies { CookieChoice::Force } else { CookieChoice::Policy };
            match app.extract(target, choice, &settings).await? {
                Ok((post, timings)) => {
                    // Not what Telegram gets when the cookie policy wouldn't have used the cookies,
                    // so kept out of the cache it's served from.
                    if !cookies {
                        let ttl = Duration::from_secs(settings.cache_minutes * 60);
                        let entry = Entry::post(post.clone(), ttl);
                        app.cache.insert(settings.cache_megabytes, target.key().to_string(), entry).await;
                    }
                    ("extracted", Ok((Arc::new(post), Some(timings))))
                }
                Err(e) => ("failed", Err(e)),
            }
        }
    };
    let mut tried = Tried {
        page_type: target.page_type.as_str(),
        outcome,
        key: target.key().to_string(),
        timings: String::new(),
        error: None,
        preview: None,
        html: String::new(),
        json: String::new(),
    };
    match result {
        Err(e) => tried.error = Some(e.to_string()),
        Ok((post, timings)) => {
            tried.timings = timings.map_or_else(
                || "No fetch: the post came from the cache.".into(),
                |t| {
                    format!(
                        "fetch {} · parse {} · extract {} · {} request{}",
                        duration(t.fetch),
                        duration(t.parse),
                        duration(t.extract),
                        t.requests,
                        if t.requests == 1 { "" } else { "s" }
                    )
                },
            );
            let media: Vec<MediaUrl> = post.media.iter().map(MediaUrl::direct).collect();
            let summary = preview::summary(&post, &media, &settings);
            tried.html = framed(&preview::preview(&post, &media, &settings, target.original.as_str()));
            let tags = tried
                .html
                .lines()
                .filter(|line| line.starts_with("<meta property=") || line.starts_with("<meta name=\"twitter"))
                .collect::<Vec<_>>()
                .join("\n");
            tried.preview = Some(Mock {
                site: summary.site.to_string(),
                title: summary.title.to_string(),
                description: summary.description,
                image: summary.image.map(str::to_string),
                video: summary.video.is_some(),
                instant_view: summary.instant_view,
                tags,
            });
            tried.json = serde_json::to_string_pretty(&*post).unwrap_or_default();
        }
    }
    Ok(tried)
}

// ---- Failures ----

#[derive(Template)]
#[template(path = "dashboard/failures.html")]
pub struct FailuresPage {
    shell: Shell,
    page_types: Vec<&'static str>,
    stages: Vec<&'static str>,
    page_type: String,
    stage: String,
    failures: Vec<(String, Failure, String)>,
}

#[derive(Debug, Default, Deserialize)]
pub struct FailureFilter {
    #[serde(default)]
    page_type: String,
    #[serde(default)]
    stage: String,
}

pub async fn failures(State(d): State<Shared>, Query(q): Query<FailureFilter>) -> Response {
    let page = async {
        let failures = d
            .app
            .stats
            .failures(500)
            .await?
            .into_iter()
            .filter(|f| {
                (q.page_type.is_empty() || f.page_type == q.page_type) && (q.stage.is_empty() || f.stage == q.stage)
            })
            .take(200)
            .map(|f| {
                let link = url::form_urlencoded::byte_serialize(f.url.as_bytes()).collect();
                (when(f.at), f, link)
            })
            .collect();
        anyhow::Ok(FailuresPage {
            shell: Shell::new("failures", &Done::default()),
            page_types: PageType::ALL.iter().map(|t| t.as_str()).collect(),
            stages: Stage::ALL.iter().map(|s| s.as_str()).collect(),
            page_type: q.page_type.clone(),
            stage: q.stage.clone(),
            failures,
        })
    };
    show(page.await)
}

// ---- Cache ----

/// One cached post or failure.
struct CacheRow {
    key: String,
    /// `key` for a query string.
    link: String,
    title: String,
    /// `None` for a failure: it never got as far as knowing.
    page_type: Option<&'static str>,
    /// What the post holds, or why it failed.
    detail: String,
    cached: String,
    expires: String,
}

/// Rows the cache page lists at most; the search finds the rest.
const CACHE_ROWS: usize = 100;

#[derive(Template)]
#[template(path = "dashboard/cache.html")]
pub struct CachePage {
    shell: Shell,
    s: Settings,
    posts: usize,
    failed: usize,
    size: String,
    q: String,
    rows: Vec<CacheRow>,
    shown: usize,
    matching: usize,
}

/// What the Cache page calls an entry, and searches by with its link.
fn cache_title(entry: &Entry) -> &str {
    match &entry.outcome {
        Ok(post) => post.title.as_deref().or(post.author.as_deref()).unwrap_or("Untitled"),
        Err(_) => "Couldn't load",
    }
}

fn cache_row(key: &str, entry: &Entry) -> CacheRow {
    let (page_type, detail) = match &entry.outcome {
        Ok(post) => {
            let photos = post.photo_count();
            let videos = post.media.len() - photos;
            let detail = match (photos, videos) {
                (0, 0) => "text".to_string(),
                (p, 0) => plural(p, "photo"),
                (0, v) => plural(v, "video"),
                (p, v) => format!("{}, {}", plural(p, "photo"), plural(v, "video")),
            };
            (Some(post.page_type.as_str()), detail)
        }
        Err(e) => (None, format!("{}: {}", e.stage.as_str(), e.message)),
    };
    CacheRow {
        link: url::form_urlencoded::byte_serialize(key.as_bytes()).collect(),
        key: key.to_string(),
        title: cache_title(entry).to_string(),
        page_type,
        detail,
        cached: ago(entry.stored_at),
        expires: until(entry.expires_at()),
    }
}

/// "Firefox on Linux" from a user agent; unknown parts are left out.
fn browser(user_agent: &str) -> String {
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

fn until(at: i64) -> String {
    match (at - now()).max(0) {
        0..60 => "in under a minute".into(),
        s @ 60..3600 => format!("in {} min", s / 60),
        s => format!("in {} h", s / 3600),
    }
}

async fn cache_page(app: &App, shell: Shell, s: Settings, q: &str) -> CachePage {
    let (_, bytes) = app.cache.usage().await;
    let all = app.cache.entries().await;
    let failed = all.iter().filter(|(_, entry)| entry.outcome.is_err()).count();
    let needle = q.trim().to_lowercase();
    let matches = |key: &str, entry: &Entry| {
        needle.is_empty() || key.to_lowercase().contains(&needle) || cache_title(entry).to_lowercase().contains(&needle)
    };
    // The cache can hold far more than a page: only the newest matches are sorted and shown.
    let mut found: Vec<&(Arc<String>, Entry)> = all.iter().filter(|(key, entry)| matches(key, entry)).collect();
    let matching = found.len();
    let newest_first = |(_, entry): &&(Arc<String>, Entry)| std::cmp::Reverse(entry.stored_at);
    if found.len() > CACHE_ROWS {
        found.select_nth_unstable_by_key(CACHE_ROWS, newest_first);
        found.truncate(CACHE_ROWS);
    }
    found.sort_unstable_by_key(newest_first);
    let rows: Vec<CacheRow> = found.into_iter().map(|(key, entry)| cache_row(key, entry)).collect();
    CachePage {
        shell,
        s,
        posts: all.len() - failed,
        failed,
        size: size(bytes),
        q: q.trim().to_string(),
        shown: rows.len(),
        matching,
        rows,
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct CacheQuery {
    #[serde(default)]
    q: String,
    done: Option<String>,
}

pub async fn cache(State(d): State<Shared>, Query(q): Query<CacheQuery>) -> Response {
    let shell = Shell::new("cache", &Done { done: q.done.clone() });
    render(&cache_page(&d.app, shell, (*d.app.settings.get()).clone(), &q.q).await)
}

#[derive(Deserialize)]
pub struct CacheForm {
    cache_minutes: u64,
    failure_cache_minutes: u64,
    cache_megabytes: u64,
}

pub async fn save_cache(State(d): State<Shared>, Form(f): Form<CacheForm>) -> Response {
    let mut s = (*d.app.settings.get()).clone();
    s.cache_minutes = f.cache_minutes;
    s.failure_cache_minutes = f.failure_cache_minutes;
    s.cache_megabytes = f.cache_megabytes;
    match d.app.settings.set(s.clone()).await {
        Ok(()) => done("/cache", "saved"),
        Err(e) => refused(&cache_page(&d.app, Shell::refused("cache", format!("{e:#}")), s, "").await),
    }
}

/// A link to forget, or a cache key from the list.
#[derive(Deserialize)]
pub struct LinkForm {
    #[serde(default)]
    url: String,
    #[serde(default)]
    key: String,
}

pub async fn cache_remove(State(d): State<Shared>, Form(f): Form<LinkForm>) -> Response {
    if !f.key.is_empty() {
        d.app.cache.remove(&f.key).await;
        return done("/cache", "forgotten");
    }
    let Some(target) = Target::from_link(f.url.trim()) else {
        let shell = Shell::refused("cache", "That isn't a facebook.com link.");
        return refused(&cache_page(&d.app, shell, (*d.app.settings.get()).clone(), "").await);
    };
    d.app.cache.remove(target.key()).await;
    done("/cache", "forgotten")
}

pub async fn cache_clear(State(d): State<Shared>) -> Response {
    d.app.cache.clear();
    done("/cache", "cleared")
}

// ---- Import and export ----

#[derive(Template)]
#[template(path = "dashboard/backup.html")]
pub struct BackupPage {
    shell: Shell,
    /// What was pasted, when an import was refused.
    pasted: String,
}

pub async fn backup(Query(q): Query<Done>) -> Response {
    render(&BackupPage { shell: Shell::new("backup", &q), pasted: String::new() })
}

/// The settings as a JSON file.
pub async fn export_settings(State(d): State<Shared>) -> Response {
    match serde_json::to_string_pretty(&*d.app.settings.get()) {
        Ok(json) => (
            [(CONTENT_TYPE, "application/json")],
            [(CONTENT_DISPOSITION, "attachment; filename=\"settings.json\"")],
            json + "\n",
        )
            .into_response(),
        Err(e) => server_error(&e.into()),
    }
}

#[derive(Deserialize)]
pub struct ImportForm {
    settings: String,
}

/// Replaces the settings with a file's, checked like the settings pages check them.
pub async fn import_settings(State(d): State<Shared>, Form(f): Form<ImportForm>) -> Response {
    let saved = match serde_json::from_str::<Settings>(&f.settings) {
        Ok(settings) => d.app.settings.set(settings).await,
        Err(e) => Err(anyhow::anyhow!("That isn't a settings file from this dashboard ({e}). Nothing changed.")),
    };
    match saved {
        Ok(()) => done("/backup", "imported"),
        Err(e) => refused(&BackupPage { shell: Shell::refused("backup", format!("{e:#}")), pasted: f.settings }),
    }
}

// ---- Test links and alerts ----

#[derive(Template)]
#[template(path = "dashboard/tests.html")]
pub struct TestsPage {
    shell: Shell,
    links: String,
    alert_chat: String,
    has_token: bool,
    results: Vec<(String, TestResult)>,
}

async fn tests_page(app: &App, shell: Shell, links: String, alert_chat: String) -> Result<TestsPage> {
    let results = canary::results(app).await?.into_iter().map(|r| (ago(r.checked_at), r)).collect();
    Ok(TestsPage { shell, links, alert_chat, has_token: app.config.telegram_bot_token.is_some(), results })
}

pub async fn tests(State(d): State<Shared>, Query(q): Query<Done>) -> Response {
    let s = d.app.settings.get();
    let alert_chat = s.alert_chat.map(|c| c.to_string()).unwrap_or_default();
    show(tests_page(&d.app, Shell::new("tests", &q), s.test_links.join("\n"), alert_chat).await)
}

#[derive(Deserialize)]
pub struct TestsForm {
    links: String,
    alert_chat: String,
}

pub async fn save_tests(State(d): State<Shared>, Form(f): Form<TestsForm>) -> Response {
    let mut s = (*d.app.settings.get()).clone();
    s.test_links = f.links.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect();
    let chat = f.alert_chat.trim();
    let saved = match chat.parse::<i64>() {
        _ if chat.is_empty() => {
            s.alert_chat = None;
            d.app.settings.set(s).await
        }
        Ok(chat) => {
            s.alert_chat = Some(chat);
            d.app.settings.set(s).await
        }
        Err(_) => Err(anyhow::anyhow!("the alert chat ID is a number, like 123456789 or -1001234567890")),
    };
    match saved {
        Ok(()) => done("/tests", "saved"),
        Err(e) => match tests_page(&d.app, Shell::refused("tests", format!("{e:#}")), f.links, f.alert_chat).await {
            Ok(page) => refused(&page),
            Err(e) => server_error(&e),
        },
    }
}

pub async fn run_tests(State(d): State<Shared>) -> Response {
    match d.app.canary.run(&d.app).await {
        Ok(_) => done("/tests", "ran"),
        Err(e) => server_error(&e),
    }
}

/// Sends a message to the alert chat, so the chat ID and token can be checked before something
/// goes wrong.
pub async fn test_alert(State(d): State<Shared>) -> Response {
    let s = d.app.settings.get();
    let sent = match (&d.app.config.telegram_bot_token, s.alert_chat) {
        (None, _) => Err(anyhow::anyhow!("Set TELEGRAM_BOT_TOKEN in .env and restart the server to send alerts.")),
        (_, None) => Err(anyhow::anyhow!("Save an alert chat ID first.")),
        _ => crate::alerts::send(&d.app, "Test alert from the Homura dashboard: alerts reach this chat.").await,
    };
    match sent {
        Ok(()) => done("/tests", "alert_sent"),
        Err(e) => {
            let alert_chat = s.alert_chat.map(|c| c.to_string()).unwrap_or_default();
            let shell = Shell::refused("tests", format!("No alert sent: {e:#}"));
            match tests_page(&d.app, shell, s.test_links.join("\n"), alert_chat).await {
                Ok(page) => refused(&page),
                Err(e) => server_error(&e),
            }
        }
    }
}

// ---- Security ----

struct PasskeyRow {
    id: String,
    name: String,
    added: String,
    used: String,
}

struct SessionView {
    /// The session's hash in hex, which names it in the sign-out link; the token can't be had from it.
    id: String,
    method: String,
    current: bool,
    since: String,
    seen: String,
    ip: String,
    agent: String,
}

/// An authenticator app being set up: what it scans, opens or has typed in.
struct TotpSetup {
    qr: String,
    size: u32,
    link: String,
    key: String,
    grouped: String,
}

#[derive(Template)]
#[template(path = "dashboard/security.html")]
pub struct SecurityPage {
    shell: Shell,
    password: bool,
    totp_since: Option<String>,
    totp_setup: Option<TotpSetup>,
    /// Why the code didn't turn the app on, shown by its field.
    totp_error: Option<String>,
    passkeys_enabled: bool,
    passkeys: Vec<PasskeyRow>,
    access: String,
    sessions: Vec<SessionView>,
}

async fn security_page(d: &super::Dashboard, shell: Shell, identity: &Identity) -> Result<SecurityPage> {
    let db = &d.app.db;
    let passkeys = auth::passkey::list(db)
        .await?
        .into_iter()
        .map(|p| PasskeyRow {
            id: data_encoding::HEXLOWER.encode(&p.id),
            name: p.name,
            added: day(p.created_at),
            used: p.last_used_at.map_or_else(|| "never".into(), ago),
        })
        .collect();
    let current = match identity {
        Identity::Session { hash, .. } => Some(hash.as_slice()),
        Identity::Access { .. } => None,
    };
    let sessions = auth::sessions(db)
        .await?
        .into_iter()
        .map(|s| SessionView {
            id: data_encoding::HEXLOWER.encode(&s.hash),
            current: current == Some(s.hash.as_slice()),
            method: s.method,
            since: when(s.created_at),
            seen: ago(s.last_seen_at),
            ip: s.ip,
            agent: browser(&s.user_agent),
        })
        .collect();
    let access = match (&d.auth.access, identity) {
        (Some(access), Identity::Access { email }) => {
            format!("On for {}. You're signed in through it as {email}.", access.team_domain())
        }
        (Some(access), Identity::Session { .. }) => {
            format!("On for {}. This visit used a dashboard session instead.", access.team_domain())
        }
        (None, _) => "Off. Set ACCESS_TEAM and ACCESS_AUD to accept Cloudflare Access sign-ins.".into(),
    };
    let totp_since = auth::totp::enabled_at(db).await?.map(day);
    let totp_setup = match d.auth.totp_setup.current() {
        Some(secret) if totp_since.is_none() => {
            let account = d.app.config.dashboard_url.as_ref().and_then(|u| u.host_str()).unwrap_or("dashboard");
            let link = auth::totp::link(&secret, account);
            let (qr, size) = auth::totp::qr(&link)?;
            let key = auth::totp::key(&secret);
            Some(TotpSetup { qr, size, link, grouped: auth::totp::grouped(&key), key })
        }
        _ => None,
    };
    Ok(SecurityPage {
        shell: Shell { setup: identity.is_setup(), ..shell },
        password: auth::password::is_set(db).await?,
        totp_since,
        totp_setup,
        totp_error: None,
        passkeys_enabled: d.auth.passkeys.is_some(),
        passkeys,
        access,
        sessions,
    })
}

pub async fn security(
    State(d): State<Shared>,
    Extension(identity): Extension<Identity>,
    Query(q): Query<Done>,
) -> Response {
    show(security_page(&d, Shell::new("security", &q), &identity).await)
}

async fn security_refused(d: &super::Dashboard, identity: &Identity, error: impl Into<String>) -> Response {
    match security_page(d, Shell::refused("security", error), identity).await {
        Ok(page) => refused(&page),
        Err(e) => server_error(&e),
    }
}

const LAST_LOGIN: &str =
    "That's the last way to sign in. Add another first (if you're locked out, `server setup-link` gets you back in).";

/// Whether a way to sign in other than the password is set up: a passkey, or Access.
async fn besides_password(d: &super::Dashboard) -> Result<bool> {
    let passkeys = d.auth.passkeys.is_some() && auth::passkey::count(&d.app.db).await? > 0;
    Ok(passkeys || d.auth.access.is_some())
}

/// Removing the password is refused when it's the last way in.
async fn password_needed(d: &super::Dashboard) -> Result<bool> {
    Ok(auth::password::is_set(&d.app.db).await? && !besides_password(d).await?)
}

pub async fn set_password(
    State(d): State<Shared>,
    Extension(identity): Extension<Identity>,
    Form(f): Form<super::PasswordForm>,
) -> Response {
    let current = match &identity {
        Identity::Session { hash, .. } => Some(hash.clone()),
        Identity::Access { .. } => None,
    };
    let saved = async {
        auth::password::set(&d.app.db, &f.password).await?;
        auth::end_other_sessions(&d.app.db, current).await
    };
    match saved.await {
        Ok(()) => done("/security", "password"),
        Err(e) => security_refused(&d, &identity, format!("{e:#}")).await,
    }
}

pub async fn remove_password(State(d): State<Shared>, Extension(identity): Extension<Identity>) -> Response {
    match password_needed(&d).await {
        Ok(true) => return security_refused(&d, &identity, LAST_LOGIN).await,
        Ok(false) => {}
        Err(e) => return server_error(&e),
    }
    // The authenticator app goes too: it's only ever asked for with the password.
    let removed = async {
        auth::password::remove(&d.app.db).await?;
        auth::totp::disable(&d.app.db).await
    };
    match removed.await {
        Ok(()) => done("/security", "password_removed"),
        Err(e) => server_error(&e),
    }
}

pub async fn remove_passkey(
    State(d): State<Shared>,
    Extension(identity): Extension<Identity>,
    Path(id): Path<String>,
) -> Response {
    let Ok(id) = data_encoding::HEXLOWER_PERMISSIVE.decode(id.as_bytes()) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let others = async {
        let password = auth::password::is_set(&d.app.db).await?;
        anyhow::Ok(password || auth::passkey::count(&d.app.db).await? > 1 || d.auth.access.is_some())
    };
    match others.await {
        Ok(false) => security_refused(&d, &identity, LAST_LOGIN).await,
        Ok(true) => match auth::passkey::remove(&d.app.db, id).await {
            Ok(()) => done("/security", "passkey_removed"),
            Err(e) => server_error(&e),
        },
        Err(e) => server_error(&e),
    }
}

pub async fn totp_start(State(d): State<Shared>) -> Response {
    d.auth.totp_setup.start();
    Redirect::to("/security#totp").into_response()
}

#[derive(Deserialize)]
pub struct CodeForm {
    code: String,
}

pub async fn totp_confirm(
    State(d): State<Shared>,
    Extension(identity): Extension<Identity>,
    Form(f): Form<CodeForm>,
) -> Response {
    let Some(secret) = d.auth.totp_setup.current() else {
        return totp_refused(&d, &identity, "That setup expired; start again.").await;
    };
    match auth::totp::enable(&d.app.db, &secret, &f.code).await {
        Ok(true) => {
            d.auth.totp_setup.cancel();
            done("/security", "totp")
        }
        Ok(false) => {
            let error = "That code didn't match. Use the newest one, and check that the phone's clock is right.";
            totp_refused(&d, &identity, error).await
        }
        Err(e) => server_error(&e),
    }
}

async fn totp_refused(d: &super::Dashboard, identity: &Identity, error: &str) -> Response {
    match security_page(d, Shell::new("security", &Done::default()), identity).await {
        Ok(page) => refused(&SecurityPage { totp_error: Some(error.into()), ..page }),
        Err(e) => server_error(&e),
    }
}

pub async fn totp_cancel(State(d): State<Shared>) -> Response {
    d.auth.totp_setup.cancel();
    Redirect::to("/security").into_response()
}

pub async fn totp_remove(State(d): State<Shared>) -> Response {
    match auth::totp::disable(&d.app.db).await {
        Ok(()) => done("/security", "totp_removed"),
        Err(e) => server_error(&e),
    }
}

#[derive(Deserialize)]
pub struct NewPasskey {
    #[serde(default)]
    name: String,
}

pub async fn passkey_register_start(State(d): State<Shared>, Json(body): Json<NewPasskey>) -> Response {
    let Some(passkeys) = &d.auth.passkeys else {
        return json_error(StatusCode::NOT_FOUND, "passkeys need DASHBOARD_URL");
    };
    match passkeys.start_registration(&d.app.db, &body.name).await {
        Ok((id, options)) => Json(json!({ "id": id, "options": options })).into_response(),
        Err(e) => json_error(StatusCode::BAD_REQUEST, &e.to_string()),
    }
}

#[derive(Deserialize)]
pub struct PasskeyRegistration {
    id: String,
    credential: RegisterPublicKeyCredential,
}

pub async fn passkey_register_finish(State(d): State<Shared>, Json(body): Json<PasskeyRegistration>) -> Response {
    let Some(passkeys) = &d.auth.passkeys else {
        return json_error(StatusCode::NOT_FOUND, "passkeys need DASHBOARD_URL");
    };
    match passkeys.finish_registration(&d.app.db, &body.id, &body.credential).await {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => json_error(StatusCode::BAD_REQUEST, &e.to_string()),
    }
}

/// Signs one browser out. Ending this browser's own session is signing out.
pub async fn end_session(
    State(d): State<Shared>,
    Extension(identity): Extension<Identity>,
    Path(id): Path<String>,
) -> Response {
    let Ok(hash) = data_encoding::HEXLOWER_PERMISSIVE.decode(id.as_bytes()) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let own = matches!(&identity, Identity::Session { hash: current, .. } if *current == hash);
    match auth::end_session(&d.app.db, hash).await {
        Ok(()) if own => with_cookie(Redirect::to("/login").into_response(), auth::clear_cookie()),
        Ok(()) => done("/security", "session_ended"),
        Err(e) => server_error(&e),
    }
}

/// Ends every session, this one included. An Access sign-in carries on.
pub async fn clear_sessions(State(d): State<Shared>) -> Response {
    match auth::end_all_sessions(&d.app.db).await {
        Ok(()) => with_cookie(Redirect::to("/login").into_response(), auth::clear_cookie()),
        Err(e) => server_error(&e),
    }
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
        assert_eq!(when(0), "1970-01-01 00:00 UTC");
        assert_eq!(
            [success_level(0, 0), success_level(95, 100), success_level(80, 100), success_level(1, 2)],
            [IDLE, OK, WARN, BAD]
        );
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

    #[test]
    fn counts_split_outcomes() {
        let mut c = Counts::default();
        c.add("hit", Agg { count: 3, total_ms: 3, max_ms: 2 });
        c.add("extracted", Agg { count: 1, total_ms: 900, max_ms: 900 });
        c.add("failed:login_wall", Agg { count: 1, total_ms: 1200, max_ms: 1200 });
        c.add("redirect", Agg { count: 5, total_ms: 5, max_ms: 1 });
        c.add("rate_limited", Agg { count: 2, total_ms: 0, max_ms: 0 });
        assert_eq!((c.all, c.telegram, c.hits, c.success, c.redirects), (12, 5, 3, 4, 5));
        assert_eq!((c.extracted.count, c.extracted.total_ms, c.slowest), (1, 900, 1200));
    }
}
