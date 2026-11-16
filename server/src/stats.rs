//! Request counts by outcome and page type, kept in memory and written to SQLite once a minute.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use anyhow::Result;
use serde::Serialize;
use tokio::sync::broadcast;

use crate::db::Db;
use crate::extract::Stage;
use crate::extract::facebook::url::PageType;
use crate::sync::lock;

const MINUTE_DAYS: i64 = 7;
const HOUR_DAYS: i64 = 90;
const KEEP_FAILURES: i64 = 500;
const KEEP_SUSPECTS: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum Outcome {
    /// A person (or another app) was sent to Facebook.
    Redirect,
    /// Telegram got a cached post.
    Hit,
    /// Telegram got a freshly extracted post.
    Extracted,
    Failed(Stage),
    /// The per-IP limit refused the request.
    RateLimited,
    /// No Facebook budget was free in time.
    Busy,
}

impl Outcome {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Redirect => "redirect",
            Self::Hit => "hit",
            Self::Extracted => "extracted",
            Self::Failed(Stage::Fetch) => "failed:fetch",
            Self::Failed(Stage::Redirect) => "failed:redirect",
            Self::Failed(Stage::LoginWall) => "failed:login_wall",
            Self::Failed(Stage::NotFound) => "failed:not_found",
            Self::Failed(Stage::Timeout) => "failed:timeout",
            Self::RateLimited => "rate_limited",
            Self::Busy => "busy",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Agg {
    pub count: u64,
    pub total_ms: u64,
    pub max_ms: u64,
}

impl Agg {
    fn add(&mut self, ms: u64) {
        self.merge(Self { count: 1, total_ms: ms, max_ms: ms });
    }

    fn merge(&mut self, other: Self) {
        self.count += other.count;
        self.total_ms += other.total_ms;
        self.max_ms = self.max_ms.max(other.max_ms);
    }
}

/// Sent to live dashboard pages for every counted request.
#[derive(Debug, Clone, Serialize)]
pub struct Event {
    pub outcome: &'static str,
    pub page_type: &'static str,
    pub ms: u64,
}

/// A request that claimed to be Telegram but came from outside its ranges.
#[derive(Debug, Clone, Serialize)]
pub struct Suspect {
    pub at: i64,
    pub ip: IpAddr,
    pub user_agent: String,
    /// The address was a trusted proxy's own, so CLIENT_IP_FROM is probably wrong.
    pub from_proxy: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Failure {
    pub at: i64,
    pub url: String,
    pub page_type: String,
    pub stage: String,
    pub error: String,
}

type Key = (i64, &'static str, &'static str);

pub struct Stats {
    db: Db,
    pending: Mutex<HashMap<Key, Agg>>,
    suspects: Mutex<VecDeque<Suspect>>,
    last_proxy_warning: AtomicI64,
    events: broadcast::Sender<Event>,
}

fn now() -> i64 {
    jiff::Timestamp::now().as_second()
}

impl Stats {
    pub fn new(db: Db) -> Self {
        Self {
            db,
            pending: Mutex::new(HashMap::new()),
            suspects: Mutex::new(VecDeque::new()),
            last_proxy_warning: AtomicI64::new(0),
            events: broadcast::channel(256).0,
        }
    }

    pub fn record(&self, outcome: Outcome, page_type: Option<PageType>, elapsed: Duration) {
        let ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        let page_type = page_type.map_or("-", PageType::as_str);
        let label = outcome.label();
        let minute = now() / 60 * 60;
        lock(&self.pending).entry((minute, label, page_type)).or_default().add(ms);
        if self.events.receiver_count() > 0 {
            // A page closing in between is fine.
            let _ = self.events.send(Event { outcome: label, page_type, ms });
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// Remembers a request with Telegram's user agent from outside Telegram's ranges.
    pub fn suspect(&self, ip: IpAddr, user_agent: &str, from_proxy: bool) {
        let at = now();
        if from_proxy && at - self.last_proxy_warning.swap(at, Ordering::Relaxed) > 600 {
            tracing::warn!(
                %ip,
                "a request with Telegram's user agent came from a proxy address; \
                 CLIENT_IP_FROM is probably wrong, so Telegram gets redirected instead of previews"
            );
        }
        let mut suspects = lock(&self.suspects);
        if suspects.len() == KEEP_SUSPECTS {
            suspects.pop_front();
        }
        let user_agent = user_agent.chars().take(200).collect();
        suspects.push_back(Suspect { at, ip, user_agent, from_proxy });
    }

    pub fn suspects(&self) -> Vec<Suspect> {
        lock(&self.suspects).iter().rev().cloned().collect()
    }

    /// Whether Telegram-looking requests arrived from a proxy address in the last hour.
    pub fn proxy_warning(&self) -> bool {
        now() - self.last_proxy_warning.load(Ordering::Relaxed) < 3600
    }

    pub async fn failure(&self, url: &str, page_type: PageType, stage: Stage, error: &str) -> Result<()> {
        let (url, error) = (url.to_string(), error.chars().take(500).collect::<String>());
        self.db
            .call(move |c| {
                c.execute(
                    "INSERT INTO failures (at, url, page_type, stage, error) VALUES (?1, ?2, ?3, ?4, ?5)",
                    (now(), url, page_type.as_str(), stage.as_str(), error),
                )?;
                c.execute("DELETE FROM failures WHERE id <= (SELECT max(id) FROM failures) - ?1", [KEEP_FAILURES])
            })
            .await?;
        Ok(())
    }

    pub async fn failures(&self, limit: u32) -> Result<Vec<Failure>> {
        self.db
            .call(move |c| {
                let mut statement =
                    c.prepare("SELECT at, url, page_type, stage, error FROM failures ORDER BY id DESC LIMIT ?1")?;
                statement
                    .query_map([limit], |r| {
                        Ok(Failure {
                            at: r.get(0)?,
                            url: r.get(1)?,
                            page_type: r.get(2)?,
                            stage: r.get(3)?,
                            error: r.get(4)?,
                        })
                    })?
                    .collect()
            })
            .await
    }

    /// Adds the counts gathered since the last flush to the one-minute buckets.
    pub async fn flush(&self) -> Result<()> {
        let pending = std::mem::take(&mut *lock(&self.pending));
        if pending.is_empty() {
            return Ok(());
        }
        self.db
            .call(move |c| {
                let tx = c.transaction()?;
                {
                    let mut upsert = tx.prepare(
                        "INSERT INTO stats (size, bucket, outcome, page_type, count, total_ms, max_ms)
                         VALUES (60, ?1, ?2, ?3, ?4, ?5, ?6)
                         ON CONFLICT (size, bucket, outcome, page_type) DO UPDATE SET
                           count = count + excluded.count,
                           total_ms = total_ms + excluded.total_ms,
                           max_ms = max(max_ms, excluded.max_ms)",
                    )?;
                    for ((minute, outcome, page_type), agg) in pending {
                        upsert.execute((
                            minute,
                            outcome,
                            page_type,
                            to_i64(agg.count),
                            to_i64(agg.total_ms),
                            to_i64(agg.max_ms),
                        ))?;
                    }
                }
                tx.commit()
            })
            .await
    }

    /// Rolls minute buckets older than 7 days into hours and drops hours older than 90 days.
    pub async fn rollup(&self) -> Result<()> {
        let minute_cutoff = now() - MINUTE_DAYS * 86_400;
        let hour_cutoff = now() - HOUR_DAYS * 86_400;
        self.db
            .call(move |c| {
                let tx = c.transaction()?;
                tx.execute(
                    "INSERT INTO stats (size, bucket, outcome, page_type, count, total_ms, max_ms)
                     SELECT 3600, bucket / 3600 * 3600, outcome, page_type, sum(count), sum(total_ms), max(max_ms)
                     FROM stats WHERE size = 60 AND bucket < ?1
                     GROUP BY bucket / 3600, outcome, page_type
                     ON CONFLICT (size, bucket, outcome, page_type) DO UPDATE SET
                       count = count + excluded.count,
                       total_ms = total_ms + excluded.total_ms,
                       max_ms = max(max_ms, excluded.max_ms)",
                    [minute_cutoff],
                )?;
                tx.execute("DELETE FROM stats WHERE size = 60 AND bucket < ?1", [minute_cutoff])?;
                tx.execute("DELETE FROM stats WHERE size = 3600 AND bucket < ?1", [hour_cutoff])?;
                tx.commit()
            })
            .await
    }

    /// Totals per (outcome, page type) since `since`, including counts not yet flushed.
    pub async fn totals(&self, since: i64) -> Result<HashMap<(String, String), Agg>> {
        let mut totals = self.stored(since).await?;
        for (&(minute, outcome, page_type), agg) in lock(&self.pending).iter() {
            if minute >= since {
                totals.entry((outcome.to_string(), page_type.to_string())).or_default().merge(*agg);
            }
        }
        Ok(totals)
    }

    /// Hourly and minute buckets never cover the same minutes, so they simply add up.
    async fn stored(&self, since: i64) -> Result<HashMap<(String, String), Agg>> {
        let rows: Vec<(String, String, i64, i64, i64)> = self
            .db
            .call(move |c| {
                let mut statement = c.prepare(
                    "SELECT outcome, page_type, sum(count), sum(total_ms), max(max_ms) FROM stats
                     WHERE bucket >= ?1
                     GROUP BY outcome, page_type",
                )?;
                statement.query_map([since], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?.collect()
            })
            .await?;
        Ok(rows
            .into_iter()
            .map(|(outcome, page_type, count, total, max)| {
                ((outcome, page_type), Agg { count: to_u64(count), total_ms: to_u64(total), max_ms: to_u64(max) })
            })
            .collect())
    }

    /// Request counts per bucket of `step` seconds since `since`, by outcome, for charts.
    pub async fn series(&self, since: i64, step: i64) -> Result<Vec<(i64, String, u64)>> {
        self.flush().await?;
        let rows: Vec<(i64, String, i64)> = self
            .db
            .call(move |c| {
                let mut statement = c.prepare(
                    "SELECT bucket / ?2 * ?2 AS b, outcome, sum(count) FROM stats
                     WHERE bucket >= ?1
                     GROUP BY b, outcome ORDER BY b",
                )?;
                statement.query_map((since, step), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect()
            })
            .await?;
        Ok(rows.into_iter().map(|(bucket, outcome, count)| (bucket, outcome, to_u64(count))).collect())
    }
}

fn to_i64(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

fn to_u64(n: i64) -> u64 {
    u64::try_from(n).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_labels_name_their_stage() {
        for stage in Stage::ALL {
            assert_eq!(Outcome::Failed(stage).label(), format!("failed:{}", stage.as_str()));
        }
    }

    #[tokio::test]
    async fn counts_flush_and_add_up() {
        let stats = Stats::new(Db::memory().await.unwrap());
        stats.record(Outcome::Hit, Some(PageType::Reel), Duration::from_millis(10));
        stats.record(Outcome::Hit, Some(PageType::Reel), Duration::from_millis(30));
        stats.record(Outcome::Redirect, None, Duration::from_millis(1));
        stats.flush().await.unwrap();
        stats.record(Outcome::Hit, Some(PageType::Reel), Duration::from_millis(50));

        let totals = stats.totals(0).await.unwrap();
        let hit = totals[&("hit".to_string(), "reel".to_string())];
        assert_eq!((hit.count, hit.total_ms, hit.max_ms), (3, 90, 50));
        assert_eq!(totals[&("redirect".to_string(), "-".to_string())].count, 1);

        stats.flush().await.unwrap();
        let series = stats.series(0, 3600).await.unwrap();
        assert_eq!(series.iter().map(|s| s.2).sum::<u64>(), 4);
    }

    #[tokio::test]
    async fn rollup_moves_old_minutes_into_hours() {
        let db = Db::memory().await.unwrap();
        let stats = Stats::new(db.clone());
        let old = now() - 8 * 86_400;
        let old_minute = old / 60 * 60;
        db.call(move |c| {
            c.execute(
                "INSERT INTO stats VALUES (60, ?1, 'hit', 'reel', 2, 20, 15), (60, ?2, 'hit', 'reel', 3, 30, 12)",
                (old_minute, old_minute + 60),
            )
        })
        .await
        .unwrap();
        stats.rollup().await.unwrap();
        let rows: Vec<(i64, i64, i64)> = db
            .call(|c| {
                c.prepare("SELECT size, count, max_ms FROM stats")?
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                    .collect()
            })
            .await
            .unwrap();
        // Two minutes in the same hour, or one each side of an hour boundary.
        assert!(rows.iter().all(|r| r.0 == 3600));
        assert_eq!(rows.iter().map(|r| r.1).sum::<i64>(), 5);
        assert_eq!(stats.totals(0).await.unwrap()[&("hit".to_string(), "reel".to_string())].count, 5);
    }

    #[tokio::test]
    async fn keeps_recent_failures() {
        let stats = Stats::new(Db::memory().await.unwrap());
        for i in 0..3 {
            stats
                .failure(&format!("https://www.facebook.com/reel/{i}"), PageType::Reel, Stage::NotFound, "x")
                .await
                .unwrap();
        }
        let failures = stats.failures(2).await.unwrap();
        assert_eq!(failures.len(), 2);
        assert_eq!(failures[0].url, "https://www.facebook.com/reel/2");
    }

    #[test]
    fn suspects_are_capped() {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let stats = Stats::new(rt.block_on(Db::memory()).unwrap());
        for _ in 0..60 {
            stats.suspect("1.2.3.4".parse().unwrap(), "TelegramBot (like TwitterBot)", false);
        }
        assert_eq!(stats.suspects().len(), KEEP_SUSPECTS);
        assert!(!stats.proxy_warning());
        stats.suspect("127.0.0.1".parse().unwrap(), "TelegramBot (like TwitterBot)", true);
        assert!(stats.proxy_warning());
    }
}
