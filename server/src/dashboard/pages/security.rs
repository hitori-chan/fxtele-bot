//! The dashboard's security page.

use super::*;

struct PasskeyRow {
    id: String,
    name: String,
    added: String,
    used: String,
}

struct SessionView {
    /// The session's hash in hex, which names it in the sign-out link; the token can't be had from it.
    id: String,
    method: String,
    current: bool,
    since: String,
    seen: String,
    ip: String,
    agent: String,
}

/// An authenticator app being set up: what it scans, opens or has typed in.
struct TotpSetup {
    qr: String,
    size: u32,
    link: String,
    key: String,
    grouped: String,
}

#[derive(Template)]
#[template(path = "dashboard/security.html")]
pub struct SecurityPage {
    shell: Shell,
    /// When the password was last changed, if there is one.
    password: Option<String>,
    totp_since: Option<String>,
    totp_setup: Option<TotpSetup>,
    /// Why the code didn't turn the app on, shown by its field.
    totp_error: Option<String>,
    passkeys_enabled: bool,
    passkeys: Vec<PasskeyRow>,
    /// Cloudflare Access's team, and how this visit came in, when it's on.
    access: Option<(String, String)>,
    sessions: Vec<SessionView>,
}

async fn security_page(d: &Dashboard, shell: Shell, identity: &Identity) -> Result<SecurityPage> {
    let db = &d.app.db;
    let passkeys = auth::passkey::list(db)
        .await?
        .into_iter()
        .map(|p| PasskeyRow {
            id: data_encoding::HEXLOWER.encode(&p.id),
            name: p.name,
            added: day(p.created_at),
            used: p.last_used_at.map_or_else(|| "never".into(), ago),
        })
        .collect();
    let current = match identity {
        Identity::Session { hash, .. } => Some(hash.as_slice()),
        Identity::Access { .. } => None,
    };
    let sessions = auth::sessions(db)
        .await?
        .into_iter()
        .map(|s| SessionView {
            id: data_encoding::HEXLOWER.encode(&s.hash),
            current: current == Some(s.hash.as_slice()),
            method: s.method,
            since: when(s.created_at),
            seen: ago(s.last_seen_at),
            ip: s.ip,
            agent: browser(&s.user_agent),
        })
        .collect();
    let access = d.auth.access.as_ref().map(|access| {
        let how = match identity {
            Identity::Access { email } => format!("You're signed in through it as {email}."),
            Identity::Session { .. } => "This visit used a dashboard session instead.".into(),
        };
        (access.team_domain().to_string(), how)
    });
    let totp_since = auth::totp::enabled_at(db).await?.map(day);
    let totp_setup = match d.auth.totp_setup.current() {
        Some(secret) if totp_since.is_none() => {
            let account = d.app.config.dashboard_url.as_ref().and_then(|u| u.host_str()).unwrap_or("dashboard");
            let link = auth::totp::link(&secret, account);
            let (qr, size) = auth::totp::qr(&link)?;
            let key = auth::totp::key(&secret);
            Some(TotpSetup { qr, size, link, grouped: auth::totp::grouped(&key), key })
        }
        _ => None,
    };
    Ok(SecurityPage {
        shell: Shell { setup: identity.is_setup(), ..shell },
        password: auth::password::changed_at(db).await?.map(day),
        totp_since,
        totp_setup,
        totp_error: None,
        passkeys_enabled: d.auth.passkeys.is_some(),
        passkeys,
        access,
        sessions,
    })
}

pub async fn security(
    State(d): State<Shared>,
    Extension(identity): Extension<Identity>,
    Query(q): Query<Done>,
) -> Response {
    show(security_page(&d, Shell::new("security", &q), &identity).await)
}

async fn security_refused(d: &Dashboard, identity: &Identity, error: impl Into<String>) -> Response {
    match security_page(d, Shell::refused("security", error), identity).await {
        Ok(page) => refused(&page),
        Err(e) => server_error(&e),
    }
}

const NOT_RECENT: &str = "Changes to how you sign in need a recent sign-in: sign out, sign in again, then \
                          make the change within 15 minutes.";

/// After a change to how one signs in: every other browser out, and sign-ins waiting for a
/// code ended.
async fn signed_out_elsewhere(d: &Dashboard, identity: &Identity) -> Result<()> {
    d.auth.clear_challenges();
    let this = match identity {
        Identity::Session { hash, .. } => Some(hash.clone()),
        Identity::Access { .. } => None,
    };
    auth::end_other_sessions(&d.app.db, this).await
}

const LAST_LOGIN: &str =
    "That's the last way to sign in. Add another first (if you're locked out, `server setup-link` gets you back in).";

/// Whether a way to sign in other than the password is set up: a passkey, or Access.
async fn besides_password(d: &Dashboard) -> Result<bool> {
    let passkeys = d.auth.passkeys.is_some() && auth::passkey::count(&d.app.db).await? > 0;
    Ok(passkeys || d.auth.access.is_some())
}

/// Removing the password is refused when it's the last way in.
async fn password_needed(d: &Dashboard) -> Result<bool> {
    Ok(auth::password::is_set(&d.app.db).await? && !besides_password(d).await?)
}

#[derive(Deserialize)]
pub struct PasswordChange {
    #[serde(default)]
    current: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    confirm: String,
}

/// Asks for the password as it is before it's changed or removed, or the authenticator app turned
/// off, so a browser left signed in isn't enough. A setup link, there for when it's forgotten or
/// the phone is lost, skips this. Wrong guesses count toward the lockout, as at sign-in.
async fn check_current(d: &Dashboard, identity: &Identity, ip: std::net::IpAddr, current: &str) -> Result<(), String> {
    let set = auth::password::is_set(&d.app.db).await.map_err(|e| format!("{e:#}"))?;
    if identity.is_setup() || !set {
        // A first password, without one to type: only soon after signing in.
        return if set || identity.is_recent() { Ok(()) } else { Err(NOT_RECENT.into()) };
    }
    if current.is_empty() {
        return Err("Enter the current password first.".into());
    }
    let turn = auth::password::turn().await.ok_or("The server is busy; try again in a moment.")?;
    if let Some(wait) = d.auth.locked(ip) {
        return Err(crate::dashboard::lockout_message(wait));
    }
    match auth::password::verify(&d.app.db, current, turn).await {
        Ok(true) => Ok(()),
        Ok(false) => {
            d.auth.failed(ip);
            Err("The current password is wrong.".into())
        }
        Err(e) => Err(format!("{e:#}")),
    }
}

pub async fn set_password(
    State(d): State<Shared>,
    Extension(identity): Extension<Identity>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    Form(f): Form<PasswordChange>,
) -> Response {
    let ip = crate::dashboard::client_ip(&d, peer, &headers);
    if f.password != f.confirm {
        return security_refused(&d, &identity, "The new passwords don't match. Type the same one in both fields.")
            .await;
    }
    if let Err(error) = check_current(&d, &identity, ip, &f.current).await {
        return security_refused(&d, &identity, error).await;
    }
    if !f.current.is_empty() && f.current == f.password {
        return security_refused(&d, &identity, "That's the password already. Choose a new one.").await;
    }
    let saved = async {
        auth::password::set(&d.app.db, &f.password).await?;
        signed_out_elsewhere(&d, &identity).await
    };
    match saved.await {
        Ok(()) => done("/security", "password"),
        Err(e) => security_refused(&d, &identity, format!("{e:#}")).await,
    }
}

pub async fn remove_password(
    State(d): State<Shared>,
    Extension(identity): Extension<Identity>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    Form(f): Form<PasswordChange>,
) -> Response {
    let ip = crate::dashboard::client_ip(&d, peer, &headers);
    if let Err(error) = check_current(&d, &identity, ip, &f.current).await {
        return security_refused(&d, &identity, error).await;
    }
    match password_needed(&d).await {
        Ok(true) => return security_refused(&d, &identity, LAST_LOGIN).await,
        Ok(false) => {}
        Err(e) => return server_error(&e),
    }
    // It would take the app with it, which otherwise takes a code to turn off.
    match auth::totp::enabled_at(&d.app.db).await {
        Ok(Some(_)) if !identity.is_setup() => {
            let error = "Turn off the authenticator app first (it takes a code from it), then remove the password.";
            return security_refused(&d, &identity, error).await;
        }
        Ok(_) => {}
        Err(e) => return server_error(&e),
    }
    let removed = async {
        auth::password::remove(&d.app.db).await?;
        auth::totp::disable(&d.app.db).await?;
        signed_out_elsewhere(&d, &identity).await
    };
    match removed.await {
        Ok(()) => done("/security", "password_removed"),
        Err(e) => server_error(&e),
    }
}

pub async fn remove_passkey(
    State(d): State<Shared>,
    Extension(identity): Extension<Identity>,
    Path(id): Path<String>,
) -> Response {
    let Ok(id) = data_encoding::HEXLOWER_PERMISSIVE.decode(id.as_bytes()) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !identity.is_recent() {
        return security_refused(&d, &identity, NOT_RECENT).await;
    }
    let others = async {
        let password = auth::password::is_set(&d.app.db).await?;
        anyhow::Ok(password || auth::passkey::count(&d.app.db).await? > 1 || d.auth.access.is_some())
    };
    match others.await {
        Ok(false) => security_refused(&d, &identity, LAST_LOGIN).await,
        Ok(true) => {
            let removed = async {
                auth::passkey::remove(&d.app.db, id).await?;
                if let Some(passkeys) = &d.auth.passkeys {
                    passkeys.forget_sign_ins();
                }
                signed_out_elsewhere(&d, &identity).await
            };
            match removed.await {
                Ok(()) => done("/security", "passkey_removed"),
                Err(e) => server_error(&e),
            }
        }
        Err(e) => server_error(&e),
    }
}

pub async fn totp_start(State(d): State<Shared>, Extension(identity): Extension<Identity>) -> Response {
    match auth::totp::enabled_at(&d.app.db).await {
        Ok(Some(_)) => {
            return totp_refused(&d, &identity, "The app is on already; turn it off first to change it.").await;
        }
        Ok(None) if !identity.is_recent() => return totp_refused(&d, &identity, NOT_RECENT).await,
        Ok(None) => {}
        Err(e) => return server_error(&e),
    }
    d.auth.totp_setup.start();
    Redirect::to("/security#totp").into_response()
}

pub async fn totp_confirm(
    State(d): State<Shared>,
    Extension(identity): Extension<Identity>,
    Form(f): Form<CodeForm>,
) -> Response {
    if !identity.is_recent() {
        return totp_refused(&d, &identity, NOT_RECENT).await;
    }
    let Some(secret) = d.auth.totp_setup.current() else {
        return totp_refused(&d, &identity, "That setup expired; start again.").await;
    };
    match auth::totp::enable(&d.app.db, &secret, &f.code).await {
        Ok(true) => {
            d.auth.totp_setup.cancel();
            d.auth.clear_challenges();
            done("/security", "totp")
        }
        Ok(false) => {
            let error = "That code didn't match. Use the newest one, and check that the phone's clock is right.";
            totp_refused(&d, &identity, error).await
        }
        Err(e) => server_error(&e),
    }
}

async fn totp_refused(d: &Dashboard, identity: &Identity, error: &str) -> Response {
    match security_page(d, Shell::new("security", &Done::default()), identity).await {
        Ok(page) => refused(&SecurityPage { totp_error: Some(error.into()), ..page }),
        Err(e) => server_error(&e),
    }
}

pub async fn totp_cancel(State(d): State<Shared>) -> Response {
    d.auth.totp_setup.cancel();
    Redirect::to("/security").into_response()
}

/// Turning the app off takes a recent sign-in and a code from it (a setup link skips both, for a
/// lost phone). Wrong codes count as at sign-in: toward the address's lockout, and the pause
/// that wrong codes from anywhere start.
pub async fn totp_remove(
    State(d): State<Shared>,
    Extension(identity): Extension<Identity>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    Form(f): Form<CodeForm>,
) -> Response {
    let ip = crate::dashboard::client_ip(&d, peer, &headers);
    if !identity.is_setup() {
        let Some(_turn) = auth::Auth::code_turn().await else {
            return totp_refused(&d, &identity, "The server is busy; try again in a moment.").await;
        };
        if let Some(wait) = d.auth.locked(ip) {
            return totp_refused(&d, &identity, &crate::dashboard::lockout_message(wait)).await;
        }
        match auth::totp::verify(&d.app.db, &f.code).await {
            Ok(true) => {}
            Ok(false) => {
                d.auth.failed(ip);
                let error = "That code didn't match. Use the newest one the app shows.";
                return totp_refused(&d, &identity, error).await;
            }
            Err(e) => return server_error(&e),
        }
    }
    match auth::totp::disable(&d.app.db).await {
        Ok(()) => {
            d.auth.clear_challenges();
            done("/security", "totp_removed")
        }
        Err(e) => server_error(&e),
    }
}

#[derive(Deserialize)]
pub struct NewPasskey {
    #[serde(default)]
    name: String,
}

pub async fn passkey_register_start(
    State(d): State<Shared>,
    Extension(identity): Extension<Identity>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<NewPasskey>,
) -> Response {
    let Some(passkeys) = &d.auth.passkeys else {
        return json_error(StatusCode::NOT_FOUND, "passkeys need DASHBOARD_URL");
    };
    if !identity.is_recent() {
        return json_error(StatusCode::FORBIDDEN, NOT_RECENT);
    }
    let ip = crate::dashboard::client_ip(&d, peer, &headers);
    match passkeys.start_registration(&d.app.db, &body.name, ip).await {
        Ok((id, options)) => Json(json!({ "id": id, "options": options })).into_response(),
        Err(e) => json_error(StatusCode::BAD_REQUEST, &e.to_string()),
    }
}

#[derive(Deserialize)]
pub struct PasskeyRegistration {
    id: String,
    credential: RegisterPublicKeyCredential,
}

pub async fn passkey_register_finish(
    State(d): State<Shared>,
    Extension(identity): Extension<Identity>,
    Json(body): Json<PasskeyRegistration>,
) -> Response {
    let Some(passkeys) = &d.auth.passkeys else {
        return json_error(StatusCode::NOT_FOUND, "passkeys need DASHBOARD_URL");
    };
    match passkeys.finish_registration(&d.app.db, &body.id, &body.credential).await {
        Ok(()) => {
            crate::activity::record(&d.app.db, crate::activity::Kind::Change, &identity.actor(), "Added a passkey")
                .await;
            Json(json!({ "ok": true })).into_response()
        }
        Err(e) => json_error(StatusCode::BAD_REQUEST, &e.to_string()),
    }
}

/// Signs one browser out. Ending this browser's own session is signing out.
pub async fn end_session(
    State(d): State<Shared>,
    Extension(identity): Extension<Identity>,
    Path(id): Path<String>,
) -> Response {
    let Ok(hash) = data_encoding::HEXLOWER_PERMISSIVE.decode(id.as_bytes()) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let own = matches!(&identity, Identity::Session { hash: current, .. } if *current == hash);
    match auth::end_session(&d.app.db, hash).await {
        Ok(()) if own => with_cookie(Redirect::to("/login").into_response(), auth::clear_cookie()),
        Ok(()) => done("/security", "session_ended"),
        Err(e) => server_error(&e),
    }
}

/// Ends every session, this one included. An Access sign-in carries on.
pub async fn clear_sessions(State(d): State<Shared>, Extension(identity): Extension<Identity>) -> Response {
    d.auth.clear_challenges();
    match auth::end_other_sessions(&d.app.db, None).await {
        Ok(()) => {
            let text = "Signed every browser out";
            crate::activity::record(&d.app.db, crate::activity::Kind::Change, &identity.actor(), text).await;
            with_cookie(Redirect::to("/login").into_response(), auth::clear_cookie())
        }
        Err(e) => server_error(&e),
    }
}
