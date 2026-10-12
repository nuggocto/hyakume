//! Compiled limits and the admission budget.
//!
//! Every attacker-influenced count and size has a compiled maximum here.
//! Configuration may lower a limit for one endpoint ([`Limits::clamped`]),
//! but nothing can raise one. The buffer budget also has a floor,
//! [`MIN_BUFFER_BYTES`].
//!
//! Capacity is handed out as permits. A permit returns its capacity when it
//! is dropped, so capacity cannot be released twice, and it is released
//! exactly when its owner gives up the work it stood for.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::sync::Notify;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
const GIB: u64 = 1024 * MIB;

/// Devices one installation may trust at once.
pub const MAX_TRUSTED_DEVICES: usize = 256;
/// Trust records kept in total, including revoked devices.
pub const MAX_TRUST_RECORDS: usize = 512;
/// Invitations open at once on one device.
pub const MAX_OPEN_INVITATIONS: usize = 8;
/// Exchanges one temporary or permanent invitation accepts.
pub const MAX_CLAIMANTS: usize = 8;
/// Failed exchanges one invitation tolerates before it closes.
pub const MAX_ATTEMPTS: u8 = 5;
/// How long a QR or native invitation stays valid.
pub const INVITATION_LIFETIME: Duration = Duration::from_secs(15 * 60);
/// How long a human code stays valid.
pub const CODE_LIFETIME: Duration = Duration::from_secs(120);

/// Items one batch may contain.
pub const MAX_BATCH_ITEMS: usize = 256;
pub const MAX_FILE_BYTES: u64 = 16 * GIB;
pub const MAX_BATCH_BYTES: u64 = 16 * GIB;
pub const MAX_TEXT_BYTES: u64 = MIB;
pub const MAX_URL_BYTES: u64 = 8 * KIB;
/// Encoded offer metadata, all items included.
pub const MAX_OFFER_METADATA_BYTES: usize = 64 * 1024;
/// Declared device names and offered item names.
pub const MAX_PEER_NAME_BYTES: usize = 1024;
/// Widest rendered name.
pub const MAX_NAME_COLUMNS: usize = 120;
/// Text preview shown before consent.
pub const MAX_PREVIEW_BYTES: usize = 4 * 1024;
/// Preview lines shown before consent.
pub const MAX_PREVIEW_LINES: usize = 16;
/// Items listed by name on a consent screen.
pub const MAX_LISTED_ITEMS: usize = 10;

/// Largest protocol frame, header included.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;
/// Room reserved in a frame for the chunk header.
pub const CHUNK_OVERHEAD: usize = 64;
/// Largest chunk of payload in one frame.
pub const MAX_CHUNK_BYTES: usize = MAX_FRAME_BYTES - CHUNK_OVERHEAD;
pub const MAX_IN_FLIGHT_CHUNKS: usize = 8;

/// Buffer that held and sending leases together always leave free: the
/// reader's lease for one largest frame, raw and decoded. With it free, a
/// connection can always read its next frame.
pub const FRAME_HEADROOM: u64 = 2 * MAX_FRAME_BYTES as u64;
/// Buffer that held leases also leave free for sending: the cost of one
/// largest chunk. With it free, an accepted transfer never stops this node
/// from sending, so two nodes sending to each other cannot wait on each
/// other's staging.
pub const CHUNK_HEADROOM: u64 = chunk_cost(MAX_CHUNK_BYTES);
/// The smallest buffer budget: one frame read and one chunk sent at once.
/// A budget configured below it is raised to it. Staging needs room on top.
pub const MIN_BUFFER_BYTES: u64 = FRAME_HEADROOM + CHUNK_HEADROOM;

/// Buffer one chunk of `len` payload bytes takes while it is sent: the
/// slice copy, the encoded payload, and the frame.
pub const fn chunk_cost(len: usize) -> u64 {
    3 * (len as u64 + CHUNK_OVERHEAD as u64)
}

/// Payload bytes the loopback proof keeps in receiver-owned staging. The
/// qualified file pipeline replaces this with private partial files.
pub const MAX_STAGED_BYTES: u64 = 64 * MIB;
/// Items accepted or received and not yet taken, empty ones included. Each
/// keeps its name and identifiers in memory even when it has no bytes.
pub const MAX_HELD_ITEMS: usize = 4 * MAX_BATCH_ITEMS;

const _: () = {
    // One resident copy of every in-flight chunk must fit the buffer budget.
    let chunks = (16 * MAX_IN_FLIGHT_CHUNKS * MAX_FRAME_BYTES) as u64;
    assert!(chunks <= 128 * MIB);
    assert!(MAX_TRUSTED_DEVICES <= MAX_TRUST_RECORDS);
    assert!(MAX_CLAIMANTS >= MAX_ATTEMPTS as usize);
    assert!(MAX_TEXT_BYTES as usize <= MAX_STAGED_BYTES as usize);
    assert!(MAX_HELD_ITEMS >= MAX_BATCH_ITEMS);
    assert!(CHUNK_OVERHEAD >= 1 + 16 + 2 + 8 + 4);
    assert!(CHUNK_HEADROOM == 3 * MAX_FRAME_BYTES as u64);
};

/// Limits that one endpoint may lower but never raise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub sessions: usize,
    pub active_transfers: usize,
    pub active_transfers_per_session: usize,
    pub nonterminal_transfers: usize,
    pub buffer_bytes: u64,
    pub handshake_deadline: Duration,
    pub idle_timeout: Duration,
}

impl Limits {
    /// The compiled maxima from the resource budget.
    pub const COMPILED: Self = Self {
        sessions: 32,
        active_transfers: 16,
        active_transfers_per_session: 4,
        nonterminal_transfers: 64,
        buffer_bytes: 128 * MIB,
        handshake_deadline: Duration::from_secs(30),
        idle_timeout: Duration::from_secs(10 * 60),
    };

    /// Each limit, capped at its compiled maximum. The buffer budget is
    /// raised to [`MIN_BUFFER_BYTES`] if it is lower.
    pub fn clamped(self) -> Self {
        let c = Self::COMPILED;
        Self {
            sessions: self.sessions.min(c.sessions),
            active_transfers: self.active_transfers.min(c.active_transfers),
            active_transfers_per_session: self
                .active_transfers_per_session
                .min(c.active_transfers_per_session),
            nonterminal_transfers: self.nonterminal_transfers.min(c.nonterminal_transfers),
            buffer_bytes: self.buffer_bytes.clamp(MIN_BUFFER_BYTES, c.buffer_bytes),
            handshake_deadline: self.handshake_deadline.min(c.handshake_deadline),
            idle_timeout: self.idle_timeout.min(c.idle_timeout),
        }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self::COMPILED
    }
}

/// Why admission was refused. Refusal changes nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("too many connections are open")]
    Sessions,
    #[error("too many connections are still proving who they are or pairing")]
    Connections,
    #[error("too many transfers are active")]
    ActiveTransfers,
    #[error("too many transfers are active on this connection")]
    SessionActiveTransfers,
    #[error("too many transfers are waiting or in progress")]
    Transfers,
    #[error("not enough buffer space")]
    Buffer,
}

/// Current use of each budget, for status output and tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Usage {
    pub connections: usize,
    pub sessions: usize,
    pub active_transfers: usize,
    pub nonterminal_transfers: usize,
    pub buffer_bytes: u64,
}

#[derive(Default)]
struct Counts {
    usage: Usage,
    /// Bytes of `usage.buffer_bytes` taken by held leases.
    held_bytes: u64,
    /// Bytes of `usage.buffer_bytes` taken by chunks being sent.
    sending_bytes: u64,
    /// Open connections that hold no session.
    unsessioned: usize,
    next_session: u64,
    /// Active transfers on each open session.
    per_session: BTreeMap<u64, usize>,
}

struct Inner {
    limits: Limits,
    counts: Mutex<Counts>,
    /// Woken whenever buffer bytes are released.
    released: Notify,
}

/// One endpoint's admission budget. Clones share the same budget.
#[derive(Clone)]
pub struct Budget(Arc<Inner>);

impl Budget {
    pub fn new(limits: Limits) -> Self {
        Self(Arc::new(Inner {
            limits: limits.clamped(),
            counts: Mutex::new(Counts::default()),
            released: Notify::new(),
        }))
    }

    pub fn limits(&self) -> Limits {
        self.0.limits
    }

    pub fn usage(&self) -> Usage {
        self.lock().usage
    }

    fn lock(&self) -> MutexGuard<'_, Counts> {
        // Counts stay consistent across a panic: every update is a single
        // assignment made after its checks.
        self.0.counts.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn admit_session(&self) -> Result<SessionSlot, Refusal> {
        let mut c = self.lock();
        if c.usage.sessions >= self.0.limits.sessions {
            return Err(Refusal::Sessions);
        }
        let key = c.next_session;
        c.next_session = c.next_session.wrapping_add(1);
        c.usage.sessions += 1;
        c.per_session.insert(key, 0);
        Ok(SessionSlot {
            budget: self.clone(),
            key,
        })
    }

    /// Admits an offered or queued transfer. The slot lasts until the
    /// transfer reaches a terminal state, including while it is paused.
    pub fn admit_transfer(&self) -> Result<TransferSlot, Refusal> {
        let mut c = self.lock();
        if c.usage.nonterminal_transfers >= self.0.limits.nonterminal_transfers {
            return Err(Refusal::Transfers);
        }
        c.usage.nonterminal_transfers += 1;
        Ok(TransferSlot {
            budget: self.clone(),
        })
    }

    /// Makes an admitted transfer active on `session`. Pausing or ending the
    /// transfer drops this slot and keeps the [`TransferSlot`].
    pub fn activate(
        &self,
        session: &SessionSlot,
        _transfer: &TransferSlot,
    ) -> Result<ActiveSlot, Refusal> {
        let mut c = self.lock();
        if c.usage.active_transfers >= self.0.limits.active_transfers {
            return Err(Refusal::ActiveTransfers);
        }
        let limit = self.0.limits.active_transfers_per_session;
        let here = c.per_session.get(&session.key).copied().unwrap_or(0);
        if here >= limit {
            return Err(Refusal::SessionActiveTransfers);
        }
        c.usage.active_transfers += 1;
        c.per_session.insert(session.key, here + 1);
        Ok(ActiveSlot {
            budget: self.clone(),
            session: session.key,
        })
    }

    /// Reserves `bytes` of buffer for a frame being read.
    pub fn lease(&self, bytes: u64) -> Result<BufferLease, Refusal> {
        self.try_lease(Kind::Read, bytes)
    }

    /// The most that held leases may take together.
    pub fn held_capacity(&self) -> u64 {
        self.ceiling(Kind::Held)
    }

    /// Reserves `bytes` that stay held until a transfer ends, such as its
    /// staged payload. Held leases always leave [`MIN_BUFFER_BYTES`] free,
    /// so they never stop this node from reading or sending.
    pub fn lease_held(&self, bytes: u64) -> Result<BufferLease, Refusal> {
        self.try_lease(Kind::Held, bytes)
    }

    /// Reserves `bytes` of buffer for a frame being read, waiting for other
    /// leases to be released when the budget is full. This is the
    /// backpressure point: a reader that waits here stops pulling from the
    /// network.
    pub async fn lease_wait(&self, bytes: u64) -> Result<BufferLease, Refusal> {
        self.wait(Kind::Read, bytes).await
    }

    /// Reserves `bytes` for a chunk being sent, waiting like
    /// [`Budget::lease_wait`]. Held and sending leases together leave
    /// [`FRAME_HEADROOM`] free, so sending never stops this node reading.
    pub async fn lease_send_wait(&self, bytes: u64) -> Result<BufferLease, Refusal> {
        self.wait(Kind::Send, bytes).await
    }

    /// The most that leases of `kind` and the kinds below it may take.
    fn ceiling(&self, kind: Kind) -> u64 {
        let all = self.0.limits.buffer_bytes;
        match kind {
            Kind::Read => all,
            Kind::Send => all.saturating_sub(FRAME_HEADROOM),
            Kind::Held => all.saturating_sub(MIN_BUFFER_BYTES),
        }
    }

    fn try_lease(&self, kind: Kind, bytes: u64) -> Result<BufferLease, Refusal> {
        let mut c = self.lock();
        let (mut held, mut sending) = (c.held_bytes, c.sending_bytes);
        match kind {
            Kind::Read => {}
            Kind::Send => sending = sending.checked_add(bytes).ok_or(Refusal::Buffer)?,
            Kind::Held => held = held.checked_add(bytes).ok_or(Refusal::Buffer)?,
        }
        let total = c
            .usage
            .buffer_bytes
            .checked_add(bytes)
            .ok_or(Refusal::Buffer)?;
        let fits = total <= self.ceiling(Kind::Read)
            && held.saturating_add(sending) <= self.ceiling(Kind::Send)
            && held <= self.ceiling(Kind::Held);
        if !fits {
            return Err(Refusal::Buffer);
        }
        c.usage.buffer_bytes = total;
        c.held_bytes = held;
        c.sending_bytes = sending;
        Ok(BufferLease {
            budget: self.clone(),
            bytes,
            kind,
        })
    }

    /// Waits until a lease fits. A request that could never fit is refused
    /// at once.
    async fn wait(&self, kind: Kind, bytes: u64) -> Result<BufferLease, Refusal> {
        if bytes > self.ceiling(kind) {
            return Err(Refusal::Buffer);
        }
        loop {
            let released = self.0.released.notified();
            tokio::pin!(released);
            // Register before trying, so a release in between is not missed.
            released.as_mut().enable();
            if let Ok(lease) = self.try_lease(kind, bytes) {
                return Ok(lease);
            }
            released.await;
        }
    }

    /// Opens a connection that holds no session yet: a pairing exchange, or
    /// an incoming connection that has not proved who it is. At most
    /// [`MAX_CLAIMANTS`] are open at once. With one connection per session,
    /// a node never holds more than `sessions + MAX_CLAIMANTS` connections.
    pub fn open_connection(&self) -> Result<ConnectionSlot, Refusal> {
        let mut c = self.lock();
        if c.unsessioned >= MAX_CLAIMANTS {
            return Err(Refusal::Connections);
        }
        c.unsessioned += 1;
        c.usage.connections += 1;
        Ok(ConnectionSlot {
            budget: self.clone(),
            session: false,
        })
    }

    /// Opens a connection for a session this node starts.
    pub fn open_session_connection(&self) -> Result<ConnectionSlot, Refusal> {
        let mut c = self.lock();
        self.session_connection_fits(&c)?;
        c.usage.connections += 1;
        Ok(ConnectionSlot {
            budget: self.clone(),
            session: true,
        })
    }

    fn session_connection_fits(&self, c: &Counts) -> Result<(), Refusal> {
        if c.usage.connections - c.unsessioned >= self.0.limits.sessions {
            return Err(Refusal::Sessions);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A frame being read. It may use the whole budget.
    Read,
    /// A chunk being sent.
    Send,
    /// Bytes held until a transfer ends.
    Held,
}

/// One open connection, counted until it is dropped.
pub struct ConnectionSlot {
    budget: Budget,
    session: bool,
}

impl ConnectionSlot {
    /// Counts this connection as a session's from now on, which frees its
    /// place among connections that hold no session.
    pub fn attach(&mut self) -> Result<(), Refusal> {
        if self.session {
            return Ok(());
        }
        let mut c = self.budget.lock();
        self.budget.session_connection_fits(&c)?;
        c.unsessioned -= 1;
        self.session = true;
        Ok(())
    }
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        let mut c = self.budget.lock();
        c.usage.connections -= 1;
        if !self.session {
            c.unsessioned -= 1;
        }
    }
}

/// One open session.
pub struct SessionSlot {
    budget: Budget,
    key: u64,
}

impl Drop for SessionSlot {
    fn drop(&mut self) {
        let mut c = self.budget.lock();
        c.usage.sessions -= 1;
        c.per_session.remove(&self.key);
    }
}

/// One transfer that is offered, queued, active, or paused.
pub struct TransferSlot {
    budget: Budget,
}

impl Drop for TransferSlot {
    fn drop(&mut self) {
        self.budget.lock().usage.nonterminal_transfers -= 1;
    }
}

/// One transfer currently moving bytes.
pub struct ActiveSlot {
    budget: Budget,
    session: u64,
}

impl Drop for ActiveSlot {
    fn drop(&mut self) {
        let mut c = self.budget.lock();
        c.usage.active_transfers -= 1;
        // The session may already be closed; its count went with it.
        if let Some(n) = c.per_session.get_mut(&self.session) {
            *n -= 1;
        }
    }
}

/// Buffer bytes reserved before allocation.
pub struct BufferLease {
    budget: Budget,
    bytes: u64,
    kind: Kind,
}

impl BufferLease {
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}

impl Drop for BufferLease {
    fn drop(&mut self) {
        let mut c = self.budget.lock();
        c.usage.buffer_bytes -= self.bytes;
        match self.kind {
            Kind::Read => {}
            Kind::Send => c.sending_bytes -= self.bytes,
            Kind::Held => c.held_bytes -= self.bytes,
        }
        drop(c);
        self.budget.0.released.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small() -> Budget {
        Budget::new(Limits {
            sessions: 2,
            active_transfers: 2,
            active_transfers_per_session: 1,
            nonterminal_transfers: 3,
            buffer_bytes: MIN_BUFFER_BYTES,
            ..Limits::COMPILED
        })
    }

    #[test]
    fn held_bytes_leave_room_to_send_one_chunk_and_read_one_frame_at_once() {
        let budget = Budget::new(Limits {
            buffer_bytes: MIN_BUFFER_BYTES + 100,
            ..Limits::COMPILED
        });
        assert_eq!(budget.lease_held(101).err(), Some(Refusal::Buffer));
        let staged = budget.lease_held(100).unwrap();
        assert_eq!(budget.lease_held(1).err(), Some(Refusal::Buffer));
        // The largest chunk still fits next to everything held, and the
        // largest frame next to both.
        let chunk = budget.try_lease(Kind::Send, CHUNK_HEADROOM).unwrap();
        assert_eq!(budget.try_lease(Kind::Send, 1).err(), Some(Refusal::Buffer));
        let frame = budget.lease(FRAME_HEADROOM).unwrap();
        drop((staged, chunk, frame));
        assert_eq!(budget.usage(), Usage::default());
    }

    #[test]
    fn connections_without_a_session_and_session_connections_have_separate_limits() {
        let b = small();
        let mut waiting: Vec<_> = (0..MAX_CLAIMANTS)
            .map(|_| b.open_connection().unwrap())
            .collect();
        assert_eq!(b.open_connection().err(), Some(Refusal::Connections));
        // Starting a session is not blocked by connections that hold none.
        let mine = b.open_session_connection().unwrap();
        let mut proved = waiting.pop().unwrap();
        proved.attach().unwrap();
        assert_eq!(b.open_session_connection().err(), Some(Refusal::Sessions));
        waiting.push(b.open_connection().unwrap());
        assert_eq!(b.usage().connections, b.limits().sessions + MAX_CLAIMANTS);
        drop((mine, proved, waiting));
        assert_eq!(b.usage(), Usage::default());
    }

    #[test]
    fn configuration_cannot_raise_a_compiled_limit() {
        let raised = Budget::new(Limits {
            sessions: 10_000,
            buffer_bytes: u64::MAX,
            ..Limits::COMPILED
        });
        assert_eq!(raised.limits(), Limits::COMPILED);
    }

    #[test]
    fn admission_stops_at_the_limit_and_resumes_after_release() {
        let b = small();
        let first = b.admit_session().unwrap();
        let second = b.admit_session().unwrap();
        assert_eq!(b.admit_session().err(), Some(Refusal::Sessions));
        drop(first);
        let third = b.admit_session().unwrap();
        drop((second, third));
        assert_eq!(b.usage(), Usage::default());
    }

    #[test]
    fn buffer_leases_refuse_the_byte_past_the_limit_and_overflow() {
        let b = small();
        let all = b.lease(b.limits().buffer_bytes).unwrap();
        assert_eq!(b.lease(1).err(), Some(Refusal::Buffer));
        drop(all);
        let _held = b.lease(1).unwrap();
        assert_eq!(b.lease(u64::MAX).err(), Some(Refusal::Buffer));
        assert_eq!(b.usage().buffer_bytes, 1);
    }

    #[tokio::test]
    async fn a_waiting_lease_proceeds_once_room_is_released() {
        let b = small();
        let max = b.limits().buffer_bytes;
        let held = b.lease(max - 20).unwrap();
        let waiter = tokio::spawn({
            let b = b.clone();
            async move { b.lease_wait(50).await.map(|l| l.bytes()) }
        });
        // The waiter cannot proceed while all but 20 bytes are held.
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        drop(held);
        assert_eq!(waiter.await.unwrap(), Ok(50));
        assert_eq!(b.lease_wait(max + 1).await.err(), Some(Refusal::Buffer));
    }

    #[test]
    fn pausing_releases_active_capacity_but_keeps_the_transfer_slot() {
        let b = small();
        let session = b.admit_session().unwrap();
        let transfer = b.admit_transfer().unwrap();
        let active = b.activate(&session, &transfer).unwrap();
        let other = b.admit_transfer().unwrap();
        assert_eq!(
            b.activate(&session, &other).err(),
            Some(Refusal::SessionActiveTransfers)
        );
        drop(active); // paused
        assert_eq!(b.usage().active_transfers, 0);
        assert_eq!(b.usage().nonterminal_transfers, 2);
        let resumed = b.activate(&session, &other).unwrap();
        drop((resumed, transfer, other, session));
        assert_eq!(b.usage(), Usage::default());
    }

    #[test]
    fn a_session_closing_before_its_transfers_finish_does_not_corrupt_counts() {
        let b = small();
        let session = b.admit_session().unwrap();
        let transfer = b.admit_transfer().unwrap();
        let active = b.activate(&session, &transfer).unwrap();
        drop(session);
        drop(active);
        drop(transfer);
        assert_eq!(b.usage(), Usage::default());
    }
}
