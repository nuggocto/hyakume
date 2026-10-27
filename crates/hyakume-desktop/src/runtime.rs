//! All network, keyring, and SQLite work runs on one Tokio thread. reqwest
//! and SQLx need Tokio and GPUI runs its own executor, so the window and this
//! thread only talk through two channels. Tokio's channels work on any executor.

use std::collections::HashMap;
use std::sync::Arc;
use std::thread::JoinHandle;

use hyakume_client::{Client, ClientError, DataDir};
use hyakume_core::api::{
    CreateRelation, CreateRelationType, CreateRole, CreateTask, Layout, LayoutNode, SetMemberRoles,
    TaskDetail, UpdateTask,
};
use hyakume_core::ids::{IdentityId, ProjectId, RelationId, RoleId, TaskId, WorkspaceId};
use hyakume_core::model::{Task, is_unsafe_char};
use time::OffsetDateTime;
use tokio::sync::{Notify, mpsc};

use crate::sources::{self, Backend, Backends, SourceKey, SourceState, Status};
use hyakume_storage::Cache;

pub enum Command {
    /// Reconnect or reload now instead of waiting for the next attempt.
    Retry(SourceKey),
    /// Fetch comments and the latest fields for the inspector.
    LoadDetail {
        source: SourceKey,
        task: TaskId,
        reference: String,
        /// Echoed back, so the window can drop answers it no longer wants.
        request: u64,
        /// The window shows the source offline: go straight to the copy
        /// rather than wait out a connect timeout first.
        offline: bool,
    },
    /// A change to work in `workspace`; the answer is `Update::Wrote`.
    Write {
        source: SourceKey,
        workspace: WorkspaceId,
        request: u64,
        write: Write,
    },
    /// This person's own positions. Failures come back as a notice only:
    /// the canvas already shows the new spot.
    SaveLayout {
        source: SourceKey,
        workspace: WorkspaceId,
        nodes: Vec<LayoutNode>,
    },
}

pub enum Write {
    /// Fails with `WriteError::Conflict` if the task moved past the
    /// version in `update`.
    SaveTask {
        task: TaskId,
        update: UpdateTask,
    },
    CreateTask {
        project: ProjectId,
        body: CreateTask,
    },
    /// Its links and comments go with it.
    DeleteTask(TaskId),
    Comment {
        task: TaskId,
        body: String,
    },
    Link(CreateRelation),
    Unlink(RelationId),
    CreateRelationType(CreateRelationType),
    CreateRole(CreateRole),
    /// Replaces every role `identity` has in the workspace.
    SetRoles {
        identity: IdentityId,
        roles: Vec<RoleId>,
    },
}

pub enum Wrote {
    /// The task as saved or created.
    Task(Box<Task>),
    Done,
}

pub enum WriteError {
    /// Someone saved first. This is the task as it is now.
    Conflict(Box<Task>),
    Failed(String),
}

pub struct Fetched {
    pub detail: TaskDetail,
    /// When the copy shown was saved, if the server couldn't be asked.
    pub saved_at: Option<OffsetDateTime>,
}

pub enum Update {
    Source(Box<SourceState>),
    Detail {
        source: SourceKey,
        task: TaskId,
        request: u64,
        detail: Result<Box<Fetched>, String>,
    },
    Wrote {
        source: SourceKey,
        request: u64,
        result: Result<Wrote, WriteError>,
    },
    /// Positions that didn't save, so their echo isn't waited for.
    LayoutUnsaved {
        source: SourceKey,
        nodes: Vec<LayoutNode>,
        reason: String,
    },
    /// The profile itself can't be read, so there is nothing to show.
    Fatal(String),
}

/// Dropping this closes the command channel, which ends the thread's main
/// loop and with it every source task.
pub struct Runtime {
    commands: mpsc::UnboundedSender<Command>,
    _thread: JoinHandle<()>,
}

impl Runtime {
    pub fn start() -> std::io::Result<(Self, mpsc::UnboundedReceiver<Update>)> {
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (update_tx, updates) = mpsc::unbounded_channel();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let thread = std::thread::Builder::new()
            .name("hyakume-io".into())
            .spawn(move || runtime.block_on(run(command_rx, update_tx)))?;
        Ok((
            Self {
                commands,
                _thread: thread,
            },
            updates,
        ))
    }

    pub fn send(&self, command: Command) {
        // Fails only once the thread is gone, and then there's nobody to tell.
        let _ = self.commands.send(command);
    }
}

async fn run(
    mut commands: mpsc::UnboundedReceiver<Command>,
    updates: mpsc::UnboundedSender<Update>,
) {
    let client = match DataDir::resolve().and_then(Client::load) {
        Ok(client) => client,
        Err(err) => {
            let _ = updates.send(Update::Fatal(err.to_string()));
            return;
        }
    };
    let backends = Backends::default();
    let opened = match client.data.create() {
        Ok(()) => hyakume_storage::Cache::open(&client.data.cache_db())
            .await
            .map_err(|e| e.to_string()),
        Err(err) => Err(err.to_string()),
    };
    let cache = match opened {
        Ok(cache) => Some(cache),
        Err(err) => {
            eprintln!("hyakume: no offline copies this run: {err}");
            None
        }
    };
    let mut retries = HashMap::new();
    let (state_tx, mut states) = mpsc::unbounded_channel();

    let retry = Arc::new(Notify::new());
    retries.insert(SourceKey::Local, retry.clone());
    tokio::spawn(sources::run_local(
        client.data.clone(),
        retry,
        backends.clone(),
        state_tx.clone(),
    ));
    for server in &client.config.servers {
        let name: Arc<str> = server.name.as_str().into();
        let retry = Arc::new(Notify::new());
        retries.insert(SourceKey::Server(name.clone()), retry.clone());
        tokio::spawn(sources::run_server(
            client.data.clone(),
            name,
            display_address(&server.url),
            retry,
            backends.clone(),
            cache.clone(),
            state_tx.clone(),
        ));
    }
    drop(state_tx);
    // Saves to one source go one at a time, in order: two in flight could
    // land swapped, and the server keeps whichever lands last.
    let mut layout_queues = HashMap::new();

    loop {
        tokio::select! {
            command = commands.recv() => match command {
                Some(Command::Retry(key)) => {
                    if let Some(retry) = retries.get(&key) {
                        retry.notify_one();
                    }
                }
                Some(Command::LoadDetail { source, task, reference, request, offline }) => {
                    let backends = backends.clone();
                    let updates = updates.clone();
                    let (cache, data) = (cache.clone(), client.data.clone());
                    tokio::spawn(async move {
                        let asked = Asked { source: &source, task, reference: &reference, offline };
                        let detail = load_detail(&backends, cache.as_ref(), &data, asked)
                            .await
                            .map(Box::new);
                        let _ = updates.send(Update::Detail { source, task, request, detail });
                    });
                }
                Some(Command::Write { source, workspace, request, write }) => {
                    let backends = backends.clone();
                    let updates = updates.clone();
                    tokio::spawn(async move {
                        let result = write_to(&backends, &source, workspace, write).await;
                        let _ = updates.send(Update::Wrote { source, request, result });
                    });
                }
                Some(Command::SaveLayout { source, workspace, nodes }) => {
                    let queue = layout_queues.entry(source.clone()).or_insert_with(|| {
                        let (queue, saves) = mpsc::unbounded_channel();
                        tokio::spawn(save_layouts(
                            backends.clone(),
                            source.clone(),
                            saves,
                            updates.clone(),
                        ));
                        queue
                    });
                    let _ = queue.send((workspace, nodes));
                }
                // The window is gone.
                None => return,
            },
            Some(state) = states.recv() => {
                // Error text comes from servers we don't control: no escapes
                // reach the terminal, the same rule as the CLI.
                let line: String = describe(&state)
                    .chars()
                    .map(|c| if is_unsafe_char(c) { '\u{FFFD}' } else { c })
                    .collect();
                eprintln!("{line}");
                if updates.send(Update::Source(Box::new(state))).is_err() {
                    return;
                }
            }
        }
    }
}

/// One task's details, as the window asked for them.
struct Asked<'a> {
    source: &'a SourceKey,
    task: TaskId,
    reference: &'a str,
    offline: bool,
}

/// A server's answer is also kept in cache.db; when the server can't be
/// reached, the copy kept last time stands in, marked with its age.
async fn load_detail(
    backends: &Backends,
    cache: Option<&Cache>,
    data: &DataDir,
    asked: Asked<'_>,
) -> Result<Fetched, String> {
    let fresh = |detail| Fetched {
        detail,
        saved_at: None,
    };
    let backend = sources::backend(backends, asked.source);
    let (remote, key) = match (backend, asked.source) {
        (Some(Backend::Local(open)), _) => {
            let reference = asked
                .reference
                .parse()
                .map_err(|e: hyakume_core::model::ValidationError| e.to_string())?;
            return open
                .task_by_ref(&reference)
                .await
                .map(fresh)
                .map_err(|e| e.to_string());
        }
        (Some(Backend::Server(remote, key)), _) => (Some(remote), key.map(|k| k.to_string())),
        (None, SourceKey::Server(name)) => (None, sources::current_cache_key(data, name)),
        (None, SourceKey::Local) => return Err("local work isn't open yet".into()),
    };
    let why = match remote.filter(|_| !asked.offline) {
        Some(remote) => match remote.task(asked.task).await {
            Ok(detail) => {
                save_copy(cache, key.as_deref(), &detail).await;
                return Ok(fresh(detail));
            }
            // Refused or gone: an old copy would show what it shouldn't.
            Err(err @ ClientError::Unreachable { .. }) => err.to_string(),
            Err(err) => return Err(err.to_string()),
        },
        None => format!("{} isn't connected", asked.source),
    };
    let saved = match (cache, key) {
        (Some(cache), Some(key)) => cache
            .comments(&key, &asked.task.to_string())
            .await
            .ok()
            .flatten(),
        _ => None,
    };
    saved
        .and_then(|(body, at)| {
            let detail = serde_json::from_str(&body).ok()?;
            Some(Fetched {
                detail,
                saved_at: Some(at),
            })
        })
        .ok_or(why)
}

/// A copy that doesn't save costs only the offline view, so it's reported
/// and dropped.
async fn save_copy(cache: Option<&Cache>, key: Option<&str>, detail: &TaskDetail) {
    let (Some(cache), Some(key)) = (cache, key) else {
        return;
    };
    let Ok(body) = serde_json::to_string(detail) else {
        return;
    };
    let task = detail.task.id.to_string();
    if let Err(err) = cache.save_comments(key, &task, &body).await {
        eprintln!("hyakume: saving comments for offline: {err}");
    }
}

async fn write_to(
    backends: &Backends,
    source: &SourceKey,
    workspace: WorkspaceId,
    write: Write,
) -> Result<Wrote, WriteError> {
    let failed = |err: ClientError| match err {
        ClientError::Conflict { latest, .. } => WriteError::Conflict(latest),
        other => WriteError::Failed(other.to_string()),
    };
    let task = |t: Task| Wrote::Task(Box::new(t));
    let done = |()| Wrote::Done;
    match sources::backend(backends, source) {
        // Local work is one workspace, so the id is already known there.
        Some(Backend::Local(open)) => match write {
            Write::SaveTask { task: id, update } => open.update_task(id, update).await.map(task),
            Write::CreateTask { project, body } => open.create_task(project, body).await.map(task),
            Write::DeleteTask(id) => open.delete_task(id).await.map(done),
            Write::Comment { task: id, body } => {
                open.add_comment(id, &body).await.map(|_| ()).map(done)
            }
            Write::Link(body) => open.create_relation(body).await.map(|_| ()).map(done),
            Write::Unlink(id) => open.delete_relation(id).await.map(done),
            Write::CreateRelationType(body) => {
                open.create_relation_type(body).await.map(|_| ()).map(done)
            }
            Write::CreateRole(body) => open.create_role(body).await.map(|_| ()).map(done),
            Write::SetRoles { identity, roles } => open
                .set_member_roles(identity, SetMemberRoles { roles })
                .await
                .map(done),
        }
        .map_err(failed),
        Some(Backend::Server(remote, _)) => match write {
            Write::SaveTask { task: id, update } => remote.update_task(id, &update).await.map(task),
            Write::CreateTask { project, body } => {
                remote.create_task(project, &body).await.map(task)
            }
            Write::DeleteTask(id) => remote.delete_task(id).await.map(done),
            Write::Comment { task: id, body } => {
                remote.add_comment(id, &body).await.map(|_| ()).map(done)
            }
            Write::Link(body) => remote
                .create_relation(workspace, &body)
                .await
                .map(|_| ())
                .map(done),
            Write::Unlink(id) => remote.delete_relation(id).await.map(done),
            Write::CreateRelationType(body) => remote
                .create_relation_type(workspace, &body)
                .await
                .map(|_| ())
                .map(done),
            Write::CreateRole(body) => remote
                .create_role(workspace, &body)
                .await
                .map(|_| ())
                .map(done),
            Write::SetRoles { identity, roles } => remote
                .set_member_roles(workspace, identity, &SetMemberRoles { roles })
                .await
                .map(done),
        }
        .map_err(failed),
        None => Err(WriteError::Failed(format!("{source} isn't connected yet"))),
    }
}

/// One source's position saves, one batch at a time. What piles up during a
/// save goes in the next batch, only the latest spot per node, so holding
/// Alt+arrow against a slow server doesn't build a backlog.
async fn save_layouts(
    backends: Backends,
    source: SourceKey,
    mut saves: mpsc::UnboundedReceiver<(WorkspaceId, Vec<LayoutNode>)>,
    updates: mpsc::UnboundedSender<Update>,
) {
    while let Some(first) = saves.recv().await {
        let mut batches: Vec<(WorkspaceId, Vec<LayoutNode>)> = Vec::new();
        let mut next = Some(first);
        while let Some((workspace, nodes)) = next.take().or_else(|| saves.try_recv().ok()) {
            let at = match batches.iter().position(|(w, _)| *w == workspace) {
                Some(at) => at,
                None => {
                    batches.push((workspace, Vec::new()));
                    batches.len() - 1
                }
            };
            for node in nodes {
                batches[at].1.retain(|n| n.id != node.id);
                batches[at].1.push(node);
            }
        }
        for (workspace, nodes) in batches {
            if let Err(reason) = save_layout(&backends, &source, workspace, nodes.clone()).await {
                let source = source.clone();
                let _ = updates.send(Update::LayoutUnsaved {
                    source,
                    nodes,
                    reason,
                });
            }
        }
    }
}

async fn save_layout(
    backends: &Backends,
    source: &SourceKey,
    workspace: WorkspaceId,
    nodes: Vec<LayoutNode>,
) -> Result<(), String> {
    let layout = Layout { nodes };
    let saved = match sources::backend(backends, source) {
        Some(Backend::Local(open)) => open.put_layout(&layout).await,
        Some(Backend::Server(remote, _)) => remote.put_layout(workspace, &layout).await,
        None => return Err("it isn't connected yet".into()),
    };
    saved.map_err(|e| e.to_string())
}

/// One stderr line per update, so a run from a terminal shows what each
/// source is doing without opening the window.
fn describe(state: &SourceState) -> String {
    let projects: usize = state.workspaces.iter().map(|w| w.snapshots.len()).sum();
    let tasks: usize = state
        .workspaces
        .iter()
        .flat_map(|w| &w.snapshots)
        .map(|s| s.tasks.len())
        .sum();
    let loaded = format!(
        "{} workspaces, {projects} projects, {tasks} tasks",
        state.workspaces.len()
    );
    let status = match &state.status {
        Status::Loading => "loading".to_owned(),
        Status::Live => format!("live · {loaded}"),
        Status::Offline { reason, retry_at } => format!(
            "offline, retrying in {} s: {reason} · showing {loaded}",
            sources::seconds_until(*retry_at)
        ),
        Status::SignedOut(reason) => format!("not signed in: {reason}"),
        Status::Failed(reason) => format!("failed: {reason}"),
    };
    format!("hyakume: {} ({}) {status}", state.key, state.address)
}

/// `http://127.0.0.1:7878/` reads as `127.0.0.1:7878` in a column header.
fn display_address(url: &str) -> String {
    url.trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_owned()
}
