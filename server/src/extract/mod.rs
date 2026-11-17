//! What an extractor produces, the sources it can read, and the trait in front of them.

pub mod facebook;
pub mod instagram;
pub mod page;
pub mod reddit;
pub mod threads;

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde::Serialize;
use url::Url;

use crate::settings::Settings;

/// A site posts come from. Each has its own preview hostname, settings, cookies and fetch budget,
/// and its own prefix in the video bucket (`facebook/…`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Facebook,
    Instagram,
    Reddit,
    Threads,
}

impl Source {
    pub const ALL: [Self; 4] = [Self::Facebook, Self::Instagram, Self::Reddit, Self::Threads];

    /// Its name in settings, statistics and the bucket's file names.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Facebook => "facebook",
            Self::Instagram => "instagram",
            Self::Reddit => "reddit",
            Self::Threads => "threads",
        }
    }

    /// Its name as people know it.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Facebook => "Facebook",
            Self::Instagram => "Instagram",
            Self::Reddit => "Reddit",
            Self::Threads => "Threads",
        }
    }

    /// Its own domain, which its links and cookies are on.
    pub const fn domain(self) -> &'static str {
        match self {
            Self::Facebook => "facebook.com",
            Self::Instagram => "instagram.com",
            Self::Reddit => "reddit.com",
            Self::Threads => "threads.com",
        }
    }

    /// The variable naming its preview host. Facebook has none: it's every host no other site
    /// has.
    pub const fn host_variable(self) -> Option<&'static str> {
        match self {
            Self::Facebook => None,
            Self::Instagram => Some("INSTAGRAM_HOST"),
            Self::Reddit => Some("REDDIT_HOST"),
            Self::Threads => Some("THREADS_HOST"),
        }
    }

    /// Every site's name, for messages: "Facebook, Instagram, Reddit or Threads".
    pub fn listed() -> String {
        let names = Self::ALL.map(Self::name);
        match names.split_last() {
            Some((last, [])) => (*last).to_string(),
            Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
            None => String::new(),
        }
    }

    /// Its place in [`Self::ALL`], for things kept per site.
    pub const fn index(self) -> usize {
        self as usize
    }

    /// The source named `name`, as [`Self::as_str`] spells it.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|source| source.as_str() == name)
    }

    /// The site an http(s) link points at, by its host or a subdomain of it.
    pub fn of_url(url: &Url) -> Option<Self> {
        if !matches!(url.scheme(), "http" | "https") {
            return None;
        }
        // `url` lower-cases http(s) hosts already.
        let host = url.host_str()?.trim_end_matches('.');
        let on = |domain: &str| host == domain || host.strip_suffix(domain).is_some_and(|sub| sub.ends_with('.'));
        if on("facebook.com") {
            Some(Self::Facebook)
        } else if on("instagram.com") || on("instagr.am") {
            Some(Self::Instagram)
        } else if on("reddit.com") || on("redd.it") {
            Some(Self::Reddit)
        } else if on("threads.com") || on("threads.net") {
            Some(Self::Threads)
        } else {
            None
        }
    }

    /// The source of a cache key or another link (`https://www.facebook.com/reel/1`).
    pub fn of_link(link: &str) -> Option<Self> {
        Self::of_url(&Url::parse(link.trim()).ok()?)
    }
}

/// What a link points at, in one vocabulary for every source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PageType {
    /// A post in a feed: Facebook's `/{page}/posts/…` and share links, Instagram's `/p/…`.
    Post,
    /// A short vertical video: `/reel/…`.
    Reel,
    /// Facebook's `/{page}/videos/…`.
    Video,
    /// Facebook's `/watch?v=…`.
    Watch,
    /// Facebook's photo pages.
    Photo,
    /// Facebook's photo albums: `/media/set/?set=a.…`.
    Album,
    /// A story, gone after a day: `/stories/…`.
    Story,
    /// A page or a person.
    Profile,
    /// Any other page (Facebook's events, groups...): previewed from its meta tags.
    Other,
}

impl PageType {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Post => "post",
            Self::Reel => "reel",
            Self::Video => "video",
            Self::Watch => "watch",
            Self::Photo => "photo",
            Self::Album => "album",
            Self::Story => "story",
            Self::Profile => "profile",
            Self::Other => "other",
        }
    }
}

/// A page to preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub source: Source,
    /// Where people are redirected: the request's path and query on the source's site.
    pub original: Url,
    /// Normalized copy for fetching and the cache key.
    pub url: Url,
    pub page_type: PageType,
}

impl Target {
    /// Maps a request's path and query (`/reel/1?s=x`) onto `source`'s site. The origin is fixed
    /// there, so the result can only ever point at that site.
    pub fn from_request(source: Source, path_and_query: &str) -> Option<Self> {
        match source {
            Source::Facebook => facebook::url::request(path_and_query),
            Source::Instagram => instagram::url::request(path_and_query),
            Source::Reddit => reddit::url::request(path_and_query),
            Source::Threads => threads::url::request(path_and_query),
        }
    }

    /// A full link to any source (playground, test links, the command line).
    pub fn from_link(link: &str) -> Option<Self> {
        let url = Url::parse(link.trim()).ok()?;
        let source = Source::of_url(&url)?;
        let path_and_query = match url.query() {
            Some(query) => format!("{}?{query}", url.path()),
            None => url.path().to_string(),
        };
        Self::from_request(source, &path_and_query)
    }

    pub fn key(&self) -> &str {
        self.url.as_str()
    }

    /// Which of a post's items the link asks the card to show, from 0: `img_index`, which counts
    /// from 1 (Instagram's carousel links). Not part of the key: every item shares one post.
    pub fn item_index(&self) -> Option<usize> {
        let value = self.original.query_pairs().find(|(k, _)| k == "img_index")?.1;
        value.parse::<usize>().ok()?.checked_sub(1)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Post {
    /// The normalized URL the post was requested by; the cache key.
    pub key: String,
    /// Canonical original URL (may be a group permalink).
    pub url: String,
    pub page_type: PageType,
    /// Page, author or group name, or the route title.
    pub title: Option<String>,
    /// Page or person who posted, when known.
    pub author: Option<String>,
    /// Where to find the author, and their picture.
    pub profile: AuthorProfile,
    /// The group it was posted in.
    pub group: Option<String>,
    /// Post time (Unix seconds), when found.
    pub published_at: Option<i64>,
    /// Caption or post text.
    pub text: Option<String>,
    pub media: Vec<Media>,
    pub kind: PostKind,
    pub stats: Stats,
    /// The post this one shares, when it's a share; its media are `media`, as a share has none
    /// of its own.
    pub shared: Option<Shared>,
}

/// A shared post, as the share shows it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Shared {
    /// The shared post's own page.
    pub url: Option<String>,
    /// Its headline, where posts have one (Reddit's) and it isn't the share's own.
    pub title: Option<String>,
    pub author: Option<String>,
    pub profile: AuthorProfile,
    pub published_at: Option<i64>,
    pub text: Option<String>,
    pub relation: Relation,
}

/// What a post does with the post it carries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Relation {
    /// Shares it: Facebook's shares, Reddit's crossposts.
    #[default]
    Share,
    /// Answers it: a Reddit comment, a Threads reply.
    Reply,
    /// Quotes it, with something to say of its own: Threads' quotes.
    Quote,
}

/// The author beyond their name.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AuthorProfile {
    /// Their profile page.
    pub url: Option<String>,
    /// Their @name, where the site has one (Instagram).
    pub handle: Option<String>,
    /// The small picture from the post's page (Facebook's is 40 px).
    pub avatar: Option<String>,
    /// The full-size picture, from their profile page.
    pub picture: Option<String>,
    /// Their bio, from their profile page.
    pub bio: Option<String>,
}

impl AuthorProfile {
    /// The best picture there is.
    pub fn best_picture(&self) -> Option<&str> {
        self.picture.as_deref().or(self.avatar.as_deref())
    }
}

/// Counts as the site showed them when the post was fetched.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Stats {
    /// Reactions on Facebook, likes on Instagram.
    pub reactions: Option<u64>,
    pub comments: Option<u64>,
    pub shares: Option<u64>,
    pub views: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PostKind {
    Media,
    TextOnly,
    Profile,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Media {
    pub kind: MediaKind,
    /// The file on the site's CDN.
    pub url: String,
    pub thumbnail: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// From the CDN URL's `oe` query parameter (hex Unix time).
    pub expires_at: Option<i64>,
    /// A video's higher quality, kept as separate files that the server joins.
    pub hd: Option<HdVideo>,
    /// A video file's estimated size, from its `bitrate=` and the video's duration.
    pub bytes: Option<u64>,
    /// The site's smaller file of a video (SD), for when this one is too big for Telegram.
    pub smaller: Option<SmallerFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SmallerFile {
    pub url: String,
    pub bytes: Option<u64>,
}

/// A video above its progressive file: the site's DASH video and audio streams (Facebook's 1080p
/// is AV1 or VP9),
/// which the server joins into one MP4 (`crate::video`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HdVideo {
    pub video: String,
    pub audio: Option<String>,
    pub width: u32,
    pub height: u32,
    /// The video stream's codec string, e.g. `av01.0.08M.08.0.111.01.01.01.0`.
    pub codec: String,
    /// The joined file's estimated size, from the streams' bitrates and the duration.
    pub bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Photo,
    Video,
}

impl Media {
    pub fn photo(url: String) -> Self {
        let expires_at = cdn_expiry(&url);
        Self {
            kind: MediaKind::Photo,
            url,
            thumbnail: None,
            width: None,
            height: None,
            expires_at,
            hd: None,
            bytes: None,
            smaller: None,
        }
    }

    pub fn video(url: String, thumbnail: Option<String>) -> Self {
        let expires_at = cdn_expiry(&url);
        Self {
            kind: MediaKind::Video,
            url,
            thumbnail,
            width: None,
            height: None,
            expires_at,
            hd: None,
            bytes: None,
            smaller: None,
        }
    }
}

impl Media {
    /// Telegram plays videos in previews only up to its limit (20 MB), so a bigger file gives way
    /// to the site's smaller one, as FxEmbed picks the best file under the limit. The size stays
    /// the original's, which only sets the shape.
    pub fn fit(&mut self, limit_bytes: u64) -> bool {
        let too_big = self.bytes.is_some_and(|b| b > limit_bytes);
        let Some(smaller) = self.smaller.take_if(|s| too_big && s.bytes.is_none_or(|b| self.bytes > Some(b))) else {
            return false;
        };
        self.expires_at = cdn_expiry(&smaller.url);
        self.url = smaller.url;
        self.bytes = smaller.bytes;
        true
    }
}

impl Post {
    /// Where it comes from, by its cache key.
    pub fn source(&self) -> Option<Source> {
        Source::of_link(&self.key)
    }

    /// A rough size in bytes, for the cache's memory budget.
    pub fn weight(&self) -> usize {
        let text = |s: &Option<String>| s.as_ref().map_or(0, String::len);
        let hd =
            |m: &Media| m.hd.as_ref().map_or(0, |hd| 64 + hd.video.len() + hd.audio.as_ref().map_or(0, String::len));
        let media: usize =
            self.media.iter().map(|m| 64 + m.url.len() + m.thumbnail.as_ref().map_or(0, String::len) + hd(m)).sum();
        let p = &self.profile;
        let profile = text(&p.url) + text(&p.avatar) + text(&p.picture) + text(&p.bio);
        let names = text(&self.title) + text(&self.author) + profile + text(&self.group);
        256 + self.key.len() + self.url.len() + names + text(&self.text) + media
    }

    /// The earliest time a media URL stops working.
    pub fn expires_at(&self) -> Option<i64> {
        self.media
            .iter()
            .flat_map(|m| {
                let hd = m.hd.as_ref();
                [
                    m.expires_at,
                    m.thumbnail.as_deref().and_then(cdn_expiry),
                    hd.and_then(|hd| cdn_expiry(&hd.video)),
                    hd.and_then(|hd| hd.audio.as_deref().and_then(cdn_expiry)),
                ]
            })
            .flatten()
            .chain([&self.profile.avatar, &self.profile.picture].into_iter().flatten().filter_map(|u| cdn_expiry(u)))
            .min()
    }

    pub fn photo_count(&self) -> usize {
        self.media.iter().filter(|m| m.kind == MediaKind::Photo).count()
    }

    pub fn first_video(&self) -> Option<&Media> {
        self.media.iter().find(|m| m.kind == MediaKind::Video)
    }
}

/// Reads the expiry from a signed CDN URL: Meta's `oe` parameter (hex Unix time), which Facebook's
/// and Instagram's CDNs sign every URL with, or the `e` of Reddit's packaged videos (decimal).
pub fn cdn_expiry(url: &str) -> Option<i64> {
    let query = url.split_once('?')?.1;
    let query = query.split_once('#').map_or(query, |(q, _)| q);
    // As u32s: no sign, and no time so far off that the cache's arithmetic on it overflows.
    let seconds = if url.starts_with("https://packaged-media.redd.it/") {
        query.split('&').find_map(|pair| pair.strip_prefix("e=")).and_then(|n| n.parse::<u32>().ok())
    } else {
        query.split('&').find_map(|pair| pair.strip_prefix("oe=")).and_then(|hex| u32::from_str_radix(hex, 16).ok())
    };
    seconds.map(i64::from)
}

/// Whether `url` is an https file on the sites' CDNs (Meta's `fbcdn.net` and `cdninstagram.com`,
/// Reddit's `redd.it` and `redditmedia.com`): the only places the media routes redirect to and
/// 1080p streams are downloaded from, whatever a page held.
pub fn trusted_media_url(url: &str) -> bool {
    let Ok(url) = Url::parse(url) else { return false };
    let host = url.host_str().unwrap_or_default().trim_end_matches('.');
    let on = |domain: &str| host == domain || host.strip_suffix(domain).is_some_and(|sub| sub.ends_with('.'));
    url.scheme() == "https"
        && url.username().is_empty()
        && url.port().is_none()
        && (on("fbcdn.net") || on("cdninstagram.com") || on("redd.it") || on("redditmedia.com"))
}

/// Drops every key not in `kept`, at any depth: what a fixture keeps of a site's answer, so it names
/// no one but the posting account.
pub fn keep_keys(value: &mut serde_json::Value, kept: &[&str]) {
    match value {
        serde_json::Value::Object(map) => {
            map.retain(|key, _| kept.contains(&key.as_str()));
            map.values_mut().for_each(|v| keep_keys(v, kept));
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(|v| keep_keys(v, kept)),
        _ => {}
    }
}

/// What Meta's share links (`/share/…`) are followed with: a browser's full user agent gets a page
/// that redirects by script, anything else the redirect itself.
pub const SHARE_USER_AGENT: &str = "Mozilla/5.0";

/// A failed request to a site, saying which way it failed.
pub fn network_error(error: reqwest::Error) -> ExtractError {
    let kind = if error.is_timeout() {
        "timed out"
    } else if error.is_connect() {
        "couldn't connect"
    } else if error.is_body() || error.is_decode() {
        "the download broke off"
    } else {
        "request failed"
    };
    ExtractError::new(Stage::Fetch, format!("{kind}: {}", crate::net::describe(error)))
}

/// A bad proxy setting, as an extraction's failure.
pub fn proxy_error(error: &anyhow::Error) -> ExtractError {
    ExtractError::new(Stage::Fetch, format!("bad proxy setting: {error:#}"))
}

/// A count as a page writes it, in any language: `1.5K` and `1,5K` → 1500, `3,204`, `3.204` and
/// `3 204` → 3204, `2M` → 2000000. A lone separator is a decimal one before a K, M or B, or when
/// it isn't followed by exactly three digits.
pub fn short_count(text: &str) -> Option<u64> {
    let text = text.trim();
    let (number, scale) = match text.char_indices().last()? {
        (i, 'K' | 'k') => (&text[..i], 1e3),
        (i, 'M' | 'm') => (&text[..i], 1e6),
        (i, 'B' | 'b') => (&text[..i], 1e9),
        _ => (text, 1.0),
    };
    let number: String = number.chars().filter(|c| !c.is_whitespace()).collect();
    let separators = |n: &str| n.matches([',', '.']).count();
    let decimal = match (number.rfind(','), number.rfind('.')) {
        // Both: the last one is the decimal separator.
        (Some(comma), Some(dot)) => Some(comma.max(dot)),
        (Some(at), None) | (None, Some(at)) if separators(&number) == 1 => {
            let grouping = scale == 1.0 && number.len() - at - 1 == 3;
            (!grouping).then_some(at)
        }
        _ => None,
    };
    let whole: String =
        number.char_indices().filter(|&(i, c)| !matches!(c, ',' | '.') || Some(i) == decimal).map(|(_, c)| c).collect();
    let number: f64 = whole.replace(',', ".").parse().ok().filter(|n: &f64| n.is_finite() && *n >= 0.0)?;
    #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "checked finite and not negative")]
    Some((number * scale).round() as u64)
}

/// The step where an extraction failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// Network error or an unexpected answer from the site.
    Fetch,
    /// Too many redirects, or a redirect off the site.
    Redirect,
    /// The site asked for a login and no usable cookies were available.
    LoginWall,
    /// The page loaded but held nothing for the requested post.
    NotFound,
    /// The site asked us to slow down: HTTP 429, or Instagram's "Please wait".
    RateLimited,
    /// The overall deadline passed.
    Timeout,
}

impl Stage {
    pub const ALL: [Self; 6] =
        [Self::Fetch, Self::Redirect, Self::LoginWall, Self::NotFound, Self::RateLimited, Self::Timeout];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fetch => "fetch",
            Self::Redirect => "redirect",
            Self::LoginWall => "login_wall",
            Self::NotFound => "not_found",
            Self::RateLimited => "rate_limited",
            Self::Timeout => "timeout",
        }
    }

    /// What the dashboard calls the step.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Fetch => "Couldn't fetch",
            Self::Redirect => "Not a post",
            Self::LoginWall => "Login wall",
            Self::NotFound => "Not found",
            Self::RateLimited => "Rate limited",
            Self::Timeout => "Timed out",
        }
    }

    /// The step a stored code (`not_found`) names.
    pub fn parse(code: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.as_str() == code)
    }

    /// Says nothing about the post itself, so it's cached for only a minute: the next request
    /// may well work.
    pub const fn is_transient(self) -> bool {
        matches!(self, Self::Fetch | Self::RateLimited | Self::Timeout)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}: {message}", stage.as_str())]
pub struct ExtractError {
    pub stage: Stage,
    pub message: String,
}

impl ExtractError {
    pub fn new(stage: Stage, message: impl Into<String>) -> Self {
        Self { stage, message: message.into() }
    }
}

/// How long each step took, for logs and the playground.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Timings {
    pub fetch: Duration,
    pub parse: Duration,
    pub extract: Duration,
    pub requests: u32,
}

impl Timings {
    /// Adds what work done alongside took.
    pub fn add(&mut self, other: &Self) {
        self.fetch += other.fetch;
        self.parse += other.parse;
        self.extract += other.extract;
        self.requests += other.requests;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CookieChoice {
    /// Follow the cookie policy setting.
    #[default]
    Policy,
    /// Use cookies regardless (the `--cookies` flag).
    Force,
}

pub type ExtractResult = Result<(Post, Timings), ExtractError>;
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait Extractor: Send + Sync {
    /// Extracts the post at `target`. `settings` carries the proxy and the cookie policy.
    fn extract<'a>(
        &'a self,
        target: &'a Target,
        settings: &'a Settings,
        cookies: CookieChoice,
    ) -> BoxFuture<'a, ExtractResult>;
}

/// Every source's extractor, each given its own targets.
pub struct Sites {
    pub facebook: facebook::Facebook,
    pub instagram: instagram::Instagram,
    pub reddit: reddit::Reddit,
    pub threads: threads::Threads,
}

/// Reads a fixture saved as JSON (Instagram's answers, Reddit's crawler tags, Threads' posts), for
/// its site.
pub fn read_fixture(saved: &serde_json::Value, target: &Target) -> Result<Post, ExtractError> {
    match target.source {
        Source::Instagram => instagram::read_fixture(saved, target),
        Source::Reddit => reddit::read_fixture(saved, target),
        Source::Threads => threads::read_fixture(saved, target),
        Source::Facebook => Err(ExtractError::new(Stage::Fetch, "Facebook's fixtures are saved pages, not JSON")),
    }
}

impl Sites {
    /// Fetches the page or response a target is extracted from, for a fixture.
    pub async fn record(&self, target: &Target, settings: &Settings, with_cookies: bool) -> anyhow::Result<String> {
        match target.source {
            Source::Facebook => self.facebook.record(target, settings, with_cookies).await,
            Source::Instagram => self.instagram.record(target, settings, with_cookies).await,
            Source::Reddit => self.reddit.record(target, settings, with_cookies).await,
            Source::Threads => self.threads.record(target, settings, with_cookies).await,
        }
    }
}

impl Extractor for Sites {
    fn extract<'a>(
        &'a self,
        target: &'a Target,
        settings: &'a Settings,
        cookies: CookieChoice,
    ) -> BoxFuture<'a, ExtractResult> {
        Box::pin(async move {
            let (mut post, timings) = match target.source {
                Source::Facebook => self.facebook.extract(target, settings, cookies).await?,
                Source::Instagram => self.instagram.extract(target, settings, cookies).await?,
                Source::Reddit => self.reddit.extract(target, settings, cookies).await?,
                Source::Threads => self.threads.extract(target, settings, cookies).await?,
            };
            keep_trusted_media(&mut post);
            Ok((post, timings))
        })
    }
}

/// Drops media that isn't on Meta's CDNs, so nothing a page says can send Telegram, or the
/// server's own 1080p downloads, anywhere else. The author's pictures included: they go on the
/// card and in Instant View.
fn keep_trusted_media(post: &mut Post) {
    let before = post.media.len();
    post.media.retain(|m| trusted_media_url(&m.url));
    let profile = &mut post.profile;
    profile.avatar = profile.avatar.take().filter(|u| trusted_media_url(u));
    profile.picture = profile.picture.take().filter(|u| trusted_media_url(u));
    for media in &mut post.media {
        media.thumbnail = media.thumbnail.take().filter(|t| trusted_media_url(t));
        media.smaller = media.smaller.take().filter(|s| trusted_media_url(&s.url));
        media.hd = media
            .hd
            .take()
            .filter(|hd| trusted_media_url(&hd.video) && hd.audio.as_deref().is_none_or(trusted_media_url));
    }
    if post.media.len() < before {
        tracing::warn!(url = %post.key, dropped = before - post.media.len(), "media outside the sites' CDNs was left out");
    }
}

#[cfg(test)]
pub mod fake {
    //! A stand-in extractor for testing the server without Facebook.

    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    #[derive(Default)]
    pub struct FakeExtractor {
        pub calls: AtomicU32,
        pub delay: Option<Duration>,
        pub fail: Mutex<Option<ExtractError>>,
        /// Media to return instead of the two photos.
        pub media: Mutex<Option<Vec<Media>>>,
    }

    impl FakeExtractor {
        pub fn post(target: &Target) -> Post {
            Post {
                key: target.key().to_string(),
                url: target.url.to_string(),
                page_type: target.page_type,
                title: Some("Some Page".into()),
                author: Some("Some Page".into()),
                profile: AuthorProfile::default(),
                group: None,
                published_at: Some(1_700_000_000),
                text: Some("Hello <world>\n\nSecond paragraph https://example.com/x".into()),
                media: vec![
                    Media::photo("https://scontent.xx.fbcdn.net/v/a.jpg?oe=7FFFFFFF".into()),
                    Media::photo("https://scontent.xx.fbcdn.net/v/b.jpg?oe=7FFFFFFF".into()),
                ],
                kind: PostKind::Media,
                stats: Stats::default(),
                shared: None,
            }
        }
    }

    impl Extractor for FakeExtractor {
        fn extract<'a>(&'a self, target: &'a Target, _: &'a Settings, _: CookieChoice) -> BoxFuture<'a, ExtractResult> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                if let Some(delay) = self.delay {
                    tokio::time::sleep(delay).await;
                }
                if let Some(error) = self.fail.lock().unwrap().clone() {
                    return Err(error);
                }
                let mut post = Self::post(target);
                if let Some(media) = self.media.lock().unwrap().clone() {
                    post.media = media;
                }
                Ok((post, Timings::default()))
            })
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_video_too_big_for_telegram_gives_way_to_the_smaller_file() {
        let mut video = Media::video("https://video.xx.fbcdn.net/hd.mp4?bitrate=1&oe=10".into(), None);
        video.bytes = Some(30_000_000);
        video.smaller =
            Some(SmallerFile { url: "https://video.xx.fbcdn.net/sd.mp4?oe=20".into(), bytes: Some(9_000_000) });
        let mut fits = video.clone();
        fits.bytes = Some(5_000_000);
        assert!(!fits.fit(20 << 20), "under the limit: keeps the better file");
        assert!(video.fit(20 << 20));
        assert_eq!(
            (video.url.as_str(), video.bytes, video.expires_at),
            ("https://video.xx.fbcdn.net/sd.mp4?oe=20", Some(9_000_000), Some(32))
        );
        let mut unknown = Media::video("https://video.xx.fbcdn.net/hd.mp4".into(), None);
        unknown.smaller = video.smaller.clone();
        assert!(!unknown.fit(20 << 20), "no estimate, no swap");
    }

    use super::*;

    #[test]
    fn sources_by_name_and_link() {
        assert_eq!(Source::from_name("facebook"), Some(Source::Facebook));
        assert_eq!(Source::from_name("Facebook"), None);
        assert_eq!(Source::of_link("https://www.facebook.com/reel/1"), Some(Source::Facebook));
        assert_eq!(Source::of_link("https://m.facebook.com/watch?v=1"), Some(Source::Facebook));
        for other in ["https://facebook.com.example.com/reel/1", "https://example.com/reel/1", "not a link"] {
            assert_eq!(Source::of_link(other), None, "{other}");
        }
        let post = fake::FakeExtractor::post(&Target::from_request(Source::Facebook, "/reel/1").unwrap());
        assert_eq!(post.source(), Some(Source::Facebook));
    }

    #[test]
    fn reads_cdn_expiry() {
        assert_eq!(cdn_expiry("https://x.fbcdn.net/v/a.jpg?stp=1&oe=6650A1B2&_nc=1"), Some(0x6650_A1B2));
        assert_eq!(cdn_expiry("https://x.fbcdn.net/v/a.jpg?stp=1"), None);
        assert_eq!(cdn_expiry("https://x.fbcdn.net/v/a.jpg?oe=zz"), None);
    }
}
