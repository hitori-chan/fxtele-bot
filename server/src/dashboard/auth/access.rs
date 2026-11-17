//! Cloudflare Access: a request with a valid `Cf-Access-Jwt-Assertion` token is signed in.
//! The token's signature is checked against the team's published keys, which are cached.

use std::sync::{PoisonError, RwLock};
use std::time::{Duration, Instant};

use anyhow::Result;
use axum::http::HeaderMap;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::config::AccessConfig;

const KEYS_TTL: Duration = Duration::from_secs(3600);
const REFETCH_AFTER: Duration = Duration::from_secs(60);
/// Sign-ins whose key isn't held wait for the fetch.
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Deserialize)]
struct Claims {
    #[serde(default)]
    email: Option<String>,
}

pub struct AccessVerifier {
    aud: String,
    issuer: String,
    certs_url: String,
    client: reqwest::Client,
    keys: RwLock<Option<(Instant, JwkSet)>>,
    /// One fetch at a time. Requests whose key is known never wait for it: a token naming an
    /// unknown key, which anyone can send, mustn't hold up everyone else's sign-in.
    fetching: Mutex<()>,
    /// The latest fetch of the keys, worked or not: a failing one isn't retried for every request.
    tried: std::sync::Mutex<Option<Instant>>,
}

impl AccessVerifier {
    pub fn new(config: &AccessConfig) -> Result<Self> {
        let issuer = format!("https://{}.cloudflareaccess.com", config.team);
        Ok(Self {
            aud: config.aud.clone(),
            certs_url: format!("{issuer}/cdn-cgi/access/certs"),
            issuer,
            client: crate::net::client()?,
            keys: RwLock::new(None),
            fetching: Mutex::new(()),
            tried: std::sync::Mutex::new(None),
        })
    }

    pub const fn team_domain(&self) -> &str {
        self.issuer.as_str()
    }

    /// The signed-in email if the request carries a valid Access token.
    pub async fn verify(&self, headers: &HeaderMap) -> Option<String> {
        let token = token(headers)?;
        let kid = decode_header(&token).ok()?.kid?;
        let key = self.key(&kid).await?;
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_audience(&[&self.aud]);
        validation.set_issuer(&[&self.issuer]);
        let data = decode::<Claims>(&token, &key, &validation).ok()?;
        Some(data.claims.email.unwrap_or_else(|| "Cloudflare Access".into()))
    }

    async fn key(&self, kid: &str) -> Option<DecodingKey> {
        if let Some(key) = self.cached(kid, true) {
            return Some(key);
        }
        let _one = self.fetching.lock().await;
        // Someone else may have fetched them while this waited.
        if let Some(key) = self.cached(kid, true) {
            return Some(key);
        }
        if self.may_fetch() {
            match self.fetch().await {
                Ok(set) => *self.keys.write().unwrap_or_else(PoisonError::into_inner) = Some((Instant::now(), set)),
                Err(e) => tracing::warn!("can't fetch Cloudflare Access keys: {e:#}"),
            }
        }
        // Keys past their hour still do while Cloudflare can't be reached.
        self.cached(kid, false)
    }

    /// Key `kid` from the keys held, if they're under an hour old or `fresh` isn't asked for.
    fn cached(&self, kid: &str, fresh: bool) -> Option<DecodingKey> {
        let keys = self.keys.read().unwrap_or_else(PoisonError::into_inner);
        let (fetched, set) = keys.as_ref()?;
        if fresh && fetched.elapsed() >= KEYS_TTL {
            return None;
        }
        DecodingKey::from_jwk(set.find(kid)?).ok()
    }

    /// Once a minute at most.
    fn may_fetch(&self) -> bool {
        let mut tried = self.tried.lock().unwrap_or_else(PoisonError::into_inner);
        if tried.is_some_and(|at| at.elapsed() < REFETCH_AFTER) {
            return false;
        }
        *tried = Some(Instant::now());
        true
    }

    async fn fetch(&self) -> Result<JwkSet> {
        let response = self.client.get(&self.certs_url).timeout(FETCH_TIMEOUT).send().await?;
        Ok(response.error_for_status()?.json().await?)
    }

    #[cfg(test)]
    fn set_keys(&self, set: JwkSet) {
        *self.keys.write().unwrap_or_else(PoisonError::into_inner) = Some((Instant::now(), set));
    }
}

fn token(headers: &HeaderMap) -> Option<String> {
    if let Some(value) = headers.get("cf-access-jwt-assertion").and_then(|v| v.to_str().ok()) {
        return Some(value.trim().to_string());
    }
    super::cookie(headers, "CF_Authorization")
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;
    use jsonwebtoken::{EncodingKey, Header, encode};
    use serde_json::json;

    use super::*;

    // A throwaway RSA key made for these tests only.
    const KEY_DER: &str = include_str!("../../../tests/keys/access-test-key.der.b64");
    const KEY_N: &str = include_str!("../../../tests/keys/access-test-key.n.b64url");

    /// Where the tests' keys are fetched from: a server that closes each connection, or with
    /// `stall`, one that never answers.
    async fn key_server(stall: bool) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((connection, _)) = listener.accept().await {
                if stall {
                    held.push(connection);
                }
            }
        });
        format!("http://{addr}/cdn-cgi/access/certs")
    }

    async fn verifier() -> AccessVerifier {
        let mut v = AccessVerifier::new(&AccessConfig { team: "team".into(), aud: "aud1".into() }).unwrap();
        v.certs_url = key_server(false).await;
        let set = json!({ "keys": [{ "kty": "RSA", "kid": "k1", "alg": "RS256", "use": "sig", "n": KEY_N.trim(), "e": "AQAB" }] });
        v.set_keys(serde_json::from_value(set).unwrap());
        v
    }

    fn token(kid: &str, aud: &str, iss: &str, exp: i64) -> HeaderMap {
        let der = data_encoding::BASE64.decode(KEY_DER.trim().as_bytes()).unwrap();
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(kid.into());
        let claims = json!({ "aud": [aud], "iss": iss, "exp": exp, "email": "me@example.com" });
        let jwt = encode(&header, &claims, &EncodingKey::from_rsa_der(&der)).unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("cf-access-jwt-assertion", HeaderValue::from_str(&jwt).unwrap());
        headers
    }

    #[tokio::test]
    async fn checks_signature_audience_issuer_and_expiry() {
        let v = verifier().await;
        let iss = "https://team.cloudflareaccess.com";
        let later = jiff::Timestamp::now().as_second() + 600;
        assert_eq!(v.verify(&token("k1", "aud1", iss, later)).await.as_deref(), Some("me@example.com"));
        assert_eq!(v.verify(&token("k1", "other", iss, later)).await, None, "wrong audience");
        assert_eq!(
            v.verify(&token("k1", "aud1", "https://evil.cloudflareaccess.com", later)).await,
            None,
            "wrong issuer"
        );
        assert_eq!(v.verify(&token("k1", "aud1", iss, 1_000)).await, None, "expired");
        assert_eq!(v.verify(&token("k2", "aud1", iss, later)).await, None, "unknown key");
        let mut forged = token("k1", "aud1", iss, later);
        let jwt = forged["cf-access-jwt-assertion"].to_str().unwrap().to_string();
        let (signed, signature) = jwt.rsplit_once('.').unwrap();
        let first = if signature.starts_with('A') { 'B' } else { 'A' };
        let tampered = format!("{signed}.{first}{}", &signature[1..]);
        forged.insert("cf-access-jwt-assertion", HeaderValue::from_str(&tampered).unwrap());
        assert_eq!(v.verify(&forged).await, None, "bad signature");
        assert_eq!(v.verify(&HeaderMap::new()).await, None);
    }

    #[tokio::test]
    async fn an_unknown_key_doesnt_hold_up_known_ones() {
        let mut v = verifier().await;
        v.certs_url = key_server(true).await;
        let v = std::sync::Arc::new(v);
        let (iss, later) = ("https://team.cloudflareaccess.com", jiff::Timestamp::now().as_second() + 600);
        let unknown = tokio::spawn({
            let v = std::sync::Arc::clone(&v);
            async move { v.verify(&token("k2", "aud1", iss, later)).await }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        let known = tokio::time::timeout(Duration::from_secs(2), v.verify(&token("k1", "aud1", iss, later))).await;
        assert_eq!(known.expect("not held up by the fetch").as_deref(), Some("me@example.com"));
        assert!(!unknown.is_finished(), "still fetching the keys");
        unknown.abort();
    }
}
