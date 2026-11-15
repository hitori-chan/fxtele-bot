//! `/{any facebook path}`: the preview page for Telegram, a redirect for everyone else.

use std::sync::Arc;
use std::time::Instant;

use axum::Extension;
use axum::extract::State;
use axum::http::header::HOST;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use tracing::info;

use super::{Visitor, elapsed, html, redirect};
use crate::app::{App, Lookup};
use crate::extract::Post;
use crate::extract::facebook::url::Target;
use crate::render::{self, MediaUrl};
use crate::settings::{FailureResponse, MediaLinks, Settings};
use crate::stats::Outcome;

pub async fn handle(
    State(app): State<Arc<App>>,
    Extension(visitor): Extension<Visitor>,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    let start = Instant::now();
    let path = uri.path_and_query().map_or("/", |p| p.as_str());
    let Some(target) = Target::from_request(path) else {
        return (StatusCode::NOT_FOUND, "Not found\n").into_response();
    };
    if !visitor.is_telegram || uri.path() == "/" {
        app.stats.record(Outcome::Redirect, Some(target.page_type), elapsed(start));
        return redirect(target.original.as_str());
    }

    let settings = app.settings.get();
    let (outcome, response) = match app.lookup(&target).await {
        Lookup::Hit(post) => (Outcome::Hit, page(&app, &post, &settings, &target, host(&uri, &headers)).await),
        Lookup::Extracted(post, _) => {
            (Outcome::Extracted, page(&app, &post, &settings, &target, host(&uri, &headers)).await)
        }
        Lookup::Failed(error) => (Outcome::Failed(error.stage), failure(&settings, &target)),
        Lookup::Busy => (Outcome::Busy, failure(&settings, &target)),
    };
    let took = elapsed(start);
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
    let media = match (settings.media_links, host) {
        (MediaLinks::Redirect, Some(host)) => {
            let id = app.cache.media_id(&post.key).await;
            (0..post.media.len())
                .map(|i| MediaUrl {
                    src: format!("https://{host}/_/m/{id}/{i}"),
                    poster: post.media[i].thumbnail.as_ref().map(|_| format!("https://{host}/_/m/{id}/{i}t")),
                })
                .collect::<Vec<_>>()
        }
        _ => post.media.iter().map(|m| MediaUrl { src: m.url.clone(), poster: m.thumbnail.clone() }).collect(),
    };
    html(render::preview(post, &media, settings, target.original.as_str()))
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
