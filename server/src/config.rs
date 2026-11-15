//! Environment variables: only what's needed to start. Everything else is a live setting.

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use ipnet::IpNet;
use url::Url;

use crate::client_ip::ClientIpFrom;

#[derive(Debug, Clone)]
pub struct Config {
    pub public_addr: SocketAddr,
    pub dashboard_addr: SocketAddr,
    pub data_dir: PathBuf,
    pub client_ip_from: ClientIpFrom,
    pub trusted_proxies: Vec<IpNet>,
    /// The dashboard's public URL (origin only), needed for passkeys.
    pub dashboard_url: Option<Url>,
    pub access: Option<AccessConfig>,
    pub telegram_bot_token: Option<String>,
}

/// Cloudflare Access: the team name (`<team>.cloudflareaccess.com`) and the application's audience tag.
#[derive(Debug, Clone)]
pub struct AccessConfig {
    pub team: String,
    pub aud: String,
}

/// Loopback and private ranges: where a local front end usually connects from.
pub const DEFAULT_TRUSTED_PROXIES: &[&str] =
    &["127.0.0.0/8", "::1/128", "10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "fc00::/7", "::ffff:127.0.0.0/104"];

impl Config {
    pub fn from_env() -> Result<Self> {
        Self::from_lookup(|name| std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty()))
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let addr = |name: &str, default: &str| -> Result<SocketAddr> {
            let value = get(name).unwrap_or_else(|| default.to_string());
            value.parse().with_context(|| format!("{name}={value:?} isn't an address like 0.0.0.0:8080"))
        };
        let client_ip_from = match get("CLIENT_IP_FROM") {
            Some(value) => value.parse()?,
            None => ClientIpFrom::Peer,
        };
        let trusted_proxies = match get("TRUSTED_PROXIES") {
            Some(value) => value
                .split([',', ' '])
                .filter(|s| !s.is_empty())
                .map(parse_net)
                .collect::<Result<Vec<_>>>()
                .context("TRUSTED_PROXIES")?,
            None => DEFAULT_TRUSTED_PROXIES.iter().map(|s| s.parse().expect("valid default")).collect(),
        };
        let dashboard_url = get("DASHBOARD_URL")
            .map(|value| {
                let url = Url::parse(&value).with_context(|| format!("DASHBOARD_URL={value:?} isn't a URL"))?;
                if !matches!(url.scheme(), "https" | "http") || url.host_str().is_none() {
                    bail!("DASHBOARD_URL={value:?} must look like https://homura.example.com");
                }
                // Keep only the origin.
                Url::parse(&url.origin().ascii_serialization()).context("DASHBOARD_URL")
            })
            .transpose()?;
        let access = match (get("ACCESS_TEAM"), get("ACCESS_AUD")) {
            (Some(team), Some(aud)) => {
                let team = team.trim_end_matches(".cloudflareaccess.com").to_string();
                Some(AccessConfig { team, aud })
            }
            (None, None) => None,
            _ => bail!("set both ACCESS_TEAM and ACCESS_AUD, or neither"),
        };
        Ok(Self {
            public_addr: addr("PUBLIC_ADDR", "0.0.0.0:8080")?,
            dashboard_addr: addr("DASHBOARD_ADDR", "0.0.0.0:8081")?,
            data_dir: get("DATA_DIR").map_or_else(|| PathBuf::from("/data"), PathBuf::from),
            client_ip_from,
            trusted_proxies,
            dashboard_url,
            access,
            telegram_bot_token: get("TELEGRAM_BOT_TOKEN"),
        })
    }
}

/// Parses `10.0.0.0/8` or a bare address.
fn parse_net(value: &str) -> Result<IpNet> {
    value
        .parse::<IpNet>()
        .or_else(|_| value.parse::<std::net::IpAddr>().map(IpNet::from))
        .with_context(|| format!("{value:?} isn't an address or range"))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn config(vars: &[(&str, &str)]) -> Result<Config> {
        let vars: HashMap<String, String> = vars.iter().map(|(k, v)| ((*k).to_string(), (*v).to_string())).collect();
        Config::from_lookup(|name| vars.get(name).cloned())
    }

    #[test]
    fn defaults() {
        let config = config(&[]).unwrap();
        assert_eq!(config.public_addr.to_string(), "0.0.0.0:8080");
        assert_eq!(config.dashboard_addr.to_string(), "0.0.0.0:8081");
        assert_eq!(config.client_ip_from, ClientIpFrom::Peer);
        assert!(config.trusted_proxies.iter().any(|n| n.contains(&"192.168.1.1".parse::<std::net::IpAddr>().unwrap())));
        assert!(config.dashboard_url.is_none() && config.access.is_none());
    }

    #[test]
    fn parses_values() {
        let config = config(&[
            ("CLIENT_IP_FROM", "x-forwarded-for"),
            ("TRUSTED_PROXIES", "10.1.0.0/16, 203.0.113.7"),
            ("DASHBOARD_URL", "https://homura.example.com/some/path"),
            ("ACCESS_TEAM", "myteam.cloudflareaccess.com"),
            ("ACCESS_AUD", "abc"),
        ])
        .unwrap();
        assert_eq!(config.client_ip_from, ClientIpFrom::XForwardedFor);
        assert_eq!(config.trusted_proxies.len(), 2);
        assert_eq!(config.dashboard_url.unwrap().as_str(), "https://homura.example.com/");
        assert_eq!(config.access.unwrap().team, "myteam");
    }

    #[test]
    fn rejects_mistakes() {
        assert!(config(&[("CLIENT_IP_FROM", "forwarded")]).is_err());
        assert!(config(&[("PUBLIC_ADDR", "8080")]).is_err());
        assert!(config(&[("DASHBOARD_URL", "homura.example.com")]).is_err());
        assert!(config(&[("ACCESS_TEAM", "t")]).is_err());
        assert!(config(&[("TRUSTED_PROXIES", "nope")]).is_err());
    }
}
