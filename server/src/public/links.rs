//! `/_/l/{signature}?u=<link>`: the links in Instant View text. Telegram gets 403, so its
//! Instant View builder never visits other sites, where one failing would break the whole article
//! (FxEmbed does the same). Only links the server signed itself work. Facebook's own (hashtags) are
//! redirected to; any other site gets a page that names it, with a link on: anyone can put a link
//! in a public post and have it signed, so a redirect would lend this host to phishing.

use std::sync::Arc;

use askama::Template;
use axum::Extension;
use axum::extract::{Path, State};
use axum::http::header::{CONTENT_SECURITY_POLICY, REFERRER_POLICY};
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};

use super::Visitor;
use crate::app::App;

#[derive(Template)]
#[template(path = "leaving.html")]
struct Leaving<'a> {
    host: &'a str,
    link: &'a str,
}

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
    let Ok(url) = url::Url::parse(&link) else { return super::not_found() };
    let host = url.host_str().unwrap_or_default();
    if host == "www.facebook.com" {
        return super::redirect(&link);
    }
    match (Leaving { host, link: url.as_str() }).render() {
        Ok(page) => (
            [
                (CONTENT_SECURITY_POLICY, "default-src 'none'; style-src 'unsafe-inline'"),
                (REFERRER_POLICY, "no-referrer"),
            ],
            super::html(page),
        )
            .into_response(),
        Err(_) => super::not_found(),
    }
}
