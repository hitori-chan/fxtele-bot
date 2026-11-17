//! `/_/o/{signature}`: the oEmbed Discord reads for the lines above a card's title. It only says
//! back what a page signed, so nobody can put words on this host.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::app::App;

#[derive(Deserialize)]
pub struct Lines {
    /// The provider line: the site.
    p: String,
    /// The author line: a video's text, or the counts.
    #[serde(default)]
    a: String,
    /// The post.
    u: String,
}

#[derive(Serialize)]
struct OEmbed<'a> {
    version: &'static str,
    #[serde(rename = "type")]
    kind: &'static str,
    provider_name: &'a str,
    provider_url: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    author_name: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    author_url: &'a str,
}

/// What the signature covers, kept apart from `/_/l/` links, which must start with `http`.
fn signed(p: &str, a: &str, u: &str) -> String {
    format!("oembed\n{p}\n{a}\n{u}")
}

/// The link to `lines`' oEmbed on `host`.
pub fn link(app: &App, host: &str, provider: &str, author: Option<&str>, post: &str) -> String {
    let author = author.unwrap_or_default();
    let query: String = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("p", provider)
        .append_pair("a", author)
        .append_pair("u", post)
        .finish();
    format!("https://{host}/_/o/{}?{query}", app.cache.link_signature(&signed(provider, author, post)))
}

pub async fn lines(
    State(app): State<Arc<App>>,
    axum::extract::Path(signature): axum::extract::Path<String>,
    Query(q): Query<Lines>,
) -> Response {
    if !app.cache.link_signed(&signed(&q.p, &q.a, &q.u), &signature) {
        return super::not_found();
    }
    let author_url = if q.a.is_empty() { "" } else { q.u.as_str() };
    Json(OEmbed {
        version: "1.0",
        kind: "link",
        provider_name: &q.p,
        provider_url: &q.u,
        author_name: &q.a,
        author_url,
    })
    .into_response()
}
