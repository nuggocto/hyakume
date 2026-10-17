//! The per-user service: the one owner of this installation's identity,
//! state, endpoint, and work, so clients never touch state themselves.
//! Accepted work belongs to the service and outlives its client; so do
//! lock-state transitions and turning finished input into an offer, which
//! run as service tasks (`Service::own`). Prompts belong to the client and
//! are answered no when it leaves. Requests reach the node only through
//! `Service::with_node`, so none outlives a lock.

mod clients;
mod lifecycle;
mod pairing;
mod transfers;
mod views;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::net::UnixListener;
use tokio::sync::{Notify, Semaphore, oneshot, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::timeout;

use self::clients::{Client, refuse_client, serve_client};
use crate::display::SafeLine;
use crate::files::destination::Destination;
use crate::ids::InvitationId;
use crate::ipc::{self, Body, ErrorCode, Failure, Reply};
use crate::limits::{Limits, MAX_NAME_COLUMNS};
use crate::node::{Node, NodeError};
use crate::paths::{self, LocationError, Locations};
use crate::secrets::{self, SecretError};
use crate::state::{Place, State, StateCell, StateError};
use crate::storage::{Installation, Store, StoreError, StoreKind};
use crate::tasks;

/// How long a new client has to say hello.
const HELLO_DEADLINE: Duration = Duration::from_secs(5);
/// How long what is queued for a departing client may take to reach it.
/// A client that stopped reading must not keep its connection slot.
const WRITER_DRAIN: Duration = Duration::from_secs(5);

/// How long shutdown waits for clients to end: long enough for each
/// writer's [`WRITER_DRAIN`].
const CLIENTS_END: Duration = Duration::from_secs(6);
/// How long a request waits for the credential it needs.
const SECRET_DEADLINE: Duration = Duration::from_secs(60);
/// How long standard input may take to reach its end, at most.
pub const STDIN_DEADLINE: Duration = Duration::from_secs(10 * 60);
/// Blocking jobs the service runs at once: preparing files, spooling,
/// key files, folder checks, and deletions.
pub const SERVICE_JOBS: usize = 4;
/// How often expiry and cleanup run.
const MAINTENANCE: Duration = Duration::from_secs(5);
/// How long a cancel request waits for the transfer to end.
const CANCEL_WAIT: Duration = Duration::from_secs(5);
/// Data frames waiting for a request to take them.
const INPUT_QUEUE: usize = 4;

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error(transparent)]
    Location(#[from] LocationError),
    #[error(transparent)]
    State(#[from] StateError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("cannot listen on {0}: {1}")]
    Socket(PathBuf, std::io::Error),
    #[error(
        "the service did not stop within its drain deadline; interrupted work is recovered at the next start"
    )]
    DrainFailed,
}

/// How the service is run.
#[derive(Clone, Debug)]
pub struct ServiceConfig {
    pub locations: Locations,
    /// Where the peer endpoint listens while unlocked.
    pub listen: SocketAddr,
    /// How long standard input may take to reach its end; capped at
    /// [`STDIN_DEADLINE`].
    pub stdin_deadline: Duration,
}

enum Phase {
    Uninitialized,
    Locked(Installation),
    Unlocked {
        installation: Installation,
        node: Arc<Node>,
        router: JoinHandle<()>,
    },
}

#[derive(Default)]
struct Routes {
    /// The client receiving incoming offers, its request, and where.
    receiver: Option<(Arc<Client>, u64, Arc<Destination>)>,
    /// The client running each pairing, and its request.
    pairing: HashMap<InvitationId, (Arc<Client>, u64)>,
}

struct Service {
    config: ServiceConfig,
    cell: Arc<StateCell>,
    phase: Mutex<Phase>,
    /// Serializes init, unlock, lock, and reset.
    transition: tokio::sync::Mutex<()>,
    /// Work that must finish even if the request waiting for it ends
    /// first; see [`Service::own`]. Shutdown waits for all of it.
    owned: Mutex<JoinSet<()>>,
    /// Serializes cleanup passes, so two never act on one ledger snapshot.
    maintaining: tokio::sync::Mutex<()>,
    /// Locked before `phase` when both are held.
    routes: Mutex<Routes>,
    spool: Place,
    /// Bytes of input being spooled and not sealed yet.
    spooling: Mutex<u64>,
    /// Permits for blocking jobs; see [`Service::blocking`].
    jobs: Arc<Semaphore>,
    /// Lock or reset could not stop the node's work in time. The service
    /// then stops instead of reusing the state while that work runs.
    drain_failed: AtomicBool,
    stop: Notify,
    stopping: AtomicBool,
    /// Set when shutdown begins: every client ends its request and gets
    /// what is still queued for it.
    closing: watch::Sender<bool>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Runs the service until it is asked to stop or gets SIGTERM or SIGINT.
pub async fn run(mut config: ServiceConfig) -> Result<(), ServiceError> {
    config.locations = config.locations.prepare()?;
    let locations = config.locations.clone();

    // One service per state directory, and one per socket.
    let _locks = paths::lock_service(&locations)?;

    let mut state = State::load(Store::open(&locations.database())?)?;
    let now = state.now_ms();
    let found = crate::recovery::recover(&mut state, &locations.spool(), now)?;
    tracing::info!(?found, "recovered");
    let installation = settle_installation(&mut state, &locations).await?;

    let spool =
        dir_place(&locations.spool()).map_err(|e| ServiceError::Socket(locations.spool(), e))?;

    let socket = locations.socket();
    let listener = bind_socket(&socket).map_err(|e| ServiceError::Socket(socket.clone(), e))?;
    let svc = Arc::new(Service {
        config,
        cell: Arc::new(StateCell::new(state)),
        phase: Mutex::new(match installation.clone() {
            Some(i) => Phase::Locked(i),
            None => Phase::Uninitialized,
        }),
        transition: tokio::sync::Mutex::new(()),
        owned: Mutex::new(JoinSet::new()),
        maintaining: tokio::sync::Mutex::new(()),
        routes: Mutex::new(Routes::default()),
        spool,
        spooling: Mutex::new(0),
        jobs: Arc::new(Semaphore::new(SERVICE_JOBS)),
        drain_failed: AtomicBool::new(false),
        stop: Notify::new(),
        stopping: AtomicBool::new(false),
        closing: watch::Sender::new(false),
    });

    // A closed standard output must not stop the service.
    {
        use std::io::Write as _;
        let mut out = std::io::stdout();
        let _ = writeln!(out, "listening {}", socket.display());
        let _ = out.flush();
    }

    // A key in the Secret Service needs no passphrase, so it unlocks at
    // start when the keyring allows it without asking anyone.
    if let Some(i) = installation.filter(|i| i.store == StoreKind::SecretService) {
        let _t = svc.transition.lock().await;
        match secrets::load(&i.id).await {
            Ok(bundle) => {
                if let Err(e) = svc.start(i, bundle).await {
                    tracing::warn!(error = %e.message, "could not unlock at start");
                }
            }
            Err(e) => tracing::info!(error = %e, "staying locked"),
        }
    }

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|e| ServiceError::Socket(socket.clone(), e))?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .map_err(|e| ServiceError::Socket(socket.clone(), e))?;

    let permits = Arc::new(Semaphore::new(ipc::MAX_CLIENTS));
    let mut clients = JoinSet::new();
    let mut maintenance: Option<JoinHandle<()>> = None;
    let mut tick = tokio::time::interval(MAINTENANCE);

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { continue };
                match permits.clone().try_acquire_owned() {
                    Ok(permit) => {
                        clients.spawn(serve_client(svc.clone(), stream, permit));
                    }
                    Err(_) => {
                        clients.spawn(refuse_client(stream));
                    }
                }
            }
            Some(done) = clients.join_next(), if !clients.is_empty() => {
                tasks::observe(done);
            }
            _ = tick.tick() => {
                svc.reap();
                if maintenance.as_ref().is_none_or(|m| m.is_finished()) {
                    if let Some(m) = maintenance.take() {
                        tasks::observe(m.await);
                    }
                    maintenance = Some(tokio::spawn(maintain(svc.clone())));
                }
            }
            () = svc.stop.notified() => break,
            _ = sigterm.recv() => break,
            _ = sigint.recv() => break,
        }
    }

    // Close admission first, so no new client arrives while work drains.
    drop(listener);
    let _ = std::fs::remove_file(&socket);
    svc.stopping.store(true, Ordering::SeqCst);

    let drained = timeout(Limits::COMPILED.drain_deadline, async {
        // Clients end first and get what is queued for them, such as the
        // reply to `stop`. One that has not ended in time is cut off.
        svc.closing.send_replace(true);
        let _ = timeout(CLIENTS_END, async {
            while let Some(done) = clients.join_next().await {
                tasks::observe(done);
            }
        })
        .await;
        clients.abort_all();
        while let Some(done) = clients.join_next().await {
            tasks::observe(done);
        }
        // No client is left to start more; finish what requests began.
        let mut owned = std::mem::take(&mut *lock(&svc.owned));
        while let Some(done) = owned.join_next().await {
            tasks::observe(done);
        }
        if let Some(m) = maintenance.take() {
            tasks::observe(m.await);
        }
        let _t = svc.transition.lock().await;
        let stopped = svc.stop_node().await;
        // Blocking jobs outlive the requests that started them; holding
        // every permit means none still runs.
        let _all = svc.jobs.acquire_many(SERVICE_JOBS as u32).await;
        stopped
    })
    .await;

    match drained {
        Ok(Ok(())) if !svc.drain_failed.load(Ordering::SeqCst) => Ok(()),
        _ => Err(ServiceError::DrainFailed),
    }
}

/// An initialization that never finished is undone, removing only what it
/// created; a finished one is returned.
async fn settle_installation(
    state: &mut State,
    locations: &Locations,
) -> Result<Option<Installation>, ServiceError> {
    match state.store().installation()? {
        Some(i) if i.ready => Ok(Some(i)),
        Some(i) => {
            tracing::warn!("removing an initialization that did not finish");
            match delete_key(locations, &i).await {
                Ok(()) => state.forget_all()?,
                // The record names the key, so it stays until the key is
                // gone; init and the next start try again.
                Err(e) => tracing::warn!(error = %e, "could not remove the unfinished key"),
            }
            Ok(None)
        }
        None => Ok(None),
    }
}

/// Deletes the installation's key from its store. A key that is not
/// there counts as deleted.
async fn delete_key(locations: &Locations, i: &Installation) -> Result<(), SecretError> {
    match i.store {
        StoreKind::KeyFile => secrets::delete_key_file(&locations.key_file()),
        StoreKind::SecretService => secrets::delete(&i.id).await,
    }
}

fn dir_place(path: &Path) -> std::io::Result<Place> {
    let meta = std::fs::symlink_metadata(path)?;
    Ok(Place {
        dir: path.as_os_str().as_bytes().to_vec(),
        dev: meta.dev(),
        ino: meta.ino(),
    })
}

/// Binds the socket. The instance lock is held, so a socket left at this
/// path by an earlier run is stale and goes; anything else there is
/// refused.
fn bind_socket(path: &Path) -> std::io::Result<UnixListener> {
    match std::fs::symlink_metadata(path) {
        Ok(meta)
            if meta.file_type().is_socket()
                && meta.uid() == rustix::process::geteuid().as_raw() =>
        {
            std::fs::remove_file(path)?;
        }
        Ok(_) => {
            return Err(std::io::Error::other(
                "something other than Hyakume's socket is in its place",
            ));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Expires paused work past its window, checks unconfirmed renames again,
/// and deletes files that are due.
async fn maintain(svc: Arc<Service>) {
    let _pass = svc.maintaining.lock().await;
    let unsettled = svc.cell.change(|s| {
        let now = s.now_ms();
        s.expire(now);
        crate::recovery::unsettled(s)
    });
    match unsettled {
        Ok(unsettled) => {
            for item in unsettled {
                let check = item.clone();
                let Ok(evidence) = svc.blocking(move || crate::recovery::recheck(&check)).await
                else {
                    return;
                };
                svc.cell
                    .change(|s| crate::recovery::settle(s, &item, evidence));
            }
        }
        Err(e) => tracing::error!(error = %e, "cannot read the file ledger"),
    }

    let due = svc.cell.change(|s| {
        let now = s.now_ms();
        s.store().files_due(now)
    });
    let due = match due {
        Ok(due) => due,
        Err(e) => {
            tracing::error!(error = %e, "cannot read the file ledger");
            return;
        }
    };
    for file in due {
        let target = file.clone();
        let Ok(result) = svc
            .blocking(move || crate::recovery::delete_owned(&target))
            .await
        else {
            return;
        };
        if let Err(e) = &result {
            tracing::warn!(error = %e, "could not delete a file Hyakume owns");
        }
        let recorded = svc.cell.change(|s| {
            let now = s.now_ms();
            crate::recovery::record_deletion(s, &file, result, now)
        });
        if let Err(e) = recorded {
            tracing::error!(error = %e, "could not record a deletion");
        }
    }
}

fn failed(code: ErrorCode, message: impl Into<String>) -> Reply {
    Reply::Failed {
        failure: Failure::new(code, message),
    }
}

fn ok(body: Body) -> Reply {
    Reply::Ok { body }
}

fn secret_failure(e: SecretError) -> Failure {
    let code = match &e {
        SecretError::Unavailable(_) | SecretError::Timeout | SecretError::Confirmation => {
            ErrorCode::StoreUnavailable
        }
        SecretError::Locked => ErrorCode::Locked,
        SecretError::WrongPassphrase => ErrorCode::WrongPassphrase,
        SecretError::WeakPassphrase => ErrorCode::WeakPassphrase,
        SecretError::Missing | SecretError::Corrupt(_) => ErrorCode::StoreUnavailable,
        SecretError::Io(_) => ErrorCode::Storage,
    };
    Failure::new(code, e.to_string())
}

fn node_failure(e: NodeError) -> Failure {
    let code = match &e {
        NodeError::Trust(_)
        | NodeError::Session(_)
        | NodeError::Peer(crate::protocol::ErrorCode::NotTrusted) => ErrorCode::NotTrusted,
        NodeError::Peer(_) | NodeError::Pairing(crate::pairing::PairingError::Declined) => {
            ErrorCode::Refused
        }
        NodeError::Refused(_) => ErrorCode::Busy,
        NodeError::Token(_) | NodeError::Offer(_) => ErrorCode::Invalid,
        NodeError::State(_) => ErrorCode::Storage,
        NodeError::Stopping => ErrorCode::Stopping,
        _ => ErrorCode::Failed,
    };
    Failure::new(code, e.to_string())
}

/// The old key could not be deleted, so nothing was forgotten.
fn key_not_deleted(e: &SecretError) -> Failure {
    match e {
        SecretError::Locked => Failure::new(
            ErrorCode::Locked,
            "unlock the keyring so the old key can be deleted, then try again",
        ),
        e => Failure::new(
            ErrorCode::StoreUnavailable,
            format!(
                "the old key could not be deleted ({e}); nothing was forgotten, try again once its store is reachable"
            ),
        ),
    }
}

fn state_failure(e: StateError) -> Failure {
    match e {
        StateError::Trust(t) => Failure::new(ErrorCode::NotTrusted, t.to_string()),
        other => Failure::new(ErrorCode::Storage, other.to_string()),
    }
}

/// A safe single-line view of local bytes, such as a path.
fn shown(bytes: &[u8]) -> String {
    SafeLine::project(bytes, 4 * MAX_NAME_COLUMNS).to_string()
}

fn hex(bytes: &[u8]) -> String {
    data_encoding::HEXLOWER.encode(bytes)
}

impl Service {
    /// Runs `f` with the running node, or says why there is none. The node
    /// never leaves `f`, which cannot await, so no request keeps a node
    /// and its key alive after lock or reset stopped it.
    fn with_node<T>(&self, f: impl FnOnce(&Node) -> T) -> Result<T, Failure> {
        let node = match &*lock(&self.phase) {
            Phase::Unlocked { node, .. } => node.clone(),
            Phase::Locked(_) => {
                return Err(Failure::new(
                    ErrorCode::Locked,
                    "the device key is locked; run hyakume unlock",
                ));
            }
            Phase::Uninitialized => {
                return Err(Failure::new(
                    ErrorCode::NotInitialized,
                    "this device has no identity yet; run hyakume init",
                ));
            }
        };
        Ok(f(&node))
    }

    /// Runs `job` on a blocking thread holding one of [`SERVICE_JOBS`]
    /// permits. The permit goes only when the job really ends, even if its
    /// request is gone, and shutdown waits for every permit.
    async fn blocking<T: Send + 'static>(
        &self,
        job: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, Failure> {
        let stopping = || Failure::new(ErrorCode::Stopping, "the service is stopping");
        let permit = self
            .jobs
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| stopping())?;
        let handle = tasks::spawn_blocking(move || {
            let result = job();
            drop(permit);
            result
        });
        tasks::observe(handle.await).ok_or_else(stopping)
    }

    /// Runs `work` as a task the service owns, so it finishes even if the
    /// request waiting for it ends first. Shutdown waits for it.
    async fn own<T: Send + 'static>(
        &self,
        work: impl Future<Output = T> + Send + 'static,
    ) -> Result<T, Failure> {
        let (done, result) = oneshot::channel();
        lock(&self.owned).spawn(async move {
            let _ = done.send(work.await);
        });
        result
            .await
            .map_err(|_| Failure::new(ErrorCode::Stopping, "the service is stopping"))
    }

    /// Observes the owned tasks that finished.
    fn reap(&self) {
        let mut owned = lock(&self.owned);
        while let Some(done) = owned.try_join_next() {
            tasks::observe(done);
        }
    }

    /// Waits for the turn to change the lock state. Once the service is
    /// stopping, no transition starts: it could start a node after the
    /// last one was stopped.
    async fn transition(&self) -> Result<tokio::sync::MutexGuard<'_, ()>, Failure> {
        let turn = self.transition.lock().await;
        if self.stopping.load(Ordering::SeqCst) {
            return Err(Failure::new(ErrorCode::Stopping, "the service is stopping"));
        }
        Ok(turn)
    }
}
