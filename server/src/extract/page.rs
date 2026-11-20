//! Reading a page from one of the sites with lol_html (Facebook's and Threads' as they download).
//! Only the meta tags and the JSON scripts (`data-sjs`, which Meta's sites embed) we need are
//! kept; the rest of the multi-megabyte page is never stored.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use lol_html::send::{HtmlRewriter, Settings};
use lol_html::{element, text};
use serde_json::Value;

use crate::extract::{ExtractError, Stage, Timings, network_error};

/// Pages bigger than this are refused.
const MAX_PAGE_BYTES: usize = 16 * 1024 * 1024;

pub const META: [&str; 9] = [
    "og:title",
    "og:description",
    "og:image",
    "og:image:alt",
    "og:url",
    "twitter:title",
    "description",
    "al:ios:url",
    "al:android:url",
];
/// On Facebook's pages, a script holds post data when it has `__bbox` and one of these. Photo
/// nodes cover album pages, whose photo grid has none of the others.
/// `contributors` is an album's: its name and its owner's are only in those scripts.
pub const FACEBOOK_HINTS: &[&str] = &[
    "videoDeliveryLegacyFields",
    "currMedia",
    "node_v2",
    "attachments",
    r#""__typename":"Photo""#,
    r#""contributors""#,
];

/// One `<script type="application/json" data-sjs>` that matched.
#[derive(Debug)]
pub struct Script {
    pub json: Value,
    /// Holds post data (`__bbox` plus one of the parser's hints).
    pub media: bool,
    /// Holds route data (`initialRouteInfo`).
    pub route: bool,
}

#[derive(Debug, Default)]
pub struct Page {
    pub scripts: Vec<Script>,
    /// The first value of each wanted meta tag.
    pub meta: HashMap<&'static str, String>,
}

impl Page {
    pub fn media(&self) -> impl Iterator<Item = &Value> {
        self.scripts.iter().filter(|s| s.media).map(|s| &s.json)
    }

    pub fn route(&self) -> impl Iterator<Item = &Value> {
        self.scripts.iter().filter(|s| s.route).map(|s| &s.json)
    }

    pub fn meta(&self, property: &str) -> Option<&str> {
        self.meta.get(property).map(String::as_str).map(str::trim).filter(|s| !s.is_empty())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("the page is bigger than {} MB", MAX_PAGE_BYTES / 1024 / 1024)]
    TooBig,
    #[error("unreadable HTML: {0}")]
    Html(#[from] lol_html::errors::RewritingError),
}

/// Feeds a page to lol_html chunk by chunk.
pub struct Parser<'h> {
    rewriter: HtmlRewriter<'h, fn(&[u8])>,
    /// Bytes fed so far, against `MAX_PAGE_BYTES`.
    bytes: usize,
}

impl<'h> Parser<'h> {
    /// Keeps the scripts with `__bbox` and one of `hints`, and the route's.
    pub fn with_hints(page: &'h mut Page, hints: &'static [&'static str]) -> Self {
        let Page { scripts, meta } = page;
        let mut buffer = String::new();
        let settings = Settings::new_send()
            .append_element_content_handler(element!("meta[property], meta[name]", move |el| {
                let key = el.get_attribute("property").or_else(|| el.get_attribute("name"));
                if let Some(key) = key.and_then(|k| META.iter().find(|m| **m == k))
                    && !meta.contains_key(key)
                    && let Some(content) = el.get_attribute("content")
                {
                    meta.insert(key, decode_entities(&content));
                }
                Ok(())
            }))
            .append_element_content_handler(text!(r#"script[type="application/json"][data-sjs]"#, move |chunk| {
                buffer.push_str(chunk.as_str());
                if chunk.last_in_text_node() {
                    let media = buffer.contains("__bbox") && hints.iter().any(|h| buffer.contains(h));
                    let route = buffer.contains("initialRouteInfo");
                    if (media || route)
                        && let Ok(json) = serde_json::from_str(&buffer)
                    {
                        scripts.push(Script { json, media, route });
                    }
                    // Kept, not dropped: the next script reuses its room.
                    buffer.clear();
                }
                Ok(())
            }));
        let sink: fn(&[u8]) = |_| {};
        Self { rewriter: HtmlRewriter::new(settings, sink), bytes: 0 }
    }

    pub fn write(&mut self, chunk: &[u8]) -> Result<(), ParseError> {
        self.bytes += chunk.len();
        if self.bytes > MAX_PAGE_BYTES {
            return Err(ParseError::TooBig);
        }
        Ok(self.rewriter.write(chunk)?)
    }

    pub fn end(self) -> Result<(), ParseError> {
        Ok(self.rewriter.end()?)
    }
}

/// Reads a page as it downloads, keeping the scripts with one of `hints`. The time parsing took
/// goes to `timings.parse`, the rest since `started` to `timings.fetch`.
pub async fn read(
    response: reqwest::Response,
    hints: &'static [&'static str],
    started: Instant,
    timings: &mut Timings,
) -> Result<Page, ExtractError> {
    let unreadable = |e: ParseError| ExtractError::new(Stage::Fetch, e.to_string());
    let mut page = Page::default();
    let mut parse_time = Duration::ZERO;
    {
        let mut parser = Parser::with_hints(&mut page, hints);
        let mut body = response.bytes_stream();
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(network_error)?;
            let parse_started = Instant::now();
            parser.write(&chunk).map_err(unreadable)?;
            parse_time += parse_started.elapsed();
        }
        let parse_started = Instant::now();
        parser.end().map_err(unreadable)?;
        parse_time += parse_started.elapsed();
    }
    timings.fetch += started.elapsed().saturating_sub(parse_time);
    timings.parse += parse_time;
    Ok(page)
}

/// The link a saved page was recorded from: its first line, `<!-- url: … -->`, which `record`
/// writes.
pub fn saved_url(html: &[u8]) -> Option<&str> {
    let end = html.iter().position(|&b| b == b'\n').unwrap_or(html.len());
    let line = std::str::from_utf8(&html[..end]).ok()?.trim_end_matches('\r');
    line.strip_prefix("<!-- url: ")?.strip_suffix(" -->")
}

/// Decodes the character references lol_html leaves in attribute values:
/// numeric ones and the few named ones Facebook uses.
pub fn decode_entities(value: &str) -> String {
    if !value.contains('&') {
        return value.to_string();
    }
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        let decoded = rest.find(';').filter(|end| *end <= 10).and_then(|end| {
            let name = &rest[1..end];
            let ch = match name {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some('\u{a0}'),
                _ => {
                    let number = name.strip_prefix('#')?;
                    let code = match number.strip_prefix(['x', 'X']) {
                        Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                        None => number.parse().ok()?,
                    };
                    char::from_u32(code)
                }
            }?;
            Some((ch, end + 1))
        });
        match decoded {
            Some((ch, len)) => {
                out.push(ch);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Parses a whole page held in memory (tests, saved pages, album pages).
pub fn parse(html: &[u8]) -> Result<Page, ParseError> {
    parse_with(html, FACEBOOK_HINTS)
}

/// [`parse`], keeping the scripts with one of `hints`.
pub fn parse_with(html: &[u8], hints: &'static [&'static str]) -> Result<Page, ParseError> {
    let mut page = Page::default();
    let mut parser = Parser::with_hints(&mut page, hints);
    // Small chunks, as from the network.
    for chunk in html.chunks(16 * 1024) {
        parser.write(chunk)?;
    }
    parser.end()?;
    Ok(page)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_matching_scripts_and_meta() {
        let html = br#"<html><head>
            <meta property="og:title" content="First">
            <meta property="og:title" content="Second">
            <meta name="og:image" content="https://x/i.jpg">
            <meta property="og:unwanted" content="x">
            </head><body>
            <script type="application/json" data-sjs>{"__bbox":{"node_v2":{"id":"1"}}}</script>
            <script type="application/json" data-sjs>{"initialRouteInfo":{"route":{}}}</script>
            <script type="application/json" data-sjs>{"__bbox":{"unrelated":true}}</script>
            <script type="application/json">{"__bbox":{"node_v2":{}}}</script>
            <script type="application/json" data-sjs>{"__bbox": broken node_v2</script>
            <script>var node_v2 = "__bbox";</script>
            </body></html>"#;
        let page = parse(html).unwrap();
        assert_eq!(page.media().count(), 1);
        assert_eq!(page.route().count(), 1);
        assert_eq!(page.meta("og:title"), Some("First"));
        assert_eq!(page.meta("og:image"), Some("https://x/i.jpg"));
        assert_eq!(page.meta.len(), 2);
    }

    #[test]
    fn meta_content_is_decoded() {
        let page = parse(br#"<meta property="og:image" content="https://x/a.jpg?a=1&amp;oe=6AFF&#x26;b"><meta property="og:description" content="1 &#xb7; 2 &#183; &bogus; &">"#).unwrap();
        assert_eq!(page.meta("og:image"), Some("https://x/a.jpg?a=1&oe=6AFF&b"));
        assert_eq!(page.meta("og:description"), Some("1 · 2 · &bogus; &"));
    }

    #[test]
    fn scripts_split_across_chunks() {
        let json = format!(r#"{{"__bbox":{{"node_v2":{{"text":"{}"}}}}}}"#, "x".repeat(100_000));
        let html = format!(r#"<script type="application/json" data-sjs>{json}</script>"#);
        assert_eq!(parse(html.as_bytes()).unwrap().media().count(), 1);
    }
}
