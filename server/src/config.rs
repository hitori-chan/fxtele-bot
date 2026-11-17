//! Environment variables: only what's needed to start. Everything else is a live setting.

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use ipnet::IpNet;
use url::Url;

use crate::client_ip::ClientIpFrom;
use crate::extract::Source;

/// No `Debug`: it holds the bot's token, which mustn't reach a log.
#[derive(Clone)]
pub struct Config {
    pub public_addr: SocketAddr,
    pub dashboard_addr: SocketAddr,
    pub data_dir: PathBuf,
    pub client_ip_from: ClientIpFrom,
    pub trusted_proxies: Vec<IpNet>,
    /// The dashboard's public URL (origin only), needed for passkeys.
    pub dashboard_url: Option<Url>,
    /// The public hostname whose links are Instagram's (`INSTAGRAM_HOST`); every other one serves
    /// Facebook. Unset, Instagram links are only read in the dashboard.
    pub instagram_host: Option<String>,
    pub access: Option<AccessConfig>,
    pub telegram_bot_token: Option<String>,
    /// Where 1080p videos are kept.
    pub video_bucket: Option<crate::video::BucketConfig>,
}

/// Cloudflare Access: the team name (`<team>.cloudflareaccess.com`) and the application's audience tag.
#[derive(Debug, Clone)]
pub struct AccessConfig {
    pub team: String,
    pub aud: String,
}

/// Loopback: a front end on the same machine. Not private ranges,
/// or anyone on the same network could claim to be Telegram with a header.
pub const DEFAULT_TRUSTED_PROXIES: &[&str] = &["127.0.0.0/8", "::1/128"];

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
        let instagram_host = get("INSTAGRAM_HOST")
            .map(|value| {
                let host = value.to_ascii_lowercase();
                let bare =
                    !host.is_empty() && host.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
                if !bare {
                    bail!("INSTAGRAM_HOST={value:?} must be a bare hostname like ig.example.com");
                }
                Ok(host)
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
            instagram_host,
            access,
            telegram_bot_token: get("TELEGRAM_BOT_TOKEN"),
            video_bucket: crate::video::BucketConfig::from_lookup(&get)?,
        })
    }
}

impl Config {
    /// The source a request's `Host` header (with or without a port) is for.
    pub fn source_for_host(&self, host: Option<&str>) -> Source {
        let name =
            host.map(|h| h.rsplit_once(':').filter(|(_, port)| port.parse::<u16>().is_ok()).map_or(h, |(n, _)| n));
        match (&self.instagram_host, name) {
            (Some(instagram), Some(name)) if name.eq_ignore_ascii_case(instagram) => Source::Instagram,
            _ => Source::Facebook,
        }
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
        let trusted =
            |ip: &str| config.trusted_proxies.iter().any(|n| n.contains(&ip.parse::<std::net::IpAddr>().unwrap()));
        assert!(trusted("127.0.0.1") && trusted("::1"));
        assert!(!trusted("192.168.1.1") && !trusted("10.0.0.1"), "only this machine");
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
        assert!(config(&[("INSTAGRAM_HOST", "https://ig.example.com")]).is_err());
    }

    #[test]
    fn hosts_pick_the_source() {
        let config = config(&[("INSTAGRAM_HOST", "IG.example.com")]).unwrap();
        for host in ["ig.example.com", "ig.example.com:8080", "IG.Example.com"] {
            assert_eq!(config.source_for_host(Some(host)), Source::Instagram, "{host}");
        }
        for host in [Some("fb.example.com"), Some("ig.example.com.evil.org"), None] {
            assert_eq!(config.source_for_host(host), Source::Facebook, "{host:?}");
        }
        let unset = Config::from_lookup(|_| None).unwrap();
        assert_eq!(unset.source_for_host(Some("ig.example.com")), Source::Facebook, "unset: all Facebook");
    }
}
