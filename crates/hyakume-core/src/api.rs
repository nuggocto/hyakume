//! Request, response, and event bodies for the `/v1` HTTP and WebSocket API.
//! The server and every client use these same types.

use serde::{Deserialize, Serialize};
use time::Date;
use uuid::Uuid;

use crate::ids::{IdentityId, ProjectId, RelationTypeId, RoleId, StageId, TaskId, WorkspaceId};
use crate::model::{
    Comment, Identity, Priority, Project, ProjectKey, Relation, RelationType, Stage, Task,
    TaskFields, Workspace,
};
use crate::permission::{Access, MembershipKind, Permission, Preset};

pub const API_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServerInfo {
    pub api_version: u32,
    /// Clients refuse the development-only credential file for servers without this.
    pub dev_mode: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DevLoginRequest {
    pub identity: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LoginResponse {
    pub token: String,
    pub identity: Identity,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceListing {
    pub workspace: Workspace,
    pub access: Access,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceSummary {
    pub workspace: Workspace,
    pub access: Access,
    /// Only the projects this identity may view.
    pub projects: Vec<Project>,
    pub relation_types: Vec<RelationType>,
    pub roles: Vec<Role>,
    pub members: Vec<Member>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Role {
    pub id: RoleId,
    pub name: String,
    pub preset: Option<Preset>,
    pub permissions: Vec<Permission>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Member {
    pub identity: Identity,
    pub kind: MembershipKind,
    pub is_owner: bool,
    pub roles: Vec<RoleId>,
}

/// Everything needed to draw one project: its tasks, the relations touching
/// them, and enough about tasks in other projects to draw those links.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProjectSnapshot {
    pub project: Project,
    pub stages: Vec<Stage>,
    pub tasks: Vec<Task>,
    pub relations: Vec<Relation>,
    pub linked_tasks: Vec<TaskStub>,
}

/// A task from another project that a relation points at. Relations to
/// projects the viewer cannot see are left out entirely.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskStub {
    pub id: TaskId,
    pub project_id: ProjectId,
    pub reference: String,
    pub title: String,
    pub finished: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskDetail {
    pub task: Task,
    pub comments: Vec<Comment>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CreateProject {
    pub key: ProjectKey,
    pub name: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CreateTask {
    pub title: String,
    #[serde(default)]
    pub description: String,
    /// Defaults to the project's first stage.
    pub stage_id: Option<StageId>,
    pub assignee: Option<IdentityId>,
    #[serde(default)]
    pub labels: Vec<String>,
    pub priority: Option<Priority>,
    pub due_date: Option<Date>,
}

/// Replaces every editable field. Rejected with 409 if the task is no longer
/// at `expected_version`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UpdateTask {
    pub expected_version: i64,
    #[serde(flatten)]
    pub fields: TaskFields,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CreateComment {
    pub body: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CreateRelation {
    pub type_id: RelationTypeId,
    pub from: TaskId,
    pub to: TaskId,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CreateRelationType {
    pub name: String,
    pub directed: bool,
    pub reverse_name: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CreateRole {
    pub name: String,
    pub permissions: Vec<Permission>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SetMemberRoles {
    pub roles: Vec<RoleId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    Project,
    Task,
}

impl NodeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Task => "task",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "project" => Some(Self::Project),
            "task" => Some(Self::Task),
            _ => None,
        }
    }
}

/// One person's position for one canvas node. Never shared with other people.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LayoutNode {
    pub kind: NodeKind,
    pub id: Uuid,
    pub x: f64,
    pub y: f64,
    pub pinned: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Layout {
    pub nodes: Vec<LayoutNode>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Invalid,
    Unauthorized,
    Forbidden,
    NotFound,
    /// The task changed since the version the client edited.
    Conflict,
    /// A unique name or link already exists.
    Duplicate,
    Internal,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApiError {
    pub code: ErrorCode,
    pub message: String,
    /// Present on `conflict`: the task as it is now, for side-by-side review.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest: Option<Box<Task>>,
}

/// Server-to-client WebSocket messages. They name what changed, not the new
/// content, so a client refetches through the normal permission-checked API.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// First message on a new connection. Fetch data only after receiving it,
    /// so no change committed after the fetch can be missed.
    Subscribed,
    ProjectChanged {
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    },
    WorkspaceChanged {
        workspace_id: WorkspaceId,
    },
    /// Sent only to the identity that owns the layout, on all its devices.
    LayoutChanged {
        workspace_id: WorkspaceId,
    },
    /// The connection fell behind and dropped events; refetch everything.
    Resync,
}
