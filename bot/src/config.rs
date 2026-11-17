//! `bot.toml`: loading and the startup checks.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The owner's user ID. Their private chat is always allowed.
    pub owner_id: i64,
    /// People who may use the bot in a private chat. Permanent.
    #[serde(default)]
    pub allowed_users: Vec<i64>,
    /// Groups the bot works in. Permanent.
    #[serde(default)]
    pub allowed_chats: Vec<i64>,
    /// People and groups added from the menu or approved by the owner.
    #[serde(default = "default_state_path")]
    pub state_path: PathBuf,
    #[serde(default = "default_max_links")]
    pub max_links_per_message: usize,
    #[serde(default = "default_user_rate")]
    pub user_replies_per_minute: u32,
    #[serde(default = "default_chat_rate")]
    pub chat_replies_per_minute: u32,
    /// A link to the key's domain, or any subdomain of it, gets the value as its host, and the
    /// value's path, if it has one, in front of its own. An empty value leaves such links alone.
    #[serde(default)]
    pub domains: BTreeMap<String, String>,
}

fn default_state_path() -> PathBuf {
    PathBuf::from("state.json")
}
const fn default_max_links() -> usize {
    10
}
const fn default_user_rate() -> u32 {
    10
}
const fn default_chat_rate() -> u32 {
    20
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("can't read the config file {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("bad config file {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let mut config: Self = toml::from_str(text)?;
        config.check()?;
        Ok(config)
    }

    fn check(&mut self) -> Result<()> {
        if self.owner_id <= 0 {
            bail!("owner_id must be your user ID (a positive number), got {}", self.owner_id);
        }
        for &id in &self.allowed_users {
            if id <= 0 {
                bail!("allowed_users: {id} is a group ID (negative); put groups in allowed_chats");
            }
        }
        for &id in &self.allowed_chats {
            if id >= 0 {
                bail!("allowed_chats: {id} is a user ID (positive); put people in allowed_users");
            }
        }
        if self.max_links_per_message == 0 {
            bail!("max_links_per_message must be at least 1");
        }
        if self.user_replies_per_minute == 0 || self.chat_replies_per_minute == 0 {
            bail!("user_replies_per_minute and chat_replies_per_minute must be at least 1");
        }

        let mut domains = BTreeMap::new();
        for (from, to) in std::mem::take(&mut self.domains) {
            let key = normalize_domain(&from)
                .with_context(|| format!("[domains]: {from:?} isn't a domain like \"x.com\""))?;
            let target = to.trim().trim_end_matches('/');
            let (host, path) = target.split_once('/').map_or((target, ""), |(host, path)| (host, path));
            let host = host.to_ascii_lowercase();
            let path_ok = path.split('/').all(|part| {
                !matches!(part, "" | "." | "..")
                    && part.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'~'))
            });
            if !target.is_empty() && !(is_host_port(&host) && (path.is_empty() || path_ok)) {
                bail!(
                    "[domains]: {to:?} (for {from:?}) must be a host like \"fixupx.com\", without https://, \
                     and at most a plain path after it (\"fb.example.com/fb.watch\"), or \"\" to leave such links alone"
                );
            }
            let target = if path.is_empty() { host } else { format!("{host}/{path}") };
            if domains.insert(key.clone(), target).is_some() {
                bail!("[domains]: {key:?} is listed twice");
            }
        }
        self.domains = domains;
        Ok(())
    }
}

/// Lowercases a domain key and drops a leading `*.` or `.` and a trailing dot.
fn normalize_domain(raw: &str) -> Option<String> {
    let domain = raw.trim().trim_start_matches("*.").trim_start_matches('.').trim_end_matches('.');
    let domain = domain.to_ascii_lowercase();
    is_hostname(&domain).then_some(domain)
}

fn is_hostname(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        })
}

fn is_host_port(value: &str) -> bool {
    match value.rsplit_once(':') {
        Some((host, port)) => is_hostname(host) && port.parse::<u16>().is_ok_and(|p| p > 0),
        None => is_hostname(value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_the_sample() {
        let config = Config::parse("owner_id = 1\n").unwrap();
        assert_eq!(
            (config.max_links_per_message, config.user_replies_per_minute, config.chat_replies_per_minute),
            (10, 10, 20)
        );
        assert_eq!(config.state_path, PathBuf::from("state.json"));
        assert!(config.domains.is_empty());

        let sample = Config::parse(include_str!("../bot.example.toml")).unwrap();
        assert_eq!(sample.domains["facebook.com"], "fb.example.com");
        assert_eq!(sample.allowed_chats, [-1_001_234_567_890]);
    }

    #[test]
    fn normalizes_domains() {
        let config = Config::parse(
            "owner_id = 1\n[domains]\n\"*.X.com.\" = \"FixupX.com\"\n\"youtu.be\" = \"koutube.com:8443\"",
        )
        .unwrap();
        assert_eq!(config.domains["x.com"], "fixupx.com");
        assert_eq!(config.domains["youtu.be"], "koutube.com:8443");
        let config = Config::parse(
            "owner_id = 1\n[domains]\n\"fb.watch\" = \"Fb.example.com/fb.watch/\"\n\"l.facebook.com\" = \"\"",
        )
        .unwrap();
        assert_eq!(config.domains["fb.watch"], "fb.example.com/fb.watch");
        assert_eq!(config.domains["l.facebook.com"], "");
    }

    #[test]
    fn rejects_mistakes() {
        let err = |text: &str| format!("{:#}", Config::parse(text).unwrap_err());
        assert!(err("owner_id = -5").contains("owner_id"));
        assert!(err("owner_id = 1\nallowed_users = [-100]").contains("allowed_users: -100"));
        assert!(err("owner_id = 1\nallowed_chats = [42]").contains("allowed_chats: 42"));
        assert!(Config::parse("owner_id = 1\nalowed_users = [2]").is_err(), "unknown keys");
        let domain = |line: &str| err(&format!("owner_id = 1\n[domains]\n{line}"));
        assert!(domain("\"x.com\" = \"https://fixupx.com\"").contains("without https://"));
        assert!(domain("\"x.com\" = \"fixupx.com/a?b\"").contains("plain path"));
        assert!(domain("\"x.com\" = \"fixupx.com//a\"").contains("plain path"));
        assert!(domain("\"https://x.com\" = \"fixupx.com\"").contains("isn't a domain"));
        assert!(domain("\"x.com\" = \"a.example\"\n\"X.com\" = \"b.example\"").contains("twice"));
    }
}
