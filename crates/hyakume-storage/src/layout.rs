//! Personal canvas positions, one set per identity and workspace.

use hyakume_core::api::{Layout, LayoutNode, NodeKind};
use hyakume_core::ids::{IdentityId, WorkspaceId};

use sqlx::SqliteConnection;

use crate::convert::{id, now_ms};
use crate::{Store, StoreError};

impl Store {
    pub async fn layout(
        &self,
        identity: IdentityId,
        workspace: WorkspaceId,
    ) -> Result<Layout, StoreError> {
        let nodes = fetch_layout(&mut *self.pool.acquire().await?, identity, workspace).await?;
        Ok(Layout { nodes })
    }

    /// Upserts positions for the given nodes; other saved nodes stay as they
    /// are. Every node must be a project or task in `workspace`.
    pub async fn put_layout(
        &self,
        identity: IdentityId,
        workspace: WorkspaceId,
        nodes: &[LayoutNode],
    ) -> Result<(), StoreError> {
        let mut tx = self.write().await?;
        let now = now_ms();
        for node in nodes {
            if !(node.x.is_finite() && node.y.is_finite()) {
                return Err(StoreError::invalid(
                    "layout positions must be finite numbers",
                ));
            }
            let in_workspace: bool = match node.kind {
                NodeKind::Project => sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM projects WHERE id = ? AND workspace_id = ?)",
                ),
                NodeKind::Task => sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM tasks t JOIN projects p ON p.id = t.project_id \
                     WHERE t.id = ? AND p.workspace_id = ?)",
                ),
            }
            .bind(node.id.to_string())
            .bind(workspace.to_string())
            .fetch_one(&mut *tx)
            .await?;
            if !in_workspace {
                return Err(StoreError::NotFound("layout node"));
            }
            sqlx::query(
                "INSERT INTO layouts (identity_id, workspace_id, node_kind, node_id, x, y, pinned, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT (identity_id, node_kind, node_id) DO UPDATE SET \
                 x = excluded.x, y = excluded.y, pinned = excluded.pinned, updated_at = excluded.updated_at",
            )
            .bind(identity.to_string())
            .bind(workspace.to_string())
            .bind(node.kind.as_str())
            .bind(node.id.to_string())
            .bind(node.x)
            .bind(node.y)
            .bind(node.pinned)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

pub(crate) async fn fetch_layout(
    conn: &mut SqliteConnection,
    identity: IdentityId,
    workspace: WorkspaceId,
) -> Result<Vec<LayoutNode>, StoreError> {
    let rows: Vec<(String, String, f64, f64, bool)> = sqlx::query_as(
        "SELECT node_kind, node_id, x, y, pinned FROM layouts \
         WHERE identity_id = ? AND workspace_id = ? ORDER BY node_kind, node_id",
    )
    .bind(identity.to_string())
    .bind(workspace.to_string())
    .fetch_all(conn)
    .await?;
    rows.into_iter()
        .map(|(kind, node_id, x, y, pinned)| {
            Ok(LayoutNode {
                kind: NodeKind::parse(&kind)
                    .ok_or_else(|| StoreError::Corrupt(format!("node kind {kind:?}")))?,
                id: id(&node_id)?,
                x,
                y,
                pinned,
            })
        })
        .collect()
}
