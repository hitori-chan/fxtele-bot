//! `/_/g/{id}.jpg`: the first photos of a post as one picture for its card (`crate::mosaic`). It
//! downloads the photos, so only Telegram's addresses get one built, and Discord from its share of
//! the site's budget; everyone else, and any failure, is sent to the first photo. Only posts the
//! server extracted itself are served.

use std::sync::Arc;
use std::time::Duration;

use axum::Extension;
use axum::extract::{Path, State};
use axum::http::header::CONTENT_TYPE;
use axum::response::{IntoResponse, Response};
use tracing::warn;

use super::Visitor;
use crate::app::App;
use crate::extract::{MediaKind, Source};
use crate::stats::Media;

/// Telegram waits a few seconds for a card's picture; past this, the first photo will do.
const WAIT: Duration = Duration::from_secs(6);

pub async fn serve(
    State(app): State<Arc<App>>,
    Extension(visitor): Extension<Visitor>,
    Path(file): Path<String>,
) -> Response {
    let Some(id) = file.strip_suffix(".jpg") else { return super::not_found() };
    let jpeg = |bytes| ([(CONTENT_TYPE, "image/jpeg")], bytes).into_response();
    if let Some(made) = app.mosaics.get(id).await {
        if visitor.is_telegram {
            let source = app.cache.key_for_media_id(id).await.and_then(|key| Source::of_link(&key));
            app.stats.media(Media::Mosaic, source);
        }
        return jpeg(made);
    }
    let Some(post) = super::media::post_for(&app, &visitor, id, 0).await else { return super::not_found() };
    let Some(first) = post.media.first() else { return super::not_found() };
    let source = post.source().unwrap_or(Source::Facebook);
    let settings = app.settings.get();
    // Anything but a mosaic is the first photo, counted as one when Telegram asked.
    let photo = || {
        if visitor.is_telegram {
            app.stats.media(Media::Image, post.source());
        }
        super::redirect(&first.url)
    };
    // Photos only: a video's file isn't a picture to draw.
    let photos = post.media.len() >= 2 && post.media.iter().all(|m| m.kind == MediaKind::Photo);
    if !photos || !(visitor.in_telegram_ranges || visitor.is_discord) {
        return photo();
    }
    // Discord's builds come out of its share, like its fetches.
    let _share = if visitor.in_telegram_ranges {
        None
    } else {
        let Some(permit) = app.discord_share(source, &settings).await else { return photo() };
        Some(permit)
    };
    let Ok(client) = app.videos.client(settings.source(source).proxy.as_deref()) else { return photo() };
    match tokio::time::timeout(WAIT, app.mosaics.build(id, &post, &client)).await {
        Ok(Ok(made)) => {
            if visitor.is_telegram {
                app.stats.media(Media::Mosaic, post.source());
            }
            jpeg(made)
        }
        Ok(Err(e)) => {
            warn!(url = %post.key, "no mosaic, so the first photo: {e:#}");
            photo()
        }
        Err(_) => photo(),
    }
}
