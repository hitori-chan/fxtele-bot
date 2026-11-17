//! `/_/v/{id}/{n}.mp4`: a post's video. With 1080p on, the file joined from the site's DASH
//! streams and kept in the bucket (`crate::video`), reached through a link signed just then. Only
//! requests from Telegram's addresses get the bucket's file or make the server download anything:
//! media IDs follow from a post's link, so anyone else could otherwise fetch it. Everyone else, and
//! anything that goes wrong, is sent to the site's own file.

use std::sync::Arc;
use std::time::Duration;

use axum::Extension;
use axum::extract::{Path, State};
use axum::response::Response;
use tracing::warn;

use super::Visitor;
use crate::app::App;
use crate::extract::Source;
use crate::stats::Media;
use crate::video::Videos;

/// How long a request takes at most before Telegram is sent to the 720p file: the public
/// listener gives up at 30 s, and fetching the post again may have taken some of it.
const WAIT: Duration = Duration::from_secs(25);
/// How long finding a file already made may take.
const LOOKUP_WAIT: Duration = Duration::from_secs(5);

pub async fn serve(
    State(app): State<Arc<App>>,
    Extension(visitor): Extension<Visitor>,
    Path((id, file)): Path<(String, String)>,
) -> Response {
    let start = std::time::Instant::now();
    let Some(index) = file.strip_suffix(".mp4").and_then(|n| n.parse::<usize>().ok()) else {
        return super::not_found();
    };
    let settings = app.settings.get();
    let ready = app.videos.ready(&settings) && visitor.in_telegram_ranges;
    // Looking it up may list the bucket (on first use, or once a failure's pause is over); a
    // bucket that stalls mustn't hold Telegram past its wait, so the site's video goes instead.
    let looked = if ready { tokio::time::timeout(LOOKUP_WAIT, app.videos.made(&id, index)).await } else { Ok(None) };
    // A bucket that can't list in time won't take an upload either: no build, the site's video.
    let stalled = looked.is_err();
    if let Ok(Some(name)) = looked
        && let Some(link) = app.videos.link(&name)
    {
        // Its name starts with its site: `facebook/…`.
        app.stats.media(Media::Video1080, name.split('/').next().and_then(Source::from_name));
        return super::redirect(&link);
    }

    let Some(post) = super::media::post_for(&app, &visitor, &id, index).await else { return super::not_found() };
    let Some(media) = post.media.get(index) else { return super::not_found() };
    let name = post.source().and_then(|source| Videos::file_name(source, &id, index));
    // Only a stream the preview would join: 1080p on, and its file within Telegram's limit.
    let hd = super::preview::joined(media, &settings).cloned().filter(|_| ready && !stalled);
    let (Some(hd), Some(name)) = (hd, name) else {
        if visitor.in_telegram_ranges {
            app.stats.media(Media::Video, post.source());
        }
        return super::redirect(&media.url);
    };
    // Its own task: a request that gives up mustn't hand the build to the next one, which would
    // make it twice.
    let made = {
        let (videos, name, url) = (app.videos.clone(), name.clone(), post.url.clone());
        tokio::spawn(async move { videos.get(&name, &hd, &url, &settings).await })
    };
    let made = tokio::time::timeout(WAIT.saturating_sub(start.elapsed()), made).await;
    let kind = if matches!(made, Ok(Ok(Ok(_)))) { Media::Video1080 } else { Media::Fallback };
    app.stats.media(kind, post.source());
    match made {
        Ok(Ok(Ok(link))) => super::redirect(&link),
        Ok(_) => super::redirect(&media.url),
        Err(_) => {
            warn!(file = name, "the 1080p video took too long, so Telegram got the 720p file");
            super::redirect(&media.url)
        }
    }
}
