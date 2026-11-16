//! `/_/g/{id}.jpg`: the first photos of a post as one picture for its card (`crate::mosaic`). It
//! downloads the photos, so only Telegram's addresses get one built; everyone else, and any
//! failure, is sent to the first photo. Only posts the server extracted itself are served.

use std::sync::Arc;
use std::time::Duration;

use axum::Extension;
use axum::extract::{Path, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::response::{IntoResponse, Response};
use tracing::warn;

use super::Visitor;
use crate::app::App;

/// Telegram waits a few seconds for a card's picture; past this, the first photo will do.
const WAIT: Duration = Duration::from_secs(6);

pub async fn serve(
    State(app): State<Arc<App>>,
    Extension(visitor): Extension<Visitor>,
    Path(file): Path<String>,
) -> Response {
    let Some(id) = file.strip_suffix(".jpg") else { return super::not_found() };
    let jpeg = |bytes| ([(CONTENT_TYPE, "image/jpeg"), (CACHE_CONTROL, "no-store")], bytes).into_response();
    if let Some(made) = app.mosaics.get(id).await {
        return jpeg(made);
    }
    let Some(post) = super::media::post_for(&app, &visitor, id, 0).await else { return super::not_found() };
    let Some(first) = post.media.first() else { return super::not_found() };
    let source = post.source().unwrap_or(crate::extract::Source::Facebook);
    let settings = app.settings.get();
    let client = match app.videos.client(settings.source(source).proxy.as_deref()) {
        Ok(client) if visitor.in_telegram_ranges && post.media.len() >= 2 => client,
        _ => return super::redirect(&first.url),
    };
    match tokio::time::timeout(WAIT, app.mosaics.build(id, &post, &client)).await {
        Ok(Ok(made)) => jpeg(made),
        Ok(Err(e)) => {
            warn!(url = %post.key, "no mosaic, so the first photo: {e:#}");
            super::redirect(&first.url)
        }
        Err(_) => super::redirect(&first.url),
    }
}
