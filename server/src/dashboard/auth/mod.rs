//! Dashboard login, always with two factors: a password with an authenticator-app code, or a
//! passkey (which verifies the person with a PIN or biometric), or Cloudflare Access (whose policy
//! decides). Setup links, made on the server's command line, only lead to setting one up. Also
//! sessions, `Origin` checks and the lockout for failed attempts.

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

use crate::config::Config;
use crate::db::Db;
use crate::sync::lock;

pub const COOKIE_NAME: &str = "__Host-session";
/// Sessions end a day after signing in, or after an hour unused, whichever comes first (NIST
/// SP 800-63B's guidance for two-factor sign-in).
const SESSION_SECONDS: i64 = 86_400;
const IDLE_SECONDS: i64 = 3600;
/// How a setup link signs in: only the login settings, for an hour.
pub const SETUP_METHOD: &str = "setup link";
const SETUP_SESSION_SECONDS: i64 = 3600;
const SETUP_SECONDS: i64 = 30 * 60;
const STRIKES: u32 = 5;
/// Wrong authenticator codes after the right password, from anywhere, before password sign-in
/// pauses for [`PAUSE`]: per-address lockouts alone let someone who has the password guess codes
/// from many addresses.
const CODE_STRIKES: u32 = 10;
const CODE_WINDOW: Duration = Duration::from_secs(3600);

/// Who is signed in, for this request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Identity {
    Session { hash: Vec<u8>, method: String },
    Access { email: String },
}

impl Identity {
    /// Signed in with a setup link: it may only set up a way to sign in.
    pub fn is_setup(&self) -> bool {
        matches!(self, Self::Session { method, .. } if method == SETUP_METHOD)
    }
}

#[derive(Debug)]
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
    /// Wrong codes after the right password: when the count started, and the count.
    code_strikes: Mutex<(Instant, u32)>,
}

impl Auth {
    pub fn new(config: &Config) -> Result<Self> {
        Ok(Self {
            passkeys: config.dashboard_url.as_ref().map(passkey::Passkeys::new).transpose()?,
            access: config.access.as_ref().map(access::AccessVerifier::new).transpose()?,
            totp_setup: totp::Setup::default(),
            origin: config.dashboard_url.as_ref().map(|url| url.origin().ascii_serialization()),
            strikes: Mutex::new(HashMap::new()),
            code_strikes: Mutex::new((Instant::now(), 0)),
        })
    }

    /// The signed-in identity: a valid Access token, or a live session cookie.
    pub async fn identify(&self, db: &Db, headers: &HeaderMap) -> Option<Identity> {
        if let Some(access) = &self.access
            && let Some(email) = access.verify(headers).await
        {
            return Some(Identity::Access { email });
        }
        let token = session_token(headers)?;
        let hash = hash(&token);
        let now = now();
        let row: Option<(String, i64)> = db
            .call({
                let hash = hash.clone();
                move |c| {
                    c.query_row(
                        "SELECT method, last_seen_at FROM sessions
                         WHERE token_hash = ?1 AND expires_at > ?2 AND last_seen_at > ?2 - ?3",
                        (hash, now, IDLE_SECONDS),
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()
                }
            })
            .await
            .ok()
            .flatten();
        let (method, last_seen) = row?;
        if now - last_seen >= 60 {
            // Use keeps it going, noted at most once a minute.
            let hash = hash.clone();
            let _ = db
                .call(move |c| c.execute("UPDATE sessions SET last_seen_at = ?2 WHERE token_hash = ?1", (hash, now)))
                .await;
        }
        Some(Identity::Session { hash, method })
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

    /// A wrong authenticator code after the right password.
    pub fn wrong_code(&self) {
        let mut strikes = lock(&self.code_strikes);
        if strikes.0.elapsed() > CODE_WINDOW {
            *strikes = (Instant::now(), 0);
        }
        strikes.1 += 1;
        if strikes.1 == CODE_STRIKES {
            tracing::warn!(
                "too many wrong authenticator codes after the right password; password sign-in paused for an hour"
            );
        }
    }

    /// Whether password sign-in is paused after too many wrong codes.
    pub fn codes_paused(&self) -> bool {
        let strikes = lock(&self.code_strikes);
        strikes.1 >= CODE_STRIKES && strikes.0.elapsed() <= CODE_WINDOW
    }
}

/// The address lockouts count by: an IPv6 address's /64, which one person usually has whole.
fn network(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V6(v6) => IpAddr::V6((u128::from(v6) & !u128::from(u64::MAX)).into()),
        v4 => v4,
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

fn session_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|pair| pair.trim().strip_prefix(COOKIE_NAME)?.strip_prefix('=').map(str::to_string))
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

pub async fn end_all_sessions(db: &Db) -> Result<()> {
    db.call(|c| c.execute("DELETE FROM sessions", [])).await?;
    Ok(())
}

/// Ends every session but `keep`: after a new password, a browser signed in with the old one
/// shouldn't stay in.
pub async fn end_other_sessions(db: &Db, keep: Option<Vec<u8>>) -> Result<()> {
    let keep = keep.unwrap_or_default();
    db.call(move |c| c.execute("DELETE FROM sessions WHERE token_hash != ?1", [keep])).await?;
    Ok(())
}

/// Whether the password can sign in: it's set, and so is the authenticator app.
pub async fn password_usable(db: &Db) -> Result<bool> {
    Ok(password::is_set(db).await? && totp::enabled_at(db).await?.is_some())
}

/// Whether any way to sign in (besides Access) is set up: a passkey, or the password with the
/// authenticator app.
pub async fn has_login(db: &Db) -> Result<bool> {
    Ok(passkey::count(db).await? > 0 || password_usable(db).await?)
}

/// A one-time link that signs in and leads to the login settings; valid 30 minutes.
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
        let identity = auth.identify(&db, &headers(&[("cookie", &cookie)])).await.unwrap();
        let Identity::Session { hash, method } = identity else { panic!() };
        assert_eq!(method, "password");
        assert!(auth.identify(&db, &headers(&[("cookie", "__Host-session=wrong")])).await.is_none());
        assert!(auth.identify(&db, &HeaderMap::new()).await.is_none());

        assert_eq!(sessions(&db).await.unwrap().len(), 1);
        end_session(&db, hash).await.unwrap();
        assert!(auth.identify(&db, &headers(&[("cookie", &cookie)])).await.is_none());

        let token = create_session(&db, "passkey", "127.0.0.1".parse().unwrap(), "test").await.unwrap();
        let cookie = format!("{COOKIE_NAME}={token}");
        let expires: i64 =
            db.call(|c| c.query_row("SELECT expires_at - created_at FROM sessions", [], |r| r.get(0))).await.unwrap();
        assert_eq!(expires, SESSION_SECONDS, "a day, however much it's used");
        db.call(|c| c.execute("UPDATE sessions SET last_seen_at = last_seen_at - 3599", [])).await.unwrap();
        assert!(auth.identify(&db, &headers(&[("cookie", &cookie)])).await.is_some(), "used within the hour");
        db.call(|c| c.execute("UPDATE sessions SET last_seen_at = last_seen_at - 3600", [])).await.unwrap();
        assert!(auth.identify(&db, &headers(&[("cookie", &cookie)])).await.is_none(), "an hour unused");
        assert!(sessions(&db).await.unwrap().is_empty());

        let token = create_session(&db, "passkey", "127.0.0.1".parse().unwrap(), "test").await.unwrap();
        db.call(|c| c.execute("UPDATE sessions SET expires_at = 1", [])).await.unwrap();
        assert!(auth.identify(&db, &headers(&[("cookie", &format!("{COOKIE_NAME}={token}"))])).await.is_none());
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
        assert!(Identity::Session { hash: session.hash.clone(), method: session.method.clone() }.is_setup());
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
        password::set(&db, "correct horse battery").await.unwrap();
        assert!(!has_login(&db).await.unwrap(), "a password alone isn't a way to sign in");
        let secret = totp::Setup::default().start();
        assert!(totp::enable(&db, &secret, &totp::code_now(&secret, 0)).await.unwrap());
        create_session(&db, "password + code", "127.0.0.1".parse().unwrap(), "t").await.unwrap();
        assert!(has_login(&db).await.unwrap());
        reset_login(&db).await.unwrap();
        assert!(!has_login(&db).await.unwrap());
        assert!(sessions(&db).await.unwrap().is_empty());
    }
}
