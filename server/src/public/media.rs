//! `/_/m/{id}/{n}`: redirects to the current CDN URL of media item `n` (or its thumbnail with a
//! `t` suffix), refetching the post if that URL expired. Only ever redirects to media of posts
//! the server extracted itself.

use std::sync::Arc;

use axum::Extension;
use axum::extract::{Path, State};
use axum::response::Response;

use super::Visitor;
use crate::app::{App, Lookup};
use crate::cache::Entry;
use crate::extract::{Post, Target};

pub async fn redirect(
    State(app): State<Arc<App>>,
    Extension(visitor): Extension<Visitor>,
    Path((id, item)): Path<(String, String)>,
) -> Response {
    let (index, thumbnail) = match item.strip_suffix('t') {
        Some(index) => (index, true),
        None => (item.as_str(), false),
    };
    let Ok(index) = index.parse::<usize>() else { return super::not_found() };
    let Some(post) = post_for(&app, &visitor, &id, index).await else { return super::not_found() };
    let Some(media) = post.media.get(index) else { return super::not_found() };
    let url = if thumbnail { media.thumbnail.as_deref() } else { Some(media.url.as_str()) };
    url.map_or_else(super::not_found, super::redirect)
}

/// The post behind media ID `id`, refetched if media item `index`'s URL ran out. Only
/// Telegram's verified requests may trigger a fetch, as with previews.
pub(super) async fn post_for(app: &App, visitor: &Visitor, id: &str, index: usize) -> Option<Arc<Post>> {
    let key = app.cache.key_for_media_id(id).await?;
    let now = jiff::Timestamp::now().as_second();
    let usable = |post: &Post| post.media.get(index).is_some_and(|m| m.expires_at.is_none_or(|t| t > now + 60));
    let post = match app.cache.get(&key).await {
        Some(Entry { outcome: Ok(post), .. }) => Some(post),
        _ => None,
    };
    // An item the post doesn't have is no reason to fetch it again.
    let missing = post.as_deref().is_some_and(|p| index >= p.media.len());
    if missing || post.as_deref().is_some_and(usable) || !visitor.is_telegram {
        return post;
    }
    app.cache.remove(&key).await;
    match app.lookup(&Target::from_link(&key)?).await {
        Lookup::Hit(post) | Lookup::Extracted(post) => Some(post),
        Lookup::Failed(_) | Lookup::Busy => None,
    }
}
