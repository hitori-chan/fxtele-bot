//! Dashboard login: the password, then a code from an authenticator app once that's on (a second
//! step, held by a ticket for that browser alone), a passkey (which verifies the person with a PIN
//! or biometric), or Cloudflare Access (whose policy decides). Setup links, made on the server's
//! command line, only lead to setting one up. Also sessions, `Origin` checks and the lockout for
//! failed attempts.

pub mod access;
pub mod passkey;
pub mod password;
pub mod totp;

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::Result;
use axum::http::header::{COOKIE, HOST, ORIGIN};
use axum::http::{HeaderMap, HeaderValue};
use rusqlite::OptionalExtension;
use sha2::{Digest, Sha256};

use crate::client_ip::network;
use crate::config::Config;
use crate::db::Db;
use crate::sync::lock;

pub const COOKIE_NAME: &str = "__Host-session";
/// Sessions end a day after signing in, or after an hour unused, whichever comes first.
const SESSION_SECONDS: i64 = 86_400;
const IDLE_SECONDS: i64 = 3600;
/// How a setup link signs in: only the Security page, for an hour.
pub const SETUP_METHOD: &str = "setup link";
const SETUP_SESSION_SECONDS: i64 = 3600;
const SETUP_SECONDS: i64 = 30 * 60;
const STRIKES: u32 = 5;
/// Wrong authenticator codes after the right password, from anywhere, before password sign-in
/// pauses for [`PAUSE`]: per-address lockouts alone let someone who has the password guess codes
/// from many addresses.
const CODE_STRIKES: u32 = 10;
/// Wrong passwords from anywhere before it pauses, for guesses spread over many addresses.
const PASSWORD_STRIKES: u32 = 100;
/// The window the strikes above are counted in, and how long the pause lasts.
const PAUSE: Duration = Duration::from_secs(3600);
/// The second step's cookie: the password was right, and the code is still to come.
const CHALLENGE_COOKIE: &str = "__Host-signin";
/// How long the second step waits for its code, and how many wrong ones end it.
const CHALLENGE_SECONDS: u64 = 300;
const CHALLENGE_TRIES: u32 = 5;
/// Changes to how one signs in (passkeys, the authenticator app, a first password) need a
/// sign-in this recent, so a session cookie someone took can't add its own way in.
const RECENT_SECONDS: i64 = 15 * 60;

/// Who is signed in, for this request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Identity {
    /// A dashboard session: its token's hash, how it signed in, and when.
    Session {
        hash: Vec<u8>,
        method: String,
        since: i64,
    },
    Access {
        email: String,
    },
}

impl Identity {
    /// This session, or this Access user, apart from any other.
    pub fn owner(&self) -> Vec<u8> {
        match self {
            Self::Session { hash, .. } => [b"session:".as_slice(), hash].concat(),
            Self::Access { email } => [b"access:".as_slice(), email.as_bytes()].concat(),
        }
    }

    /// Who, for the Activity page: how they signed in, or Cloudflare Access's email.
    pub fn actor(&self) -> String {
        match self {
            Self::Session { method, .. } => method.clone(),
            Self::Access { email } => email.clone(),
        }
    }

    /// Signed in with a setup link: it may only set up a way to sign in.
    pub fn is_setup(&self) -> bool {
        matches!(self, Self::Session { method, .. } if method == SETUP_METHOD)
    }

    /// Signed in within [`RECENT_SECONDS`] (or through Access, whose policy decides, or a setup
    /// link, which lasts an hour and exists to set a way in up).
    pub fn is_recent(&self) -> bool {
        match self {
            Self::Session { since, .. } => self.is_setup() || now() - since < RECENT_SECONDS,
            Self::Access { .. } => true,
        }
    }
}

/// A sign-in waiting for its code.
#[derive(Debug)]
struct Challenge {
    expires: Instant,
    tries: u32,
}

#[derive(Debug)]
/// Failures from anywhere, counted for an hour from the first; reaching the limit pauses
/// password sign-in for an hour.
struct Pause {
    started: Instant,
    count: u32,
    until: Option<Instant>,
}

impl Pause {
    fn new() -> Self {
        Self { started: Instant::now(), count: 0, until: None }
    }

    /// Counts one failure: whether it's the one that starts the pause.
    fn strike(&mut self, limit: u32) -> bool {
        let now = Instant::now();
        if now.duration_since(self.started) > PAUSE {
            (self.started, self.count) = (now, 0);
        }
        self.count += 1;
        if self.count < limit {
            return false;
        }
        *self = Self { started: now, count: 0, until: Some(now + PAUSE) };
        true
    }

    fn on(&self) -> bool {
        self.until.is_some_and(|until| until > Instant::now())
    }
}

struct Strikes {
    window: Instant,
    count: u32,
    lockouts: u32,
    locked_until: Option<Instant>,
}

pub struct Auth {
    pub passkeys: Option<passkey::Passkeys>,
    pub access: Option<access::AccessVerifier>,
    pub totp_setup: totp::Setup,
    origin: Option<String>,
    strikes: Mutex<HashMap<IpAddr, Strikes>>,
    /// Wrong codes after the right password, and wrong passwords, from anywhere.
    code_strikes: Mutex<Pause>,
    password_strikes: Mutex<Pause>,
    /// Sign-ins waiting for their code, by their ticket's hash.
    challenges: Mutex<HashMap<Vec<u8>, Challenge>>,
}

impl Auth {
    pub fn new(config: &Config) -> Result<Self> {
        Ok(Self {
            passkeys: config.dashboard_url.as_ref().map(passkey::Passkeys::new).transpose()?,
            access: config.access.as_ref().map(access::AccessVerifier::new).transpose()?,
            totp_setup: totp::Setup::default(),
            origin: config.dashboard_url.as_ref().map(|url| url.origin().ascii_serialization()),
            strikes: Mutex::new(HashMap::new()),
            code_strikes: Mutex::new(Pause::new()),
            password_strikes: Mutex::new(Pause::new()),
            challenges: Mutex::new(HashMap::new()),
        })
    }

    /// The signed-in identity: a valid Access token, or a live session cookie. `used` keeps the
    /// session from going idle.
    pub async fn identify(&self, db: &Db, headers: &HeaderMap, used: bool) -> Option<Identity> {
        if let Some(access) = &self.access
            && let Some(email) = access.verify(headers).await
        {
            return Some(Identity::Access { email });
        }
        let token = cookie(headers, COOKIE_NAME)?;
        let hash = hash(&token);
        let now = now();
        let row: Option<(String, i64, i64)> = db
            .call({
                let hash = hash.clone();
                move |c| {
                    c.query_row(
                        "SELECT method, last_seen_at, created_at FROM sessions
                         WHERE token_hash = ?1 AND expires_at > ?2 AND last_seen_at > ?2 - ?3",
                        (hash, now, IDLE_SECONDS),
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .optional()
                }
            })
            .await
            .ok()
            .flatten();
        let (method, last_seen, since) = row?;
        if used && now - last_seen >= 60 {
            // Noted at most once a minute.
            let hash = hash.clone();
            let _ = db
                .call(move |c| c.execute("UPDATE sessions SET last_seen_at = ?2 WHERE token_hash = ?1", (hash, now)))
                .await;
        }
        Some(Identity::Session { hash, method, since })
    }

    /// Whether a sign-in checked earlier still holds: the session hasn't ended, expired or gone
    /// idle, or the Access token hasn't expired. Unlike [`Auth::identify`], it doesn't count as
    /// use, so a long request (the live log) can't keep an idle session alive.
    pub async fn still_valid(&self, db: &Db, identity: &Identity, headers: &HeaderMap) -> bool {
        match identity {
            Identity::Access { .. } => match &self.access {
                Some(access) => access.verify(headers).await.is_some(),
                None => false,
            },
            Identity::Session { hash, .. } => {
                let (hash, now) = (hash.clone(), now());
                db.call(move |c| {
                    c.query_row(
                        "SELECT 1 FROM sessions WHERE token_hash = ?1 AND expires_at > ?2 AND last_seen_at > ?2 - ?3",
                        (hash, now, IDLE_SECONDS),
                        |_| Ok(()),
                    )
                    .optional()
                })
                .await
                .is_ok_and(|row| row.is_some())
            }
        }
    }

    /// Accepts a change only from the dashboard's own pages: `Origin` is the dashboard's, or a
    /// browser that hides it (`null`) says `Sec-Fetch-Site: same-origin`. Pages can set neither.
    pub fn same_origin(&self, headers: &HeaderMap) -> bool {
        let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
        let Some(origin) = header(ORIGIN.as_str()).filter(|o| *o != "null") else {
            return header("sec-fetch-site") == Some("same-origin");
        };
        match &self.origin {
            Some(expected) => origin == expected,
            None => {
                let host = headers.get(HOST).and_then(|v| v.to_str().ok());
                let origin_host = origin.split_once("://").map(|(_, rest)| rest);
                host.is_some() && origin_host == host
            }
        }
    }

    /// How long this IP must still wait after too many failed sign-ins.
    pub fn locked(&self, ip: IpAddr) -> Option<Duration> {
        let strikes = lock(&self.strikes);
        let until = strikes.get(&network(ip))?.locked_until?;
        until.checked_duration_since(Instant::now())
    }

    /// Five failures within a minute lock the IP out: 1 minute, then 2, 4... up to an hour.
    pub fn failed(&self, ip: IpAddr) {
        let mut strikes = lock(&self.strikes);
        let now = Instant::now();
        let entry =
            strikes.entry(network(ip)).or_insert(Strikes { window: now, count: 0, lockouts: 0, locked_until: None });
        if now.duration_since(entry.window) > Duration::from_secs(60) {
            entry.window = now;
            entry.count = 0;
        }
        entry.count += 1;
        if entry.count >= STRIKES {
            entry.lockouts += 1;
            let minutes = 1u64 << (entry.lockouts - 1).min(6);
            entry.locked_until = Some(now + Duration::from_secs((minutes * 60).min(3600)));
            entry.count = 0;
            tracing::warn!(%ip, minutes, "too many failed dashboard sign-ins; locked out");
        }
        if strikes.len() > 10_000 {
            strikes.retain(|_, s| s.locked_until.is_some_and(|t| t > now));
        }
    }

    pub fn succeeded(&self, ip: IpAddr) {
        lock(&self.strikes).remove(&network(ip));
    }

    /// A wrong authenticator code after the right password. The tenth within an hour pauses
    /// password sign-in for an hour from then.
    pub fn wrong_code(&self) {
        if lock(&self.code_strikes).strike(CODE_STRIKES) {
            tracing::warn!(
                "too many wrong authenticator codes after the right password; password sign-in paused for an hour"
            );
        }
    }

    /// A wrong password, from any address. The hundredth within an hour pauses password sign-in
    /// for an hour from then.
    pub fn wrong_password(&self) {
        if lock(&self.password_strikes).strike(PASSWORD_STRIKES) {
            tracing::warn!(
                "too many wrong dashboard passwords from all addresses together; password sign-in paused for an hour. Passkeys and setup links still work"
            );
        }
    }

    /// The password was right and a code comes next: a ticket that says so, for this browser and
    /// five minutes. Only its hash is kept.
    pub fn start_challenge(&self) -> String {
        let token = random_token();
        let now = Instant::now();
        let mut challenges = lock(&self.challenges);
        challenges.retain(|_, c| c.expires > now);
        challenges.insert(hash(&token), Challenge { expires: now + Duration::from_secs(CHALLENGE_SECONDS), tries: 0 });
        token
    }

    /// This browser's ticket for the second step, while it's good.
    pub fn challenge(&self, headers: &HeaderMap) -> Option<Vec<u8>> {
        let hash = hash(&cookie(headers, CHALLENGE_COOKIE)?);
        lock(&self.challenges).get(&hash).filter(|c| c.expires > Instant::now()).map(|_| hash)
    }

    /// A wrong code on a ticket: whether it has tries left. The last one ends it.
    pub fn challenge_missed(&self, hash: &[u8]) -> bool {
        let mut challenges = lock(&self.challenges);
        let Some(challenge) = challenges.get_mut(hash) else { return false };
        challenge.tries += 1;
        if challenge.tries < CHALLENGE_TRIES {
            return true;
        }
        challenges.remove(hash);
        false
    }

    /// A turn to check an authenticator code, one at a time: the limits are looked at once it's
    /// this request's turn, so many guesses sent at once can't all get in before the first misses
    /// count.
    pub async fn code_turn() -> Option<tokio::sync::SemaphorePermit<'static>> {
        static TURN: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
        tokio::time::timeout(Duration::from_secs(5), TURN.acquire()).await.ok()?.ok()
    }

    pub fn end_challenge(&self, hash: &[u8]) {
        lock(&self.challenges).remove(hash);
    }

    /// Whether password sign-in is paused after too many wrong codes or passwords.
    pub fn password_paused(&self) -> bool {
        lock(&self.code_strikes).on() || lock(&self.password_strikes).on()
    }

    /// Ends every sign-in waiting for its code: after the password or the app changed, or every
    /// browser was signed out.
    pub fn clear_challenges(&self) {
        lock(&self.challenges).clear();
    }
}

fn now() -> i64 {
    jiff::Timestamp::now().as_second()
}

pub fn hash(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

pub fn random_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("the system has a random source");
    data_encoding::BASE64URL_NOPAD.encode(&bytes)
}

/// A cookie's value, when it's set and not empty.
pub fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|pair| pair.trim().strip_prefix(name)?.strip_prefix('=').map(str::to_string))
        .filter(|t| !t.is_empty())
}

pub fn session_cookie(token: &str) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{COOKIE_NAME}={token}; Path=/; HttpOnly; Secure; SameSite=Strict; Max-Age={SESSION_SECONDS}"
    ))
    .expect("tokens are base64url")
}

pub fn clear_cookie() -> HeaderValue {
    HeaderValue::from_static("__Host-session=; Path=/; HttpOnly; Secure; SameSite=Strict; Max-Age=0")
}

pub fn challenge_cookie(token: &str) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{CHALLENGE_COOKIE}={token}; Path=/; HttpOnly; Secure; SameSite=Strict; Max-Age={CHALLENGE_SECONDS}"
    ))
    .expect("tokens are base64url")
}

pub fn clear_challenge_cookie() -> HeaderValue {
    HeaderValue::from_static("__Host-signin=; Path=/; HttpOnly; Secure; SameSite=Strict; Max-Age=0")
}

/// Starts a session and returns its token, for the cookie.
pub async fn create_session(db: &Db, method: &'static str, ip: IpAddr, user_agent: &str) -> Result<String> {
    let token = random_token();
    let (hash, now, ua) = (hash(&token), now(), user_agent.chars().take(200).collect::<String>());
    let lasts = if method == SETUP_METHOD { SETUP_SESSION_SECONDS } else { SESSION_SECONDS };
    db.call(move |c| {
        c.execute("DELETE FROM sessions WHERE expires_at <= ?1 OR last_seen_at <= ?1 - ?2", (now, IDLE_SECONDS))?;
        c.execute(
            "INSERT INTO sessions (token_hash, method, created_at, last_seen_at, expires_at, ip, user_agent)
             VALUES (?1, ?2, ?3, ?3, ?4, ?5, ?6)",
            (hash, method, now, now + lasts, ip.to_string(), ua),
        )
    })
    .await?;
    Ok(token)
}

pub async fn end_session(db: &Db, hash: Vec<u8>) -> Result<()> {
    db.call(move |c| c.execute("DELETE FROM sessions WHERE token_hash = ?1", [hash])).await?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct SessionRow {
    pub hash: Vec<u8>,
    pub method: String,
    pub created_at: i64,
    pub last_seen_at: i64,
    pub ip: String,
    pub user_agent: String,
}

pub async fn sessions(db: &Db) -> Result<Vec<SessionRow>> {
    let now = now();
    db.call(move |c| {
        c.prepare(
            "SELECT token_hash, method, created_at, last_seen_at, ip, user_agent FROM sessions
             WHERE expires_at > ?1 AND last_seen_at > ?1 - ?2 ORDER BY last_seen_at DESC",
        )?
        .query_map((now, IDLE_SECONDS), |r| {
            Ok(SessionRow {
                hash: r.get(0)?,
                method: r.get(1)?,
                created_at: r.get(2)?,
                last_seen_at: r.get(3)?,
                ip: r.get(4)?,
                user_agent: r.get(5)?,
            })
        })?
        .collect()
    })
    .await
}

/// Ends every session but `keep`: after a new password, a browser signed in with the old one
/// shouldn't stay in.
pub async fn end_other_sessions(db: &Db, keep: Option<Vec<u8>>) -> Result<()> {
    let keep = keep.unwrap_or_default();
    db.call(move |c| c.execute("DELETE FROM sessions WHERE token_hash != ?1", [keep])).await?;
    Ok(())
}

/// Whether any way to sign in (besides Access) is set up: the password or a passkey.
pub async fn has_login(db: &Db) -> Result<bool> {
    Ok(passkey::count(db).await? > 0 || password::is_set(db).await?)
}

/// A one-time link that signs in and leads to the Security page; valid 30 minutes. It replaces
/// any earlier one, so only the link last printed works.
pub async fn create_setup_link(db: &Db, config: &Config) -> Result<String> {
    let token = random_token();
    let (hash, expires) = (hash(&token), now() + SETUP_SECONDS);
    db.call(move |c| {
        c.execute("DELETE FROM setup_links", [])?;
        c.execute("INSERT INTO setup_links (token_hash, expires_at) VALUES (?1, ?2)", (hash, expires))
    })
    .await?;
    let path = format!("/setup/{token}");
    Ok(match &config.dashboard_url {
        Some(url) => format!("{}{path}", url.origin().ascii_serialization()),
        None => path,
    })
}

/// Uses up a setup link. `true` if it was valid.
/// Whether a setup link still works, without using it up.
pub async fn setup_link_works(db: &Db, token: &str) -> Result<bool> {
    let (hash, now) = (hash(token), now());
    let found = db
        .call(move |c| {
            c.query_row("SELECT 1 FROM setup_links WHERE token_hash = ?1 AND expires_at > ?2", (hash, now), |_| Ok(()))
                .optional()
        })
        .await?;
    Ok(found.is_some())
}

pub async fn consume_setup_link(db: &Db, token: &str) -> Result<bool> {
    let (hash, now) = (hash(token), now());
    let removed = db
        .call(move |c| c.execute("DELETE FROM setup_links WHERE token_hash = ?1 AND expires_at > ?2", (hash, now)))
        .await?;
    Ok(removed == 1)
}

/// Removes the password, the authenticator app, every passkey and every session (`server reset-login`).
pub async fn reset_login(db: &Db) -> Result<()> {
    db.call(|c| {
        let tx = c.transaction()?;
        tx.execute("DELETE FROM kv WHERE key IN ('password_hash', 'totp')", [])?;
        tx.execute("DELETE FROM passkeys", [])?;
        tx.execute("DELETE FROM sessions", [])?;
        tx.commit()
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(vars: &[(&str, &str)]) -> Config {
        let vars: HashMap<String, String> = vars.iter().map(|(k, v)| ((*k).to_string(), (*v).to_string())).collect();
        Config::from_lookup(|k| vars.get(k).cloned()).unwrap()
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[tokio::test]
    async fn sessions_round_trip_and_expire() {
        let db = Db::memory().await.unwrap();
        let auth = Auth::new(&config(&[])).unwrap();
        let token = create_session(&db, "password", "127.0.0.1".parse().unwrap(), "test").await.unwrap();
        let cookie = format!("other=1; {COOKIE_NAME}={token}");
        let identity = auth.identify(&db, &headers(&[("cookie", &cookie)]), true).await.unwrap();
        let Identity::Session { hash, method, .. } = identity else { panic!() };
        assert_eq!(method, "password");
        assert!(auth.identify(&db, &headers(&[("cookie", "__Host-session=wrong")]), true).await.is_none());
        assert!(auth.identify(&db, &HeaderMap::new(), true).await.is_none());

        assert_eq!(sessions(&db).await.unwrap().len(), 1);
        let signed_in = Identity::Session { hash: hash.clone(), method: method.clone(), since: 0 };
        assert!(auth.still_valid(&db, &signed_in, &HeaderMap::new()).await);
        end_session(&db, hash).await.unwrap();
        assert!(auth.identify(&db, &headers(&[("cookie", &cookie)]), true).await.is_none());
        assert!(!auth.still_valid(&db, &signed_in, &HeaderMap::new()).await, "signed out: the live log ends");

        let token = create_session(&db, "passkey", "127.0.0.1".parse().unwrap(), "test").await.unwrap();
        let cookie = format!("{COOKIE_NAME}={token}");
        let expires: i64 =
            db.call(|c| c.query_row("SELECT expires_at - created_at FROM sessions", [], |r| r.get(0))).await.unwrap();
        assert_eq!(expires, SESSION_SECONDS, "a day, however much it's used");
        db.call(|c| c.execute("UPDATE sessions SET last_seen_at = last_seen_at - 3599", [])).await.unwrap();
        assert!(auth.identify(&db, &headers(&[("cookie", &cookie)]), false).await.is_some(), "a page's poll");
        assert!(now() - sessions(&db).await.unwrap()[0].last_seen_at >= 3599, "isn't use");
        assert!(auth.identify(&db, &headers(&[("cookie", &cookie)]), true).await.is_some(), "used within the hour");
        db.call(|c| c.execute("UPDATE sessions SET last_seen_at = last_seen_at - 3600", [])).await.unwrap();
        assert!(auth.identify(&db, &headers(&[("cookie", &cookie)]), true).await.is_none(), "an hour unused");
        assert!(sessions(&db).await.unwrap().is_empty());

        let token = create_session(&db, "passkey", "127.0.0.1".parse().unwrap(), "test").await.unwrap();
        db.call(|c| c.execute("UPDATE sessions SET expires_at = 1", [])).await.unwrap();
        assert!(auth.identify(&db, &headers(&[("cookie", &format!("{COOKIE_NAME}={token}"))]), true).await.is_none());
    }

    #[tokio::test]
    async fn setup_links_work_once_for_an_hour_long_session() {
        let db = Db::memory().await.unwrap();
        let link = create_setup_link(&db, &config(&[("DASHBOARD_URL", "https://homura.example.com")])).await.unwrap();
        let token = link.strip_prefix("https://homura.example.com/setup/").unwrap();
        assert!(consume_setup_link(&db, token).await.unwrap());
        assert!(!consume_setup_link(&db, token).await.unwrap());
        let earlier = create_setup_link(&db, &config(&[])).await.unwrap();
        create_setup_link(&db, &config(&[])).await.unwrap();
        assert!(!consume_setup_link(&db, earlier.strip_prefix("/setup/").unwrap()).await.unwrap(), "replaced");

        let path = create_setup_link(&db, &config(&[])).await.unwrap();
        let token = path.strip_prefix("/setup/").unwrap();
        db.call(|c| c.execute("UPDATE setup_links SET expires_at = 1", [])).await.unwrap();
        assert!(!consume_setup_link(&db, token).await.unwrap(), "expired");

        create_session(&db, SETUP_METHOD, "127.0.0.1".parse().unwrap(), "t").await.unwrap();
        let session = &sessions(&db).await.unwrap()[0];
        let expires: i64 = db.call(|c| c.query_row("SELECT expires_at FROM sessions", [], |r| r.get(0))).await.unwrap();
        assert_eq!(expires - session.created_at, SETUP_SESSION_SECONDS);
        let setup = Identity::Session { hash: session.hash.clone(), method: session.method.clone(), since: 0 };
        assert!(setup.is_setup() && setup.is_recent(), "a setup link's hour is recent enough");
        let old = Identity::Session { hash: vec![], method: "password".into(), since: now() - RECENT_SECONDS };
        assert!(!old.is_recent());
    }

    #[test]
    fn failures_from_anywhere_pause_password_sign_in() {
        let auth = Auth::new(&config(&[])).unwrap();
        for _ in 0..CODE_STRIKES - 1 {
            auth.wrong_code();
        }
        assert!(!auth.password_paused());
        auth.wrong_code();
        assert!(auth.password_paused(), "for an hour from the tenth wrong code");

        let auth = Auth::new(&config(&[])).unwrap();
        for _ in 0..PASSWORD_STRIKES - 1 {
            auth.wrong_password();
        }
        assert!(!auth.password_paused());
        auth.wrong_password();
        assert!(auth.password_paused(), "for an hour from the hundredth wrong password");
    }

    #[test]
    fn origin_checks() {
        let with_url = Auth::new(&config(&[("DASHBOARD_URL", "https://homura.example.com")])).unwrap();
        assert!(with_url.same_origin(&headers(&[("origin", "https://homura.example.com")])));
        assert!(!with_url.same_origin(&headers(&[("origin", "https://evil.example")])));
        assert!(!with_url.same_origin(&headers(&[("origin", "http://homura.example.com")])));
        assert!(!with_url.same_origin(&HeaderMap::new()), "no Origin, no change");
        let hidden = |site| headers(&[("origin", "null"), ("sec-fetch-site", site)]);
        assert!(with_url.same_origin(&hidden("same-origin")), "a browser hiding Origin");
        assert!(!with_url.same_origin(&hidden("cross-site")));
        assert!(!with_url.same_origin(&headers(&[("origin", "null")])));

        let without = Auth::new(&config(&[])).unwrap();
        assert!(without.same_origin(&headers(&[("origin", "http://localhost:8081"), ("host", "localhost:8081")])));
        assert!(!without.same_origin(&headers(&[("origin", "http://evil.example"), ("host", "localhost:8081")])));
    }

    #[test]
    fn lockouts() {
        let auth = Auth::new(&config(&[])).unwrap();
        let ip: IpAddr = "203.0.113.5".parse().unwrap();
        for _ in 0..4 {
            auth.failed(ip);
        }
        assert!(auth.locked(ip).is_none());
        auth.failed(ip);
        let first = auth.locked(ip).unwrap();
        assert!(first <= Duration::from_secs(60) && first > Duration::from_secs(50));
        for _ in 0..5 {
            auth.failed(ip);
        }
        assert!(auth.locked(ip).unwrap() > Duration::from_secs(100), "the second lockout is longer");
        auth.succeeded(ip);
        assert!(auth.locked(ip).is_none());

        // An IPv6 network's addresses count together.
        let (a, b): (IpAddr, IpAddr) = ("2001:db8:1:2::a".parse().unwrap(), "2001:db8:1:2:ffff::b".parse().unwrap());
        for _ in 0..5 {
            auth.failed(a);
        }
        assert!(auth.locked(b).is_some(), "same /64");
        assert!(auth.locked("2001:db8:1:3::a".parse().unwrap()).is_none(), "another /64");
    }

    #[tokio::test]
    async fn reset_removes_everything() {
        let db = Db::memory().await.unwrap();
        assert!(!has_login(&db).await.unwrap());
        password::set(&db, "correct horse battery").await.unwrap();
        let secret = totp::Setup::default().start(b"session:test".to_vec());
        assert!(totp::enable(&db, &secret, &totp::code_now(&secret, 0)).await.unwrap());
        create_session(&db, "password + code", "127.0.0.1".parse().unwrap(), "t").await.unwrap();
        assert!(has_login(&db).await.unwrap());
        reset_login(&db).await.unwrap();
        assert!(!has_login(&db).await.unwrap());
        assert!(sessions(&db).await.unwrap().is_empty());
    }
}
