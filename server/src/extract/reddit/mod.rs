//! Reddit (`docs/reddit.md`): posts, galleries, videos, crossposts, comments, users and
//! communities, from the JSON Reddit's own site reads, which it answers only to a logged-in
//! session. Without cookies, the tags Reddit gives link-preview crawlers make a card, and a post's
//! or a comment's embed page adds its real media, author, date and text. Reddit's video files have
//! no sound, so a video's comes from its embed page, which has files with it, or from its streams,
//! joined like Facebook's 1080p.

pub mod parse;
pub mod url;

use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::header;
use serde_json::Value;
use tracing::{debug, info};

use crate::cookies::CookieStore;
use crate::extract::page::{self, Page};
use crate::extract::{
    BoxFuture, CookieChoice, CookiePlan, ExtractError, ExtractResult, Extractor, Media, PageType, Post, PostKind,
    Source, Stage, Stats, Target, Timings, network_error, proxy_error,
};
use crate::net::ProxyClients;
use crate::settings::Settings;
use url::{Link, ORIGIN};

/// How Reddit's API rules ask a program to name itself; its redirects and embed pages answer
/// that without a challenge, which a browser's user agent gets.
const BOT: &str = "server:homura:v1 (link previews)";
/// Reddit's page for link-preview crawlers.
const CRAWLER: &str = "facebookexternalhit/1.1";
const EMBED: &str = "https://embed.reddit.com";
/// Answers bigger than this are refused; a post's JSON with its comments' IDs is about 200 KB, an
/// embed page 350 KB.
const MAX_BYTES: usize = 4 * 1024 * 1024;
/// The packaged video files are signed for a few hours; the cache drops the post before then.
const PACKAGED_HOST: &str = "https://packaged-media.redd.it/";

/// An author's picture and bio.
type About = Arc<(Option<String>, Option<String>)>;

pub struct Reddit {
    clients: ProxyClients,
    cookies: Arc<CookieStore>,
    /// Authors' pictures and bios by name: one more request per author at most every six hours.
    authors: moka::future::Cache<String, Option<About>>,
}

impl Reddit {
    pub fn new(cookies: Arc<CookieStore>) -> anyhow::Result<Self> {
        let authors = crate::extract::profile_cache();
        Ok(Self { clients: ProxyClients::new(build_client)?, cookies, authors })
    }

    /// An author's picture and bio, from their `about.json` with the session; remembered for six
    /// hours, none included. One that fails isn't remembered.
    async fn author(&self, client: &reqwest::Client, name: &str, timings: &mut Timings) -> Option<About> {
        if let Some(known) = self.authors.get(name).await {
            return known;
        }
        // The name comes from the post's data and goes into a path sent with the session.
        if !url::is_name(name) {
            return None;
        }
        let session = self.cookies.value("reddit_session")?;
        let request = client
            .get(format!("{ORIGIN}/user/{name}/about.json?raw_json=1"))
            .header(header::COOKIE, format!("reddit_session={session}"))
            .header(header::ACCEPT, "application/json");
        let about = match json(request, timings).await {
            Ok(response) => parse::author(&response).map(Arc::new),
            Err(e) => {
                debug!(name, "no profile for the author: {e}");
                return None;
            }
        };
        self.authors.insert(name.to_string(), about.clone()).await;
        about
    }

    async fn run(&self, target: &Target, settings: &Settings, choice: CookieChoice) -> ExtractResult {
        let s = &settings.reddit;
        let client = self.clients.get(s.proxy.as_deref()).map_err(|e| proxy_error(&e))?;
        let mut timings = Timings::default();
        // The post is cached under the link asked for.
        let key = target.key().to_string();
        let followed;
        let target = match url::link(target) {
            Link::Share | Link::Video(_) => {
                followed = follow(&client, target, &mut timings).await?;
                &followed
            }
            _ => target,
        };
        let link = url::link(target);
        if let Link::Image(_) = link {
            let mut post = picture_post(target);
            post.key = key;
            return Ok((post, timings));
        }
        let jar = self.cookies.header().filter(|_| uses_cookies(&link));
        let Some(CookiePlan { first: first_with_cookies, retry: retry_with_cookies }) =
            CookiePlan::of(choice, s.cookie_policy, jar.is_some(), false)
        else {
            return Err(ExtractError::new(
                Stage::LoginWall,
                "no usable Reddit cookies are saved, or this page never uses them",
            ));
        };
        // Whether the post was read with the session: only then may its author's be asked with it.
        let mut with_session = first_with_cookies && jar.is_some();
        let first = match (&jar, first_with_cookies) {
            (Some(jar), true) => self.logged_in(&client, target, &link, jar, settings, &mut timings).await,
            _ => crawler(&client, target, settings, &mut timings).await,
        };
        let result = match (first, &jar) {
            (Err(e), Some(jar)) if retry_with_cookies && matches!(e.stage, Stage::LoginWall | Stage::NotFound) => {
                debug!(url = %target.key(), "retrying with cookies after: {e}");
                with_session = true;
                self.logged_in(&client, target, &link, jar, settings, &mut timings).await
            }
            // Refused with cookies: the crawlers' card still says what it is.
            (Err(e), _)
                if first_with_cookies
                    && choice == CookieChoice::Policy
                    && matches!(e.stage, Stage::LoginWall | Stage::NotFound) =>
            {
                debug!(url = %target.key(), "reading the crawlers' card after: {e}");
                with_session = false;
                crawler(&client, target, settings, &mut timings).await.map_err(|_| e)
            }
            (other, _) => other,
        };
        let mut post = result?;
        if s.author_profiles
            && with_session
            && post.page_type != PageType::Profile
            && let Some(name) = post.author.as_deref().and_then(|a| a.strip_prefix("u/")).map(str::to_string)
            && let Some(about) = self.author(&client, &name, &mut timings).await
        {
            (post.profile.picture, post.profile.bio) = (*about).clone();
        }
        post.key = key;
        Ok((post, timings))
    }

    /// With the saved session: the post's or the comment's JSON, or a user's or community's
    /// `about.json`. Only the session cookie goes, with the client's one browser user agent.
    async fn logged_in(
        &self,
        client: &reqwest::Client,
        target: &Target,
        link: &Link,
        jar: &str,
        settings: &Settings,
        timings: &mut Timings,
    ) -> Result<Post, ExtractError> {
        let session = self.cookies.value("reddit_session").unwrap_or_default();
        let get = |path: String| {
            client
                .get(format!("{ORIGIN}{path}"))
                .header(header::COOKIE, format!("reddit_session={session}"))
                .header(header::ACCEPT, "application/json")
        };
        // Comments' IDs come with a post, as many as there are; one level of one is enough.
        let result = match link {
            Link::Post(id) => match json(get(format!("/comments/{id}.json?raw_json=1&limit=1&depth=1")), timings).await
            {
                Ok(response) => self.with_sound(client, parse::post(&response, target, None), settings, timings).await,
                Err(e) => Err(e),
            },
            Link::Comment(id, comment) => {
                let path = format!("/comments/{id}/comment/{comment}.json?raw_json=1&limit=1&depth=1");
                match json(get(path), timings).await {
                    Ok(response) => {
                        let parsed = parse::post(&response, target, Some(comment));
                        self.with_sound(client, parsed, settings, timings).await
                    }
                    Err(e) => Err(e),
                }
            }
            Link::User(name) => json(get(format!("/user/{name}/about.json?raw_json=1")), timings)
                .await
                .and_then(|response| parse::person(&response, target)),
            Link::Community(name) => json(get(format!("/r/{name}/about.json?raw_json=1")), timings)
                .await
                .and_then(|response| parse::community(&response, target)),
            Link::Share | Link::Video(_) | Link::Image(_) | Link::Other => {
                Err(ExtractError::new(Stage::NotFound, "this link names nothing Reddit's data has"))
            }
        };
        if let Err(e) = &result
            && e.stage == Stage::LoginWall
            && e.message.contains("login page")
            && self.cookies.mark_rejected(jar).await
        {
            tracing::warn!("Reddit rejected the saved cookies; paste fresh ones in the dashboard");
        }
        result
    }

    /// A video post's sound: its embed page's files have it; else its streams, which the server
    /// joins when 1080p videos are on.
    async fn with_sound(
        &self,
        client: &reqwest::Client,
        parsed: Result<(Post, Option<parse::Video>), ExtractError>,
        settings: &Settings,
        timings: &mut Timings,
    ) -> Result<Post, ExtractError> {
        let (mut post, video) = parsed?;
        let Some(video) = video.filter(|v| v.has_audio) else { return Ok(post) };
        let Some(media) = post.media.first_mut() else { return Ok(post) };
        let files = match &video.permalink {
            Some(permalink) => packaged(client, permalink, timings).await,
            None => Ok(Vec::new()),
        };
        match files {
            Ok(files) if !files.is_empty() => {
                with_files(media, &files, video.duration_seconds, settings.video_megabytes * 1024 * 1024);
                return Ok(post);
            }
            Ok(_) => debug!(url = %post.key, "the embed page had no video files with sound"),
            Err(e) => debug!(url = %post.key, "no embed page for the sound: {e}"),
        }
        if let Some(playlist) = &video.playlist {
            match send(client.get(playlist).header(header::USER_AGENT, BOT), timings).await {
                Ok(body) => {
                    let xml = String::from_utf8_lossy(&body);
                    media.hd = crate::extract::facebook::dash::hd_from_manifest(&xml, 0).map(|mut hd| {
                        let base = format!("https://v.redd.it/{}/", video.id);
                        hd.video = resolve(&base, &hd.video);
                        hd.audio = hd.audio.map(|audio| resolve(&base, &audio));
                        hd
                    });
                }
                Err(e) => debug!(url = %post.key, "no playlist for the sound: {e}"),
            }
        }
        if media.hd.is_none() {
            info!(url = %post.key, "a Reddit video without its sound: neither its embed page nor its streams could be read");
        }
        Ok(post)
    }

    /// What extraction reads for a target logged out, as a test fixture: Reddit's tags for
    /// crawlers. Logged-in answers aren't recorded.
    pub async fn record(&self, target: &Target, settings: &Settings, with_cookies: bool) -> anyhow::Result<String> {
        anyhow::ensure!(!with_cookies, "Reddit fixtures are recorded logged out only");
        let client = self.clients.get(settings.reddit.proxy.as_deref()).map_err(|e| proxy_error(&e))?;
        let page = crawler_page(&client, target.url.as_str(), &mut Timings::default()).await?;
        let mut fixture = serde_json::Map::new();
        fixture.insert("url".into(), target.original.as_str().into());
        fixture.insert("crawler".into(), serde_json::to_value(page.meta)?);
        Ok(serde_json::to_string_pretty(&Value::Object(fixture))? + "\n")
    }
}

impl Extractor for Reddit {
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
    let tags: std::collections::HashMap<String, String> = saved
        .get("crawler")
        .and_then(|t| serde_json::from_value(t.clone()).ok())
        .ok_or_else(|| ExtractError::new(Stage::Fetch, "the fixture has no answer for this"))?;
    let meta = page::META.iter().filter_map(|k| Some((*k, tags.get(*k)?.clone()))).collect();
    parse::crawler_post(&Page { scripts: Vec::new(), meta }, target)
}

/// Whether the session may be used: Reddit's data, never its other pages.
const fn uses_cookies(link: &Link) -> bool {
    matches!(link, Link::Post(_) | Link::Comment(..) | Link::User(_) | Link::Community(_))
}

/// A picture on `i.redd.it` on its own: there's no post to read, so nothing is fetched.
fn picture_post(target: &Target) -> Post {
    Post {
        key: target.key().to_string(),
        url: target.url.to_string(),
        page_type: PageType::Photo,
        title: None,
        author: None,
        profile: crate::extract::AuthorProfile::default(),
        group: None,
        published_at: None,
        text: None,
        media: vec![Media::photo(target.url.to_string())],
        kind: PostKind::Media,
        stats: Stats::default(),
        shared: None,
        sensitive: false,
    }
}

/// Share links and `v.redd.it` addresses redirect to the post they name: once, or twice by way of
/// `/video/{id}`. Logged out, as the redirects need no login.
async fn follow(client: &reqwest::Client, target: &Target, timings: &mut Timings) -> Result<Target, ExtractError> {
    let mut at = target.url.clone();
    for _ in 0..3 {
        let started = Instant::now();
        let response = client.get(at.as_str()).header(header::USER_AGENT, BOT).send().await.map_err(network_error)?;
        timings.fetch += started.elapsed();
        timings.requests += 1;
        let location = response.headers().get(header::LOCATION).and_then(|v| v.to_str().ok()).unwrap_or_default();
        if response.status().as_u16() == 429 {
            return Err(ExtractError::new(Stage::RateLimited, "Reddit answered 429 Too Many Requests"));
        }
        let Some(next) = at.join(location).ok().filter(|_| response.status().is_redirection()) else { break };
        if let Some(found) = Target::from_link(next.as_str())
            .filter(|t| t.source == Source::Reddit && matches!(url::link(t), Link::Post(_) | Link::Comment(..)))
        {
            return Ok(found);
        }
        // `v.redd.it/{id}` goes by way of `www.reddit.com/video/{id}`.
        let on_reddit = next.host_str().is_some_and(|h| h == "www.reddit.com" || h == "v.redd.it");
        if !on_reddit {
            break;
        }
        at = next;
    }
    Err(ExtractError::new(Stage::Redirect, "the link didn't lead to a Reddit post"))
}

/// The card from Reddit's tags for crawlers, logged out, with what a post's or a comment's embed
/// page adds. The card stands on its own when that page can't be read.
async fn crawler(
    client: &reqwest::Client,
    target: &Target,
    settings: &Settings,
    timings: &mut Timings,
) -> Result<Post, ExtractError> {
    let page = crawler_page(client, target.url.as_str(), timings).await?;
    let mut post = parse::crawler_post(&page, target)?;
    let thing = match url::link(target) {
        Link::Post(id) => Some(format!("t3_{id}")),
        Link::Comment(_, id) => Some(format!("t1_{id}")),
        _ => None,
    };
    if let Some(thing) = thing {
        match embed_page(client, &post.url, timings).await {
            Ok(html) => match parse::embed(&html, &thing) {
                Some(embed) => with_embed(&mut post, embed, &html, settings.video_megabytes * 1024 * 1024),
                None => debug!(url = %post.key, "the embed page didn't show this post"),
            },
            Err(e) => debug!(url = %post.key, "no embed page for the card: {e}"),
        }
    }
    Ok(post)
}

/// The crawlers' card with the post's real media in place of the picture Reddit draws of it, its
/// author and date, and a text post's or a comment's whole text.
fn with_embed(post: &mut Post, embed: parse::Embed, html: &str, limit: u64) {
    if let Some(author) = embed.author {
        post.profile = parse::profile(Some(&author));
        post.author = Some(format!("u/{author}"));
    }
    post.published_at = embed.published_at.or(post.published_at);
    post.stats.reactions = embed.score.or(post.stats.reactions);
    // The card's title may say so already.
    if embed.nsfw && !post.sensitive {
        post.title = post.title.take().map(|title| format!("NSFW · {title}"));
        post.sensitive = true;
    }
    let files = packaged_files(html);
    let media = match embed.kind.as_str() {
        "image" => embed.file.filter(|f| f.starts_with("https://i.redd.it/")).map(|f| vec![Media::photo(f)]),
        "gallery" if !embed.gallery.is_empty() => Some(embed.gallery.into_iter().map(Media::photo).collect()),
        "video" if !files.is_empty() => {
            let mut video = Media::video(String::new(), embed.poster);
            with_files(&mut video, &files, packaged_seconds(html), limit);
            Some(vec![video])
        }
        "gif" => embed.clip.map(|clip| vec![Media::video(clip, embed.poster)]),
        // Reddit's picture of a headline alone, or of words the card has, only repeats them.
        "text" => Some(Vec::new()),
        "multi_media" | "comment" if embed.text.is_some() => Some(Vec::new()),
        _ => None,
    };
    if let Some(media) = media {
        post.kind = if media.is_empty() { PostKind::TextOnly } else { PostKind::Media };
        post.media = media;
    }
    if embed.text.is_some() {
        post.text = embed.text;
    }
    if let Some(link) = embed.link
        && !post.text.as_deref().is_some_and(|text| text.contains(&link))
    {
        post.text = Some(match post.text.take() {
            Some(text) => format!("{text}\n\n{link}"),
            None => link,
        });
    }
}

/// A page as Reddit serves crawlers, after the redirect a link without its title's slug gets.
async fn crawler_page(client: &reqwest::Client, url: &str, timings: &mut Timings) -> Result<Page, ExtractError> {
    let mut at = reqwest::Url::parse(url).map_err(|e| ExtractError::new(Stage::Fetch, e.to_string()))?;
    let mut redirects = 0;
    let body = loop {
        let started = Instant::now();
        timings.requests += 1;
        let response =
            client.get(at.as_str()).header(header::USER_AGENT, CRAWLER).send().await.map_err(network_error)?;
        let location = response.headers().get(header::LOCATION).and_then(|v| v.to_str().ok()).unwrap_or_default();
        match at.join(location) {
            Ok(next)
                if response.status().is_redirection() && next.host_str() == Some("www.reddit.com") && redirects < 2 =>
            {
                timings.fetch += started.elapsed();
                redirects += 1;
                at = next;
            }
            _ => break read(response, started, timings).await?,
        }
    };
    let started = Instant::now();
    let page = page::parse(&body).map_err(|e| ExtractError::new(Stage::Fetch, e.to_string()));
    timings.parse += started.elapsed();
    page
}

/// A packaged video file: its address, size and bitrate.
struct Packaged {
    url: String,
    width: u32,
    height: u32,
    bits_per_second: u64,
}

/// The video files with sound on the post's embed page, asked for logged out.
async fn packaged(
    client: &reqwest::Client,
    permalink: &str,
    timings: &mut Timings,
) -> Result<Vec<Packaged>, ExtractError> {
    Ok(packaged_files(&embed_page(client, permalink, timings).await?))
}

/// A post's embed page, asked for logged out.
async fn embed_page(client: &reqwest::Client, permalink: &str, timings: &mut Timings) -> Result<String, ExtractError> {
    let path = permalink.strip_prefix(ORIGIN).ok_or_else(|| ExtractError::new(Stage::NotFound, "no permalink"))?;
    // Set as a path, so whatever the post's data holds stays on the embed host.
    let mut embed = ::url::Url::parse(EMBED).expect("valid origin");
    embed.set_path(path);
    embed.set_query(Some("embed=true"));
    let request = client.get(embed).header(header::USER_AGENT, BOT);
    Ok(crate::extract::body_text(send(request, timings).await?))
}

/// What the embed page's player says of its files (`packaged-media-json`).
fn packaged_json(html: &str) -> Option<Value> {
    let start = html.find("packaged-media-json=\"")? + "packaged-media-json=\"".len();
    let end = html[start..].find('"')?;
    serde_json::from_str(&page::decode_entities(&html[start..start + end])).ok()
}

/// How long the embed page's video is.
fn packaged_seconds(html: &str) -> Option<u64> {
    packaged_json(html)?.pointer("/playbackMp4s/duration")?.as_u64()
}

fn packaged_files(html: &str) -> Vec<Packaged> {
    let Some(value) = packaged_json(html) else { return Vec::new() };
    let permutations =
        value.pointer("/playbackMp4s/permutations").and_then(Value::as_array).cloned().unwrap_or_default();
    permutations
        .iter()
        .filter_map(|p| {
            let source = p.get("source")?;
            let url = source.get("url")?.as_str()?;
            url.starts_with(PACKAGED_HOST).then(|| Packaged {
                url: url.to_string(),
                width: source
                    .pointer("/dimensions/width")
                    .and_then(Value::as_u64)
                    .and_then(|n| u32::try_from(n).ok())
                    .unwrap_or(0),
                height: source
                    .pointer("/dimensions/height")
                    .and_then(Value::as_u64)
                    .and_then(|n| u32::try_from(n).ok())
                    .unwrap_or(0),
                bits_per_second: source.get("bitrateBps").and_then(Value::as_u64).unwrap_or(0),
            })
        })
        .collect()
}

/// The video becomes its best file with sound; the next best that fits Telegram's limit is kept
/// as the smaller file, as Facebook's SD is.
fn with_files(media: &mut Media, files: &[Packaged], duration_seconds: Option<u64>, limit: u64) {
    let bytes = |f: &Packaged| duration_seconds.map(|d| f.bits_per_second.saturating_mul(d) / 8);
    let mut by_size: Vec<&Packaged> = files.iter().collect();
    by_size.sort_by_key(|f| (f.height, f.bits_per_second));
    let Some(best) = by_size.last() else { return };
    media.url.clone_from(&best.url);
    media.expires_at = crate::extract::cdn_expiry(&best.url);
    media.bytes = bytes(best);
    if best.width > 0 && best.height > 0 {
        (media.width, media.height) = (Some(best.width), Some(best.height));
    }
    media.smaller = by_size
        .iter()
        .rev()
        .skip(1)
        .find(|f| bytes(f).is_none_or(|b| b <= limit))
        .map(|f| crate::extract::SmallerFile { url: f.url.clone(), bytes: bytes(f) });
    let _ = media.fit(limit);
}

/// A playlist's file name, as an address under the video's.
fn resolve(base: &str, file: &str) -> String {
    if file.starts_with("https://") { file.to_string() } else { format!("{base}{}", file.trim_start_matches('/')) }
}

/// Sends a request and reads a JSON answer, telling Reddit's refusals apart.
async fn json(request: reqwest::RequestBuilder, timings: &mut Timings) -> Result<Value, ExtractError> {
    let body = send(request, timings).await?;
    let started = Instant::now();
    let value: Value = serde_json::from_slice(&body).map_err(|_| {
        ExtractError::new(
            Stage::LoginWall,
            "Reddit answered with its network-security page instead of data: it answers only a logged-in session",
        )
    })?;
    timings.parse += started.elapsed();
    match parse::refusal(&value) {
        Some(e) => Err(e),
        None => Ok(value),
    }
}

/// Sends a request and reads the answer, up to [`MAX_BYTES`]. Refusals with JSON are read by
/// [`json`].
async fn send(request: reqwest::RequestBuilder, timings: &mut Timings) -> Result<Vec<u8>, ExtractError> {
    let started = Instant::now();
    timings.requests += 1;
    let response = request.send().await.map_err(network_error)?;
    read(response, started, timings).await
}

/// Reads an answer [`send`] got, telling Reddit's refusals apart.
async fn read(response: reqwest::Response, started: Instant, timings: &mut Timings) -> Result<Vec<u8>, ExtractError> {
    let status = response.status();
    if status.is_redirection() {
        timings.fetch += started.elapsed();
        let location = response.headers().get(header::LOCATION).and_then(|v| v.to_str().ok()).unwrap_or_default();
        return Err(if location.contains("/login") {
            ExtractError::new(Stage::LoginWall, "Reddit sent this to its login page")
        } else {
            ExtractError::new(Stage::Redirect, format!("Reddit redirected ({status})"))
        });
    }
    if status.as_u16() == 429 {
        return Err(ExtractError::new(Stage::RateLimited, "Reddit answered 429 Too Many Requests"));
    }
    let body = crate::extract::read_body(response, MAX_BYTES, Source::Reddit).await?;
    timings.fetch += started.elapsed();
    let json = body.first() == Some(&b'{') || body.first() == Some(&b'[');
    match status.as_u16() {
        404 if !json => Err(ExtractError::new(Stage::NotFound, "Reddit has no such page")),
        401 => Err(ExtractError::new(Stage::LoginWall, "Reddit sent this to its login page")),
        403 if !json => Err(ExtractError::new(
            Stage::LoginWall,
            "Reddit answered with its network-security page: it answers only a logged-in session",
        )),
        code if status.is_success() || (json && matches!(code, 403 | 404)) => Ok(body),
        _ => Err(ExtractError::new(Stage::Fetch, format!("Reddit answered {status}"))),
    }
}

fn build_client(proxy: Option<&str>) -> anyhow::Result<reqwest::Client> {
    Ok(crate::extract::site_client(&[("accept-language", "en-US,en;q=0.5")], Duration::from_secs(10), proxy)?
        .build()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packaged_files_from_the_embed_page() {
        let json = r#"{"playbackMp4s":{"permutations":[
            {"source":{"url":"https://packaged-media.redd.it/v1/pb/m2-res_480p.mp4?e=1797500000&s=a","dimensions":{"width":270,"height":480},"bitrateBps":250000}},
            {"source":{"url":"https://packaged-media.redd.it/v1/pb/m2-res_1280p.mp4?e=1797500000&s=b","dimensions":{"width":720,"height":1280},"bitrateBps":20000000}},
            {"source":{"url":"https://elsewhere.example/x.mp4","dimensions":{"width":1,"height":1},"bitrateBps":1}}]}}"#;
        let html = format!(r#"<shreddit-player packaged-media-json="{}" post-id="t3_x">"#, json.replace('"', "&quot;"));
        let files = packaged_files(&html);
        assert_eq!(files.len(), 2, "only Reddit's own files");

        let mut media = Media::video("https://v.redd.it/v1/CMAF_720.mp4?source=fallback".into(), None);
        with_files(&mut media, &files, Some(10), 20 << 20);
        // 20 Mbit/s for 10 s is 25 MB, over the limit: the smaller file it is, with sound.
        assert_eq!(media.url, "https://packaged-media.redd.it/v1/pb/m2-res_480p.mp4?e=1797500000&s=a");
        assert_eq!(media.expires_at, Some(1_797_500_000));
        assert_eq!(media.height, Some(1280), "the shape stays the best file's");
        assert!(packaged_files("<p>nothing</p>").is_empty());
    }

    #[test]
    fn the_embed_page_replaces_the_picture_reddit_draws() {
        let card = || Post {
            key: "/comments/1abc2d3/".into(),
            url: "https://www.reddit.com/r/x/comments/1abc2d3/title/".into(),
            page_type: PageType::Post,
            title: Some("Title".into()),
            author: None,
            profile: crate::extract::AuthorProfile::default(),
            group: Some("r/x".into()),
            published_at: None,
            text: Some("The start of the text…".into()),
            media: vec![Media::photo("https://share.redd.it/preview/post/1abc2d3".into())],
            kind: PostKind::Media,
            stats: Stats::default(),
            shared: None,
            sensitive: false,
        };
        let embed = |kind: &str| parse::Embed {
            kind: kind.into(),
            file: Some("https://i.redd.it/picture.jpeg".into()),
            author: Some("someone".into()),
            published_at: Some(1_794_923_037),
            ..parse::Embed::default()
        };

        let mut post = card();
        with_embed(&mut post, embed("image"), "", 20 << 20);
        assert_eq!(post.media[0].url, "https://i.redd.it/picture.jpeg");
        assert_eq!((post.author.as_deref(), post.published_at), (Some("u/someone"), Some(1_794_923_037)));
        assert_eq!(post.profile.url.as_deref(), Some("https://www.reddit.com/user/someone/"));
        assert_eq!(post.text.as_deref(), Some("The start of the text…"), "an image's page doesn't have the text");

        let mut post = card();
        with_embed(&mut post, parse::Embed { text: Some("All of it".into()), ..embed("multi_media") }, "", 20 << 20);
        assert_eq!((post.kind, post.media.len(), post.text.as_deref()), (PostKind::TextOnly, 0, Some("All of it")));

        // Nothing to show for it: the card stays as it was.
        let mut post = card();
        with_embed(&mut post, embed("video"), "", 20 << 20);
        assert_eq!(post.media[0].url, "https://share.redd.it/preview/post/1abc2d3");
        let mut post = card();
        with_embed(&mut post, parse::Embed { link: Some("https://youtu.be/x".into()), ..embed("link") }, "", 20 << 20);
        assert_eq!(post.media[0].url, "https://share.redd.it/preview/post/1abc2d3", "a link's preview, drawn");
        assert_eq!(post.text.as_deref(), Some("The start of the text…\n\nhttps://youtu.be/x"), "and where it leads");

        let mut post = card();
        let clip = "https://preview.redd.it/a.gif?format=mp4&s=1";
        with_embed(&mut post, parse::Embed { clip: Some(clip.into()), ..embed("gif") }, "", 20 << 20);
        assert_eq!((post.media[0].kind, post.media[0].url.as_str()), (crate::extract::MediaKind::Video, clip));

        let mut post = card();
        with_embed(&mut post, embed("text"), "", 20 << 20);
        assert_eq!((post.kind, post.media.len()), (PostKind::TextOnly, 0), "not Reddit's picture of the headline");

        // A comment's score and words; the card already says it's NSFW.
        let mut post = Post { title: Some("Re: NSFW · Title".into()), sensitive: true, ..card() };
        let comment =
            parse::Embed { score: Some(5399), text: Some("All of it".into()), nsfw: true, ..embed("comment") };
        with_embed(&mut post, comment, "", 20 << 20);
        assert_eq!((post.title.as_deref(), post.stats.reactions), (Some("Re: NSFW · Title"), Some(5399)));
        assert_eq!((post.kind, post.text.as_deref()), (PostKind::TextOnly, Some("All of it")));
    }

    #[test]
    fn playlist_files_under_the_video() {
        assert_eq!(resolve("https://v.redd.it/abc/", "CMAF_720.mp4"), "https://v.redd.it/abc/CMAF_720.mp4");
        assert_eq!(resolve("https://v.redd.it/abc/", "https://v.redd.it/abc/x.mp4"), "https://v.redd.it/abc/x.mp4");
    }
}
