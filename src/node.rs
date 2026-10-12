//! One running endpoint: the owner of its Iroh endpoint, its tasks, and its
//! state.
//!
//! The domain modules decide; this module moves bytes. It binds an Iroh
//! endpoint with no relays and no address publication, accepts connections
//! in one owned task, and runs each connection as an owned task under that
//! one. Pairing and sessions drive the state machines in [`crate::pairing`],
//! [`crate::receive`], and [`crate::send`].
//!
//! People answer through [`Prompt`]s, delivered on a bounded channel with a
//! reply slot. Tests answer them directly.
//!
//! This runtime proves the protocol on loopback. Address lookup on a LAN,
//! the persistent service, and file publication come with later work.

use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use iroh::endpoint::{
    Connection, Incoming, PortmapperConfig, QuicTransportConfig, ReadExactError, RecvStream,
    SendStream, VarInt, presets,
};
use iroh::{Endpoint, EndpointAddr, NetReportConfig, RelayMode};
use rand_core::{OsRng, RngCore};
use tokio::sync::{Semaphore, mpsc, oneshot, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{Instant, timeout, timeout_at};
use zeroize::Zeroizing;

use crate::clock::{Moment, MonotonicClock};
use crate::consent::ConsentView;
use crate::display::PeerText;
use crate::identity::{DeviceId, DeviceKey};
use crate::ids::{ClaimId, InvitationId, TransferId};
use crate::invitation::{
    ClaimState, Invitation, InvitationKind, InvitationSecret, InvitationState, InvitationToken,
    Route, TokenError,
};
use crate::limits::{
    Budget, BufferLease, ConnectionSlot, INVITATION_LIFETIME, Limits, MAX_CHUNK_BYTES,
    MAX_PEER_NAME_BYTES, Refusal, Usage, chunk_cost,
};
use crate::offer::{Offer, OfferError, PayloadKind};
use crate::pairing::{self, ChannelBinding, ExchangeContext, PairingError, Verifier};
use crate::protocol::{self, ErrorCode, Hello, Message, ProtocolError, Purpose};
use crate::receive::{self, Receiver};
use crate::send::{self, SendError, Sender};
use crate::session::{self, SessionBinding, SessionError, Side};
use crate::state::{PendingInvitation, Received, State, StateError};
use crate::tasks;
use crate::transfer::{BatchOutcome, CancelCause, FailCause, ItemState};
use crate::trust::{LocalAlias, TrustError, VerifiedPeer};

/// The QUIC application protocol name.
pub const ALPN: &[u8] = b"hyakume/1";

/// A connection that stops answering is closed after this long. Iroh's
/// keepalives keep a healthy idle connection inside it.
const QUIC_IDLE_TIMEOUT: Duration = Duration::from_secs(10);
/// Prompts waiting for an answer at once. A question that finds the queue
/// full is answered no at once: an offer is refused as busy.
pub const PROMPT_QUEUE: usize = 8;
/// Frames read ahead of the connection task.
const FRAME_QUEUE: usize = 1;
/// Graceful shutdown waits this long for owned tasks.
const DRAIN_DEADLINE: Duration = Duration::from_secs(20);
/// Close code for an orderly end.
const CLOSE_DONE: u32 = 0;
/// Reset and close code after a write that could not finish.
const CLOSE_STOPPED: u32 = 1;
/// QUIC flow-control windows: one full frame plus a quarter more.
const QUIC_WINDOW_BYTES: u32 =
    (crate::limits::MAX_FRAME_BYTES + crate::limits::MAX_FRAME_BYTES / 4) as u32;

#[derive(Clone, Debug)]
pub struct NodeConfig {
    /// The name this device declares to peers. Presentation only.
    pub name: String,
    pub limits: Limits,
    pub bind: SocketAddr,
    /// When set, reading each incoming frame waits for one permit. Tests
    /// use it to make a device that stops reading.
    pub read_gate: Option<Arc<Semaphore>>,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            name: "hyakume".into(),
            limits: Limits::COMPILED,
            bind: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)),
            read_gate: None,
        }
    }
}

/// A question for the person using this device.
pub enum Prompt {
    /// Compare these words with the other device's screen.
    Verify {
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
        answer: oneshot::Sender<bool>,
    },
}

/// Content to send.
#[derive(Clone, Debug)]
pub enum Payload {
    Text(String),
    Url(String),
    Files(Vec<(String, Vec<u8>)>),
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
    #[error(transparent)]
    State(#[from] StateError),
    #[error(transparent)]
    Send(#[from] SendError),
    #[error("the device that answered is not the one expected")]
    WrongPeer,
    #[error("the invitation could not be used")]
    Invitation,
    #[error("the endpoint did not stop within its drain deadline")]
    DrainFailed,
    #[error("the transfer was stopped on this device")]
    Stopped,
}

struct Shared {
    key: DeviceKey,
    name: PeerText,
    state: Mutex<State>,
    budget: Budget,
    clock: MonotonicClock,
    changes: watch::Sender<u64>,
    prompts: mpsc::Sender<Prompt>,
    read_gate: Option<Arc<Semaphore>>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        // A panic while holding the lock stops the process (see tasks), so a
        // poisoned lock is never observed by surviving code.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Runs `f` on the state and wakes everything waiting for changes.
    fn change<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        let result = f(&mut self.lock());
        self.changes.send_modify(|v| *v = v.wrapping_add(1));
        result
    }

    /// Handles a wake-up. Others are notified only when the handler did
    /// something; otherwise a task woken by its own notification would wake
    /// itself again and spin.
    fn wake<T>(&self, f: impl FnOnce(&mut State) -> Vec<T>) -> Vec<T> {
        let outputs = f(&mut self.lock());
        if !outputs.is_empty() {
            self.changes.send_modify(|v| *v = v.wrapping_add(1));
        }
        outputs
    }

    fn limits(&self) -> Limits {
        self.budget.limits()
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
    async fn until(&self, mut condition: impl FnMut(&State) -> bool) {
        let mut changes = self.changes.subscribe();
        loop {
            if condition(&self.lock()) {
                return;
            }
            if changes.changed().await.is_err() {
                return std::future::pending().await;
            }
        }
    }
}

/// One endpoint instance. Dropping it aborts its tasks; call
/// [`Node::shutdown`] to stop gracefully.
pub struct Node {
    shared: Arc<Shared>,
    endpoint: Endpoint,
    accept: Option<JoinHandle<()>>,
}

impl Node {
    /// Binds a new endpoint with a fresh transport key and starts accepting.
    pub async fn bind(
        config: NodeConfig,
        key: DeviceKey,
    ) -> Result<(Self, mpsc::Receiver<Prompt>), NodeError> {
        let name = PeerText::new(config.name.into_bytes(), MAX_PEER_NAME_BYTES)
            .map_err(|e| NodeError::Bind(e.to_string()))?;
        let budget = Budget::new(config.limits);
        let endpoint = bind_endpoint(config.bind).await?;
        let (prompts, prompt_rx) = mpsc::channel(PROMPT_QUEUE);
        let shared = Arc::new(Shared {
            key,
            name,
            state: Mutex::new(State::new()),
            budget,
            clock: MonotonicClock::new(),
            changes: watch::Sender::new(0),
            prompts,
            read_gate: config.read_gate,
        });
        let accept = tokio::spawn(accept_loop(shared.clone(), endpoint.clone()));
        Ok((
            Self {
                shared,
                endpoint,
                accept: Some(accept),
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

    /// Creates a permanent invitation and returns its token text. The device
    /// that uses it will be called `alias` here.
    pub fn invite(&self, alias: &str) -> Result<Zeroizing<String>, NodeError> {
        let alias = LocalAlias::new(alias)?;
        let id = InvitationId::random(&mut OsRng);
        let secret = InvitationSecret::random(&mut OsRng);
        let token = InvitationToken {
            kind: InvitationKind::Permanent,
            id,
            secret: InvitationSecret::from_bytes(*secret.expose()),
            route: Route {
                endpoint: *self.endpoint.id().as_bytes(),
                addresses: self.endpoint.bound_sockets().into_iter().take(4).collect(),
            },
        };
        let text = token.to_text()?;
        let now = self.shared.clock.now();
        self.shared.change(|s| {
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
                },
            )
            .map_err(NodeError::from)
        })?;
        Ok(text)
    }

    /// Uses another device's invitation. The other device will be called
    /// `alias` here. Returns the paired device once both people confirmed.
    pub async fn join(&self, token: &str, alias: &str) -> Result<DeviceId, NodeError> {
        let token = InvitationToken::parse(token)?;
        if token.kind != InvitationKind::Permanent {
            return Err(NodeError::Invitation);
        }
        let alias = LocalAlias::new(alias)?;
        self.shared.lock().trust.check_alias(&alias, None)?;
        let peer =
            iroh::PublicKey::from_bytes(&token.route.endpoint).map_err(|_| TokenError::Endpoint)?;
        let mut addr = EndpointAddr::new(peer);
        for a in &token.route.addresses {
            addr = addr.with_ip_addr(*a);
        }
        let (conn, mut wire, negotiation) = self.dial(addr, Purpose::Pair).await?;
        let me = *self.endpoint.id().as_bytes();
        let result = join_exchange(
            &self.shared,
            &conn,
            &mut wire,
            &token,
            me,
            negotiation,
            alias,
        )
        .await;
        if let Err(e) = &result {
            wire.send_error(e).await;
        }
        wire.finish_dialer(&conn, self.shared.limits().handshake_deadline)
            .await;
        result
    }

    /// Sends `payload` to the trusted device `to`, reachable at `at`.
    pub async fn send(
        &self,
        to: DeviceId,
        at: EndpointAddr,
        payload: Payload,
        options: SendOptions,
    ) -> Result<SendReport, NodeError> {
        let authority = self.shared.lock().trust.authorize(&to)?;
        let transfer = TransferId::random(&mut OsRng);
        let (offer, bytes) = build_offer(transfer, payload)?;
        let session = self.shared.budget.admit_session()?;
        // If this future is dropped, or fails, before the transfer ends, the
        // guard ends it so it never stays unfinished in the state.
        let _unfinished = SendGuard {
            shared: self.shared.clone(),
            transfer,
        };
        let (conn, mut wire, negotiation) = self.dial(at, Purpose::Session).await?;
        let result = send_session(
            &self.shared,
            &conn,
            &mut wire,
            *self.endpoint.id().as_bytes(),
            negotiation,
            to,
            authority,
            session,
            offer,
            bytes,
            options,
        )
        .await;
        if let Err(e) = &result {
            wire.send_error(e).await;
            let cause = match e {
                NodeError::Refused(_) => FailCause::Busy,
                _ => FailCause::Connection,
            };
            self.shared.change(|s| {
                if let Some(t) = s.transfer_mut(&transfer) {
                    t.fail_rest(cause);
                    s.mark_finished(transfer);
                }
            });
        }
        wire.finish_dialer(&conn, self.shared.limits().idle_timeout)
            .await;
        let report = self.report(transfer);
        match (result, report) {
            (_, Some(report)) => Ok(report),
            (Err(e), None) => Err(e),
            (Ok(()), None) => Err(NodeError::Unexpected),
        }
    }

    /// Asks the connection carrying `transfer` to cancel it.
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

    /// Removes and returns the received items, releasing their room.
    pub fn take_received(&self) -> Vec<Received> {
        self.shared.change(|s| s.take_received())
    }

    /// Items received and published so far.
    pub fn received(&self) -> Vec<Received> {
        self.shared.lock().received().to_vec()
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

    /// Bytes reserved for accepted content or held by received items.
    pub fn held_bytes(&self) -> u64 {
        self.shared.lock().held_bytes()
    }

    /// Waits until `condition` holds for the state, up to `deadline`.
    pub async fn wait_until(
        &self,
        deadline: Duration,
        mut condition: impl FnMut(&State) -> bool,
    ) -> Result<(), NodeError> {
        let mut changes = self.shared.changes.subscribe();
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
        let mut changes = self.shared.changes.subscribe();
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

    /// Stops accepting, closes connections, and waits for owned tasks.
    pub async fn shutdown(mut self) -> Result<(), NodeError> {
        let Some(mut accept) = self.accept.take() else {
            return Ok(());
        };
        let endpoint = self.endpoint.clone();
        let drained = timeout(DRAIN_DEADLINE, async {
            endpoint.close().await;
            tasks::observe((&mut accept).await);
        })
        .await;
        if drained.is_err() {
            // The accept task owns every connection task, so aborting it
            // stops them all. Its end is still observed.
            accept.abort();
            tasks::observe(accept.await);
            return Err(NodeError::DrainFailed);
        }
        Ok(())
    }

    async fn dial(
        &self,
        addr: EndpointAddr,
        purpose: Purpose,
    ) -> Result<(Connection, Wire, [u8; 32]), NodeError> {
        let deadline = self.shared.limits().handshake_deadline;
        let slot = match purpose {
            Purpose::Pair => self.shared.budget.open_connection()?,
            Purpose::Session => self.shared.budget.open_session_connection()?,
        };
        let until = Instant::now() + deadline;
        let conn = timeout_at(until, self.endpoint.connect(addr, ALPN))
            .await
            .map_err(|_| NodeError::Timeout)?
            .map_err(|e| NodeError::Connect(e.to_string()))?;
        let (send, recv) = timeout_at(until, conn.open_bi())
            .await
            .map_err(|_| NodeError::Timeout)?
            .map_err(|_| NodeError::ConnectionLost)?;
        let mut wire = Wire::new(&conn, send, recv, &self.shared, slot, until);
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
}

/// Ends a sent transfer that is still unfinished when its `send` call
/// returns or is dropped.
struct SendGuard {
    shared: Arc<Shared>,
    transfer: TransferId,
}

impl Drop for SendGuard {
    fn drop(&mut self) {
        let transfer = self.transfer;
        self.shared.change(|s| {
            if let Some(t) = s.transfer_mut(&transfer)
                && !t.is_terminal()
            {
                t.cancel(CancelCause::Sender);
                s.mark_finished(transfer);
            }
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

impl Drop for Node {
    fn drop(&mut self) {
        if let Some(handle) = self.accept.take() {
            handle.abort();
        }
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

fn build_offer(transfer: TransferId, payload: Payload) -> Result<(Offer, Vec<Vec<u8>>), NodeError> {
    Ok(match payload {
        Payload::Text(text) => {
            let offer = Offer::for_text(transfer, PayloadKind::Text, text.as_bytes())?;
            (offer, vec![text.into_bytes()])
        }
        Payload::Url(url) => {
            let offer = Offer::for_text(transfer, PayloadKind::Url, url.as_bytes())?;
            (offer, vec![url.into_bytes()])
        }
        Payload::Files(files) => {
            let refs: Vec<(&str, &[u8])> = files
                .iter()
                .map(|(n, b)| (n.as_str(), b.as_slice()))
                .collect();
            let offer = Offer::for_files(transfer, &refs)?;
            (offer, files.into_iter().map(|(_, b)| b).collect())
        }
    })
}

fn exporter(conn: &Connection, label: &[u8]) -> Result<[u8; 32], NodeError> {
    let mut out = [0u8; 32];
    conn.export_keying_material(&mut out, label, b"")
        .map_err(|_| NodeError::ConnectionLost)?;
    Ok(out)
}

// ---- Accepting ----

async fn accept_loop(shared: Arc<Shared>, endpoint: Endpoint) {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            incoming = endpoint.accept() => match incoming {
                // A connection that has not proved who it is costs a task
                // and QUIC windows until its handshake deadline, so their
                // number is bounded too.
                Some(incoming) => match shared.budget.open_connection() {
                    Ok(slot) => {
                        let (shared, endpoint) = (shared.clone(), endpoint.clone());
                        connections.spawn(serve(shared, endpoint, incoming, slot));
                    }
                    Err(_) => incoming.refuse(),
                },
                None => break,
            },
            Some(done) = connections.join_next(), if !connections.is_empty() => {
                tasks::observe(done);
            }
        }
    }
    while let Some(done) = connections.join_next().await {
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
        shared.changes.send_modify(|v| *v = v.wrapping_add(1));
        return;
    };
    let mut wire = Wire::new(&conn, send, recv, &shared, slot, until);
    let result = serve_connection(&shared, &endpoint, &conn, &mut wire).await;
    // The session's capacity was released when its state machine dropped.
    shared.changes.send_modify(|v| *v = v.wrapping_add(1));
    if let Err(e) = &result {
        wire.send_error(e).await;
    }
    // Finishing consumes the wire, which releases the connection's slot.
    wire.finish_listener(&conn, deadline).await;
    shared.changes.send_modify(|v| *v = v.wrapping_add(1));
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
    let exporter = match exporter(conn, pairing::EXPORTER_LABEL) {
        Ok(e) => e,
        Err(e) => {
            settle_claim(shared, invitation, claim, ClaimEnd::Unverified);
            return Err(e);
        }
    };
    let cx = ExchangeContext {
        binding: ChannelBinding {
            exporter,
            inviter_endpoint: *endpoint.id().as_bytes(),
            joiner_endpoint: *conn.remote_id().as_bytes(),
            negotiation,
        },
        kind,
        invitation,
        claim,
    };
    let verified = match inviter_steps(shared, wire, &secret, cx).await {
        Ok(v) => v,
        Err(e) => {
            settle_claim(shared, invitation, claim, ClaimEnd::Unverified);
            return Err(e);
        }
    };
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
    let person = ask_verifier(shared, conn, &verified).await;
    // Answer yes only if this side can still use the invitation and save
    // the trust; otherwise the other side would trust a device that does
    // not trust it back.
    let answer = person
        && shared.change(|s| {
            let now = shared.clock.now();
            let usable = s.invitations.get_mut(&invitation).is_some_and(|p| {
                p.invitation.state(now) == InvitationState::Open
                    && matches!(
                        p.invitation.claim_state(claim),
                        Some(ClaimState::Verified(_))
                    )
            });
            usable && s.trust.can_trust(verified.peer(), &alias).is_ok()
        });
    wire.write(&Message::PairDecision(verified.decide(answer)))
        .await?;
    let theirs = match wire.message(shared.limits().idle_timeout).await {
        Ok(Message::PairDecision(d)) => d,
        Ok(_) => {
            settle_claim(shared, invitation, claim, ClaimEnd::Declined);
            return Err(NodeError::Unexpected);
        }
        Err(e) => {
            settle_claim(shared, invitation, claim, ClaimEnd::Declined);
            return Err(e);
        }
    };
    let peer = match verified.conclude_permanent(answer, &theirs) {
        Ok(peer) => peer,
        Err(e) => {
            settle_claim(shared, invitation, claim, ClaimEnd::Declined);
            return Err(e.into());
        }
    };
    let now = shared.clock.now();
    shared.change(|s| save_pairing(s, now, invitation, claim, peer, alias))
}

/// Uses the invitation and trusts the peer. Whether trust can be saved is
/// checked before the invitation is used, so a failure changes nothing:
/// the invitation is not used up by a pairing that saved no trust.
fn save_pairing(
    s: &mut State,
    now: Moment,
    invitation: InvitationId,
    claim: ClaimId,
    peer: VerifiedPeer,
    alias: LocalAlias,
) -> Result<(), NodeError> {
    s.trust.can_trust(peer.device(), &alias)?;
    let pending = s
        .invitations
        .get_mut(&invitation)
        .ok_or(NodeError::Invitation)?;
    pending
        .invitation
        .confirm_permanent(now, claim)
        .map_err(|_| NodeError::Invitation)?;
    s.trust.trust(peer, alias)?;
    Ok(())
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

#[derive(Clone, Copy)]
enum ClaimEnd {
    Unverified,
    Declined,
}

fn settle_claim(shared: &Shared, invitation: InvitationId, claim: ClaimId, end: ClaimEnd) {
    let now = shared.clock.now();
    shared.change(|s| {
        if let Some(p) = s.invitations.get_mut(&invitation) {
            let _ = match end {
                ClaimEnd::Unverified => p.invitation.claim_unverified(now, claim),
                ClaimEnd::Declined => p.invitation.decline(now, claim),
            };
        }
    });
}

/// Asks the person to compare the verifier. No answer, a full prompt
/// queue, or a closed connection count as no.
async fn ask_verifier(shared: &Shared, conn: &Connection, verified: &pairing::Verified) -> bool {
    let Some(reply) = shared.ask(|answer| Prompt::Verify {
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
    let person = ask_verifier(shared, conn, &verified).await;
    // As on the inviting side: never agree to trust that cannot be saved.
    let answer = person
        && shared
            .lock()
            .trust
            .can_trust(verified.peer(), &alias)
            .is_ok();
    wire.write(&Message::PairDecision(verified.decide(answer)))
        .await?;
    let theirs = match wire.message(shared.limits().idle_timeout).await? {
        Message::PairDecision(d) => d,
        _ => return Err(NodeError::Unexpected),
    };
    let peer = verified.conclude_permanent(answer, &theirs)?;
    let device = peer.device();
    shared.change(|s| s.trust.trust(peer, alias))?;
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
    authority: crate::trust::Authority,
    session: crate::limits::SessionSlot,
    offer: Offer,
    payload: Vec<Vec<u8>>,
    options: SendOptions,
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
        Message::Error(code) => return Err(NodeError::Peer(code)),
        _ => return Err(NodeError::Unexpected),
    };
    if session::verify(&proof, &binding, Side::Listener)? != to {
        return Err(NodeError::WrongPeer);
    }
    wire.end_handshake();
    let mut sender = shared.change(|s| {
        s.trust.check(&authority)?;
        Sender::new(
            offer,
            payload,
            options.chunk_bytes,
            authority,
            shared.budget.clone(),
            session,
            s,
        )
        .map_err(NodeError::from)
    })?;
    wire.write(&sender.offer_message()).await?;
    let mut changes = shared.changes.subscribe();
    let idle = shared.limits().idle_timeout;
    while !sender.is_done() {
        let mut _lease = None;
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
                frame = wire.next_by(until) => Err(incoming(shared, &mut sender, frame)),
            };
            match waited {
                Err(outputs) => outputs,
                Ok(lease) => {
                    _lease = Some(lease.map_err(|_| Refusal::Buffer)??);
                    // Wait for permission to send (tests only), while still
                    // noticing replies and local changes.
                    if let Some(gate) = &options.gate {
                        tokio::select! {
                            permit = gate.acquire() => {
                                if let Ok(p) = permit { p.forget(); }
                                shared.change(|s| sender.next_frame(s)).into_iter().collect()
                            }
                            frame = wire.next_by(wire.idle_until(idle)) => incoming(shared, &mut sender, frame),
                            _ = changes.changed() => shared.wake(|s| sender.check_stop(s)),
                        }
                    } else {
                        match wire.try_next() {
                            Some(frame) => incoming(shared, &mut sender, frame),
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
                frame = wire.next_by(wire.idle_until(idle)) => incoming(shared, &mut sender, frame),
                _ = changes.changed() => shared.wake(|s| sender.on_wake(s)),
            }
        };
        for out in outputs {
            match out {
                send::Output::Send(msg) => {
                    let stop = shared.until(|s| sender.should_stop(s));
                    match wire.write_or_stop(&msg, stop).await {
                        Ok(()) => {}
                        Err(NodeError::Stopped) => {
                            // The stream is reset, so the peer learns of the
                            // stop from the closed connection.
                            shared.change(|s| sender.check_stop(s));
                            return Ok(());
                        }
                        Err(NodeError::ConnectionLost) => {
                            // The receiver stopped reading; its reason may be
                            // queued.
                            let frame = wire.next(deadline).await;
                            incoming(shared, &mut sender, frame);
                            if !sender.is_done() {
                                shared.change(|s| sender.on_disconnect(s));
                            }
                        }
                        Err(e) => return Err(e),
                    }
                }
                send::Output::Abort(code) => {
                    wire.write(&Message::Error(code)).await.ok();
                    return Ok(());
                }
                send::Output::End => return Ok(()),
            }
        }
    }
    Ok(())
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
    let mut changes = shared.changes.subscribe();
    let mut answer: Option<(TransferId, oneshot::Receiver<bool>)> = None;
    let idle = shared.limits().idle_timeout;
    loop {
        let outputs = tokio::select! {
            frame = wire.next_by(wire.idle_until(idle)) => match frame.and_then(|f| protocol::decode_payload(&f.payload).map_err(NodeError::from)) {
                Ok(msg) => shared.change(|s| receiver.on_message(msg, s)),
                // The guard ends unfinished work on every way out.
                Err(NodeError::Ended) if receiver.awaiting_answer().is_none() => return Ok(()),
                Err(e) => return Err(e),
            },
            reply = async { match answer.as_mut() { Some((_, rx)) => rx.await, None => std::future::pending().await } } => {
                match answer.take() {
                    Some((transfer, _)) => shared.change(|s| receiver.on_answer(transfer, reply.unwrap_or(false), s)),
                    None => Vec::new(),
                }
            }
            _ = changes.changed() => shared.wake(|s| receiver.on_wake(s)),
        };
        for out in outputs {
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
                    let asked = shared.ask(|answer| Prompt::Offer {
                        transfer,
                        from,
                        view,
                        answer,
                    });
                    match asked {
                        Some(rx) => answer = Some((transfer, rx)),
                        None => {
                            let outs = shared.change(|s| receiver.on_unasked(transfer, s));
                            for o in outs {
                                if let receive::Output::Send(m) = o {
                                    wire.write(&m).await?;
                                }
                            }
                        }
                    }
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

    fn shared() -> Shared {
        let (prompts, _) = mpsc::channel(1);
        Shared {
            key: DeviceKey::from_seed(&Zeroizing::new([1; 32])),
            name: PeerText::default(),
            state: Mutex::new(State::new()),
            budget: Budget::new(Limits::COMPILED),
            clock: MonotonicClock::new(),
            changes: watch::Sender::new(0),
            prompts,
            read_gate: None,
        }
    }

    #[test]
    fn a_pairing_that_cannot_be_saved_leaves_the_invitation_usable() {
        let mut s = State::new();
        let now = Moment::from_millis(0);
        let (invitation, claim) = (
            InvitationId::from_bytes([1; 16]),
            ClaimId::from_bytes([2; 16]),
        );
        let mut open = Invitation::new(
            invitation,
            InvitationKind::Permanent,
            InvitationSecret::from_bytes([3; 32]),
            now,
            INVITATION_LIFETIME,
        );
        open.open_claim(now, claim).unwrap();
        open.claim_verified(now, claim, Verifier::from_bytes([4; 4]))
            .unwrap();
        let alias = LocalAlias::new("phone").unwrap();
        s.add_invitation(
            now,
            PendingInvitation {
                invitation: open,
                alias: alias.clone(),
            },
        )
        .unwrap();
        // Another device took the alias after this side answered yes.
        let device = |seed| DeviceKey::from_seed(&Zeroizing::new([seed; 32])).id();
        s.trust
            .trust(
                VerifiedPeer::new(device(5), PeerText::default()),
                alias.clone(),
            )
            .unwrap();

        let peer = VerifiedPeer::new(device(6), PeerText::default());
        let saved = save_pairing(&mut s, now, invitation, claim, peer, alias);

        assert_eq!(saved, Err(NodeError::Trust(TrustError::AliasInUse)));
        let pending = s.invitations.get_mut(&invitation).unwrap();
        assert_eq!(pending.invitation.state(now), InvitationState::Open);
        assert!(matches!(
            pending.invitation.claim_state(claim),
            Some(ClaimState::Verified(_))
        ));
        assert!(s.trust.authorize(&device(6)).is_err());
    }

    #[test]
    fn a_wake_that_changes_nothing_notifies_no_one() {
        let shared = shared();
        let mut changes = shared.changes.subscribe();
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
