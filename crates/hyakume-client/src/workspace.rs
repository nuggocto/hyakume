//! One workspace, local or remote, behind the same operations.

use hyakume_core::api::{
    CreateProject, CreateRelation, CreateRelationType, CreateRole, CreateTask, Layout,
    ProjectSnapshot, Role, SetMemberRoles, TaskDetail, UpdateTask, WorkspaceSummary,
};
use hyakume_core::ids::{IdentityId, ProjectId, RelationId, TaskId};
use hyakume_core::model::{
    Comment, Identity, Project, Relation, RelationType, Task, TaskRef, Workspace,
};
use hyakume_core::permission::Access;
use hyakume_storage::Store;

use crate::ClientError;
use crate::remote::Remote;

pub struct OpenWorkspace {
    /// `local` or `SERVER/SLUG`.
    pub selector: String,
    pub workspace: Workspace,
    backend: Backend,
}

enum Backend {
    /// Local work has one person, who may do everything.
    Local {
        store: Store,
        me: Identity,
    },
    Remote(Remote),
}

impl OpenWorkspace {
    pub(crate) fn local(workspace: Workspace, store: Store, me: Identity) -> Self {
        Self {
            selector: crate::LOCAL.into(),
            workspace,
            backend: Backend::Local { store, me },
        }
    }

    pub(crate) fn remote(selector: String, workspace: Workspace, remote: Remote) -> Self {
        Self {
            selector,
            workspace,
            backend: Backend::Remote(remote),
        }
    }

    pub fn is_local(&self) -> bool {
        matches!(self.backend, Backend::Local { .. })
    }

    pub fn remote_client(&self) -> Option<&Remote> {
        match &self.backend {
            Backend::Remote(remote) => Some(remote),
            Backend::Local { .. } => None,
        }
    }

    pub async fn me(&self) -> Result<Identity, ClientError> {
        match &self.backend {
            Backend::Local { me, .. } => Ok(me.clone()),
            Backend::Remote(remote) => remote.me().await,
        }
    }

    pub async fn summary(&self) -> Result<WorkspaceSummary, ClientError> {
        let id = self.workspace.id;
        match &self.backend {
            Backend::Local { store, .. } => Ok(WorkspaceSummary {
                workspace: self.workspace.clone(),
                access: Access::local_owner(),
                projects: store.projects(id).await?,
                relation_types: store.relation_types(id).await?,
                roles: store.roles(id).await?,
                members: store.members(id).await?,
            }),
            Backend::Remote(remote) => remote.summary(id).await,
        }
    }

    pub async fn snapshot(&self, project: ProjectId) -> Result<ProjectSnapshot, ClientError> {
        match &self.backend {
            Backend::Local { store, .. } => Ok(store.project_snapshot(project, |_| true).await?),
            Backend::Remote(remote) => remote.snapshot(project).await,
        }
    }

    pub async fn task_by_ref(&self, reference: &TaskRef) -> Result<TaskDetail, ClientError> {
        match &self.backend {
            Backend::Local { store, .. } => {
                let task = store.task_by_ref(self.workspace.id, reference).await?;
                let comments = store.comments(task.id).await?;
                Ok(TaskDetail { task, comments })
            }
            Backend::Remote(remote) => remote.task_by_ref(self.workspace.id, reference).await,
        }
    }

    pub async fn create_project(&self, body: CreateProject) -> Result<Project, ClientError> {
        match &self.backend {
            Backend::Local { store, .. } => Ok(store
                .create_project(self.workspace.id, &body.key, &body.name)
                .await?),
            Backend::Remote(remote) => remote.create_project(self.workspace.id, &body).await,
        }
    }

    pub async fn create_task(
        &self,
        project: ProjectId,
        body: CreateTask,
    ) -> Result<Task, ClientError> {
        match &self.backend {
            Backend::Local { store, me } => {
                Ok(store.create_task(project, body, Some(me.id)).await?)
            }
            Backend::Remote(remote) => remote.create_task(project, &body).await,
        }
    }

    /// Fails with `ClientError::Conflict` carrying the latest task when
    /// `body.expected_version` is stale.
    pub async fn update_task(&self, task: TaskId, body: UpdateTask) -> Result<Task, ClientError> {
        match &self.backend {
            Backend::Local { store, me } => Ok(store.update_task(task, body, Some(me.id)).await?),
            Backend::Remote(remote) => remote.update_task(task, &body).await,
        }
    }

    pub async fn delete_task(&self, task: TaskId) -> Result<(), ClientError> {
        match &self.backend {
            Backend::Local { store, .. } => {
                store.delete_task(task).await.map(drop).map_err(Into::into)
            }
            Backend::Remote(remote) => remote.delete_task(task).await,
        }
    }

    pub async fn add_comment(&self, task: TaskId, body: &str) -> Result<Comment, ClientError> {
        match &self.backend {
            Backend::Local { store, me } => Ok(store.add_comment(task, Some(me.id), body).await?),
            Backend::Remote(remote) => remote.add_comment(task, body).await,
        }
    }

    pub async fn create_relation(&self, body: CreateRelation) -> Result<Relation, ClientError> {
        match &self.backend {
            Backend::Local { store, me } => Ok(store
                .create_relation(self.workspace.id, body, Some(me.id))
                .await?),
            Backend::Remote(remote) => remote.create_relation(self.workspace.id, &body).await,
        }
    }

    pub async fn delete_relation(&self, relation: RelationId) -> Result<(), ClientError> {
        match &self.backend {
            Backend::Local { store, .. } => Ok(store.delete_relation(relation).await?),
            Backend::Remote(remote) => remote.delete_relation(relation).await,
        }
    }

    pub async fn create_relation_type(
        &self,
        body: CreateRelationType,
    ) -> Result<RelationType, ClientError> {
        match &self.backend {
            Backend::Local { store, .. } => {
                Ok(store.create_relation_type(self.workspace.id, body).await?)
            }
            Backend::Remote(remote) => remote.create_relation_type(self.workspace.id, &body).await,
        }
    }

    pub async fn create_role(&self, body: CreateRole) -> Result<Role, ClientError> {
        match &self.backend {
            Backend::Local { store, .. } => Ok(store.create_role(self.workspace.id, body).await?),
            Backend::Remote(remote) => remote.create_role(self.workspace.id, &body).await,
        }
    }

    pub async fn set_member_roles(
        &self,
        identity: IdentityId,
        body: SetMemberRoles,
    ) -> Result<(), ClientError> {
        match &self.backend {
            Backend::Local { store, .. } => Ok(store
                .set_member_roles(self.workspace.id, identity, &body.roles)
                .await?),
            Backend::Remote(remote) => {
                remote
                    .set_member_roles(self.workspace.id, identity, &body)
                    .await
            }
        }
    }

    pub async fn layout(&self) -> Result<Layout, ClientError> {
        match &self.backend {
            Backend::Local { store, me } => Ok(store.layout(me.id, self.workspace.id).await?),
            Backend::Remote(remote) => remote.layout(self.workspace.id).await,
        }
    }

    pub async fn put_layout(&self, layout: &Layout) -> Result<(), ClientError> {
        match &self.backend {
            Backend::Local { store, me } => Ok(store
                .put_layout(me.id, self.workspace.id, &layout.nodes)
                .await?),
            Backend::Remote(remote) => remote.put_layout(self.workspace.id, layout).await,
        }
    }
}
