//! Dashboard login: password (with an optional authenticator-app code), passkeys and
//! Cloudflare Access; sessions, setup links, `Origin` checks and the lockout for failed attempts.

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
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rusqlite::OptionalExtension;
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::db::Db;

pub const COOKIE_NAME: &str = "__Host-session";
const SESSION_SECONDS: i64 = 30 * 86_400;
const SETUP_SECONDS: i64 = 30 * 60;
const STRIKES: u32 = 5;

/// Who is signed in, for this request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Identity {
    Session { hash: Vec<u8>, method: String },
    Access { email: String },
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
}

impl Auth {
    pub fn new(config: &Config) -> Result<Self> {
        Ok(Self {
            passkeys: config.dashboard_url.as_ref().map(passkey::Passkeys::new).transpose()?,
            access: config.access.as_ref().map(access::AccessVerifier::new).transpose()?,
            totp_setup: totp::Setup::default(),
            origin: config.dashboard_url.as_ref().map(|url| url.origin().ascii_serialization()),
            strikes: Mutex::new(HashMap::new()),
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
                        "SELECT method, last_seen_at FROM sessions WHERE token_hash = ?1 AND expires_at > ?2",
                        (hash, now),
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()
                }
            })
            .await
            .ok()
            .flatten();
        let (method, last_seen) = row?;
        if now - last_seen > 3600 {
            // Extend on use, at most once an hour.
            let hash = hash.clone();
            let _ = db
                .call(move |c| {
                    c.execute(
                        "UPDATE sessions SET last_seen_at = ?2, expires_at = ?3 WHERE token_hash = ?1",
                        (hash, now, now + SESSION_SECONDS),
                    )
                })
                .await;
        }
        Some(Identity::Session { hash, method })
    }

    /// Accepts a change only when `Origin` is the dashboard's own.
    pub fn same_origin(&self, headers: &HeaderMap) -> bool {
        let Some(origin) = headers.get(ORIGIN).and_then(|v| v.to_str().ok()) else { return false };
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
        let strikes = self.strikes.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let until = strikes.get(&ip)?.locked_until?;
        until.checked_duration_since(Instant::now())
    }

    /// Five failures within a minute lock the IP out: 1 minute, then 2, 4... up to an hour.
    pub fn failed(&self, ip: IpAddr) {
        let mut strikes = self.strikes.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = Instant::now();
        let entry = strikes.entry(ip).or_insert(Strikes { window: now, count: 0, lockouts: 0, locked_until: None });
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
        self.strikes.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&ip);
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
    URL_SAFE_NO_PAD.encode(bytes)
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
    db.call(move |c| {
        c.execute("DELETE FROM sessions WHERE expires_at <= ?1", [now])?;
        c.execute(
            "INSERT INTO sessions (token_hash, method, created_at, last_seen_at, expires_at, ip, user_agent)
             VALUES (?1, ?2, ?3, ?3, ?4, ?5, ?6)",
            (hash, method, now, now + SESSION_SECONDS, ip.to_string(), ua),
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
             WHERE expires_at > ?1 ORDER BY last_seen_at DESC",
        )?
        .query_map([now], |r| {
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

/// Whether any way to sign in (besides Access) is set up.
pub async fn has_login(db: &Db) -> Result<bool> {
    Ok(password::is_set(db).await? || passkey::count(db).await? > 0)
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

        let token = create_session(&db, "password", "127.0.0.1".parse().unwrap(), "test").await.unwrap();
        db.call(|c| c.execute("UPDATE sessions SET expires_at = 1", [])).await.unwrap();
        assert!(auth.identify(&db, &headers(&[("cookie", &format!("{COOKIE_NAME}={token}"))])).await.is_none());
    }

    #[tokio::test]
    async fn setup_links_work_once() {
        let db = Db::memory().await.unwrap();
        let link = create_setup_link(&db, &config(&[("DASHBOARD_URL", "https://homura.example.com")])).await.unwrap();
        let token = link.strip_prefix("https://homura.example.com/setup/").unwrap();
        assert!(consume_setup_link(&db, token).await.unwrap());
        assert!(!consume_setup_link(&db, token).await.unwrap());

        let path = create_setup_link(&db, &config(&[])).await.unwrap();
        let token = path.strip_prefix("/setup/").unwrap();
        db.call(|c| c.execute("UPDATE setup_links SET expires_at = 1", [])).await.unwrap();
        assert!(!consume_setup_link(&db, token).await.unwrap(), "expired");
    }

    #[test]
    fn origin_checks() {
        let with_url = Auth::new(&config(&[("DASHBOARD_URL", "https://homura.example.com")])).unwrap();
        assert!(with_url.same_origin(&headers(&[("origin", "https://homura.example.com")])));
        assert!(!with_url.same_origin(&headers(&[("origin", "https://evil.example")])));
        assert!(!with_url.same_origin(&headers(&[("origin", "http://homura.example.com")])));
        assert!(!with_url.same_origin(&HeaderMap::new()), "no Origin, no change");

        let without = Auth::new(&config(&[])).unwrap();
        assert!(without.same_origin(&headers(&[("origin", "http://localhost:8081"), ("host", "localhost:8081")])));
        assert!(!without.same_origin(&headers(&[("origin", "http://evil.example"), ("host", "localhost:8081")])));
    }

    #[test]
    fn lockout_grows() {
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
    }

    #[tokio::test]
    async fn reset_removes_everything() {
        let db = Db::memory().await.unwrap();
        password::set(&db, "correct horse").await.unwrap();
        create_session(&db, "password", "127.0.0.1".parse().unwrap(), "t").await.unwrap();
        assert!(has_login(&db).await.unwrap());
        reset_login(&db).await.unwrap();
        assert!(!has_login(&db).await.unwrap());
        assert!(sessions(&db).await.unwrap().is_empty());
    }
}
