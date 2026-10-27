//! Workspaces, projects, stages, tasks, comments, and relations.

use std::collections::HashSet;

use hyakume_core::api::{
    CreateRelation, CreateRelationType, CreateTask, ProjectSnapshot, TaskStub, UpdateTask,
};
use hyakume_core::ids::{
    CommentId, IdentityId, ProjectId, RelationId, RelationTypeId, RoleId, StageId, TaskId,
    WorkspaceId,
};
use hyakume_core::model::{
    BuiltinRelation, Comment, DEFAULT_STAGES, Priority, Project, ProjectKey, Relation,
    RelationType, Stage, Task, TaskFields, TaskRef, ValidationError, Workspace, check_text,
    name_key, relation_endpoints,
};
use hyakume_core::permission::Preset;
use sqlx::{Sqlite, SqliteConnection, Transaction};

use crate::convert::{date_from_text, date_to_text, from_millis, id, now_ms, opt_id};
use crate::{Store, StoreError, unique_violation};

mod transfer;

#[derive(sqlx::FromRow)]
struct WorkspaceRow {
    id: String,
    slug: String,
    name: String,
}

impl TryFrom<WorkspaceRow> for Workspace {
    type Error = StoreError;

    fn try_from(r: WorkspaceRow) -> Result<Self, StoreError> {
        Ok(Self {
            id: id(&r.id)?,
            slug: r.slug,
            name: r.name,
        })
    }
}

#[derive(sqlx::FromRow)]
struct ProjectRow {
    id: String,
    workspace_id: String,
    key: String,
    name: String,
}

impl TryFrom<ProjectRow> for Project {
    type Error = StoreError;

    fn try_from(r: ProjectRow) -> Result<Self, StoreError> {
        Ok(Self {
            id: id(&r.id)?,
            workspace_id: id(&r.workspace_id)?,
            key: r
                .key
                .parse()
                .map_err(|_| StoreError::Corrupt(format!("project key {:?}", r.key)))?,
            name: r.name,
        })
    }
}

#[derive(sqlx::FromRow)]
struct StageRow {
    id: String,
    project_id: String,
    name: String,
    position: i64,
    finished: bool,
}

impl TryFrom<StageRow> for Stage {
    type Error = StoreError;

    fn try_from(r: StageRow) -> Result<Self, StoreError> {
        Ok(Self {
            id: id(&r.id)?,
            project_id: id(&r.project_id)?,
            name: r.name,
            position: r.position,
            finished: r.finished,
        })
    }
}

#[derive(sqlx::FromRow)]
struct TaskRow {
    id: String,
    project_id: String,
    number: i64,
    key: String,
    title: String,
    description: String,
    stage_id: String,
    assignee_id: Option<String>,
    labels: String,
    priority: Option<String>,
    due_date: Option<String>,
    version: i64,
    created_at: i64,
    updated_at: i64,
}

impl TryFrom<TaskRow> for Task {
    type Error = StoreError;

    fn try_from(r: TaskRow) -> Result<Self, StoreError> {
        let labels: Vec<String> = serde_json::from_str(&r.labels)
            .map_err(|e| StoreError::Corrupt(format!("task labels: {e}")))?;
        let priority = r
            .priority
            .as_deref()
            .map(str::parse::<Priority>)
            .transpose()
            .map_err(|e| StoreError::Corrupt(e.to_string()))?;
        Ok(Self {
            id: id(&r.id)?,
            project_id: id(&r.project_id)?,
            number: r.number,
            reference: format!("{}-{}", r.key, r.number),
            fields: TaskFields {
                title: r.title,
                description: r.description,
                stage_id: id(&r.stage_id)?,
                assignee: opt_id(r.assignee_id.as_deref())?,
                labels,
                priority,
                due_date: r.due_date.as_deref().map(date_from_text).transpose()?,
            },
            version: r.version,
            created_at: from_millis(r.created_at)?,
            updated_at: from_millis(r.updated_at)?,
        })
    }
}

macro_rules! task_select {
    () => {
        "SELECT t.id, t.project_id, t.number, p.key, t.title, t.description, t.stage_id, \
         t.assignee_id, t.labels, t.priority, t.due_date, t.version, t.created_at, t.updated_at \
         FROM tasks t JOIN projects p ON p.id = t.project_id"
    };
}

#[derive(sqlx::FromRow)]
struct RelationTypeRow {
    id: String,
    workspace_id: String,
    name: String,
    reverse_name: Option<String>,
    directed: bool,
    builtin: Option<String>,
}

impl TryFrom<RelationTypeRow> for RelationType {
    type Error = StoreError;

    fn try_from(r: RelationTypeRow) -> Result<Self, StoreError> {
        let builtin = match r.builtin.as_deref() {
            None => None,
            Some(s) => Some(
                BuiltinRelation::parse(s)
                    .ok_or_else(|| StoreError::Corrupt(format!("builtin relation {s:?}")))?,
            ),
        };
        Ok(Self {
            id: id(&r.id)?,
            workspace_id: id(&r.workspace_id)?,
            name: r.name,
            reverse_name: r.reverse_name,
            directed: r.directed,
            builtin,
        })
    }
}

#[derive(sqlx::FromRow)]
struct RelationRow {
    id: String,
    workspace_id: String,
    type_id: String,
    from_task: String,
    to_task: String,
}

impl TryFrom<RelationRow> for Relation {
    type Error = StoreError;

    fn try_from(r: RelationRow) -> Result<Self, StoreError> {
        Ok(Self {
            id: id(&r.id)?,
            workspace_id: id(&r.workspace_id)?,
            type_id: id(&r.type_id)?,
            from: id(&r.from_task)?,
            to: id(&r.to_task)?,
        })
    }
}

#[derive(sqlx::FromRow)]
struct CommentRow {
    id: String,
    task_id: String,
    author_id: Option<String>,
    body: String,
    created_at: i64,
}

impl TryFrom<CommentRow> for Comment {
    type Error = StoreError;

    fn try_from(r: CommentRow) -> Result<Self, StoreError> {
        Ok(Self {
            id: id(&r.id)?,
            task_id: id(&r.task_id)?,
            author: opt_id(r.author_id.as_deref())?,
            body: r.body,
            created_at: from_millis(r.created_at)?,
        })
    }
}

fn convert_all<R, T>(rows: Vec<R>) -> Result<Vec<T>, StoreError>
where
    T: TryFrom<R, Error = StoreError>,
{
    rows.into_iter().map(T::try_from).collect()
}

impl Store {
    /// Creates a workspace with the built-in relation types, the four editable
    /// role presets, and `owner` as its owner, all in one transaction, so a
    /// workspace never exists without an owner.
    pub async fn create_workspace(
        &self,
        slug: &str,
        name: &str,
        owner: IdentityId,
    ) -> Result<Workspace, StoreError> {
        let workspace = Workspace {
            id: WorkspaceId::new(),
            slug: slug.trim().to_owned(),
            name: name.trim().to_owned(),
        };
        if workspace.slug.is_empty() || workspace.name.is_empty() {
            return Err(StoreError::invalid(
                "workspace slug and name must not be empty",
            ));
        }
        check_text("workspace slug", &workspace.slug, false)?;
        check_text("workspace name", &workspace.name, false)?;
        let mut tx = self.write().await?;
        sqlx::query("INSERT INTO workspaces (id, slug, name, created_at) VALUES (?, ?, ?, ?)")
            .bind(workspace.id.to_string())
            .bind(&workspace.slug)
            .bind(&workspace.name)
            .bind(now_ms())
            .execute(&mut *tx)
            .await
            .map_err(|e| unique_violation(e, || format!("workspace {slug:?} already exists")))?;

        let builtins = [
            (BuiltinRelation::Blocks, "blocks", Some("blocked by"), true),
            (BuiltinRelation::RelatedTo, "related to", None, false),
        ];
        for (builtin, name, reverse, directed) in builtins {
            sqlx::query(
                "INSERT INTO relation_types (id, workspace_id, name, reverse_name, directed, builtin) \
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(RelationTypeId::new().to_string())
            .bind(workspace.id.to_string())
            .bind(name)
            .bind(reverse)
            .bind(directed)
            .bind(builtin.as_str())
            .execute(&mut *tx)
            .await?;
        }

        let mut owner_role = None;
        for preset in Preset::ALL {
            let role_id = RoleId::new();
            if preset == Preset::Owner {
                owner_role = Some(role_id);
            }
            sqlx::query("INSERT INTO roles (id, workspace_id, name, preset) VALUES (?, ?, ?, ?)")
                .bind(role_id.to_string())
                .bind(workspace.id.to_string())
                .bind(preset.name())
                .bind(preset.as_str())
                .execute(&mut *tx)
                .await?;
            for permission in preset.default_permissions() {
                sqlx::query("INSERT INTO role_permissions (role_id, permission) VALUES (?, ?)")
                    .bind(role_id.to_string())
                    .bind(permission.as_str())
                    .execute(&mut *tx)
                    .await?;
            }
        }

        let identity_exists: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM identities WHERE id = ?)")
                .bind(owner.to_string())
                .fetch_one(&mut *tx)
                .await?;
        if !identity_exists {
            return Err(StoreError::NotFound("owner identity"));
        }
        sqlx::query(
            "INSERT INTO memberships (workspace_id, identity_id, kind, is_owner) \
             VALUES (?, ?, 'member', 1)",
        )
        .bind(workspace.id.to_string())
        .bind(owner.to_string())
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO membership_roles (workspace_id, identity_id, role_id) VALUES (?, ?, ?)",
        )
        .bind(workspace.id.to_string())
        .bind(owner.to_string())
        .bind(owner_role.map(|r| r.to_string()))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(workspace)
    }

    pub async fn workspace(&self, id: WorkspaceId) -> Result<Workspace, StoreError> {
        sqlx::query_as::<_, WorkspaceRow>("SELECT id, slug, name FROM workspaces WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .ok_or(StoreError::NotFound("workspace"))?
            .try_into()
    }

    pub async fn workspace_by_slug(&self, slug: &str) -> Result<Workspace, StoreError> {
        sqlx::query_as::<_, WorkspaceRow>("SELECT id, slug, name FROM workspaces WHERE slug = ?")
            .bind(slug)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(StoreError::NotFound("workspace"))?
            .try_into()
    }

    /// Workspaces the identity belongs to, as member or guest.
    pub async fn workspaces_for(&self, identity: IdentityId) -> Result<Vec<Workspace>, StoreError> {
        let rows = sqlx::query_as::<_, WorkspaceRow>(
            "SELECT w.id, w.slug, w.name FROM workspaces w \
             JOIN memberships m ON m.workspace_id = w.id \
             WHERE m.identity_id = ? ORDER BY w.name",
        )
        .bind(identity.to_string())
        .fetch_all(&self.pool)
        .await?;
        convert_all(rows)
    }

    /// Creates a project with the default workflow stages.
    pub async fn create_project(
        &self,
        workspace: WorkspaceId,
        key: &ProjectKey,
        name: &str,
    ) -> Result<Project, StoreError> {
        let project = Project {
            id: ProjectId::new(),
            workspace_id: workspace,
            key: key.clone(),
            name: name.trim().to_owned(),
        };
        if project.name.is_empty() {
            return Err(StoreError::invalid("project name must not be empty"));
        }
        check_text("project name", &project.name, false)?;
        let mut tx = self.write().await?;
        sqlx::query(
            "INSERT INTO projects (id, workspace_id, key, name, created_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(project.id.to_string())
        .bind(workspace.to_string())
        .bind(key.as_str())
        .bind(&project.name)
        .bind(now_ms())
        .execute(&mut *tx)
        .await
        .map_err(|e| unique_violation(e, || format!("project key {key} is already used")))?;
        for (position, (stage, finished)) in DEFAULT_STAGES.into_iter().enumerate() {
            sqlx::query(
                "INSERT INTO stages (id, project_id, name, position, finished) VALUES (?, ?, ?, ?, ?)",
            )
            .bind(StageId::new().to_string())
            .bind(project.id.to_string())
            .bind(stage)
            .bind(position as i64)
            .bind(finished)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(project)
    }

    pub async fn projects(&self, workspace: WorkspaceId) -> Result<Vec<Project>, StoreError> {
        let rows = sqlx::query_as::<_, ProjectRow>(
            "SELECT id, workspace_id, key, name FROM projects WHERE workspace_id = ? ORDER BY key",
        )
        .bind(workspace.to_string())
        .fetch_all(&self.pool)
        .await?;
        convert_all(rows)
    }

    pub async fn project(&self, id: ProjectId) -> Result<Project, StoreError> {
        fetch_project(&mut *self.pool.acquire().await?, id).await
    }

    pub async fn project_by_key(
        &self,
        workspace: WorkspaceId,
        key: &ProjectKey,
    ) -> Result<Project, StoreError> {
        sqlx::query_as::<_, ProjectRow>(
            "SELECT id, workspace_id, key, name FROM projects WHERE workspace_id = ? AND key = ?",
        )
        .bind(workspace.to_string())
        .bind(key.as_str())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(StoreError::NotFound("project"))?
        .try_into()
    }

    pub async fn stages(&self, project: ProjectId) -> Result<Vec<Stage>, StoreError> {
        fetch_stages(&mut *self.pool.acquire().await?, project).await
    }

    pub async fn create_task(
        &self,
        project: ProjectId,
        input: CreateTask,
        actor: Option<IdentityId>,
    ) -> Result<Task, StoreError> {
        let mut tx = self.write().await?;
        let workspace = project_workspace(&mut tx, project).await?;
        let stage_id = match input.stage_id {
            Some(stage) => stage,
            None => {
                let first: Option<String> = sqlx::query_scalar(
                    "SELECT id FROM stages WHERE project_id = ? ORDER BY position LIMIT 1",
                )
                .bind(project.to_string())
                .fetch_optional(&mut *tx)
                .await?;
                id(&first.ok_or_else(|| StoreError::invalid("project has no stages"))?)?
            }
        };
        let fields = TaskFields {
            title: input.title,
            description: input.description,
            stage_id,
            assignee: input.assignee,
            labels: input.labels,
            priority: input.priority,
            due_date: input.due_date,
        }
        .normalized()?;
        check_fields(&mut tx, workspace, project, &fields).await?;

        let number: i64 = sqlx::query_scalar(
            "UPDATE projects SET next_task_number = next_task_number + 1 WHERE id = ? \
             RETURNING next_task_number - 1",
        )
        .bind(project.to_string())
        .fetch_one(&mut *tx)
        .await?;
        let task_id = TaskId::new();
        let now = now_ms();
        sqlx::query(
            "INSERT INTO tasks (id, project_id, number, title, description, stage_id, assignee_id, \
             labels, priority, due_date, version, created_at, updated_at, created_by, updated_by) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1, ?, ?, ?, ?)",
        )
        .bind(task_id.to_string())
        .bind(project.to_string())
        .bind(number)
        .bind(&fields.title)
        .bind(&fields.description)
        .bind(fields.stage_id.to_string())
        .bind(fields.assignee.map(|a| a.to_string()))
        .bind(labels_json(&fields.labels))
        .bind(fields.priority.map(Priority::as_str))
        .bind(fields.due_date.map(date_to_text))
        .bind(now)
        .bind(now)
        .bind(actor.map(|a| a.to_string()))
        .bind(actor.map(|a| a.to_string()))
        .execute(&mut *tx)
        .await?;
        let task = fetch_task(&mut tx, task_id).await?;
        tx.commit().await?;
        Ok(task)
    }

    pub async fn task(&self, id: TaskId) -> Result<Task, StoreError> {
        let mut conn = self.pool.acquire().await?;
        fetch_task(&mut conn, id).await
    }

    pub async fn task_by_ref(
        &self,
        workspace: WorkspaceId,
        reference: &TaskRef,
    ) -> Result<Task, StoreError> {
        sqlx::query_as::<_, TaskRow>(concat!(
            task_select!(),
            " WHERE p.workspace_id = ? AND p.key = ? AND t.number = ?"
        ))
        .bind(workspace.to_string())
        .bind(reference.project.as_str())
        .bind(reference.number)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(StoreError::NotFound("task"))?
        .try_into()
    }

    pub async fn tasks(&self, project: ProjectId) -> Result<Vec<Task>, StoreError> {
        fetch_tasks(&mut *self.pool.acquire().await?, project).await
    }

    /// Replaces every editable field if the task is still at
    /// `expected_version`. Otherwise returns `Conflict` with the latest task and
    /// changes nothing, so the caller can show both versions for review.
    pub async fn update_task(
        &self,
        id: TaskId,
        update: UpdateTask,
        actor: Option<IdentityId>,
    ) -> Result<Task, StoreError> {
        let fields = update.fields.normalized()?;
        let mut tx = self.write().await?;
        let current = fetch_task(&mut tx, id).await?;
        if current.version != update.expected_version {
            return Err(StoreError::Conflict {
                expected: update.expected_version,
                latest: Box::new(current),
            });
        }
        let workspace = project_workspace(&mut tx, current.project_id).await?;
        check_fields(&mut tx, workspace, current.project_id, &fields).await?;
        sqlx::query(
            "UPDATE tasks SET title = ?, description = ?, stage_id = ?, assignee_id = ?, \
             labels = ?, priority = ?, due_date = ?, version = version + 1, updated_at = ?, \
             updated_by = ? WHERE id = ?",
        )
        .bind(&fields.title)
        .bind(&fields.description)
        .bind(fields.stage_id.to_string())
        .bind(fields.assignee.map(|a| a.to_string()))
        .bind(labels_json(&fields.labels))
        .bind(fields.priority.map(Priority::as_str))
        .bind(fields.due_date.map(date_to_text))
        .bind(now_ms())
        .bind(actor.map(|a| a.to_string()))
        .bind(id.to_string())
        .execute(&mut *tx)
        .await?;
        let task = fetch_task(&mut tx, id).await?;
        tx.commit().await?;
        Ok(task)
    }

    /// Deletes the task with its comments and relations. Returns the other
    /// projects it was linked to, since their snapshots lose those links.
    pub async fn delete_task(&self, id: TaskId) -> Result<Vec<ProjectId>, StoreError> {
        let mut tx = self.write().await?;
        let task = fetch_task(&mut tx, id).await?;
        let linked = fetch_linked_projects(&mut tx, task.id).await?;
        sqlx::query("DELETE FROM tasks WHERE id = ?")
            .bind(id.to_string())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(linked)
    }

    pub async fn add_comment(
        &self,
        task: TaskId,
        author: Option<IdentityId>,
        body: &str,
    ) -> Result<Comment, StoreError> {
        let body = body.trim();
        if body.is_empty() {
            return Err(StoreError::invalid("comment must not be empty"));
        }
        check_text("comment", body, true)?;
        let comment_id = CommentId::new();
        let now = now_ms();
        let result = sqlx::query(
            "INSERT INTO comments (id, task_id, author_id, body, created_at) \
             SELECT ?, id, ?, ?, ? FROM tasks WHERE id = ?",
        )
        .bind(comment_id.to_string())
        .bind(author.map(|a| a.to_string()))
        .bind(body)
        .bind(now)
        .bind(task.to_string())
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(StoreError::NotFound("task"));
        }
        Ok(Comment {
            id: comment_id,
            task_id: task,
            author,
            body: body.to_owned(),
            created_at: from_millis(now)?,
        })
    }

    pub async fn comments(&self, task: TaskId) -> Result<Vec<Comment>, StoreError> {
        let rows = sqlx::query_as::<_, CommentRow>(
            "SELECT id, task_id, author_id, body, created_at FROM comments \
             WHERE task_id = ? ORDER BY created_at, id",
        )
        .bind(task.to_string())
        .fetch_all(&self.pool)
        .await?;
        convert_all(rows)
    }

    pub async fn relation_types(
        &self,
        workspace: WorkspaceId,
    ) -> Result<Vec<RelationType>, StoreError> {
        let rows = sqlx::query_as::<_, RelationTypeRow>(
            "SELECT id, workspace_id, name, reverse_name, directed, builtin FROM relation_types \
             WHERE workspace_id = ? ORDER BY builtin IS NULL, name",
        )
        .bind(workspace.to_string())
        .fetch_all(&self.pool)
        .await?;
        convert_all(rows)
    }

    /// Custom types carry no automatic behavior; only the built-in "blocks"
    /// type affects the blocked indicator.
    pub async fn create_relation_type(
        &self,
        workspace: WorkspaceId,
        input: CreateRelationType,
    ) -> Result<RelationType, StoreError> {
        let relation_type = relation_type(RelationTypeId::new(), workspace, input)?;
        let mut tx = self.write().await?;
        insert_relation_type(&mut tx, &relation_type).await?;
        tx.commit().await?;
        Ok(relation_type)
    }

    /// Links two tasks in the same workspace. They may be in different projects.
    pub async fn create_relation(
        &self,
        workspace: WorkspaceId,
        input: CreateRelation,
        actor: Option<IdentityId>,
    ) -> Result<Relation, StoreError> {
        if input.from == input.to {
            return Err(StoreError::invalid("a task cannot be related to itself"));
        }
        let mut tx = self.write().await?;
        let directed: Option<bool> = sqlx::query_scalar(
            "SELECT directed FROM relation_types WHERE id = ? AND workspace_id = ?",
        )
        .bind(input.type_id.to_string())
        .bind(workspace.to_string())
        .fetch_optional(&mut *tx)
        .await?;
        let directed = directed.ok_or(StoreError::NotFound("relation type"))?;
        for task in [input.from, input.to] {
            let task = fetch_task(&mut tx, task).await?;
            if project_workspace(&mut tx, task.project_id).await? != workspace {
                // Treat as missing rather than revealing a task in another workspace.
                return Err(StoreError::NotFound("task"));
            }
        }
        let (from, to) = relation_endpoints(directed, input.from, input.to);
        let relation = Relation {
            id: RelationId::new(),
            workspace_id: workspace,
            type_id: input.type_id,
            from,
            to,
        };
        sqlx::query(
            "INSERT INTO relations (id, workspace_id, type_id, from_task, to_task, created_at, created_by) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(relation.id.to_string())
        .bind(workspace.to_string())
        .bind(relation.type_id.to_string())
        .bind(from.to_string())
        .bind(to.to_string())
        .bind(now_ms())
        .bind(actor.map(|a| a.to_string()))
        .execute(&mut *tx)
        .await
        .map_err(|e| unique_violation(e, || "these tasks are already linked that way".into()))?;
        tx.commit().await?;
        Ok(relation)
    }

    pub async fn relation(&self, id: RelationId) -> Result<Relation, StoreError> {
        sqlx::query_as::<_, RelationRow>(
            "SELECT id, workspace_id, type_id, from_task, to_task FROM relations WHERE id = ?",
        )
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(StoreError::NotFound("relation"))?
        .try_into()
    }

    pub async fn delete_relation(&self, id: RelationId) -> Result<(), StoreError> {
        let result = sqlx::query("DELETE FROM relations WHERE id = ?")
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(StoreError::NotFound("relation"));
        }
        Ok(())
    }

    /// Relations with at least one end in `project`, including links to
    /// tasks in other projects.
    pub async fn relations_touching(
        &self,
        project: ProjectId,
    ) -> Result<Vec<Relation>, StoreError> {
        fetch_relations_touching(&mut *self.pool.acquire().await?, project).await
    }

    /// Projects other than the task's own that hold tasks linked to it.
    pub async fn linked_projects(&self, task: TaskId) -> Result<Vec<ProjectId>, StoreError> {
        fetch_linked_projects(&mut *self.pool.acquire().await?, task).await
    }

    /// Reads one project with its relations. Relations to projects for which
    /// `can_view` returns false are dropped, so a guest never learns that those
    /// tasks exist.
    pub async fn project_snapshot(
        &self,
        project: ProjectId,
        can_view: impl Fn(ProjectId) -> bool,
    ) -> Result<ProjectSnapshot, StoreError> {
        // One read transaction, so every query sees the same commit. Separate
        // reads could load a relation and then miss its task deleted meanwhile.
        let mut tx = self.pool.begin().await?;
        let project = fetch_project(&mut tx, project).await?;
        let stages = fetch_stages(&mut tx, project.id).await?;
        let tasks = fetch_tasks(&mut tx, project.id).await?;
        let own: HashSet<TaskId> = tasks.iter().map(|t| t.id).collect();

        let mut relations = Vec::new();
        let mut linked_tasks = Vec::new();
        let mut seen = HashSet::new();
        for relation in fetch_relations_touching(&mut tx, project.id).await? {
            let mut visible = true;
            for end in [relation.from, relation.to] {
                if own.contains(&end) {
                    continue;
                }
                let stub = fetch_task_stub(&mut tx, end).await?;
                if !can_view(stub.project_id) {
                    visible = false;
                    break;
                }
                if seen.insert(end) {
                    linked_tasks.push(stub);
                }
            }
            if visible {
                relations.push(relation);
            }
        }
        tx.commit().await?;
        // A stub may have been added before a later end of its relation was hidden.
        let kept: HashSet<TaskId> = relations.iter().flat_map(|r| [r.from, r.to]).collect();
        linked_tasks.retain(|s| kept.contains(&s.id));

        Ok(ProjectSnapshot {
            project,
            stages,
            tasks,
            relations,
            linked_tasks,
        })
    }
}

async fn fetch_project(conn: &mut SqliteConnection, id: ProjectId) -> Result<Project, StoreError> {
    sqlx::query_as::<_, ProjectRow>("SELECT id, workspace_id, key, name FROM projects WHERE id = ?")
        .bind(id.to_string())
        .fetch_optional(conn)
        .await?
        .ok_or(StoreError::NotFound("project"))?
        .try_into()
}

async fn fetch_stages(
    conn: &mut SqliteConnection,
    project: ProjectId,
) -> Result<Vec<Stage>, StoreError> {
    let rows = sqlx::query_as::<_, StageRow>(
        "SELECT id, project_id, name, position, finished FROM stages \
         WHERE project_id = ? ORDER BY position",
    )
    .bind(project.to_string())
    .fetch_all(conn)
    .await?;
    convert_all(rows)
}

async fn fetch_tasks(
    conn: &mut SqliteConnection,
    project: ProjectId,
) -> Result<Vec<Task>, StoreError> {
    let rows = sqlx::query_as::<_, TaskRow>(concat!(
        task_select!(),
        " WHERE t.project_id = ? ORDER BY t.number"
    ))
    .bind(project.to_string())
    .fetch_all(conn)
    .await?;
    convert_all(rows)
}

async fn fetch_relations_touching(
    conn: &mut SqliteConnection,
    project: ProjectId,
) -> Result<Vec<Relation>, StoreError> {
    let rows = sqlx::query_as::<_, RelationRow>(
        "SELECT r.id, r.workspace_id, r.type_id, r.from_task, r.to_task FROM relations r \
         WHERE r.from_task IN (SELECT id FROM tasks WHERE project_id = ?1) \
            OR r.to_task IN (SELECT id FROM tasks WHERE project_id = ?1) \
         ORDER BY r.id",
    )
    .bind(project.to_string())
    .fetch_all(conn)
    .await?;
    convert_all(rows)
}

/// A single statement, so a concurrent delete can't leave it half-read.
/// A task that's already gone just has no linked projects.
async fn fetch_linked_projects(
    conn: &mut SqliteConnection,
    task: TaskId,
) -> Result<Vec<ProjectId>, StoreError> {
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT t.project_id FROM relations r \
         JOIN tasks t ON t.id = CASE WHEN r.from_task = ?1 THEN r.to_task ELSE r.from_task END \
         WHERE (r.from_task = ?1 OR r.to_task = ?1) \
           AND t.project_id <> (SELECT project_id FROM tasks WHERE id = ?1) \
         ORDER BY t.project_id",
    )
    .bind(task.to_string())
    .fetch_all(conn)
    .await?;
    rows.iter().map(|p| id(p)).collect()
}

async fn fetch_task_stub(
    conn: &mut SqliteConnection,
    task: TaskId,
) -> Result<TaskStub, StoreError> {
    let row: Option<(String, String, i64, String, bool)> = sqlx::query_as(
        "SELECT t.project_id, p.key, t.number, t.title, s.finished FROM tasks t \
         JOIN projects p ON p.id = t.project_id JOIN stages s ON s.id = t.stage_id \
         WHERE t.id = ?",
    )
    .bind(task.to_string())
    .fetch_optional(conn)
    .await?;
    let (project_id, key, number, title, finished) = row.ok_or(StoreError::NotFound("task"))?;
    Ok(TaskStub {
        id: task,
        project_id: id(&project_id)?,
        reference: format!("{key}-{number}"),
        title,
        finished,
    })
}

async fn fetch_task(conn: &mut SqliteConnection, id: TaskId) -> Result<Task, StoreError> {
    sqlx::query_as::<_, TaskRow>(concat!(task_select!(), " WHERE t.id = ?"))
        .bind(id.to_string())
        .fetch_optional(conn)
        .await?
        .ok_or(StoreError::NotFound("task"))?
        .try_into()
}

async fn project_workspace(
    conn: &mut SqliteConnection,
    project: ProjectId,
) -> Result<WorkspaceId, StoreError> {
    let workspace: Option<String> =
        sqlx::query_scalar("SELECT workspace_id FROM projects WHERE id = ?")
            .bind(project.to_string())
            .fetch_optional(conn)
            .await?;
    id(&workspace.ok_or(StoreError::NotFound("project"))?)
}

/// The stage must belong to the task's project and the assignee must be in
/// the workspace; neither is expressible as a foreign key.
async fn check_fields(
    tx: &mut Transaction<'static, Sqlite>,
    workspace: WorkspaceId,
    project: ProjectId,
    fields: &TaskFields,
) -> Result<(), StoreError> {
    let stage_ok: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM stages WHERE id = ? AND project_id = ?)")
            .bind(fields.stage_id.to_string())
            .bind(project.to_string())
            .fetch_one(&mut **tx)
            .await?;
    if !stage_ok {
        return Err(ValidationError::Other("stage is not part of this project".into()).into());
    }
    if let Some(assignee) = fields.assignee {
        let member: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM memberships WHERE workspace_id = ? AND identity_id = ?)",
        )
        .bind(workspace.to_string())
        .bind(assignee.to_string())
        .fetch_one(&mut **tx)
        .await?;
        if !member {
            return Err(ValidationError::Other("assignee is not in this workspace".into()).into());
        }
    }
    Ok(())
}

/// A custom relation type as `input` asks for it, checked on its own.
fn relation_type(
    id: RelationTypeId,
    workspace: WorkspaceId,
    input: CreateRelationType,
) -> Result<RelationType, StoreError> {
    let name = input.name.trim().to_owned();
    if name.is_empty() {
        return Err(StoreError::invalid("relation type name must not be empty"));
    }
    check_text("relation type name", &name, false)?;
    let reverse_name = if input.directed {
        // Without a reverse label, the target's side would read "API-1
        // depends on WEB-1" when it's really WEB-1 that depends on API-1.
        let reverse = input
            .reverse_name
            .map(|r| r.trim().to_owned())
            .filter(|r| !r.is_empty());
        if let Some(reverse) = &reverse {
            check_text("reverse name", reverse, false)?;
        } else {
            return Err(StoreError::invalid(
                "a directed relation type needs a reverse name, such as \"needed by\"",
            ));
        }
        reverse
    } else if input.reverse_name.is_some() {
        return Err(StoreError::invalid(
            "only directed relation types have a reverse name",
        ));
    } else {
        None
    };
    Ok(RelationType {
        id,
        workspace_id: workspace,
        name,
        reverse_name,
        directed: input.directed,
        builtin: None,
    })
}

/// Adds a custom type once its names are clear of the workspace's others.
async fn insert_relation_type(
    tx: &mut Transaction<'static, Sqlite>,
    relation_type: &RelationType,
) -> Result<(), StoreError> {
    // Every name and reverse name must be unambiguous as people type it,
    // or "Blocks" could silently resolve to the built-in "blocks".
    let existing: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT name, reverse_name FROM relation_types WHERE workspace_id = ?")
            .bind(relation_type.workspace_id.to_string())
            .fetch_all(&mut **tx)
            .await?;
    let taken: HashSet<String> = existing
        .iter()
        .flat_map(|(name, reverse)| std::iter::once(name).chain(reverse))
        .map(|n| name_key(n))
        .collect();
    let new_names = std::iter::once(&relation_type.name).chain(&relation_type.reverse_name);
    for candidate in new_names {
        if taken.contains(&name_key(candidate)) {
            return Err(StoreError::Duplicate(format!(
                "a relation type already uses the name {candidate:?}"
            )));
        }
    }
    if relation_type
        .reverse_name
        .as_ref()
        .is_some_and(|r| name_key(r) == name_key(&relation_type.name))
    {
        return Err(StoreError::invalid(
            "a relation type's reverse name must differ from its name",
        ));
    }
    sqlx::query(
        "INSERT INTO relation_types (id, workspace_id, name, reverse_name, directed) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(relation_type.id.to_string())
    .bind(relation_type.workspace_id.to_string())
    .bind(&relation_type.name)
    .bind(&relation_type.reverse_name)
    .bind(relation_type.directed)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn labels_json(labels: &[String]) -> String {
    serde_json::to_string(labels).unwrap_or_else(|_| "[]".to_owned())
}
