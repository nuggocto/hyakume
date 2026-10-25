use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use time::{Date, OffsetDateTime};

use crate::ids::{
    CommentId, IdentityId, ProjectId, RelationId, RelationTypeId, StageId, TaskId, WorkspaceId,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub slug: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: ProjectId,
    pub workspace_id: WorkspaceId,
    pub key: ProjectKey,
    pub name: String,
}

/// Short uppercase prefix used in task references such as `WEB-12`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ProjectKey(String);

impl ProjectKey {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for ProjectKey {
    type Err = ValidationError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let key = s.trim().to_ascii_uppercase();
        let valid_len = (2..=10).contains(&key.len());
        let starts_with_letter = key.starts_with(|c: char| c.is_ascii_uppercase());
        let alphanumeric = key
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
        if valid_len && starts_with_letter && alphanumeric {
            Ok(Self(key))
        } else {
            Err(ValidationError::ProjectKey(s.to_owned()))
        }
    }
}

impl TryFrom<String> for ProjectKey {
    type Error = ValidationError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<ProjectKey> for String {
    fn from(key: ProjectKey) -> Self {
        key.0
    }
}

impl fmt::Display for ProjectKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Human-facing task identifier, stable for the life of the project key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TaskRef {
    pub project: ProjectKey,
    pub number: i64,
}

impl FromStr for TaskRef {
    type Err = ValidationError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let invalid = || ValidationError::TaskRef(s.to_owned());
        let (key, number) = s.trim().rsplit_once('-').ok_or_else(invalid)?;
        let number: i64 = number.parse().map_err(|_| invalid())?;
        if number < 1 {
            return Err(invalid());
        }
        let project = key.parse().map_err(|_| invalid())?;
        Ok(Self { project, number })
    }
}

impl fmt::Display for TaskRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.project, self.number)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stage {
    pub id: StageId,
    pub project_id: ProjectId,
    pub name: String,
    pub position: i64,
    /// Tasks in a finished stage stop blocking dependent tasks.
    pub finished: bool,
}

/// Name and "counts as finished" flag for each stage a new project starts with.
pub const DEFAULT_STAGES: [(&str, bool); 5] = [
    ("Backlog", false),
    ("To do", false),
    ("In progress", false),
    ("Review", false),
    ("Done", true),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Low,
    Medium,
    High,
    Urgent,
}

impl Priority {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Urgent => "urgent",
        }
    }
}

impl FromStr for Priority {
    type Err = ValidationError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "urgent" => Ok(Self::Urgent),
            _ => Err(ValidationError::Priority(s.to_owned())),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub project_id: ProjectId,
    pub number: i64,
    /// `KEY-number`, denormalized so clients can print it without the project.
    pub reference: String,
    #[serde(flatten)]
    pub fields: TaskFields,
    /// Incremented on every saved change; updates must name the version they edited.
    pub version: i64,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

/// The editable part of a task. Saves replace all of it at once.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskFields {
    pub title: String,
    pub description: String,
    pub stage_id: StageId,
    pub assignee: Option<IdentityId>,
    pub labels: Vec<String>,
    pub priority: Option<Priority>,
    pub due_date: Option<Date>,
}

impl TaskFields {
    /// Trims text, drops empty or duplicate labels, and rejects an empty title.
    pub fn normalized(mut self) -> Result<Self, ValidationError> {
        self.title = self.title.trim().to_owned();
        if self.title.is_empty() {
            return Err(ValidationError::EmptyTitle);
        }
        if self.title.chars().count() > MAX_TITLE_CHARS {
            return Err(ValidationError::TitleTooLong);
        }
        check_text("title", &self.title, false)?;
        check_text("description", &self.description, true)?;
        let mut labels: Vec<String> = Vec::with_capacity(self.labels.len());
        for label in self.labels {
            let label = label.trim().to_owned();
            check_text("label", &label, false)?;
            if !label.is_empty() && !labels.contains(&label) {
                labels.push(label);
            }
        }
        self.labels = labels;
        Ok(self)
    }
}

pub const MAX_TITLE_CHARS: usize = 300;

/// ESC, BEL, and friends let shared text drive whoever's terminal prints it,
/// and bidi overrides make text display in a different order than it reads.
pub fn is_unsafe_char(c: char) -> bool {
    c.is_control() || matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{061C}')
}

/// Rejects unsafe characters in `field`. Multiline text such as a description
/// may still hold newlines and tabs.
pub fn check_text(field: &'static str, text: &str, multiline: bool) -> Result<(), ValidationError> {
    let allowed = |c: char| multiline && matches!(c, '\n' | '\t');
    if text.chars().any(|c| is_unsafe_char(c) && !allowed(c)) {
        return Err(ValidationError::UnsafeText(field));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Comment {
    pub id: CommentId,
    pub task_id: TaskId,
    pub author: Option<IdentityId>,
    pub body: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuiltinRelation {
    Blocks,
    RelatedTo,
}

impl BuiltinRelation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Blocks => "blocks",
            Self::RelatedTo => "related_to",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "blocks" => Some(Self::Blocks),
            "related_to" => Some(Self::RelatedTo),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationType {
    pub id: RelationTypeId,
    pub workspace_id: WorkspaceId,
    pub name: String,
    /// Label read from the target's side, such as "blocked by". Only directed types have one.
    pub reverse_name: Option<String>,
    pub directed: bool,
    pub builtin: Option<BuiltinRelation>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Relation {
    pub id: RelationId,
    pub workspace_id: WorkspaceId,
    pub type_id: RelationTypeId,
    pub from: TaskId,
    pub to: TaskId,
}

/// Undirected relations are stored with the smaller id first so `A–B` and `B–A`
/// hit the same unique index.
pub fn relation_endpoints(directed: bool, from: TaskId, to: TaskId) -> (TaskId, TaskId) {
    if directed || from <= to {
        (from, to)
    } else {
        (to, from)
    }
}

/// How people type stage and relation names: case, `-`, `_`, and repeated
/// spaces don't matter. Names that share a key are treated as the same name.
pub fn name_key(name: &str) -> String {
    name.to_lowercase()
        .replace(['-', '_'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub id: IdentityId,
    pub name: String,
    pub display_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValidationError {
    #[error("task title must not be empty")]
    EmptyTitle,
    #[error("task title must be at most {MAX_TITLE_CHARS} characters")]
    TitleTooLong,
    #[error("project key {0:?} must be 2-10 letters or digits, starting with a letter")]
    ProjectKey(String),
    #[error("{0:?} is not a task reference like WEB-12")]
    TaskRef(String),
    #[error("{0:?} is not a priority (low, medium, high, urgent)")]
    Priority(String),
    #[error("{0} must not contain control or text-direction characters")]
    UnsafeText(&'static str),
    #[error("{0}")]
    Other(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(title: &str, description: &str, label: &str) -> TaskFields {
        TaskFields {
            title: title.into(),
            description: description.into(),
            stage_id: StageId::new(),
            assignee: None,
            labels: vec![label.into()],
            priority: None,
            due_date: None,
        }
    }

    #[test]
    fn task_text_cannot_carry_terminal_escapes() {
        // OSC 52 asks the terminal to overwrite the clipboard.
        let osc = "\u{1b}]52;c;cm0gLXJmIH4=\u{7}";
        for bad in [
            fields(&format!("Fix {osc}"), "", "ui"),
            fields("Fix", &format!("see\n{osc}"), "ui"),
            fields("Fix", "", &format!("ui{osc}")),
            fields("Fix \u{202E}gpj.exe", "", "ui"),
        ] {
            assert!(matches!(
                bad.normalized(),
                Err(ValidationError::UnsafeText(_))
            ));
        }
        let ok = fields("Fix", "line one\n\tline two", "ui").normalized();
        assert_eq!(ok.unwrap().description, "line one\n\tline two");
    }
}
