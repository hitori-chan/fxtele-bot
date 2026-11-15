//! `/_/m/{id}/{n}`: redirects to the current CDN URL of media item `n` (or its thumbnail with a
//! `t` suffix), refetching the post if that URL expired. Only ever redirects to media of posts
//! the server extracted itself.

use std::sync::Arc;

use axum::Extension;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use super::Visitor;
use crate::app::{App, Lookup};
use crate::cache::Entry;
use crate::extract::Post;
use crate::extract::facebook::url::Target;

pub async fn redirect(
    State(app): State<Arc<App>>,
    Extension(visitor): Extension<Visitor>,
    Path((id, item)): Path<(String, String)>,
) -> Response {
    let not_found = || (StatusCode::NOT_FOUND, "Not found\n").into_response();
    let (index, thumbnail) = match item.strip_suffix('t') {
        Some(index) => (index, true),
        None => (item.as_str(), false),
    };
    let Ok(index) = index.parse::<usize>() else { return not_found() };
    let Some(key) = app.cache.key_for_media_id(&id).await else { return not_found() };

    let now = jiff::Timestamp::now().as_second();
    let usable = |post: &Post| post.media.get(index).is_some_and(|m| m.expires_at.is_none_or(|t| t > now + 60));
    let mut post = match app.cache.get(&key).await {
        Some(Entry { outcome: Ok(post), .. }) => Some(post),
        _ => None,
    };
    if !post.as_deref().is_some_and(usable) && visitor.is_telegram {
        // Only Telegram's verified requests may trigger a fetch, as with previews.
        app.cache.remove(&key).await;
        if let Some(target) = Target::from_link(&key) {
            post = match app.lookup(&target).await {
                Lookup::Hit(post) | Lookup::Extracted(post, _) => Some(post),
                Lookup::Failed(_) | Lookup::Busy => None,
            };
        }
    }
    let Some(media) = post.as_deref().and_then(|p| p.media.get(index)) else { return not_found() };
    let url = if thumbnail { media.thumbnail.as_deref() } else { Some(media.url.as_str()) };
    url.map_or_else(not_found, super::redirect)
}
