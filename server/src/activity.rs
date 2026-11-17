//! What happened, for the dashboard's Activity page: changes made on the dashboard, sign-ins
//! (and refused ones), and alerts. Kept in SQLite, the newest [`KEEP`]. Nothing secret goes in:
//! the text says what changed, never to what.

use anyhow::Result;
use serde::Serialize;

use crate::db::Db;

const KEEP: i64 = 1000;
/// Refused sign-ins are kept apart, the newest this many: strangers can make as many as they like,
/// and they mustn't push out the changes, sign-ins and alerts that say what happened.
const KEEP_REFUSED: i64 = 200;
/// How a refused sign-in's text starts.
pub const REFUSED: &str = "Refused a sign-in";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Kind {
    Change,
    SignIn,
    Alert,
}

impl Kind {
    pub const ALL: [Self; 3] = [Self::Change, Self::SignIn, Self::Alert];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Change => "change",
            Self::SignIn => "sign_in",
            Self::Alert => "alert",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == text)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub at: i64,
    pub kind: String,
    pub actor: String,
    pub text: String,
}

/// Notes something; a failure is logged, not returned, since the action itself went through.
pub async fn record(db: &Db, kind: Kind, actor: &str, text: &str) {
    let (actor, text) = (actor.chars().take(200).collect::<String>(), text.chars().take(1000).collect::<String>());
    let at = jiff::Timestamp::now().as_second();
    let saved = db
        .call(move |c| {
            c.execute(
                "INSERT INTO activity (at, kind, actor, text) VALUES (?1, ?2, ?3, ?4)",
                (at, kind.as_str(), actor, text),
            )?;
            // Each kept on its own: the refused ones, then everything else.
            let prune = |refused: bool, keep: i64| {
                c.execute(
                    "DELETE FROM activity WHERE id IN (
                         SELECT id FROM activity WHERE (kind = 'sign_in' AND text LIKE ?1 || '%') = ?2
                         ORDER BY id DESC LIMIT -1 OFFSET ?3)",
                    (REFUSED, refused, keep),
                )
            };
            prune(true, KEEP_REFUSED)?;
            prune(false, KEEP)
        })
        .await;
    if let Err(e) = saved {
        tracing::warn!("can't note activity: {e:#}");
    }
}

/// The newest entries first, of one kind or all.
pub async fn list(db: &Db, kind: Option<Kind>, limit: u32) -> Result<Vec<Entry>> {
    Ok(page(db, kind, 0, limit).await?.0)
}

/// `limit` entries after the newest `skip`, newest first, of one kind or all; and how many there
/// are in all.
pub async fn page(db: &Db, kind: Option<Kind>, skip: usize, limit: u32) -> Result<(Vec<Entry>, usize)> {
    let skip = i64::try_from(skip).unwrap_or(i64::MAX);
    db.call(move |c| {
        let kind = kind.map(Kind::as_str);
        let total: i64 =
            c.query_row("SELECT count(*) FROM activity WHERE ?1 IS NULL OR kind = ?1", [kind], |r| r.get(0))?;
        let entries = c
            .prepare(
                "SELECT at, kind, actor, text FROM activity WHERE ?1 IS NULL OR kind = ?1
                 ORDER BY id DESC LIMIT ?2 OFFSET ?3",
            )?
            .query_map((kind, limit, skip), |r| {
                Ok(Entry { at: r.get(0)?, kind: r.get(1)?, actor: r.get(2)?, text: r.get(3)? })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok((entries, usize::try_from(total).unwrap_or_default()))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn keeps_the_newest_by_kind() {
        let db = Db::memory().await.unwrap();
        record(&db, Kind::SignIn, "password", "Signed in").await;
        record(&db, Kind::Change, "password", "Saved the preview settings").await;
        record(&db, Kind::Alert, "alerts", "Previews work again.").await;
        let all = list(&db, None, 10).await.unwrap();
        assert_eq!(all.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(), ["alert", "change", "sign_in"]);
        let changes = list(&db, Some(Kind::Change), 10).await.unwrap();
        assert_eq!((changes.len(), changes[0].text.as_str()), (1, "Saved the preview settings"));
        assert_eq!(list(&db, None, 1).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn refused_sign_ins_push_out_only_each_other() {
        let db = Db::memory().await.unwrap();
        record(&db, Kind::Change, "password", "Saved the cache settings").await;
        for _ in 0..KEEP_REFUSED + 5 {
            record(&db, Kind::SignIn, "password", &format!("{REFUSED} from Firefox, 192.0.2.1")).await;
        }
        let all = page(&db, None, 0, 10_000).await.unwrap().0;
        assert_eq!(all.len(), usize::try_from(KEEP_REFUSED).unwrap() + 1);
        assert_eq!(all.last().unwrap().text, "Saved the cache settings", "the change stays");
    }
}
