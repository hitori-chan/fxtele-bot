//! `/{any path}`: the preview page for Telegram, a redirect to the post for everyone else. The
//! hostname picks the site (`INSTAGRAM_HOST`, or Facebook).

use std::sync::Arc;
use std::time::Instant;

use axum::Extension;
use axum::extract::State;
use axum::http::header::HOST;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use tracing::info;

use super::{Visitor, html, redirect};
use crate::app::{App, Lookup};
use crate::extract::Target;
use crate::extract::{HdVideo, Media, MediaKind, Post, Stage};
use crate::render::{self, MediaUrl};
use crate::settings::{FailureResponse, MediaLinks, Settings, VideoQuality};
use crate::stats::Outcome;
use crate::video::Videos;

pub async fn handle(
    State(app): State<Arc<App>>,
    Extension(visitor): Extension<Visitor>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    let start = Instant::now();
    let path = uri.path_and_query().map_or("/", |p| p.as_str());
    if is_site_file(uri.path()) {
        return super::not_found();
    }
    let source = app.config.source_for_host(host(&uri, &headers));
    let Some(target) = Target::from_request(source, path) else {
        return super::not_found();
    };
    match method.as_str() {
        "GET" | "HEAD" => {}
        // @WebpageBot refreshing a preview: the next look fetches the post again instead of
        // getting the cached copy, a failure included (FxEmbed honours the same).
        "PURGE" | "DELETE" if visitor.in_telegram_ranges => {
            app.cache.remove(target.key()).await;
            info!(source = source.as_str(), page_type = target.page_type.as_str(), "telegram purged a preview");
            return StatusCode::OK.into_response();
        }
        _ => return (StatusCode::METHOD_NOT_ALLOWED, [(axum::http::header::ALLOW, "GET, HEAD")]).into_response(),
    }
    if !visitor.is_telegram || uri.path() == "/" {
        app.stats.record(Outcome::Redirect, Some(&target), start.elapsed());
        return redirect(target.original.as_str());
    }

    let settings = app.settings.get();
    let (outcome, post, stage) = match app.lookup(&target).await {
        Lookup::Hit(post) => (Outcome::Hit, Some(post), None),
        Lookup::Extracted(post) => (Outcome::Extracted, Some(post), None),
        Lookup::Failed(error) => (Outcome::Failed(error.stage), None, Some(error.stage)),
        Lookup::CachedFailure(error) => (Outcome::CachedFailure(error.stage), None, Some(error.stage)),
        Lookup::Busy => (Outcome::Busy, None, None),
    };
    let response = match post {
        Some(post) => page(&app, &post, &settings, &target, host(&uri, &headers)).await,
        None => failure(&settings, &target, stage),
    };
    let took = start.elapsed();
    app.stats.record(outcome, Some(&target), took);
    info!(
        source = source.as_str(),
        page_type = target.page_type.as_str(),
        outcome = outcome.label(),
        ms = u64::try_from(took.as_millis()).unwrap_or(u64::MAX),
        "telegram"
    );
    response
}

async fn page(app: &App, post: &Post, settings: &Settings, target: &Target, host: Option<&str>) -> Response {
    let ready = app.videos.ready(settings);
    // The card shows one item (as `render::shown_item` picks it), Instant View every one.
    let card = render::shown_item(post, target);
    let every = render::instant_view_enabled(post, settings);
    let hd = |i: usize| {
        let shown = every || Some(i) == card;
        post.media.get(i).and_then(|m| joined(m, settings)).filter(|_| ready && shown)
    };
    // Telegram plays a card's video only when it comes through a redirect on this host, not
    // straight from the site's CDN, so videos always go through `/_/v/` (FxEmbed has the same
    // workaround). It sends Telegram on to the 1080p file or the site's, signing a fresh link for
    // the bucket each time, so the page never holds one that has run out.
    let has_video = post.media.iter().any(|m| m.kind == MediaKind::Video);
    let mosaic = crate::mosaic::wanted(post, target, settings);
    let linked = settings.media_links == MediaLinks::Redirect || has_video || mosaic;
    let id = match host {
        Some(_) if linked => Some(app.cache.media_id(&post.key).await),
        _ => None,
    };
    let mut media: Vec<MediaUrl> = post.media.iter().map(MediaUrl::direct).collect();
    // Photos linked straight to the site's CDN never come through `/_/m/`, where they're counted
    // otherwise: they count as Telegram gets the page that links them, as it fetches them next.
    // The page links every item when Instant View is on, else the card's (or a mosaic, counted
    // when it's fetched).
    if settings.media_links != MediaLinks::Redirect {
        let pictured = |m: &&Media| m.kind != MediaKind::Video || m.thumbnail.is_some();
        let photos = match (every, card) {
            (true, _) => post.media.iter().filter(pictured).count(),
            (false, Some(i)) if !mosaic => post.media.get(i).filter(pictured).into_iter().count(),
            (false, _) => 0,
        };
        app.stats.media_many(crate::stats::Media::Image, post.source(), photos as u64);
    }
    if let (Some(host), Some(id)) = (host, &id) {
        for (i, (url, item)) in media.iter_mut().zip(&post.media).enumerate() {
            if settings.media_links == MediaLinks::Redirect {
                url.src = format!("https://{host}/_/m/{id}/{i}");
                url.poster = item.thumbnail.as_ref().map(|_| format!("https://{host}/_/m/{id}/{i}t"));
            }
            if item.kind == MediaKind::Video {
                url.src = format!("https://{host}/_/v/{id}/{i}.mp4");
            }
            if let (Some(hd), Some(name)) = (hd(i), post.source().and_then(|s| Videos::file_name(s, id, i))) {
                url.size = Some((hd.width, hd.height));
                // Telegram asks for the video right after the page: have it ready. In the
                // background, as the first look at the bucket lists it.
                app.videos.prepare(name, hd.clone(), post.url.clone(), app.settings.get());
            }
        }
    }
    // Links in the text go through `/_/l/`, which Telegram can't follow: its Instant View builder
    // visits links, and one site that fails breaks the whole article (FxEmbed does the same).
    let link = |url: &str| match host {
        Some(host) => {
            let encoded: String = url::form_urlencoded::byte_serialize(url.as_bytes()).collect();
            format!("https://{host}/_/l/{}?u={encoded}", app.cache.link_signature(url))
        }
        None => url.to_string(),
    };
    // Several photos show as one picture on the card, built when Telegram asks for it.
    let card = match (host, &id) {
        (Some(host), Some(id)) if mosaic => Some(MediaUrl {
            src: format!("https://{host}/_/g/{id}.jpg"),
            poster: None,
            size: crate::mosaic::size(post),
        }),
        _ => None,
    };
    html(render::preview_with(post, &media, card.as_ref(), settings, target, &link))
}

/// Files browsers and apps ask any site for (`/favicon.ico`, `/apple-touch-icon.png`,
/// `/.well-known/...`), which would otherwise look like usernames and cost a fetch.
fn is_site_file(path: &str) -> bool {
    const EXTENSIONS: [&str; 7] = ["ico", "png", "svg", "txt", "xml", "json", "webmanifest"];
    let file = path.rsplit('/').next().unwrap_or_default();
    path.starts_with("/.well-known/")
        || file.rsplit_once('.').is_some_and(|(_, ext)| EXTENSIONS.iter().any(|e| ext.eq_ignore_ascii_case(e)))
}

/// The 1080p version to link instead of the 720p file: when it's on and will fit.
/// The 1080p stream to join for `media`, when 1080p is on and its file fits Telegram's limit.
pub(super) fn joined<'a>(media: &'a Media, settings: &Settings) -> Option<&'a HdVideo> {
    let limit = settings.video_megabytes * 1024 * 1024;
    media.hd.as_ref().filter(|hd| settings.video_quality == VideoQuality::Best && hd.bytes.is_none_or(|b| b <= limit))
}

fn failure(settings: &Settings, target: &Target, stage: Option<Stage>) -> Response {
    let card = || html(render::unavailable(settings, target, stage));
    match settings.failure_response {
        FailureResponse::Redirect => redirect(target.original.as_str()),
        FailureResponse::Page => card(),
        FailureResponse::Unavailable => (StatusCode::SERVICE_UNAVAILABLE, card()).into_response(),
    }
}

/// The hostname the request came in on, for absolute `/_/m/` links. Only plain host characters.
fn host<'a>(uri: &'a Uri, headers: &'a HeaderMap) -> Option<&'a str> {
    let host = headers.get(HOST).and_then(|v| v.to_str().ok()).or_else(|| uri.authority().map(|a| a.as_str()))?;
    let valid = !host.is_empty()
        && host.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']'));
    valid.then_some(host)
}
