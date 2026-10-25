//! All network, keyring, and SQLite work runs on one Tokio thread. reqwest
//! and SQLx need Tokio and GPUI runs its own executor, so the window and this
//! thread only talk through two channels. Tokio's channels work on any executor.

use std::collections::HashMap;
use std::sync::Arc;
use std::thread::JoinHandle;

use hyakume_client::{Client, DataDir};
use hyakume_core::api::TaskDetail;
use hyakume_core::ids::TaskId;
use hyakume_core::model::is_unsafe_char;
use tokio::sync::{Notify, mpsc};

use crate::sources::{self, Backend, Backends, SourceKey, SourceState, Status};

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
    },
}

pub enum Update {
    Source(Box<SourceState>),
    Detail {
        source: SourceKey,
        task: TaskId,
        request: u64,
        detail: Result<Box<TaskDetail>, String>,
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
            state_tx.clone(),
        ));
    }
    drop(state_tx);

    loop {
        tokio::select! {
            command = commands.recv() => match command {
                Some(Command::Retry(key)) => {
                    if let Some(retry) = retries.get(&key) {
                        retry.notify_one();
                    }
                }
                Some(Command::LoadDetail { source, task, reference, request }) => {
                    let backends = backends.clone();
                    let updates = updates.clone();
                    tokio::spawn(async move {
                        let detail = load_detail(&backends, &source, task, &reference)
                            .await
                            .map(Box::new);
                        let _ = updates.send(Update::Detail { source, task, request, detail });
                    });
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

async fn load_detail(
    backends: &Backends,
    source: &SourceKey,
    task: TaskId,
    reference: &str,
) -> Result<TaskDetail, String> {
    match sources::backend(backends, source) {
        Some(Backend::Local(open)) => {
            let reference = reference
                .parse()
                .map_err(|e: hyakume_core::model::ValidationError| e.to_string())?;
            open.task_by_ref(&reference)
                .await
                .map_err(|e| e.to_string())
        }
        Some(Backend::Server(remote)) => remote.task(task).await.map_err(|e| e.to_string()),
        None => Err(format!("{source} isn't connected yet")),
    }
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
