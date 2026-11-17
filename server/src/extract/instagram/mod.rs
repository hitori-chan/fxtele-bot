//! Instagram (`docs/instagram.md`): posts, reels, carousels, stories and profiles, read from the
//! JSON Instagram's own web app gets. Logged out first, through the query its post pages make,
//! with the embed page racing it after a moment; then the logged-in API, if the cookie policy
//! allows; then the tags Instagram gives link-preview crawlers, which survive most gates and make
//! a one-picture preview.

pub mod parse;
pub mod url;

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use reqwest::header::{self, HeaderMap, HeaderValue};
use serde_json::Value;
use tracing::{debug, info};

use crate::cookies::CookieStore;
use crate::extract::page::{self, Page};
use crate::extract::{
    BoxFuture, CookieChoice, ExtractError, ExtractResult, Extractor, PageType, Post, Source, Stage, Target, Timings,
    network_error, proxy_error,
};
use crate::net::ProxyClients;
use crate::settings::{CookiePolicy, Settings};
use url::Link;

/// The queries Instagram's logged-out web app makes; it retires their IDs now and then, which
/// shows up as test links failing at `fetch` (`docs/instagram.md` says how to find new ones).
const POST_QUERY: Query =
    Query { url: "https://i.instagram.com/graphql/query", name: "PolarisPostRootQuery", id: "27128499623469141" };
const PROFILE_QUERY: Query = Query {
    url: "https://www.instagram.com/api/graphql/",
    name: "PolarisLoggedOutDesktopWWWProfileRootContentQuery",
    id: "27981003384861049",
};
/// Instagram's web app, which its API wants named.
const APP_ID: &str = "936619743392459";
/// Instagram's own pages for link-preview crawlers: the embed page and the preview tags.
const CRAWLER: &str = "facebookexternalhit/1.1";
/// How long the query has before the embed page is asked as well.
const HEDGE: Duration = Duration::from_millis(800);
/// Answers bigger than this are refused; a highlight's page is over a megabyte.
const MAX_BYTES: usize = 8 * 1024 * 1024;
const PROFILE_TTL: Duration = Duration::from_secs(6 * 3600);
/// How long a post that showed only with cookies is asked for with them first.
const LOGIN_ONLY_TTL: Duration = Duration::from_secs(24 * 3600);

struct Query {
    url: &'static str,
    name: &'static str,
    id: &'static str,
}

pub struct Instagram {
    clients: ProxyClients,
    cookies: Arc<CookieStore>,
    /// Authors' bios by username: one more request per author at most every six hours.
    bios: moka::future::Cache<String, Option<Arc<str>>>,
    /// Posts that showed only with cookies (age-restricted, say), by cache key. Under the
    /// `fallback` policy they're asked for with cookies first: the logged-out query would only
    /// say "not shown" again, and while it's rate limited they'd fail for nothing.
    login_only: moka::future::Cache<String, ()>,
}

impl Instagram {
    pub fn new(cookies: Arc<CookieStore>) -> anyhow::Result<Self> {
        let bios = moka::future::Cache::builder()
            .max_capacity(5000)
            .eviction_policy(moka::policy::EvictionPolicy::lru())
            .time_to_live(PROFILE_TTL)
            .build();
        let login_only = moka::future::Cache::builder().max_capacity(5000).time_to_live(LOGIN_ONLY_TTL).build();
        Ok(Self { clients: ProxyClients::new(build_client)?, cookies, bios, login_only })
    }

    async fn run(&self, target: &Target, settings: &Settings, choice: CookieChoice) -> ExtractResult {
        let s = &settings.instagram;
        let client = self.clients.get(s.proxy.as_deref()).map_err(|e| proxy_error(&e))?;
        let mut timings = Timings::default();
        // The post is cached under the link asked for, so its media links must name that one.
        let key = target.key().to_string();
        let followed;
        let target = if url::link(target) == Link::Share {
            followed = follow_share(&client, target, &mut timings).await?;
            &followed
        } else {
            target
        };
        let link = url::link(target);
        // Profiles and other pages are only fetched logged out: with cookies they'd show the
        // account's own view of them to anyone on Telegram.
        let jar = self.cookies.header().filter(|_| uses_cookies(link));
        // An account's live stories and highlights can't be seen logged out at all, so "after a
        // login wall" means right away.
        let login_only = matches!(link, Link::Stories(_) | Link::Highlight(_));
        let remembered = jar.is_some() && self.login_only.contains_key(target.key());
        let (first_with_cookies, retry_with_cookies) = match (choice, s.cookie_policy) {
            (CookieChoice::Force, _) if jar.is_none() => {
                return Err(ExtractError::new(
                    Stage::LoginWall,
                    "no usable Instagram cookies are saved, or this page never uses them",
                ));
            }
            (CookieChoice::Force, _) | (CookieChoice::Policy, CookiePolicy::Always) => (jar.is_some(), false),
            (CookieChoice::Policy, CookiePolicy::Never) => (false, false),
            (CookieChoice::Policy, CookiePolicy::Fallback) => {
                let first = (login_only || remembered) && jar.is_some();
                (first, !first && jar.is_some())
            }
        };
        let first = match (first_with_cookies, &jar) {
            (true, Some(jar)) => self.logged_in(&client, target, jar, &mut timings).await,
            _ => logged_out(&client, target, &mut timings).await,
        };
        let result = match (first, &jar) {
            (Err(e), Some(jar)) if retry_with_cookies && matches!(e.stage, Stage::LoginWall | Stage::NotFound) => {
                debug!(url = %target.key(), "retrying with cookies after: {e}");
                let post = self.logged_in(&client, target, jar, &mut timings).await;
                if post.is_ok() {
                    self.login_only.insert(target.key().to_string(), ()).await;
                }
                post
            }
            // Gone since, or the cookies stopped working: logged out first again next time.
            (Err(e), _) if remembered && first_with_cookies && e.stage != Stage::RateLimited => {
                self.login_only.invalidate(target.key()).await;
                Err(e)
            }
            // Without the stories, the account or the highlight's cover is still worth a card.
            (Err(e), _)
                if first_with_cookies
                    && login_only
                    && choice == CookieChoice::Policy
                    && e.stage != Stage::RateLimited =>
            {
                debug!(url = %target.key(), "reading it logged out after: {e}");
                logged_out(&client, target, &mut timings).await.map_err(|_| e)
            }
            (other, _) => other,
        };
        let mut post = match (result, link) {
            // The crawlers' page answers when the query is rate-limited, as it's another route.
            (Err(e), Link::Post(code))
                if matches!(e.stage, Stage::LoginWall | Stage::NotFound | Stage::RateLimited) =>
            {
                match crawler_page(&client, &format!("{}/p/{code}/", url::ORIGIN), &mut timings).await {
                    Ok(page) if let Ok(post) = parse::crawler_post(&page, target) => {
                        info!(url = %target.key(), "only Instagram's preview tags were left: {e}");
                        post
                    }
                    _ => return Err(e),
                }
            }
            (other, _) => other?,
        };
        if s.author_profiles
            && post.page_type != PageType::Profile
            && let Some(handle) = post.profile.handle.clone()
        {
            post.profile.bio = self.bio(&client, &handle, &mut timings).await.map(|b| b.to_string());
        }
        post.key = key;
        Ok((post, timings))
    }

    /// With the saved cookies: the logged-in API for one post or story, or the page an account's
    /// stories or a highlight are on, read as a browser would.
    async fn logged_in(
        &self,
        client: &reqwest::Client,
        target: &Target,
        jar: &str,
        timings: &mut Timings,
    ) -> Result<Post, ExtractError> {
        let result = match url::link(target) {
            Link::Stories(user) => {
                self.reel_page(client, &format!("{}/stories/{user}/", url::ORIGIN), jar, target, timings).await
            }
            Link::Highlight(id) => {
                self.reel_page(client, &format!("{}/stories/highlights/{id}/", url::ORIGIN), jar, target, timings).await
            }
            link => {
                let id = match link {
                    Link::Story(_, id) => Some(id),
                    Link::Post(code) => url::media_id(code),
                    _ => None,
                }
                .ok_or_else(|| {
                    ExtractError::new(Stage::NotFound, "this link names no media the logged-in API can look up")
                })?;
                let request = self.with_cookies(client.get(format!("{}/api/v1/media/{id}/info/", url::ORIGIN)), jar);
                json(request, timings)
                    .await
                    .and_then(|response| parse::api_post(&response, target))
                    .and_then(|post| check_story(target, post))
            }
        };
        if let Err(e) = &result
            && e.stage == Stage::LoginWall
            && self.cookies.mark_rejected(jar).await
        {
            tracing::warn!("Instagram rejected the saved cookies; paste fresh ones in the dashboard");
        }
        result
    }

    /// The headers Instagram's web app sends with its session. Always the client's one browser
    /// user agent: a session that shows up as another device gets logged out.
    fn with_cookies(&self, request: reqwest::RequestBuilder, jar: &str) -> reqwest::RequestBuilder {
        request
            .header(header::COOKIE, jar)
            .header("x-csrftoken", self.cookies.value("csrftoken").unwrap_or_default())
            .header("x-ig-app-id", APP_ID)
            .header(header::ACCEPT, "*/*")
    }

    /// A story or highlight page, logged in, and the stories it preloads.
    async fn reel_page(
        &self,
        client: &reqwest::Client,
        page: &str,
        jar: &str,
        target: &Target,
        timings: &mut Timings,
    ) -> Result<Post, ExtractError> {
        let request = client
            .get(page)
            .header(header::COOKIE, jar)
            .header(header::ACCEPT, "text/html,application/xhtml+xml")
            .header("sec-fetch-dest", "document")
            .header("sec-fetch-mode", "navigate")
            .header("sec-fetch-site", "none");
        let html = String::from_utf8_lossy(&send(request, timings).await?).into_owned();
        let started = Instant::now();
        let post = parse::reel_page(&html, target);
        timings.parse += started.elapsed();
        post
    }

    /// An author's bio, from the profile query; remembered for six hours, none included. A query
    /// that fails isn't remembered, so the next post by them asks again.
    async fn bio(&self, client: &reqwest::Client, username: &str, timings: &mut Timings) -> Option<Arc<str>> {
        if let Some(known) = self.bios.get(username).await {
            return known;
        }
        let response = query(client, &PROFILE_QUERY, &serde_json::json!({ "username": username }), timings).await;
        let bio = match response {
            Ok(response) => response
                .pointer("/data/xig_user_by_username/biography")
                .and_then(Value::as_str)
                .filter(|b| !b.trim().is_empty())
                .map(Arc::from),
            Err(e) => {
                debug!(username, "no bio: {e}");
                return None;
            }
        };
        self.bios.insert(username.to_string(), bio.clone()).await;
        bio
    }

    /// What extraction reads for a target logged out, as a test fixture: the query's answer, the
    /// embed page's post and the preview tags, each with other people's names dropped. Only
    /// logged-out answers: logged-in ones carry the account.
    pub async fn record(&self, target: &Target, settings: &Settings, with_cookies: bool) -> anyhow::Result<String> {
        anyhow::ensure!(!with_cookies, "Instagram fixtures are recorded logged out only");
        let client = self.clients.get(settings.instagram.proxy.as_deref()).map_err(|e| proxy_error(&e))?;
        let mut timings = Timings::default();
        let mut fixture = serde_json::Map::new();
        fixture.insert("url".into(), target.original.as_str().into());
        let tags = |page: Page| serde_json::to_value(page.meta);
        match url::link(target) {
            Link::Profile(user) | Link::Stories(user) => {
                let response =
                    query(&client, &PROFILE_QUERY, &serde_json::json!({ "username": user }), &mut timings).await?;
                fixture.insert("query".into(), response);
            }
            Link::Story(_, id) => {
                let response =
                    query(&client, &POST_QUERY, &post_variables(&url::shortcode_of(id)), &mut timings).await?;
                fixture.insert("query".into(), response);
            }
            Link::Post(code) => {
                fixture.insert("query".into(), query(&client, &POST_QUERY, &post_variables(code), &mut timings).await?);
                let embed = embed_media(&client, code, &mut timings).await.unwrap_or(Value::Null);
                fixture.insert("embed".into(), embed);
                let page = crawler_page(&client, &format!("{}/p/{code}/", url::ORIGIN), &mut timings).await?;
                fixture.insert("crawler".into(), tags(page)?);
            }
            Link::Highlight(_) | Link::Other => {
                fixture
                    .insert("crawler".into(), tags(crawler_page(&client, target.url.as_str(), &mut timings).await?)?);
            }
            Link::Share => anyhow::bail!("record the post the share link leads to"),
        }
        for answer in ["query", "embed"] {
            fixture.get_mut(answer).map(scrub);
        }
        Ok(serde_json::to_string_pretty(&Value::Object(fixture))? + "\n")
    }
}

/// Reads a fixture `record` saved, in the order extraction tries its parts.
pub fn read_fixture(saved: &Value, target: &Target) -> Result<Post, ExtractError> {
    let missing = || ExtractError::new(Stage::Fetch, "the fixture has no answer for this");
    let crawler = || -> Result<Page, ExtractError> {
        let tags: HashMap<String, String> =
            saved.get("crawler").and_then(|t| serde_json::from_value(t.clone()).ok()).ok_or_else(missing)?;
        let meta = page::META.iter().filter_map(|k| Some((*k, tags.get(*k)?.clone()))).collect();
        Ok(Page { scripts: Vec::new(), meta })
    };
    let query = saved.get("query").ok_or_else(missing);
    match url::link(target) {
        Link::Profile(_) => parse::profile(query?, target),
        Link::Stories(_) => parse::stories_profile(query?, target),
        Link::Story(..) => parse::query_post(query?, target).and_then(|post| check_story(target, post)),
        Link::Highlight(_) => parse::highlight_tags(&crawler()?, target),
        Link::Other | Link::Share => parse::other_page(&crawler()?, target),
        Link::Post(_) => {
            let embed = saved.get("embed").filter(|e| !e.is_null());
            parse::query_post(query?, target)
                .or_else(|e| embed.map_or(Err(e.clone()), |m| parse::embed_post(m, target).map_err(|_| e)))
                .or_else(|e| crawler().and_then(|page| parse::crawler_post(&page, target)).map_err(|_| e))
        }
    }
}

impl Extractor for Instagram {
    fn extract<'a>(
        &'a self,
        target: &'a Target,
        settings: &'a Settings,
        cookies: CookieChoice,
    ) -> BoxFuture<'a, ExtractResult> {
        Box::pin(self.run(target, settings, cookies))
    }
}

/// Logged out: the query (with the embed page racing it, for posts), the profile query, or the
/// preview tags of any other page.
async fn logged_out(client: &reqwest::Client, target: &Target, timings: &mut Timings) -> Result<Post, ExtractError> {
    match url::link(target) {
        Link::Post(code) => {
            let (mut query_timings, mut embed_timings) = (Timings::default(), Timings::default());
            let by_query = async {
                let response = query(client, &POST_QUERY, &post_variables(code), &mut query_timings).await?;
                parse::query_post(&response, target)
            };
            let embed_timings_ref = &mut embed_timings;
            let by_embed = move || async move {
                let media = embed_media(client, code, embed_timings_ref).await?;
                parse::embed_post(&media, target)
            };
            let post = hedged(by_query, by_embed, HEDGE).await;
            timings.add(&query_timings);
            timings.add(&embed_timings);
            post
        }
        // A story's ID is a media ID like any other, which the post query takes as a shortcode.
        Link::Story(_, id) => {
            let response = query(client, &POST_QUERY, &post_variables(&url::shortcode_of(id)), timings).await?;
            check_story(target, parse::query_post(&response, target)?)
        }
        Link::Profile(user) => {
            let response = query(client, &PROFILE_QUERY, &serde_json::json!({ "username": user }), timings).await?;
            parse::profile(&response, target)
        }
        Link::Stories(user) => {
            let response = query(client, &PROFILE_QUERY, &serde_json::json!({ "username": user }), timings).await?;
            parse::stories_profile(&response, target)
        }
        Link::Highlight(_) => parse::highlight_tags(&crawler_page(client, target.url.as_str(), timings).await?, target),
        Link::Share | Link::Other => {
            parse::other_page(&crawler_page(client, target.url.as_str(), timings).await?, target)
        }
    }
}

/// Whether cookies may be used for a link: posts, reels and stories, never profiles or other
/// pages.
const fn uses_cookies(link: Link<'_>) -> bool {
    matches!(link, Link::Post(_) | Link::Story(..) | Link::Stories(_) | Link::Highlight(_))
}

/// The post query answers for any media ID, so a story's must be checked against the link.
fn check_story(target: &Target, post: Post) -> Result<Post, ExtractError> {
    let Link::Story(user, id) = url::link(target) else { return Ok(post) };
    let matches = post.page_type == PageType::Story
        && post.profile.handle.as_deref().is_some_and(|h| h.eq_ignore_ascii_case(user))
        && post.url.contains(&format!("/{id}/"));
    if matches {
        Ok(post)
    } else {
        Err(ExtractError::new(Stage::NotFound, "no such story: it's gone, or the link names someone else's"))
    }
}

fn post_variables(code: &str) -> Value {
    serde_json::json!({ "shortcode": code, "__relay_internal__pv__PolarisAIGMMediaWebLabelEnabledrelayprovider": false })
}

/// Runs `first`; `second` starts after `delay`, or as soon as `first` fails. The first success
/// wins (OGInstagram's staged fetch); with neither, `first`'s error, which says more.
async fn hedged<T, F, S>(first: F, second: impl FnOnce() -> S, delay: Duration) -> Result<T, ExtractError>
where
    F: Future<Output = Result<T, ExtractError>>,
    S: Future<Output = Result<T, ExtractError>>,
{
    tokio::pin!(first);
    let early = tokio::select! {
        result = &mut first => match result {
            Ok(value) => return Ok(value),
            Err(e) => Some(e),
        },
        () = tokio::time::sleep(delay) => None,
    };
    let second = second();
    tokio::pin!(second);
    match early {
        Some(e) => second.await.map_err(|_| e),
        None => tokio::select! {
            result = &mut first => match result {
                Ok(value) => Ok(value),
                Err(e) => second.await.map_err(|_| e),
            },
            result = &mut second => match result {
                Ok(value) => Ok(value),
                Err(_) => first.await,
            },
        },
    }
}

/// One of the logged-out web app's GraphQL queries. Its token (`lsd`) is checked only against the
/// header that repeats it, so a random one does.
async fn query(
    client: &reqwest::Client,
    query: &Query,
    variables: &Value,
    timings: &mut Timings,
) -> Result<Value, ExtractError> {
    let mut token = [0u8; 16];
    getrandom::fill(&mut token).map_err(|e| ExtractError::new(Stage::Fetch, format!("no randomness: {e}")))?;
    let lsd = data_encoding::BASE64URL_NOPAD.encode(&token);
    let form = ::url::form_urlencoded::Serializer::new(String::new())
        .append_pair("variables", &variables.to_string())
        .append_pair("doc_id", query.id)
        .append_pair("lsd", &lsd)
        .finish();
    let request = client
        .post(query.url)
        .header(header::USER_AGENT, "Mozilla/5.0")
        .header(header::ACCEPT, "*/*")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header("x-fb-friendly-name", query.name)
        .header("x-fb-lsd", lsd)
        .body(form);
    let response = json(request, timings).await?;
    match parse::query_error(&response) {
        Some(e) if e.stage == Stage::RateLimited => Err(e),
        _ => Ok(response),
    }
}

/// The embed page's post (`"contextJSON"` → `gql_data.shortcode_media`).
async fn embed_media(client: &reqwest::Client, code: &str, timings: &mut Timings) -> Result<Value, ExtractError> {
    let request = client.get(format!("{}/p/{code}/embed/captioned/", url::ORIGIN)).header(header::USER_AGENT, CRAWLER);
    let html = String::from_utf8_lossy(&send(request, timings).await?).into_owned();
    let missing = || ExtractError::new(Stage::Fetch, "the embed page held no post data");
    let start = html.find("\"contextJSON\":").ok_or_else(missing)? + "\"contextJSON\":".len();
    let mut stream = serde_json::Deserializer::from_str(&html[start..]).into_iter::<Option<String>>();
    let inner = stream.next().and_then(Result::ok).flatten().ok_or_else(missing)?;
    let context: Value = serde_json::from_str(&inner).map_err(|_| missing())?;
    context.pointer("/gql_data/shortcode_media").filter(|m| !m.is_null()).cloned().ok_or_else(missing)
}

/// A page as Instagram serves link-preview crawlers.
async fn crawler_page(client: &reqwest::Client, url: &str, timings: &mut Timings) -> Result<Page, ExtractError> {
    let body = send(client.get(url).header(header::USER_AGENT, CRAWLER), timings).await?;
    let started = Instant::now();
    let page = page::parse(&body).map_err(|e| ExtractError::new(Stage::Fetch, e.to_string()));
    timings.parse += started.elapsed();
    page
}

/// Share links redirect once to the post they name.
async fn follow_share(
    client: &reqwest::Client,
    target: &Target,
    timings: &mut Timings,
) -> Result<Target, ExtractError> {
    let started = Instant::now();
    let response = client.get(target.url.as_str()).send().await.map_err(network_error)?;
    timings.fetch += started.elapsed();
    timings.requests += 1;
    let location = response.headers().get(header::LOCATION).and_then(|v| v.to_str().ok()).unwrap_or_default();
    let next = target.url.join(location).ok().map(|u| u.to_string()).and_then(|u| Target::from_link(&u));
    match next {
        Some(next)
            if next.source == Source::Instagram
                && !next.url.path().starts_with("/share/")
                && next.page_type != PageType::Other =>
        {
            Ok(next)
        }
        _ if is_login(location) => {
            Err(ExtractError::new(Stage::LoginWall, "Instagram wants a login to open this share link"))
        }
        _ => Err(ExtractError::new(Stage::Redirect, "the share link didn't lead to a post")),
    }
}

fn is_login(location: &str) -> bool {
    ["/accounts/login", "/challenge", "/checkpoint"].iter().any(|path| location.contains(path))
}

/// Sends a request and reads a JSON answer, telling Instagram's refusals apart.
async fn json(request: reqwest::RequestBuilder, timings: &mut Timings) -> Result<Value, ExtractError> {
    let body = send(request, timings).await?;
    let started = Instant::now();
    let value: Value = serde_json::from_slice(&body).map_err(|_| {
        ExtractError::new(Stage::LoginWall, "Instagram answered with a page instead of data, which means a login")
    })?;
    timings.parse += started.elapsed();
    let message = value.get("message").and_then(Value::as_str).unwrap_or_default();
    if value.get("status").and_then(Value::as_str) == Some("fail") {
        return Err(if message.contains("Please wait") {
            ExtractError::new(Stage::RateLimited, "Instagram asked to wait before more requests")
        } else if value.get("require_login").and_then(Value::as_bool) == Some(true)
            || ["login", "checkpoint", "challenge"].iter().any(|word| message.contains(word))
        {
            ExtractError::new(Stage::LoginWall, "Instagram wants a login for this")
        } else {
            ExtractError::new(Stage::Fetch, format!("Instagram refused: {message}"))
        });
    }
    Ok(value)
}

/// Sends a request and reads the answer, up to [`MAX_BYTES`].
async fn send(request: reqwest::RequestBuilder, timings: &mut Timings) -> Result<Vec<u8>, ExtractError> {
    let started = Instant::now();
    timings.requests += 1;
    let response = request.send().await.map_err(network_error)?;
    let status = response.status();
    let location =
        response.headers().get(header::LOCATION).and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
    if status.is_redirection() {
        timings.fetch += started.elapsed();
        return Err(if is_login(&location) {
            ExtractError::new(Stage::LoginWall, "Instagram sent this to its login page")
        } else {
            ExtractError::new(Stage::Redirect, format!("Instagram redirected ({status})"))
        });
    }
    // Instagram's errors come with JSON worth reading; a 429 is clear enough.
    if status.as_u16() == 429 {
        return Err(ExtractError::new(Stage::RateLimited, "Instagram answered 429 Too Many Requests"));
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        body.extend_from_slice(&chunk.map_err(network_error)?);
        if body.len() > MAX_BYTES {
            return Err(ExtractError::new(Stage::Fetch, "Instagram's answer was too big"));
        }
    }
    timings.fetch += started.elapsed();
    if status.as_u16() == 404 {
        return Err(ExtractError::new(Stage::NotFound, "Instagram has no such page"));
    }
    if !status.is_success() && !matches!(status.as_u16(), 400 | 401 | 403) {
        return Err(ExtractError::new(Stage::Fetch, format!("Instagram answered {status}")));
    }
    Ok(body)
}

/// Keeps only what extraction reads of a saved answer. Anything else may name other people
/// (`usertags`, the music's artist, likers) or track (`*_token`), so it's dropped by default.
fn scrub(value: &mut Value) {
    const KEPT: &[&str] = &[
        // Errors, and the post query's items.
        "data",
        "errors",
        "code",
        "message",
        "xdt_api__v1__media__shortcode__web_info",
        "items",
        "pk",
        "id",
        "media_type",
        "product_type",
        "taken_at",
        "user",
        "username",
        "full_name",
        "profile_pic_url",
        "hd_profile_pic_url_info",
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
        "comment_count",
        "play_count",
        "ig_play_count",
        "view_count",
        "media_repost_count",
        "like_and_view_counts_disabled",
        // The embed page's `shortcode_media`.
        "__typename",
        "shortcode",
        "display_url",
        "dimensions",
        "is_video",
        "video_url",
        "edge_sidecar_to_children",
        "edges",
        "node",
        "edge_media_to_caption",
        "edge_liked_by",
        "edge_media_preview_like",
        "edge_media_to_comment",
        "count",
        "owner",
        "taken_at_timestamp",
        "video_view_count",
        // The profile query.
        "xig_user_by_username",
        "biography",
        "is_private",
    ];
    crate::extract::keep_keys(value, KEPT);
}

fn build_client(proxy: Option<&str>) -> anyhow::Result<reqwest::Client> {
    let mut headers = HeaderMap::new();
    headers.insert(header::ACCEPT_LANGUAGE, HeaderValue::from_static("en-US,en;q=0.5"));
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

    #[tokio::test]
    async fn hedging_takes_the_first_success() {
        let fail = || async { Err::<u8, _>(ExtractError::new(Stage::NotFound, "first")) };
        let slow_ok = |n: u8, ms: u64| async move {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            Ok::<_, ExtractError>(n)
        };
        let delay = Duration::from_millis(50);
        assert_eq!(hedged(slow_ok(1, 0), || slow_ok(2, 0), delay).await.unwrap(), 1, "fast first: second never runs");
        assert_eq!(hedged(slow_ok(1, 500), || slow_ok(2, 0), delay).await.unwrap(), 2, "slow first: second wins");
        assert_eq!(
            hedged(fail(), || slow_ok(2, 0), Duration::from_secs(60)).await.unwrap(),
            2,
            "a failure starts it early"
        );
        let both = hedged(fail(), || async { Err::<u8, _>(ExtractError::new(Stage::Fetch, "second")) }, delay).await;
        assert_eq!(both.unwrap_err().message, "first", "the first's error says more");
    }

    #[test]
    fn scrub_keeps_only_what_extraction_reads() {
        let mut value = serde_json::json!({
            "user": {"username": "nasa", "friendship_status": {"following": true}},
            "top_likers": ["someone"],
            "usertags": {"in": [{"user": {"username": "tagged"}}]},
            "clips_metadata": {"music_info": {"display_artist": "Someone"}},
            "carousel_media": [{"preview_comments": [{"text": "hi"}], "code": "x"}],
            "organic_tracking_token": "abc"
        });
        scrub(&mut value);
        assert_eq!(value, serde_json::json!({"user": {"username": "nasa"}, "carousel_media": [{"code": "x"}]}));
    }
}
