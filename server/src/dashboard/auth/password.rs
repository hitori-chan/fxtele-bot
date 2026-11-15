//! The admin password, hashed with argon2id. Hashing runs on the blocking pool.

use anyhow::{Result, bail};
use argon2::Argon2;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};

use crate::db::Db;

const DB_KEY: &str = "password_hash";
pub const MIN_LENGTH: usize = 10;

pub async fn is_set(db: &Db) -> Result<bool> {
    Ok(db.get_json::<String>(DB_KEY).await?.is_some())
}

pub async fn set(db: &Db, password: &str) -> Result<()> {
    if password.chars().count() < MIN_LENGTH {
        bail!("use at least {MIN_LENGTH} characters");
    }
    let password = password.to_string();
    let hash = tokio::task::spawn_blocking(move || {
        Argon2::default().hash_password(password.as_bytes()).map(|h| h.to_string())
    })
    .await?
    .map_err(|e| anyhow::anyhow!("can't hash the password: {e}"))?;
    db.set_json(DB_KEY, &hash).await
}

pub async fn remove(db: &Db) -> Result<()> {
    db.delete(DB_KEY).await
}

/// Whether `password` matches. `false` when no password is set.
pub async fn verify(db: &Db, password: &str) -> Result<bool> {
    let Some(hash) = db.get_json::<String>(DB_KEY).await? else { return Ok(false) };
    let password = password.to_string();
    Ok(tokio::task::spawn_blocking(move || {
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
        assert!(!verify(&db, "anything at all").await.unwrap());
        assert!(set(&db, "short").await.is_err());
        set(&db, "correct horse battery").await.unwrap();
        assert!(verify(&db, "correct horse battery").await.unwrap());
        assert!(!verify(&db, "correct horse battery!").await.unwrap());
        let stored: String = db.get_json(DB_KEY).await.unwrap().unwrap();
        assert!(stored.starts_with("$argon2id$"));
        remove(&db).await.unwrap();
        assert!(!is_set(&db).await.unwrap());
    }
}
