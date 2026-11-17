//! Passkeys via webauthn-rs. They're tied to `DASHBOARD_URL`'s hostname. Signing in offers
//! every registered passkey, so no username is needed.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, ensure};
use url::Url;
use webauthn_rs::prelude::{
    CreationChallengeResponse, Passkey, PasskeyAuthentication, PasskeyRegistration, PublicKeyCredential,
    RegisterPublicKeyCredential, RequestChallengeResponse, Uuid,
};
use webauthn_rs::{Webauthn, WebauthnBuilder};

use super::random_token;
use crate::db::Db;
use crate::sync::lock;

const USER_KEY: &str = "webauthn_user_id";
const CEREMONY_TTL: Duration = Duration::from_secs(300);
/// Ceremonies of one kind (sign-ins, or passkeys being added) under way at most, from one address
/// and from all: started and not finished within `CEREMONY_TTL`. Starting a sign-in needs no
/// sign-in, so one address can't take them all.
const MAX_PER_ADDRESS: usize = 5;
const MAX_CEREMONIES: usize = 1000;

enum Ceremony {
    Register { state: PasskeyRegistration, name: String },
    Login(PasskeyAuthentication),
}

pub struct Passkeys {
    webauthn: Webauthn,
    /// By ID: when it started, from which address (its /64 for IPv6), and what it is.
    ceremonies: Mutex<HashMap<String, (Instant, IpAddr, Ceremony)>>,
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
        // Passkeys belong to a domain; webauthn-rs would only say "The configuration was invalid".
        ensure!(
            dashboard_url.domain().is_some(),
            "DASHBOARD_URL must use a hostname, not an IP address: e.g. https://homura.example.com, or http://localhost:8081 on this machine"
        );
        let webauthn = WebauthnBuilder::new(host, dashboard_url)
            .and_then(|b| b.rp_name("Homura").build())
            .map_err(|e| anyhow!("can't set up passkeys for DASHBOARD_URL: {e}"))?;
        Ok(Self { webauthn, ceremonies: Mutex::new(HashMap::new()) })
    }

    /// Keeps a started ceremony for its finish, within the caps.
    fn remember(&self, ceremony: Ceremony, ip: IpAddr) -> Result<String> {
        let (id, ip) = (random_token(), crate::client_ip::network(ip));
        let registering = matches!(ceremony, Ceremony::Register { .. });
        let mut ceremonies = lock(&self.ceremonies);
        make_room(&mut ceremonies, |c| matches!(c, Ceremony::Register { .. }), registering, ip, MAX_CEREMONIES)?;
        ceremonies.insert(id.clone(), (Instant::now(), ip, ceremony));
        Ok(id)
    }

    fn take(&self, id: &str) -> Option<Ceremony> {
        let mut ceremonies = lock(&self.ceremonies);
        ceremonies.remove(id).filter(|(at, ..)| at.elapsed() < CEREMONY_TTL).map(|(.., c)| c)
    }

    /// Drops sign-ins under way, for passkeys that may since have been removed.
    pub fn forget_sign_ins(&self) {
        lock(&self.ceremonies).retain(|_, (.., c)| !matches!(c, Ceremony::Login(_)));
    }

    pub async fn start_registration(
        &self,
        db: &Db,
        name: &str,
        ip: IpAddr,
    ) -> Result<(String, CreationChallengeResponse)> {
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
            .start_passkey_registration(user, "admin", "Homura admin", Some(existing))
            .map_err(|e| anyhow!("{e}"))?;
        let name = name.trim().chars().take(60).collect::<String>();
        let name = if name.is_empty() { "Passkey".to_string() } else { name };
        Ok((self.remember(Ceremony::Register { state, name }, ip)?, challenge))
    }

    pub async fn finish_registration(&self, db: &Db, id: &str, credential: &RegisterPublicKeyCredential) -> Result<()> {
        let Some(Ceremony::Register { state, name }) = self.take(id) else {
            return Err(anyhow!("that passkey request expired; try again"));
        };
        let passkey = self.webauthn.finish_passkey_registration(credential, &state).map_err(|e| anyhow!("{e}"))?;
        save(db, &passkey, Some(name)).await
    }

    pub async fn start_login(&self, db: &Db, ip: IpAddr) -> Result<(String, RequestChallengeResponse)> {
        let passkeys = all(db).await?;
        if passkeys.is_empty() {
            return Err(anyhow!("no passkeys are registered"));
        }
        let (challenge, state) = self.webauthn.start_passkey_authentication(&passkeys).map_err(|e| anyhow!("{e}"))?;
        Ok((self.remember(Ceremony::Login(state), ip)?, challenge))
    }

    /// Checks a sign-in and updates the passkey's counter. The passkey must still be stored:
    /// webauthn-rs checks against the list taken when the sign-in started.
    pub async fn finish_login(&self, db: &Db, id: &str, credential: &PublicKeyCredential) -> Result<()> {
        let Some(Ceremony::Login(state)) = self.take(id) else {
            return Err(anyhow!("that sign-in request expired; try again"));
        };
        let result = self.webauthn.finish_passkey_authentication(credential, &state).map_err(|e| anyhow!("{e}"))?;
        let mut passkeys = all(db).await?;
        let passkey = passkeys
            .iter_mut()
            .find(|p| p.cred_id() == result.cred_id())
            .ok_or_else(|| anyhow!("that passkey was removed"))?;
        passkey.update_credential(&result);
        save(db, passkey, None).await
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

/// Makes room for a new ceremony from `ip`: drops those past `CEREMONY_TTL`, and refuses one past
/// the address's cap. Sign-ins and registrations are counted apart, as starting a sign-in needs no
/// sign-in: strangers' can't stop a passkey being added. With `max` sign-ins under way, the oldest
/// goes, so filling the table locks no one out for long.
fn make_room<C>(
    ceremonies: &mut HashMap<String, (Instant, IpAddr, C)>,
    is_registration: impl Fn(&C) -> bool,
    registering: bool,
    ip: IpAddr,
    max: usize,
) -> Result<()> {
    ceremonies.retain(|_, (at, ..)| at.elapsed() < CEREMONY_TTL);
    let alike = |c: &C| is_registration(c) == registering;
    let busy = if registering {
        "too many passkeys being added at once; try again in a few minutes"
    } else {
        "too many passkey sign-ins under way; try again in a few minutes"
    };
    let mine = ceremonies.values().filter(|(_, from, c)| *from == ip && alike(c)).count();
    ensure!(mine < MAX_PER_ADDRESS, busy);
    if ceremonies.values().filter(|(.., c)| alike(c)).count() >= max {
        ensure!(!registering, busy);
        let oldest = ceremonies.iter().filter(|(_, (.., c))| alike(c)).min_by_key(|(_, (at, ..))| *at);
        if let Some(oldest) = oldest.map(|(id, _)| id.clone()) {
            ceremonies.remove(&oldest);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strangers_cant_fill_the_ceremonies() {
        let mut ceremonies: HashMap<String, (Instant, IpAddr, bool)> = HashMap::new();
        let address = |n: u8| IpAddr::from([198, 51, 100, n]);
        let add = |c: &mut HashMap<_, _>, n: u8, registering: bool, key: &str, max: usize| {
            let room = make_room(c, |r: &bool| *r, registering, address(n), max);
            if room.is_ok() {
                c.insert(key.to_string(), (Instant::now(), address(n), registering));
            }
            room
        };
        // One address gets five sign-ins at most.
        for i in 0..5 {
            add(&mut ceremonies, 1, false, &format!("a{i}"), 100).unwrap();
        }
        assert!(add(&mut ceremonies, 1, false, "a5", 100).is_err());
        // A passkey being added isn't held up by sign-ins.
        add(&mut ceremonies, 1, true, "r", 100).unwrap();
        // A full table of sign-ins drops the oldest rather than refusing.
        ceremonies.clear();
        for n in 0..6 {
            add(&mut ceremonies, n, false, &format!("s{n}"), 4).unwrap();
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(ceremonies.len(), 4);
        assert!(!ceremonies.contains_key("s0") && ceremonies.contains_key("s5"));
    }

    #[test]
    fn needs_a_hostname() {
        assert!(Passkeys::new(&Url::parse("https://homura.example.com").unwrap()).is_ok());
        assert!(Passkeys::new(&Url::parse("http://localhost:8081").unwrap()).is_ok());
        let error = Passkeys::new(&Url::parse("http://127.0.0.1:8081").unwrap()).err().unwrap().to_string();
        assert!(error.contains("not an IP address"), "{error}");
    }
}
