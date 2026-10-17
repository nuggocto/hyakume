//! One running endpoint. The domain modules decide; this module moves bytes
//! and owns every task it starts, so stopping it reaches them all. Blocking
//! file work keeps one of [`FS_JOBS`] permits until it really ends, so
//! shutdown never counts it finished early; the publishing rename runs in
//! the state lock instead, so revocation cannot slip in first. Waiting for
//! that lock is blocking work too (see [`StateCell::lock`]).

mod connections;
mod exchange;
mod receiving;
mod sending;
mod wire;

use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use iroh::{Endpoint, EndpointAddr};
use rand_core::OsRng;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout, timeout_at};
use zeroize::Zeroizing;

use crate::clock::MonotonicClock;
use crate::consent::ConsentView;
use crate::display::PeerText;
use crate::files::destination::Destination;
use crate::files::source::PreparedItem;
use crate::identity::{DeviceId, DeviceKey};
use crate::ids::{InvitationId, TransferId};
use crate::invitation::{
    Invitation, InvitationKind, InvitationSecret, InvitationState, InvitationToken, Route,
    TokenError,
};
use crate::limits::{
    Budget, INVITATION_LIFETIME, Limits, MAX_CHUNK_BYTES, MAX_PEER_NAME_BYTES, Refusal, Usage,
};
use crate::offer::{Offer, OfferError, OfferFields, PayloadKind};
use crate::pairing::{PairingError, Verifier};
use crate::protocol::{ErrorCode, ProtocolError};
use crate::send::SendError;
use crate::session::SessionError;
use crate::state::{PendingInvitation, State, StateCell, StateError};
use crate::tasks;
use crate::transfer::{BatchOutcome, ItemState};
use crate::trust::{LocalAlias, TrustError};

use self::connections::{accept_loop, bind_endpoint};
use self::exchange::{check_join, run_join};
use self::sending::{begin_send, run_send};

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

    /// Uses another device's invitation; the other device will be called
    /// `alias` here. The join is a task the node owns, so stopping the node
    /// waits for it. The receiver gets the paired device once both people
    /// confirmed.
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
    ///
    /// The deadline starts before any wait, and the state lock is never
    /// waited for here, so a stalled lock holder fails the drain in time.
    pub async fn shutdown(&self) -> Result<(), NodeError> {
        let until = Instant::now() + self.shared.limits().drain_deadline;
        self.shared.cell.set_pausing(true);
        let Ok(mut accept) = timeout_at(until, self.accept.lock()).await else {
            return Err(NodeError::DrainFailed);
        };

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

        // Blocking jobs can outlive the tasks that started them. Each holds
        // a permit until it ends, so holding every permit means none runs.
        let jobs = self
            .shared
            .fs_jobs
            .clone()
            .acquire_many_owned(FS_JOBS as u32);
        let Ok(Ok(jobs)) = timeout_at(until, jobs).await else {
            return Err(NodeError::DrainFailed);
        };
        // Invitations name this endpoint and are timed by this node's
        // clock, so no later node can serve them. Every exchange has
        // ended, so nothing waits for their outcome.
        //
        // The job keeps every permit until it has the lock, so a call that
        // gives up first leaves the next one waiting for it.
        let cell = self.shared.cell.clone();
        let cleared = tasks::spawn_blocking(move || {
            cell.change(|s| s.invitations.clear());
            drop(jobs);
        });
        match timeout_at(until, cleared).await {
            Ok(done) => tasks::observe(done),
            Err(_) => return Err(NodeError::DrainFailed),
        };
        if joined.is_err() {
            return Err(NodeError::DrainFailed);
        }
        self.shared.cell.set_pausing(false);
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

/// A node's shared parts with nothing running, for unit tests.
#[cfg(test)]
fn test_shared() -> Shared {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::source::Source;
    use crate::trust::VerifiedPeer;

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
    async fn a_state_lock_held_past_the_drain_deadline_fails_the_drain_in_time() {
        let config = NodeConfig {
            limits: Limits {
                drain_deadline: Duration::from_millis(100),
                ..Limits::COMPILED
            },
            ..NodeConfig::default()
        };
        let (node, _prompts) = Node::bind(config, DeviceKey::generate(&mut OsRng))
            .await
            .unwrap();
        // Another thread holds the state lock, like a publication or a
        // database write stuck on the disk.
        let (held, holding) = std::sync::mpsc::channel::<()>();
        let (release, stuck) = std::sync::mpsc::channel::<()>();
        let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (cell, flag) = (node.shared.cell.clone(), released.clone());
        let holder = std::thread::spawn(move || {
            let _state = cell.lock();
            held.send(()).unwrap();
            // Bounded, so a shutdown that waits for the lock ends the test.
            let _ = stuck.recv_timeout(Duration::from_secs(3));
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        holding.recv().unwrap();
        assert_eq!(node.shutdown().await, Err(NodeError::DrainFailed));
        assert!(!released.load(std::sync::atomic::Ordering::SeqCst));
        let _ = release.send(());
        holder.join().unwrap();
        assert_eq!(node.shutdown().await, Ok(()));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn a_task_stalled_inside_the_state_lock_cannot_starve_the_drain_timer() {
        let config = NodeConfig {
            limits: Limits {
                drain_deadline: Duration::from_millis(100),
                ..Limits::COMPILED
            },
            ..NodeConfig::default()
        };
        let (node, _prompts) = Node::bind(config, DeviceKey::generate(&mut OsRng))
            .await
            .unwrap();
        // A task on the only worker stalls inside the lock, like a
        // publication on a stuck disk. Bounded, so a regression ends.
        let (held, holding) = std::sync::mpsc::channel::<()>();
        let (release, stuck) = std::sync::mpsc::channel::<()>();
        let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (cell, flag) = (node.shared.cell.clone(), released.clone());
        let stalled = tokio::spawn(async move {
            cell.change(|_| {
                held.send(()).unwrap();
                let _ = stuck.recv_timeout(Duration::from_secs(3));
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
            });
        });
        holding.recv().unwrap();
        assert_eq!(node.shutdown().await, Err(NodeError::DrainFailed));
        assert!(!released.load(std::sync::atomic::Ordering::SeqCst));
        let _ = release.send(());
        stalled.await.unwrap();
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
            let prepared = Prepared {
                transfer: TransferId::random(&mut OsRng),
                kind: PayloadKind::Text,
                items: vec![PreparedItem {
                    name: Vec::new(),
                    size: 1,
                    digest: *blake3::hash(b"x").as_bytes(),
                    source: Source::Memory(Arc::new(b"x".to_vec())),
                }],
                preview: b"x".to_vec(),
                paths: vec![None],
            };
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
        let shared = test_shared();
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
