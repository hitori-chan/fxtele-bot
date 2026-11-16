//! `/_/v/{id}/{n}.mp4`: a video above 720p, joined from Facebook's DASH files and kept in the
//! bucket (`crate::video`), which this redirects to through a link signed just then. Only requests
//! from Telegram's addresses get the bucket's file or make the server download anything: media IDs
//! follow from a post's link, so anyone else could otherwise fetch it. Everyone else, and anything
//! that goes wrong, gets Facebook's own 720p file.

use std::sync::Arc;
use std::time::Duration;

use axum::Extension;
use axum::extract::{Path, State};
use axum::response::Response;
use tracing::warn;

use super::Visitor;
use crate::app::App;
use crate::video::Videos;

/// How long a request waits for the file to be made; the public listener gives up at 30 s.
const WAIT: Duration = Duration::from_secs(25);

pub async fn serve(
    State(app): State<Arc<App>>,
    Extension(visitor): Extension<Visitor>,
    Path((id, file)): Path<(String, String)>,
) -> Response {
    let Some(index) = file.strip_suffix(".mp4").and_then(|n| n.parse::<usize>().ok()) else {
        return super::not_found();
    };
    let Some(name) = Videos::file_name(&id, index) else { return super::not_found() };
    let settings = app.settings.get();
    let ready = app.videos.ready(&settings) && visitor.in_telegram_ranges;
    if ready
        && app.videos.size(&name).await.is_some()
        && let Some(link) = app.videos.link(&name)
    {
        return super::redirect(&link);
    }

    let Some(post) = super::media::post_for(&app, &visitor, &id, index).await else { return super::not_found() };
    let Some(media) = post.media.get(index) else { return super::not_found() };
    let Some(hd) = media.hd.clone().filter(|_| ready) else { return super::redirect(&media.url) };
    // Its own task: a request that gives up mustn't hand the build to the next one, which would
    // make it twice.
    let made = {
        let (videos, name, url) = (app.videos.clone(), name.clone(), post.url.clone());
        tokio::spawn(async move { videos.get(&name, &hd, &url, &settings).await })
    };
    match tokio::time::timeout(WAIT, made).await {
        Ok(Ok(Ok(link))) => super::redirect(&link),
        Ok(_) => super::redirect(&media.url),
        Err(_) => {
            warn!(file = name, "the 1080p video took too long, so Telegram got the 720p file");
            super::redirect(&media.url)
        }
    }
}
