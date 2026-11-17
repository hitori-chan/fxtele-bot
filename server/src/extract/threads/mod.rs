//! Threads (`docs/threads.md`): posts with their photos, videos, quotes and the post they answer,
//! and profiles, read from the data a post's page carries for its own scripts. Logged out first;
//! with the saved cookies after a login wall, if the cookie policy allows. The cookies only ever
//! go to www.threads.com, with the client's one browser user agent.

pub mod parse;
pub mod url;

use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::header::{self, HeaderMap, HeaderValue};
use serde_json::Value;
use tracing::debug;

use crate::cookies::CookieStore;
use crate::extract::page::{self, Page};
use crate::extract::{
    BoxFuture, CookieChoice, ExtractError, ExtractResult, Extractor, PageType, Post, Stage, Target, Timings,
    network_error, proxy_error,
};
use crate::net::ProxyClients;
use crate::settings::{CookiePolicy, Settings};
use url::{Link, ORIGIN};

/// Threads redirects a link with the wrong name, and the old domain's, once each.
const MAX_REDIRECTS: usize = 4;
/// How long an author's picture and bio are remembered.
const PROFILE_TTL: Duration = Duration::from_secs(6 * 3600);

/// An author's full-size picture and bio.
type About = Arc<(Option<String>, Option<String>)>;

pub struct Threads {
    clients: ProxyClients,
    cookies: Arc<CookieStore>,
    /// Authors' pictures and bios by username: one more request per author at most every six
    /// hours.
    authors: moka::future::Cache<String, Option<About>>,
}

impl Threads {
    pub fn new(cookies: Arc<CookieStore>) -> anyhow::Result<Self> {
        let authors = moka::future::Cache::builder()
            .max_capacity(5000)
            .eviction_policy(moka::policy::EvictionPolicy::lru())
            .time_to_live(PROFILE_TTL)
            .build();
        Ok(Self { clients: ProxyClients::new(build_client)?, cookies, authors })
    }

    async fn run(&self, target: &Target, settings: &Settings, choice: CookieChoice) -> ExtractResult {
        let s = &settings.threads;
        let client = self.clients.get(s.proxy.as_deref()).map_err(|e| proxy_error(&e))?;
        let mut timings = Timings::default();
        // The post is cached under the link asked for.
        let key = target.key().to_string();
        let followed;
        let target = if url::link(target) == Link::Share {
            followed = follow_share(&client, target, &mut timings).await?;
            &followed
        } else {
            target
        };
        let link = url::link(target);
        // Profiles are only fetched logged out: with cookies they'd show the account's own view
        // of them to anyone on Telegram.
        let jar = self.cookies.header().filter(|_| matches!(link, Link::Post(_)));
        let (first_with_cookies, retry_with_cookies) = match (choice, s.cookie_policy) {
            (CookieChoice::Force, _) if jar.is_none() => {
                return Err(ExtractError::new(
                    Stage::LoginWall,
                    "no usable Threads cookies are saved, or this page never uses them",
                ));
            }
            (CookieChoice::Force, _) | (CookieChoice::Policy, CookiePolicy::Always) => (jar.is_some(), false),
            (CookieChoice::Policy, CookiePolicy::Never) => (false, false),
            (CookieChoice::Policy, CookiePolicy::Fallback) => (false, jar.is_some()),
        };
        let first = read(&client, target, jar.as_deref().filter(|_| first_with_cookies), &mut timings).await;
        let result = match (first, &jar) {
            (Err(e), Some(jar)) if retry_with_cookies && matches!(e.stage, Stage::LoginWall | Stage::NotFound) => {
                debug!(url = %target.key(), "retrying with cookies after: {e}");
                read(&client, target, Some(jar), &mut timings).await
            }
            (other, _) => other,
        };
        if let (Err(e), Some(jar)) = (&result, &jar)
            && (first_with_cookies || retry_with_cookies)
            && e.stage == Stage::LoginWall
            && self.cookies.mark_rejected(jar).await
        {
            tracing::warn!("Threads rejected the saved cookies; paste fresh ones in the dashboard");
        }
        let mut post = result?;
        if s.author_profiles
            && post.page_type != PageType::Profile
            && let Some(handle) = post.profile.handle.clone()
            && let Some(about) = self.author(&client, &handle, &mut timings).await
        {
            (post.profile.picture, post.profile.bio) = (*about).clone();
        }
        post.key = key;
        Ok((post, timings))
    }

    /// An author's picture and bio, from their page, logged out; remembered for six hours, none
    /// included. One that fails isn't remembered.
    async fn author(&self, client: &reqwest::Client, username: &str, timings: &mut Timings) -> Option<About> {
        if let Some(known) = self.authors.get(username).await {
            return known;
        }
        let at = ::url::Url::parse(&format!("{ORIGIN}/@{username}")).ok()?;
        let about = match fetch(client, &at, None, timings).await {
            Ok(page) => parse::about(&roots(&page), username).map(Arc::new),
            Err(e) => {
                debug!(username, "no profile for the author: {e}");
                return None;
            }
        };
        self.authors.insert(username.to_string(), about.clone()).await;
        about
    }

    /// What extraction reads for a target logged out, as a test fixture: the post with the one it
    /// answers, or the account, each cut down to what's read.
    pub async fn record(&self, target: &Target, settings: &Settings, with_cookies: bool) -> anyhow::Result<String> {
        anyhow::ensure!(!with_cookies, "Threads fixtures are recorded logged out only");
        let client = self.clients.get(settings.threads.proxy.as_deref()).map_err(|e| proxy_error(&e))?;
        anyhow::ensure!(url::link(target) != Link::Share, "record the post the share link leads to");
        let page = fetch(&client, &target.url, None, &mut Timings::default()).await?;
        read_page(&page, target)?;
        let mut saved: Vec<Value> =
            parse::fixture_parts(&roots(&page), url::link(target)).into_iter().cloned().collect();
        saved.iter_mut().for_each(scrub);
        let mut fixture = serde_json::Map::new();
        fixture.insert("url".into(), target.original.as_str().into());
        fixture.insert("scripts".into(), Value::Array(saved));
        Ok(serde_json::to_string_pretty(&Value::Object(fixture))? + "\n")
    }
}

impl Extractor for Threads {
    fn extract<'a>(
        &'a self,
        target: &'a Target,
        settings: &'a Settings,
        cookies: CookieChoice,
    ) -> BoxFuture<'a, ExtractResult> {
        Box::pin(self.run(target, settings, cookies))
    }
}

/// Reads a fixture `record` saved.
pub fn read_fixture(saved: &Value, target: &Target) -> Result<Post, ExtractError> {
    let scripts = saved
        .get("scripts")
        .and_then(Value::as_array)
        .ok_or_else(|| ExtractError::new(Stage::Fetch, "the fixture has no answer for this"))?;
    let roots: Vec<&Value> = scripts.iter().collect();
    from_roots(&roots, target)
}

/// The scripts a page kept.
fn roots(page: &Page) -> Vec<&Value> {
    page.media().collect()
}

fn read_page(page: &Page, target: &Target) -> Result<Post, ExtractError> {
    from_roots(&roots(page), target)
}

fn from_roots(roots: &[&Value], target: &Target) -> Result<Post, ExtractError> {
    match url::link(target) {
        Link::Post(code) => parse::post(roots, target, code),
        Link::Profile(user) => parse::profile(roots, target, user),
        Link::Share => Err(ExtractError::new(Stage::Redirect, "a share link names no post until it's followed")),
    }
}

/// A target's page, with cookies or without, read.
async fn read(
    client: &reqwest::Client,
    target: &Target,
    jar: Option<&str>,
    timings: &mut Timings,
) -> Result<Post, ExtractError> {
    let page = fetch(client, &target.url, jar, timings).await?;
    let started = Instant::now();
    let post = read_page(&page, target);
    timings.parse += started.elapsed();
    post
}

/// Fetches a page on www.threads.com, following Threads' redirects there, and keeps the scripts
/// that hold posts and accounts as it downloads.
async fn fetch(
    client: &reqwest::Client,
    start: &::url::Url,
    jar: Option<&str>,
    timings: &mut Timings,
) -> Result<Page, ExtractError> {
    let mut at = start.clone();
    for hop in 0..=MAX_REDIRECTS {
        let started = Instant::now();
        let mut request = client.get(at.as_str());
        if let Some(jar) = jar {
            request = request.header(header::COOKIE, jar);
        }
        let response = request.send().await.map_err(network_error)?;
        timings.requests += 1;
        let status = response.status();
        if status.is_redirection() {
            timings.fetch += started.elapsed();
            let location = response.headers().get(header::LOCATION).and_then(|v| v.to_str().ok()).unwrap_or_default();
            let next = at
                .join(location)
                .map_err(|_| ExtractError::new(Stage::Redirect, "Threads redirected to an unreadable address"))?;
            if let Some(error) = refusal(&next) {
                return Err(error);
            }
            if next.host_str() != Some("www.threads.com") || next.scheme() != "https" {
                return Err(ExtractError::new(
                    Stage::Redirect,
                    format!("redirected off Threads to {}", next.host_str().unwrap_or("?")),
                ));
            }
            if hop == MAX_REDIRECTS {
                return Err(ExtractError::new(Stage::Redirect, "Threads redirected too many times"));
            }
            at = next;
            continue;
        }
        match status.as_u16() {
            429 => return Err(ExtractError::new(Stage::RateLimited, "Threads answered 429 Too Many Requests")),
            404 => return Err(ExtractError::new(Stage::NotFound, "Threads has no such page")),
            _ if !status.is_success() => {
                return Err(ExtractError::new(Stage::Fetch, format!("Threads answered {status}")));
            }
            _ => {}
        }
        return page::read(response, parse::HINTS, started, timings).await;
    }
    unreachable!("the loop returns on its last hop")
}

/// What a redirect to Threads' login page or front page with an error says.
fn refusal(next: &::url::Url) -> Option<ExtractError> {
    let path = next.path();
    if path.starts_with("/login") || path.starts_with("/accounts/login") {
        return Some(ExtractError::new(
            Stage::LoginWall,
            "Threads sent this to its login page: the account is private or gone, or Threads wants a login",
        ));
    }
    // `/?error=invalid_post`: deleted, or a code that never was.
    next.query_pairs()
        .any(|(k, _)| k == "error")
        .then(|| ExtractError::new(Stage::NotFound, "Threads has no such post: it was deleted, or the link is wrong"))
}

/// Share links redirect to the post they name, logged out, for anything but a browser.
async fn follow_share(
    client: &reqwest::Client,
    target: &Target,
    timings: &mut Timings,
) -> Result<Target, ExtractError> {
    let mut at = target.url.clone();
    for _ in 0..MAX_REDIRECTS {
        let started = Instant::now();
        let request = client.get(at.as_str()).header(header::USER_AGENT, crate::extract::SHARE_USER_AGENT);
        let response = request.send().await.map_err(network_error)?;
        timings.fetch += started.elapsed();
        timings.requests += 1;
        if response.status().as_u16() == 429 {
            return Err(ExtractError::new(Stage::RateLimited, "Threads answered 429 Too Many Requests"));
        }
        let location = response.headers().get(header::LOCATION).and_then(|v| v.to_str().ok()).unwrap_or_default();
        let Some(next) = at.join(location).ok().filter(|_| response.status().is_redirection()) else { break };
        if let Some(error) = refusal(&next) {
            return Err(error);
        }
        if next.host_str() != Some("www.threads.com") {
            break;
        }
        let path_and_query = match next.query() {
            Some(query) => format!("{}?{query}", next.path()),
            None => next.path().to_string(),
        };
        if let Some(found) = url::request(&path_and_query).filter(|t| matches!(url::link(t), Link::Post(_))) {
            return Ok(found);
        }
        at = next;
    }
    Err(ExtractError::new(Stage::Redirect, "the share link didn't lead to a Threads post"))
}

/// Keeps only what extraction reads of a saved post or account. Anything else may name other
/// people (mentions, the thread's other replies, likers) or track (`*_token`).
fn scrub(value: &mut Value) {
    const KEPT: &[&str] = &[
        "code",
        "pk",
        "id",
        "permalink",
        "media_type",
        "taken_at",
        "user",
        "username",
        "full_name",
        "profile_pic_url",
        "is_verified",
        "caption",
        "text",
        "carousel_media",
        "image_versions2",
        "candidates",
        "url",
        "width",
        "height",
        "original_width",
        "original_height",
        "video_versions",
        "type",
        "video_dash_manifest",
        "has_audio",
        "like_count",
        "like_and_view_counts_disabled",
        "text_post_app_info",
        "direct_reply_count",
        "repost_count",
        "quote_count",
        "share_info",
        "quoted_post",
        "reply_to_post_id",
        "tag_header",
        "display_name",
        "tag_cluster_name",
        "link_preview_attachment",
        "image_url",
        "caption_add_on",
        "poll",
        "tallies",
        "count",
        "finished",
        // An account's page.
        "biography",
        "hd_profile_pic_versions",
    ];
    crate::extract::keep_keys(value, KEPT);
}

fn build_client(proxy: Option<&str>) -> anyhow::Result<reqwest::Client> {
    let mut headers = HeaderMap::new();
    for (name, value) in [
        ("accept", "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"),
        ("accept-language", "en-US,en;q=0.5"),
        ("sec-fetch-dest", "document"),
        ("sec-fetch-mode", "navigate"),
        ("sec-fetch-site", "none"),
        ("upgrade-insecure-requests", "1"),
    ] {
        headers.insert(name, HeaderValue::from_static(value));
    }
    let mut builder = crate::net::client_builder()?
        .user_agent(crate::extract::facebook::fetch::USER_AGENT)
        .default_headers(headers)
        .redirect(reqwest::redirect::Policy::none())
        .read_timeout(Duration::from_secs(10))
        .pool_idle_timeout(Duration::from_secs(300))
        .tcp_keepalive(Duration::from_secs(60));
    if let Some(proxy) = proxy {
        builder = builder.proxy(reqwest::Proxy::all(proxy)?);
    }
    Ok(builder.build()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirects_that_refuse() {
        let at = |u: &str| ::url::Url::parse(u).unwrap();
        assert_eq!(refusal(&at("https://www.threads.com/?error=invalid_post")).unwrap().stage, Stage::NotFound);
        assert_eq!(refusal(&at("https://www.threads.com/login/?next=%2F%40x")).unwrap().stage, Stage::LoginWall);
        assert!(refusal(&at("https://www.threads.com/@zuck/post/DfaNgswkQPG")).is_none());
    }
}
