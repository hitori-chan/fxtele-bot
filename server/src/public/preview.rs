//! `/{any facebook path}`: the preview page for Telegram, a redirect for everyone else.

use std::sync::Arc;
use std::time::Instant;

use axum::Extension;
use axum::extract::State;
use axum::http::header::HOST;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use tracing::info;

use super::{Visitor, html, redirect};
use crate::app::{App, Lookup};
use crate::extract::facebook::url::Target;
use crate::extract::{HdVideo, Media, MediaKind, Post};
use crate::render::{self, MediaUrl};
use crate::settings::{FailureResponse, MediaLinks, Settings, VideoQuality};
use crate::stats::Outcome;
use crate::video::Videos;

pub async fn handle(
    State(app): State<Arc<App>>,
    Extension(visitor): Extension<Visitor>,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    let start = Instant::now();
    let path = uri.path_and_query().map_or("/", |p| p.as_str());
    let Some(target) = Target::from_request(path) else {
        return super::not_found();
    };
    if !visitor.is_telegram || uri.path() == "/" {
        app.stats.record(Outcome::Redirect, Some(target.page_type), start.elapsed());
        return redirect(target.original.as_str());
    }

    let settings = app.settings.get();
    let (outcome, post) = match app.lookup(&target).await {
        Lookup::Hit(post) => (Outcome::Hit, Some(post)),
        Lookup::Extracted(post) => (Outcome::Extracted, Some(post)),
        Lookup::Failed(error) => (Outcome::Failed(error.stage), None),
        Lookup::Busy => (Outcome::Busy, None),
    };
    let response = match post {
        Some(post) => page(&app, &post, &settings, &target, host(&uri, &headers)).await,
        None => failure(&settings, &target),
    };
    let took = start.elapsed();
    app.stats.record(outcome, Some(target.page_type), took);
    info!(
        page_type = target.page_type.as_str(),
        outcome = outcome.label(),
        ms = u64::try_from(took.as_millis()).unwrap_or(u64::MAX),
        "telegram"
    );
    response
}

async fn page(app: &App, post: &Post, settings: &Settings, target: &Target, host: Option<&str>) -> Response {
    let ready = app.videos.ready(settings);
    // The page shows its first video (as `render::summary` picks it), or every item in Instant View.
    let first_video = post.media.iter().position(|m| m.kind == MediaKind::Video);
    let every = render::instant_view_enabled(post, settings);
    let hd = |i: usize| {
        let shown = every || Some(i) == first_video;
        post.media.get(i).and_then(|m| joined(m, settings)).filter(|_| ready && shown)
    };
    let linked = settings.media_links == MediaLinks::Redirect || (0..post.media.len()).any(|i| hd(i).is_some());
    let id = match host {
        Some(_) if linked => Some(app.cache.media_id(&post.key).await),
        _ => None,
    };
    let mut media: Vec<MediaUrl> = post.media.iter().map(MediaUrl::direct).collect();
    if let (Some(host), Some(id)) = (host, &id) {
        for (i, (url, item)) in media.iter_mut().zip(&post.media).enumerate() {
            if settings.media_links == MediaLinks::Redirect {
                url.src = format!("https://{host}/_/m/{id}/{i}");
                url.poster = item.thumbnail.as_ref().map(|_| format!("https://{host}/_/m/{id}/{i}t"));
            }
            if let (Some(hd), Some(name)) = (hd(i), Videos::file_name(id, i)) {
                url.size = Some((hd.width, hd.height));
                // Always through `/_/v/`, which signs a fresh link each time Telegram downloads, so
                // the page never holds one that has run out.
                url.src = format!("https://{host}/_/v/{id}/{i}.mp4");
                // Telegram asks for the video right after the page: have it ready. In the
                // background, as the first look at the bucket lists it.
                app.videos.prepare(name, hd.clone(), post.url.clone(), app.settings.get());
            }
        }
    }
    html(render::preview(post, &media, settings, target.original.as_str()))
}

/// The 1080p version to link instead of the 720p file: when it's on and will fit.
fn joined<'a>(media: &'a Media, settings: &Settings) -> Option<&'a HdVideo> {
    let limit = settings.video_megabytes * 1024 * 1024;
    media.hd.as_ref().filter(|hd| settings.video_quality == VideoQuality::Best && hd.bytes.is_none_or(|b| b <= limit))
}

fn failure(settings: &Settings, target: &Target) -> Response {
    match settings.failure_response {
        FailureResponse::Redirect => redirect(target.original.as_str()),
        FailureResponse::Page => html(render::unavailable(settings, target.original.as_str())),
        FailureResponse::Unavailable => {
            (StatusCode::SERVICE_UNAVAILABLE, html(render::unavailable(settings, target.original.as_str())))
                .into_response()
        }
    }
}

/// The hostname the request came in on, for absolute `/_/m/` links. Only plain host characters.
fn host<'a>(uri: &'a Uri, headers: &'a HeaderMap) -> Option<&'a str> {
    let host = headers.get(HOST).and_then(|v| v.to_str().ok()).or_else(|| uri.authority().map(|a| a.as_str()))?;
    let valid = !host.is_empty()
        && host.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']'));
    valid.then_some(host)
}
