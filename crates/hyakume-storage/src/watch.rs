//! Notices commits from other connections, including other processes such as
//! the CLI, so the desktop app can reload local work it didn't write itself.

use std::path::Path;
use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqliteConnection};
use sqlx::{Connection, Row};

use crate::StoreError;

/// Holds one dedicated connection. SQLite's `data_version` only moves when a
/// *different* connection commits, so it has to be the same connection on
/// every check, never one borrowed from a pool.
pub struct ChangeWatcher {
    conn: SqliteConnection,
    last: i64,
}

impl ChangeWatcher {
    /// Opens a read-only connection to an existing database, such as one a
    /// `Store` already created and migrated.
    pub async fn open(path: &Path) -> Result<Self, StoreError> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .read_only(true)
            .busy_timeout(Duration::from_secs(5));
        let mut conn = SqliteConnection::connect_with(&options).await?;
        let last = data_version(&mut conn).await?;
        Ok(Self { conn, last })
    }

    /// True if anyone else committed since the previous call (or since open).
    /// Cheap enough to poll a couple of times a second.
    pub async fn changed(&mut self) -> Result<bool, StoreError> {
        let now = data_version(&mut self.conn).await?;
        let changed = now != self.last;
        self.last = now;
        Ok(changed)
    }
}

async fn data_version(conn: &mut SqliteConnection) -> Result<i64, StoreError> {
    let row = sqlx::query("PRAGMA data_version").fetch_one(conn).await?;
    Ok(row.try_get(0)?)
}
