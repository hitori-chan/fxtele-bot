//! Requests to Facebook: a warm HTTP/2 client per proxy setting, redirects followed by hand
//! (at most 10, Facebook only), login detection, and the body streamed into the parser.

use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use futures_util::StreamExt;
use reqwest::header::{self, HeaderMap, HeaderValue};
use url::Url;

use super::url::{clean, is_login_url, share_redirect_target};
use crate::extract::page::{Page, Parser};
use crate::extract::{ExtractError, Source, Stage, Timings};

pub const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36";
const MAX_REDIRECTS: usize = 10;

pub struct Fetcher {
    client: ArcSwap<(Option<String>, reqwest::Client)>,
}

impl Fetcher {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self { client: ArcSwap::from_pointee((None, build_client(None)?)) })
    }

    /// The client for this proxy setting; a new setting builds a new client once.
    fn client(&self, proxy: Option<&str>) -> Result<reqwest::Client, ExtractError> {
        let current = self.client.load();
        if current.0.as_deref() == proxy {
            return Ok(current.1.clone());
        }
        let client =
            build_client(proxy).map_err(|e| ExtractError::new(Stage::Fetch, format!("bad proxy setting: {e}")))?;
        self.client.store(Arc::new((proxy.map(str::to_string), client.clone())));
        Ok(client)
    }

    /// Fetches `start`, following redirects, and parses the page as it downloads.
    /// Returns the final URL and the page.
    pub async fn page(
        &self,
        start: &Url,
        proxy: Option<&str>,
        cookies: Option<&str>,
        timings: &mut Timings,
    ) -> Result<(Url, Page), ExtractError> {
        let client = self.client(proxy)?;
        let mut current = clean(start);
        for hop in 0..=MAX_REDIRECTS {
            if Source::of_url(&current) != Some(Source::Facebook) {
                return Err(ExtractError::new(
                    Stage::Redirect,
                    format!("redirected off Facebook to {}", current.host_str().unwrap_or("?")),
                ));
            }
            let started = Instant::now();
            let mut request = client.get(current.clone());
            if let Some(cookies) = cookies {
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
                    return Err(ExtractError::new(Stage::LoginWall, "Facebook redirected to its login page"));
                }
                current = clean(&share_redirect_target(&next).unwrap_or(next));
                continue;
            }
            if !status.is_success() {
                return Err(ExtractError::new(Stage::Fetch, format!("Facebook answered {status}")));
            }
            if is_login_url(&current) {
                return Err(ExtractError::new(Stage::LoginWall, "Facebook showed its login page"));
            }

            let mut page = Page::default();
            let mut parse_time = Duration::ZERO;
            {
                let mut parser = Parser::new(&mut page);
                let mut body = response.bytes_stream();
                while let Some(chunk) = body.next().await {
                    let chunk = chunk.map_err(network_error)?;
                    let parse_started = Instant::now();
                    parser.write(&chunk).map_err(|e| ExtractError::new(Stage::Fetch, e.to_string()))?;
                    parse_time += parse_started.elapsed();
                }
                let parse_started = Instant::now();
                parser.end().map_err(|e| ExtractError::new(Stage::Fetch, e.to_string()))?;
                parse_time += parse_started.elapsed();
            }
            timings.fetch += started.elapsed().saturating_sub(parse_time);
            timings.parse += parse_time;
            return Ok((current, page));
        }
        unreachable!("the loop returns on its last hop")
    }
}

fn network_error(error: reqwest::Error) -> ExtractError {
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
