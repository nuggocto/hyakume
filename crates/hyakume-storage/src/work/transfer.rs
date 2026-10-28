//! Moving a whole workspace in and out as one `LocalExport`, for carrying
//! local work to another computer. Ids travel with it, so links and saved
//! positions still point at the right tasks after an import.

use std::collections::{HashMap, HashSet};

use hyakume_core::api::NodeKind;
use hyakume_core::export::{ExportedProject, FORMAT, LocalExport};
use hyakume_core::ids::{IdentityId, ProjectId, RelationTypeId, StageId, TaskId, WorkspaceId};
use hyakume_core::model::{BuiltinRelation, check_text, relation_endpoints};
use sqlx::{Sqlite, Transaction};
use time::OffsetDateTime;

use super::{
    CommentRow, RelationRow, RelationTypeRow, convert_all, date_to_text, fetch_project,
    fetch_stages, fetch_tasks, insert_relation_type, labels_json, relation_type,
};
use crate::convert::{id, millis, now_ms};
use crate::layout::fetch_layout;
use crate::{Store, StoreError, unique_violation};

impl Store {
    /// Everything in `workspace`, plus `identity`'s positions there, read in
    /// one transaction so the file is one consistent moment.
    pub async fn export_workspace(
        &self,
        workspace: WorkspaceId,
        identity: IdentityId,
    ) -> Result<LocalExport, StoreError> {
        let mut tx = self.pool.begin().await?;
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT id, next_task_number FROM projects WHERE workspace_id = ? ORDER BY key",
        )
        .bind(workspace.to_string())
        .fetch_all(&mut *tx)
        .await?;
        let mut projects = Vec::with_capacity(rows.len());
        for (project, next_task_number) in rows {
            let project: ProjectId = id(&project)?;
            let comments = sqlx::query_as::<_, CommentRow>(
                "SELECT c.id, c.task_id, c.author_id, c.body, c.created_at FROM comments c \
                 JOIN tasks t ON t.id = c.task_id WHERE t.project_id = ? \
                 ORDER BY c.created_at, c.id",
            )
            .bind(project.to_string())
            .fetch_all(&mut *tx)
            .await?;
            projects.push(ExportedProject {
                project: fetch_project(&mut tx, project).await?,
                next_task_number,
                stages: fetch_stages(&mut tx, project).await?,
                tasks: fetch_tasks(&mut tx, project).await?,
                comments: convert_all(comments)?,
            });
        }
        let types = sqlx::query_as::<_, RelationTypeRow>(
            "SELECT id, workspace_id, name, reverse_name, directed, builtin FROM relation_types \
             WHERE workspace_id = ? ORDER BY builtin IS NULL, name",
        )
        .bind(workspace.to_string())
        .fetch_all(&mut *tx)
        .await?;
        let relations = sqlx::query_as::<_, RelationRow>(
            "SELECT id, workspace_id, type_id, from_task, to_task FROM relations \
             WHERE workspace_id = ? ORDER BY id",
        )
        .bind(workspace.to_string())
        .fetch_all(&mut *tx)
        .await?;
        let layout = fetch_layout(&mut tx, identity, workspace).await?;
        tx.commit().await?;
        Ok(LocalExport {
            format: FORMAT,
            exported_at: OffsetDateTime::now_utc(),
            relation_types: convert_all(types)?,
            projects,
            relations: convert_all(relations)?,
            layout,
        })
    }

    /// Puts `export` into `workspace`, ids and all. What's there now goes
    /// first, and only with `replace`: an import never merges two copies of
    /// the same work. Whoever the file names becomes `identity`, since local
    /// work has one person. A file that breaks a rule imports nothing.
    pub async fn import_workspace(
        &self,
        workspace: WorkspaceId,
        identity: IdentityId,
        export: &LocalExport,
        replace: bool,
    ) -> Result<(), StoreError> {
        if export.format != FORMAT {
            return Err(StoreError::invalid(format!(
                "the file is export format {}, and this version reads only {FORMAT}",
                export.format
            )));
        }
        let mut tx = self.write().await?;
        let ws = workspace.to_string();
        let projects: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM projects WHERE workspace_id = ?")
                .bind(&ws)
                .fetch_one(&mut *tx)
                .await?;
        if projects > 0 && !replace {
            return Err(StoreError::Duplicate(format!(
                "this workspace already has {projects} projects"
            )));
        }
        // Tasks before projects: a task's stage has no cascade of its own.
        for clear in [
            "DELETE FROM tasks WHERE project_id IN (SELECT id FROM projects WHERE workspace_id = ?)",
            "DELETE FROM projects WHERE workspace_id = ?",
            "DELETE FROM relation_types WHERE workspace_id = ? AND builtin IS NULL",
        ] {
            sqlx::query(clear).bind(&ws).execute(&mut *tx).await?;
        }
        sqlx::query("DELETE FROM layouts WHERE identity_id = ? AND workspace_id = ?")
            .bind(identity.to_string())
            .bind(&ws)
            .execute(&mut *tx)
            .await?;

        let types = import_types(&mut tx, workspace, export).await?;
        let mut tasks = HashSet::new();
        for project in &export.projects {
            import_project(&mut tx, workspace, identity, project, &mut tasks).await?;
        }

        let me = identity.to_string();
        let now = now_ms();
        for relation in &export.relations {
            let Some(&(type_id, directed)) = types.get(&relation.type_id) else {
                return Err(StoreError::invalid(
                    "a link has a type the file doesn't list",
                ));
            };
            if !(tasks.contains(&relation.from) && tasks.contains(&relation.to)) {
                return Err(StoreError::invalid(
                    "a link points at a task the file doesn't hold",
                ));
            }
            if relation.from == relation.to {
                return Err(StoreError::invalid("a task can't be linked to itself"));
            }
            let (from, to) = relation_endpoints(directed, relation.from, relation.to);
            sqlx::query(
                "INSERT INTO relations (id, workspace_id, type_id, from_task, to_task, created_at, created_by) \
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(relation.id.to_string())
            .bind(&ws)
            .bind(type_id.to_string())
            .bind(from.to_string())
            .bind(to.to_string())
            .bind(now)
            .bind(&me)
            .execute(&mut *tx)
            .await
            .map_err(|e| unique_violation(e, || "the file lists one link twice".into()))?;
        }

        let projects: HashSet<ProjectId> = export.projects.iter().map(|p| p.project.id).collect();
        for node in &export.layout {
            // A position for something the file doesn't hold has nothing to place.
            let held = match node.kind {
                NodeKind::Project => projects.contains(&ProjectId(node.id)),
                NodeKind::Task => tasks.contains(&TaskId(node.id)),
            };
            if !held {
                continue;
            }
            if !(node.x.is_finite() && node.y.is_finite()) {
                return Err(StoreError::invalid(
                    "layout positions must be finite numbers",
                ));
            }
            sqlx::query(
                "INSERT INTO layouts (identity_id, workspace_id, node_kind, node_id, x, y, pinned, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&me)
            .bind(&ws)
            .bind(node.kind.as_str())
            .bind(node.id.to_string())
            .bind(node.x)
            .bind(node.y)
            .bind(node.pinned)
            .bind(now)
            .execute(&mut *tx)
            .await
            .map_err(|e| unique_violation(e, || "the file places one node twice".into()))?;
        }
        tx.commit().await?;
        Ok(())
    }
}

/// The file's relation types, each mapped to the id it has here with its
/// direction. Built-in ones exist in every workspace under their own ids,
/// so links of those types move over to these.
async fn import_types(
    tx: &mut Transaction<'static, Sqlite>,
    workspace: WorkspaceId,
    export: &LocalExport,
) -> Result<HashMap<RelationTypeId, (RelationTypeId, bool)>, StoreError> {
    let rows: Vec<(String, String, bool)> = sqlx::query_as(
        "SELECT id, builtin, directed FROM relation_types \
         WHERE workspace_id = ? AND builtin IS NOT NULL",
    )
    .bind(workspace.to_string())
    .fetch_all(&mut **tx)
    .await?;
    let mut builtins = Vec::with_capacity(rows.len());
    for (type_id, builtin, directed) in rows {
        let builtin = BuiltinRelation::parse(&builtin)
            .ok_or_else(|| StoreError::Corrupt(format!("builtin relation {builtin:?}")))?;
        builtins.push((builtin, (id(&type_id)?, directed)));
    }
    let mut types = HashMap::new();
    for t in &export.relation_types {
        let here = match t.builtin {
            Some(builtin) => builtins
                .iter()
                .find(|(b, _)| *b == builtin)
                .map(|(_, here)| *here)
                .ok_or(StoreError::NotFound("built-in relation type"))?,
            None => {
                let input = hyakume_core::api::CreateRelationType {
                    name: t.name.clone(),
                    directed: t.directed,
                    reverse_name: t.reverse_name.clone(),
                };
                let custom = relation_type(t.id, workspace, input)?;
                insert_relation_type(tx, &custom).await?;
                (t.id, t.directed)
            }
        };
        types.insert(t.id, here);
    }
    Ok(types)
}

/// One project with its stages, tasks, and comments. Adds its task ids to
/// `tasks`, which links and positions are checked against.
async fn import_project(
    tx: &mut Transaction<'static, Sqlite>,
    workspace: WorkspaceId,
    identity: IdentityId,
    exported: &ExportedProject,
    tasks: &mut HashSet<TaskId>,
) -> Result<(), StoreError> {
    let project = &exported.project;
    let name = project.name.trim();
    if name.is_empty() {
        return Err(StoreError::invalid("project name must not be empty"));
    }
    check_text("project name", name, false)?;
    let key = project.key.as_str();
    // Numbers already used stay used, even if the file's counter says less.
    let highest = exported.tasks.iter().map(|t| t.number).max().unwrap_or(0);
    let next = highest
        .checked_add(1)
        .ok_or_else(|| StoreError::invalid(format!("a task in {key} has a number too large")))?
        .max(exported.next_task_number);
    sqlx::query(
        "INSERT INTO projects (id, workspace_id, key, name, next_task_number, created_at) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(project.id.to_string())
    .bind(workspace.to_string())
    .bind(key)
    .bind(name)
    .bind(next)
    .bind(now_ms())
    .execute(&mut **tx)
    .await
    .map_err(|e| unique_violation(e, || format!("the file has project {key} twice")))?;

    let mut stages: HashSet<StageId> = HashSet::new();
    for stage in &exported.stages {
        let stage_name = stage.name.trim();
        if stage_name.is_empty() {
            return Err(StoreError::invalid("stage name must not be empty"));
        }
        check_text("stage name", stage_name, false)?;
        sqlx::query(
            "INSERT INTO stages (id, project_id, name, position, finished) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(stage.id.to_string())
        .bind(project.id.to_string())
        .bind(stage_name)
        .bind(stage.position)
        .bind(stage.finished)
        .execute(&mut **tx)
        .await
        .map_err(|e| unique_violation(e, || format!("{key} has stage {stage_name:?} twice")))?;
        stages.insert(stage.id);
    }

    let me = identity.to_string();
    let mut own = HashSet::new();
    for task in &exported.tasks {
        let mut fields = task.fields.clone().normalized()?;
        if !stages.contains(&fields.stage_id) {
            return Err(StoreError::invalid(format!(
                "a task in {key} is in a stage the project doesn't have"
            )));
        }
        if task.number < 1 || task.version < 1 {
            return Err(StoreError::invalid(format!(
                "a task in {key} has a number or version below 1"
            )));
        }
        fields.assignee = fields.assignee.map(|_| identity);
        sqlx::query(
            "INSERT INTO tasks (id, project_id, number, title, description, stage_id, assignee_id, \
             labels, priority, due_date, version, created_at, updated_at, created_by, updated_by) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(task.id.to_string())
        .bind(project.id.to_string())
        .bind(task.number)
        .bind(&fields.title)
        .bind(&fields.description)
        .bind(fields.stage_id.to_string())
        .bind(fields.assignee.map(|a| a.to_string()))
        .bind(labels_json(&fields.labels))
        .bind(fields.priority.map(|p| p.as_str()))
        .bind(fields.due_date.map(date_to_text))
        .bind(task.version)
        .bind(millis(task.created_at))
        .bind(millis(task.updated_at))
        .bind(&me)
        .bind(&me)
        .execute(&mut **tx)
        .await
        .map_err(|e| {
            unique_violation(e, || format!("{key}-{} is in the file twice", task.number))
        })?;
        tasks.insert(task.id);
        own.insert(task.id);
    }

    for comment in &exported.comments {
        let body = comment.body.trim();
        if body.is_empty() {
            return Err(StoreError::invalid("comment must not be empty"));
        }
        check_text("comment", body, true)?;
        if !own.contains(&comment.task_id) {
            return Err(StoreError::invalid(format!(
                "a comment in {key} is on a task the project doesn't hold"
            )));
        }
        sqlx::query(
            "INSERT INTO comments (id, task_id, author_id, body, created_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(comment.id.to_string())
        .bind(comment.task_id.to_string())
        .bind(comment.author.map(|_| me.clone()))
        .bind(body)
        .bind(millis(comment.created_at))
        .execute(&mut **tx)
        .await
        .map_err(|e| unique_violation(e, || "the file lists one comment twice".into()))?;
    }
    Ok(())
}
