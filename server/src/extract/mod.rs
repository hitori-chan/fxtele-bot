//! What an extractor produces, and the trait in front of the Facebook implementation.

pub mod facebook;

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde::Serialize;

use crate::extract::facebook::url::{PageType, Target};
use crate::settings::Settings;

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
}

/// The author beyond their name.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AuthorProfile {
    /// Their profile page.
    pub url: Option<String>,
    /// Facebook's small picture from the post's page (40 px).
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

/// Counts as Facebook showed them when the post was fetched.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Stats {
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
    /// Facebook CDN URL.
    pub url: String,
    pub thumbnail: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// From the fbcdn `oe` query parameter (hex Unix time).
    pub expires_at: Option<i64>,
    /// A video's higher quality, kept as separate files that the server joins.
    pub hd: Option<HdVideo>,
    /// A video file's estimated size, from its `bitrate=` and the video's duration.
    pub bytes: Option<u64>,
    /// Facebook's smaller file of a video (SD), for when this one is too big for Telegram.
    pub smaller: Option<SmallerFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SmallerFile {
    pub url: String,
    pub bytes: Option<u64>,
}

/// A video above its 720p file: Facebook's DASH video and audio streams (1080p is AV1 or VP9),
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
    /// to Facebook's smaller one, as FxEmbed picks the best file under the limit. The size stays
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

/// Reads the expiry from an fbcdn URL's `oe` parameter (hex Unix time).
pub fn cdn_expiry(url: &str) -> Option<i64> {
    let query = url.split_once('?')?.1;
    let query = query.split_once('#').map_or(query, |(q, _)| q);
    query.split('&').find_map(|pair| pair.strip_prefix("oe=")).and_then(|hex| i64::from_str_radix(hex, 16).ok())
}

/// The step where an extraction failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// Network error or an unexpected status from Facebook.
    Fetch,
    /// Too many redirects, or a redirect off Facebook.
    Redirect,
    /// Facebook asked for a login and no usable cookies were available.
    LoginWall,
    /// The page loaded but held nothing for the requested post.
    NotFound,
    /// The overall deadline passed.
    Timeout,
}

impl Stage {
    pub const ALL: [Self; 5] = [Self::Fetch, Self::Redirect, Self::LoginWall, Self::NotFound, Self::Timeout];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fetch => "fetch",
            Self::Redirect => "redirect",
            Self::LoginWall => "login_wall",
            Self::NotFound => "not_found",
            Self::Timeout => "timeout",
        }
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
    fn reads_cdn_expiry() {
        assert_eq!(cdn_expiry("https://x.fbcdn.net/v/a.jpg?stp=1&oe=6650A1B2&_nc=1"), Some(0x6650_A1B2));
        assert_eq!(cdn_expiry("https://x.fbcdn.net/v/a.jpg?stp=1"), None);
        assert_eq!(cdn_expiry("https://x.fbcdn.net/v/a.jpg?oe=zz"), None);
    }
}
