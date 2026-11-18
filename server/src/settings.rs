//! Live settings: stored in SQLite, edited in the dashboard, swapped in place without a restart.

use std::sync::Arc;

use anyhow::{Result, bail};
use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};

use crate::db::Db;
use crate::extract::Source;

const DB_KEY: &str = "settings";
/// Where saved settings this version can't read are kept.
const UNREADABLE_KEY: &str = "settings_unreadable";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub facebook: SourceSettings,
    pub instagram: SourceSettings,
    pub reddit: SourceSettings,
    pub threads: SourceSettings,
    /// How long extracted posts are kept.
    pub cache_minutes: u64,
    /// How long failures that say something about the post are kept; others, a minute.
    pub failure_cache_minutes: u64,
    pub cache_megabytes: u64,
    /// Requests per IP per minute on the public listener; Telegram is exempt.
    pub ip_requests_per_minute: u32,
    /// Overall time for one extraction; keep it under how long Telegram waits for a page.
    pub extract_timeout_seconds: u64,
    pub instant_view: InstantView,
    /// Reactions, comments, shares and views at the end of Instant View, as of the fetch.
    pub instant_view_counts: bool,
    /// A card for several photos shows the first four as one picture (`crate::mosaic`).
    pub photo_mosaic: bool,
    /// Discord's fetcher gets cards too. It has no published addresses, so anyone can claim to be
    /// it: it stays under the per-address limit and has half of each site's fetches at most.
    pub discord: bool,
    /// `og:description` is cut to this many characters.
    pub description_length: usize,
    pub failure_response: FailureResponse,
    pub media_links: MediaLinks,
    pub video_quality: VideoQuality,
    /// Largest video the server joins, in MB; Telegram doesn't play bigger ones in previews.
    pub video_megabytes: u64,
    /// Space for joined videos in the bucket, in MB.
    #[serde(alias = "video_cache_megabytes")]
    pub video_space_megabytes: u64,
    /// Days a joined video stays in the bucket. Telegram fetches it right after the preview, and
    /// keeps its own copy.
    pub video_days: u64,
    pub test_links: Vec<String>,
    /// Minutes between runs of the test links. Each check is a fetch the site counts.
    pub test_minutes: u64,
    pub alert_chat: Option<i64>,
}

/// What each source has its own of.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SourceSettings {
    /// Outgoing proxy for this site, e.g. `socks5://127.0.0.1:40000`. `None` goes direct.
    pub proxy: Option<String>,
    pub cookie_policy: CookiePolicy,
    /// Extractions per minute: cache misses from Telegram, the playground and test links.
    pub fetches_per_minute: u32,
    /// Extractions at once.
    pub concurrency: u32,
    /// Fetch the author's profile for their full-size picture and bio (Instant View, text
    /// posts); one more request per author every six hours.
    pub author_profiles: bool,
    /// `og:site_name`; empty means the site's own name.
    pub site_name: String,
}

impl Default for SourceSettings {
    fn default() -> Self {
        Self {
            proxy: None,
            cookie_policy: CookiePolicy::Fallback,
            fetches_per_minute: 60,
            concurrency: 4,
            author_profiles: true,
            site_name: String::new(),
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            facebook: SourceSettings::default(),
            // Instagram blocks an address after a few hundred requests an hour, so it gets a smaller
            // budget and no extra request for authors' bios.
            instagram: SourceSettings {
                fetches_per_minute: 20,
                concurrency: 2,
                author_profiles: false,
                ..SourceSettings::default()
            },
            // Reddit answers its data only to a logged-in session, 100 requests in 10 minutes per
            // account; logged out there are only the crawlers' card and the embed page.
            reddit: SourceSettings {
                cookie_policy: CookiePolicy::Always,
                fetches_per_minute: 6,
                concurrency: 2,
                author_profiles: false,
                ..SourceSettings::default()
            },
            // Threads' pages are a megabyte each, and Meta blocks busy addresses as on Instagram;
            // an author's bio costs another page.
            threads: SourceSettings {
                fetches_per_minute: 20,
                concurrency: 2,
                author_profiles: false,
                ..SourceSettings::default()
            },
            cache_minutes: 60,
            failure_cache_minutes: 5,
            cache_megabytes: 64,
            ip_requests_per_minute: 120,
            extract_timeout_seconds: 8,
            instant_view: InstantView::default(),
            instant_view_counts: true,
            photo_mosaic: true,
            discord: true,
            description_length: 300,
            failure_response: FailureResponse::Page,
            media_links: MediaLinks::Direct,
            video_quality: VideoQuality::Standard,
            video_megabytes: 20,
            video_space_megabytes: 1024,
            video_days: 3,
            test_links: Vec::new(),
            test_minutes: 60,
            alert_chat: None,
        }
    }
}

/// When to send a site's login cookies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CookiePolicy {
    /// Without login first; with cookies after a login wall or an empty result.
    Fallback,
    Always,
    Never,
}

/// What Telegram gets when extraction fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureResponse {
    /// `302` to the post, so Telegram shows the site's own preview.
    Redirect,
    /// A minimal "couldn't load" page.
    Page,
    /// `503`.
    Unavailable,
}

/// How media URLs appear in previews.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaLinks {
    /// The CDN URLs as they are.
    Direct,
    /// `/_/m/...` links that redirect to a fresh CDN URL.
    Redirect,
}

/// Which file Telegram gets for a video.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoQuality {
    /// Facebook's own 720p H.264 file, which plays everywhere and never passes through the server.
    Standard,
    /// Up to 1080p: Facebook's DASH video and audio files, joined by the server and kept in the
    /// bucket set up in `.env`, which Telegram downloads from. Above 720p Facebook encodes only AV1
    /// or VP9, which some phones show as a black picture.
    Best,
}

/// Instant View per post type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct InstantView {
    pub video: bool,
    pub photo: bool,
    pub album: bool,
    pub text: bool,
    pub profile: bool,
}

impl Default for InstantView {
    fn default() -> Self {
        Self { video: true, photo: true, album: true, text: true, profile: true }
    }
}

impl Settings {
    pub const fn source(&self, source: Source) -> &SourceSettings {
        match source {
            Source::Facebook => &self.facebook,
            Source::Instagram => &self.instagram,
            Source::Reddit => &self.reddit,
            Source::Threads => &self.threads,
        }
    }

    pub const fn source_mut(&mut self, source: Source) -> &mut SourceSettings {
        match source {
            Source::Facebook => &mut self.facebook,
            Source::Instagram => &mut self.instagram,
            Source::Reddit => &mut self.reddit,
            Source::Threads => &mut self.threads,
        }
    }

    /// `og:site_name` for posts from `source`.
    pub fn site_name(&self, source: Source) -> &str {
        let name = self.source(source).site_name.trim();
        if name.is_empty() { source.name() } else { name }
    }

    /// Puts what [`Settings::check`] would refuse within its limits, and says what it changed.
    fn repair(&mut self) -> Vec<String> {
        let mut fixed = Vec::new();
        if self.check().is_ok() {
            return fixed;
        }
        for source in Source::ALL {
            let name = source.name();
            let default = Self::default().source(source).clone();
            let s = self.source_mut(source);
            if s.proxy.as_deref().is_some_and(|p| !is_proxy(p)) {
                s.proxy = None;
                fixed.push(format!("{name}'s proxy wasn't a proxy's URL, so it's off; set it again on its page"));
            }
            if s.fetches_per_minute == 0 || s.concurrency == 0 {
                (s.fetches_per_minute, s.concurrency) = (default.fetches_per_minute, default.concurrency);
                fixed.push(format!("{name}'s fetches were 0, so they're back to the defaults"));
            }
            if s.fetches_per_minute > MAX_FETCHES || s.concurrency > MAX_AT_ONCE {
                (s.fetches_per_minute, s.concurrency) =
                    (s.fetches_per_minute.min(MAX_FETCHES), s.concurrency.min(MAX_AT_ONCE));
                fixed.push(format!("{name}'s fetches are now within their limits"));
            }
        }
        for (name, field, min, max, unit) in self.numbers() {
            let fitted = (*field).clamp(min, max);
            if fitted != *field {
                fixed.push(format!("{name} was {field} {unit}, now {fitted}, within its limits"));
                *field = fitted;
            }
        }
        if self.ip_requests_per_minute == 0 {
            self.ip_requests_per_minute = Self::default().ip_requests_per_minute;
            fixed.push("Rate limits: Requests per IP was 0, now the default".into());
        }
        if self.ip_requests_per_minute > MAX_IP_REQUESTS {
            self.ip_requests_per_minute = MAX_IP_REQUESTS;
            fixed.push(format!("Rate limits: Requests per IP is now {MAX_IP_REQUESTS}, its most"));
        }
        let fitted = self.description_length.clamp(MIN_DESCRIPTION, MAX_DESCRIPTION);
        if fitted != self.description_length {
            self.description_length = fitted;
            fixed.push(format!("Previews: Description length is now {fitted}, within its limits"));
        }
        if self.video_space_megabytes < self.video_megabytes {
            self.video_space_megabytes = self.video_megabytes;
            fixed.push("Videos: Space in the bucket is now at least one largest video".into());
        }
        let before = self.test_links.len();
        self.test_links.retain(|link| crate::extract::Target::from_link(link).is_some());
        if self.test_links.len() < before {
            fixed.push(format!(
                "{} test links that aren't links to a site were dropped",
                before - self.test_links.len()
            ));
        }
        fixed
    }

    /// The numbers the dashboard sets, named as their fields are on their pages, with their
    /// limits. Sizes are worked out in bytes and times in seconds, which mustn't overflow.
    fn numbers(&mut self) -> [(&'static str, &mut u64, u64, u64, &'static str); 8] {
        [
            ("Cache: Keep posts", &mut self.cache_minutes, 1, 10_080, "minutes"),
            ("Cache: Keep failures", &mut self.failure_cache_minutes, 1, 10_080, "minutes"),
            ("Cache: Size", &mut self.cache_megabytes, 1, 65_536, "MB"),
            // Requests get 30 s in all, and a preview that's too late is no preview at all.
            ("Rate limits: Extraction timeout", &mut self.extract_timeout_seconds, 1, 25, "seconds"),
            // Held in memory while joined, by two joins at once.
            ("Videos: Largest video", &mut self.video_megabytes, 1, 100, "MB"),
            ("Videos: Space in the bucket", &mut self.video_space_megabytes, 1, 1_000_000, "MB"),
            ("Videos: Days in the bucket", &mut self.video_days, 1, 30, "days"),
            ("Test links: Check every", &mut self.test_minutes, 15, 1_440, "minutes"),
        ]
    }

    pub fn check(&self) -> Result<()> {
        for source in Source::ALL {
            let s = self.source(source);
            let name = source.name();
            if let Some(proxy) = &s.proxy {
                if url::Url::parse(proxy).is_err() {
                    bail!("{name}'s proxy must be a URL, such as socks5h://127.0.0.1:1080.");
                }
                if !is_proxy(proxy) {
                    bail!("{name}'s proxy must start with socks5://, socks5h://, http:// or https://.");
                }
            }
            if s.fetches_per_minute == 0 || s.concurrency == 0 {
                bail!("{name}'s fetches a minute and at once must each be at least 1.");
            }
            if s.fetches_per_minute > MAX_FETCHES || s.concurrency > MAX_AT_ONCE {
                bail!("{name}'s fetches can be at most {MAX_FETCHES} a minute and {MAX_AT_ONCE} at once.");
            }
        }
        // "1 minute", "25 seconds", "1 MB", "1 a minute".
        let amount = |n: u64, unit: &str| match unit.strip_suffix('s') {
            Some(one) if n == 1 => format!("1 {one}"),
            _ => format!("{n} {unit}"),
        };
        for (name, value, min, max, unit) in self.clone().numbers() {
            if *value < min {
                bail!("{name} must be at least {}.", amount(min, unit));
            }
            if *value > max {
                bail!("{name} can be at most {}.", amount(max, unit));
            }
        }
        if !(1..=MAX_IP_REQUESTS).contains(&self.ip_requests_per_minute) {
            bail!("Rate limits: Requests per IP must be from 1 to {MAX_IP_REQUESTS} a minute.");
        }
        if !(MIN_DESCRIPTION..=MAX_DESCRIPTION).contains(&self.description_length) {
            bail!("Previews: Description length must be from {MIN_DESCRIPTION} to {MAX_DESCRIPTION} characters.");
        }
        if self.video_space_megabytes < self.video_megabytes {
            bail!("Videos: Space in the bucket must hold at least one video of the largest size.");
        }
        for link in &self.test_links {
            if crate::extract::Target::from_link(link).is_none() {
                bail!("{link:?} isn't a link to {}.", Source::listed());
            }
        }
        Ok(())
    }
}

/// The shortest and longest description a card can have.
const MIN_DESCRIPTION: usize = 20;
const MAX_DESCRIPTION: usize = 4_096;
/// The most a site's budget can allow, a minute and at once, and requests from an address.
const MAX_FETCHES: u32 = 1_000;
const MAX_AT_ONCE: u32 = 100;
const MAX_IP_REQUESTS: u32 = 100_000;

/// A proxy's URL, of a kind the HTTP client can use.
fn is_proxy(proxy: &str) -> bool {
    url::Url::parse(proxy).is_ok_and(|url| matches!(url.scheme(), "socks5" | "socks5h" | "http" | "https"))
}

pub struct LiveSettings {
    current: ArcSwap<Settings>,
    db: Db,
    /// One change at a time, each from the last one saved.
    saving: tokio::sync::Mutex<()>,
}

impl LiveSettings {
    pub async fn load(db: Db) -> Result<Self> {
        let saved = db.get_json::<serde_json::Value>(DB_KEY).await.unwrap_or_else(|e| {
            tracing::error!("stored settings are unreadable, so the defaults apply: {e:#}");
            None
        });
        let settings = match saved {
            None => Settings::default(),
            Some(saved) => match Settings::from_value(saved.clone()) {
                Ok(settings) => settings,
                // Kept apart, as the next save replaces them: test links, proxies, the alert chat.
                Err(e) => {
                    db.set_json(UNREADABLE_KEY, &saved).await?;
                    tracing::error!(
                        "stored settings are unreadable, so the defaults apply; the old ones are kept as \
                         `{UNREADABLE_KEY}` in server.db: {e:#}"
                    );
                    Settings::default()
                }
            },
        };
        // Saved before a limit was tightened, say: fixed here, or every save on any page would be
        // refused over a field on another.
        let mut settings = settings;
        for fixed in settings.repair() {
            tracing::warn!("stored settings: {fixed}");
        }
        Ok(Self { current: ArcSwap::from_pointee(settings), db, saving: tokio::sync::Mutex::new(()) })
    }

    pub fn get(&self) -> Arc<Settings> {
        self.current.load_full()
    }

    /// Replaces the settings: checks, saves and applies them.
    pub async fn set(&self, settings: Settings) -> Result<()> {
        self.update(|s| {
            *s = settings;
            Ok(())
        })
        .await
    }

    /// [`LiveSettings::update`] with a change that can't fail by itself.
    pub async fn edit(&self, change: impl FnOnce(&mut Settings)) -> Result<()> {
        self.update(|s| {
            change(s);
            Ok(())
        })
        .await
    }

    /// Changes the settings with `change`, then checks, saves and applies them. One change runs at
    /// a time, from the last one saved, so two pages saving at once can't undo each other's
    /// change. Nothing is saved when `change` or the check fails.
    pub async fn update(&self, change: impl FnOnce(&mut Settings) -> Result<()>) -> Result<()> {
        let _one = self.saving.lock().await;
        let mut settings = (*self.get()).clone();
        change(&mut settings)?;
        settings.check()?;
        self.db.set_json(DB_KEY, &settings).await?;
        self.current.store(Arc::new(settings));
        Ok(())
    }
}

impl Settings {
    /// Saved or exported settings, older ones included.
    pub fn from_value(value: serde_json::Value) -> serde_json::Result<Self> {
        let mut value = upgrade(value);
        // A site's section that leaves fields out gets that site's defaults, not the ones every
        // section starts from (Facebook's budget would get Instagram's address blocked).
        if let (Some(saved), serde_json::Value::Object(defaults)) =
            (value.as_object_mut(), serde_json::to_value(Self::default())?)
        {
            for source in Source::ALL {
                if let (Some(serde_json::Value::Object(section)), Some(serde_json::Value::Object(default))) =
                    (saved.get_mut(source.as_str()), defaults.get(source.as_str()))
                {
                    for (key, value) in default {
                        section.entry(key.clone()).or_insert_with(|| value.clone());
                    }
                }
            }
        }
        serde_json::from_value(value)
    }
}

/// Moves what settings saved before Instagram held for Facebook into its own section.
fn upgrade(mut value: serde_json::Value) -> serde_json::Value {
    const MOVED: [(&str, &str); 6] = [
        ("proxy", "proxy"),
        ("cookie_policy", "cookie_policy"),
        ("facebook_fetches_per_minute", "fetches_per_minute"),
        ("facebook_concurrency", "concurrency"),
        ("author_pictures", "author_profiles"),
        ("site_name", "site_name"),
    ];
    if let Some(saved) = value.as_object_mut()
        && !saved.contains_key("facebook")
    {
        let facebook: serde_json::Map<_, _> =
            MOVED.iter().filter_map(|(old, new)| Some(((*new).to_string(), saved.remove(*old)?))).collect();
        saved.insert("facebook".into(), facebook.into());
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_pass_the_check() {
        Settings::default().check().unwrap();
    }

    #[test]
    fn a_partial_section_keeps_its_sites_defaults() {
        let loaded =
            Settings::from_value(serde_json::json!({ "instagram": { "proxy": "socks5://127.0.0.1:1" } })).unwrap();
        assert_eq!(loaded.instagram.proxy.as_deref(), Some("socks5://127.0.0.1:1"));
        assert_eq!(loaded.instagram.fetches_per_minute, Settings::default().instagram.fetches_per_minute);
        assert!(!loaded.instagram.author_profiles);
    }

    #[test]
    fn check_rejects_mistakes() {
        let with = |f: fn(&mut Settings)| {
            let mut s = Settings::default();
            f(&mut s);
            s.check()
        };
        assert!(with(|s| s.facebook.proxy = Some("127.0.0.1:40000".into())).is_err());
        assert!(with(|s| s.instagram.proxy = Some("ftp://x".into())).is_err());
        assert!(with(|s| s.instagram.proxy = Some("socks5://127.0.0.1:40000".into())).is_ok());
        assert!(with(|s| s.facebook.concurrency = 0).is_err());
        assert!(with(|s| s.instagram.fetches_per_minute = 0).is_err());
        assert!(with(|s| s.reddit.fetches_per_minute = u32::MAX).is_err(), "would overflow a test run's share");
        assert!(with(|s| s.threads.concurrency = 101).is_err());
        assert!(with(|s| s.ip_requests_per_minute = 100_001).is_err());
        assert!(with(|s| s.description_length = 4_097).is_err());
        assert!(with(|s| s.cache_megabytes = 65_537).is_err());
        assert!(with(|s| (s.video_megabytes, s.video_space_megabytes) = (101, 200)).is_err());
        assert!(with(|s| s.extract_timeout_seconds = 30).is_err());
        assert!(with(|s| s.video_space_megabytes = 10).is_err(), "smaller than one video");
        assert!(with(|s| s.video_megabytes = u64::MAX).is_err(), "would overflow in bytes");
        assert!(with(|s| s.cache_minutes = 1_000_000).is_err());
        assert!(with(|s| s.video_days = 0).is_err());
        assert!(with(|s| s.video_days = 31).is_err());
        assert!(with(|s| s.video_days = 30).is_ok());
        assert!(with(|s| s.test_minutes = 14).is_err());
        assert!(with(|s| s.test_minutes = 1_441).is_err());
        assert!(with(|s| s.test_minutes = 1_440).is_ok());
        assert!(with(|s| s.test_links = vec!["https://example.com/a".into()]).is_err());
        let both = vec!["https://www.facebook.com/reel/1".into(), "https://www.instagram.com/p/DfSi56IFwZK/".into()];
        assert!(Settings { test_links: both, ..Settings::default() }.check().is_ok());
    }

    #[test]
    fn site_names_default_to_the_sites() {
        let mut s = Settings::default();
        assert_eq!((s.site_name(Source::Facebook), s.site_name(Source::Instagram)), ("Facebook", "Instagram"));
        s.instagram.site_name = "IG".into();
        assert_eq!(s.site_name(Source::Instagram), "IG");
    }

    #[tokio::test]
    async fn older_saved_settings_load() {
        let db = Db::memory().await.unwrap();
        let old = serde_json::json!({
            "site_name": "FB", "proxy": "socks5://127.0.0.1:40000", "facebook_concurrency": 2,
            "author_pictures": false, "video_cache_megabytes": 300, "video_storage": "server"
        });
        db.set_json(DB_KEY, &old).await.unwrap();
        let s = LiveSettings::load(db).await.unwrap().get();
        assert_eq!(s.facebook.site_name, "FB");
        assert_eq!(s.facebook.proxy.as_deref(), Some("socks5://127.0.0.1:40000"));
        assert_eq!((s.facebook.concurrency, s.facebook.fetches_per_minute, s.facebook.author_profiles), (2, 60, false));
        assert_eq!(s.instagram, Settings::default().instagram, "Instagram starts from its defaults");
        assert_eq!(s.cache_minutes, 60);
        assert_eq!(s.video_space_megabytes, 300, "the old name still loads");
        assert_eq!(s.test_minutes, 60, "test links saved before it had a setting");
    }

    #[tokio::test]
    async fn saved_and_reloaded() {
        let db = Db::memory().await.unwrap();
        let live = LiveSettings::load(db.clone()).await.unwrap();
        let mut s = Settings::default();
        s.instagram.site_name = "IG".into();
        live.set(s.clone()).await.unwrap();
        assert_eq!(live.get().instagram.site_name, "IG");
        assert_eq!(*LiveSettings::load(db.clone()).await.unwrap().get(), s);

        // Settings this version can't read: the defaults apply, and the saved ones are kept.
        let unreadable =
            serde_json::json!({ "cache_minutes": "an hour", "test_links": ["https://www.facebook.com/reel/1"] });
        db.set_json(DB_KEY, &unreadable).await.unwrap();
        assert_eq!(*LiveSettings::load(db.clone()).await.unwrap().get(), Settings::default());
        assert_eq!(db.get_json::<serde_json::Value>(UNREADABLE_KEY).await.unwrap(), Some(unreadable));
    }

    #[tokio::test]
    async fn changes_start_from_the_last_saved_and_old_limits_are_fixed() {
        let db = Db::memory().await.unwrap();
        let live = LiveSettings::load(db.clone()).await.unwrap();
        // Two pages' changes, one after the other, both stay.
        live.edit(|s| s.cache_minutes = 30).await.unwrap();
        live.edit(|s| s.video_days = 7).await.unwrap();
        assert_eq!((live.get().cache_minutes, live.get().video_days), (30, 7));
        // A change the check refuses leaves the saved ones as they were.
        assert!(live.edit(|s| s.video_days = 0).await.is_err());
        assert_eq!(live.get().video_days, 7);

        // Saved before a limit was tightened: brought within it on load, the rest kept.
        let mut old = serde_json::to_value(Settings::default()).unwrap();
        old["extract_timeout_seconds"] = 30.into();
        old["cache_minutes"] = 45.into();
        old["facebook"]["fetches_per_minute"] = 0.into();
        db.set_json(DB_KEY, &old).await.unwrap();
        let loaded = LiveSettings::load(db.clone()).await.unwrap().get();
        assert_eq!((loaded.extract_timeout_seconds, loaded.cache_minutes), (25, 45));
        assert_eq!(loaded.facebook.fetches_per_minute, Settings::default().facebook.fetches_per_minute);
        loaded.check().unwrap();
    }
}
