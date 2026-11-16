//! SQLite in WAL mode through one connection on the blocking thread pool. The load is a
//! handful of small queries a minute, so one connection is plenty. The file is created with
//! mode 0600, since it holds cookies and login data.

use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::sync::lock;
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;
use serde::de::DeserializeOwned;

pub const FILE_NAME: &str = "server.db";

/// Applied in order; `PRAGMA user_version` records how many have run.
const MIGRATIONS: &[&str] = &[include_str!("../migrations/0001_init.sql")];

#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

impl Db {
    pub async fn open(data_dir: &Path) -> Result<Self> {
        let data_dir = data_dir.to_path_buf();
        tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&data_dir)
                .with_context(|| format!("can't create DATA_DIR {}", data_dir.display()))?;
            let path = data_dir.join(FILE_NAME);
            create_private(&path).with_context(|| format!("can't create {}", path.display()))?;
            let conn = Connection::open(&path).with_context(|| format!("can't open {}", path.display()))?;
            Self::prepare(conn)
        })
        .await?
    }

    /// A private in-memory database for tests.
    #[cfg(test)]
    pub async fn memory() -> Result<Self> {
        Self::prepare(Connection::open_in_memory()?)
    }

    fn prepare(mut conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        migrate(&mut conn).context("database migration failed")?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    /// Runs `f` with the connection on the blocking thread pool.
    pub async fn call<R, F>(&self, f: F) -> Result<R>
    where
        R: Send + 'static,
        F: FnOnce(&mut Connection) -> rusqlite::Result<R> + Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        let result = tokio::task::spawn_blocking(move || {
            let mut conn = lock(&conn);
            f(&mut conn)
        })
        .await?;
        Ok(result?)
    }

    pub async fn get_json<T: DeserializeOwned + Send + 'static>(&self, key: &'static str) -> Result<Option<T>> {
        let value: Option<String> = self
            .call(move |c| c.query_row("SELECT value FROM kv WHERE key = ?1", [key], |r| r.get(0)).optional())
            .await?;
        value.map(|v| serde_json::from_str(&v).with_context(|| format!("stored {key} is unreadable"))).transpose()
    }

    pub async fn set_json<T: Serialize + ?Sized>(&self, key: &'static str, value: &T) -> Result<()> {
        let value = serde_json::to_string(value)?;
        self.call(move |c| {
            c.execute(
                "INSERT INTO kv (key, value, updated_at) VALUES (?1, ?2, unixepoch())
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
                (key, value),
            )
        })
        .await?;
        Ok(())
    }

    pub async fn delete(&self, key: &'static str) -> Result<()> {
        self.call(move |c| c.execute("DELETE FROM kv WHERE key = ?1", [key])).await?;
        Ok(())
    }
}

fn migrate(conn: &mut Connection) -> Result<()> {
    let applied: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    for (index, sql) in MIGRATIONS.iter().enumerate().skip(usize::try_from(applied).unwrap_or(0)) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql).with_context(|| format!("migration {}", index + 1))?;
        tx.pragma_update(None, "user_version", i64::try_from(index + 1)?)?;
        tx.commit()?;
    }
    Ok(())
}

/// Creates the file readable by its owner only; SQLite gives its WAL files the same mode.
fn create_private(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        match std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path) {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            }
            Err(e) => Err(e),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn kv_round_trip() {
        let db = Db::memory().await.unwrap();
        assert_eq!(db.get_json::<Vec<u32>>("k").await.unwrap(), None);
        db.set_json("k", &vec![1, 2]).await.unwrap();
        db.set_json("k", &vec![3]).await.unwrap();
        assert_eq!(db.get_json::<Vec<u32>>("k").await.unwrap(), Some(vec![3]));
        db.delete("k").await.unwrap();
        assert_eq!(db.get_json::<Vec<u32>>("k").await.unwrap(), None);
    }

    #[tokio::test]
    async fn reopening_keeps_data_and_skips_applied_migrations() {
        let dir = tempfile::tempdir().unwrap();
        Db::open(dir.path()).await.unwrap().set_json("k", &1).await.unwrap();
        let db = Db::open(dir.path()).await.unwrap();
        assert_eq!(db.get_json::<u32>("k").await.unwrap(), Some(1));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path()).await.unwrap();
        db.set_json("k", &1).await.unwrap();
        for name in [FILE_NAME, "server.db-wal"] {
            let mode = std::fs::metadata(dir.path().join(name)).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{name}");
        }
    }
}
