//! `/v1` HTTP handlers. Each one authenticates, checks access, runs one store
//! operation, and notifies subscribers after the commit.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use hyakume_core::api::{
    API_VERSION, CreateComment, CreateProject, CreateRelation, CreateRelationType, CreateRole,
    CreateTask, DevLoginRequest, Event, Layout, LoginResponse, NodeKind, ProjectSnapshot, Role,
    ServerInfo, SetMemberRoles, TaskDetail, UpdateTask, WorkspaceListing, WorkspaceSummary,
};
use hyakume_core::ids::{IdentityId, ProjectId, RelationId, TaskId, WorkspaceId};
use hyakume_core::model::{Comment, Identity, Project, Relation, RelationType, Task, TaskRef};
use hyakume_core::permission::{Access, Permission};

use crate::auth::{Caller, hash_token, new_token};
use crate::error::Failure;
use crate::{AppState, Notice, events};

pub fn v1() -> Router<AppState> {
    Router::new()
        .route("/v1/info", get(info))
        .route("/v1/dev/login", post(dev_login))
        .route("/v1/me", get(me))
        .route("/v1/events", get(events::subscribe))
        .route("/v1/workspaces", get(list_workspaces))
        .route("/v1/workspaces/{workspace}", get(workspace_summary))
        .route("/v1/workspaces/{workspace}/projects", post(create_project))
        .route(
            "/v1/workspaces/{workspace}/tasks/{reference}",
            get(task_by_reference),
        )
        .route(
            "/v1/workspaces/{workspace}/relations",
            post(create_relation),
        )
        .route(
            "/v1/workspaces/{workspace}/relation-types",
            post(create_relation_type),
        )
        .route("/v1/workspaces/{workspace}/roles", post(create_role))
        .route(
            "/v1/workspaces/{workspace}/members/{identity}/roles",
            put(set_member_roles),
        )
        .route(
            "/v1/workspaces/{workspace}/layout",
            get(get_layout).put(put_layout),
        )
        .route("/v1/projects/{project}", get(project_snapshot))
        .route("/v1/projects/{project}/tasks", post(create_task))
        .route(
            "/v1/tasks/{task}",
            get(task_detail).put(update_task).delete(delete_task),
        )
        .route("/v1/tasks/{task}/comments", post(add_comment))
        .route("/v1/relations/{relation}", delete(delete_relation))
}

type Reply<T> = Result<Json<T>, Failure>;

async fn info(State(app): State<AppState>) -> Json<ServerInfo> {
    Json(ServerInfo {
        api_version: API_VERSION,
        dev_mode: app.dev_mode(),
    })
}

/// Development-only sign-in: anyone who can reach the server can act as any
/// configured test identity. Disabled unless `dev_mode` is on.
async fn dev_login(
    State(app): State<AppState>,
    Json(body): Json<DevLoginRequest>,
) -> Result<(StatusCode, Json<LoginResponse>), Failure> {
    if !app.dev_mode() {
        return Err(Failure::not_found("route"));
    }
    if !app.dev_identities.contains(&body.identity) {
        return Err(Failure::invalid(format!(
            "{:?} is not a configured dev identity",
            body.identity
        )));
    }
    let identity = app.store.identity_by_name(&body.identity).await?;
    let token = new_token().map_err(|err| {
        tracing::error!(%err, "no randomness for a session token");
        Failure::new(
            hyakume_core::api::ErrorCode::Internal,
            "cannot create a session",
        )
    })?;
    app.store
        .create_session(identity.id, &hash_token(&token))
        .await?;
    tracing::info!(identity = %identity.name, "dev login");
    Ok((StatusCode::CREATED, Json(LoginResponse { token, identity })))
}

async fn me(Caller(me): Caller) -> Json<Identity> {
    Json(me)
}

/// The caller's access, or 404 if they are not in the workspace at all.
async fn membership(
    app: &AppState,
    workspace: WorkspaceId,
    caller: &Identity,
) -> Result<Access, Failure> {
    app.store
        .access(workspace, caller.id)
        .await?
        .ok_or_else(|| Failure::not_found("workspace"))
}

/// 404 when the caller cannot see the project, 403 when they can see it but
/// lack `permission`.
fn require(
    access: &Access,
    permission: Permission,
    project: Option<ProjectId>,
    action: &str,
) -> Result<(), Failure> {
    if let Some(project) = project
        && !access.can_view_project(project)
    {
        return Err(Failure::not_found("project"));
    }
    if access.allows(permission, project) {
        Ok(())
    } else {
        Err(Failure::forbidden(action))
    }
}

/// Loads a project and the caller's access to its workspace.
async fn project_access(
    app: &AppState,
    project: ProjectId,
    caller: &Identity,
) -> Result<(Project, Access), Failure> {
    let project = app.store.project(project).await?;
    let access = membership(app, project.workspace_id, caller).await?;
    if !access.can_view_project(project.id) {
        return Err(Failure::not_found("project"));
    }
    Ok((project, access))
}

async fn task_access(
    app: &AppState,
    task: TaskId,
    caller: &Identity,
) -> Result<(Task, Project, Access), Failure> {
    let task = app.store.task(task).await?;
    let (project, access) = project_access(app, task.project_id, caller)
        .await
        .map_err(|_| Failure::not_found("task"))?;
    Ok((task, project, access))
}

fn project_changed(app: &AppState, project: &Project) {
    app.notify(Notice {
        workspace: project.workspace_id,
        project: Some(project.id),
        only: None,
        event: Event::ProjectChanged {
            workspace_id: project.workspace_id,
            project_id: project.id,
        },
    });
}

fn workspace_changed(app: &AppState, workspace: WorkspaceId) {
    app.notify(Notice {
        workspace,
        project: None,
        only: None,
        event: Event::WorkspaceChanged {
            workspace_id: workspace,
        },
    });
}

async fn list_workspaces(
    State(app): State<AppState>,
    Caller(me): Caller,
) -> Reply<Vec<WorkspaceListing>> {
    let mut listings = Vec::new();
    for workspace in app.store.workspaces_for(me.id).await? {
        if let Some(access) = app.store.access(workspace.id, me.id).await? {
            listings.push(WorkspaceListing { workspace, access });
        }
    }
    Ok(Json(listings))
}

async fn workspace_summary(
    State(app): State<AppState>,
    Caller(me): Caller,
    Path(workspace): Path<WorkspaceId>,
) -> Reply<WorkspaceSummary> {
    let access = membership(&app, workspace, &me).await?;
    let mut projects = app.store.projects(workspace).await?;
    projects.retain(|p| access.can_view_project(p.id));
    Ok(Json(WorkspaceSummary {
        workspace: app.store.workspace(workspace).await?,
        projects,
        relation_types: app.store.relation_types(workspace).await?,
        roles: app.store.roles(workspace).await?,
        members: app.store.members(workspace).await?,
        access,
    }))
}

async fn create_project(
    State(app): State<AppState>,
    Caller(me): Caller,
    Path(workspace): Path<WorkspaceId>,
    Json(body): Json<CreateProject>,
) -> Result<(StatusCode, Json<Project>), Failure> {
    let access = membership(&app, workspace, &me).await?;
    require(
        &access,
        Permission::ConfigureProjects,
        None,
        "creating projects",
    )?;
    let project = app
        .store
        .create_project(workspace, &body.key, &body.name)
        .await?;
    workspace_changed(&app, workspace);
    Ok((StatusCode::CREATED, Json(project)))
}

async fn project_snapshot(
    State(app): State<AppState>,
    Caller(me): Caller,
    Path(project): Path<ProjectId>,
) -> Reply<ProjectSnapshot> {
    let (project, access) = project_access(&app, project, &me).await?;
    let snapshot = app
        .store
        .project_snapshot(project.id, |p| access.can_view_project(p))
        .await?;
    Ok(Json(snapshot))
}

async fn create_task(
    State(app): State<AppState>,
    Caller(me): Caller,
    Path(project): Path<ProjectId>,
    Json(body): Json<CreateTask>,
) -> Result<(StatusCode, Json<Task>), Failure> {
    let (project, access) = project_access(&app, project, &me).await?;
    require(
        &access,
        Permission::EditTasks,
        Some(project.id),
        "creating tasks",
    )?;
    let task = app.store.create_task(project.id, body, Some(me.id)).await?;
    project_changed(&app, &project);
    Ok((StatusCode::CREATED, Json(task)))
}

async fn task_detail(
    State(app): State<AppState>,
    Caller(me): Caller,
    Path(task): Path<TaskId>,
) -> Reply<TaskDetail> {
    let (task, _, _) = task_access(&app, task, &me).await?;
    let comments = app.store.comments(task.id).await?;
    Ok(Json(TaskDetail { task, comments }))
}

/// Looks a task up by its human reference, such as `WEB-12`.
async fn task_by_reference(
    State(app): State<AppState>,
    Caller(me): Caller,
    Path((workspace, reference)): Path<(WorkspaceId, String)>,
) -> Reply<TaskDetail> {
    let access = membership(&app, workspace, &me).await?;
    let reference: TaskRef = reference
        .parse()
        .map_err(|e: hyakume_core::model::ValidationError| Failure::invalid(e.to_string()))?;
    let task = app.store.task_by_ref(workspace, &reference).await?;
    if !access.can_view_project(task.project_id) {
        return Err(Failure::not_found("task"));
    }
    let comments = app.store.comments(task.id).await?;
    Ok(Json(TaskDetail { task, comments }))
}

async fn update_task(
    State(app): State<AppState>,
    Caller(me): Caller,
    Path(task): Path<TaskId>,
    Json(body): Json<UpdateTask>,
) -> Reply<Task> {
    let (task, project, access) = task_access(&app, task, &me).await?;
    require(
        &access,
        Permission::EditTasks,
        Some(project.id),
        "editing tasks",
    )?;
    let task = app.store.update_task(task.id, body, Some(me.id)).await?;
    project_changed(&app, &project);
    // Linked projects draw this task's title and finished state, which may
    // have changed their blocked indicators.
    for other in app.store.linked_projects(task.id).await? {
        project_changed(&app, &app.store.project(other).await?);
    }
    Ok(Json(task))
}

async fn delete_task(
    State(app): State<AppState>,
    Caller(me): Caller,
    Path(task): Path<TaskId>,
) -> Result<StatusCode, Failure> {
    let (task, project, access) = task_access(&app, task, &me).await?;
    require(
        &access,
        Permission::DeleteTasks,
        Some(project.id),
        "deleting tasks",
    )?;
    // Relations into other projects disappear with the task, so those
    // projects change too.
    let linked = app.store.delete_task(task.id).await?;
    project_changed(&app, &project);
    for other in linked {
        project_changed(&app, &app.store.project(other).await?);
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn add_comment(
    State(app): State<AppState>,
    Caller(me): Caller,
    Path(task): Path<TaskId>,
    Json(body): Json<CreateComment>,
) -> Result<(StatusCode, Json<Comment>), Failure> {
    let (task, project, access) = task_access(&app, task, &me).await?;
    require(&access, Permission::Comment, Some(project.id), "commenting")?;
    let comment = app
        .store
        .add_comment(task.id, Some(me.id), &body.body)
        .await?;
    project_changed(&app, &project);
    Ok((StatusCode::CREATED, Json(comment)))
}

/// Both ends must be visible and the caller needs ManageRelations in both
/// projects, since the link shows up in each.
async fn relation_projects(
    app: &AppState,
    access: &Access,
    workspace: WorkspaceId,
    ends: [TaskId; 2],
) -> Result<Vec<Project>, Failure> {
    let mut projects: Vec<Project> = Vec::new();
    for end in ends {
        let task = app.store.task(end).await?;
        let project = app.store.project(task.project_id).await?;
        if project.workspace_id != workspace || !access.can_view_project(project.id) {
            return Err(Failure::not_found("task"));
        }
        require(
            access,
            Permission::ManageRelations,
            Some(project.id),
            "managing relationships",
        )?;
        if !projects.iter().any(|p| p.id == project.id) {
            projects.push(project);
        }
    }
    Ok(projects)
}

async fn create_relation(
    State(app): State<AppState>,
    Caller(me): Caller,
    Path(workspace): Path<WorkspaceId>,
    Json(body): Json<CreateRelation>,
) -> Result<(StatusCode, Json<Relation>), Failure> {
    let access = membership(&app, workspace, &me).await?;
    let projects = relation_projects(&app, &access, workspace, [body.from, body.to]).await?;
    let relation = app
        .store
        .create_relation(workspace, body, Some(me.id))
        .await?;
    for project in &projects {
        project_changed(&app, project);
    }
    Ok((StatusCode::CREATED, Json(relation)))
}

async fn delete_relation(
    State(app): State<AppState>,
    Caller(me): Caller,
    Path(relation): Path<RelationId>,
) -> Result<StatusCode, Failure> {
    let relation = app.store.relation(relation).await?;
    let access = membership(&app, relation.workspace_id, &me)
        .await
        .map_err(|_| Failure::not_found("relation"))?;
    let projects = relation_projects(
        &app,
        &access,
        relation.workspace_id,
        [relation.from, relation.to],
    )
    .await?;
    app.store.delete_relation(relation.id).await?;
    for project in &projects {
        project_changed(&app, project);
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn create_relation_type(
    State(app): State<AppState>,
    Caller(me): Caller,
    Path(workspace): Path<WorkspaceId>,
    Json(body): Json<CreateRelationType>,
) -> Result<(StatusCode, Json<RelationType>), Failure> {
    let access = membership(&app, workspace, &me).await?;
    require(
        &access,
        Permission::ConfigureProjects,
        None,
        "defining relationship types",
    )?;
    let relation_type = app.store.create_relation_type(workspace, body).await?;
    workspace_changed(&app, workspace);
    Ok((StatusCode::CREATED, Json(relation_type)))
}

async fn create_role(
    State(app): State<AppState>,
    Caller(me): Caller,
    Path(workspace): Path<WorkspaceId>,
    Json(body): Json<CreateRole>,
) -> Result<(StatusCode, Json<Role>), Failure> {
    let access = membership(&app, workspace, &me).await?;
    require(&access, Permission::ManageRoles, None, "managing roles")?;
    let role = app.store.create_role(workspace, body).await?;
    workspace_changed(&app, workspace);
    Ok((StatusCode::CREATED, Json(role)))
}

async fn set_member_roles(
    State(app): State<AppState>,
    Caller(me): Caller,
    Path((workspace, identity)): Path<(WorkspaceId, IdentityId)>,
    Json(body): Json<SetMemberRoles>,
) -> Result<StatusCode, Failure> {
    let access = membership(&app, workspace, &me).await?;
    require(&access, Permission::ManageRoles, None, "managing roles")?;
    app.store
        .set_member_roles(workspace, identity, &body.roles)
        .await?;
    workspace_changed(&app, workspace);
    Ok(StatusCode::NO_CONTENT)
}

async fn get_layout(
    State(app): State<AppState>,
    Caller(me): Caller,
    Path(workspace): Path<WorkspaceId>,
) -> Reply<Layout> {
    membership(&app, workspace, &me).await?;
    Ok(Json(app.store.layout(me.id, workspace).await?))
}

/// Saves the caller's own positions. Other people never see them; the
/// caller's other devices get a LayoutChanged event.
async fn put_layout(
    State(app): State<AppState>,
    Caller(me): Caller,
    Path(workspace): Path<WorkspaceId>,
    Json(body): Json<Layout>,
) -> Result<StatusCode, Failure> {
    let access = membership(&app, workspace, &me).await?;
    for node in &body.nodes {
        let project = match node.kind {
            NodeKind::Project => ProjectId(node.id),
            NodeKind::Task => app.store.task(TaskId(node.id)).await?.project_id,
        };
        if !access.can_view_project(project) {
            return Err(Failure::not_found("layout node"));
        }
    }
    app.store.put_layout(me.id, workspace, &body.nodes).await?;
    app.notify(Notice {
        workspace,
        project: None,
        only: Some(me.id),
        event: Event::LayoutChanged {
            workspace_id: workspace,
        },
    });
    Ok(StatusCode::NO_CONTENT)
}
