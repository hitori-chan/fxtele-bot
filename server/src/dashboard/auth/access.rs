//! Cloudflare Access: a request with a valid `Cf-Access-Jwt-Assertion` token is signed in.
//! The token's signature is checked against the team's published keys, which are cached.

use std::time::{Duration, Instant};

use anyhow::Result;
use axum::http::HeaderMap;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;
use tokio::sync::RwLock;

use crate::config::AccessConfig;

const KEYS_TTL: Duration = Duration::from_secs(3600);
const REFETCH_AFTER: Duration = Duration::from_secs(60);

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
        {
            let keys = self.keys.read().await;
            if let Some((fetched, set)) = keys.as_ref()
                && let Some(jwk) = set.find(kid)
                && fetched.elapsed() < KEYS_TTL
            {
                return DecodingKey::from_jwk(jwk).ok();
            }
        }
        let mut keys = self.keys.write().await;
        // Someone else may have refreshed while we waited; don't refetch for every unknown kid.
        let stale = keys.as_ref().is_none_or(|(fetched, set)| {
            fetched.elapsed() >= KEYS_TTL || (set.find(kid).is_none() && fetched.elapsed() >= REFETCH_AFTER)
        });
        if stale {
            match self.fetch().await {
                Ok(set) => *keys = Some((Instant::now(), set)),
                Err(e) => tracing::warn!("can't fetch Cloudflare Access keys: {e:#}"),
            }
        }
        let jwk = keys.as_ref()?.1.find(kid)?;
        DecodingKey::from_jwk(jwk).ok()
    }

    async fn fetch(&self) -> Result<JwkSet> {
        Ok(self.client.get(&self.certs_url).send().await?.error_for_status()?.json().await?)
    }

    #[cfg(test)]
    pub async fn set_keys(&self, set: JwkSet) {
        *self.keys.write().await = Some((Instant::now(), set));
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

    async fn verifier() -> AccessVerifier {
        let v = AccessVerifier::new(&AccessConfig { team: "team".into(), aud: "aud1".into() }).unwrap();
        let set = json!({ "keys": [{ "kty": "RSA", "kid": "k1", "alg": "RS256", "use": "sig", "n": KEY_N.trim(), "e": "AQAB" }] });
        v.set_keys(serde_json::from_value(set).unwrap()).await;
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
}
