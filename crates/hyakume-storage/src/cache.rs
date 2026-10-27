//! `cache.db`: copies of shared workspaces as a client last loaded them,
//! plus the comments of tasks it opened.
//! It's a separate file from local.db, so personal work and other teams'
//! copies never share a table, and deleting it loses nothing but the copies.

use std::path::Path;

use sqlx::SqlitePool;
use sqlx::migrate::Migrator;
use time::OffsetDateTime;

use crate::StoreError;
use crate::convert::{from_millis, now_ms};

static MIGRATOR: Migrator = sqlx::migrate!("migrations/cache");

#[derive(Clone, Debug)]
pub struct Cache {
    pool: SqlitePool,
}

impl Cache {
    pub async fn open(path: &Path) -> Result<Self, StoreError> {
        let pool = crate::pool(path, 2).await?;
        MIGRATOR.run(&pool).await?;
        Ok(Self { pool })
    }

    /// Replaces everything kept for `source` with `workspaces`, as
    /// (workspace id, body) pairs, and drops the comments of any task not
    /// in `tasks`. Lost access or a deleted task leaves no copy behind.
    pub async fn replace(
        &self,
        source: &str,
        workspaces: &[(String, String)],
        tasks: &[String],
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("DELETE FROM workspaces WHERE source = ?")
            .bind(source)
            .execute(&mut *tx)
            .await?;
        // One JSON array keeps this a single statement however many tasks.
        let kept = serde_json::to_string(tasks)
            .map_err(|e| StoreError::invalid(format!("task ids: {e}")))?;
        sqlx::query(
            "DELETE FROM comments WHERE source = ? \
             AND task_id NOT IN (SELECT value FROM json_each(?))",
        )
        .bind(source)
        .bind(kept)
        .execute(&mut *tx)
        .await?;
        let now = now_ms();
        for (id, body) in workspaces {
            sqlx::query(
                "INSERT INTO workspaces (source, workspace_id, body, saved_at) VALUES (?, ?, ?, ?)",
            )
            .bind(source)
            .bind(id)
            .bind(body)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// The bodies kept for `source`, in workspace id order.
    pub async fn load(&self, source: &str) -> Result<Vec<String>, StoreError> {
        Ok(
            sqlx::query_scalar(
                "SELECT body FROM workspaces WHERE source = ? ORDER BY workspace_id",
            )
            .bind(source)
            .fetch_all(&self.pool)
            .await?,
        )
    }
}

impl Cache {
    /// Keeps `body`, one task's comments as the client wrote them.
    pub async fn save_comments(
        &self,
        source: &str,
        task: &str,
        body: &str,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO comments (source, task_id, body, saved_at) VALUES (?, ?, ?, ?) \
             ON CONFLICT (source, task_id) DO UPDATE SET \
             body = excluded.body, saved_at = excluded.saved_at",
        )
        .bind(source)
        .bind(task)
        .bind(body)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The comments kept for `task`, and when they were saved.
    pub async fn comments(
        &self,
        source: &str,
        task: &str,
    ) -> Result<Option<(String, OffsetDateTime)>, StoreError> {
        let row: Option<(String, i64)> =
            sqlx::query_as("SELECT body, saved_at FROM comments WHERE source = ? AND task_id = ?")
                .bind(source)
                .bind(task)
                .fetch_optional(&self.pool)
                .await?;
        row.map(|(body, at)| Ok((body, from_millis(at)?)))
            .transpose()
    }
}
