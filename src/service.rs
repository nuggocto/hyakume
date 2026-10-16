//! The per-user service: the one owner of this installation's identity,
//! state, endpoint, and work, so clients never touch state themselves.
//! Accepted work belongs to the service and outlives its client; so do
//! lock-state transitions and turning finished input into an offer, which
//! run as service tasks (`Service::own`). Prompts belong to the client and
//! are answered no when it leaves. Requests reach the node only through
//! `Service::with_node`, so none outlives a lock.

use std::collections::HashMap;
use std::ffi::OsString;
use std::net::SocketAddr;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use rand_core::{OsRng, RngCore};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{Instant, timeout, timeout_at};
use zeroize::Zeroizing;

use crate::display::SafeLine;
use crate::files::destination::Destination;
use crate::files::names::OutputName;
use crate::files::source::{PreparedItem, Sealed, Source, Spool, SpoolError, prepare_file};
use crate::identity::{DeviceId, DeviceKey};
use crate::ids::{InvitationId, TransferId};
use crate::invitation::{InvitationState, InvitationToken, MAX_TOKEN_CHARS, Route};
use crate::ipc::{
    self, Body, ClientMessage, DeviceView, ErrorCode, Event, Failure, Frame, ItemView, Reply,
    Request, ServerMessage, StatusView, StoresView, StreamKind, TransferSummary, TransferView,
};
use crate::limits::{
    INVITATION_LIFETIME, Limits, MAX_BATCH_ITEMS, MAX_NAME_COLUMNS, MAX_PREVIEW_BYTES,
    MAX_SPOOL_BYTES, MAX_SPOOL_TOTAL, MAX_TEXT_BYTES, MAX_URL_BYTES,
};
use crate::node::{self, Node, NodeConfig, NodeError, PROMPT_QUEUE, Prepared, Prompt, SendOptions};
use crate::offer::PayloadKind;
use crate::paths::{self, LocationError, Locations};
use crate::secrets::{self, Availability, KeyBundle, MIN_PASSPHRASE_BYTES, SecretError};
use crate::state::{Place, State, StateCell, StateError, is_paused};
use crate::storage::{FileKind, FileState, Installation, NewFile, Store, StoreError, StoreKind};
use crate::tasks;
use crate::transfer::BatchOutcome;
use crate::trust::{LocalAlias, TrustError, TrustState};

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

// ---- Clients ----

/// What a running request reads besides its first message.
enum Input {
    Data(Vec<u8>),
    Secret(Zeroizing<Vec<u8>>),
    End,
}

struct PendingPrompt {
    context: String,
    answer: oneshot::Sender<bool>,
    transfer: Option<TransferId>,
}

/// One connected client: its event queue and the prompts it holds.
struct Client {
    events: mpsc::Sender<ServerMessage>,
    prompts: Mutex<HashMap<u64, PendingPrompt>>,
    /// Its event queue filled up; the connection ends.
    overloaded: Notify,
    /// Offers this client accepted, reported when they end.
    accepted: Mutex<Vec<TransferId>>,
}

impl Client {
    fn send(&self, msg: ServerMessage) {
        if self.events.try_send(msg).is_err() {
            self.overloaded.notify_one();
        }
    }

    fn event(&self, id: u64, event: Event) {
        self.send(ServerMessage::Event { id, event });
    }

    /// Hands a question to this client. If it already holds as many as
    /// the node may ask, the question is dropped unanswered, which the
    /// node takes as having nobody to ask.
    fn ask(
        &self,
        id: u64,
        context: String,
        transfer: Option<TransferId>,
        answer: oneshot::Sender<bool>,
        event: impl FnOnce(u64) -> Event,
    ) {
        let prompt = OsRng.next_u64();
        {
            let mut prompts = lock(&self.prompts);
            // Questions the node stopped waiting for, such as a withdrawn
            // offer, hold no place.
            prompts.retain(|_, p| !p.answer.is_closed());
            if prompts.len() >= PROMPT_QUEUE {
                return;
            }
            prompts.insert(
                prompt,
                PendingPrompt {
                    context,
                    answer,
                    transfer,
                },
            );
        }
        self.event(id, event(prompt));
    }

    /// Delivers an answer. Only a prompt this client holds, with the same
    /// context, can be answered; any other answer approves nothing.
    fn answer(&self, prompt: u64, context: &str, accept: bool) -> bool {
        let Some(pending) = lock(&self.prompts).remove(&prompt) else {
            return false;
        };
        if pending.context != context {
            // The prompt is used up as a no.
            return false;
        }
        if pending.answer.send(accept).is_err() {
            return false;
        }
        if accept && let Some(transfer) = pending.transfer {
            lock(&self.accepted).push(transfer);
        }
        true
    }

    /// The client is gone: every question it holds is answered no.
    fn decline_all(&self) {
        for (_, pending) in lock(&self.prompts).drain() {
            let _ = pending.answer.send(false);
        }
    }
}

/// Tells a client past the limit that the service is busy, then closes.
async fn refuse_client(mut stream: UnixStream) {
    let reply = ServerMessage::Reply {
        id: 0,
        reply: Reply::Failed {
            failure: Failure::new(ErrorCode::Busy, "too many clients are connected"),
        },
    };
    let _ = timeout(HELLO_DEADLINE, ipc::write_message(&mut stream, &reply)).await;
}

async fn serve_client(svc: Arc<Service>, stream: UnixStream, _permit: OwnedSemaphorePermit) {
    // Only this user's processes may talk to the service.
    match stream.peer_cred() {
        Ok(cred) if cred.uid() == rustix::process::geteuid().as_raw() => {}
        _ => return,
    }

    let (rd, mut wr) = stream.into_split();
    let mut rd = ipc::FrameReader::new(rd);
    let (events, mut outgoing) = mpsc::channel::<ServerMessage>(ipc::EVENT_QUEUE);
    let mut writer = tokio::spawn(async move {
        while let Some(msg) = outgoing.recv().await {
            if ipc::write_message(&mut wr, &msg).await.is_err() {
                break;
            }
        }
    });

    let client = Arc::new(Client {
        events,
        prompts: Mutex::new(HashMap::new()),
        overloaded: Notify::new(),
        accepted: Mutex::new(Vec::new()),
    });

    let hello = timeout(HELLO_DEADLINE, rd.next()).await;
    let version = match hello {
        Ok(Ok(Frame::Message(body))) => match ipc::decode::<ClientMessage>(&body) {
            Ok(ClientMessage::Hello { version }) => version,
            _ => 0,
        },
        _ => 0,
    };
    client.send(ServerMessage::Hello {
        version: ipc::VERSION,
    });
    if version == ipc::VERSION {
        client_loop(&svc, &client, &mut rd).await;
    }

    // The client is gone: its prompts are answered no, and its running
    // request ended with the loop.
    client.decline_all();
    drop(client);
    let written = match timeout(WRITER_DRAIN, &mut writer).await {
        Ok(written) => written,
        Err(_) => {
            writer.abort();
            writer.await
        }
    };
    tasks::observe(written);
}

async fn client_loop(
    svc: &Arc<Service>,
    client: &Arc<Client>,
    rd: &mut ipc::FrameReader<tokio::net::unix::OwnedReadHalf>,
) {
    let mut input_tx: Option<mpsc::Sender<Input>> = None;
    let mut closing = svc.closing.subscribe();

    // Owns the running request so it ends when the connection does. The
    // request returns its reply instead of sending it, and the loop sends
    // it once the request is off this guard: a client that asks again
    // after reading a reply never finds the last request still running.
    struct Running(Option<JoinHandle<ServerMessage>>);
    impl Drop for Running {
        fn drop(&mut self) {
            if let Some(h) = self.0.take() {
                h.abort();
            }
        }
    }
    let mut guard = Running(None);
    // The running request is `stop`. The service is asked to stop only
    // once its reply is queued, so shutdown cannot end this connection
    // before the reply is on its way.
    let mut stops = false;

    loop {
        let finished = async {
            match guard.0.as_mut() {
                Some(h) => tasks::observe(h.await),
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            frame = rd.next() => {
                let input = match frame {
                    Ok(Frame::Message(body)) => match ipc::decode::<ClientMessage>(&body) {
                        Ok(ClientMessage::Request { id, request }) => {
                            if guard.0.is_some() {
                                client.send(ServerMessage::Reply {
                                    id,
                                    reply: failed(ErrorCode::Busy, "a request is already running on this connection"),
                                });
                                continue;
                            }
                            stops = matches!(request, Request::Stop {});
                            let (tx, rx) = mpsc::channel(INPUT_QUEUE);
                            let (svc, client) = (svc.clone(), client.clone());
                            guard.0 = Some(tokio::spawn(async move {
                                let reply = handle(&svc, &client, id, request, rx).await;
                                ServerMessage::Reply { id, reply }
                            }));
                            input_tx = Some(tx);
                            continue;
                        }
                        Ok(ClientMessage::Answer { prompt, context, accept }) => {
                            if !client.answer(prompt, &context, accept) {
                                client.send(ServerMessage::AnswerRefused { prompt });
                            }
                            continue;
                        }
                        Ok(ClientMessage::StreamEnd) => Input::End,
                        Ok(ClientMessage::Hello { .. }) | Err(_) => break,
                    },
                    Ok(Frame::Data(data)) => Input::Data(data),
                    Ok(Frame::Secret(secret)) => Input::Secret(secret),
                    Err(_) => break,
                };
                // Input for the running request; waiting here stops reading
                // the socket, which pushes back on the client. Input that
                // arrives after its request ended, such as the rest of a
                // refused stream, is dropped so the client can read why.
                if let Some(tx) = input_tx.as_ref() {
                    let _ = tx.send(input).await;
                }
            }
            reply = finished => {
                guard.0 = None;
                input_tx = None;
                if let Some(reply) = reply {
                    reply_then_stop(svc, client, reply, std::mem::take(&mut stops));
                }
            }
            () = client.overloaded.notified() => break,
            () = async { drop(closing.wait_for(|closing| *closing).await) } => break,
        }
    }

    // The request ends with its connection; observing it lets a panic in it
    // stop the process. A reply it finished in time still goes out.
    if let Some(handle) = guard.0.take() {
        handle.abort();
        if let Some(reply) = tasks::observe(handle.await) {
            reply_then_stop(svc, client, reply, stops);
        }
    }
}

fn reply_then_stop(svc: &Service, client: &Client, reply: ServerMessage, stops: bool) {
    client.send(reply);
    if stops {
        svc.stop.notify_one();
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

async fn handle(
    svc: &Arc<Service>,
    client: &Arc<Client>,
    id: u64,
    request: Request,
    mut inputs: mpsc::Receiver<Input>,
) -> Reply {
    if svc.stopping.load(Ordering::SeqCst) {
        return failed(ErrorCode::Stopping, "the service is stopping");
    }
    // A request reads input only while it needs it; past that, input is
    // refused at once instead of filling a queue nobody reads.
    let takes_input = matches!(
        request,
        Request::Init { .. }
            | Request::Unlock {}
            | Request::Join { .. }
            | Request::SendStream { .. }
    );
    if !takes_input {
        inputs.close();
    }
    let result = match request {
        Request::Status {} => Ok(Body::Status(svc.status())),
        Request::Stores {} => Ok(Body::Stores(StoresView {
            secret_service: match secrets::probe().await {
                Availability::Unlocked => "unlocked",
                Availability::Locked => "locked",
                Availability::Unavailable => "unavailable",
            }
            .into(),
            key_file: true,
        })),
        // A client that leaves during a lock-state transition ends its wait,
        // never the transition: a drain cut short would leave work running
        // that the next unlock could not see.
        Request::Init { store, name } => {
            let s = svc.clone();
            svc.own(async move { s.init(store, name, &mut inputs).await })
                .await
                .flatten()
        }
        Request::Unlock {} => {
            let s = svc.clone();
            svc.own(async move { s.unlock(&mut inputs).await })
                .await
                .flatten()
        }
        Request::Lock {} => {
            let s = svc.clone();
            svc.own(async move { s.lock().await }).await.flatten()
        }
        Request::Invite { alias } => svc.invite(client, id, alias).await,
        Request::Join { alias } => svc.join(client, id, alias, &mut inputs).await,
        Request::Devices {} => Ok(svc.devices()),
        Request::Remove { device } => svc.remove(&device),
        Request::Receive { dir } => svc.receive(client, id, dir).await,
        Request::SendFiles {
            device,
            route,
            paths,
        } => svc.send_files(client, id, &device, &route, paths).await,
        Request::SendStream {
            device,
            route,
            kind,
            name,
        } => {
            svc.send_stream(client, id, &device, &route, kind, name, &mut inputs)
                .await
        }
        Request::Cancel { transfer } => svc.cancel(&transfer).await,
        Request::Inspect { transfer } => svc
            .find_transfer(&transfer)
            .and_then(|t| svc.transfer_view(t))
            .map(Body::Transfer),
        Request::Transfers {} => Ok(svc.transfers()),
        Request::Cleanup {} => svc.cleanup().await,
        Request::Reset {} => {
            let s = svc.clone();
            svc.own(async move { s.reset().await }).await.flatten()
        }
        // The connection asks the service to stop once this reply is
        // queued; see `client_loop`.
        Request::Stop {} => Ok(Body::Done),
    };
    match result {
        Ok(body) => ok(body),
        Err(failure) => Reply::Failed { failure },
    }
}

/// Waits for the next piece of streamed input until `deadline`.
async fn next_input(
    inputs: &mut mpsc::Receiver<Input>,
    deadline: Instant,
) -> Result<Input, Failure> {
    // A frame that is ready wins over a timer that has run out, so the
    // deadline is checked again: a backlog of input must not outlast it.
    match timeout_at(deadline, inputs.recv()).await {
        Ok(Some(input)) if Instant::now() < deadline => Ok(input),
        Ok(None) => Err(Failure::new(ErrorCode::Failed, "the input stopped")),
        _ => Err(Failure::new(
            ErrorCode::Refused,
            "the input did not end in time; nothing was offered",
        )),
    }
}

/// Waits for the credential a request needs.
async fn secret_input(inputs: &mut mpsc::Receiver<Input>) -> Result<Zeroizing<Vec<u8>>, Failure> {
    let received = timeout(SECRET_DEADLINE, inputs.recv()).await;
    inputs.close();
    match received {
        Ok(Some(Input::Secret(secret))) => Ok(secret),
        Ok(Some(_)) => Err(Failure::new(ErrorCode::Malformed, "expected a credential")),
        _ => Err(Failure::new(ErrorCode::Malformed, "no credential arrived")),
    }
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

    // ---- Lock lifecycle ----

    async fn init(
        self: &Arc<Self>,
        store: StoreKind,
        name: String,
        inputs: &mut mpsc::Receiver<Input>,
    ) -> Result<Body, Failure> {
        let _t = self.transition().await?;
        if !matches!(&*lock(&self.phase), Phase::Uninitialized) {
            return Err(Failure::new(
                ErrorCode::AlreadyInitialized,
                "this device already has an identity; hyakume reset forgets it",
            ));
        }

        let earlier = self
            .cell
            .change(|s| s.store().installation())
            .map_err(|e| Failure::new(ErrorCode::Storage, e.to_string()))?;
        if earlier.as_ref().is_some_and(|i| i.ready) {
            return Err(Failure::new(
                ErrorCode::AlreadyInitialized,
                "this device already has an identity; hyakume reset forgets it",
            ));
        }

        // Only an initialization that never finished is undone.
        if let Some(old) = earlier {
            delete_key(&self.config.locations, &old)
                .await
                .map_err(|e| key_not_deleted(&e))?;
            self.cell
                .change(|s| s.forget_all())
                .map_err(state_failure)?;
        }

        let name = LocalAlias::new(&name).map_err(|_| {
            Failure::new(
                ErrorCode::Invalid,
                "a device name must be 1 to 64 characters without control characters",
            )
        })?;

        let passphrase = match store {
            StoreKind::KeyFile => {
                let pass = secret_input(inputs).await?;
                if pass.len() < MIN_PASSPHRASE_BYTES {
                    return Err(secret_failure(SecretError::WeakPassphrase));
                }
                Some(pass)
            }
            StoreKind::SecretService => {
                if secrets::probe().await != Availability::Unlocked {
                    return Err(Failure::new(
                        ErrorCode::StoreUnavailable,
                        "the Secret Service is unavailable or locked; choose the key file instead",
                    ));
                }
                None
            }
        };

        let mut id = [0u8; 16];
        OsRng.fill_bytes(&mut id);
        let key = DeviceKey::generate(&mut OsRng);
        let installation = Installation {
            id,
            device: key.id(),
            store,
            name: name.as_str().to_owned(),
            ready: false,
        };

        self.cell
            .change(|s| s.store().begin_installation(&installation))
            .map_err(|e| Failure::new(ErrorCode::Storage, e.to_string()))?;

        let bundle = KeyBundle {
            installation: id,
            seed: key.seed(),
        };
        drop(key);

        let written = match (store, passphrase) {
            (StoreKind::KeyFile, Some(pass)) => {
                let path = self.config.locations.key_file();
                let seed = bundle.seed.clone();
                self.blocking(move || {
                    let bundle = KeyBundle {
                        installation: id,
                        seed,
                    };
                    secrets::write_key_file(&path, &bundle, &pass)
                })
                .await
                .unwrap_or_else(|_| {
                    Err(SecretError::Io(std::io::Error::other(
                        "the key writer stopped",
                    )))
                })
            }
            _ => secrets::store(&bundle).await,
        };

        if let Err(e) = written {
            // Undo what was written. If that fails too, the unfinished
            // record stays so the next init or start can finish the job.
            if delete_key(&self.config.locations, &installation)
                .await
                .is_ok()
            {
                let _ = self.cell.change(|s| s.forget_all());
            }
            return Err(secret_failure(e));
        }

        self.cell
            .change(|s| s.store().finish_installation())
            .map_err(|e| Failure::new(ErrorCode::Storage, e.to_string()))?;
        let installation = Installation {
            ready: true,
            ..installation
        };

        // The identity is saved. If the endpoint cannot start, the service
        // keeps it, locked, and unlock tries again.
        if let Err(failure) = self.start(installation.clone(), bundle).await {
            *lock(&self.phase) = Phase::Locked(installation);
            return Err(Failure::new(
                failure.code,
                format!(
                    "{}; the identity is saved, run hyakume unlock to try again",
                    failure.message
                ),
            ));
        }
        Ok(Body::Status(self.status()))
    }

    async fn unlock(self: &Arc<Self>, inputs: &mut mpsc::Receiver<Input>) -> Result<Body, Failure> {
        let _t = self.transition().await?;

        // The phase lock is released before anything below locks it again.
        let locked = match &*lock(&self.phase) {
            Phase::Locked(i) => Some(i.clone()),
            Phase::Unlocked { .. } | Phase::Uninitialized => None,
        };
        let Some(installation) = locked else {
            self.with_node(|_| ())?;
            return Ok(Body::Status(self.status()));
        };

        let bundle = match installation.store {
            StoreKind::KeyFile => {
                let pass = secret_input(inputs).await?;
                let path = self.config.locations.key_file();
                let id = installation.id;
                self.blocking(move || secrets::read_key_file(&path, &pass, &id))
                    .await?
            }
            StoreKind::SecretService => secrets::load(&installation.id).await,
        }
        .map_err(secret_failure)?;

        self.start(installation, bundle).await?;
        Ok(Body::Status(self.status()))
    }

    /// Starts the node with the unlocked key. The key must be the one this
    /// installation recorded; a different key is refused, never adopted.
    async fn start(
        self: &Arc<Self>,
        installation: Installation,
        bundle: KeyBundle,
    ) -> Result<(), Failure> {
        let key = DeviceKey::from_seed(&bundle.seed);
        if key.id() != installation.device {
            return Err(Failure::new(
                ErrorCode::StoreUnavailable,
                "the stored key is not this installation's device key; hyakume reset starts over",
            ));
        }

        let config = NodeConfig {
            name: installation.name.clone(),
            limits: Limits::COMPILED,
            bind: self.config.listen,
            read_gate: None,
            state: Some(self.cell.clone()),
        };

        let (node, prompts) = Node::bind(config, key).await.map_err(node_failure)?;
        let node = Arc::new(node);
        let router = tokio::spawn(route_prompts(self.clone(), prompts));

        // The receiver cannot change until the node is in place, so the
        // node never keeps the destination of a receiver that left.
        let routes = lock(&self.routes);
        if let Some((_, _, dest)) = &routes.receiver {
            node.set_destination(Some(dest.clone()));
        }
        *lock(&self.phase) = Phase::Unlocked {
            installation,
            node,
            router,
        };
        drop(routes);
        Ok(())
    }

    async fn lock(self: &Arc<Self>) -> Result<Body, Failure> {
        let _t = self.transition().await?;
        self.stop_node_or_service().await?;
        Ok(Body::Status(self.status()))
    }

    /// Stops the node for lock or reset. If its work does not stop in
    /// time, the whole service stops: a new node must not share the state
    /// with work that still runs, and the next start recovers it.
    async fn stop_node_or_service(&self) -> Result<(), Failure> {
        self.stop_node().await.map_err(|e| {
            // New requests are refused from now on, before the stop begins.
            self.stopping.store(true, Ordering::SeqCst);
            self.drain_failed.store(true, Ordering::SeqCst);
            self.stop.notify_one();
            Failure::new(ErrorCode::Stopping, format!("{e}; the service is stopping"))
        })
    }

    /// Stops the node, if running, pausing its work. The key goes with it.
    async fn stop_node(&self) -> Result<(), NodeError> {
        let running = {
            let mut phase = lock(&self.phase);
            match std::mem::replace(&mut *phase, Phase::Uninitialized) {
                Phase::Unlocked {
                    installation,
                    node,
                    router,
                } => {
                    *phase = Phase::Locked(installation);
                    Some((node, router))
                }
                other => {
                    *phase = other;
                    None
                }
            }
        };

        let Some((node, router)) = running else {
            return Ok(());
        };

        let stopped = node.shutdown().await;
        router.abort();
        tasks::observe(router.await);
        stopped
    }

    async fn reset(self: &Arc<Self>) -> Result<Body, Failure> {
        let _t = self.transition().await?;
        self.stop_node_or_service().await?;

        let installation = self
            .cell
            .change(|s| s.store().installation())
            .map_err(|e| Failure::new(ErrorCode::Storage, e.to_string()))?;
        if let Some(i) = installation {
            // The old key must be gone before the identity is forgotten:
            // the installation record is what finds the key to delete.
            delete_key(&self.config.locations, &i)
                .await
                .map_err(|e| key_not_deleted(&e))?;
            self.cell
                .change(|s| s.forget_all())
                .map_err(state_failure)?;
        }

        *lock(&self.phase) = Phase::Uninitialized;
        Ok(Body::Status(self.status()))
    }

    // ---- Views ----

    fn status(&self) -> StatusView {
        let (state, installation, route) = match &*lock(&self.phase) {
            Phase::Uninitialized => ("uninitialized", None, None),
            Phase::Locked(i) => ("locked", Some(i.clone()), None),
            Phase::Unlocked {
                installation, node, ..
            } => (
                "unlocked",
                Some(installation.clone()),
                node.route().to_text().ok(),
            ),
        };

        let receiving = lock(&self.routes)
            .receiver
            .as_ref()
            .map(|(_, _, d)| shown(&d.place().dir));

        let mut s = self.cell.lock();
        let trusted = s.trust().trusted().count();
        let unfinished = s.unfinished();
        let paused = unfinished
            .iter()
            .filter(|t| s.transfer(t).is_some_and(is_paused))
            .count();
        let store_failed = s.store_failed();
        let files = s.store().files().unwrap_or_default();
        let (spool_bytes, partial_bytes) = s.store().charged().unwrap_or_default();
        let retained: Vec<_> = files
            .iter()
            .filter(|f| f.state == FileState::Retained)
            .collect();

        StatusView {
            state: state.into(),
            store: installation.as_ref().map(|i| i.store),
            fingerprint: installation.as_ref().map(|i| i.device.fingerprint()),
            // Loading already refused a name with controls; escaping
            // again leaves a valid name as it is.
            name: installation.as_ref().map(|i| {
                SafeLine::project(i.name.as_bytes(), crate::trust::MAX_ALIAS_BYTES)
                    .as_str()
                    .to_owned()
            }),
            route,
            trusted,
            unfinished: unfinished.len(),
            paused,
            receiving,
            retained_files: retained.len(),
            retained_bytes: retained.iter().map(|f| f.bytes).sum(),
            spool_bytes,
            partial_bytes,
            store_failed,
        }
    }

    fn devices(&self) -> Body {
        let s = self.cell.lock();
        let devices = s
            .trust()
            .records()
            .map(|r| DeviceView {
                alias: r.alias.as_str().to_owned(),
                fingerprint: r.device.fingerprint(),
                name: r.declared_name.line(MAX_NAME_COLUMNS).to_string(),
                trusted: matches!(r.state, TrustState::Trusted { .. }),
            })
            .collect();
        Body::Devices { devices }
    }

    fn find_device(&self, name: &str) -> Result<DeviceId, Failure> {
        self.cell.lock().trust().find(name).map_err(|e| match e {
            TrustError::Ambiguous => Failure::new(ErrorCode::Invalid, e.to_string()),
            _ => Failure::new(ErrorCode::NotFound, "no trusted device has that name"),
        })
    }

    fn remove(&self, name: &str) -> Result<Body, Failure> {
        let device = self.find_device(name)?;
        self.cell
            .change(|s| s.revoke(&device))
            .map_err(state_failure)?;
        let s = self.cell.lock();
        let record = s
            .trust()
            .record(&device)
            .ok_or_else(|| Failure::new(ErrorCode::NotFound, "no such device"))?;
        Ok(Body::Device(DeviceView {
            alias: record.alias.as_str().to_owned(),
            fingerprint: record.device.fingerprint(),
            name: record.declared_name.line(MAX_NAME_COLUMNS).to_string(),
            trusted: false,
        }))
    }

    /// Finds a transfer by its ID or a unique prefix of at least 8 hex
    /// digits.
    fn find_transfer(&self, text: &str) -> Result<TransferId, Failure> {
        let text = text.to_ascii_lowercase();
        if text.len() < 8 || text.len() > 32 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Failure::new(
                ErrorCode::Invalid,
                "a transfer is named by at least 8 hex digits of its ID",
            ));
        }
        let s = self.cell.lock();
        let mut found = s
            .transfers()
            .map(|t| t.id())
            .filter(|id| hex(id.as_bytes()).starts_with(&text));
        match (found.next(), found.next()) {
            (Some(id), None) => Ok(id),
            (None, _) => Err(Failure::new(ErrorCode::NotFound, "no transfer has that ID")),
            (Some(_), Some(_)) => Err(Failure::new(
                ErrorCode::Invalid,
                "more than one transfer starts with that; give more digits",
            )),
        }
    }

    fn transfer_view(&self, id: TransferId) -> Result<TransferView, Failure> {
        let mut s = self.cell.lock();
        let retained_bytes = s
            .store()
            .files()
            .unwrap_or_default()
            .iter()
            .filter(|f| f.transfer == id)
            .map(|f| f.bytes)
            .sum();

        let (Some(t), Some(info)) = (s.transfer(&id), s.info(&id)) else {
            return Err(Failure::new(ErrorCode::NotFound, "no transfer has that ID"));
        };

        let peer = match s.trust().record(&t.peer()) {
            Some(r) if matches!(r.state, TrustState::Trusted { .. }) => r.alias.as_str().to_owned(),
            Some(r) => format!("{} (removed)", r.alias.as_str()),
            None => t.peer().fingerprint(),
        };
        let outcome = match t.outcome() {
            BatchOutcome::Offered => "offered",
            BatchOutcome::InProgress if t.items().iter().any(|s| s.is_unconfirmed()) => {
                "unconfirmed"
            }
            BatchOutcome::InProgress if is_paused(t) => "paused",
            BatchOutcome::InProgress => "in-progress",
            BatchOutcome::Completed => "completed",
            BatchOutcome::PartiallyCompleted { .. } => "partially-completed",
            BatchOutcome::Unsuccessful(_) => "unsuccessful",
        };

        let kind = info.offer.kind();
        let items = info
            .offer
            .items()
            .iter()
            .zip(t.items())
            .enumerate()
            .map(|(n, (item, state))| ItemView {
                name: match kind {
                    PayloadKind::Files => item.name.line(MAX_NAME_COLUMNS).to_string(),
                    PayloadKind::Text => "(text)".into(),
                    PayloadKind::Url => "(link)".into(),
                },
                size: item.size,
                state: state.name(),
                output: info.outputs.get(n).cloned().flatten().map(|o| shown(&o)),
            })
            .collect();

        Ok(TransferView {
            transfer: hex(id.as_bytes()),
            outgoing: info.outgoing,
            peer,
            kind: match kind {
                PayloadKind::Files => "files",
                PayloadKind::Text => "text",
                PayloadKind::Url => "link",
            }
            .into(),
            offer: t.offer_state().name(),
            outcome: outcome.into(),
            items,
            place: info.place.as_ref().map(|p| shown(&p.dir)),
            retained_bytes,
            expires_ms: info.expires_ms,
        })
    }

    fn transfers(&self) -> Body {
        let mut ids: Vec<(u64, TransferId)> = {
            let s = self.cell.lock();
            s.transfers()
                .filter_map(|t| s.info(&t.id()).map(|i| (i.created_ms, t.id())))
                .collect()
        };
        ids.sort_by_key(|a| std::cmp::Reverse(a.0));
        Body::Transfers {
            transfers: ids
                .into_iter()
                .filter_map(|(_, id)| self.transfer_view(id).ok())
                .map(|view| TransferSummary::from(&view))
                .collect(),
        }
    }

    // ---- Pairing ----

    async fn invite(
        self: &Arc<Self>,
        client: &Arc<Client>,
        id: u64,
        alias: String,
    ) -> Result<Body, Failure> {
        let (invitation, token) = self
            .with_node(|n| n.invite(&alias))?
            .map_err(node_failure)?;

        lock(&self.routes)
            .pairing
            .insert(invitation, (client.clone(), id));
        let _route = PairingRoute {
            svc: self.clone(),
            invitation,
            cancel: true,
        };
        client.event(
            id,
            Event::Invitation {
                token: token.to_string(),
            },
        );

        let mut changes = self.cell.subscribe();
        let deadline = Instant::now() + INVITATION_LIFETIME + Limits::COMPILED.idle_timeout;

        loop {
            match self.with_node(|n| n.invitation(invitation))? {
                None => return Err(Failure::new(ErrorCode::Failed, "the invitation is gone")),
                Some((_, Some(Ok(device)))) => return self.device_body(device),
                Some((_, Some(Err(why)))) => return Err(Failure::new(ErrorCode::Refused, why)),
                Some((InvitationState::Open | InvitationState::Consumed, None)) => {}
                Some((state, None)) => {
                    return Err(Failure::new(
                        ErrorCode::Refused,
                        format!("the invitation ended: {}", invitation_ended(state)),
                    ));
                }
            }
            if Instant::now() >= deadline {
                return Err(Failure::new(ErrorCode::Refused, "the invitation expired"));
            }
            // Expiry changes nothing in the state, so look again each second.
            let _ = timeout(Duration::from_secs(1), changes.changed()).await;
        }
    }

    async fn join(
        self: &Arc<Self>,
        client: &Arc<Client>,
        id: u64,
        alias: String,
        inputs: &mut mpsc::Receiver<Input>,
    ) -> Result<Body, Failure> {
        self.with_node(|_| ())?;
        let secret = secret_input(inputs).await?;
        if secret.len() > MAX_TOKEN_CHARS {
            return Err(Failure::new(
                ErrorCode::Invalid,
                "the invitation is too long",
            ));
        }

        let text = std::str::from_utf8(&secret)
            .map_err(|_| Failure::new(ErrorCode::Invalid, "the invitation is not text"))?
            .trim();
        let token = InvitationToken::parse(text)
            .map_err(|e| Failure::new(ErrorCode::Invalid, e.to_string()))?;
        lock(&self.routes)
            .pairing
            .insert(token.id, (client.clone(), id));
        let _route = PairingRoute {
            svc: self.clone(),
            invitation: token.id,
            cancel: false,
        };

        let joined = self
            .with_node(|n| n.start_join(text, &alias))?
            .map_err(node_failure)?;
        let device = joined
            .await
            .map_err(|_| Failure::new(ErrorCode::Stopping, "the device was locked"))?
            .map_err(node_failure)?;
        self.device_body(device)
    }

    fn device_body(&self, device: DeviceId) -> Result<Body, Failure> {
        let s = self.cell.lock();
        let r = s
            .trust()
            .record(&device)
            .ok_or_else(|| Failure::new(ErrorCode::Failed, "the device was not saved"))?;
        Ok(Body::Device(DeviceView {
            alias: r.alias.as_str().to_owned(),
            fingerprint: r.device.fingerprint(),
            name: r.declared_name.line(MAX_NAME_COLUMNS).to_string(),
            trusted: matches!(r.state, TrustState::Trusted { .. }),
        }))
    }

    // ---- Receiving ----

    async fn receive(
        self: &Arc<Self>,
        client: &Arc<Client>,
        id: u64,
        dir: Option<Vec<u8>>,
    ) -> Result<Body, Failure> {
        self.with_node(|_| ())?;
        let path = match dir {
            Some(bytes) => PathBuf::from(OsString::from_vec(bytes)),
            None => self.config.locations.app_storage(),
        };

        let shown_path = shown(path.as_os_str().as_bytes());
        let qualify = path.clone();
        let dest = self
            .blocking(move || Destination::qualify(&qualify))
            .await?
            .map_err(|e| {
                Failure::new(
                    ErrorCode::Destination,
                    format!("cannot receive into {shown_path}: {e}"),
                )
            })?;
        let dest = Arc::new(dest);

        {
            let mut routes = lock(&self.routes);
            if routes.receiver.is_some() {
                return Err(Failure::new(
                    ErrorCode::Busy,
                    "another client is already receiving",
                ));
            }
            routes.receiver = Some((client.clone(), id, dest.clone()));
            // While locked, the next unlock applies the registered
            // destination.
            let _ = self.with_node(|n| n.set_destination(Some(dest.clone())));
        }
        let _registered = ReceiverRoute { svc: self.clone() };
        client.event(id, Event::Receiving { dir: shown_path });
        let mut changes = self.cell.subscribe();

        loop {
            let ended: Vec<TransferId> = {
                let s = self.cell.lock();
                let mut accepted = lock(&client.accepted);
                let ended = accepted
                    .iter()
                    .copied()
                    .filter(|t| {
                        s.transfer(t)
                            .is_none_or(|t| t.is_terminal() || is_paused(t))
                    })
                    .collect();
                accepted.retain(|t| {
                    s.transfer(t)
                        .is_some_and(|t| !t.is_terminal() && !is_paused(t))
                });
                ended
            };
            for transfer in ended {
                if let Ok(view) = self.transfer_view(transfer) {
                    client.event(id, Event::Received(view));
                }
            }
            if changes.changed().await.is_err() {
                return Ok(Body::Done);
            }
        }
    }

    // ---- Sending ----

    fn send_target(
        &self,
        device: &str,
        route: &str,
    ) -> Result<(DeviceId, iroh::EndpointAddr), Failure> {
        self.with_node(|_| ())?;
        let device = self.find_device(device)?;
        let route = Route::parse(route.trim())
            .map_err(|e| Failure::new(ErrorCode::Invalid, format!("the route: {e}")))?;
        let addr = node::route_addr(&route).map_err(node_failure)?;
        Ok((device, addr))
    }

    async fn send_files(
        self: &Arc<Self>,
        client: &Arc<Client>,
        id: u64,
        device: &str,
        route: &str,
        paths: Vec<Vec<u8>>,
    ) -> Result<Body, Failure> {
        let (device, addr) = self.send_target(device, route)?;
        if paths.is_empty() || paths.len() > MAX_BATCH_ITEMS {
            return Err(Failure::new(
                ErrorCode::Invalid,
                "send 1 to 256 files at once",
            ));
        }

        let mut items = Vec::with_capacity(paths.len());
        for raw in &paths {
            if raw.len() > ipc::MAX_PATH_BYTES {
                return Err(Failure::new(ErrorCode::Invalid, "a path is too long"));
            }
            let path = PathBuf::from(OsString::from_vec(raw.clone()));
            if !path.is_absolute() {
                return Err(Failure::new(ErrorCode::Invalid, "paths must be absolute"));
            }
            let item = self
                .blocking(move || prepare_file(&path))
                .await?
                .map_err(|e| Failure::new(ErrorCode::Invalid, shown(e.to_string().as_bytes())))?;
            if items.iter().any(|i: &PreparedItem| i.name == item.name) {
                return Err(Failure::new(
                    ErrorCode::Invalid,
                    "two files have the same name; one batch needs distinct names",
                ));
            }
            items.push(item);
        }

        let prepared = Prepared {
            transfer: TransferId::random(&mut OsRng),
            kind: PayloadKind::Files,
            items,
            preview: Vec::new(),
            paths: paths.into_iter().map(Some).collect(),
        };

        let started = self.start_send(device, addr, prepared)?;
        self.follow_send(client, id, started).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn send_stream(
        self: &Arc<Self>,
        client: &Arc<Client>,
        id: u64,
        device: &str,
        route: &str,
        kind: StreamKind,
        name: Option<String>,
        inputs: &mut mpsc::Receiver<Input>,
    ) -> Result<Body, Failure> {
        let (device, addr) = self.send_target(device, route)?;
        let transfer = TransferId::random(&mut OsRng);
        let name = match (kind, name) {
            (StreamKind::Stdin, Some(name)) => OutputName::new(name.as_bytes())
                .map_err(|e| Failure::new(ErrorCode::Invalid, format!("the name: {e}")))?
                .as_str()
                .to_owned(),
            (StreamKind::Stdin, None) => format!("stdin-{}", &hex(transfer.as_bytes())[..8]),
            (_, Some(_)) => {
                return Err(Failure::new(
                    ErrorCode::Invalid,
                    "only standard input takes a name",
                ));
            }
            (_, None) => String::new(),
        };

        let (limit, keep, payload) = match kind {
            StreamKind::Text => (MAX_TEXT_BYTES, MAX_TEXT_BYTES as usize, PayloadKind::Text),
            StreamKind::Url => (MAX_URL_BYTES, MAX_URL_BYTES as usize, PayloadKind::Url),
            StreamKind::Stdin => (MAX_SPOOL_BYTES, 0, PayloadKind::Files),
        };
        let spool = Spool::create(&self.config.locations.spool(), transfer, limit, keep)
            .map_err(|e| Failure::new(ErrorCode::Storage, e.to_string()))?;
        let mut input = InputSpool {
            spool,
            charge: SpoolCharge {
                svc: self.clone(),
                bytes: 0,
            },
            transfer,
        };

        let deadline = Instant::now() + self.config.stdin_deadline.min(STDIN_DEADLINE);
        loop {
            match next_input(inputs, deadline).await? {
                Input::End => {
                    inputs.close();
                    break;
                }
                Input::Secret(_) => {
                    return Err(Failure::new(ErrorCode::Malformed, "unexpected credential"));
                }
                Input::Data(data) => {
                    input.charge.add(data.len() as u64)?;
                    let (back, written) = self
                        .blocking(move || {
                            let written = input.spool.write(&data);
                            (input, written)
                        })
                        .await?;
                    input = back;
                    written.map_err(|e| {
                        let code = match e {
                            SpoolError::TooLarge(_) => ErrorCode::TooLarge,
                            _ => ErrorCode::Storage,
                        };
                        Failure::new(code, format!("{e}; nothing was offered"))
                    })?;
                }
            }
        }

        // Text and links fit whole in the head.
        if payload != PayloadKind::Files && std::str::from_utf8(input.spool.head()).is_err() {
            return Err(Failure::new(
                ErrorCode::Invalid,
                "text and links must be UTF-8",
            ));
        }
        // From the end of input on, the offer is the service's to make, so a
        // client that leaves now cannot strand a sealed spool.
        let svc = self.clone();
        let started = self
            .own(async move { svc.offer_input(input, device, addr, payload, name).await })
            .await
            .flatten()?;
        self.follow_send(client, id, started).await
    }

    /// Seals finished input, hands its spool to the ledger, and starts its
    /// send. A spool no transfer took is deleted.
    async fn offer_input(
        &self,
        input: InputSpool,
        device: DeviceId,
        addr: iroh::EndpointAddr,
        kind: PayloadKind,
        name: String,
    ) -> Result<Started, Failure> {
        let (mut input, sealed) = self
            .blocking(move || {
                let mut input = input;
                let sealed = input.spool.seal();
                (input, sealed)
            })
            .await?;
        let sealed: Sealed = sealed.map_err(|e| Failure::new(ErrorCode::Storage, e.to_string()))?;
        let Source::File { identity, .. } = &sealed.source else {
            return Err(Failure::new(ErrorCode::Failed, "a spool is a file"));
        };

        // The ledger charges the sealed spool from now until it is deleted.
        let transfer = input.transfer;
        input
            .keep(&NewFile {
                kind: FileKind::Spool,
                transfer,
                item: 0,
                dir: &self.spool,
                name: sealed.name.as_bytes(),
                identity: Some(identity.key()),
                bytes: sealed.size,
            })
            .map_err(|e| Failure::new(ErrorCode::Storage, format!("{e}; nothing was offered")))?;

        let preview = match kind {
            PayloadKind::Files => Vec::new(),
            _ => sealed.head[..sealed.head.len().min(MAX_PREVIEW_BYTES)].to_vec(),
        };

        let prepared = Prepared {
            transfer,
            kind,
            items: vec![PreparedItem {
                name: name.into_bytes(),
                size: sealed.size,
                digest: sealed.digest,
                source: sealed.source,
            }],
            preview,
            paths: vec![None],
        };
        let started = self.start_send(device, addr, prepared);
        if started.is_err() && self.cell.lock().transfer(&transfer).is_none() {
            // No transfer took the spool, so it goes.
            let marked = self.cell.change(|s| {
                let now = s.now_ms();
                s.store().delete_files_of(transfer, now)
            });
            if let Err(e) = marked {
                tracing::error!(error = %e, "could not mark an unused spool for deletion");
            }
        }
        started
    }

    fn start_send(
        &self,
        device: DeviceId,
        addr: iroh::EndpointAddr,
        prepared: Prepared,
    ) -> Result<Started, Failure> {
        self.with_node(|n| n.start_send(device, addr, prepared, SendOptions::default()))?
            .map_err(node_failure)
    }

    /// Tells the client the send started and waits for its end. The send
    /// belongs to the node, so a client that stops waiting does not stop it.
    async fn follow_send(
        &self,
        client: &Client,
        id: u64,
        (transfer, done): Started,
    ) -> Result<Body, Failure> {
        client.event(
            id,
            Event::Started {
                transfer: hex(transfer.as_bytes()),
            },
        );
        if let Ok(Err(e)) = done.await {
            return Err(node_failure(e));
        }
        self.transfer_view(transfer).map(Body::Transfer)
    }

    async fn cancel(&self, text: &str) -> Result<Body, Failure> {
        let id = self.find_transfer(text)?;
        self.cell.change(|s| s.request_cancel(id));
        let mut changes = self.cell.subscribe();
        let deadline = Instant::now() + CANCEL_WAIT;
        loop {
            let settled = self
                .cell
                .lock()
                .transfer(&id)
                .is_none_or(|t| t.is_terminal() || is_paused(t));
            if settled {
                break;
            }
            if timeout_at(deadline, changes.changed()).await.is_err() {
                break;
            }
        }
        self.transfer_view(id).map(Body::Transfer)
    }

    async fn cleanup(self: &Arc<Self>) -> Result<Body, Failure> {
        let retried = self
            .cell
            .change(|s| {
                let now = s.now_ms();
                s.store().retry_retained(now)
            })
            .map_err(|e| Failure::new(ErrorCode::Storage, e.to_string()))?;
        maintain(self.clone()).await;
        Ok(Body::Cleanup { retried })
    }
}

fn invitation_ended(state: InvitationState) -> &'static str {
    match state {
        InvitationState::Expired => "it expired",
        InvitationState::Exhausted => "too many attempts failed",
        InvitationState::Cancelled => "it was cancelled",
        InvitationState::Declined => "the words were declined",
        InvitationState::Consumed | InvitationState::Open => "it was used",
    }
}

/// Bytes of one input being spooled, counted in `spooling` until sealed,
/// then in the ledger. Admission and the handoff both lock `spooling`
/// first, then the state, so no byte is ever counted twice or not at all.
struct SpoolCharge {
    svc: Arc<Service>,
    bytes: u64,
}

impl SpoolCharge {
    fn add(&mut self, more: u64) -> Result<(), Failure> {
        let mut spooling = lock(&self.svc.spooling);
        let charged = self
            .svc
            .cell
            .lock()
            .store()
            .charged()
            .map(|(spools, _)| spools)
            .map_err(|e| Failure::new(ErrorCode::Storage, e.to_string()))?;
        let total = charged
            .checked_add(*spooling)
            .and_then(|t| t.checked_add(more));
        if total.is_none_or(|t| t > MAX_SPOOL_TOTAL) {
            return Err(Failure::new(
                ErrorCode::TooLarge,
                "spool storage is full; nothing was offered",
            ));
        }
        *spooling += more;
        self.bytes += more;
        Ok(())
    }

    /// Moves the bytes counted while the spool was written into the ledger
    /// row `record` adds, as one step under `spooling`.
    fn hand_off(
        &mut self,
        record: impl FnOnce(&mut State) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        let mut spooling = lock(&self.svc.spooling);
        self.svc.cell.change(record)?;
        *spooling = spooling.saturating_sub(self.bytes);
        self.bytes = 0;
        Ok(())
    }

    /// Leaves the bytes counted after this charge ends, for a spool that
    /// could be neither recorded nor deleted. The next start removes the
    /// spool, and the count starts again from the ledger.
    fn keep(&mut self) {
        self.bytes = 0;
    }
}

impl Drop for SpoolCharge {
    fn drop(&mut self) {
        let mut spooling = lock(&self.svc.spooling);
        *spooling = spooling.saturating_sub(self.bytes);
    }
}

/// A send that started: its transfer, and the node's word on how it ended.
type Started = (TransferId, oneshot::Receiver<Result<(), NodeError>>);

/// Input being spooled, with the bytes counted for it. Its bytes stay
/// counted until its file is gone or the ledger has it: dropped before
/// [`InputSpool::keep`], it deletes the file, and a file that cannot be
/// deleted goes to the ledger, still charged, for cleanup to retry.
struct InputSpool {
    spool: Spool,
    charge: SpoolCharge,
    transfer: TransferId,
}

impl InputSpool {
    /// Hands the sealed spool to the ledger, which charges it from now on.
    fn keep(&mut self, file: &NewFile<'_>) -> Result<(), StoreError> {
        self.charge.hand_off(|s| s.add_file(file).map(drop))?;
        self.spool.keep();
        Ok(())
    }
}

impl Drop for InputSpool {
    fn drop(&mut self) {
        let Err((e, left)) = self.spool.discard() else {
            return;
        };
        tracing::warn!(error = %e, "could not delete unfinished input; cleanup retries");
        let svc = self.charge.svc.clone();
        let file = NewFile {
            kind: FileKind::Spool,
            transfer: self.transfer,
            item: 0,
            dir: &svc.spool,
            name: left.name.as_bytes(),
            identity: left.identity,
            bytes: left.bytes,
        };
        if self
            .charge
            .hand_off(|s| s.add_file_to_delete(&file))
            .is_err()
        {
            tracing::error!("unfinished input could be neither deleted nor recorded");
            self.charge.keep();
        }
    }
}

/// Unregisters a pairing's prompt route when its request ends, and with
/// `cancel`, cancels the invitation if it is still open: nobody is left to
/// answer for it.
struct PairingRoute {
    svc: Arc<Service>,
    invitation: InvitationId,
    cancel: bool,
}

impl Drop for PairingRoute {
    fn drop(&mut self) {
        lock(&self.svc.routes).pairing.remove(&self.invitation);
        if self.cancel {
            let _ = self.svc.with_node(|n| n.cancel_invitation(self.invitation));
        }
    }
}

/// Unregisters the receiving client and stops taking offers when its
/// request ends. The node's destination only changes with the routes
/// locked, so a receiver leaving cannot clear the one that replaced it.
struct ReceiverRoute {
    svc: Arc<Service>,
}

impl Drop for ReceiverRoute {
    fn drop(&mut self) {
        let mut routes = lock(&self.svc.routes);
        routes.receiver = None;
        if let Phase::Unlocked { node, .. } = &*lock(&self.svc.phase) {
            node.set_destination(None);
        }
    }
}

/// Hands each question from the node to the client that owns it. A
/// question nobody owns is answered no as it drops.
async fn route_prompts(svc: Arc<Service>, mut prompts: mpsc::Receiver<Prompt>) {
    while let Some(prompt) = prompts.recv().await {
        match prompt {
            Prompt::Verify {
                invitation,
                peer,
                declared_name,
                verifier,
                answer,
            } => {
                let owner = lock(&svc.routes).pairing.get(&invitation).cloned();
                let Some((client, id)) = owner else {
                    continue;
                };
                let mut h = blake3::Hasher::new();
                h.update(invitation.as_bytes());
                h.update(peer.as_bytes());
                h.update(verifier.as_bytes());
                let context = hex(h.finalize().as_bytes());
                let shown_context = context.clone();
                client.ask(id, context, None, answer, |prompt| Event::Verify {
                    prompt,
                    context: shown_context,
                    fingerprint: peer.fingerprint(),
                    name: declared_name.line(MAX_NAME_COLUMNS).to_string(),
                    words: verifier.words(),
                });
            }
            Prompt::Offer {
                transfer,
                view,
                digest,
                answer,
                ..
            } => {
                let owner = lock(&svc.routes)
                    .receiver
                    .as_ref()
                    .map(|(c, id, _)| (c.clone(), *id));
                let Some((client, id)) = owner else {
                    continue;
                };
                let context = hex(&digest);
                let shown_context = context.clone();
                client.ask(id, context, Some(transfer), answer, |prompt| Event::Offer {
                    prompt,
                    context: shown_context,
                    transfer: hex(transfer.as_bytes()),
                    screen: view.render(),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn input_still_queued_when_its_deadline_passes_is_refused() {
        let (tx, mut inputs) = mpsc::channel(INPUT_QUEUE);
        tx.send(Input::Data(b"more".to_vec())).await.unwrap();
        tx.send(Input::End).await.unwrap();

        let now = Instant::now();
        let Err(refused) = next_input(&mut inputs, now).await else {
            panic!("input was taken after its deadline");
        };
        assert_eq!(refused.code, ErrorCode::Refused);
        // Before the deadline, the same queue is read as usual.
        let later = now + Duration::from_secs(60);
        assert!(matches!(
            next_input(&mut inputs, later).await,
            Ok(Input::End)
        ));
    }

    fn client() -> (Client, mpsc::Receiver<ServerMessage>) {
        let (events, rx) = mpsc::channel(ipc::EVENT_QUEUE);
        let client = Client {
            events,
            prompts: Mutex::new(HashMap::new()),
            overloaded: Notify::new(),
            accepted: Mutex::new(Vec::new()),
        };
        (client, rx)
    }

    /// Hands `client` one prompt and returns its number and the node's side.
    fn prompt(
        client: &Client,
        rx: &mut mpsc::Receiver<ServerMessage>,
    ) -> (u64, oneshot::Receiver<bool>) {
        let (answer, node) = oneshot::channel();
        client.ask(1, "ctx".into(), None, answer, |prompt| Event::Offer {
            prompt,
            context: "ctx".into(),
            transfer: "00".into(),
            screen: String::new(),
        });
        match rx.try_recv().unwrap() {
            ServerMessage::Event {
                event: Event::Offer { prompt, .. },
                ..
            } => (prompt, node),
            other => panic!("expected a prompt, got {other:?}"),
        }
    }

    #[test]
    fn only_the_connection_holding_a_prompt_can_answer_it_with_its_context() {
        let (owner, mut events) = client();
        let (other, _) = client();
        let (number, mut node) = prompt(&owner, &mut events);
        // Another connection that learns the number approves nothing.
        assert!(!other.answer(number, "ctx", true));
        assert!(node.try_recv().is_err());
        assert!(owner.answer(number, "ctx", true));
        assert_eq!(node.try_recv(), Ok(true));
        // A prompt is answered once.
        assert!(!owner.answer(number, "ctx", true));
    }

    #[test]
    fn an_answer_with_the_wrong_context_uses_the_prompt_up_as_a_no() {
        let (owner, mut events) = client();
        let (number, mut node) = prompt(&owner, &mut events);
        assert!(!owner.answer(number, "another offer", true));
        // The node sees no answer, which counts as no.
        assert!(matches!(
            node.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        ));
        assert!(!owner.answer(number, "ctx", true));
    }

    #[test]
    fn a_client_holding_as_many_prompts_as_the_node_may_ask_gets_no_more() {
        let (owner, mut events) = client();
        let held: Vec<_> = (0..PROMPT_QUEUE)
            .map(|_| prompt(&owner, &mut events))
            .collect();
        let (answer, mut node) = oneshot::channel();
        owner.ask(1, "ctx".into(), None, answer, |prompt| Event::Offer {
            prompt,
            context: "ctx".into(),
            transfer: "00".into(),
            screen: String::new(),
        });
        assert!(events.try_recv().is_err(), "the extra prompt was shown");
        assert!(matches!(
            node.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        ));
        drop(held);
    }

    #[test]
    fn questions_the_node_stopped_waiting_for_hold_no_place() {
        let (owner, mut events) = client();
        let held: Vec<_> = (0..PROMPT_QUEUE)
            .map(|_| prompt(&owner, &mut events))
            .collect();
        // The node gave up on every one, as when the offers were withdrawn.
        drop(held);
        let (number, mut node) = prompt(&owner, &mut events);
        assert!(owner.answer(number, "ctx", true));
        assert_eq!(node.try_recv(), Ok(true));
    }
}
