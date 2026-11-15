//! Live settings: stored in SQLite, edited in the dashboard, swapped in place without a restart.

use std::sync::Arc;

use anyhow::{Result, bail};
use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};

use crate::db::Db;

const DB_KEY: &str = "settings";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Outgoing proxy for Facebook, e.g. `socks5://127.0.0.1:40000`. `None` goes direct.
    pub proxy: Option<String>,
    pub cookie_policy: CookiePolicy,
    pub cache_minutes: u64,
    pub failure_cache_minutes: u64,
    pub cache_megabytes: u64,
    /// Requests per IP per minute on the public listener; Telegram is exempt.
    pub ip_requests_per_minute: u32,
    pub facebook_fetches_per_minute: u32,
    pub facebook_concurrency: u32,
    /// Overall time for one extraction; keep it under how long Telegram waits for a page.
    pub extract_timeout_seconds: u64,
    pub instant_view: InstantView,
    /// `og:description` is cut to this many characters.
    pub description_length: usize,
    pub site_name: String,
    pub failure_response: FailureResponse,
    pub media_links: MediaLinks,
    pub test_links: Vec<String>,
    pub alert_chat: Option<i64>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            proxy: None,
            cookie_policy: CookiePolicy::Fallback,
            cache_minutes: 60,
            failure_cache_minutes: 5,
            cache_megabytes: 64,
            ip_requests_per_minute: 120,
            facebook_fetches_per_minute: 60,
            facebook_concurrency: 4,
            extract_timeout_seconds: 8,
            instant_view: InstantView::default(),
            description_length: 300,
            site_name: "Facebook".into(),
            failure_response: FailureResponse::Redirect,
            media_links: MediaLinks::Direct,
            test_links: Vec::new(),
            alert_chat: None,
        }
    }
}

/// When to send the Facebook login cookies.
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
    /// `302` to the post, so Telegram shows Facebook's own preview.
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
    /// Facebook's CDN URLs as they are.
    Direct,
    /// `/_/m/...` links that redirect to a fresh CDN URL.
    Redirect,
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
    pub fn check(&self) -> Result<()> {
        if let Some(proxy) = &self.proxy {
            let url = url::Url::parse(proxy).map_err(|_| anyhow::anyhow!("the proxy must be a URL"))?;
            if !matches!(url.scheme(), "socks5" | "socks5h" | "http" | "https") {
                bail!("the proxy must start with socks5://, socks5h://, http:// or https://");
            }
        }
        let positive = [
            ("cache minutes", self.cache_minutes),
            ("failure cache minutes", self.failure_cache_minutes),
            ("cache size", self.cache_megabytes),
            ("requests per IP", self.ip_requests_per_minute.into()),
            ("Facebook fetches per minute", self.facebook_fetches_per_minute.into()),
            ("Facebook concurrency", self.facebook_concurrency.into()),
            ("extraction timeout", self.extract_timeout_seconds),
        ];
        for (name, value) in positive {
            if value == 0 {
                bail!("{name} must be at least 1");
            }
        }
        if self.extract_timeout_seconds > 60 {
            bail!("the extraction timeout can be at most 60 seconds");
        }
        if self.description_length < 20 {
            bail!("the description length must be at least 20");
        }
        if self.site_name.trim().is_empty() {
            bail!("the site name can't be empty");
        }
        for link in &self.test_links {
            if crate::extract::facebook::url::Target::from_link(link).is_none() {
                bail!("{link:?} isn't a facebook.com link");
            }
        }
        Ok(())
    }
}

pub struct LiveSettings {
    current: ArcSwap<Settings>,
    db: Db,
}

impl LiveSettings {
    pub async fn load(db: Db) -> Result<Self> {
        let settings = match db.get_json::<Settings>(DB_KEY).await {
            Ok(Some(settings)) => settings,
            Ok(None) => Settings::default(),
            Err(e) => {
                tracing::error!("stored settings are unreadable, using defaults: {e:#}");
                Settings::default()
            }
        };
        Ok(Self { current: ArcSwap::from_pointee(settings), db })
    }

    pub fn get(&self) -> Arc<Settings> {
        self.current.load_full()
    }

    /// Checks, saves and applies new settings.
    pub async fn set(&self, settings: Settings) -> Result<()> {
        settings.check()?;
        self.db.set_json(DB_KEY, &settings).await?;
        self.current.store(Arc::new(settings));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_pass_the_check() {
        Settings::default().check().unwrap();
    }

    #[test]
    fn check_rejects_mistakes() {
        let with = |f: fn(&mut Settings)| {
            let mut s = Settings::default();
            f(&mut s);
            s.check()
        };
        assert!(with(|s| s.proxy = Some("127.0.0.1:40000".into())).is_err());
        assert!(with(|s| s.proxy = Some("ftp://x".into())).is_err());
        assert!(with(|s| s.proxy = Some("socks5://127.0.0.1:40000".into())).is_ok());
        assert!(with(|s| s.facebook_concurrency = 0).is_err());
        assert!(with(|s| s.extract_timeout_seconds = 120).is_err());
        assert!(with(|s| s.test_links = vec!["https://example.com/a".into()]).is_err());
        assert!(with(|s| s.test_links = vec!["https://www.facebook.com/reel/1".into()]).is_ok());
    }

    #[test]
    fn older_saved_settings_fill_in_new_fields() {
        let s: Settings = serde_json::from_str(r#"{"site_name":"FB"}"#).unwrap();
        assert_eq!(s.site_name, "FB");
        assert_eq!(s.cache_minutes, 60);
    }

    #[tokio::test]
    async fn saved_and_reloaded() {
        let db = Db::memory().await.unwrap();
        let live = LiveSettings::load(db.clone()).await.unwrap();
        let s = Settings { site_name: "FB".into(), ..Settings::default() };
        live.set(s.clone()).await.unwrap();
        assert_eq!(live.get().site_name, "FB");
        assert_eq!(*LiveSettings::load(db).await.unwrap().get(), s);
    }
}
