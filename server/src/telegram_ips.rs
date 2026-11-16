//! Telegram's published IP ranges: a bundled copy, replaced by a daily refresh.

use std::net::IpAddr;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use arc_swap::ArcSwap;
use ipnet::IpNet;
use serde::{Deserialize, Serialize};

use crate::db::Db;

pub const SOURCE_URL: &str = "https://core.telegram.org/resources/cidr.txt";
const BUNDLED: &str = include_str!("telegram_cidr.txt");
const DB_KEY: &str = "telegram_ranges";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ranges {
    pub nets: Vec<IpNet>,
    /// Unix time of the last successful refresh; `None` for the bundled copy.
    pub refreshed_at: Option<i64>,
}

impl Ranges {
    pub fn bundled() -> Self {
        Self { nets: parse(BUNDLED).expect("bundled ranges parse"), refreshed_at: None }
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        self.nets.iter().any(|net| net.contains(&ip))
    }
}

/// The widest ranges taken. Telegram's are /20 to /24 and /32 to /48; a wider one (`0.0.0.0/0`
/// from a page that went wrong) would let anyone in as Telegram.
const WIDEST_V4: u8 = 16;
const WIDEST_V6: u8 = 32;

/// Parses cidr.txt: one range per line. Refuses an empty or partly unreadable list, or one with
/// a range wider than Telegram's own could be.
pub fn parse(text: &str) -> Result<Vec<IpNet>> {
    let nets = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let net = line.parse::<IpNet>().with_context(|| format!("bad range {line:?}"))?;
            let widest = if matches!(net, IpNet::V4(_)) { WIDEST_V4 } else { WIDEST_V6 };
            if net.prefix_len() < widest {
                bail!("the range {line} is far wider than any of Telegram's");
            }
            Ok(net)
        })
        .collect::<Result<Vec<_>>>()?;
    if nets.is_empty() {
        bail!("the list is empty");
    }
    Ok(nets)
}

pub struct TelegramIps {
    ranges: ArcSwap<Ranges>,
}

impl TelegramIps {
    pub fn new(ranges: Ranges) -> Self {
        Self { ranges: ArcSwap::from_pointee(ranges) }
    }

    /// The saved list from the last refresh, or the bundled copy.
    pub async fn load(db: &Db) -> Self {
        let saved = db.get_json::<Ranges>(DB_KEY).await.ok().flatten().filter(|r| !r.nets.is_empty());
        Self::new(saved.unwrap_or_else(Ranges::bundled))
    }

    pub fn current(&self) -> Arc<Ranges> {
        self.ranges.load_full()
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        self.ranges.load().contains(ip)
    }

    pub async fn refresh(&self, client: &reqwest::Client, db: &Db) -> Result<usize> {
        let text = client.get(SOURCE_URL).send().await?.error_for_status()?.text().await?;
        let nets = parse(&text)?;
        let count = nets.len();
        let ranges = Ranges { nets, refreshed_at: Some(jiff::Timestamp::now().as_second()) };
        db.set_json(DB_KEY, &ranges).await?;
        self.ranges.store(Arc::new(ranges));
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_list_covers_known_fetcher_addresses() {
        let ranges = Ranges::bundled();
        assert!(ranges.contains("149.154.161.200".parse().unwrap()));
        assert!(ranges.contains("91.108.4.1".parse().unwrap()));
        assert!(ranges.contains("2001:b28:f23d::1".parse().unwrap()));
        assert!(!ranges.contains("1.1.1.1".parse().unwrap()));
    }

    #[test]
    fn refuses_bad_lists() {
        assert!(parse("").is_err());
        assert!(parse("<html>blocked</html>").is_err());
        assert_eq!(parse("# comment\n10.0.0.0/16\n\n").unwrap().len(), 1);
        for wide in ["0.0.0.0/0", "10.0.0.0/8", "::/0", "2001::/16"] {
            assert!(parse(&format!("91.108.4.0/22\n{wide}")).is_err(), "{wide}");
        }
    }
}
