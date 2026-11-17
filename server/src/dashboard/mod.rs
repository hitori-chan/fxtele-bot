//! The dashboard listener: login, settings, statistics and the playground.

pub mod auth;
mod chart;
mod events;
mod pages;

use std::net::SocketAddr;
use std::sync::Arc;

use askama::Template;
use axum::extract::{ConnectInfo, Path, Request, State};
use axum::http::header::{
    CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, REFERRER_POLICY, SET_COOKIE, STRICT_TRANSPORT_SECURITY,
    X_CONTENT_TYPE_OPTIONS, X_FRAME_OPTIONS,
};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::middleware::{Next, from_fn, from_fn_with_state};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use serde::Deserialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;
use tower_http::compression::CompressionLayer;
use tower_http::set_header::SetResponseHeaderLayer;

use self::auth::{Auth, Identity};
use crate::activity::{self, Kind};
use crate::app::App;
use crate::client_ip::user_agent;

pub struct Dashboard {
    pub app: Arc<App>,
    pub auth: Auth,
    /// Ends live-update streams so shutdown doesn't wait on them.
    pub shutdown: CancellationToken,
    /// Links tried in the playground lately, newest first.
    pub tried: std::sync::Mutex<std::collections::VecDeque<String>>,
}

type Shared = Arc<Dashboard>;

pub fn router(app: Arc<App>, shutdown: CancellationToken) -> anyhow::Result<Router> {
    std::sync::LazyLock::force(&pages::STARTED);
    let state = Arc::new(Dashboard { auth: Auth::new(&app.config)?, app, shutdown, tried: Default::default() });
    let open = Router::new()
        .route("/login", get(login_page))
        .route("/login/password", post(login_password))
        .route("/login/code", get(login_code_page).post(login_code))
        .route("/login/passkey/start", post(passkey_start))
        .route("/login/passkey/finish", post(passkey_finish))
        .route("/setup/{token}", get(setup))
        .route("/static/{file}", get(static_file))
        .route("/_/health", get(|| async { "ok\n" }));
    let private = Router::new()
        .route("/", get(pages::overview))
        .route("/status", get(pages::status))
        .route("/events", get(events::stream))
        .route("/requests", get(pages::requests))
        .route("/issues", get(pages::issues_page))
        .route("/issues/fixed", post(pages::resolve_issue))
        .route("/activity", get(pages::activity_page))
        .route("/export/requests.csv", get(pages::export_requests))
        .route("/backup", get(pages::backup))
        .route("/backup/settings.json", get(pages::export_settings))
        .route("/backup/import", post(pages::import_settings))
        .route("/sites/{site}", get(pages::site_settings).post(pages::save_site))
        .route("/sites/{site}/cookies", post(pages::save_cookies))
        .route("/sites/{site}/cookies/clear", post(pages::clear_cookies))
        .route("/previews", get(pages::previews).post(pages::save_previews))
        .route("/videos", get(pages::videos).post(pages::save_videos))
        .route("/videos/delete", post(pages::delete_videos))
        .route("/limits", get(pages::limits).post(pages::save_limits))
        .route("/telegram", get(pages::telegram))
        .route("/telegram/refresh", post(pages::refresh_telegram))
        .route("/playground", get(pages::playground).post(pages::run_playground))
        .route("/playground/card/{file}", get(pages::playground_card))
        .route("/cache", get(pages::cache).post(pages::save_cache))
        .route("/cache/remove", post(pages::cache_remove))
        .route("/cache/clear", post(pages::cache_clear))
        .route("/tests", get(pages::tests))
        .route("/tests/add", post(pages::add_tests))
        .route("/tests/check", post(pages::check_test))
        .route("/tests/remove", post(pages::remove_test))
        .route("/tests/run", post(pages::run_tests))
        .route("/alerts", get(pages::alerts).post(pages::save_alerts))
        .route("/alerts/test", post(pages::test_alert))
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
        .route("/security/sessions/{id}/end", post(pages::end_session))
        .route("/logout", post(logout))
        .route_layer(from_fn_with_state(Arc::clone(&state), audit))
        .route_layer(from_fn_with_state(Arc::clone(&state), require_login));
    // Images and video come from the sites' CDNs in the playground's previews.
    let csp = "default-src 'self'; img-src 'self' https: data:; media-src https:; style-src 'self'; \
               script-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'";
    Ok(open
        .merge(private)
        .layer(from_fn(in_zone))
        .layer(from_fn_with_state(Arc::clone(&state), same_origin))
        .layer(SetResponseHeaderLayer::overriding(CONTENT_SECURITY_POLICY, HeaderValue::from_static(csp)))
        .layer(SetResponseHeaderLayer::overriding(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff")))
        .layer(SetResponseHeaderLayer::overriding(X_FRAME_OPTIONS, HeaderValue::from_static("DENY")))
        // Browsers ignore it over plain HTTP, so local use is unaffected.
        .layer(SetResponseHeaderLayer::overriding(
            STRICT_TRANSPORT_SECURITY,
            HeaderValue::from_static("max-age=31536000"),
        ))
        // No other site gets a handle on a dashboard window, or loads its files (but see `static_file`).
        .layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static("cross-origin-opener-policy"),
            HeaderValue::from_static("same-origin"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            HeaderName::from_static("cross-origin-resource-policy"),
            HeaderValue::from_static("same-origin"),
        ))
        // Not `no-referrer`: browsers then send `Origin: null` with forms, which `same_origin` refuses.
        .layer(SetResponseHeaderLayer::overriding(REFERRER_POLICY, HeaderValue::from_static("same-origin")))
        .layer(SetResponseHeaderLayer::if_not_present(CACHE_CONTROL, HeaderValue::from_static("no-store")))
        // The default leaves out event streams, so live updates aren't held back.
        .layer(CompressionLayer::new())
        .with_state(state))
}

tokio::task_local! {
    /// The time zone of the browser being answered, for the times on its pages.
    static ZONE: jiff::tz::TimeZone;
}

/// Answers in the browser's time zone, which app.js keeps in the `tz` cookie; UTC until it has.
async fn in_zone(request: Request, next: Next) -> Response {
    let zone = auth::cookie(request.headers(), "tz")
        .and_then(|name| {
            url::form_urlencoded::parse(format!("z={name}").as_bytes()).next().map(|(_, z)| z.into_owned())
        })
        .filter(|name| name.len() <= 64)
        .and_then(|name| jiff::tz::TimeZone::get(&name).ok())
        .unwrap_or(jiff::tz::TimeZone::UTC);
    ZONE.scope(zone, next.run(request)).await
}

/// The time zone pages show times in.
pub fn zone() -> jiff::tz::TimeZone {
    ZONE.try_with(Clone::clone).unwrap_or(jiff::tz::TimeZone::UTC)
}

/// The zone's name, as "Europe/Paris", for saying which times a page shows.
pub fn zone_name() -> String {
    zone().iana_name().unwrap_or("UTC").to_string()
}

/// `at` in the browser's time zone.
pub fn local(at: i64) -> Option<jiff::ZonedDateTime> {
    jiff::Timestamp::from_second(at).ok().map(|t| t.to_zoned_datetime(zone()))
}

/// Seconds the browser's clock is ahead of UTC at `at`.
pub fn offset(at: i64) -> i64 {
    jiff::Timestamp::from_second(at).map_or(0, |t| i64::from(zone().to_offset(t).seconds()))
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
        // A setup link only leads to setting up a way to sign in.
        Some(identity) if identity.is_setup() && !is_login_settings(request.uri().path()) => {
            if request.method() == Method::GET {
                Redirect::to("/security").into_response()
            } else {
                let message = "Signed in with a setup link, which only reaches the Security page. \
                               Sign in with a passkey, or a password and code, for the rest.\n";
                (StatusCode::FORBIDDEN, message).into_response()
            }
        }
        Some(identity) => {
            request.extensions_mut().insert(identity);
            next.run(request).await
        }
        // htmx would put the login page inside the current one; have it load the page instead.
        None if request.headers().contains_key("hx-request") => {
            (StatusCode::UNAUTHORIZED, [("hx-redirect", "/login")]).into_response()
        }
        None if request.method() == Method::GET => Redirect::to("/login").into_response(),
        None => (StatusCode::UNAUTHORIZED, Json(json!({ "error": "sign in first" }))).into_response(),
    }
}

/// Notes a sign-in, or a refused one, on the Activity page.
async fn signed_in(d: &Dashboard, method: &str, ip: std::net::IpAddr, headers: &HeaderMap, ok: bool) {
    let browser = pages::browser(user_agent(headers));
    let text =
        if ok { format!("Signed in from {browser}, {ip}") } else { format!("Refused a sign-in from {browser}, {ip}") };
    activity::record(&d.app.db, Kind::SignIn, method, &text).await;
}

/// Notes changes on the Activity page: anything posted that redirects with a `done` code says
/// what it did, and the identity says who.
async fn audit(State(d): State<Shared>, request: Request, next: Next) -> Response {
    let path = request.uri().path().to_string();
    let actor =
        request.extensions().get::<Identity>().map(Identity::actor).filter(|_| request.method() == Method::POST);
    let response = next.run(request).await;
    let code = response
        .headers()
        .get(axum::http::header::LOCATION)
        .and_then(|l| l.to_str().ok())
        .and_then(|l| l.split_once("done="))
        .map(|(_, code)| code.split('&').next().unwrap_or_default().to_string());
    if let (Some(actor), Some(code)) = (actor, code)
        && let Some(text) = pages::audit_text(&path, &code)
    {
        activity::record(&d.app.db, Kind::Change, &actor, &text).await;
    }
    response
}

fn is_login_settings(path: &str) -> bool {
    path == "/security" || path.starts_with("/security/") || path == "/logout"
}

/// The client's IP, the same way the public listener works it out.
pub(crate) fn client_ip(d: &Dashboard, peer: SocketAddr, headers: &HeaderMap) -> std::net::IpAddr {
    d.app.resolver.resolve(peer.ip(), headers).ip
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
    /// The authenticator app is on, so a code follows the password.
    code: bool,
    /// The password was right: this is the step that asks for the code.
    code_step: bool,
    passkeys: bool,
    access: bool,
    error: Option<String>,
}

async fn login_view(d: &Dashboard, error: Option<String>) -> LoginPage {
    let db = &d.app.db;
    let password = auth::password::is_set(db).await.unwrap_or(false);
    let code = auth::totp::enabled_at(db).await.ok().flatten().is_some();
    let passkeys = d.auth.passkeys.is_some() && auth::passkey::count(db).await.unwrap_or(0) > 0;
    LoginPage { password, code, code_step: false, passkeys, access: d.auth.access.is_some(), error }
}

/// The login page again, with why, and the second step's ticket gone.
async fn back_to_password(d: &Dashboard, status: StatusCode, error: &str) -> Response {
    let page = render(&login_view(d, Some(error.to_string())).await);
    with_cookie((status, page).into_response(), auth::clear_challenge_cookie())
}

async fn code_view(d: &Dashboard, status: StatusCode, error: Option<&str>) -> Response {
    let page = LoginPage { code_step: true, error: error.map(str::to_string), ..login_view(d, None).await };
    (status, render(&page)).into_response()
}

async fn login_page(State(d): State<Shared>, headers: HeaderMap) -> Response {
    if d.auth.identify(&d.app.db, &headers).await.is_some() {
        return Redirect::to("/").into_response();
    }
    render(&login_view(&d, None).await)
}

#[derive(Deserialize)]
pub struct PasswordForm {
    password: String,
}

#[derive(Deserialize)]
pub struct CodeForm {
    #[serde(default)]
    pub code: String,
}

const CODES_PAUSED: &str = "Password sign-in is paused for up to an hour after many wrong codes. \
                            Use a passkey, or `server setup-link`.";

pub(crate) fn lockout_message(wait: std::time::Duration) -> String {
    format!("Too many failed attempts. Try again in {} minutes.", wait.as_secs().div_ceil(60).max(1))
}

async fn login_password(
    State(d): State<Shared>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Form(form): Form<PasswordForm>,
) -> Response {
    let ip = client_ip(&d, peer, &headers);
    // A locked-out address doesn't queue for a turn; once it's this request's turn, the lockout
    // is looked at again, so a burst can't get past it.
    if let Some(wait) = d.auth.locked(ip) {
        return (StatusCode::TOO_MANY_REQUESTS, render(&login_view(&d, Some(lockout_message(wait))).await))
            .into_response();
    }
    let refusal = match auth::password::turn().await {
        None => Err("The server is busy; try again in a moment.".to_string()),
        Some(_) if let Some(wait) = d.auth.locked(ip) => Err(lockout_message(wait)),
        Some(_) if d.auth.codes_paused() => Err(CODES_PAUSED.to_string()),
        Some(turn) => Ok(turn),
    };
    let turn = match refusal {
        Ok(turn) => turn,
        Err(message) => {
            return (StatusCode::TOO_MANY_REQUESTS, render(&login_view(&d, Some(message)).await)).into_response();
        }
    };
    let db = &d.app.db;
    let checked = async {
        let right = auth::password::verify(db, &form.password, turn).await?;
        anyhow::Ok((right, auth::totp::enabled_at(db).await?.is_some()))
    };
    match checked.await {
        // The code is asked for on its own page, only once the password is right.
        Ok((true, true)) => {
            let ticket = d.auth.start_challenge();
            with_cookie(Redirect::to("/login/code").into_response(), auth::challenge_cookie(&ticket))
        }
        Ok((true, false)) => start_session(&d, "password", ip, &headers).await,
        Ok((false, _)) => {
            d.auth.failed(ip);
            tracing::info!(%ip, "failed dashboard password sign-in");
            signed_in(&d, "password", ip, &headers, false).await;
            (StatusCode::UNAUTHORIZED, render(&login_view(&d, Some("Wrong password.".into())).await)).into_response()
        }
        Err(e) => server_error(&e),
    }
}

/// Signs in after the last step: the session, its cookie, and a line on the Activity page.
async fn start_session(d: &Dashboard, method: &'static str, ip: std::net::IpAddr, headers: &HeaderMap) -> Response {
    d.auth.succeeded(ip);
    signed_in(d, method, ip, headers, true).await;
    match auth::create_session(&d.app.db, method, ip, user_agent(headers)).await {
        Ok(token) => with_cookie(Redirect::to("/").into_response(), auth::session_cookie(&token)),
        Err(e) => server_error(&e),
    }
}

/// The second step, for a browser whose password was right.
async fn login_code_page(State(d): State<Shared>, headers: HeaderMap) -> Response {
    if d.auth.identify(&d.app.db, &headers).await.is_some() {
        return Redirect::to("/").into_response();
    }
    match d.auth.challenge(&headers) {
        Some(_) => code_view(&d, StatusCode::OK, None).await,
        None => Redirect::to("/login").into_response(),
    }
}

async fn login_code(
    State(d): State<Shared>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Form(form): Form<CodeForm>,
) -> Response {
    let ip = client_ip(&d, peer, &headers);
    // Signed in already: a second send of the same form (a password manager's, say).
    if d.auth.identify(&d.app.db, &headers).await.is_some() {
        return Redirect::to("/").into_response();
    }
    // Codes are checked one at a time, and the limits once it's this request's turn.
    let Some(_turn) = Auth::code_turn().await else {
        return code_view(&d, StatusCode::TOO_MANY_REQUESTS, Some("The server is busy; try again in a moment.")).await;
    };
    let Some(ticket) = d.auth.challenge(&headers) else {
        return back_to_password(&d, StatusCode::UNAUTHORIZED, "That sign-in expired. Enter the password again.").await;
    };
    if let Some(wait) = d.auth.locked(ip) {
        return code_view(&d, StatusCode::TOO_MANY_REQUESTS, Some(&lockout_message(wait))).await;
    }
    if d.auth.codes_paused() {
        d.auth.end_challenge(&ticket);
        return back_to_password(&d, StatusCode::TOO_MANY_REQUESTS, CODES_PAUSED).await;
    }
    match auth::totp::verify(&d.app.db, &form.code).await {
        Ok(true) => {
            d.auth.end_challenge(&ticket);
            let response = start_session(&d, "password + code", ip, &headers).await;
            with_cookie(response, auth::clear_challenge_cookie())
        }
        Ok(false) => {
            d.auth.wrong_code();
            d.auth.failed(ip);
            tracing::info!(%ip, "wrong authenticator code at dashboard sign-in");
            signed_in(&d, "password + code", ip, &headers, false).await;
            if d.auth.challenge_missed(&ticket) {
                let error = "Wrong code. Use the newest one, and check that the phone's clock is right.";
                code_view(&d, StatusCode::UNAUTHORIZED, Some(error)).await
            } else {
                back_to_password(&d, StatusCode::UNAUTHORIZED, "Too many wrong codes. Enter the password again.").await
            }
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
    match passkeys.start_login(&d.app.db, client_ip(&d, peer, &headers)).await {
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
        signed_in(&d, "passkey", ip, &headers, false).await;
        return json_error(StatusCode::UNAUTHORIZED, "that passkey didn't work");
    }
    d.auth.succeeded(ip);
    signed_in(&d, "passkey", ip, &headers, true).await;
    match auth::create_session(&d.app.db, "passkey", ip, user_agent(&headers)).await {
        Ok(token) => with_cookie(Json(json!({ "ok": true })).into_response(), auth::session_cookie(&token)),
        Err(e) => json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// A one-time setup link: signs in, then continues to the Security page.
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
        Ok(true) => match auth::create_session(&d.app.db, auth::SETUP_METHOD, ip, user_agent(&headers)).await {
            // A page rather than a redirect: the session cookie is SameSite=Strict, and a link
            // opened from elsewhere is a cross-site navigation, so continue from our own page.
            Ok(token) => with_cookie(
                Html(format!(
                    "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width, initial-scale=1\">\
                     <meta http-equiv=refresh content=\"0; url=/security\"><title>Signed in</title>\
                     <link rel=icon href=\"{}\"><link rel=stylesheet href=\"{}\"><main class=auth><div class=auth-card><div class=card><div class=auth-head>\
                     <h1>Signed in</h1><p><a href=\"/security\">Continue to the Security page</a></p></div></div></div></main>",
                    asset("favicon.png"),
                    asset("app.css")
                ))
                .into_response(),
                auth::session_cookie(&token),
            ),
            Err(e) => server_error(&e),
        },
        // Not counted toward the lockout: links can't be guessed, and any web page could make a
        // browser open a wrong one and lock its owner out.
        Ok(false) => {
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

const STATIC: [(&str, &str, &[u8]); 8] = [
    ("app.js", "text/javascript", include_bytes!("../../static/app.js")),
    ("app.css", "text/css", include_bytes!("../../static/app.css")),
    ("iv.css", "text/css", include_bytes!("../../static/iv.css")),
    ("htmx.min.js", "text/javascript", include_bytes!("../../static/htmx.min.js")),
    ("icons.svg", "image/svg+xml", include_bytes!("../../static/icons.svg")),
    ("favicon.png", "image/png", include_bytes!("../../static/favicon.png")),
    ("homura.webp", "image/webp", include_bytes!("../../static/homura.webp")),
    ("mascot.webp", "image/webp", include_bytes!("../../static/mascot.webp")),
];

/// A static file's URL, with a hash of this build's files so a new version is never read from
/// the browser's cache.
pub fn asset(name: &str) -> String {
    static VERSION: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        use sha2::{Digest, Sha256};
        let mut hash = Sha256::new();
        for (_, _, body) in STATIC {
            hash.update(body);
        }
        data_encoding::HEXLOWER.encode(&hash.finalize()[..6])
    });
    format!("/static/{name}?v={}", *VERSION)
}

/// An icon from the sprite, for templates (`{{ crate::dashboard::icon("trash")|safe }}`). Names
/// are written in the templates, never taken from a request.
pub fn icon(name: &str) -> String {
    static SPRITE: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| asset("icons.svg"));
    format!(r#"<svg class="icon" aria-hidden="true"><use href="{}#{name}"></use></svg>"#, *SPRITE)
}

async fn static_file(Path(file): Path<String>) -> Response {
    match STATIC.iter().find(|(name, _, _)| *name == file) {
        // Pages link them with a hash of the build's files, so a copy never goes stale.
        Some((name, kind, body)) => {
            // The playground's sandboxed frame has no origin, so to the browser its stylesheet is
            // another site's: that one public file may be loaded from anywhere.
            let corp = if *name == "iv.css" { "cross-origin" } else { "same-origin" };
            let headers = [
                (CONTENT_TYPE, *kind),
                (CACHE_CONTROL, "public, max-age=31536000, immutable"),
                (HeaderName::from_static("cross-origin-resource-policy"), corp),
            ];
            (headers, *body).into_response()
        }
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
