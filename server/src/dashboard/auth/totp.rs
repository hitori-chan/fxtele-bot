//! Authenticator-app codes (RFC 6238: HMAC-SHA-1, 6 digits, 30 seconds), asked for after the
//! password once turned on. Passkeys and Cloudflare Access don't need them. Each code works once.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use data_encoding::BASE32_NOPAD;
use hmac::{Hmac, KeyInit, Mac};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use sha1::Sha1;

use crate::db::Db;
use crate::sync::lock;

const DB_KEY: &str = "totp";
const STEP: i64 = 30;
const DIGITS: u32 = 6;
/// Codes from one step before or after are accepted too, for clocks that drift.
const SKEW: i64 = 1;
const SETUP_TTL: Duration = Duration::from_secs(10 * 60);

#[derive(Serialize, Deserialize)]
struct Stored {
    secret: Vec<u8>,
    enabled_at: i64,
    /// The newest step whose code was used; older and equal ones are refused.
    last_step: i64,
}

fn now() -> i64 {
    jiff::Timestamp::now().as_second()
}

/// The code for one 30-second step.
fn code_at(secret: &[u8], step: i64) -> u32 {
    let mut mac = Hmac::<Sha1>::new_from_slice(secret).expect("HMAC takes keys of any length");
    mac.update(&step.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let offset = usize::from(digest[digest.len() - 1] & 0x0f);
    let value = u32::from_be_bytes([digest[offset], digest[offset + 1], digest[offset + 2], digest[offset + 3]]);
    (value & 0x7fff_ffff) % 10u32.pow(DIGITS)
}

/// The step `code` belongs to, if it's valid at `at` (spaces are ignored: "123 456").
fn matching_step(secret: &[u8], code: &str, at: i64) -> Option<i64> {
    let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    if code.len() != DIGITS as usize || !code.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let code: u32 = code.parse().ok()?;
    let current = at / STEP;
    (current - SKEW..=current + SKEW).find(|step| code_at(secret, *step) == code)
}

/// When TOTP was turned on, if it is.
pub async fn enabled_at(db: &Db) -> Result<Option<i64>> {
    Ok(db.get_json::<Stored>(DB_KEY).await?.map(|s| s.enabled_at))
}

/// Turns TOTP on with `secret` if `code` matches it. `false` if it doesn't.
pub async fn enable(db: &Db, secret: &[u8], code: &str) -> Result<bool> {
    let Some(step) = matching_step(secret, code, now()) else { return Ok(false) };
    db.set_json(DB_KEY, &Stored { secret: secret.to_vec(), enabled_at: now(), last_step: step }).await?;
    Ok(true)
}

pub async fn disable(db: &Db) -> Result<()> {
    db.delete(DB_KEY).await
}

/// Checks a sign-in code and uses it up. `false` when it's wrong, already used, or TOTP is off.
pub async fn verify(db: &Db, code: &str) -> Result<bool> {
    let (code, at) = (code.to_string(), now());
    // Read, check and save in one call, so two requests can't both use the same code.
    db.call(move |c| {
        let json: Option<String> =
            c.query_row("SELECT value FROM kv WHERE key = ?1", [DB_KEY], |r| r.get(0)).optional()?;
        let Some(mut stored) = json.and_then(|j| serde_json::from_str::<Stored>(&j).ok()) else { return Ok(false) };
        match matching_step(&stored.secret, &code, at) {
            Some(step) if step > stored.last_step => {
                stored.last_step = step;
                let json = serde_json::to_string(&stored).expect("plain data serializes");
                c.execute("UPDATE kv SET value = ?2, updated_at = unixepoch() WHERE key = ?1", (DB_KEY, json))?;
                Ok(true)
            }
            _ => Ok(false),
        }
    })
    .await
}

/// A secret being set up: shown as a QR code until it's confirmed with a code, or expires. Only
/// to whoever started it ([`super::Identity::owner`]): another session, an old one say, never
/// sees the key.
#[derive(Default)]
pub struct Setup {
    pending: Mutex<Option<Pending>>,
}

struct Pending {
    at: Instant,
    owner: Vec<u8>,
    secret: Vec<u8>,
}

impl Setup {
    /// The secret `owner` is setting up, starting a new one if there's none of theirs.
    pub fn start(&self, owner: Vec<u8>) -> Vec<u8> {
        let mut pending = lock(&self.pending);
        if let Some(p) = pending.as_ref()
            && p.at.elapsed() < SETUP_TTL
            && p.owner == owner
        {
            return p.secret.clone();
        }
        let mut secret = vec![0u8; 20];
        getrandom::fill(&mut secret).expect("the system has a random source");
        *pending = Some(Pending { at: Instant::now(), owner, secret: secret.clone() });
        secret
    }

    pub fn current(&self, owner: &[u8]) -> Option<Vec<u8>> {
        let pending = lock(&self.pending);
        pending.as_ref().filter(|p| p.at.elapsed() < SETUP_TTL && p.owner == owner).map(|p| p.secret.clone())
    }

    pub fn cancel(&self) {
        *lock(&self.pending) = None;
    }
}

/// The key to type into an app by hand.
pub fn key(secret: &[u8]) -> String {
    BASE32_NOPAD.encode(secret)
}

/// The key in groups of four, for reading.
pub fn grouped(key: &str) -> String {
    key.as_bytes().chunks(4).map(|c| String::from_utf8_lossy(c).into_owned()).collect::<Vec<_>>().join(" ")
}

/// The `otpauth://` link apps scan or open. SHA-1, 6 digits and 30 seconds are the defaults, so
/// they're left out: a shorter link makes a coarser QR code, which scans more easily.
pub fn link(secret: &[u8], account: &str) -> String {
    const ISSUER: &str = "Homura";
    let account = url::form_urlencoded::byte_serialize(account.as_bytes()).collect::<String>().replace('+', "%20");
    format!("otpauth://totp/{ISSUER}:{account}?secret={}&issuer={ISSUER}", key(secret))
}

/// A QR code of `link` as an SVG `data:` URL, and its width in pixels. Each module is a whole
/// number of pixels, shown at that size: scanners that read a screenshot of the page, like
/// Bitwarden's browser extension, fail on the blurred edges of a scaled one.
pub fn qr(link: &str) -> Result<(String, u32)> {
    const MODULE: u32 = 4;
    let code = qrcode::QrCode::new(link.as_bytes()).context("can't make the QR code")?;
    // The quiet zone, four modules on each side, is part of the image.
    let size = (u32::try_from(code.width()).context("QR code too big")? + 8) * MODULE;
    let svg = code.render::<qrcode::render::svg::Color<'_>>().module_dimensions(MODULE, MODULE).build();
    Ok((format!("data:image/svg+xml;base64,{}", data_encoding::BASE64.encode(svg.as_bytes())), size))
}

#[cfg(test)]
pub fn code_now(secret: &[u8], steps_ahead: i64) -> String {
    format!("{:06}", code_at(secret, now() / STEP + steps_ahead))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RFC_SECRET: &[u8] = b"12345678901234567890";

    #[test]
    fn matches_rfc_6238_vectors() {
        // RFC 6238 appendix B (SHA-1), cut to 6 digits.
        for (time, code) in [
            (59, 287_082),
            (1_111_111_109, 81_804),
            (1_111_111_111, 50_471),
            (1_234_567_890, 5_924),
            (2_000_000_000, 279_037),
            (20_000_000_000, 353_130),
        ] {
            assert_eq!(code_at(RFC_SECRET, time / STEP), code, "at {time}");
        }
    }

    #[test]
    fn accepts_one_step_of_drift_and_spaces() {
        let at = 1_111_111_111;
        assert_eq!(matching_step(RFC_SECRET, "050471", at), Some(at / STEP));
        assert_eq!(matching_step(RFC_SECRET, "050 471", at), Some(at / STEP));
        assert_eq!(matching_step(RFC_SECRET, "050471", at + 30), Some(at / STEP), "one step late");
        assert_eq!(matching_step(RFC_SECRET, "050471", at + 61), None, "too late");
        assert_eq!(matching_step(RFC_SECRET, "50471", at), None);
        assert_eq!(matching_step(RFC_SECRET, "05047a", at), None);
    }

    #[tokio::test]
    async fn codes_work_once() {
        let db = Db::memory().await.unwrap();
        let secret = vec![7u8; 20];
        assert!(!verify(&db, &code_now(&secret, 0)).await.unwrap(), "off");
        assert!(!enable(&db, &secret, "000000x").await.unwrap());
        assert!(enable(&db, &secret, &code_now(&secret, 0)).await.unwrap());
        assert!(enabled_at(&db).await.unwrap().is_some());
        assert!(!verify(&db, &code_now(&secret, 0)).await.unwrap(), "the setup code is used up");
        assert!(verify(&db, &code_now(&secret, 1)).await.unwrap());
        assert!(!verify(&db, &code_now(&secret, 1)).await.unwrap(), "no replay");
        disable(&db).await.unwrap();
        assert!(enabled_at(&db).await.unwrap().is_none());
    }

    #[test]
    fn setup_keeps_one_secret_until_cancelled() {
        let (setup, me, other) = (Setup::default(), b"session:1".to_vec(), b"session:2".to_vec());
        assert!(setup.current(&me).is_none());
        let first = setup.start(me.clone());
        assert_eq!(setup.start(me.clone()), first);
        assert_eq!(setup.current(&me), Some(first.clone()));
        assert_eq!(setup.current(&other), None, "only whoever started it sees the key");
        setup.cancel();
        assert!(setup.current(&me).is_none());
        assert_ne!(setup.start(me), first);
    }

    #[test]
    fn key_link_and_qr() {
        assert_eq!(grouped(&key(RFC_SECRET)), "GEZD GNBV GY3T QOJQ GEZD GNBV GY3T QOJQ");
        let link = link(RFC_SECRET, "homura.example.com");
        assert_eq!(
            link,
            "otpauth://totp/Homura:homura.example.com?secret=GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ&issuer=Homura"
        );
        let (url, size) = qr(&link).unwrap();
        assert!(url.starts_with("data:image/svg+xml;base64,"));
        // Whole 4-pixel modules, quiet zone included: version 6 is 41 modules, plus 8.
        assert_eq!(size, (41 + 8) * 4);
    }
}
