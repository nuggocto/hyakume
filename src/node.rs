//! One running endpoint. The domain modules decide; this module moves bytes
//! and owns every task it starts, so stopping it reaches them all. Blocking
//! file work keeps one of [`FS_JOBS`] permits until it really ends, so
//! shutdown never counts it finished early; the publishing rename runs in
//! the state lock instead, so revocation cannot slip in first.

use std::collections::VecDeque;
use std::fs::File;
use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::os::unix::fs::FileExt;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use iroh::endpoint::{
    Connection, Incoming, PortmapperConfig, QuicTransportConfig, ReadExactError, RecvStream,
    SendStream, VarInt, presets,
};
use iroh::{Endpoint, EndpointAddr, NetReportConfig, RelayMode};
use rand_core::{OsRng, RngCore};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{Instant, timeout, timeout_at};
use zeroize::Zeroizing;

use crate::clock::{Moment, MonotonicClock};
use crate::consent::ConsentView;
use crate::display::PeerText;
use crate::files::destination::{Destination, PublishFailure};
use crate::files::source::{PreparedItem, Source};
use crate::identity::{DeviceId, DeviceKey};
use crate::ids::{ClaimId, InvitationId, TransferId};
use crate::invitation::{
    ClaimState, Invitation, InvitationKind, InvitationSecret, InvitationState, InvitationToken,
    Route, TokenError,
};
use crate::limits::{
    Budget, BufferLease, ConnectionSlot, INVITATION_LIFETIME, Limits, MAX_CHUNK_BYTES,
    MAX_PEER_NAME_BYTES, MAX_PREVIEW_BYTES, MIN_FREE_BYTES, Refusal, Usage, chunk_cost,
};
use crate::offer::{Offer, OfferError, OfferFields, PayloadKind};
use crate::pairing::{self, ChannelBinding, ExchangeContext, PairingError, Verifier};
use crate::protocol::{self, ErrorCode, Hello, Message, ProtocolError, Purpose};
use crate::receive::{self, ContentCheck, Receiver, Target};
use crate::send::{self, SendError, Sender};
use crate::session::{self, SessionBinding, SessionError, Side};
use crate::state::{PendingInvitation, State, StateCell, StateError, TransferInfo};
use crate::storage::FileKind;
use crate::tasks;
use crate::transfer::{BatchOutcome, FailCause, ItemState};
use crate::trust::{LocalAlias, Reservation, TrustError, VerifiedPeer};

/// The QUIC application protocol name.
pub const ALPN: &[u8] = b"hyakume/1";

/// A connection that stops answering is closed after this long. Iroh's
/// keepalives keep a healthy idle connection inside it.
const QUIC_IDLE_TIMEOUT: Duration = Duration::from_secs(10);
/// Prompts waiting for an answer at once. A question that finds the queue
/// full is answered no at once: an offer is refused as busy.
pub const PROMPT_QUEUE: usize = 8;
/// Blocking file operations running at once; each holds a thread and a file.
pub const FS_JOBS: usize = 4;
/// Frames read ahead of the connection task; each holds buffer budget.
const FRAME_QUEUE: usize = 1;
/// Close code for an orderly end.
const CLOSE_DONE: u32 = 0;
/// Reset and close code after a write that could not finish.
const CLOSE_STOPPED: u32 = 1;
/// QUIC flow-control windows: one full frame plus a quarter more.
const QUIC_WINDOW_BYTES: u32 =
    (crate::limits::MAX_FRAME_BYTES + crate::limits::MAX_FRAME_BYTES / 4) as u32;
/// Why an address such as `0.0.0.0` cannot be listened on.
pub const UNSPECIFIED_BIND: &str = "listen on an address other devices can reach, such as \
     192.168.1.20:7400; 0.0.0.0 and [::] cannot be given to them";

#[derive(Clone, Debug)]
pub struct NodeConfig {
    /// The name this device declares to peers. Presentation only.
    pub name: String,
    pub limits: Limits,
    /// Where to listen. Routes give this address to other devices as it
    /// is, so it must be one they can reach, never `0.0.0.0` or `[::]`.
    pub bind: SocketAddr,
    /// When set, reading each incoming frame waits for one permit. Tests
    /// use it to make a device that stops reading.
    pub read_gate: Option<Arc<Semaphore>>,
    /// The state this node shares with its owner; a fresh state in memory
    /// when `None`.
    pub state: Option<Arc<StateCell>>,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            name: "hyakume".into(),
            limits: Limits::COMPILED,
            bind: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)),
            read_gate: None,
            state: None,
        }
    }
}

/// A question for the person using this device.
pub enum Prompt {
    /// Compare these words with the other device's screen.
    Verify {
        /// The invitation being used, on either side.
        invitation: InvitationId,
        peer: DeviceId,
        declared_name: PeerText,
        verifier: Verifier,
        answer: oneshot::Sender<bool>,
    },
    /// Accept or reject an incoming offer.
    Offer {
        transfer: TransferId,
        from: DeviceId,
        view: ConsentView,
        /// The digest of the offer shown; the answer repeats it, so it approves
        /// only that offer.
        digest: [u8; 32],
        answer: oneshot::Sender<bool>,
    },
}

/// Content ready to offer: exact sizes and digests, and where to read each
/// item from.
#[derive(Clone, Debug)]
pub struct Prepared {
    pub transfer: TransferId,
    pub kind: PayloadKind,
    pub items: Vec<PreparedItem>,
    /// For text and links: the first bytes, shown before consent.
    pub preview: Vec<u8>,
    /// Each item's source path, when it came from a file, for the record.
    pub paths: Vec<Option<Vec<u8>>>,
}

impl Prepared {
    pub fn offer(&self) -> Result<Offer, OfferError> {
        Offer::validate(OfferFields {
            transfer: self.transfer,
            kind: self.kind.code(),
            items: self
                .items
                .iter()
                .map(|i| (i.name.clone(), i.size, i.digest))
                .collect(),
            preview: self.preview.clone(),
        })
    }
}

/// Content held in memory, for tests and small programs.
#[derive(Clone, Debug)]
pub enum Payload {
    Text(String),
    Url(String),
    Files(Vec<(String, Vec<u8>)>),
}

impl Payload {
    /// Prepares the content as a new transfer.
    pub fn prepare(self) -> Prepared {
        let memory = |name: Vec<u8>, bytes: Vec<u8>| PreparedItem {
            name,
            size: bytes.len() as u64,
            digest: *blake3::hash(&bytes).as_bytes(),
            source: Source::Memory(Arc::new(bytes)),
        };
        let text = |kind, text: String| {
            let bytes = text.into_bytes();
            let preview = bytes[..bytes.len().min(MAX_PREVIEW_BYTES)].to_vec();
            (kind, vec![memory(Vec::new(), bytes)], preview)
        };
        let (kind, items, preview) = match self {
            Payload::Text(t) => text(PayloadKind::Text, t),
            Payload::Url(u) => text(PayloadKind::Url, u),
            Payload::Files(files) => (
                PayloadKind::Files,
                files
                    .into_iter()
                    .map(|(name, bytes)| memory(name.into_bytes(), bytes))
                    .collect(),
                Vec::new(),
            ),
        };
        let paths = vec![None; items.len()];
        Prepared {
            transfer: TransferId::random(&mut OsRng),
            kind,
            items,
            preview,
            paths,
        }
    }
}

/// Test and tuning controls for one send.
#[derive(Clone, Debug)]
pub struct SendOptions {
    pub chunk_bytes: usize,
    /// When set, each chunk waits for one permit. Tests use it to stop a
    /// transfer at a chosen point.
    pub gate: Option<Arc<Semaphore>>,
}

impl Default for SendOptions {
    fn default() -> Self {
        Self {
            chunk_bytes: MAX_CHUNK_BYTES,
            gate: None,
        }
    }
}

/// What happened to one send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SendReport {
    pub transfer: TransferId,
    pub outcome: BatchOutcome,
    pub items: Vec<ItemState>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NodeError {
    #[error("could not start the endpoint: {0}")]
    Bind(String),
    #[error("could not connect: {0}")]
    Connect(String),
    #[error("the connection was lost")]
    ConnectionLost,
    #[error("the other device ended the exchange early")]
    Ended,
    #[error("the other device took too long")]
    Timeout,
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error("the other device sent a message out of order")]
    Unexpected,
    #[error("the other device reported {0:?}")]
    Peer(ErrorCode),
    #[error(transparent)]
    Pairing(#[from] PairingError),
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    Trust(#[from] TrustError),
    #[error(transparent)]
    Token(#[from] TokenError),
    #[error(transparent)]
    Refused(#[from] Refusal),
    #[error(transparent)]
    Offer(#[from] OfferError),
    #[error("{0}")]
    State(String),
    #[error("the device that answered is not the one expected")]
    WrongPeer,
    #[error("the invitation could not be used")]
    Invitation,
    #[error("the endpoint did not stop within its drain deadline")]
    DrainFailed,
    #[error("the transfer was stopped on this device")]
    Stopped,
    #[error("this device is stopping")]
    Stopping,
}

impl From<StateError> for NodeError {
    fn from(e: StateError) -> Self {
        match e {
            StateError::Trust(t) => Self::Trust(t),
            other => Self::State(other.to_string()),
        }
    }
}

impl From<SendError> for NodeError {
    fn from(e: SendError) -> Self {
        match e {
            SendError::Budget(r) => Self::Refused(r),
            SendError::State(s) => s.into(),
            SendError::Offer => Self::Unexpected,
        }
    }
}

type Task = Pin<Box<dyn Future<Output = ()> + Send>>;

struct Shared {
    key: DeviceKey,
    name: PeerText,
    cell: Arc<StateCell>,
    budget: Budget,
    clock: MonotonicClock,
    prompts: mpsc::Sender<Prompt>,
    read_gate: Option<Arc<Semaphore>>,
    /// Where incoming offers are received, while someone is receiving.
    destination: Mutex<Option<Arc<Destination>>>,
    /// Permits for blocking file operations; see the module notes.
    fs_jobs: Arc<Semaphore>,
    /// Hands tasks to the accept loop, which owns them so shutdown reaches
    /// them all.
    spawn: mpsc::Sender<Task>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.cell.lock()
    }

    /// Runs `f` on the state and wakes everything waiting for changes.
    fn change<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        self.cell.change(f)
    }

    fn wake<T>(&self, f: impl FnOnce(&mut State) -> Vec<T>) -> Vec<T> {
        self.cell.wake(f)
    }

    fn limits(&self) -> Limits {
        self.budget.limits()
    }

    fn destination(&self) -> Option<Arc<Destination>> {
        self.destination
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Queues a question for the person. Returns `None` at once when the
    /// queue is full or nobody listens: a connection never waits for room,
    /// so it can still notice revocation, cancellation, and shutdown.
    fn ask(
        &self,
        prompt: impl FnOnce(oneshot::Sender<bool>) -> Prompt,
    ) -> Option<oneshot::Receiver<bool>> {
        let (tx, rx) = oneshot::channel();
        self.prompts.try_send(prompt(tx)).ok()?;
        Some(rx)
    }

    /// Resolves once `condition` holds for the state.
    async fn until(&self, condition: impl FnMut(&State) -> bool) {
        self.cell.until(condition).await;
    }

    /// Runs `job` on a blocking thread holding a file-operation permit.
    /// The permit goes only when the job really ends.
    async fn blocking<T: Send + 'static>(
        &self,
        job: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, NodeError> {
        let permit: OwnedSemaphorePermit = self
            .fs_jobs
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| NodeError::Stopping)?;
        let handle = tasks::spawn_blocking(move || {
            let result = job();
            drop(permit);
            result
        });
        tasks::observe(handle.await).ok_or(NodeError::Stopping)
    }
}

/// One endpoint instance. Dropping it aborts its tasks; call
/// [`Node::shutdown`] to stop gracefully.
pub struct Node {
    shared: Arc<Shared>,
    endpoint: Endpoint,
    /// The accept loop, which owns every other task. It stays here until
    /// its end is observed, so a shutdown cut short leaves it to the next.
    accept: tokio::sync::Mutex<Option<JoinHandle<()>>>,
}

impl Node {
    /// Binds a new endpoint with a fresh transport key and starts accepting.
    pub async fn bind(
        config: NodeConfig,
        key: DeviceKey,
    ) -> Result<(Self, mpsc::Receiver<Prompt>), NodeError> {
        if config.bind.ip().is_unspecified() {
            return Err(NodeError::Bind(UNSPECIFIED_BIND.into()));
        }

        let name = PeerText::new(config.name.into_bytes(), MAX_PEER_NAME_BYTES)
            .map_err(|e| NodeError::Bind(e.to_string()))?;
        let budget = Budget::new(config.limits);
        let cell = match config.state {
            Some(cell) => cell,
            None => Arc::new(StateCell::new(State::in_memory()?)),
        };
        // Paused work from before holds its places in this node's budget.
        cell.change(|s| s.hold_paused(&budget));

        let endpoint = bind_endpoint(config.bind).await?;
        let (prompts, prompt_rx) = mpsc::channel(PROMPT_QUEUE);
        let (spawn, spawned) = mpsc::channel(budget.limits().sessions);
        let shared = Arc::new(Shared {
            key,
            name,
            cell,
            budget,
            clock: MonotonicClock::new(),
            prompts,
            read_gate: config.read_gate,
            destination: Mutex::new(None),
            fs_jobs: Arc::new(Semaphore::new(FS_JOBS)),
            spawn,
        });

        let accept = tokio::spawn(accept_loop(shared.clone(), endpoint.clone(), spawned));
        Ok((
            Self {
                shared,
                endpoint,
                accept: tokio::sync::Mutex::new(Some(accept)),
            },
            prompt_rx,
        ))
    }

    pub fn device(&self) -> DeviceId {
        self.shared.key.id()
    }

    /// Where this endpoint can be reached on loopback.
    pub fn addr(&self) -> EndpointAddr {
        let mut addr = EndpointAddr::new(self.endpoint.id());
        for socket in self.endpoint.bound_sockets() {
            addr = addr.with_ip_addr(socket);
        }
        addr
    }

    /// Where this endpoint listens, as other devices dial it.
    pub fn route(&self) -> Route {
        Route {
            endpoint: *self.endpoint.id().as_bytes(),
            addresses: self.endpoint.bound_sockets().into_iter().take(4).collect(),
        }
    }

    /// Where incoming offers are received, or `None` to refuse them.
    pub fn set_destination(&self, destination: Option<Arc<Destination>>) {
        *self
            .shared
            .destination
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = destination;
    }

    /// Creates a permanent invitation and returns its ID and token text.
    /// The device that uses it will be called `alias` here.
    pub fn invite(&self, alias: &str) -> Result<(InvitationId, Zeroizing<String>), NodeError> {
        let alias = LocalAlias::new(alias)?;
        let id = InvitationId::random(&mut OsRng);
        let secret = InvitationSecret::random(&mut OsRng);
        let token = InvitationToken {
            kind: InvitationKind::Permanent,
            id,
            secret: InvitationSecret::from_bytes(*secret.expose()),
            route: self.route(),
        };
        let text = token.to_text()?;
        let now = self.shared.clock.now();
        self.shared.change(|s| {
            s.check_store()?;
            s.trust.check_alias(&alias, None)?;
            s.add_invitation(
                now,
                PendingInvitation {
                    invitation: Invitation::new(
                        id,
                        InvitationKind::Permanent,
                        secret,
                        now,
                        INVITATION_LIFETIME,
                    ),
                    alias,
                    outcome: None,
                },
            )
            .map_err(NodeError::from)
        })?;
        Ok((id, text))
    }

    /// An invitation's state now, and how its comparison of words ended.
    pub fn invitation(
        &self,
        id: InvitationId,
    ) -> Option<(InvitationState, Option<Result<DeviceId, String>>)> {
        let now = self.shared.clock.now();
        let mut state = self.shared.lock();
        let p = state.invitations.get_mut(&id)?;
        Some((p.invitation.state(now), p.outcome.clone()))
    }

    /// Cancels an invitation that is still open.
    pub fn cancel_invitation(&self, id: InvitationId) {
        let now = self.shared.clock.now();
        self.shared.change(|s| {
            if let Some(p) = s.invitations.get_mut(&id) {
                p.invitation.cancel(now);
            }
        });
    }

    /// Uses another device's invitation. The other device will be called
    /// `alias` here. Returns the paired device once both people confirmed.
    pub async fn join(&self, token: &str, alias: &str) -> Result<DeviceId, NodeError> {
        let (token, alias) = check_join(&self.shared, token, alias)?;
        run_join(self.shared.clone(), self.endpoint.clone(), token, alias).await
    }

    /// Starts [`Node::join`] as a task the node owns, so stopping the node
    /// waits for it. The receiver gets its result.
    pub fn start_join(
        &self,
        token: &str,
        alias: &str,
    ) -> Result<oneshot::Receiver<Result<DeviceId, NodeError>>, NodeError> {
        let (token, alias) = check_join(&self.shared, token, alias)?;
        let (done, result) = oneshot::channel();
        let (shared, endpoint) = (self.shared.clone(), self.endpoint.clone());
        let task: Task = Box::pin(async move {
            let _ = done.send(run_join(shared, endpoint, token, alias).await);
        });
        self.shared
            .spawn
            .try_send(task)
            .map_err(|_| NodeError::Stopping)?;
        Ok(result)
    }

    /// Sends `prepared` and waits for the end. Only a failure before the
    /// offer arrived is an error; after that, the report tells.
    pub async fn send(
        &self,
        to: DeviceId,
        at: EndpointAddr,
        prepared: Prepared,
        options: SendOptions,
    ) -> Result<SendReport, NodeError> {
        let outgoing = begin_send(&self.shared, to, at, prepared, options)?;
        let transfer = outgoing.transfer;
        let offered = run_send(self.shared.clone(), self.endpoint.clone(), outgoing).await;
        match offered {
            Err(e) => Err(e),
            Ok(()) => self.report(transfer).ok_or(NodeError::Unexpected),
        }
    }

    /// Starts a send the node owns and returns once it is recorded, so it can
    /// be watched and outlives the caller.
    pub fn start_send(
        &self,
        to: DeviceId,
        at: EndpointAddr,
        prepared: Prepared,
        options: SendOptions,
    ) -> Result<(TransferId, oneshot::Receiver<Result<(), NodeError>>), NodeError> {
        let outgoing = begin_send(&self.shared, to, at, prepared, options)?;
        let transfer = outgoing.transfer;
        let (done, result) = oneshot::channel();
        let (shared, endpoint) = (self.shared.clone(), self.endpoint.clone());
        let task: Task = Box::pin(async move {
            let _ = done.send(run_send(shared, endpoint, outgoing).await);
        });
        if let Err(refused) = self.shared.spawn.try_send(task) {
            // The accept loop is gone or full, so the task never runs.
            // Dropping it ends the offer it recorded.
            drop(refused);
            return Err(NodeError::Stopping);
        }
        Ok((transfer, result))
    }

    /// Asks the connection carrying `transfer` to cancel it, or cancels it
    /// here if it is paused.
    pub fn cancel(&self, transfer: TransferId) -> bool {
        self.shared.change(|s| s.request_cancel(transfer))
    }

    /// Removes trust in `device`. Its sessions and transfers stop.
    pub fn revoke(&self, device: DeviceId) -> Result<(), NodeError> {
        Ok(self.shared.change(|s| s.revoke(&device))?)
    }

    pub fn report(&self, transfer: TransferId) -> Option<SendReport> {
        let state = self.shared.lock();
        state.transfer(&transfer).map(|t| SendReport {
            transfer,
            outcome: t.outcome(),
            items: t.items().to_vec(),
        })
    }

    /// Every transfer this node knows, sending or receiving.
    pub fn transfers(&self) -> Vec<SendReport> {
        let state = self.shared.lock();
        state
            .transfers()
            .map(|t| SendReport {
                transfer: t.id(),
                outcome: t.outcome(),
                items: t.items().to_vec(),
            })
            .collect()
    }

    pub fn trusted(&self) -> Vec<DeviceId> {
        self.shared
            .lock()
            .trust()
            .trusted()
            .map(|r| r.device)
            .collect()
    }

    pub fn usage(&self) -> Usage {
        self.shared.budget.usage()
    }

    /// Waits until `condition` holds for the state, up to `deadline`.
    pub async fn wait_until(
        &self,
        deadline: Duration,
        mut condition: impl FnMut(&State) -> bool,
    ) -> Result<(), NodeError> {
        let mut changes = self.shared.cell.subscribe();
        timeout(deadline, async {
            loop {
                if condition(&self.shared.lock()) {
                    return;
                }
                if changes.changed().await.is_err() {
                    return;
                }
            }
        })
        .await
        .map_err(|_| NodeError::Timeout)
    }

    /// Waits until no session, transfer, or buffer capacity is in use.
    pub async fn wait_idle(&self, deadline: Duration) -> Result<(), NodeError> {
        let mut changes = self.shared.cell.subscribe();
        timeout(deadline, async {
            while self.shared.budget.usage() != Usage::default() {
                if changes.changed().await.is_err() {
                    return;
                }
            }
        })
        .await
        .map_err(|_| NodeError::Timeout)
    }

    /// Stops and waits for owned tasks and blocking file work. Interrupted
    /// work pauses, so it can resume. Past the drain deadline, work still
    /// running is never reported as finished.
    ///
    /// It succeeds only once nothing runs. A call cut short leaves the
    /// rest to the next call, which waits for the same work.
    pub async fn shutdown(&self) -> Result<(), NodeError> {
        let mut accept = self.accept.lock().await;
        self.shared.change(|s| s.set_pausing(true));
        let until = Instant::now() + self.shared.limits().drain_deadline;

        let endpoint = self.endpoint.clone();
        let joined = timeout_at(until, async {
            endpoint.close().await;
            if let Some(handle) = accept.as_mut() {
                tasks::observe(handle.await);
            }
            // Observed: it must not be awaited again.
            *accept = None;
        })
        .await;
        if joined.is_err() {
            // The accept task owns every connection and send task, so
            // aborting it stops them all. Its end is still observed.
            if let Some(handle) = accept.take() {
                handle.abort();
                tasks::observe(handle.await);
            }
        }

        // Invitations name this endpoint and are timed by this node's
        // clock, so no later node can serve them. Every exchange has
        // ended, so nothing waits for their outcome.
        self.shared.change(|s| s.invitations.clear());
        if joined.is_err() {
            return Err(NodeError::DrainFailed);
        }

        // Blocking jobs can outlive the tasks that started them. Each holds
        // a permit until it ends, so holding every permit means none runs.
        let jobs = self.shared.fs_jobs.acquire_many(FS_JOBS as u32);
        if timeout_at(until, jobs).await.is_err() {
            return Err(NodeError::DrainFailed);
        }
        self.shared.change(|s| s.set_pausing(false));
        Ok(())
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        if let Some(handle) = self.accept.get_mut().take() {
            handle.abort();
        }
    }
}

/// The endpoint address to dial for `route`.
pub fn route_addr(route: &Route) -> Result<EndpointAddr, NodeError> {
    let peer = iroh::PublicKey::from_bytes(&route.endpoint).map_err(|_| TokenError::Endpoint)?;
    let mut addr = EndpointAddr::new(peer);
    for a in &route.addresses {
        addr = addr.with_ip_addr(*a);
    }
    Ok(addr)
}

/// Everything a send needs once its transfer is recorded.
struct Outgoing {
    transfer: TransferId,
    to: DeviceId,
    at: EndpointAddr,
    sender: Sender,
    sources: Vec<Source>,
    options: SendOptions,
    /// Ends the transfer if this send is dropped unfinished, even before
    /// its task first runs, such as when shutdown finds it still queued.
    unfinished: SendGuard,
}

/// Checks trust and admission and records the offered transfer. Nothing is
/// recorded when this fails.
fn begin_send(
    shared: &Arc<Shared>,
    to: DeviceId,
    at: EndpointAddr,
    prepared: Prepared,
    options: SendOptions,
) -> Result<Outgoing, NodeError> {
    let authority = shared.lock().trust.authorize(&to)?;
    let offer = prepared.offer()?;
    let session = shared.budget.admit_session()?;

    let sender = shared.change(|s| {
        let mut info = TransferInfo::new(true, offer.clone(), s.now_ms());
        info.sources = prepared.paths;
        Sender::new(
            offer,
            options.chunk_bytes,
            authority,
            shared.budget.clone(),
            session,
            info,
            s,
        )
    })?;

    Ok(Outgoing {
        transfer: prepared.transfer,
        to,
        at,
        sender,
        sources: prepared.items.into_iter().map(|i| i.source).collect(),
        options,
        unfinished: SendGuard {
            shared: shared.clone(),
            transfer: prepared.transfer,
        },
    })
}

/// Runs a recorded send to its end. Returns an error only when it stopped
/// before the offer reached the other device.
async fn run_send(shared: Arc<Shared>, endpoint: Endpoint, out: Outgoing) -> Result<(), NodeError> {
    let Outgoing {
        transfer,
        to,
        at,
        mut sender,
        sources,
        options,
        unfinished: _unfinished,
    } = out;

    let (conn, mut wire, negotiation) = match dial(&shared, &endpoint, at, Purpose::Session).await {
        Ok(dialed) => dialed,
        Err(e) => {
            shared.change(|s| sender.on_disconnect(s));
            return Err(e);
        }
    };

    let mut offered = false;
    let result = send_session(
        &shared,
        &conn,
        &mut wire,
        *endpoint.id().as_bytes(),
        negotiation,
        to,
        &mut sender,
        &sources,
        options,
        &mut offered,
    )
    .await;

    if let Err(e) = &result {
        wire.send_error(e).await;
        let cause = match e {
            NodeError::Refused(_) => FailCause::Busy,
            _ => FailCause::Connection,
        };
        shared.change(|s| s.interrupt(&transfer, cause));
    }

    wire.finish_dialer(&conn, shared.limits().idle_timeout)
        .await;
    match result {
        Err(e) if !offered => Err(e),
        _ => Ok(()),
    }
}

/// Ends a sent transfer that is still unfinished when its task ends or is
/// dropped: the caller gave it up, so it is cancelled, or paused while the
/// node stops.
struct SendGuard {
    shared: Arc<Shared>,
    transfer: TransferId,
}

impl Drop for SendGuard {
    fn drop(&mut self) {
        let transfer = self.transfer;
        self.shared.change(|s| {
            let pausing = s.pausing();
            s.update_transfer(&transfer, |t| {
                if pausing {
                    t.pause_all();
                } else {
                    t.cancel(crate::transfer::CancelCause::Sender);
                }
            });
        });
    }
}

/// Owns a session's receiver and ends its unfinished work however the
/// session task stops: an error, a lost connection, or abort.
struct ReceiverGuard {
    shared: Arc<Shared>,
    receiver: Receiver,
}

impl Drop for ReceiverGuard {
    fn drop(&mut self) {
        let receiver = &mut self.receiver;
        self.shared.change(|s| receiver.on_disconnect(s));
    }
}

async fn bind_endpoint(bind: SocketAddr) -> Result<Endpoint, NodeError> {
    let idle = QUIC_IDLE_TIMEOUT
        .try_into()
        .map_err(|_| NodeError::Bind("idle timeout out of range".into()))?;
    // One stream per connection, and windows that hold about one frame,
    // instead of QUIC's much larger defaults. See the resource estimate.
    let transport = QuicTransportConfig::builder()
        .max_idle_timeout(Some(idle))
        .max_concurrent_bidi_streams(VarInt::from_u32(1))
        .max_concurrent_uni_streams(VarInt::from_u32(0))
        .stream_receive_window(VarInt::from_u32(QUIC_WINDOW_BYTES))
        .receive_window(VarInt::from_u32(QUIC_WINDOW_BYTES))
        .send_window(u64::from(QUIC_WINDOW_BYTES))
        .build();
    let mut seed = Zeroizing::new([0u8; 32]);
    OsRng.fill_bytes(&mut *seed);
    // A fresh transport key every run, never stored. The long-term identity
    // is the separate device key.
    let transport_key = iroh::SecretKey::from_bytes(&seed);
    let mut net_report = NetReportConfig::minimal();
    net_report.https_probes = false;
    Endpoint::builder(presets::Minimal)
        .secret_key(transport_key)
        .transport_config(transport)
        .relay_mode(RelayMode::Disabled)
        .portmapper_config(PortmapperConfig::Disabled)
        .net_report_config(net_report)
        .clear_ip_transports()
        .bind_addr(bind)
        .map_err(|e| NodeError::Bind(e.to_string()))?
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await
        .map_err(|e| NodeError::Bind(e.to_string()))
}

fn exporter(conn: &Connection, label: &[u8]) -> Result<[u8; 32], NodeError> {
    let mut out = [0u8; 32];
    conn.export_keying_material(&mut out, label, b"")
        .map_err(|_| NodeError::ConnectionLost)?;
    Ok(out)
}

async fn dial(
    shared: &Shared,
    endpoint: &Endpoint,
    addr: EndpointAddr,
    purpose: Purpose,
) -> Result<(Connection, Wire, [u8; 32]), NodeError> {
    let deadline = shared.limits().handshake_deadline;
    let slot = match purpose {
        Purpose::Pair => shared.budget.open_connection()?,
        Purpose::Session => shared.budget.open_session_connection()?,
    };

    let until = Instant::now() + deadline;
    let conn = timeout_at(until, endpoint.connect(addr, ALPN))
        .await
        .map_err(|_| NodeError::Timeout)?
        .map_err(|e| NodeError::Connect(e.to_string()))?;
    let (send, recv) = timeout_at(until, conn.open_bi())
        .await
        .map_err(|_| NodeError::Timeout)?
        .map_err(|_| NodeError::ConnectionLost)?;

    let mut wire = Wire::new(&conn, send, recv, shared, slot, until);
    let mine = protocol::encode_payload(&Message::Hello(Hello::new(purpose)));
    wire.write_payload(&mine).await?;

    let frame = wire.next(deadline).await?;
    let theirs = match protocol::decode_payload(&frame.payload)? {
        Message::Hello(h) if h.purpose == purpose => h,
        Message::Error(code) => return Err(NodeError::Peer(code)),
        _ => return Err(NodeError::Unexpected),
    };

    protocol::negotiate(protocol::SUPPORTED, theirs.versions)?;
    let negotiation = protocol::negotiation_digest(&mine, &frame.payload);
    Ok((conn, wire, negotiation))
}

// ---- Accepting ----

async fn accept_loop(shared: Arc<Shared>, endpoint: Endpoint, mut spawned: mpsc::Receiver<Task>) {
    let mut owned = JoinSet::new();
    loop {
        tokio::select! {
            incoming = endpoint.accept() => match incoming {
                // A connection that has not proved who it is costs a task
                // and QUIC windows until its handshake deadline, so their
                // number is bounded too.
                Some(incoming) => match shared.budget.open_connection() {
                    Ok(slot) => {
                        let (shared, endpoint) = (shared.clone(), endpoint.clone());
                        owned.spawn(serve(shared, endpoint, incoming, slot));
                    }
                    Err(_) => incoming.refuse(),
                },
                None => break,
            },
            Some(task) = spawned.recv() => {
                owned.spawn(task);
            }
            Some(done) = owned.join_next(), if !owned.is_empty() => {
                tasks::observe(done);
            }
        }
    }
    // No new sends start once the endpoint is closed. Those still queued
    // never run; dropping them ends their transfers.
    spawned.close();
    while spawned.try_recv().is_ok() {}
    while let Some(done) = owned.join_next().await {
        tasks::observe(done);
    }
}

async fn serve(shared: Arc<Shared>, endpoint: Endpoint, incoming: Incoming, slot: ConnectionSlot) {
    let deadline = shared.limits().handshake_deadline;
    // One deadline for everything before the peer proves who it is, so a
    // slow peer cannot stretch it step by step.
    let until = Instant::now() + deadline;
    let accepted = timeout_at(until, async {
        let conn = incoming.accept().ok()?.await.ok()?;
        let (send, recv) = conn.accept_bi().await.ok()?;
        Some((conn, send, recv))
    })
    .await;

    let Ok(Some((conn, send, recv))) = accepted else {
        drop(slot);
        shared.cell.notify();
        return;
    };

    let mut wire = Wire::new(&conn, send, recv, &shared, slot, until);
    let result = serve_connection(&shared, &endpoint, &conn, &mut wire).await;
    // The session's capacity was released when its state machine dropped.
    shared.cell.notify();
    if let Err(e) = &result {
        wire.send_error(e).await;
    }
    // Finishing consumes the wire, which releases the connection's slot.
    wire.finish_listener(&conn, deadline).await;
    shared.cell.notify();
}

async fn serve_connection(
    shared: &Arc<Shared>,
    endpoint: &Endpoint,
    conn: &Connection,
    wire: &mut Wire,
) -> Result<(), NodeError> {
    let deadline = shared.limits().handshake_deadline;
    let frame = wire.next(deadline).await?;
    let hello = match protocol::decode_payload(&frame.payload)? {
        Message::Hello(h) => h,
        _ => return Err(NodeError::Unexpected),
    };
    protocol::negotiate(protocol::SUPPORTED, hello.versions)?;

    let mine = protocol::encode_payload(&Message::Hello(Hello::new(hello.purpose)));
    wire.write_payload(&mine).await?;
    let negotiation = protocol::negotiation_digest(&frame.payload, &mine);
    // The session outlives the Hello, so its buffer lease goes now.
    drop(frame);

    match hello.purpose {
        Purpose::Pair => invite_exchange(shared, endpoint, conn, wire, negotiation).await,
        Purpose::Session => receive_session(shared, endpoint, conn, wire, negotiation).await,
    }
}

// ---- Pairing ----

/// The inviting side. Every claim that does not verify uses an attempt.
async fn invite_exchange(
    shared: &Arc<Shared>,
    endpoint: &Endpoint,
    conn: &Connection,
    wire: &mut Wire,
    negotiation: [u8; 32],
) -> Result<(), NodeError> {
    let deadline = shared.limits().handshake_deadline;
    let (invitation, claim, kind) = match wire.message(deadline).await? {
        Message::Claim {
            invitation,
            claim,
            kind,
        } => (invitation, claim, kind),
        _ => return Err(NodeError::Unexpected),
    };

    let now = shared.clock.now();
    let opened = shared.change(|s| {
        let pending = s.invitations.get_mut(&invitation)?;
        if pending.invitation.kind() != kind {
            return None;
        }
        pending.invitation.open_claim(now, claim).ok()?;
        Some((
            InvitationSecret::from_bytes(*pending.invitation.secret().expose()),
            pending.alias.clone(),
        ))
    });
    let Some((secret, alias)) = opened else {
        return Err(NodeError::Invitation);
    };

    let mut owner = PairingOwner {
        shared,
        claim: Some((invitation, claim)),
        reservation: None,
        paired: None,
    };
    let cx = ExchangeContext {
        binding: ChannelBinding {
            exporter: exporter(conn, pairing::EXPORTER_LABEL)?,
            inviter_endpoint: *endpoint.id().as_bytes(),
            joiner_endpoint: *conn.remote_id().as_bytes(),
            negotiation,
        },
        kind,
        invitation,
        claim,
    };

    let verified = inviter_steps(shared, wire, &secret, cx).await?;

    // The person's answer has its own, longer deadline.
    wire.end_handshake();
    let recorded = shared.change(|s| {
        s.invitations.get_mut(&invitation).map(|p| {
            p.invitation
                .claim_verified(shared.clock.now(), claim, verified.verifier())
        })
    });
    if !matches!(recorded, Some(Ok(()))) {
        return Err(NodeError::Invitation);
    }

    let person = ask_verifier(shared, conn, invitation, &verified).await;
    let answer =
        person && shared.change(|s| owner.accept(s, shared.clock.now(), verified.peer(), alias));
    wire.write(&Message::PairDecision(verified.decide(answer)))
        .await?;
    let theirs = match wire.message(shared.limits().idle_timeout).await? {
        Message::PairDecision(d) => d,
        _ => return Err(NodeError::Unexpected),
    };

    let peer = verified.conclude_permanent(answer, &theirs)?;
    shared.change(|s| owner.trust(s, peer))
}

/// Settles a pairing's claim and reservation when dropped, however the
/// exchange ends. A verified claim never reopens the invitation: each one
/// allows a single comparison of words.
struct PairingOwner<'a> {
    shared: &'a Shared,
    /// The inviting side's claim. The joining side has none.
    claim: Option<(InvitationId, ClaimId)>,
    reservation: Option<Reservation>,
    /// The device trusted once both people said yes.
    paired: Option<DeviceId>,
}

impl PairingOwner<'_> {
    /// This side's person said yes. Holds the alias and a trusted place for
    /// `peer`, and on the inviting side uses the invitation. Returns false,
    /// changing nothing, when either is no longer possible; this side then
    /// answers no.
    fn accept(&mut self, s: &mut State, now: Moment, peer: DeviceId, alias: LocalAlias) -> bool {
        let Ok(reservation) = s.trust.reserve(peer, alias) else {
            return false;
        };
        if let Some((invitation, claim)) = self.claim {
            let used = s
                .invitations
                .get_mut(&invitation)
                .is_some_and(|p| p.invitation.confirm_permanent(now, claim).is_ok());
            if !used {
                s.trust.release(reservation);
                return false;
            }
        }
        self.reservation = Some(reservation);
        true
    }

    /// Both people said yes: saves the trust this side reserved.
    fn trust(&mut self, s: &mut State, peer: VerifiedPeer) -> Result<(), NodeError> {
        // A peer only concludes as trusted after this side said yes, and
        // saying yes took a reservation.
        let reservation = self.reservation.take().ok_or(NodeError::Invitation)?;
        let device = peer.device();
        s.trust_peer(peer, reservation)?;
        self.paired = Some(device);
        Ok(())
    }
}

impl Drop for PairingOwner<'_> {
    fn drop(&mut self) {
        let now = self.shared.clock.now();
        let (claim, reservation, paired) = (self.claim, self.reservation.take(), self.paired);
        self.shared.change(|s| {
            if let Some(reservation) = reservation {
                s.trust.release(reservation);
            }
            let Some((invitation, claim)) = claim else {
                return;
            };
            if let Some(p) = s.invitations.get_mut(&invitation) {
                let compared = matches!(
                    p.invitation.claim_state(claim),
                    Some(ClaimState::Verified(_) | ClaimState::Used)
                );
                let _ = match p.invitation.claim_state(claim) {
                    Some(ClaimState::Exchanging) => p.invitation.claim_unverified(now, claim),
                    Some(ClaimState::Verified(_)) => p.invitation.decline(now, claim),
                    Some(ClaimState::Used | ClaimState::Refused(_)) | None => Ok(()),
                };
                if compared && p.outcome.is_none() {
                    p.outcome = Some(paired.ok_or_else(|| {
                        "the pairing ended without both devices saying yes".to_owned()
                    }));
                }
            }
        });
    }
}

async fn inviter_steps(
    shared: &Shared,
    wire: &mut Wire,
    secret: &InvitationSecret,
    cx: ExchangeContext,
) -> Result<pairing::Verified, NodeError> {
    let deadline = shared.limits().handshake_deadline;
    let commit = match wire.message(deadline).await? {
        Message::PairCommit(c) => c,
        _ => return Err(NodeError::Unexpected),
    };
    let (await_reveal, share) =
        pairing::inviter_on_commit(secret, &shared.key, &shared.name, cx, commit, &mut OsRng)?;
    wire.write(&Message::PairShare(share)).await?;
    let reveal = match wire.message(deadline).await? {
        Message::PairReveal(c) => c,
        _ => return Err(NodeError::Unexpected),
    };
    let (await_confirm, confirm) = await_reveal.on_reveal(reveal)?;
    wire.write(&Message::PairConfirm(confirm)).await?;
    let confirm = match wire.message(deadline).await? {
        Message::PairConfirm(c) => c,
        _ => return Err(NodeError::Unexpected),
    };
    let (await_identity, proof) = await_confirm.on_confirm(&confirm, &shared.key)?;
    wire.write(&Message::PairIdentity(proof)).await?;
    let proof = match wire.message(deadline).await? {
        Message::PairIdentity(p) => p,
        _ => return Err(NodeError::Unexpected),
    };
    Ok(await_identity.on_identity(&proof)?)
}

/// Asks the person to compare the verifier. No answer, a full prompt
/// queue, or a closed connection count as no.
async fn ask_verifier(
    shared: &Shared,
    conn: &Connection,
    invitation: InvitationId,
    verified: &pairing::Verified,
) -> bool {
    let Some(reply) = shared.ask(|answer| Prompt::Verify {
        invitation,
        peer: verified.peer(),
        declared_name: verified.peer_name().clone(),
        verifier: verified.verifier(),
        answer,
    }) else {
        return false;
    };
    tokio::select! {
        answer = timeout(shared.limits().idle_timeout, reply) => matches!(answer, Ok(Ok(true))),
        _ = conn.closed() => false,
    }
}

/// The joining side.
async fn join_exchange(
    shared: &Arc<Shared>,
    conn: &Connection,
    wire: &mut Wire,
    token: &InvitationToken,
    me: [u8; 32],
    negotiation: [u8; 32],
    alias: LocalAlias,
) -> Result<DeviceId, NodeError> {
    let deadline = shared.limits().handshake_deadline;
    let claim = ClaimId::random(&mut OsRng);
    wire.write(&Message::Claim {
        invitation: token.id,
        claim,
        kind: token.kind,
    })
    .await?;

    let cx = ExchangeContext {
        binding: ChannelBinding {
            exporter: exporter(conn, pairing::EXPORTER_LABEL)?,
            inviter_endpoint: *conn.remote_id().as_bytes(),
            joiner_endpoint: me,
            negotiation,
        },
        kind: token.kind,
        invitation: token.id,
        claim,
    };

    let (await_share, commit) =
        pairing::joiner_start(&token.secret, &shared.key, &shared.name, cx, &mut OsRng)?;
    wire.write(&Message::PairCommit(commit)).await?;
    let share = match wire.message(deadline).await? {
        Message::PairShare(c) => c,
        _ => return Err(NodeError::Unexpected),
    };
    let (await_confirm, reveal) = await_share.on_share(share)?;
    wire.write(&Message::PairReveal(reveal)).await?;
    let confirm = match wire.message(deadline).await? {
        Message::PairConfirm(c) => c,
        _ => return Err(NodeError::Unexpected),
    };
    let (await_identity, confirm) = await_confirm.on_confirm(&confirm)?;
    wire.write(&Message::PairConfirm(confirm)).await?;
    let proof = match wire.message(deadline).await? {
        Message::PairIdentity(p) => p,
        _ => return Err(NodeError::Unexpected),
    };
    let (verified, proof) = await_identity.on_identity(&proof, &shared.key)?;
    wire.write(&Message::PairIdentity(proof)).await?;
    wire.end_handshake();

    let mut owner = PairingOwner {
        shared,
        claim: None,
        reservation: None,
        paired: None,
    };

    let person = ask_verifier(shared, conn, token.id, &verified).await;
    let answer =
        person && shared.change(|s| owner.accept(s, shared.clock.now(), verified.peer(), alias));
    wire.write(&Message::PairDecision(verified.decide(answer)))
        .await?;
    let theirs = match wire.message(shared.limits().idle_timeout).await? {
        Message::PairDecision(d) => d,
        _ => return Err(NodeError::Unexpected),
    };

    let peer = verified.conclude_permanent(answer, &theirs)?;
    let device = peer.device();
    shared.change(|s| owner.trust(s, peer))?;
    Ok(device)
}

// ---- Sessions ----

#[allow(clippy::too_many_arguments)]
async fn send_session(
    shared: &Arc<Shared>,
    conn: &Connection,
    wire: &mut Wire,
    me: [u8; 32],
    negotiation: [u8; 32],
    to: DeviceId,
    sender: &mut Sender,
    sources: &[Source],
    options: SendOptions,
    offered: &mut bool,
) -> Result<(), NodeError> {
    let deadline = shared.limits().handshake_deadline;
    let binding = SessionBinding {
        exporter: exporter(conn, session::EXPORTER_LABEL)?,
        dialer_endpoint: me,
        listener_endpoint: *conn.remote_id().as_bytes(),
        negotiation,
    };

    wire.write(&Message::SessionProof(session::prove(
        &shared.key,
        &binding,
        Side::Dialer,
    )))
    .await?;

    let proof = match wire.message(deadline).await? {
        Message::SessionProof(p) => p,
        _ => return Err(NodeError::Unexpected),
    };
    if session::verify(&proof, &binding, Side::Listener)? != to {
        return Err(NodeError::WrongPeer);
    }
    wire.end_handshake();

    // Trust may have changed while connecting.
    let stop = shared.change(|s| sender.check_stop(s));
    if !stop.is_empty() {
        return Ok(());
    }

    wire.write(&sender.offer_message()).await?;
    *offered = true;

    let mut changes = shared.cell.subscribe();
    let idle = shared.limits().idle_timeout;
    // Hashed as sent, to notice a file that changed underneath.
    let mut sent: Option<blake3::Hasher> = None;

    while !sender.is_done() {
        let mut lease = None;
        let outputs = if sender.is_sending() {
            let cost = chunk_cost(sender.next_chunk_len());
            let until = wire.idle_until(idle);
            // Waiting for buffer room still notices a stop, a reply, or a
            // closed connection. None of them needs a lease.
            let waited = tokio::select! {
                biased;
                lease = timeout_at(until, shared.budget.lease_send_wait(cost)) => Ok(lease),
                () = shared.until(|s| sender.should_stop(s)) => {
                    Err(shared.change(|s| sender.check_stop(s)))
                }
                frame = wire.next_by(until) => Err(incoming(shared, sender, frame)),
            };
            match waited {
                Err(outputs) => outputs,
                Ok(granted) => {
                    lease = Some(granted.map_err(|_| Refusal::Buffer)??);
                    // Wait for permission to send (tests only), while still
                    // noticing replies and local changes.
                    if let Some(gate) = &options.gate {
                        tokio::select! {
                            permit = gate.acquire() => {
                                if let Ok(p) = permit { p.forget(); }
                                shared.change(|s| sender.next_frame(s)).into_iter().collect()
                            }
                            frame = wire.next_by(wire.idle_until(idle)) => incoming(shared, sender, frame),
                            _ = changes.changed() => shared.wake(|s| sender.check_stop(s)),
                        }
                    } else {
                        match wire.try_next() {
                            Some(frame) => incoming(shared, sender, frame),
                            None => shared
                                .change(|s| sender.next_frame(s))
                                .into_iter()
                                .collect(),
                        }
                    }
                }
            }
        } else {
            tokio::select! {
                frame = wire.next_by(wire.idle_until(idle)) => incoming(shared, sender, frame),
                _ = changes.changed() => shared.wake(|s| sender.on_wake(s)),
            }
        };

        let mut queue: VecDeque<send::Output> = outputs.into();
        while let Some(out) = queue.pop_front() {
            match out {
                send::Output::Send(msg) => {
                    if let Some(end) = send_or_stop(shared, wire, sender, &msg).await? {
                        return Ok(end);
                    }
                }
                send::Output::Read { item, offset, len } => {
                    let source = sources[item].clone();
                    let read = shared.blocking(move || source.read_at(offset, len)).await?;
                    let data = match read {
                        Ok(data) => data,
                        Err(e) => {
                            tracing::warn!(error = %e, "reading a source failed");
                            let cause = if e.kind() == std::io::ErrorKind::UnexpectedEof {
                                FailCause::SourceChanged
                            } else {
                                FailCause::Storage
                            };
                            queue.extend(shared.change(|s| sender.on_read_failure(cause, s)));
                            continue;
                        }
                    };

                    if offset == 0 {
                        sent = Some(blake3::Hasher::new());
                        tasks::crash_point("sending");
                    }
                    if let Some(hasher) = sent.as_mut() {
                        hasher.update(&data);
                    }

                    let msg = sender.chunk(item, offset, data);
                    if let Some(end) = send_or_stop(shared, wire, sender, &msg).await? {
                        return Ok(end);
                    }
                    // The chunk is written; its buffer can go.
                    lease = None;
                }
                send::Output::Check { item } => {
                    let digest = sent.take().unwrap_or_default().finalize();
                    let source = sources[item].clone();
                    let unchanged = shared.blocking(move || source.unchanged()).await?;
                    let expected = sender.item_digest(item);
                    let failure = match unchanged {
                        Ok(true) if Some(*digest.as_bytes()) == expected => None,
                        Ok(_) => Some(FailCause::SourceChanged),
                        Err(_) => Some(FailCause::Storage),
                    };
                    queue.extend(shared.change(|s| sender.end_item(failure, s)));
                }
                send::Output::Abort(code) => {
                    wire.write(&Message::Error(code)).await.ok();
                    return Ok(());
                }
                send::Output::End => return Ok(()),
            }
        }
        drop(lease);
    }
    Ok(())
}

/// Writes one message for the sender, giving up when the transfer must
/// stop. Returns `Some(())` when the session is over.
async fn send_or_stop(
    shared: &Arc<Shared>,
    wire: &mut Wire,
    sender: &mut Sender,
    msg: &Message,
) -> Result<Option<()>, NodeError> {
    let stop = shared.until(|s| sender.should_stop(s));
    match wire.write_or_stop(msg, stop).await {
        Ok(()) => Ok(None),
        Err(NodeError::Stopped) => {
            // The stream is reset, so the peer learns of the stop from the
            // closed connection.
            shared.change(|s| sender.check_stop(s));
            Ok(Some(()))
        }
        Err(NodeError::ConnectionLost) => {
            // The receiver stopped reading; its reason may be queued.
            let frame = wire.next(shared.limits().handshake_deadline).await;
            incoming(shared, sender, frame);
            if !sender.is_done() {
                shared.change(|s| sender.on_disconnect(s));
            }
            Ok(Some(()))
        }
        Err(e) => Err(e),
    }
}

fn incoming(
    shared: &Shared,
    sender: &mut Sender,
    frame: Result<Frame, NodeError>,
) -> Vec<send::Output> {
    match frame.and_then(|f| protocol::decode_payload(&f.payload).map_err(NodeError::from)) {
        Ok(msg) => shared.change(|s| sender.on_message(msg, s)),
        Err(NodeError::Protocol(e)) => {
            shared.change(|s| sender.on_disconnect(s));
            vec![send::Output::Abort(e.code())]
        }
        Err(_) => {
            shared.change(|s| sender.on_disconnect(s));
            vec![send::Output::End]
        }
    }
}

/// The partial file of the item being received, and what its bytes showed
/// so far.
struct Partial {
    transfer: TransferId,
    item: usize,
    file: Arc<File>,
    check: Option<ContentCheck>,
}

/// Where an offer would go now: the destination and its free space.
fn target_now(dest: &Option<Arc<Destination>>) -> Option<(Arc<Destination>, u64)> {
    let dest = dest.clone()?;
    let free = dest.free_bytes().ok()?;
    Some((dest, free))
}

async fn receive_session(
    shared: &Arc<Shared>,
    endpoint: &Endpoint,
    conn: &Connection,
    wire: &mut Wire,
    negotiation: [u8; 32],
) -> Result<(), NodeError> {
    let deadline = shared.limits().handshake_deadline;
    let proof = match wire.message(deadline).await? {
        Message::SessionProof(p) => p,
        _ => return Err(NodeError::Unexpected),
    };
    let binding = SessionBinding {
        exporter: exporter(conn, session::EXPORTER_LABEL)?,
        dialer_endpoint: *conn.remote_id().as_bytes(),
        listener_endpoint: *endpoint.id().as_bytes(),
        negotiation,
    };

    // Check the dialer before revealing this device's identity.
    let device = session::verify(&proof, &binding, Side::Dialer)?;
    let authority = shared.lock().trust.authorize(&device)?;
    let slot = shared.budget.admit_session()?;
    wire.slot.attach()?;
    wire.write(&Message::SessionProof(session::prove(
        &shared.key,
        &binding,
        Side::Listener,
    )))
    .await?;

    wire.end_handshake();
    let mut guard = ReceiverGuard {
        shared: shared.clone(),
        receiver: Receiver::new(authority, shared.budget.clone(), slot),
    };
    let receiver = &mut guard.receiver;

    let mut changes = shared.cell.subscribe();
    let mut answer: Option<(TransferId, oneshot::Receiver<bool>)> = None;
    // The destination of the transfer being received, fixed when it was
    // accepted, and the current item's partial file.
    let mut accepted_into: Option<Arc<Destination>> = None;
    let mut partial: Option<Partial> = None;
    let idle = shared.limits().idle_timeout;

    loop {
        // A chunk's buffer lease lives until its bytes are written.
        let mut held: Option<BufferLease> = None;
        let outputs = tokio::select! {
            frame = wire.next_by(wire.idle_until(idle)) => {
                let decoded = frame.and_then(|f| {
                    let msg = protocol::decode_payload(&f.payload).map_err(NodeError::from)?;
                    held = Some(f._lease);
                    Ok(msg)
                });
                match decoded {
                    Ok(msg) => {
                        let target = match msg {
                            Message::Offer(_) => target_now(&shared.destination()),
                            _ => None,
                        };
                        shared.change(|s| {
                            let t = target.as_ref().map(|(d, free)| Target { place: d.place(), free_bytes: *free });
                            receiver.on_message(msg, t.as_ref(), s)
                        })
                    }
                    // The guard ends unfinished work on every way out.
                    Err(NodeError::Ended) if receiver.awaiting_answer().is_none() => return Ok(()),
                    Err(e) => return Err(e),
                }
            },
            reply = async { match answer.as_mut() { Some((_, rx)) => rx.await, None => std::future::pending().await } } => {
                let Some((transfer, _)) = answer.take() else {
                    continue;
                };
                match reply {
                    // Dropped without an answer, because nobody could take
                    // the question: refused as busy. Only a person declines.
                    Err(_) => shared.change(|s| receiver.on_unasked(transfer, s)),
                    Ok(accept) => {
                        let target = target_now(&shared.destination());
                        let outputs = shared.change(|s| {
                            let t = target.as_ref().map(|(d, free)| Target { place: d.place(), free_bytes: *free });
                            receiver.on_answer(transfer, accept, t.as_ref(), s)
                        });
                        if receiver.receiving().is_some() {
                            accepted_into = target.map(|(d, _)| d);
                        }
                        outputs
                    }
                }
            }
            _ = changes.changed() => shared.wake(|s| receiver.on_wake(s)),
        };

        let mut queue: VecDeque<receive::Output> = outputs.into();
        while let Some(out) = queue.pop_front() {
            match out {
                receive::Output::Send(msg) => {
                    let stop = shared.until(|s| receiver.should_stop(s));
                    match wire.write_or_stop(&msg, stop).await {
                        Ok(()) => {}
                        Err(NodeError::Stopped) => {
                            // As on the sending side: the reset stream tells
                            // the peer.
                            shared.change(|s| receiver.on_wake(s));
                            return Ok(());
                        }
                        Err(e) => return Err(e),
                    }
                }
                receive::Output::Ask { transfer, view } => {
                    let from = device;
                    let digest = receiver_offer_digest(shared, transfer);
                    let asked = shared.ask(|answer| Prompt::Offer {
                        transfer,
                        from,
                        view,
                        digest,
                        answer,
                    });
                    match asked {
                        Some(rx) => answer = Some((transfer, rx)),
                        None => queue.extend(shared.change(|s| receiver.on_unasked(transfer, s))),
                    }
                }
                receive::Output::Write { item, offset, data } => {
                    let lease = held.take();
                    let written = write_partial(
                        shared,
                        receiver,
                        &accepted_into,
                        &mut partial,
                        item,
                        offset,
                        data,
                        lease,
                    )
                    .await?;
                    if !written {
                        partial = None;
                        queue.extend(shared.change(|s| receiver.on_storage_failure(s)));
                    }
                }
                receive::Output::Verify { item } => {
                    let finished =
                        finish_partial(shared, receiver, &accepted_into, &mut partial, item)
                            .await?;
                    let Some((dest, content)) = finished else {
                        partial = None;
                        queue.extend(shared.change(|s| receiver.on_storage_failure(s)));
                        continue;
                    };
                    partial = None;
                    queue.extend(shared.change(|s| {
                        receiver.on_verified(item, content, s, |name, output| {
                            tasks::crash_point("before-rename");
                            let published = dest.publish(name, output.as_str());
                            tasks::crash_point("published-unrecorded");
                            if let Err(e @ PublishFailure::Unsynced(_)) = &published {
                                tracing::warn!(error = %e, "a received file is in place but not confirmed; it stays paused until recovery");
                            }
                            published
                        })
                    }));
                }
                receive::Output::Abort(code) => {
                    wire.write(&Message::Error(code)).await.ok();
                    return Ok(());
                }
                receive::Output::End => return Ok(()),
            }
        }
    }
}

fn check_join(
    shared: &Shared,
    token: &str,
    alias: &str,
) -> Result<(InvitationToken, LocalAlias), NodeError> {
    let token = InvitationToken::parse(token)?;
    if token.kind != InvitationKind::Permanent {
        return Err(NodeError::Invitation);
    }
    let alias = LocalAlias::new(alias)?;
    shared.change(|s| {
        s.check_store()?;
        s.trust.check_alias(&alias, None).map_err(NodeError::from)
    })?;
    Ok((token, alias))
}

async fn run_join(
    shared: Arc<Shared>,
    endpoint: Endpoint,
    token: InvitationToken,
    alias: LocalAlias,
) -> Result<DeviceId, NodeError> {
    let addr = route_addr(&token.route)?;
    let (conn, mut wire, negotiation) = dial(&shared, &endpoint, addr, Purpose::Pair).await?;
    let me = *endpoint.id().as_bytes();
    let result = join_exchange(&shared, &conn, &mut wire, &token, me, negotiation, alias).await;
    if let Err(e) = &result {
        wire.send_error(e).await;
    }
    wire.finish_dialer(&conn, shared.limits().handshake_deadline)
        .await;
    result
}

fn receiver_offer_digest(shared: &Shared, transfer: TransferId) -> [u8; 32] {
    shared
        .lock()
        .info(&transfer)
        .map_or([0; 32], |i| i.offer.digest())
}

/// Opens the item's partial file if it is not open yet, recording its
/// identity in the file ledger. Returns false if it cannot be created.
async fn open_partial(
    shared: &Arc<Shared>,
    receiver: &Receiver,
    dest: &Arc<Destination>,
    partial: &mut Option<Partial>,
    item: usize,
) -> Result<bool, NodeError> {
    let Some(offer) = receiver.receiving() else {
        return Ok(false);
    };
    let transfer = offer.transfer();
    if partial
        .as_ref()
        .is_some_and(|p| p.transfer == transfer && p.item == item)
    {
        return Ok(true);
    }

    let check = ContentCheck::new(offer);
    let name = crate::files::names::partial_name(transfer, item);
    let job_dest = dest.clone();
    let created = shared
        .blocking(move || job_dest.create_partial(&name))
        .await?;
    match created {
        Ok((file, identity)) => {
            let key = identity.key();
            let recorded =
                shared.change(|s| s.file_created(FileKind::Partial, transfer, item, key));
            if !matches!(recorded, Ok(true)) {
                // Without its inode on record, recovery could never tell this
                // file from another under the output name, and cleanup would
                // never find it, so nothing of it may be used. It goes now,
                // before any byte is in it.
                let (dest, name) = (
                    dest.clone(),
                    crate::files::names::partial_name(transfer, item),
                );
                let removed = shared
                    .blocking(move || dest.remove(name.as_bytes(), Some(key)))
                    .await?;
                if let Err(e) = removed {
                    tracing::warn!(error = %e, "could not remove an unrecorded partial file");
                }
                return Ok(false);
            }
            *partial = Some(Partial {
                transfer,
                item,
                file: Arc::new(file),
                check: Some(check),
            });
            Ok(true)
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not create a partial file");
            Ok(false)
        }
    }
}

/// Writes received bytes to the partial file, refusing to fill the disk.
/// Returns false on a storage failure.
#[allow(clippy::too_many_arguments)]
async fn write_partial(
    shared: &Arc<Shared>,
    receiver: &Receiver,
    dest: &Option<Arc<Destination>>,
    partial: &mut Option<Partial>,
    item: usize,
    offset: u64,
    data: Vec<u8>,
    lease: Option<BufferLease>,
) -> Result<bool, NodeError> {
    let Some(dest) = dest else {
        return Ok(false);
    };
    if !open_partial(shared, receiver, dest, partial, item).await? {
        return Ok(false);
    }
    let Some(p) = partial.as_mut() else {
        return Ok(false);
    };

    let (file, mut check, dest) = (p.file.clone(), p.check.take(), dest.clone());
    let (written, check) = shared
        .blocking(move || {
            let free = dest.free_bytes();
            let result = match free {
                Ok(free) if free >= (data.len() as u64).saturating_add(MIN_FREE_BYTES) => {
                    file.write_all_at(&data, offset)
                }
                Ok(_) => Err(std::io::Error::other("not enough free disk space")),
                Err(e) => Err(e),
            };
            if result.is_ok()
                && let Some(c) = check.as_mut()
            {
                c.update(&data);
            }
            drop(lease);
            (result, check)
        })
        .await?;
    p.check = check;
    if let Err(e) = written {
        tracing::warn!(error = %e, "writing a partial file failed");
        return Ok(false);
    }
    Ok(true)
}

/// Syncs the item's partial file, creating it first for an empty item, and
/// reports what its bytes showed. `None` on a storage failure.
async fn finish_partial(
    shared: &Arc<Shared>,
    receiver: &Receiver,
    dest: &Option<Arc<Destination>>,
    partial: &mut Option<Partial>,
    item: usize,
) -> Result<Option<(Arc<Destination>, receive::Content)>, NodeError> {
    let Some(dest) = dest else {
        return Ok(None);
    };
    if !open_partial(shared, receiver, dest, partial, item).await? {
        return Ok(None);
    }
    let Some(p) = partial.take() else {
        return Ok(None);
    };
    let Some(check) = p.check else {
        return Ok(None);
    };

    let file = p.file;
    let synced = shared
        .blocking(move || file.sync_all().map(|()| check.finish()))
        .await?;
    match synced {
        Ok(content) => Ok(Some((dest.clone(), content))),
        Err(e) => {
            tracing::warn!(error = %e, "syncing a partial file failed");
            Ok(None)
        }
    }
}

// ---- Framing over one QUIC stream ----

struct Frame {
    payload: Vec<u8>,
    _lease: BufferLease,
}

/// One bidirectional stream. A reader task owns the receive half and hands
/// over whole frames, so waiting for a frame can be cancelled without
/// splitting one.
struct Wire {
    conn: Connection,
    /// Counts the connection against the node's limit while it is open.
    slot: ConnectionSlot,
    send: SendStream,
    frames: mpsc::Receiver<Result<Frame, NodeError>>,
    reader: JoinHandle<()>,
    /// How long writing one frame may take.
    write_deadline: Duration,
    /// Until the peer has proved who it is, no read, write, or close on
    /// this connection waits past this instant.
    handshake: Option<Instant>,
    /// When this session last read or wrote a frame. Other sessions' work
    /// does not count, so it cannot keep this one from going idle.
    active: Instant,
    /// A write stopped partway, so nothing more can follow on this stream.
    broken: bool,
}

impl Wire {
    fn new(
        conn: &Connection,
        send: SendStream,
        recv: RecvStream,
        shared: &Shared,
        slot: ConnectionSlot,
        handshake: Instant,
    ) -> Self {
        let (tx, frames) = mpsc::channel(FRAME_QUEUE);
        let reader = tokio::spawn(read_frames(
            recv,
            shared.budget.clone(),
            shared.read_gate.clone(),
            tx,
        ));
        Self {
            conn: conn.clone(),
            slot,
            send,
            frames,
            reader,
            write_deadline: shared.limits().idle_timeout,
            handshake: Some(handshake),
            active: Instant::now(),
            broken: false,
        }
    }

    /// The peer has proved who it is; waits are no longer capped by the
    /// handshake deadline.
    fn end_handshake(&mut self) {
        self.handshake = None;
    }

    /// `until`, or the handshake deadline if that comes first.
    fn cap(&self, until: Instant) -> Instant {
        self.handshake.map_or(until, |h| h.min(until))
    }

    /// When the session counts as idle: `idle` after its last frame in or
    /// out.
    fn idle_until(&self, idle: Duration) -> Instant {
        self.active + idle
    }

    async fn write(&mut self, msg: &Message) -> Result<(), NodeError> {
        self.write_or_stop(msg, std::future::pending()).await
    }

    /// Writes `msg`, giving up as soon as `stop` resolves.
    async fn write_or_stop(
        &mut self,
        msg: &Message,
        stop: impl Future<Output = ()>,
    ) -> Result<(), NodeError> {
        let frame = protocol::encode_frame(msg)?;
        self.write_parts(&[&frame], stop).await
    }

    async fn write_payload(&mut self, payload: &[u8]) -> Result<(), NodeError> {
        let len = u32::try_from(payload.len()).map_err(|_| ProtocolError::TooLarge)?;
        self.write_parts(&[&len.to_be_bytes(), payload], std::future::pending())
            .await
    }

    /// Writes one frame. A peer that stops reading cannot hold the
    /// connection: the write gives up after the write deadline, or when
    /// `stop` resolves. A frame cut partway cannot be continued, so the
    /// stream is then reset.
    async fn write_parts(
        &mut self,
        parts: &[&[u8]],
        stop: impl Future<Output = ()>,
    ) -> Result<(), NodeError> {
        if self.broken {
            return Err(NodeError::ConnectionLost);
        }
        let until = self.cap(Instant::now() + self.write_deadline);
        let send = &mut self.send;
        let written = async {
            for part in parts {
                send.write_all(part).await.map_err(|_| ())?;
            }
            Ok::<(), ()>(())
        };
        let error = tokio::select! {
            biased;
            written = timeout_at(until, written) => match written {
                Ok(Ok(())) => {
                    self.active = Instant::now();
                    return Ok(());
                }
                Ok(Err(())) => NodeError::ConnectionLost,
                Err(_) => NodeError::Timeout,
            },
            () = stop => NodeError::Stopped,
        };
        self.broken = true;
        let _ = self.send.reset(VarInt::from_u32(CLOSE_STOPPED));
        Err(error)
    }

    /// The next frame, if it comes within `deadline`.
    async fn next(&mut self, deadline: Duration) -> Result<Frame, NodeError> {
        self.next_by(Instant::now() + deadline).await
    }

    /// The next frame, if it comes by `until`. Queued frames come first;
    /// after them, a closed connection ends the wait even if the reader is
    /// still waiting for buffer room.
    async fn next_by(&mut self, until: Instant) -> Result<Frame, NodeError> {
        let until = self.cap(until);
        let next = tokio::select! {
            biased;
            frame = timeout_at(until, self.frames.recv()) => frame,
            _ = self.conn.closed() => Ok(self.frames.try_recv().ok()),
        };
        match next {
            Ok(Some(frame)) => {
                self.active = Instant::now();
                frame
            }
            Ok(None) => Err(NodeError::Ended),
            Err(_) => Err(NodeError::Timeout),
        }
    }

    fn try_next(&mut self) -> Option<Result<Frame, NodeError>> {
        self.frames.try_recv().ok()
    }

    async fn message(&mut self, deadline: Duration) -> Result<Message, NodeError> {
        let frame = self.next(deadline).await?;
        match protocol::decode_payload(&frame.payload)? {
            Message::Error(code) => Err(NodeError::Peer(code)),
            msg => Ok(msg),
        }
    }

    /// Tells the peer why the exchange stopped, when it has a code.
    async fn send_error(&mut self, error: &NodeError) {
        let code = match error {
            NodeError::Protocol(e) => e.code(),
            NodeError::Unexpected => ErrorCode::UnexpectedMessage,
            NodeError::Pairing(_) => ErrorCode::PairingFailed,
            NodeError::Session(_) | NodeError::Trust(_) => ErrorCode::NotTrusted,
            NodeError::Refused(_) => ErrorCode::Busy,
            NodeError::Invitation => ErrorCode::InvitationUnavailable,
            NodeError::Timeout => ErrorCode::Timeout,
            _ => return,
        };
        let _ = self.write(&Message::Error(code)).await;
    }

    /// The listener's orderly end: finish sending, wait for the dialer to
    /// finish, and let the dialer close the connection. After a broken
    /// write there is nothing orderly left, so it closes at once.
    async fn finish_listener(mut self, conn: &Connection, deadline: Duration) {
        if self.broken {
            conn.close(VarInt::from_u32(CLOSE_STOPPED), b"stopped");
        } else {
            let _ = self.send.finish();
            // Keep taking frames so the dialer can finish writing, until it
            // closes. A reader stuck waiting for buffer room does not delay
            // that.
            let until = self.cap(Instant::now() + deadline);
            let frames = &mut self.frames;
            let _ = timeout_at(until, async {
                tokio::select! {
                    () = async { while frames.recv().await.is_some() {} std::future::pending().await } => {}
                    _ = conn.closed() => {}
                }
            })
            .await;
        }
        self.reader.abort();
        tasks::observe((&mut self.reader).await);
    }

    /// The dialer's orderly end: finish sending, read until the listener
    /// finishes or closes, then close.
    async fn finish_dialer(&mut self, conn: &Connection, deadline: Duration) {
        if self.broken {
            conn.close(VarInt::from_u32(CLOSE_STOPPED), b"stopped");
        } else {
            let _ = self.send.finish();
            // As in finish_listener, a reader stuck waiting for buffer room
            // does not delay a close the listener already made.
            let until = self.cap(Instant::now() + deadline);
            let frames = &mut self.frames;
            let _ = timeout_at(until, async {
                tokio::select! {
                    () = async { while frames.recv().await.is_some() {} } => {}
                    _ = conn.closed() => {}
                }
            })
            .await;
            conn.close(VarInt::from_u32(CLOSE_DONE), b"done");
        }
        self.reader.abort();
        tasks::observe((&mut self.reader).await);
    }
}

impl Drop for Wire {
    /// The reader task belongs to this stream; it never outlives it.
    fn drop(&mut self) {
        self.reader.abort();
    }
}

async fn read_frames(
    mut recv: RecvStream,
    budget: Budget,
    gate: Option<Arc<Semaphore>>,
    tx: mpsc::Sender<Result<Frame, NodeError>>,
) {
    loop {
        if let Some(gate) = &gate
            && let Ok(permit) = gate.acquire().await
        {
            permit.forget();
        }
        let frame = read_frame(&mut recv, &budget).await;
        let stop = frame.is_err();
        if let Err(NodeError::Ended) = frame {
            return;
        }
        if tx.send(frame).await.is_err() || stop {
            return;
        }
    }
}

async fn read_frame(recv: &mut RecvStream, budget: &Budget) -> Result<Frame, NodeError> {
    let mut header = [0u8; protocol::HEADER_BYTES];
    match recv.read_exact(&mut header).await {
        Ok(()) => {}
        Err(ReadExactError::FinishedEarly(0)) => return Err(NodeError::Ended),
        Err(ReadExactError::FinishedEarly(_)) => return Err(ProtocolError::Truncated.into()),
        Err(ReadExactError::ReadError(_)) => return Err(NodeError::ConnectionLost),
    }
    let len = protocol::payload_length(header)?;
    // The decoded message copies the payload, so reserve room for both.
    // Waiting here leaves the rest of the frame in QUIC's flow-control
    // window, which pushes back on the peer.
    let lease = budget.lease_wait((len as u64).saturating_mul(2)).await?;
    let mut payload = vec![0u8; len];
    match recv.read_exact(&mut payload).await {
        Ok(()) => Ok(Frame {
            payload,
            _lease: lease,
        }),
        Err(ReadExactError::FinishedEarly(_)) => Err(ProtocolError::Truncated.into()),
        Err(ReadExactError::ReadError(_)) => Err(NodeError::ConnectionLost),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::invitation::ClaimRefusal;

    fn shared() -> Shared {
        let (prompts, _) = mpsc::channel(1);
        let (spawn, _) = mpsc::channel(1);
        Shared {
            key: DeviceKey::from_seed(&Zeroizing::new([1; 32])),
            name: PeerText::default(),
            cell: Arc::new(StateCell::new(State::in_memory().unwrap())),
            budget: Budget::new(Limits::COMPILED),
            clock: MonotonicClock::new(),
            prompts,
            read_gate: None,
            destination: Mutex::new(None),
            fs_jobs: Arc::new(Semaphore::new(FS_JOBS)),
            spawn,
        }
    }

    /// Adds an invitation for `alias` with one claim in progress.
    fn claimed(shared: &Shared, alias: &str, n: u8) -> (InvitationId, ClaimId) {
        let now = shared.clock.now();
        let (invitation, claim) = (
            InvitationId::from_bytes([n; 16]),
            ClaimId::from_bytes([n; 16]),
        );
        let mut open = Invitation::new(
            invitation,
            InvitationKind::Permanent,
            InvitationSecret::from_bytes([n; 32]),
            now,
            INVITATION_LIFETIME,
        );
        open.open_claim(now, claim).unwrap();
        let alias = LocalAlias::new(alias).unwrap();
        shared
            .change(|s| {
                s.add_invitation(
                    now,
                    PendingInvitation {
                        invitation: open,
                        alias,
                        outcome: None,
                    },
                )
            })
            .unwrap();
        (invitation, claim)
    }

    fn verify(shared: &Shared, (invitation, claim): (InvitationId, ClaimId)) {
        let now = shared.clock.now();
        let mut s = shared.lock();
        let p = s.invitations.get_mut(&invitation).unwrap();
        p.invitation
            .claim_verified(now, claim, Verifier::from_bytes([4; 4]))
            .unwrap();
    }

    fn owner(shared: &Shared, claim: (InvitationId, ClaimId)) -> PairingOwner<'_> {
        PairingOwner {
            shared,
            claim: Some(claim),
            reservation: None,
            paired: None,
        }
    }

    fn invitation_state(shared: &Shared, invitation: InvitationId) -> InvitationState {
        let now = shared.clock.now();
        let mut s = shared.lock();
        s.invitations
            .get_mut(&invitation)
            .unwrap()
            .invitation
            .state(now)
    }

    #[test]
    fn an_exchange_that_ends_unverified_uses_an_attempt() {
        let shared = shared();
        let (invitation, claim) = claimed(&shared, "phone", 1);
        drop(owner(&shared, (invitation, claim)));
        let mut s = shared.lock();
        let p = s.invitations.get_mut(&invitation).unwrap();
        assert_eq!(
            p.invitation.claim_state(claim),
            Some(ClaimState::Refused(ClaimRefusal::NotVerified))
        );
    }

    #[test]
    fn an_exchange_that_ends_after_verification_never_leaves_the_invitation_open() {
        let shared = shared();
        let device = |seed| DeviceKey::from_seed(&Zeroizing::new([seed; 32])).id();

        // Nobody answered.
        let unanswered = claimed(&shared, "phone", 1);
        verify(&shared, unanswered);
        drop(owner(&shared, unanswered));
        assert_eq!(
            invitation_state(&shared, unanswered.0),
            InvitationState::Declined
        );

        // This side said yes; the other side never answered.
        let accepted = claimed(&shared, "tablet", 2);
        verify(&shared, accepted);
        let mut owner = owner(&shared, accepted);
        let now = shared.clock.now();
        let alias = LocalAlias::new("tablet").unwrap();
        assert!(shared.change(|s| owner.accept(s, now, device(6), alias.clone())));
        // While it waits, no other pairing can take the alias.
        assert_eq!(
            shared.lock().trust.can_trust(device(7), &alias),
            Err(TrustError::AliasInUse)
        );
        drop(owner);
        assert_eq!(
            invitation_state(&shared, accepted.0),
            InvitationState::Consumed
        );
        assert_eq!(shared.lock().trust.can_trust(device(7), &alias), Ok(()));
        assert!(shared.lock().trust.authorize(&device(6)).is_err());
    }

    #[tokio::test]
    async fn a_drain_that_overruns_fails_and_never_counts_stalled_work_as_finished() {
        let config = NodeConfig {
            limits: Limits {
                drain_deadline: Duration::from_millis(300),
                ..Limits::COMPILED
            },
            ..NodeConfig::default()
        };
        let (node, _prompts) = Node::bind(config, DeviceKey::generate(&mut OsRng))
            .await
            .unwrap();
        // A file operation that does not return, like a write to a stuck disk.
        let (release, stuck) = std::sync::mpsc::channel::<()>();
        let shared = node.shared.clone();
        let job = tokio::spawn(async move {
            shared
                .blocking(move || {
                    let _ = stuck.recv();
                })
                .await
        });
        while node.shared.fs_jobs.available_permits() == FS_JOBS {
            tokio::task::yield_now().await;
        }
        assert_eq!(node.shutdown().await, Err(NodeError::DrainFailed));
        // The stuck job still holds its permit: nothing it uses was freed.
        assert_eq!(node.shared.fs_jobs.available_permits(), FS_JOBS - 1);
        release.send(()).unwrap();
        job.await.unwrap().unwrap();
        assert_eq!(node.shared.fs_jobs.available_permits(), FS_JOBS);
    }

    #[tokio::test]
    async fn a_shutdown_cut_short_leaves_its_work_to_the_next_one() {
        let config = NodeConfig {
            limits: Limits {
                drain_deadline: Duration::from_millis(300),
                ..Limits::COMPILED
            },
            ..NodeConfig::default()
        };
        let (node, _prompts) = Node::bind(config, DeviceKey::generate(&mut OsRng))
            .await
            .unwrap();
        let (release, stuck) = std::sync::mpsc::channel::<()>();
        let shared = node.shared.clone();
        let job = tokio::spawn(async move {
            shared
                .blocking(move || {
                    let _ = stuck.recv();
                })
                .await
        });
        while node.shared.fs_jobs.available_permits() == FS_JOBS {
            tokio::task::yield_now().await;
        }
        // The caller gives up while the drain waits for the stuck job.
        let cut = timeout(Duration::from_millis(50), node.shutdown()).await;
        assert!(cut.is_err());
        assert!(node.shared.lock().pausing());
        // The next shutdown waits for the same job instead of succeeding.
        assert_eq!(node.shutdown().await, Err(NodeError::DrainFailed));
        release.send(()).unwrap();
        job.await.unwrap().unwrap();
        assert_eq!(node.shutdown().await, Ok(()));
        assert!(!node.shared.lock().pausing());
    }

    #[tokio::test]
    async fn invitations_open_when_the_node_stops_leave_room_for_the_next_node() {
        let cell = Arc::new(StateCell::new(State::in_memory().unwrap()));
        let config = || NodeConfig {
            state: Some(cell.clone()),
            ..NodeConfig::default()
        };
        let key = || DeviceKey::from_seed(&Zeroizing::new([1; 32]));

        // Every invitation slot is taken when the device locks.
        let (first, _prompts) = Node::bind(config(), key()).await.unwrap();
        let opened: Vec<_> = (0..crate::limits::MAX_OPEN_INVITATIONS)
            .map(|n| first.invite(&format!("peer{n}")).unwrap().0)
            .collect();
        first.shutdown().await.unwrap();
        drop(first);

        // The node after unlock answers for none of them and can invite.
        let (second, _prompts) = Node::bind(config(), key()).await.unwrap();
        for invitation in opened {
            assert_eq!(second.invitation(invitation), None);
        }
        second.invite("peer").unwrap();
        second.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn sends_still_queued_when_shutdown_begins_never_stay_offered() {
        let (node, _prompts) = Node::bind(NodeConfig::default(), DeviceKey::generate(&mut OsRng))
            .await
            .unwrap();
        let peer = DeviceKey::generate(&mut OsRng).id();
        node.shared
            .change(|s| {
                let verified = VerifiedPeer::new(peer, PeerText::default());
                s.trust.trust(verified, LocalAlias::new("peer").unwrap())
            })
            .unwrap();
        // Nobody listens at this address; it does not matter, because
        // shutdown begins before the accept loop takes most of these.
        let nowhere = route_addr(&node.route()).unwrap();
        for _ in 0..16 {
            let prepared = Payload::Text("x".into()).prepare();
            node.start_send(peer, nowhere.clone(), prepared, SendOptions::default())
                .unwrap();
        }
        node.shutdown().await.unwrap();
        let reports = node.transfers();
        assert_eq!(reports.len(), 16);
        for report in reports {
            assert_ne!(report.outcome, BatchOutcome::Offered, "{report:?}");
        }
    }

    /// Opens the partial file of a one-item receive after `lose_record`
    /// has made its record impossible, and checks that the file is neither
    /// used nor left behind.
    async fn open_partial_without_its_record(lose_record: impl FnOnce(&mut State, TransferId)) {
        let shared = Arc::new(shared());
        let dir = std::env::temp_dir().join(format!(
            "hyakume-node-unrecorded-{}-{:x}",
            std::process::id(),
            OsRng.next_u64()
        ));
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .unwrap();
        let dest = Arc::new(Destination::qualify(&dir).unwrap());

        // A receiver that accepted a one-item offer.
        let peer = DeviceKey::from_seed(&Zeroizing::new([2; 32])).id();
        let authority = shared
            .change(|s| {
                let verified = VerifiedPeer::new(peer, PeerText::default());
                s.trust.trust(verified, LocalAlias::new("peer").unwrap())
            })
            .unwrap();
        let session = shared.budget.admit_session().unwrap();
        let mut receiver = Receiver::new(authority, shared.budget.clone(), session);
        let transfer = TransferId::from_bytes([7; 16]);
        let offer = Offer::for_files(transfer, &[("f", b"abc")]).unwrap();
        let target = Target {
            place: dest.place(),
            free_bytes: u64::MAX,
        };
        shared.change(|s| receiver.on_message(Message::Offer(offer.fields()), Some(&target), s));
        shared.change(|s| receiver.on_answer(transfer, true, Some(&target), s));
        assert!(receiver.receiving().is_some());

        shared.change(|s| lose_record(s, transfer));
        let mut partial = None;
        let opened = open_partial(&shared, &receiver, &dest, &mut partial, 0)
            .await
            .unwrap();
        assert!(!opened);
        assert!(partial.is_none());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_partial_file_whose_record_cannot_be_saved_is_never_used() {
        // The disk under the state database starts failing.
        open_partial_without_its_record(|s, _| s.store().refuse_writes()).await;
    }

    #[tokio::test]
    async fn a_partial_file_whose_reservation_is_gone_is_never_used() {
        // A cancel and cleanup removed the reservation before the file was
        // created.
        open_partial_without_its_record(|s, transfer| {
            s.store().delete_files_of(transfer, 0).unwrap();
            for file in s.store().files_due(u64::MAX).unwrap() {
                s.store().file_deleted(file.id).unwrap();
            }
        })
        .await;
    }

    #[tokio::test]
    async fn an_address_other_devices_cannot_dial_is_refused() {
        let config = NodeConfig {
            bind: "0.0.0.0:0".parse().unwrap(),
            ..NodeConfig::default()
        };
        let bound = Node::bind(config, DeviceKey::generate(&mut OsRng)).await;
        assert!(matches!(bound, Err(NodeError::Bind(_))));
    }

    #[test]
    fn a_wake_that_changes_nothing_notifies_no_one() {
        let shared = shared();
        let mut changes = shared.cell.subscribe();
        let idle: Vec<()> = shared.wake(|_| Vec::new());
        assert!(idle.is_empty());
        assert!(!changes.has_changed().unwrap());
        shared.wake(|_| vec![()]);
        assert!(changes.has_changed().unwrap());
        changes.mark_unchanged();
        shared.change(|_| ());
        assert!(changes.has_changed().unwrap());
    }
}
