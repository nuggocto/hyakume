//! SQLite storage for local and server databases. Both use the same schema and
//! the same queries; permission checks happen in the caller, not here.

mod access;
mod cache;
mod convert;
mod layout;
mod watch;
mod work;

use std::path::Path;
use std::time::Duration;

use hyakume_core::model::{Task, ValidationError};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{SqlitePool, migrate::Migrator};

pub use cache::Cache;
pub use watch::ChangeWatcher;

static MIGRATOR: Migrator = sqlx::migrate!("migrations/work");

/// A local or server work database. Cheap to clone; clones share one pool.
#[derive(Clone, Debug)]
pub struct Store {
    pool: SqlitePool,
}

impl Store {
    /// Opens or creates the database at `path` and applies pending migrations.
    /// A failed migration returns an error, so callers never run against an
    /// older or partially upgraded schema.
    pub async fn open(path: &Path) -> Result<Self, StoreError> {
        let pool = pool(path, 8).await?;
        MIGRATOR.run(&pool).await?;
        Ok(Self { pool })
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }

    /// Starts a write transaction that takes the write lock immediately.
    /// A deferred transaction that reads first can fail with SQLITE_BUSY when
    /// it later tries to write; IMMEDIATE waits on busy_timeout instead.
    async fn write(&self) -> Result<sqlx::Transaction<'static, sqlx::Sqlite>, StoreError> {
        Ok(self.pool.begin_with("BEGIN IMMEDIATE").await?)
    }
}

/// One pool per database file, set up the same way for every kind.
async fn pool(path: &Path, connections: u32) -> Result<SqlitePool, StoreError> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        // NORMAL is durable across application crashes in WAL mode; only
        // an OS crash or power loss can drop the last commits.
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(5));
    Ok(SqlitePoolOptions::new()
        .max_connections(connections)
        .connect_with(options)
        .await?)
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{0} not found")]
    NotFound(&'static str),
    #[error("the task changed since version {expected}; latest is version {}", latest.version)]
    Conflict { expected: i64, latest: Box<Task> },
    #[error("{0}")]
    Duplicate(String),
    #[error(transparent)]
    Invalid(#[from] ValidationError),
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("migration failed: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),
    #[error("stored data is corrupt: {0}")]
    Corrupt(String),
}

impl StoreError {
    fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(ValidationError::Other(message.into()))
    }
}

/// Maps a UNIQUE constraint failure to `Duplicate`, leaving other errors alone.
fn unique_violation(err: sqlx::Error, message: impl FnOnce() -> String) -> StoreError {
    match &err {
        sqlx::Error::Database(db) if db.is_unique_violation() => StoreError::Duplicate(message()),
        _ => StoreError::Database(err),
    }
}
