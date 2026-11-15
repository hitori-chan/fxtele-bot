//! Passkeys via webauthn-rs. They're tied to `DASHBOARD_URL`'s hostname. Signing in offers
//! every registered passkey, so no username is needed.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use url::Url;
use webauthn_rs::prelude::{
    CreationChallengeResponse, Passkey, PasskeyAuthentication, PasskeyRegistration, PublicKeyCredential,
    RegisterPublicKeyCredential, RequestChallengeResponse, Uuid,
};
use webauthn_rs::{Webauthn, WebauthnBuilder};

use super::random_token;
use crate::db::Db;

const USER_KEY: &str = "webauthn_user_id";
const CEREMONY_TTL: Duration = Duration::from_secs(300);

enum Ceremony {
    Register { state: PasskeyRegistration, name: String },
    Login(PasskeyAuthentication),
}

pub struct Passkeys {
    webauthn: Webauthn,
    ceremonies: Mutex<HashMap<String, (Instant, Ceremony)>>,
}

#[derive(Debug, Clone)]
pub struct StoredPasskey {
    pub id: Vec<u8>,
    pub name: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}

impl Passkeys {
    pub fn new(dashboard_url: &Url) -> Result<Self> {
        let host = dashboard_url.host_str().context("DASHBOARD_URL has no host")?;
        let webauthn = WebauthnBuilder::new(host, dashboard_url)
            .and_then(|b| b.rp_name("Dashboard").build())
            .map_err(|e| anyhow!("can't set up passkeys for DASHBOARD_URL: {e}"))?;
        Ok(Self { webauthn, ceremonies: Mutex::new(HashMap::new()) })
    }

    fn remember(&self, ceremony: Ceremony) -> String {
        let id = random_token();
        let mut ceremonies = self.ceremonies.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        ceremonies.retain(|_, (at, _)| at.elapsed() < CEREMONY_TTL);
        ceremonies.insert(id.clone(), (Instant::now(), ceremony));
        id
    }

    fn take(&self, id: &str) -> Option<Ceremony> {
        let mut ceremonies = self.ceremonies.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        ceremonies.remove(id).filter(|(at, _)| at.elapsed() < CEREMONY_TTL).map(|(_, c)| c)
    }

    pub async fn start_registration(&self, db: &Db, name: &str) -> Result<(String, CreationChallengeResponse)> {
        let user = match db.get_json::<Uuid>(USER_KEY).await? {
            Some(user) => user,
            None => {
                let user = Uuid::new_v4();
                db.set_json(USER_KEY, &user).await?;
                user
            }
        };
        let existing: Vec<Vec<u8>> = all(db).await?.iter().map(|p| p.cred_id().clone()).collect();
        let (challenge, state) = self
            .webauthn
            .start_passkey_registration(user, "admin", "Dashboard admin", Some(existing))
            .map_err(|e| anyhow!("{e}"))?;
        let name = name.trim().chars().take(60).collect::<String>();
        let name = if name.is_empty() { "Passkey".to_string() } else { name };
        Ok((self.remember(Ceremony::Register { state, name }), challenge))
    }

    pub async fn finish_registration(&self, db: &Db, id: &str, credential: &RegisterPublicKeyCredential) -> Result<()> {
        let Some(Ceremony::Register { state, name }) = self.take(id) else {
            return Err(anyhow!("that passkey request expired; try again"));
        };
        let passkey = self.webauthn.finish_passkey_registration(credential, &state).map_err(|e| anyhow!("{e}"))?;
        save(db, &passkey, Some(name)).await
    }

    pub async fn start_login(&self, db: &Db) -> Result<(String, RequestChallengeResponse)> {
        let passkeys = all(db).await?;
        if passkeys.is_empty() {
            return Err(anyhow!("no passkeys are registered"));
        }
        let (challenge, state) = self.webauthn.start_passkey_authentication(&passkeys).map_err(|e| anyhow!("{e}"))?;
        Ok((self.remember(Ceremony::Login(state)), challenge))
    }

    /// Checks a sign-in and updates the passkey's counter.
    pub async fn finish_login(&self, db: &Db, id: &str, credential: &PublicKeyCredential) -> Result<()> {
        let Some(Ceremony::Login(state)) = self.take(id) else {
            return Err(anyhow!("that sign-in request expired; try again"));
        };
        let result = self.webauthn.finish_passkey_authentication(credential, &state).map_err(|e| anyhow!("{e}"))?;
        let mut passkeys = all(db).await?;
        if let Some(passkey) = passkeys.iter_mut().find(|p| p.cred_id() == result.cred_id()) {
            passkey.update_credential(&result);
            save(db, passkey, None).await?;
        }
        Ok(())
    }
}

async fn all(db: &Db) -> Result<Vec<Passkey>> {
    let rows: Vec<String> = db
        .call(|c| c.prepare("SELECT passkey FROM passkeys ORDER BY created_at")?.query_map([], |r| r.get(0))?.collect())
        .await?;
    rows.iter().map(|json| serde_json::from_str(json).context("a stored passkey is unreadable")).collect()
}

/// Inserts a new passkey with `name`, or updates a stored one (`name` = `None`).
async fn save(db: &Db, passkey: &Passkey, name: Option<String>) -> Result<()> {
    let (id, json) = (passkey.cred_id().clone(), serde_json::to_string(passkey)?);
    db.call(move |c| match name {
        Some(name) => c.execute(
            "INSERT INTO passkeys (id, name, passkey, created_at) VALUES (?1, ?2, ?3, unixepoch())",
            (id, name, json),
        ),
        None => c.execute("UPDATE passkeys SET passkey = ?2, last_used_at = unixepoch() WHERE id = ?1", (id, json)),
    })
    .await?;
    Ok(())
}

pub async fn count(db: &Db) -> Result<u64> {
    db.call(|c| c.query_row("SELECT count(*) FROM passkeys", [], |r| r.get::<_, i64>(0)))
        .await
        .map(|n| n.unsigned_abs())
}

pub async fn list(db: &Db) -> Result<Vec<StoredPasskey>> {
    db.call(|c| {
        c.prepare("SELECT id, name, created_at, last_used_at FROM passkeys ORDER BY created_at")?
            .query_map([], |r| {
                Ok(StoredPasskey { id: r.get(0)?, name: r.get(1)?, created_at: r.get(2)?, last_used_at: r.get(3)? })
            })?
            .collect()
    })
    .await
}

pub async fn remove(db: &Db, id: Vec<u8>) -> Result<()> {
    db.call(move |c| c.execute("DELETE FROM passkeys WHERE id = ?1", [id])).await?;
    Ok(())
}
