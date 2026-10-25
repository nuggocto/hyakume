//! Identities, sessions, memberships, and roles.

use std::collections::BTreeSet;

use hyakume_core::api::{CreateRole, Member, Role};
use hyakume_core::ids::{IdentityId, ProjectId, RoleId, WorkspaceId};
use hyakume_core::model::{Identity, name_key};
use hyakume_core::permission::{Access, MembershipKind, Permission, Preset};

use crate::convert::{id, now_ms};
use crate::{Store, StoreError, unique_violation};

#[derive(sqlx::FromRow)]
struct IdentityRow {
    id: String,
    name: String,
    display_name: String,
}

impl TryFrom<IdentityRow> for Identity {
    type Error = StoreError;

    fn try_from(r: IdentityRow) -> Result<Self, StoreError> {
        Ok(Self {
            id: id(&r.id)?,
            name: r.name,
            display_name: r.display_name,
        })
    }
}

impl Store {
    pub async fn create_identity(
        &self,
        name: &str,
        display_name: &str,
    ) -> Result<Identity, StoreError> {
        let identity = Identity {
            id: IdentityId::new(),
            name: name.trim().to_owned(),
            display_name: display_name.trim().to_owned(),
        };
        if identity.name.is_empty() {
            return Err(StoreError::invalid("identity name must not be empty"));
        }
        sqlx::query(
            "INSERT INTO identities (id, name, display_name, created_at) VALUES (?, ?, ?, ?)",
        )
        .bind(identity.id.to_string())
        .bind(&identity.name)
        .bind(&identity.display_name)
        .bind(now_ms())
        .execute(&self.pool)
        .await
        .map_err(|e| unique_violation(e, || format!("identity {name:?} already exists")))?;
        Ok(identity)
    }

    pub async fn identity(&self, id: IdentityId) -> Result<Identity, StoreError> {
        sqlx::query_as::<_, IdentityRow>(
            "SELECT id, name, display_name FROM identities WHERE id = ?",
        )
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(StoreError::NotFound("identity"))?
        .try_into()
    }

    pub async fn identity_by_name(&self, name: &str) -> Result<Identity, StoreError> {
        sqlx::query_as::<_, IdentityRow>(
            "SELECT id, name, display_name FROM identities WHERE name = ?",
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(StoreError::NotFound("identity"))?
        .try_into()
    }

    /// Stores a session under the SHA-256 of its token.
    pub async fn create_session(
        &self,
        identity: IdentityId,
        token_hash: &str,
    ) -> Result<(), StoreError> {
        let now = now_ms();
        sqlx::query(
            "INSERT INTO sessions (id, identity_id, token_hash, created_at, last_used_at) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(uuid::Uuid::now_v7().to_string())
        .bind(identity.to_string())
        .bind(token_hash)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Returns the session's identity and marks the session used. A session
    /// idle for longer than `max_idle_ms` is deleted and treated as unknown.
    pub async fn authenticate(
        &self,
        token_hash: &str,
        max_idle_ms: i64,
    ) -> Result<Option<Identity>, StoreError> {
        let now = now_ms();
        sqlx::query("DELETE FROM sessions WHERE token_hash = ? AND last_used_at < ?")
            .bind(token_hash)
            .bind(now - max_idle_ms)
            .execute(&self.pool)
            .await?;
        let identity: Option<String> = sqlx::query_scalar(
            "UPDATE sessions SET last_used_at = ? WHERE token_hash = ? RETURNING identity_id",
        )
        .bind(now)
        .bind(token_hash)
        .fetch_optional(&self.pool)
        .await?;
        match identity {
            Some(identity) => Ok(Some(self.identity(id(&identity)?).await?)),
            None => Ok(None),
        }
    }

    /// Adds or replaces a membership together with its roles and guest projects.
    pub async fn set_membership(
        &self,
        workspace: WorkspaceId,
        identity: IdentityId,
        kind: MembershipKind,
        is_owner: bool,
        roles: &[RoleId],
        guest_projects: &[ProjectId],
    ) -> Result<(), StoreError> {
        let mut tx = self.write().await?;
        sqlx::query(
            "INSERT INTO memberships (workspace_id, identity_id, kind, is_owner) VALUES (?, ?, ?, ?) \
             ON CONFLICT (workspace_id, identity_id) DO UPDATE SET kind = excluded.kind, \
             is_owner = excluded.is_owner",
        )
        .bind(workspace.to_string())
        .bind(identity.to_string())
        .bind(kind.as_str())
        .bind(is_owner)
        .execute(&mut *tx)
        .await
        .map_err(|e| unique_violation(e, || "the workspace already has an owner".into()))?;
        for sql in [
            "DELETE FROM membership_roles WHERE workspace_id = ? AND identity_id = ?",
            "DELETE FROM guest_projects WHERE workspace_id = ? AND identity_id = ?",
        ] {
            sqlx::query(sql)
                .bind(workspace.to_string())
                .bind(identity.to_string())
                .execute(&mut *tx)
                .await?;
        }
        insert_member_roles(&mut tx, workspace, identity, roles).await?;
        for project in guest_projects {
            let inserted = sqlx::query(
                "INSERT INTO guest_projects (workspace_id, identity_id, project_id) \
                 SELECT ?, ?, id FROM projects WHERE id = ? AND workspace_id = ?",
            )
            .bind(workspace.to_string())
            .bind(identity.to_string())
            .bind(project.to_string())
            .bind(workspace.to_string())
            .execute(&mut *tx)
            .await?;
            if inserted.rows_affected() == 0 {
                return Err(StoreError::NotFound("project"));
            }
        }
        tx.commit().await?;
        Ok(())
    }

    /// Replaces the roles of an existing member.
    pub async fn set_member_roles(
        &self,
        workspace: WorkspaceId,
        identity: IdentityId,
        roles: &[RoleId],
    ) -> Result<(), StoreError> {
        let mut tx = self.write().await?;
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM memberships WHERE workspace_id = ? AND identity_id = ?)",
        )
        .bind(workspace.to_string())
        .bind(identity.to_string())
        .fetch_one(&mut *tx)
        .await?;
        if !exists {
            return Err(StoreError::NotFound("member"));
        }
        sqlx::query("DELETE FROM membership_roles WHERE workspace_id = ? AND identity_id = ?")
            .bind(workspace.to_string())
            .bind(identity.to_string())
            .execute(&mut *tx)
            .await?;
        insert_member_roles(&mut tx, workspace, identity, roles).await?;
        tx.commit().await?;
        Ok(())
    }

    /// The identity's effective access in the workspace, or `None` if it is
    /// not a member or guest there.
    pub async fn access(
        &self,
        workspace: WorkspaceId,
        identity: IdentityId,
    ) -> Result<Option<Access>, StoreError> {
        let membership: Option<(String, bool)> = sqlx::query_as(
            "SELECT kind, is_owner FROM memberships WHERE workspace_id = ? AND identity_id = ?",
        )
        .bind(workspace.to_string())
        .bind(identity.to_string())
        .fetch_optional(&self.pool)
        .await?;
        let Some((kind, is_owner)) = membership else {
            return Ok(None);
        };
        let kind = MembershipKind::parse(&kind)
            .ok_or_else(|| StoreError::Corrupt(format!("membership kind {kind:?}")))?;

        // Permissions from several roles add up; there are no deny rules.
        let permissions: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT rp.permission FROM membership_roles mr \
             JOIN role_permissions rp ON rp.role_id = mr.role_id \
             WHERE mr.workspace_id = ? AND mr.identity_id = ?",
        )
        .bind(workspace.to_string())
        .bind(identity.to_string())
        .fetch_all(&self.pool)
        .await?;
        let guest_projects: Vec<String> = sqlx::query_scalar(
            "SELECT project_id FROM guest_projects WHERE workspace_id = ? AND identity_id = ?",
        )
        .bind(workspace.to_string())
        .bind(identity.to_string())
        .fetch_all(&self.pool)
        .await?;

        Ok(Some(Access {
            kind,
            is_owner,
            permissions: permissions
                .iter()
                .map(|p| p.parse())
                .collect::<Result<BTreeSet<Permission>, _>>()
                .map_err(|e| StoreError::Corrupt(e.to_string()))?,
            guest_projects: guest_projects
                .iter()
                .map(|p| id(p))
                .collect::<Result<_, _>>()?,
        }))
    }

    pub async fn roles(&self, workspace: WorkspaceId) -> Result<Vec<Role>, StoreError> {
        let rows: Vec<(String, String, Option<String>)> = sqlx::query_as(
            "SELECT id, name, preset FROM roles WHERE workspace_id = ? \
             ORDER BY preset IS NULL, rowid",
        )
        .bind(workspace.to_string())
        .fetch_all(&self.pool)
        .await?;
        let mut roles = Vec::with_capacity(rows.len());
        for (role_id, name, preset) in rows {
            let permissions: Vec<String> = sqlx::query_scalar(
                "SELECT permission FROM role_permissions WHERE role_id = ? ORDER BY permission",
            )
            .bind(&role_id)
            .fetch_all(&self.pool)
            .await?;
            roles.push(Role {
                id: id(&role_id)?,
                name,
                preset: preset.as_deref().and_then(Preset::parse),
                permissions: permissions
                    .iter()
                    .map(|p| p.parse())
                    .collect::<Result<_, _>>()
                    .map_err(|e: hyakume_core::model::ValidationError| {
                        StoreError::Corrupt(e.to_string())
                    })?,
            });
        }
        Ok(roles)
    }

    pub async fn create_role(
        &self,
        workspace: WorkspaceId,
        input: CreateRole,
    ) -> Result<Role, StoreError> {
        let name = input.name.trim().to_owned();
        if name.is_empty() {
            return Err(StoreError::invalid("role name must not be empty"));
        }
        let permissions: BTreeSet<Permission> = input.permissions.into_iter().collect();
        let role = Role {
            id: RoleId::new(),
            name,
            preset: None,
            permissions: permissions.into_iter().collect(),
        };
        let mut tx = self.write().await?;
        // Assignment looks roles up the way people type them, so a custom
        // "admin" next to the preset "Admin" would hand out the preset.
        let existing: Vec<String> =
            sqlx::query_scalar("SELECT name FROM roles WHERE workspace_id = ?")
                .bind(workspace.to_string())
                .fetch_all(&mut *tx)
                .await?;
        if let Some(taken) = existing
            .iter()
            .find(|n| name_key(n) == name_key(&role.name))
        {
            return Err(StoreError::Duplicate(format!(
                "role {taken:?} already exists"
            )));
        }
        sqlx::query("INSERT INTO roles (id, workspace_id, name) VALUES (?, ?, ?)")
            .bind(role.id.to_string())
            .bind(workspace.to_string())
            .bind(&role.name)
            .execute(&mut *tx)
            .await
            .map_err(|e| unique_violation(e, || format!("role {:?} already exists", role.name)))?;
        for permission in &role.permissions {
            sqlx::query("INSERT INTO role_permissions (role_id, permission) VALUES (?, ?)")
                .bind(role.id.to_string())
                .bind(permission.as_str())
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(role)
    }

    pub async fn members(&self, workspace: WorkspaceId) -> Result<Vec<Member>, StoreError> {
        let rows: Vec<(String, String, String, String, bool)> = sqlx::query_as(
            "SELECT i.id, i.name, i.display_name, m.kind, m.is_owner FROM memberships m \
             JOIN identities i ON i.id = m.identity_id WHERE m.workspace_id = ? ORDER BY i.name",
        )
        .bind(workspace.to_string())
        .fetch_all(&self.pool)
        .await?;
        let mut members = Vec::with_capacity(rows.len());
        for (identity_id, name, display_name, kind, is_owner) in rows {
            let roles: Vec<String> = sqlx::query_scalar(
                "SELECT role_id FROM membership_roles WHERE workspace_id = ? AND identity_id = ?",
            )
            .bind(workspace.to_string())
            .bind(&identity_id)
            .fetch_all(&self.pool)
            .await?;
            members.push(Member {
                identity: Identity {
                    id: id(&identity_id)?,
                    name,
                    display_name,
                },
                kind: MembershipKind::parse(&kind)
                    .ok_or_else(|| StoreError::Corrupt(format!("membership kind {kind:?}")))?,
                is_owner,
                roles: roles.iter().map(|r| id(r)).collect::<Result<_, _>>()?,
            });
        }
        Ok(members)
    }
}

async fn insert_member_roles(
    tx: &mut sqlx::Transaction<'static, sqlx::Sqlite>,
    workspace: WorkspaceId,
    identity: IdentityId,
    roles: &[RoleId],
) -> Result<(), StoreError> {
    for role in roles {
        // The SELECT guards against assigning a role from another workspace.
        let inserted = sqlx::query(
            "INSERT OR IGNORE INTO membership_roles (workspace_id, identity_id, role_id) \
             SELECT ?, ?, id FROM roles WHERE id = ? AND workspace_id = ?",
        )
        .bind(workspace.to_string())
        .bind(identity.to_string())
        .bind(role.to_string())
        .bind(workspace.to_string())
        .execute(&mut **tx)
        .await?;
        if inserted.rows_affected() == 0 {
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM roles WHERE id = ? AND workspace_id = ?)",
            )
            .bind(role.to_string())
            .bind(workspace.to_string())
            .fetch_one(&mut **tx)
            .await?;
            if !exists {
                return Err(StoreError::NotFound("role"));
            }
        }
    }
    Ok(())
}
