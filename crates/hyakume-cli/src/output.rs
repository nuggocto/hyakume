//! Text and JSON views of tasks. JSON field names are a stable contract for
//! scripts and agents; text output may change freely.

use std::collections::HashMap;

use hyakume_client::ClientError;
use hyakume_core::api::{ProjectSnapshot, WorkspaceSummary};
use hyakume_core::blocking::unfinished_blockers;
use hyakume_core::ids::{IdentityId, TaskId};
use hyakume_core::model::{BuiltinRelation, Comment, Task};
use serde::Serialize;

pub struct Output {
    pub json: bool,
}

impl Output {
    /// Prints `value` as JSON, or `text()` for people.
    pub fn show<T: Serialize>(&self, value: &T, text: impl FnOnce() -> String) {
        if self.json {
            match serde_json::to_string_pretty(value) {
                Ok(json) => println!("{json}"),
                Err(err) => eprintln!("error: cannot encode output: {err}"),
            }
        } else {
            let text = text();
            if !text.is_empty() {
                println!("{}", text.trim_end());
            }
        }
    }
}

#[derive(Serialize)]
pub struct TaskView {
    #[serde(flatten)]
    pub task: Task,
    pub project: String,
    pub stage: String,
    /// True when the task's stage counts as finished.
    pub finished: bool,
    pub assignee_name: Option<String>,
    /// References of unfinished tasks that block this one.
    pub blocked_by: Vec<String>,
}

#[derive(Serialize)]
pub struct RelationView {
    pub id: String,
    /// The relation as read from the shown task, such as "blocks" or "blocked by".
    pub relation: String,
    pub task: String,
    pub title: String,
}

#[derive(Serialize)]
pub struct CommentView {
    pub author: Option<String>,
    pub body: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
}

#[derive(Serialize)]
pub struct TaskDetailView {
    #[serde(flatten)]
    pub view: TaskView,
    pub relations: Vec<RelationView>,
    pub comments: Vec<CommentView>,
}

/// Lookups for one project snapshot, so many tasks can be described cheaply.
pub struct ProjectContext<'a> {
    summary: &'a WorkspaceSummary,
    snapshot: &'a ProjectSnapshot,
    members: HashMap<IdentityId, &'a str>,
    /// Reference, title, and finished state for own and linked tasks.
    tasks: HashMap<TaskId, (&'a str, &'a str, bool)>,
}

impl<'a> ProjectContext<'a> {
    pub fn new(summary: &'a WorkspaceSummary, snapshot: &'a ProjectSnapshot) -> Self {
        let finished: HashMap<_, _> = snapshot.stages.iter().map(|s| (s.id, s.finished)).collect();
        let mut tasks = HashMap::new();
        for task in &snapshot.tasks {
            let done = finished
                .get(&task.fields.stage_id)
                .copied()
                .unwrap_or(false);
            tasks.insert(
                task.id,
                (task.reference.as_str(), task.fields.title.as_str(), done),
            );
        }
        for stub in &snapshot.linked_tasks {
            tasks.insert(
                stub.id,
                (stub.reference.as_str(), stub.title.as_str(), stub.finished),
            );
        }
        Self {
            summary,
            snapshot,
            members: summary
                .members
                .iter()
                .map(|m| (m.identity.id, m.identity.name.as_str()))
                .collect(),
            tasks,
        }
    }

    pub fn member_name(&self, id: IdentityId) -> Option<String> {
        self.members.get(&id).map(|n| (*n).to_owned())
    }

    pub fn task_view(&self, task: &Task) -> TaskView {
        let stage = self
            .snapshot
            .stages
            .iter()
            .find(|s| s.id == task.fields.stage_id);
        let blocks = self
            .summary
            .relation_types
            .iter()
            .find(|t| t.builtin == Some(BuiltinRelation::Blocks));
        let blocked_by = match blocks {
            Some(blocks) => {
                unfinished_blockers(task.id, &self.snapshot.relations, blocks.id, |id| {
                    self.tasks.get(&id).map(|(_, _, done)| *done)
                })
                .into_iter()
                .map(|id| {
                    self.tasks
                        .get(&id)
                        .map_or_else(|| id.to_string(), |t| t.0.to_owned())
                })
                .collect()
            }
            None => Vec::new(),
        };
        TaskView {
            task: task.clone(),
            project: self.snapshot.project.key.to_string(),
            stage: stage.map_or_else(|| "?".to_owned(), |s| s.name.clone()),
            finished: stage.is_some_and(|s| s.finished),
            assignee_name: task.fields.assignee.and_then(|a| self.member_name(a)),
            blocked_by,
        }
    }

    pub fn relations_of(&self, task: TaskId) -> Vec<RelationView> {
        let mut views = Vec::new();
        for relation in &self.snapshot.relations {
            let (other, forward) = if relation.from == task {
                (relation.to, true)
            } else if relation.to == task {
                (relation.from, false)
            } else {
                continue;
            };
            let Some(kind) = self
                .summary
                .relation_types
                .iter()
                .find(|t| t.id == relation.type_id)
            else {
                continue;
            };
            let label = match (&kind.reverse_name, forward) {
                (Some(reverse), false) => reverse.clone(),
                _ => kind.name.clone(),
            };
            let (reference, title) = self
                .tasks
                .get(&other)
                .map_or(("?", ""), |(r, t, _)| (*r, *t));
            views.push(RelationView {
                id: relation.id.to_string(),
                relation: label,
                task: reference.to_owned(),
                title: title.to_owned(),
            });
        }
        views
    }

    pub fn comment_view(&self, comment: &Comment) -> CommentView {
        CommentView {
            author: comment.author.and_then(|a| self.member_name(a)),
            body: comment.body.clone(),
            created_at: comment.created_at,
        }
    }
}

pub fn task_line(view: &TaskView) -> String {
    let mut line = format!(
        "{:<9} {:<13} {}",
        view.task.reference, view.stage, view.task.fields.title
    );
    if let Some(name) = &view.assignee_name {
        line.push_str(&format!("  @{name}"));
    }
    if !view.blocked_by.is_empty() {
        line.push_str(&format!("  [blocked by {}]", view.blocked_by.join(", ")));
    }
    line
}

pub fn task_detail_text(detail: &TaskDetailView) -> String {
    let view = &detail.view;
    let fields = &view.task.fields;
    let mut out = format!("{}  {}\n", view.task.reference, fields.title);
    let mut row = |label: &str, value: String| out.push_str(&format!("  {label:<11}{value}\n"));
    row(
        "Stage",
        format!(
            "{}{}",
            view.stage,
            if view.finished { " (finished)" } else { "" }
        ),
    );
    row(
        "Assignee",
        view.assignee_name.clone().unwrap_or_else(|| "-".into()),
    );
    if !fields.labels.is_empty() {
        row("Labels", fields.labels.join(", "));
    }
    if let Some(priority) = fields.priority {
        row("Priority", priority.as_str().into());
    }
    if let Some(due) = fields.due_date {
        row("Due", due.to_string());
    }
    row("Version", view.task.version.to_string());
    if !view.blocked_by.is_empty() {
        row("Blocked by", view.blocked_by.join(", "));
    }
    if !fields.description.is_empty() {
        out.push_str(&format!("\n{}\n", fields.description));
    }
    if !detail.relations.is_empty() {
        out.push_str("\nRelations\n");
        for r in &detail.relations {
            out.push_str(&format!("  {} {}  {}\n", r.relation, r.task, r.title));
        }
    }
    if !detail.comments.is_empty() {
        out.push_str("\nComments\n");
        for c in &detail.comments {
            out.push_str(&format!(
                "  {} ({}): {}\n",
                c.author.as_deref().unwrap_or("unknown"),
                c.created_at.date(),
                c.body
            ));
        }
    }
    out
}

/// Exit codes are part of the CLI contract, listed in `hyakume --help`.
pub fn exit_code(err: &ClientError) -> u8 {
    match err {
        ClientError::Invalid(_) => 2,
        ClientError::NotFound(_) => 3,
        ClientError::Forbidden(_) => 4,
        ClientError::Conflict { .. } => 5,
        ClientError::Duplicate(_) => 6,
        ClientError::Unreachable { .. } => 7,
        ClientError::Unauthorized(_) => 8,
        ClientError::Credentials(_) | ClientError::Config(_) | ClientError::Internal(_) => 1,
    }
}

pub fn error_kind(err: &ClientError) -> &'static str {
    match err {
        ClientError::Invalid(_) => "invalid",
        ClientError::NotFound(_) => "not_found",
        ClientError::Forbidden(_) => "forbidden",
        ClientError::Conflict { .. } => "conflict",
        ClientError::Duplicate(_) => "duplicate",
        ClientError::Unreachable { .. } => "unreachable",
        ClientError::Unauthorized(_) => "unauthorized",
        ClientError::Credentials(_) => "credentials",
        ClientError::Config(_) => "config",
        ClientError::Internal(_) => "internal",
    }
}
