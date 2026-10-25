//! Where the canvas gets its data: local work plus one connection per
//! configured server. Each source loads and fails on its own, so an
//! unreachable server never hides local work or another team's.

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use hyakume_client::remote::Remote;
use hyakume_client::{Client, ClientError, DataDir, OpenWorkspace};
use hyakume_core::api::{Event, ProjectSnapshot, WorkspaceSummary};
use hyakume_core::ids::{ProjectId, WorkspaceId};
use hyakume_storage::ChangeWatcher;
use tokio::sync::{Notify, mpsc};

/// How often local.db is checked for commits from the CLI or another app.
const LOCAL_POLL: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SourceKey {
    Local,
    /// A server by its name in client.toml, such as `a`.
    Server(Arc<str>),
}

impl fmt::Display for SourceKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Local => f.write_str("local"),
            Self::Server(name) => f.write_str(name),
        }
    }
}

#[derive(Clone, Debug)]
pub enum Status {
    Loading,
    Live,
    /// Unreachable. The last loaded data stays on screen, read-only.
    Offline {
        reason: String,
        retry_at: Instant,
    },
    /// Reachable, but this profile has no valid session there.
    SignedOut(String),
    /// Something retrying won't fix on its own, like a broken config.
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct SourceState {
    pub key: SourceKey,
    /// "On this computer", or the server's address.
    pub address: String,
    pub status: Status,
    pub workspaces: Vec<WorkspaceData>,
}

#[derive(Clone, Debug)]
pub struct WorkspaceData {
    pub summary: WorkspaceSummary,
    /// One per visible project, in key order.
    pub snapshots: Vec<ProjectSnapshot>,
}

/// What a detail request needs to reach a source after it has loaded.
#[derive(Clone)]
pub enum Backend {
    Local(Arc<OpenWorkspace>),
    Server(Remote),
}

pub type Backends = Arc<Mutex<Vec<(SourceKey, Backend)>>>;

pub fn set_backend(backends: &Backends, key: &SourceKey, backend: Backend) {
    // A poisoned lock only means another task panicked mid-push; the list is still usable.
    let mut list = backends.lock().unwrap_or_else(|e| e.into_inner());
    list.retain(|(k, _)| k != key);
    list.push((key.clone(), backend));
}

pub fn backend(backends: &Backends, key: &SourceKey) -> Option<Backend> {
    let list = backends.lock().unwrap_or_else(|e| e.into_inner());
    list.iter().find(|(k, _)| k == key).map(|(_, b)| b.clone())
}

/// Whole seconds until `at`, rounded up so a 1 s wait never reads "0 s".
pub fn seconds_until(at: Instant) -> u64 {
    at.saturating_duration_since(Instant::now())
        .as_millis()
        .div_ceil(1000) as u64
}

type Sink = mpsc::UnboundedSender<SourceState>;

/// Loads local work, then reloads it whenever another connection commits.
pub async fn run_local(data: DataDir, retry: Arc<Notify>, backends: Backends, out: Sink) {
    let mut state = SourceState {
        key: SourceKey::Local,
        address: "On this computer".into(),
        status: Status::Loading,
        workspaces: Vec::new(),
    };
    let _ = out.send(state.clone());
    loop {
        match open_local(&data, &backends).await {
            Ok((open, watcher)) => {
                watch_local(&open, watcher, &mut state, &retry, &out).await;
            }
            Err(err) => {
                state.status = Status::Failed(err.to_string());
                let _ = out.send(state.clone());
            }
        }
        retry.notified().await;
    }
}

async fn open_local(
    data: &DataDir,
    backends: &Backends,
) -> Result<(Arc<OpenWorkspace>, ChangeWatcher), ClientError> {
    let client = Client::load(data.clone())?;
    let open = Arc::new(client.open_local().await?);
    set_backend(backends, &SourceKey::Local, Backend::Local(open.clone()));
    let watcher = ChangeWatcher::open(&data.local_db()).await?;
    Ok((open, watcher))
}

/// Returns only when the watcher breaks, so the caller can wait for a retry.
async fn watch_local(
    open: &OpenWorkspace,
    mut watcher: ChangeWatcher,
    state: &mut SourceState,
    retry: &Notify,
    out: &Sink,
) {
    loop {
        match load_local(open).await {
            Ok(workspace) => {
                state.workspaces = vec![workspace];
                state.status = Status::Live;
            }
            Err(err) => state.status = Status::Failed(err.to_string()),
        }
        if out.send(state.clone()).is_err() {
            return;
        }
        loop {
            tokio::select! {
                _ = tokio::time::sleep(LOCAL_POLL) => {}
                _ = retry.notified() => break,
            }
            match watcher.changed().await {
                Ok(true) => break,
                Ok(false) => {}
                Err(err) => {
                    state.status = Status::Failed(format!("watching local work: {err}"));
                    let _ = out.send(state.clone());
                    return;
                }
            }
        }
    }
}

async fn load_local(open: &OpenWorkspace) -> Result<WorkspaceData, ClientError> {
    let mut summary = open.summary().await?;
    summary.projects.sort_by(|a, b| a.key.cmp(&b.key));
    let mut snapshots = Vec::with_capacity(summary.projects.len());
    for project in &summary.projects {
        snapshots.push(open.snapshot(project.id).await?);
    }
    Ok(WorkspaceData { summary, snapshots })
}

/// Keeps one server's workspaces current: subscribe, load, follow events,
/// and reconnect with backoff when the connection drops.
pub async fn run_server(
    data: DataDir,
    name: Arc<str>,
    address: String,
    retry: Arc<Notify>,
    backends: Backends,
    out: Sink,
) {
    let key = SourceKey::Server(name.clone());
    let mut state = SourceState {
        key: key.clone(),
        address,
        status: Status::Loading,
        workspaces: Vec::new(),
    };
    let _ = out.send(state.clone());
    let mut backoff = Duration::from_secs(1);
    loop {
        let err = session(&data, &name, &mut state, &backends, &out).await;
        if out.is_closed() {
            return;
        }
        if matches!(state.status, Status::Live) {
            backoff = Duration::from_secs(1);
        }
        match err {
            ClientError::Unreachable { reason, .. } | ClientError::Internal(reason) => {
                state.status = Status::Offline {
                    reason,
                    retry_at: Instant::now() + backoff,
                };
                let _ = out.send(state.clone());
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = retry.notified() => {}
                }
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
            ClientError::Unauthorized(reason) => {
                state.status = Status::SignedOut(reason);
                let _ = out.send(state.clone());
                retry.notified().await;
            }
            other => {
                state.status = Status::Failed(other.to_string());
                let _ = out.send(state.clone());
                retry.notified().await;
            }
        }
    }
}

/// One connected stretch. Always ends with the error that ended it.
async fn session(
    data: &DataDir,
    name: &str,
    state: &mut SourceState,
    backends: &Backends,
    out: &Sink,
) -> ClientError {
    match follow(data, name, state, backends, out).await {
        Ok(()) => ClientError::Unreachable {
            server: name.to_owned(),
            reason: "the event stream ended".into(),
        },
        Err(err) => err,
    }
}

async fn follow(
    data: &DataDir,
    name: &str,
    state: &mut SourceState,
    backends: &Backends,
    out: &Sink,
) -> Result<(), ClientError> {
    // Reloaded on every attempt, so a sign-in done with the CLI meanwhile is picked up.
    let client = Client::load(data.clone())?;
    let remote = client.server(name).await?;
    if !remote.has_session() {
        return Err(ClientError::Unauthorized(format!(
            "Not signed in. Run `hyakume auth dev-login NAME --server {name}`."
        )));
    }
    // Subscribe before fetching: any commit after `subscribed` sends an event.
    let events = remote.subscribe().await?;
    let mut events = std::pin::pin!(events);
    match events.next().await {
        Some(Ok(Event::Subscribed)) => {}
        Some(Err(err)) => return Err(err),
        _ => {
            return Err(ClientError::Internal(
                "the server skipped `subscribed`".into(),
            ));
        }
    }
    set_backend(backends, &state.key, Backend::Server(remote.clone()));
    state.workspaces = load_all(&remote).await?;
    state.status = Status::Live;
    if out.send(state.clone()).is_err() {
        return Ok(());
    }
    while let Some(event) = events.next().await {
        match event? {
            Event::ProjectChanged {
                workspace_id,
                project_id,
            } => refresh_project(&remote, state, workspace_id, project_id).await?,
            Event::WorkspaceChanged { workspace_id } => {
                refresh_workspace(&remote, state, workspace_id).await?
            }
            // Saved layouts arrive with dragging in step 8.
            Event::LayoutChanged { .. } => continue,
            Event::Resync | Event::Subscribed => state.workspaces = load_all(&remote).await?,
        }
        if out.send(state.clone()).is_err() {
            return Ok(());
        }
    }
    Ok(())
}

async fn load_all(remote: &Remote) -> Result<Vec<WorkspaceData>, ClientError> {
    let mut listings = remote.workspaces().await?;
    listings.sort_by(|a, b| a.workspace.slug.cmp(&b.workspace.slug));
    let mut workspaces = Vec::with_capacity(listings.len());
    for listing in listings {
        if let Some(workspace) = load_workspace(remote, listing.workspace.id).await? {
            workspaces.push(workspace);
        }
    }
    Ok(workspaces)
}

/// `None` when the workspace is gone or no longer visible to this identity.
async fn load_workspace(
    remote: &Remote,
    id: WorkspaceId,
) -> Result<Option<WorkspaceData>, ClientError> {
    let mut summary = match remote.summary(id).await {
        Ok(summary) => summary,
        Err(ClientError::NotFound(_) | ClientError::Forbidden(_)) => return Ok(None),
        Err(err) => return Err(err),
    };
    summary.projects.sort_by(|a, b| a.key.cmp(&b.key));
    let mut snapshots = Vec::with_capacity(summary.projects.len());
    for project in &summary.projects {
        match remote.snapshot(project.id).await {
            Ok(snapshot) => snapshots.push(snapshot),
            // Deleted or hidden between the summary and now; the event for it follows.
            Err(ClientError::NotFound(_) | ClientError::Forbidden(_)) => {}
            Err(err) => return Err(err),
        }
    }
    summary
        .projects
        .retain(|p| snapshots.iter().any(|s| s.project.id == p.id));
    Ok(Some(WorkspaceData { summary, snapshots }))
}

async fn refresh_workspace(
    remote: &Remote,
    state: &mut SourceState,
    id: WorkspaceId,
) -> Result<(), ClientError> {
    let at = state
        .workspaces
        .iter()
        .position(|w| w.summary.workspace.id == id);
    match (at, load_workspace(remote, id).await?) {
        (Some(at), Some(workspace)) => state.workspaces[at] = workspace,
        (Some(at), None) => drop(state.workspaces.remove(at)),
        // Newly joined: reload everything so the order stays by slug.
        (None, Some(_)) => state.workspaces = load_all(remote).await?,
        (None, None) => {}
    }
    Ok(())
}

async fn refresh_project(
    remote: &Remote,
    state: &mut SourceState,
    workspace: WorkspaceId,
    project: ProjectId,
) -> Result<(), ClientError> {
    let Some(ws) = state
        .workspaces
        .iter_mut()
        .find(|w| w.summary.workspace.id == workspace)
    else {
        return refresh_workspace(remote, state, workspace).await;
    };
    let Some(at) = ws.snapshots.iter().position(|s| s.project.id == project) else {
        // A project we haven't seen, so the project list changed too.
        return refresh_workspace(remote, state, workspace).await;
    };
    match remote.snapshot(project).await {
        Ok(snapshot) => ws.snapshots[at] = snapshot,
        Err(ClientError::NotFound(_) | ClientError::Forbidden(_)) => {
            return refresh_workspace(remote, state, workspace).await;
        }
        Err(err) => return Err(err),
    }
    Ok(())
}
