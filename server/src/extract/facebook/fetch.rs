//! Requests to Facebook: a warm HTTP/2 client per proxy setting, redirects followed by hand
//! (at most 10, Facebook only), login detection, and the body streamed into the parser.

use std::time::{Duration, Instant};

use reqwest::header::{self, HeaderMap, HeaderValue};
use url::Url;

use super::url::{clean, is_login_url, share_redirect_target};
use crate::extract::page::{self, Page};
use crate::extract::{ExtractError, Source, Stage, Timings, network_error, proxy_error};
use crate::net::ProxyClients;

pub const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36";
const MAX_REDIRECTS: usize = 10;
/// A login wall met after the cookies were dropped, which says nothing about them.
pub const LOGIN_WITHOUT_COOKIES: &str = "Facebook wants a login for this page, which is never fetched with cookies";

pub struct Fetcher {
    clients: ProxyClients,
}

impl Fetcher {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self { clients: ProxyClients::new(build_client)? })
    }

    /// Fetches `start`, following redirects, and parses the page as it downloads.
    /// Returns the final URL and the page. The cookies are dropped on the hop that reaches a
    /// profile or another page that's never fetched with them: a share link can lead to a group.
    pub async fn page(
        &self,
        start: &Url,
        proxy: Option<&str>,
        cookies: Option<&str>,
        timings: &mut Timings,
    ) -> Result<(Url, Page), ExtractError> {
        let client = self.clients.get(proxy).map_err(|e| proxy_error(&e))?;
        let mut current = clean(start);
        let mut cookies = cookies;
        let mut dropped = false;
        for hop in 0..=MAX_REDIRECTS {
            if Source::of_url(&current) != Some(Source::Facebook) {
                return Err(ExtractError::new(
                    Stage::Redirect,
                    format!("redirected off Facebook to {}", current.host_str().unwrap_or("?")),
                ));
            }
            // Every hop over https, so the cookies never go out in the clear.
            if current.scheme() == "http" {
                current.set_scheme("https").expect("http and https are both special schemes");
            }
            // Never to `fb.watch`, whose links name no page of their own: the cookies are
            // facebook.com's, and go with the page it redirects to.
            let host = current.host_str().unwrap_or_default();
            let on_facebook = host == "facebook.com" || host.ends_with(".facebook.com");
            if on_facebook
                && cookies.is_some()
                && !super::may_use_cookies(super::url::target(current.clone()).page_type)
            {
                (cookies, dropped) = (None, true);
            }
            let sent = cookies.filter(|_| on_facebook);
            let login_wall = |message: &'static str| {
                ExtractError::new(Stage::LoginWall, if dropped { LOGIN_WITHOUT_COOKIES } else { message })
            };
            let started = Instant::now();
            let mut request = client.get(current.clone());
            if let Some(cookies) = sent {
                request = request.header(header::COOKIE, cookies);
            }
            let response = request.send().await.map_err(network_error)?;
            timings.requests += 1;
            let status = response.status();

            if status.is_redirection() {
                timings.fetch += started.elapsed();
                if hop == MAX_REDIRECTS {
                    return Err(ExtractError::new(Stage::Redirect, "more than 10 redirects"));
                }
                let location = response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| ExtractError::new(Stage::Fetch, "a redirect without a Location"))?;
                let next = current
                    .join(location)
                    .map_err(|_| ExtractError::new(Stage::Redirect, "a redirect to an unreadable URL"))?;
                if is_login_url(&next) {
                    return Err(login_wall("Facebook redirected to its login page"));
                }
                current = clean(&share_redirect_target(&next).unwrap_or(next));
                continue;
            }
            if !status.is_success() {
                return Err(ExtractError::new(Stage::Fetch, format!("Facebook answered {status}")));
            }
            if is_login_url(&current) {
                return Err(login_wall("Facebook showed its login page"));
            }

            let page = page::read(response, page::FACEBOOK_HINTS, started, timings).await?;
            return Ok((current, page));
        }
        unreachable!("the loop returns on its last hop")
    }
}

fn build_client(proxy: Option<&str>) -> anyhow::Result<reqwest::Client> {
    let mut headers = HeaderMap::new();
    for (name, value) in [
        ("accept", "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"),
        ("accept-language", "en-US,en;q=0.5"),
        ("dnt", "1"),
        ("sec-gpc", "1"),
        ("upgrade-insecure-requests", "1"),
        ("sec-fetch-dest", "document"),
        ("sec-fetch-mode", "navigate"),
        ("sec-fetch-site", "none"),
        ("sec-fetch-user", "?1"),
        ("priority", "u=0, i"),
    ] {
        headers.insert(name, HeaderValue::from_static(value));
    }
    let mut builder = crate::net::client_builder()?
        .user_agent(USER_AGENT)
        .default_headers(headers)
        .redirect(reqwest::redirect::Policy::none())
        .read_timeout(Duration::from_secs(15))
        .pool_idle_timeout(Duration::from_secs(300))
        .pool_max_idle_per_host(8)
        .tcp_keepalive(Duration::from_secs(60))
        .http2_keep_alive_interval(Duration::from_secs(30))
        .http2_keep_alive_timeout(Duration::from_secs(10))
        .http2_keep_alive_while_idle(true);
    if let Some(proxy) = proxy {
        builder = builder.proxy(reqwest::Proxy::all(proxy)?);
    }
    Ok(builder.build()?)
}
