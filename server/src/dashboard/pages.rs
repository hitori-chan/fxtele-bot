//! The dashboard's pages. A change redirects back with `?done=<what>` for the notice; a refused
//! change shows the page again, with what was entered, the error and status 400.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::{Extension, Form, Json};
use ipnet::IpNet;
use serde::Deserialize;
use serde_json::json;
use webauthn_rs::prelude::RegisterPublicKeyCredential;

use super::auth::{self, Identity};
use super::{Shared, json_error, render, server_error, with_cookie};
use crate::app::{App, Busy};
use crate::cache::Entry;
use crate::canary::{self, TestResult};
use crate::cookies::{self, parse_export};
use crate::extract::facebook::url::{PageType, Target};
use crate::extract::{CookieChoice, ExtractError, Post, Stage, Timings};
use crate::render::{self as preview, MediaUrl};
use crate::settings::{CookiePolicy, FailureResponse, MediaLinks, Settings};
use crate::stats::{Agg, Failure};

// ---- The frame around every page ----

const LINKS: [(&str, &str, &str); 9] = [
    ("/", "Overview", "overview"),
    ("/playground", "Playground", "playground"),
    ("/failures", "Failures", "failures"),
    ("/facebook", "Facebook", "facebook"),
    ("/limits", "Rate limits", "limits"),
    ("/cache", "Cache", "cache"),
    ("/tests", "Test links", "tests"),
    ("/telegram", "Telegram IPs", "telegram"),
    ("/security", "Login", "security"),
];

pub struct Shell {
    nav: &'static str,
    notice: Option<&'static str>,
    error: Option<String>,
}

impl Shell {
    fn new(nav: &'static str, done: &Done) -> Self {
        Self { nav, notice: done.done.as_deref().and_then(notice), error: None }
    }

    fn refused(nav: &'static str, error: impl Into<String>) -> Self {
        Self { nav, notice: None, error: Some(error.into()) }
    }

    pub const fn links(&self) -> [(&'static str, &'static str, &'static str); 9] {
        LINKS
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
        "password" => "Password saved.",
        "password_removed" => "Password removed.",
        "totp" => "Authenticator app on: signing in with the password now also asks for a code.",
        "totp_removed" => "Authenticator app off.",
        "passkey_removed" => "Passkey removed.",
        "welcome" => "Signed in with the setup link, which no longer works. Set a password or add a passkey now.",
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
        s @ 0..60 => format!("{s} s ago"),
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

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok()).collect()
}

fn checked(field: Option<&String>) -> bool {
    field.is_some()
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
}

#[derive(Template)]
#[template(path = "dashboard/overview.html")]
pub struct Overview {
    shell: Shell,
    proxy_warning: bool,
    hour_total: u64,
    day_telegram: u64,
    day_success: String,
    cache_entries: u64,
    cache_size: String,
    day_hit_rate: String,
    budget_in_use: u32,
    budget_exhausted: u64,
    cookie_state: String,
    cookie_detail: String,
    tests_state: String,
    tests_detail: String,
    rows: Vec<Row>,
}

fn cookie_state(status: &cookies::Status) -> (String, String) {
    let (state, detail) = if status.count == 0 {
        ("None", "public posts only".to_string())
    } else if let Some(at) = status.rejected_at {
        ("Rejected", format!("by Facebook, {}", ago(at)))
    } else if !status.has_session {
        ("Incomplete", "c_user or xs is missing".to_string())
    } else if let Some(expires) = status.expires_at {
        ("OK", format!("expire {}", day(expires)))
    } else {
        ("OK", "no expiry date".to_string())
    };
    (state.to_string(), detail)
}

async fn tests_state(app: &App, settings: &Settings) -> Result<(String, String)> {
    let results = canary::results(app).await?;
    Ok(if settings.test_links.is_empty() {
        ("None".into(), "add some under Test links".into())
    } else if results.is_empty() {
        ("Not run".into(), "the first check runs soon".into())
    } else {
        let ok = results.iter().filter(|r| r.ok).count();
        let last = results.iter().map(|r| r.checked_at).max().unwrap_or_default();
        (format!("{ok}/{} OK", results.len()), format!("checked {}", ago(last)))
    })
}

pub async fn overview(State(d): State<Shared>, Query(q): Query<Done>) -> Response {
    show(overview_page(&d.app, Shell::new("overview", &q)).await)
}

async fn overview_page(app: &App, shell: Shell) -> Result<Overview> {
    let settings = app.settings.get();
    let (hour, _) = counts(app, 3600).await?;
    let (day, by_type) = counts(app, 86_400).await?;
    let (cache_entries, cache_bytes) = app.cache.usage().await;
    let (cookie_state, cookie_detail) = cookie_state(&app.cookies.status());
    let (tests_state, tests_detail) = tests_state(app, &settings).await?;
    let rows = by_type
        .into_iter()
        .filter(|(page_type, c)| page_type != "-" && (c.telegram > 0 || c.redirects > 0))
        .map(|(page_type, c)| Row {
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
        hour_total: hour.all,
        day_telegram: day.telegram,
        day_success: percent(day.success, day.telegram),
        cache_entries,
        cache_size: size(cache_bytes),
        day_hit_rate: percent(day.hits, day.telegram),
        budget_in_use: app.budget.in_use(),
        budget_exhausted: app.budget.exhausted(),
        cookie_state,
        cookie_detail,
        tests_state,
        tests_detail,
        rows,
    })
}

// ---- Facebook: fetching, cache, previews, cookies ----

#[derive(Template)]
#[template(path = "dashboard/facebook.html")]
pub struct FacebookPage {
    shell: Shell,
    s: Settings,
    policy: &'static str,
    failure: &'static str,
    media_links: &'static str,
    cookie_summary: String,
    has_cookies: bool,
}

fn facebook_page(app: &App, shell: Shell, s: Settings) -> FacebookPage {
    let status = app.cookies.status();
    let cookie_summary = if status.count == 0 {
        "No cookies saved, so only public posts work.".to_string()
    } else {
        let saved = status.saved_at.map(|at| format!(", saved {}", when(at))).unwrap_or_default();
        let state = if let Some(at) = status.rejected_at {
            format!(" Facebook rejected them {}; paste fresh ones.", ago(at))
        } else if !status.has_session {
            " c_user or xs is missing, so they aren't used.".to_string()
        } else if let Some(expires) = status.expires_at {
            format!(" The session expires {}.", day(expires))
        } else {
            String::new()
        };
        format!("{} cookies{saved}.{state}", status.count)
    };
    FacebookPage {
        policy: match s.cookie_policy {
            CookiePolicy::Fallback => "fallback",
            CookiePolicy::Always => "always",
            CookiePolicy::Never => "never",
        },
        failure: match s.failure_response {
            FailureResponse::Redirect => "redirect",
            FailureResponse::Page => "page",
            FailureResponse::Unavailable => "unavailable",
        },
        media_links: match s.media_links {
            MediaLinks::Direct => "direct",
            MediaLinks::Redirect => "redirect",
        },
        has_cookies: status.count > 0,
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
    cache_minutes: u64,
    failure_cache_minutes: u64,
    cache_megabytes: u64,
    site_name: String,
    description_length: usize,
    failure_response: FailureResponse,
    media_links: MediaLinks,
    iv_video: Option<String>,
    iv_photo: Option<String>,
    iv_album: Option<String>,
    iv_text: Option<String>,
    iv_profile: Option<String>,
}

pub async fn save_facebook(State(d): State<Shared>, Form(f): Form<FacebookForm>) -> Response {
    let mut s = (*d.app.settings.get()).clone();
    let proxy = f.proxy.trim();
    s.proxy = (!proxy.is_empty()).then(|| proxy.to_string());
    s.cookie_policy = f.cookie_policy;
    s.extract_timeout_seconds = f.extract_timeout_seconds;
    s.cache_minutes = f.cache_minutes;
    s.failure_cache_minutes = f.failure_cache_minutes;
    s.cache_megabytes = f.cache_megabytes;
    s.site_name = f.site_name.trim().to_string();
    s.description_length = f.description_length;
    s.failure_response = f.failure_response;
    s.media_links = f.media_links;
    s.instant_view.video = checked(f.iv_video.as_ref());
    s.instant_view.photo = checked(f.iv_photo.as_ref());
    s.instant_view.album = checked(f.iv_album.as_ref());
    s.instant_view.text = checked(f.iv_text.as_ref());
    s.instant_view.profile = checked(f.iv_profile.as_ref());
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
        Ok(()) => done("/facebook", "cookies"),
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
    let (fresh, cookies) = (checked(f.fresh.as_ref()), checked(f.cookies.as_ref()));
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
                    let ttl = Duration::from_secs(settings.cache_minutes * 60);
                    app.cache
                        .insert(settings.cache_megabytes, target.key().to_string(), Entry::post(post.clone(), ttl))
                        .await;
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
            let media: Vec<MediaUrl> =
                post.media.iter().map(|m| MediaUrl { src: m.url.clone(), poster: m.thumbnail.clone() }).collect();
            let summary = preview::summary(&post, &media, &settings);
            tried.html = preview::preview(&post, &media, &settings, target.original.as_str());
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
            stages: [Stage::Fetch, Stage::Redirect, Stage::LoginWall, Stage::NotFound, Stage::Timeout]
                .iter()
                .map(|s| s.as_str())
                .collect(),
            page_type: q.page_type.clone(),
            stage: q.stage.clone(),
            failures,
        })
    };
    show(page.await)
}

// ---- Cache ----

#[derive(Template)]
#[template(path = "dashboard/cache.html")]
pub struct CachePage {
    shell: Shell,
    entries: u64,
    size: String,
    limit: String,
}

async fn cache_page(app: &App, shell: Shell) -> CachePage {
    let (entries, bytes) = app.cache.usage().await;
    CachePage { shell, entries, size: size(bytes), limit: format!("{} MB", app.settings.get().cache_megabytes) }
}

pub async fn cache(State(d): State<Shared>, Query(q): Query<Done>) -> Response {
    render(&cache_page(&d.app, Shell::new("cache", &q)).await)
}

#[derive(Deserialize)]
pub struct LinkForm {
    url: String,
}

pub async fn cache_remove(State(d): State<Shared>, Form(f): Form<LinkForm>) -> Response {
    let Some(target) = Target::from_link(f.url.trim()) else {
        return refused(&cache_page(&d.app, Shell::refused("cache", "That isn't a facebook.com link.")).await);
    };
    d.app.cache.remove(target.key()).await;
    done("/cache", "forgotten")
}

pub async fn cache_clear(State(d): State<Shared>) -> Response {
    d.app.cache.clear();
    done("/cache", "cleared")
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

// ---- Login settings ----

struct PasskeyRow {
    id: String,
    name: String,
    added: String,
    used: String,
}

struct SessionView {
    method: String,
    current: bool,
    since: String,
    seen: String,
    ip: String,
    agent: String,
}

/// An authenticator app being set up: what it scans or has typed in.
struct TotpSetup {
    qr: String,
    key: String,
}

#[derive(Template)]
#[template(path = "dashboard/security.html")]
pub struct SecurityPage {
    shell: Shell,
    password: bool,
    totp_since: Option<String>,
    totp_setup: Option<TotpSetup>,
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
            id: hex(&p.id),
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
            current: current == Some(s.hash.as_slice()),
            method: s.method,
            since: when(s.created_at),
            seen: ago(s.last_seen_at),
            ip: s.ip,
            agent: s.user_agent,
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
            Some(TotpSetup { qr: auth::totp::qr(&secret, account)?, key: auth::totp::key(&secret) })
        }
        _ => None,
    };
    Ok(SecurityPage {
        shell,
        password: auth::password::is_set(db).await?,
        totp_since,
        totp_setup,
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

pub async fn set_password(
    State(d): State<Shared>,
    Extension(identity): Extension<Identity>,
    Form(f): Form<super::PasswordForm>,
) -> Response {
    match auth::password::set(&d.app.db, &f.password).await {
        Ok(()) => done("/security", "password"),
        Err(e) => security_refused(&d, &identity, format!("{e:#}")).await,
    }
}

pub async fn remove_password(State(d): State<Shared>, Extension(identity): Extension<Identity>) -> Response {
    let passkeys = if d.auth.passkeys.is_some() { auth::passkey::count(&d.app.db).await.unwrap_or(0) } else { 0 };
    if passkeys == 0 && d.auth.access.is_none() {
        return security_refused(&d, &identity, LAST_LOGIN).await;
    }
    match auth::password::remove(&d.app.db).await {
        Ok(()) => done("/security", "password_removed"),
        Err(e) => server_error(&e),
    }
}

pub async fn remove_passkey(
    State(d): State<Shared>,
    Extension(identity): Extension<Identity>,
    Path(id): Path<String>,
) -> Response {
    let Some(id) = unhex(&id) else { return StatusCode::NOT_FOUND.into_response() };
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
        return security_refused(&d, &identity, "That setup expired; start again.").await;
    };
    match auth::totp::enable(&d.app.db, &secret, &f.code).await {
        Ok(true) => {
            d.auth.totp_setup.cancel();
            done("/security", "totp")
        }
        Ok(false) => {
            let error = "That code didn't match. Use the newest one, and check that the phone's clock is right.";
            security_refused(&d, &identity, error).await
        }
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
    fn hex_round_trip() {
        assert_eq!(hex(&[0, 15, 255]), "000fff");
        assert_eq!(unhex("000fff"), Some(vec![0, 15, 255]));
        assert_eq!(unhex("0f0"), None);
        assert_eq!(unhex("zz"), None);
    }

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
