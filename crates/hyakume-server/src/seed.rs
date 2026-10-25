//! Creates the synthetic identities and workspaces listed in `[dev]`.
//! Runs at every dev-mode start and only adds what is missing, so changes
//! made while running (custom roles, tasks) survive restarts.

use anyhow::{Context, bail};
use hyakume_core::model::ProjectKey;
use hyakume_core::permission::MembershipKind;
use hyakume_storage::{Store, StoreError};

use crate::config::DevConfig;

pub async fn apply(store: &Store, dev: &DevConfig) -> anyhow::Result<()> {
    for identity in &dev.identities {
        match store.identity_by_name(&identity.name).await {
            Ok(_) => {}
            Err(StoreError::NotFound(_)) => {
                let display = identity.display_name.as_deref().unwrap_or(&identity.name);
                store.create_identity(&identity.name, display).await?;
            }
            Err(err) => return Err(err.into()),
        }
    }

    for ws in &dev.workspaces {
        let workspace = match store.workspace_by_slug(&ws.slug).await {
            Ok(existing) => existing,
            Err(StoreError::NotFound(_)) => {
                let owner = identity(store, &ws.owner).await?;
                store.create_workspace(&ws.slug, &ws.name, owner).await?
            }
            Err(err) => return Err(err.into()),
        };

        for project in &ws.projects {
            let key: ProjectKey = project.key.parse()?;
            match store.project_by_key(workspace.id, &key).await {
                Ok(_) => {}
                Err(StoreError::NotFound(_)) => {
                    store
                        .create_project(workspace.id, &key, &project.name)
                        .await?;
                }
                Err(err) => return Err(err.into()),
            }
        }

        for member in &ws.members {
            let identity_id = identity(store, &member.identity).await?;
            if store.access(workspace.id, identity_id).await?.is_some() {
                continue;
            }
            let mut roles = Vec::new();
            for name in &member.roles {
                roles.push(role(store, workspace.id, name).await?);
            }
            let mut projects = Vec::new();
            for key in &member.guest_projects {
                projects.push(store.project_by_key(workspace.id, &key.parse()?).await?.id);
            }
            let kind = if projects.is_empty() {
                MembershipKind::Member
            } else {
                MembershipKind::Guest
            };
            store
                .set_membership(workspace.id, identity_id, kind, false, &roles, &projects)
                .await?;
        }
    }
    Ok(())
}

async fn identity(store: &Store, name: &str) -> anyhow::Result<hyakume_core::ids::IdentityId> {
    Ok(store
        .identity_by_name(name)
        .await
        .with_context(|| format!("dev identity {name:?} must be listed in [[dev.identities]]"))?
        .id)
}

async fn role(
    store: &Store,
    workspace: hyakume_core::ids::WorkspaceId,
    name: &str,
) -> anyhow::Result<hyakume_core::ids::RoleId> {
    match store
        .roles(workspace)
        .await?
        .into_iter()
        .find(|r| r.name == name)
    {
        Some(role) => Ok(role.id),
        None => bail!("role {name:?} does not exist"),
    }
}
