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

use crate::settings::{CookiePolicy, Settings};

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
        self.domains()[0]
    }

    /// Every domain its links are on, its own first: short links' and the old ones'.
    pub const fn domains(self) -> &'static [&'static str] {
        match self {
            Self::Facebook => &["facebook.com", facebook::url::SHORT_HOST],
            Self::Instagram => &["instagram.com", "instagr.am"],
            Self::Reddit => &["reddit.com", "redd.it"],
            Self::Threads => &["threads.com", "threads.net"],
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
        list(&Self::ALL.map(Self::name), "or")
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
        let on = |domain: &&str| host == *domain || host.strip_suffix(domain).is_some_and(|sub| sub.ends_with('.'));
        Self::ALL.into_iter().find(|source| source.domains().iter().any(on))
    }

    /// The source of a cache key or another link (`https://www.facebook.com/reel/1`).
    pub fn of_link(link: &str) -> Option<Self> {
        Self::of_url(&Url::parse(link.trim()).ok()?)
    }
}

/// Words for a sentence: "a", "a or b", "a, b or c" (with `and`, "a, b and c").
pub fn list(words: &[&str], and_or: &str) -> String {
    match words.split_last() {
        Some((last, [])) => (*last).to_string(),
        Some((last, rest)) => format!("{} {and_or} {last}", rest.join(", ")),
        None => String::new(),
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
        let mut path_and_query = match url.query() {
            Some(query) => format!("{}?{query}", url.path()),
            None => url.path().to_string(),
        };
        // As the bot sends it: a short link's host goes in front of its path.
        if url.host_str() == Some(facebook::url::SHORT_HOST) {
            path_and_query.insert_str(0, "/fb.watch");
        }
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
    /// Marked NSFW or a spoiler, as Reddit does: the card leaves its media out, as Reddit blurs
    /// them, and Instant View, opened on purpose, shows them.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub sensitive: bool,
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
    /// The post's media are its own, not this one's (a reply's or a quote's pictures): Instant
    /// View shows them with the post's words, before this one, rather than under it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub post_owns_media: bool,
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
    /// The site's smaller file: a video's SD one, for when this one is too big for Telegram; a
    /// photo's lighter copy, for a mosaic whose tile it still covers.
    pub smaller: Option<SmallerFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SmallerFile {
    pub url: String,
    pub bytes: Option<u64>,
    /// A photo's copy's width; its height keeps the photo's shape.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
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
        let smaller = |m: &Media| m.smaller.as_ref().map_or(0, |s| 16 + s.url.len());
        let media: usize = self
            .media
            .iter()
            .map(|m| 64 + m.url.len() + m.thumbnail.as_ref().map_or(0, String::len) + hd(m) + smaller(m))
            .sum();
        let p = &self.profile;
        let profile = text(&p.url) + text(&p.avatar) + text(&p.picture) + text(&p.bio);
        let names = text(&self.title) + text(&self.author) + profile + text(&self.group);
        // A shared post can carry as much text as the post itself.
        let shared = self.shared.as_ref().map_or(0, |s| {
            64 + text(&s.url)
                + text(&s.title)
                + text(&s.author)
                + text(&s.text)
                + text(&s.profile.url)
                + text(&s.profile.avatar)
        });
        256 + self.key.len() + self.url.len() + names + text(&self.text) + media + shared
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
        && url.password().is_none()
        && url.port().is_none()
        && (on("fbcdn.net") || on("cdninstagram.com") || on("redd.it") || on("redditmedia.com"))
}

/// Whether `link` is an https page on `origin`'s host: what a site's data says a post's address
/// is mustn't point anywhere else (`https://www.reddit.com.example.org/`, `…@example.org`).
pub fn on_origin(link: &str, origin: &str) -> bool {
    let (Ok(url), Ok(origin)) = (Url::parse(link), Url::parse(origin)) else { return false };
    url.scheme() == "https"
        && url.host_str() == origin.host_str()
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
}

/// `origin` and a path from a site's data, when the path keeps it on that host.
pub fn origin_path(origin: &str, path: &str) -> Option<String> {
    let link = format!("{origin}{path}");
    (path.starts_with('/') && on_origin(&link, origin)).then_some(link)
}

/// How long an author's picture and bio are reused, well within a picture link's life.
const PROFILE_TTL: Duration = Duration::from_secs(6 * 3600);

/// A site's cache of authors' profiles, by name.
pub fn profile_cache<V: Clone + Send + Sync + 'static>() -> moka::future::Cache<String, V> {
    lru_cache(PROFILE_TTL)
}

/// A cache that drops the least recently used entry when full, as every cache here does: moka's
/// default policy can refuse new entries instead.
pub fn lru_cache<V: Clone + Send + Sync + 'static>(ttl: Duration) -> moka::future::Cache<String, V> {
    moka::future::Cache::builder()
        .max_capacity(5000)
        .eviction_policy(moka::policy::EvictionPolicy::lru())
        .time_to_live(ttl)
        .build()
}

/// A page's body as text, without copying it when it's valid UTF-8 (it nearly always is).
pub fn body_text(body: Vec<u8>) -> String {
    String::from_utf8(body).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
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

/// The one browser every site's requests say they come from: a logged-in session that shows up as
/// another device gets logged out.
pub const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36";

/// A site's client, to finish with `build()`: [`USER_AGENT`], `headers` on every request, no
/// redirects followed (each hop is checked by the extractor), connections kept warm, and `proxy`
/// when set.
pub fn site_client(
    headers: &[(&'static str, &'static str)],
    read_timeout: Duration,
    proxy: Option<&str>,
) -> anyhow::Result<reqwest::ClientBuilder> {
    let headers = headers
        .iter()
        .map(|&(name, value)| {
            (reqwest::header::HeaderName::from_static(name), reqwest::header::HeaderValue::from_static(value))
        })
        .collect();
    let mut builder = crate::net::client_builder()?
        .user_agent(USER_AGENT)
        .default_headers(headers)
        .redirect(reqwest::redirect::Policy::none())
        .read_timeout(read_timeout)
        .pool_idle_timeout(Duration::from_secs(300))
        .tcp_keepalive(Duration::from_secs(60));
    if let Some(proxy) = proxy {
        builder = builder.proxy(reqwest::Proxy::all(proxy)?);
    }
    Ok(builder)
}

/// A response's body, up to `max` bytes: past that, `site`'s answer is refused as too big.
pub async fn read_body(response: reqwest::Response, max: usize, site: Source) -> Result<Vec<u8>, ExtractError> {
    use futures_util::StreamExt;
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        body.extend_from_slice(&chunk.map_err(network_error)?);
        if body.len() > max {
            return Err(ExtractError::new(Stage::Fetch, format!("{}'s answer was too big", site.name())));
        }
    }
    Ok(body)
}

/// When a site's cookies go with a fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CookiePlan {
    /// With the first request.
    pub first: bool,
    /// With a second, after a login wall or a missing post without them.
    pub retry: bool,
}

impl CookiePlan {
    /// The plan for `choice` under `policy`. `jar`: cookies are saved and may go with this page.
    /// `login_first`: the page needs a login anyway, so trying without would only cost a round
    /// trip. `None` when cookies are forced and there are none to use.
    pub const fn of(choice: CookieChoice, policy: CookiePolicy, jar: bool, login_first: bool) -> Option<Self> {
        let (first, retry) = match (choice, policy) {
            (CookieChoice::Force, _) if !jar => return None,
            (CookieChoice::Force, _) => (true, false),
            (CookieChoice::Policy, CookiePolicy::Always) => (jar, false),
            (CookieChoice::Policy, CookiePolicy::Never) => (false, false),
            (CookieChoice::Policy, CookiePolicy::Fallback) if login_first => (jar, false),
            (CookieChoice::Policy, CookiePolicy::Fallback) => (false, jar),
        };
        Some(Self { first, retry })
    }
}

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
    /// Too many redirects, a redirect off the site, or a link that names no post.
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
                sensitive: false,
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
        video.smaller = Some(SmallerFile {
            url: "https://video.xx.fbcdn.net/sd.mp4?oe=20".into(),
            bytes: Some(9_000_000),
            width: None,
        });
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
    fn when_cookies_go() {
        use CookieChoice::{Force, Policy};
        use CookiePolicy::{Always, Fallback, Never};
        let plan = |choice, policy, jar, login_first| {
            CookiePlan::of(choice, policy, jar, login_first).map(|p| (p.first, p.retry))
        };
        assert_eq!(plan(Force, Never, true, false), Some((true, false)), "forced, whatever the policy");
        assert_eq!(plan(Force, Always, false, false), None, "forced, with none to use");
        assert_eq!(plan(Policy, Always, true, false), Some((true, false)));
        assert_eq!(plan(Policy, Always, false, false), Some((false, false)));
        assert_eq!(plan(Policy, Never, true, true), Some((false, false)));
        assert_eq!(plan(Policy, Fallback, true, false), Some((false, true)), "after a login wall");
        assert_eq!(plan(Policy, Fallback, true, true), Some((true, false)), "a page behind a login anyway");
        assert_eq!(plan(Policy, Fallback, false, true), Some((false, false)));
    }

    #[test]
    fn sources_by_name_and_link() {
        assert_eq!(Source::from_name("facebook"), Some(Source::Facebook));
        assert_eq!(Source::from_name("Facebook"), None);
        assert_eq!(Source::of_link("https://www.facebook.com/reel/1"), Some(Source::Facebook));
        assert_eq!(Source::of_link("https://m.facebook.com/watch?v=1"), Some(Source::Facebook));
        assert_eq!(Source::of_link("https://fb.watch/x/"), Some(Source::Facebook));
        assert_eq!(Source::of_link("https://www.threads.net/@x"), Some(Source::Threads));
        assert_eq!(Source::of_link("https://v.redd.it/x"), Some(Source::Reddit));
        assert_eq!(Source::listed(), "Facebook, Instagram, Reddit or Threads");
        assert_eq!((list(&["a"], "and"), list(&["a", "b", "c"], "and")), ("a".into(), "a, b and c".into()));
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
