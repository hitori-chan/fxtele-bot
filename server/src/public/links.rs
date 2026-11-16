//! `/_/l/{signature}?u=<link>`: the links in Instant View text. People are sent on to the link;
//! Telegram gets 403, so its Instant View builder never visits other sites, where one failing
//! would break the whole article (FxEmbed does the same). Only links the server signed itself
//! are followed, so this is no open redirect.

use std::sync::Arc;

use axum::Extension;
use axum::extract::{Path, State};
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};

use super::Visitor;
use crate::app::App;

pub async fn go(
    State(app): State<Arc<App>>,
    Extension(visitor): Extension<Visitor>,
    Path(signature): Path<String>,
    uri: Uri,
) -> Response {
    let link = url::form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes())
        .find_map(|(name, value)| (name == "u").then(|| value.into_owned()));
    let Some(link) = link.filter(|l| l.starts_with("https://") || l.starts_with("http://")) else {
        return super::not_found();
    };
    if !app.cache.link_signed(&link, &signature) {
        return super::not_found();
    }
    if visitor.in_telegram_ranges {
        return (StatusCode::FORBIDDEN, "Links are for people, not Telegram's fetcher.\n").into_response();
    }
    super::redirect(&link)
}
