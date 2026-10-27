//! The file `hyakume local export` writes and `hyakume local import` reads:
//! all of a person's local work and their positions on its canvas, so it
//! can move to another computer. Local work has one person, so the file
//! names nobody; whoever imports it owns everything in it.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::api::LayoutNode;
use crate::model::{Comment, Project, Relation, RelationType, Stage, Task};

/// Bumped when the shape changes. An import refuses a format it doesn't know
/// rather than guess at fields it can't read.
pub const FORMAT: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LocalExport {
    pub format: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub exported_at: OffsetDateTime,
    /// Built-in types too, so links of those types know what they are.
    pub relation_types: Vec<RelationType>,
    pub projects: Vec<ExportedProject>,
    pub relations: Vec<Relation>,
    pub layout: Vec<LayoutNode>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExportedProject {
    pub project: Project,
    /// Kept so a task made after the import never reuses the number of
    /// one deleted before the export.
    pub next_task_number: i64,
    pub stages: Vec<Stage>,
    pub tasks: Vec<Task>,
    pub comments: Vec<Comment>,
}

impl LocalExport {
    pub fn task_count(&self) -> usize {
        self.projects.iter().map(|p| p.tasks.len()).sum()
    }

    pub fn comment_count(&self) -> usize {
        self.projects.iter().map(|p| p.comments.len()).sum()
    }
}
