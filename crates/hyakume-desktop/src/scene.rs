//! What the canvas shows, rebuilt from every source after each update. No
//! positions and no GPUI here: `layout` places things, `canvas` draws them.

use std::collections::HashMap;
use std::ops::Range;

use hyakume_core::blocking::unfinished_blockers;
use hyakume_core::ids::{IdentityId, TaskId};
use hyakume_core::model::{BuiltinRelation, Stage, Task, name_key};
use uuid::Uuid;

use crate::sources::{SourceKey, SourceState, Status, WorkspaceData};

/// A node's identity across refreshes. Ids are only unique per server, and
/// one server can be configured twice under two names.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NodeKey {
    pub source: SourceKey,
    pub id: Uuid,
}

impl NodeKey {
    fn new(source: &SourceKey, id: Uuid) -> Self {
        Self {
            source: source.clone(),
            id,
        }
    }
}

/// The eye drawn for a stage. The stage name is always printed next to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    Backlog,
    Todo,
    Doing,
    Review,
    Done,
}

#[derive(Default)]
pub struct Scene {
    pub columns: Vec<Column>,
    pub clusters: Vec<Cluster>,
    pub eyes: Vec<Eye>,
    pub threads: Vec<Thread>,
    eye_index: HashMap<NodeKey, usize>,
    cluster_index: HashMap<NodeKey, usize>,
}

/// One workspace, or a source that has none to show yet.
pub struct Column {
    /// Source plus workspace id; the nil id for a source without workspaces.
    pub key: NodeKey,
    pub source: SourceKey,
    /// "Local", or the server's name.
    pub chip: String,
    pub title: String,
    pub address: String,
    pub status: Status,
    pub clusters: Range<usize>,
    pub people: HashMap<IdentityId, String>,
}

pub struct Cluster {
    pub key: NodeKey,
    pub column: usize,
    pub project_key: String,
    pub name: String,
    pub eyes: Range<usize>,
    pub blocked: usize,
}

pub struct Eye {
    pub key: NodeKey,
    pub cluster: usize,
    pub task: Task,
    pub stage: String,
    pub shape: Shape,
    pub blocked_by: Vec<Blocker>,
}

pub struct Blocker {
    pub reference: String,
    pub title: String,
    /// The blocker's eye, when its project is loaded too.
    pub eye: Option<usize>,
}

pub struct Thread {
    pub from: usize,
    pub to: usize,
    pub kind: ThreadKind,
    pub name: String,
    /// How the link reads from the `to` end; undirected links read the same.
    pub reverse: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadKind {
    Blocks,
    Directed,
    Undirected,
}

impl Scene {
    pub fn build(sources: &[SourceState]) -> Self {
        let mut scene = Self::default();
        for source in sources {
            if source.workspaces.is_empty() {
                scene.columns.push(Column {
                    key: NodeKey::new(&source.key, Uuid::nil()),
                    source: source.key.clone(),
                    chip: chip(&source.key),
                    title: match &source.key {
                        SourceKey::Local => "Personal".into(),
                        SourceKey::Server(name) => name.to_string(),
                    },
                    address: source.address.clone(),
                    status: source.status.clone(),
                    clusters: 0..0,
                    people: HashMap::new(),
                });
            }
            for workspace in &source.workspaces {
                scene.add_workspace(source, workspace);
            }
        }
        scene
    }

    fn add_workspace(&mut self, source: &SourceState, data: &WorkspaceData) {
        let key = &source.key;
        let column = self.columns.len();
        let first_cluster = self.clusters.len();
        let first_eye = self.eyes.len();

        for snapshot in &data.snapshots {
            let mut stages: Vec<&Stage> = snapshot.stages.iter().collect();
            stages.sort_by_key(|s| s.position);
            let start = self.eyes.len();
            let mut tasks: Vec<&Task> = snapshot.tasks.iter().collect();
            tasks.sort_by_key(|t| t.number);
            for task in tasks {
                let stage = stages.iter().find(|s| s.id == task.fields.stage_id);
                self.eyes.push(Eye {
                    key: NodeKey::new(key, task.id.0),
                    cluster: self.clusters.len(),
                    task: task.clone(),
                    stage: stage.map_or_else(|| "Unknown stage".into(), |s| s.name.clone()),
                    shape: stage.map_or(Shape::Todo, |s| shape_for(&stages, s)),
                    blocked_by: Vec::new(),
                });
            }
            self.clusters.push(Cluster {
                key: NodeKey::new(key, snapshot.project.id.0),
                column,
                project_key: snapshot.project.key.to_string(),
                name: snapshot.project.name.clone(),
                eyes: start..self.eyes.len(),
                blocked: 0,
            });
        }

        let by_task: HashMap<TaskId, usize> = (first_eye..self.eyes.len())
            .map(|i| (self.eyes[i].task.id, i))
            .collect();
        let finished_stages: Vec<_> = data
            .snapshots
            .iter()
            .flat_map(|s| &s.stages)
            .filter(|s| s.finished)
            .map(|s| s.id)
            .collect();
        let stubs: HashMap<TaskId, _> = data
            .snapshots
            .iter()
            .flat_map(|s| &s.linked_tasks)
            .map(|stub| (stub.id, stub))
            .collect();
        // A cross-project link shows up in both projects' snapshots.
        let mut relations: Vec<_> = data.snapshots.iter().flat_map(|s| &s.relations).collect();
        relations.sort_by_key(|r| r.id);
        relations.dedup_by_key(|r| r.id);
        let relations: Vec<_> = relations.into_iter().cloned().collect();

        let is_finished = |task: TaskId| match by_task.get(&task) {
            Some(&i) => Some(finished_stages.contains(&self.eyes[i].task.fields.stage_id)),
            None => stubs.get(&task).map(|s| s.finished),
        };
        let blocks_type = data
            .summary
            .relation_types
            .iter()
            .find(|t| t.builtin == Some(BuiltinRelation::Blocks))
            .map(|t| t.id);
        let mut blocked: Vec<(usize, Vec<Blocker>)> = Vec::new();
        if let Some(blocks_type) = blocks_type {
            for i in first_eye..self.eyes.len() {
                let found =
                    unfinished_blockers(self.eyes[i].task.id, &relations, blocks_type, is_finished);
                let blockers: Vec<Blocker> = found
                    .into_iter()
                    .map(|id| match by_task.get(&id) {
                        Some(&b) => Blocker {
                            reference: self.eyes[b].task.reference.clone(),
                            title: self.eyes[b].task.fields.title.clone(),
                            eye: Some(b),
                        },
                        None => Blocker {
                            reference: stubs.get(&id).map_or("?".into(), |s| s.reference.clone()),
                            title: stubs.get(&id).map_or(String::new(), |s| s.title.clone()),
                            eye: None,
                        },
                    })
                    .collect();
                if !blockers.is_empty() {
                    blocked.push((i, blockers));
                }
            }
        }
        for (i, blockers) in blocked {
            self.eyes[i].blocked_by = blockers;
            let cluster = self.eyes[i].cluster;
            self.clusters[cluster].blocked += 1;
        }

        let types: HashMap<_, _> = data
            .summary
            .relation_types
            .iter()
            .map(|t| (t.id, t))
            .collect();
        for relation in &relations {
            let (Some(&from), Some(&to), Some(kind)) = (
                by_task.get(&relation.from),
                by_task.get(&relation.to),
                types.get(&relation.type_id),
            ) else {
                continue;
            };
            self.threads.push(Thread {
                from,
                to,
                kind: match (kind.builtin, kind.directed) {
                    (Some(BuiltinRelation::Blocks), _) => ThreadKind::Blocks,
                    (_, true) => ThreadKind::Directed,
                    (_, false) => ThreadKind::Undirected,
                },
                name: kind.name.clone(),
                reverse: kind
                    .reverse_name
                    .clone()
                    .unwrap_or_else(|| kind.name.clone()),
            });
        }

        self.columns.push(Column {
            key: NodeKey::new(key, data.summary.workspace.id.0),
            source: key.clone(),
            chip: chip(key),
            title: data.summary.workspace.name.clone(),
            address: source.address.clone(),
            status: source.status.clone(),
            clusters: first_cluster..self.clusters.len(),
            people: data
                .summary
                .members
                .iter()
                .map(|m| (m.identity.id, m.identity.display_name.clone()))
                .collect(),
        });
        for i in first_eye..self.eyes.len() {
            self.eye_index.insert(self.eyes[i].key.clone(), i);
        }
        for i in first_cluster..self.clusters.len() {
            self.cluster_index.insert(self.clusters[i].key.clone(), i);
        }
    }

    pub fn eye(&self, key: &NodeKey) -> Option<usize> {
        self.eye_index.get(key).copied()
    }

    pub fn cluster(&self, key: &NodeKey) -> Option<usize> {
        self.cluster_index.get(key).copied()
    }

    /// Threads from other projects that end in this cluster: the other end is
    /// drawn as a stub when the project is focused on its own.
    pub fn outside_ends(&self, cluster: usize) -> Vec<usize> {
        let inside = |eye: usize| self.eyes[eye].cluster == cluster;
        let mut ends: Vec<usize> = self
            .threads
            .iter()
            .filter_map(|t| match (inside(t.from), inside(t.to)) {
                (true, false) => Some(t.to),
                (false, true) => Some(t.from),
                _ => None,
            })
            .collect();
        ends.sort_unstable();
        ends.dedup();
        ends
    }
}

fn chip(key: &SourceKey) -> String {
    match key {
        SourceKey::Local => "Local".into(),
        SourceKey::Server(name) => name.to_string(),
    }
}

/// Default stage names map to their own eye. A custom stage takes the eye
/// for its place in the workflow, and finished stages are always closed.
pub fn shape_for(stages: &[&Stage], stage: &Stage) -> Shape {
    if stage.finished {
        return Shape::Done;
    }
    match name_key(&stage.name).as_str() {
        "backlog" => return Shape::Backlog,
        "to do" | "todo" => return Shape::Todo,
        "in progress" | "doing" => return Shape::Doing,
        "review" | "in review" => return Shape::Review,
        _ => {}
    }
    let open: Vec<_> = stages.iter().filter(|s| !s.finished).collect();
    let Some(at) = open.iter().position(|s| s.id == stage.id) else {
        return Shape::Todo;
    };
    if open.len() < 2 {
        return Shape::Todo;
    }
    const OPEN: [Shape; 4] = [Shape::Backlog, Shape::Todo, Shape::Doing, Shape::Review];
    let scaled = (at * (OPEN.len() - 1) + (open.len() - 1) / 2) / (open.len() - 1);
    OPEN[scaled.min(OPEN.len() - 1)]
}

#[cfg(test)]
pub(crate) mod tests {
    use hyakume_core::api::{ProjectSnapshot, TaskStub, WorkspaceSummary};
    use hyakume_core::ids::{ProjectId, RelationId, RelationTypeId, StageId, WorkspaceId};
    use hyakume_core::model::{
        DEFAULT_STAGES, Project, Relation, RelationType, TaskFields, Workspace,
    };
    use hyakume_core::permission::Access;
    use time::OffsetDateTime;

    use super::*;

    /// A tiny workspace builder, so tests read as "WEB has these tasks".
    pub struct Fixture {
        pub workspace: Workspace,
        pub types: Vec<RelationType>,
        pub snapshots: Vec<ProjectSnapshot>,
    }

    impl Fixture {
        pub fn new() -> Self {
            let workspace = Workspace {
                id: WorkspaceId::new(),
                slug: "acme".into(),
                name: "Acme".into(),
            };
            let types = vec![
                relation_type(
                    &workspace,
                    "blocks",
                    Some("blocked by"),
                    Some(BuiltinRelation::Blocks),
                ),
                relation_type(
                    &workspace,
                    "related to",
                    None,
                    Some(BuiltinRelation::RelatedTo),
                ),
            ];
            Self {
                workspace,
                types,
                snapshots: Vec::new(),
            }
        }

        pub fn project(&mut self, key: &str, tasks: &[(&str, usize)]) -> Vec<TaskId> {
            let project = Project {
                id: ProjectId::new(),
                workspace_id: self.workspace.id,
                key: key.parse().unwrap(),
                name: key.to_lowercase(),
            };
            let stages: Vec<Stage> = DEFAULT_STAGES
                .iter()
                .enumerate()
                .map(|(i, (name, finished))| Stage {
                    id: StageId::new(),
                    project_id: project.id,
                    name: (*name).into(),
                    position: i as i64,
                    finished: *finished,
                })
                .collect();
            self.snapshots.push(ProjectSnapshot {
                project,
                stages,
                tasks: Vec::new(),
                relations: Vec::new(),
                linked_tasks: Vec::new(),
            });
            let at = self.snapshots.len() - 1;
            tasks
                .iter()
                .map(|(title, stage)| self.add_task(at, title, *stage))
                .collect()
        }

        /// Adds a task to the `project`-th project, in the `stage`-th stage.
        pub fn add_task(&mut self, project: usize, title: &str, stage: usize) -> TaskId {
            let snapshot = &mut self.snapshots[project];
            let number = snapshot.tasks.len() as i64 + 1;
            let task = task(&snapshot.project, number, title, snapshot.stages[stage].id);
            let id = task.id;
            snapshot.tasks.push(task);
            id
        }

        /// Adds the relation to each snapshot it touches, the way the server does.
        pub fn link(&mut self, kind: usize, from: TaskId, to: TaskId) {
            let relation = Relation {
                id: RelationId::new(),
                workspace_id: self.workspace.id,
                type_id: self.types[kind].id,
                from,
                to,
            };
            let all: Vec<Task> = self
                .snapshots
                .iter()
                .flat_map(|s| s.tasks.clone())
                .collect();
            let finished: Vec<StageId> = self
                .snapshots
                .iter()
                .flat_map(|s| &s.stages)
                .filter(|s| s.finished)
                .map(|s| s.id)
                .collect();
            for snapshot in &mut self.snapshots {
                let has = |id| snapshot.tasks.iter().any(|t| t.id == id);
                if !has(from) && !has(to) {
                    continue;
                }
                snapshot.relations.push(relation.clone());
                for other in [from, to].into_iter().filter(|id| !has(*id)) {
                    let t = all.iter().find(|t| t.id == other).unwrap();
                    snapshot.linked_tasks.push(TaskStub {
                        id: t.id,
                        project_id: t.project_id,
                        reference: t.reference.clone(),
                        title: t.fields.title.clone(),
                        finished: finished.contains(&t.fields.stage_id),
                    });
                }
            }
        }

        pub fn source(&self) -> SourceState {
            SourceState {
                key: SourceKey::Server("a".into()),
                address: "127.0.0.1:7878".into(),
                status: Status::Live,
                workspaces: vec![WorkspaceData {
                    summary: WorkspaceSummary {
                        workspace: self.workspace.clone(),
                        access: Access::local_owner(),
                        projects: self.snapshots.iter().map(|s| s.project.clone()).collect(),
                        relation_types: self.types.clone(),
                        roles: Vec::new(),
                        members: Vec::new(),
                    },
                    snapshots: self.snapshots.clone(),
                }],
            }
        }
    }

    fn relation_type(
        ws: &Workspace,
        name: &str,
        reverse: Option<&str>,
        builtin: Option<BuiltinRelation>,
    ) -> RelationType {
        RelationType {
            id: RelationTypeId::new(),
            workspace_id: ws.id,
            name: name.into(),
            reverse_name: reverse.map(Into::into),
            directed: reverse.is_some(),
            builtin,
        }
    }

    fn task(project: &Project, number: i64, title: &str, stage: StageId) -> Task {
        Task {
            id: TaskId::new(),
            project_id: project.id,
            number,
            reference: format!("{}-{number}", project.key),
            fields: TaskFields {
                title: title.into(),
                description: String::new(),
                stage_id: stage,
                assignee: None,
                labels: Vec::new(),
                priority: None,
                due_date: None,
            },
            version: 1,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    const IN_PROGRESS: usize = 2;
    const REVIEW: usize = 3;
    const DONE: usize = 4;

    #[test]
    fn a_blocker_in_another_project_blocks_until_its_stage_is_finished() {
        let mut ws = Fixture::new();
        let [landing] = ws.project("WEB", &[("Landing page", IN_PROGRESS)])[..] else {
            unreachable!()
        };
        let [signup] = ws.project("API", &[("Signup endpoint", REVIEW)])[..] else {
            unreachable!()
        };
        ws.link(0, signup, landing);

        let scene = Scene::build(&[ws.source()]);
        let eye = &scene.eyes[0];
        assert_eq!(eye.task.reference, "WEB-1");
        let refs: Vec<_> = eye
            .blocked_by
            .iter()
            .map(|b| b.reference.as_str())
            .collect();
        assert_eq!(refs, ["API-1"]);
        assert_eq!(scene.clusters[0].blocked, 1);

        // Finishing the blocker in API clears WEB-1, without touching WEB-1 itself.
        let api = &mut ws.snapshots[1];
        api.tasks[0].fields.stage_id = api.stages[DONE].id;
        let scene = Scene::build(&[ws.source()]);
        assert!(scene.eyes[0].blocked_by.is_empty());
        assert_eq!(scene.clusters[0].blocked, 0);
    }

    #[test]
    fn a_link_between_two_projects_is_one_thread() {
        let mut ws = Fixture::new();
        let web = ws.project("WEB", &[("Landing page", 0)]);
        let api = ws.project("API", &[("Signup endpoint", 0)]);
        ws.link(1, web[0], api[0]);

        let scene = Scene::build(&[ws.source()]);
        assert_eq!(scene.threads.len(), 1);
        assert_eq!(scene.threads[0].kind, ThreadKind::Undirected);
        assert_eq!(scene.outside_ends(0), [1], "API-1 is WEB's stub");
    }

    #[test]
    fn custom_stages_take_the_eye_for_their_place_in_the_workflow() {
        let stage = |name: &str, position, finished| Stage {
            id: StageId::new(),
            project_id: ProjectId::new(),
            name: name.into(),
            position,
            finished,
        };
        let stages = [
            stage("Ideas", 0, false),
            stage("Ready", 1, false),
            stage("Building", 2, false),
            stage("QA", 3, false),
            stage("Shipped", 4, true),
        ];
        let refs: Vec<&Stage> = stages.iter().collect();
        let shapes: Vec<Shape> = stages.iter().map(|s| shape_for(&refs, s)).collect();
        assert_eq!(
            shapes,
            [
                Shape::Backlog,
                Shape::Todo,
                Shape::Doing,
                Shape::Review,
                Shape::Done
            ]
        );
    }
}
