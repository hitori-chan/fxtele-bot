//! The dashboard listener: login, settings, statistics and the playground (PLAN.md section 6).

pub mod auth;
mod events;
mod pages;

use std::net::SocketAddr;
use std::sync::Arc;

use askama::Template;
use axum::extract::{ConnectInfo, Path, Request, State};
use axum::http::header::{
    CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, REFERRER_POLICY, SET_COOKIE, USER_AGENT,
    X_CONTENT_TYPE_OPTIONS, X_FRAME_OPTIONS,
};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use serde::Deserialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;
use tower_http::set_header::SetResponseHeaderLayer;

use self::auth::{Auth, Identity};
use crate::app::App;

pub struct Dashboard {
    pub app: Arc<App>,
    pub auth: Auth,
    /// Ends live-update streams so shutdown doesn't wait on them.
    pub shutdown: CancellationToken,
}

type Shared = Arc<Dashboard>;

pub fn router(app: Arc<App>, shutdown: CancellationToken) -> anyhow::Result<Router> {
    let state = Arc::new(Dashboard { auth: Auth::new(&app.config)?, app, shutdown });
    let open = Router::new()
        .route("/login", get(login_page))
        .route("/login/password", post(login_password))
        .route("/login/passkey/start", post(passkey_start))
        .route("/login/passkey/finish", post(passkey_finish))
        .route("/setup/{token}", get(setup))
        .route("/static/{file}", get(static_file))
        .route("/_/health", get(|| async { "ok\n" }));
    let private = Router::new()
        .route("/", get(pages::overview))
        .route("/events", get(events::stream))
        .route("/api/series", get(events::series))
        .route("/facebook", get(pages::facebook).post(pages::save_facebook))
        .route("/facebook/cookies", post(pages::save_cookies))
        .route("/facebook/cookies/clear", post(pages::clear_cookies))
        .route("/limits", get(pages::limits).post(pages::save_limits))
        .route("/telegram", get(pages::telegram))
        .route("/telegram/refresh", post(pages::refresh_telegram))
        .route("/playground", get(pages::playground).post(pages::run_playground))
        .route("/failures", get(pages::failures))
        .route("/cache", get(pages::cache))
        .route("/cache/remove", post(pages::cache_remove))
        .route("/cache/clear", post(pages::cache_clear))
        .route("/tests", get(pages::tests).post(pages::save_tests))
        .route("/tests/run", post(pages::run_tests))
        .route("/security", get(pages::security))
        .route("/security/password", post(pages::set_password))
        .route("/security/password/remove", post(pages::remove_password))
        .route("/security/totp/start", post(pages::totp_start))
        .route("/security/totp/confirm", post(pages::totp_confirm))
        .route("/security/totp/cancel", post(pages::totp_cancel))
        .route("/security/totp/remove", post(pages::totp_remove))
        .route("/security/passkeys/start", post(pages::passkey_register_start))
        .route("/security/passkeys/finish", post(pages::passkey_register_finish))
        .route("/security/passkeys/{id}/remove", post(pages::remove_passkey))
        .route("/security/sessions/clear", post(pages::clear_sessions))
        .route("/logout", post(logout))
        .route_layer(from_fn_with_state(Arc::clone(&state), require_login));
    // Images and video come from Facebook's CDN in the playground's previews.
    let csp = "default-src 'self'; img-src 'self' https: data:; media-src https:; style-src 'self'; \
               script-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'";
    Ok(open
        .merge(private)
        .layer(from_fn_with_state(Arc::clone(&state), same_origin))
        .layer(SetResponseHeaderLayer::overriding(CONTENT_SECURITY_POLICY, HeaderValue::from_static(csp)))
        .layer(SetResponseHeaderLayer::overriding(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff")))
        .layer(SetResponseHeaderLayer::overriding(X_FRAME_OPTIONS, HeaderValue::from_static("DENY")))
        .layer(SetResponseHeaderLayer::overriding(REFERRER_POLICY, HeaderValue::from_static("no-referrer")))
        .layer(SetResponseHeaderLayer::if_not_present(CACHE_CONTROL, HeaderValue::from_static("no-store")))
        .with_state(state))
}

/// Changes (anything but GET and HEAD) must come from the dashboard's own pages.
async fn same_origin(State(d): State<Shared>, request: Request, next: Next) -> Response {
    if !matches!(*request.method(), Method::GET | Method::HEAD) && !d.auth.same_origin(request.headers()) {
        return (StatusCode::FORBIDDEN, "Refused: this request didn't come from the dashboard.\n").into_response();
    }
    next.run(request).await
}

async fn require_login(State(d): State<Shared>, mut request: Request, next: Next) -> Response {
    match d.auth.identify(&d.app.db, request.headers()).await {
        Some(identity) => {
            request.extensions_mut().insert(identity);
            next.run(request).await
        }
        None if request.method() == Method::GET => Redirect::to("/login").into_response(),
        None => (StatusCode::UNAUTHORIZED, Json(json!({ "error": "sign in first" }))).into_response(),
    }
}

/// The client's IP, the same way the public listener works it out.
fn client_ip(d: &Dashboard, peer: SocketAddr, headers: &HeaderMap) -> std::net::IpAddr {
    d.app.resolver.resolve(peer.ip(), headers).ip
}

fn user_agent(headers: &HeaderMap) -> &str {
    headers.get(USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or_default()
}

fn render(template: &impl Template) -> Response {
    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            tracing::error!("can't render a dashboard page: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong.\n").into_response()
        }
    }
}

fn with_cookie(mut response: Response, cookie: HeaderValue) -> Response {
    response.headers_mut().append(SET_COOKIE, cookie);
    response
}

#[derive(Template)]
#[template(path = "dashboard/login.html")]
struct LoginPage {
    password: bool,
    /// The password form also asks for an authenticator-app code.
    totp: bool,
    passkeys: bool,
    access: bool,
    error: Option<String>,
}

async fn login_view(d: &Dashboard, error: Option<String>) -> LoginPage {
    let password = auth::password::is_set(&d.app.db).await.unwrap_or(false);
    let totp = password && auth::totp::enabled_at(&d.app.db).await.ok().flatten().is_some();
    let passkeys = d.auth.passkeys.is_some() && auth::passkey::count(&d.app.db).await.unwrap_or(0) > 0;
    LoginPage { password, totp, passkeys, access: d.auth.access.is_some(), error }
}

async fn login_page(State(d): State<Shared>, headers: HeaderMap) -> Response {
    if d.auth.identify(&d.app.db, &headers).await.is_some() {
        return Redirect::to("/").into_response();
    }
    render(&login_view(&d, None).await)
}

#[derive(Deserialize)]
pub struct PasswordForm {
    pub password: String,
    /// The authenticator-app code, when that's on.
    #[serde(default)]
    pub code: String,
}

fn lockout_message(wait: std::time::Duration) -> String {
    format!("Too many failed attempts. Try again in {} minutes.", wait.as_secs().div_ceil(60).max(1))
}

async fn login_password(
    State(d): State<Shared>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Form(form): Form<PasswordForm>,
) -> Response {
    let ip = client_ip(&d, peer, &headers);
    if let Some(wait) = d.auth.locked(ip) {
        return (StatusCode::TOO_MANY_REQUESTS, render(&login_view(&d, Some(lockout_message(wait))).await))
            .into_response();
    }
    let db = &d.app.db;
    // The code is only checked (and used up) after the right password.
    let checked = async {
        if !auth::password::verify(db, &form.password).await? {
            return anyhow::Ok(None);
        }
        if auth::totp::enabled_at(db).await?.is_none() {
            return Ok(Some("password"));
        }
        Ok(auth::totp::verify(db, &form.code).await?.then_some("password + code"))
    };
    match checked.await {
        Ok(Some(method)) => {
            d.auth.succeeded(ip);
            match auth::create_session(db, method, ip, user_agent(&headers)).await {
                Ok(token) => with_cookie(Redirect::to("/").into_response(), auth::session_cookie(&token)),
                Err(e) => server_error(&e),
            }
        }
        Ok(None) => {
            d.auth.failed(ip);
            tracing::info!(%ip, "failed dashboard password sign-in");
            let mut page = login_view(&d, None).await;
            page.error = Some(if page.totp { "Wrong password or code." } else { "Wrong password." }.into());
            (StatusCode::UNAUTHORIZED, render(&page)).into_response()
        }
        Err(e) => server_error(&e),
    }
}

async fn passkey_start(
    State(d): State<Shared>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let Some(passkeys) = &d.auth.passkeys else {
        return json_error(StatusCode::NOT_FOUND, "passkeys need DASHBOARD_URL");
    };
    if let Some(wait) = d.auth.locked(client_ip(&d, peer, &headers)) {
        return json_error(StatusCode::TOO_MANY_REQUESTS, &lockout_message(wait));
    }
    match passkeys.start_login(&d.app.db).await {
        Ok((id, options)) => Json(json!({ "id": id, "options": options })).into_response(),
        Err(e) => json_error(StatusCode::BAD_REQUEST, &e.to_string()),
    }
}

#[derive(Deserialize)]
struct PasskeyLogin {
    id: String,
    credential: webauthn_rs::prelude::PublicKeyCredential,
}

async fn passkey_finish(
    State(d): State<Shared>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<PasskeyLogin>,
) -> Response {
    let Some(passkeys) = &d.auth.passkeys else {
        return json_error(StatusCode::NOT_FOUND, "passkeys need DASHBOARD_URL");
    };
    let ip = client_ip(&d, peer, &headers);
    if let Some(wait) = d.auth.locked(ip) {
        return json_error(StatusCode::TOO_MANY_REQUESTS, &lockout_message(wait));
    }
    if let Err(e) = passkeys.finish_login(&d.app.db, &body.id, &body.credential).await {
        d.auth.failed(ip);
        tracing::info!(%ip, "failed dashboard passkey sign-in: {e}");
        return json_error(StatusCode::UNAUTHORIZED, "that passkey didn't work");
    }
    d.auth.succeeded(ip);
    match auth::create_session(&d.app.db, "passkey", ip, user_agent(&headers)).await {
        Ok(token) => with_cookie(Json(json!({ "ok": true })).into_response(), auth::session_cookie(&token)),
        Err(e) => json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// A one-time setup link: signs in, then continues to the login settings.
async fn setup(
    State(d): State<Shared>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(token): Path<String>,
) -> Response {
    let ip = client_ip(&d, peer, &headers);
    if let Some(wait) = d.auth.locked(ip) {
        return (StatusCode::TOO_MANY_REQUESTS, lockout_message(wait)).into_response();
    }
    match auth::consume_setup_link(&d.app.db, &token).await {
        Ok(true) => match auth::create_session(&d.app.db, "setup link", ip, user_agent(&headers)).await {
            // A page rather than a redirect: the session cookie is SameSite=Strict, and a link
            // opened from elsewhere is a cross-site navigation, so continue from our own page.
            Ok(token) => with_cookie(
                Html(
                    "<!doctype html><meta charset=utf-8><meta http-equiv=refresh content=\"0; url=/security?done=welcome\">\
                     <title>Signed in</title><p>Signed in. <a href=\"/security?done=welcome\">Continue to login settings</a>.</p>",
                )
                .into_response(),
                auth::session_cookie(&token),
            ),
            Err(e) => server_error(&e),
        },
        Ok(false) => {
            d.auth.failed(ip);
            let error = "That setup link is invalid, expired or already used. Run `server setup-link` for a new one.";
            (StatusCode::NOT_FOUND, render(&login_view(&d, Some(error.into())).await)).into_response()
        }
        Err(e) => server_error(&e),
    }
}

async fn logout(State(d): State<Shared>, axum::Extension(identity): axum::Extension<Identity>) -> Response {
    if let Identity::Session { hash, .. } = identity
        && let Err(e) = auth::end_session(&d.app.db, hash).await
    {
        return server_error(&e);
    }
    with_cookie(Redirect::to("/login").into_response(), auth::clear_cookie())
}

const STATIC: [(&str, &str, &[u8]); 4] = [
    ("app.js", "text/javascript", include_bytes!("../../static/app.js")),
    ("app.css", "text/css", include_bytes!("../../static/app.css")),
    ("uplot.min.js", "text/javascript", include_bytes!("../../static/uplot.min.js")),
    ("uplot.min.css", "text/css", include_bytes!("../../static/uplot.min.css")),
];

async fn static_file(Path(file): Path<String>) -> Response {
    match STATIC.iter().find(|(name, _, _)| *name == file) {
        Some((_, kind, body)) => ([(CONTENT_TYPE, *kind), (CACHE_CONTROL, "max-age=3600")], *body).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

fn json_error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

fn server_error(error: &anyhow::Error) -> Response {
    tracing::error!("dashboard error: {error:#}");
    (StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong; see the server log.\n").into_response()
}

#[cfg(test)]
mod tests;
