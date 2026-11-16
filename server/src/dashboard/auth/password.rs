//! The admin password: refused when easy to guess, hashed with argon2id on the blocking pool,
//! two at a time.

use std::time::Duration;

use anyhow::{Result, bail};
use argon2::Argon2;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use tokio::sync::{Semaphore, SemaphorePermit};

use crate::db::Db;

const DB_KEY: &str = "password_hash";
pub const MIN_LENGTH: usize = 12;
pub const MAX_LENGTH: usize = 128;
/// Words a guesser would try first on this dashboard.
const OBVIOUS: &[&str] = &["homura", "akemi", "dashboard", "admin", "telegram", "facebook", "preview", "password"];

/// Refuses a password that's too short or long, or that zxcvbn (common passwords, words, names,
/// keyboard patterns, repeats, dates) rates easy to guess. The authenticator app and the lockout
/// do the rest, so no rules about symbols or digits.
pub fn check(password: &str) -> Result<()> {
    let length = password.chars().count();
    if !(MIN_LENGTH..=MAX_LENGTH).contains(&length) {
        bail!("use {MIN_LENGTH} to {MAX_LENGTH} characters; three or four unrelated words make a good password");
    }
    let estimate = zxcvbn::zxcvbn(password, OBVIOUS);
    if estimate.score() < zxcvbn::Score::Three {
        let why = estimate.feedback().and_then(|f| f.warning()).map(|w| format!(": {}", w.to_string().to_lowercase()));
        let why = why.as_deref().unwrap_or(".").trim_end_matches('.');
        bail!("that password is easy to guess{why}. Add another unrelated word, or let a password manager make one");
    }
    Ok(())
}

/// A turn to hash or check a password, waited for a few seconds at most. Each takes argon2's
/// 19 MiB, so a flood of sign-ins at once would otherwise run the server out of memory. The
/// work holds the turn until it's done, even when the request that asked is gone.
pub struct Turn(#[expect(dead_code, reason = "held, not read")] SemaphorePermit<'static>);

pub async fn turn() -> Option<Turn> {
    static TURNS: Semaphore = Semaphore::const_new(2);
    tokio::time::timeout(Duration::from_secs(5), TURNS.acquire()).await.ok()?.ok().map(Turn)
}

pub async fn is_set(db: &Db) -> Result<bool> {
    Ok(db.get_json::<String>(DB_KEY).await?.is_some())
}

pub async fn set(db: &Db, password: &str) -> Result<()> {
    let password = password.to_string();
    let turn = turn().await.ok_or_else(|| anyhow::anyhow!("the server is busy; try again in a moment"))?;
    // zxcvbn takes tens of milliseconds on a long password, so it runs off the async threads too.
    let hash = tokio::task::spawn_blocking(move || {
        let _turn = turn;
        check(&password)?;
        Argon2::default()
            .hash_password(password.as_bytes())
            .map(|h| h.to_string())
            .map_err(|e| anyhow::anyhow!("can't hash the password: {e}"))
    })
    .await??;
    db.set_json(DB_KEY, &hash).await
}

pub async fn remove(db: &Db) -> Result<()> {
    db.delete(DB_KEY).await
}

/// Whether `password` matches, in its [`turn`]. `false` when no password is set.
pub async fn verify(db: &Db, password: &str, turn: Turn) -> Result<bool> {
    let Some(hash) = db.get_json::<String>(DB_KEY).await? else { return Ok(false) };
    let password = password.to_string();
    Ok(tokio::task::spawn_blocking(move || {
        let _turn = turn;
        Argon2::default().verify_password(password.as_bytes(), hash.as_str()).is_ok()
    })
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn set_verify_remove() {
        let db = Db::memory().await.unwrap();
        let check = |password: &'static str| {
            let db = db.clone();
            async move { verify(&db, password, turn().await.unwrap()).await.unwrap() }
        };
        assert!(!check("anything at all").await);
        assert!(set(&db, "short").await.is_err());
        assert!(set(&db, "aaaaaaaaaaaaaaaaaaaa").await.is_err(), "long but easy");
        set(&db, "correct horse battery").await.unwrap();
        assert!(check("correct horse battery").await);
        assert!(!check("correct horse battery!").await);
        let stored: String = db.get_json(DB_KEY).await.unwrap().unwrap();
        assert!(stored.starts_with("$argon2id$"));
        remove(&db).await.unwrap();
        assert!(!is_set(&db).await.unwrap());
    }

    #[test]
    fn strength() {
        for weak in
            ["password12345678", "qwertyuiopasdfgh", "12345678901234567890", "P@ssw0rd!P@ssw0rd!", "Homura2024!"]
        {
            assert!(check(weak).is_err(), "{weak}");
        }
        for strong in ["correct horse battery", "plaid otter mosaic", "x7Kp2mQw9Lz4"] {
            check(strong).unwrap();
        }
        assert!(check(&"word ".repeat(30)).is_err(), "too long");
        let message = check("password12345678").unwrap_err().to_string();
        assert!(message.starts_with("that password is easy to guess: ") && message.contains(". Add"), "{message}");
    }
}
