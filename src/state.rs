//! One endpoint's domain state under one lock, so revocation and publication
//! always happen in one order and the store records them in that order.
//! A change that claims success is written to the store first; a change that
//! ends work is applied in memory first. A failed write therefore never
//! becomes a success, and recovery settles what was left unfinished.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Mutex, MutexGuard, PoisonError};

use tokio::sync::watch;

use crate::clock::{LifetimeClock, Moment, wall_now};
use crate::identity::DeviceId;
use crate::ids::{InvitationId, TransferId};
use crate::invitation::{Invitation, InvitationState};
use crate::limits::{Budget, MAX_OPEN_INVITATIONS, RESUME_WINDOW, TransferSlot};
use crate::offer::Offer;
use crate::storage::{Loaded, NewFile, Store, StoreError, stored};
use crate::transfer::{CancelCause, FailCause, ItemState, PublishError, Transfer, TransitionError};
use crate::trust::{Authority, LocalAlias, Reservation, TrustError, TrustStore, VerifiedPeer};

/// Finished transfers kept for status before the oldest are dropped.
pub const MAX_KEPT_TRANSFERS: usize = 256;
/// Offered transfer IDs a receiver remembers to refuse a repeat, whatever
/// the status history has dropped.
pub const MAX_REMEMBERED_OFFERS: usize = 4096;

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("a transfer with that identifier already exists")]
    DuplicateTransfer,
    #[error("too many transfers are kept")]
    TooManyTransfers,
    #[error("too many invitations are open")]
    TooManyInvitations,
    #[error(transparent)]
    Trust(#[from] TrustError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("an earlier write to the state database failed; restart the service")]
    StoreFailed,
    #[error("saved trust records are inconsistent")]
    Restore,
}

/// An open invitation and the alias its device will get.
pub struct PendingInvitation {
    pub invitation: Invitation,
    pub alias: LocalAlias,
    /// How the exchange that compared verifier words ended: the device now
    /// trusted, or why there is none. Each invitation allows one such
    /// exchange, so this is its final result.
    pub outcome: Option<Result<DeviceId, String>>,
}

/// A receive destination: a directory named by its path and identified by
/// its device and inode, so a directory put in its place is noticed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Place {
    pub dir: Vec<u8>,
    pub dev: u64,
    pub ino: u64,
}

/// One item's private partial file, reserved when its offer is accepted.
#[derive(Clone, Debug)]
pub struct PartialFile {
    pub item: usize,
    /// The name the item gets when it is published.
    pub output: Vec<u8>,
    /// The partial file's name in the same folder.
    pub partial: Vec<u8>,
    /// Bytes charged for it.
    pub bytes: u64,
}

/// What is kept about a transfer besides its states.
#[derive(Clone, Debug)]
pub struct TransferInfo {
    pub outgoing: bool,
    pub offer: Offer,
    /// When the offer was made or received, on the clock of
    /// [`State::now_ms`].
    pub created_ms: u64,
    /// After this, paused work can no longer resume.
    pub expires_ms: u64,
    /// Receiving: where accepted items are written.
    pub place: Option<Place>,
    /// Receiving: each item's output name in `place`, set at acceptance.
    pub outputs: Vec<Option<Vec<u8>>>,
    /// Sending: each item's source file path, when it came from a file.
    pub sources: Vec<Option<Vec<u8>>>,
}

impl TransferInfo {
    /// Info for a transfer of `offer` starting now.
    pub fn new(outgoing: bool, offer: Offer, now_ms: u64) -> Self {
        let items = offer.items().len();
        Self {
            outgoing,
            offer,
            created_ms: now_ms,
            expires_ms: now_ms.saturating_add(RESUME_WINDOW.as_millis() as u64),
            place: None,
            outputs: vec![None; items],
            sources: vec![None; items],
        }
    }

    /// Whether the time to resume has passed at `now_ms`. A clock earlier
    /// than the start cannot tell how much time passed, so it counts as
    /// passed.
    pub fn expired(&self, now_ms: u64) -> bool {
        now_ms >= self.expires_ms || now_ms < self.created_ms
    }
}

struct Kept {
    transfer: Transfer,
    info: TransferInfo,
}

pub struct State {
    store: Store,
    pub(crate) trust: TrustStore,
    pub(crate) invitations: BTreeMap<InvitationId, PendingInvitation>,
    transfers: BTreeMap<TransferId, Kept>,
    /// Finished transfers in the order they finished.
    finished: VecDeque<TransferId>,
    next_finish: u64,
    /// The last offered transfer IDs, oldest first.
    offered: VecDeque<TransferId>,
    cancels: BTreeSet<TransferId>,
    /// Transfer slots held by paused work, which has no session to hold
    /// them. A slot goes when its transfer ends.
    paused_slots: BTreeMap<TransferId, TransferSlot>,
    /// The node is stopping: interrupted work pauses instead of failing.
    pausing: bool,
    /// A write to the store failed; no new work starts until a restart.
    store_failed: bool,
    clock: LifetimeClock,
    clock_saved_ms: u64,
    /// The wall clock reads earlier than at the last saved reading, so the
    /// time the service was stopped cannot be measured.
    downtime_unknown: bool,
}

impl State {
    /// An empty state with a store in memory.
    pub fn in_memory() -> Result<Self, StateError> {
        Self::load(Store::memory()?)
    }

    /// Loads the saved state, checking that trust and transfers are
    /// consistent. Startup recovery runs on the result.
    pub fn load(store: Store) -> Result<Self, StateError> {
        let Loaded {
            trust,
            next_epoch,
            clock,
            offered,
            transfers,
        } = store.load()?;
        let trust = TrustStore::restore(trust, next_epoch).map_err(|_| StateError::Restore)?;

        let resumed = LifetimeClock::resume(clock, wall_now(), std::time::Instant::now());
        let mut state = Self {
            store,
            trust,
            invitations: BTreeMap::new(),
            transfers: BTreeMap::new(),
            finished: VecDeque::new(),
            next_finish: 0,
            offered: offered.into(),
            cancels: BTreeSet::new(),
            paused_slots: BTreeMap::new(),
            pausing: false,
            store_failed: false,
            clock: resumed.0,
            clock_saved_ms: clock.lifetime_ms,
            downtime_unknown: !resumed.1,
        };

        let mut unsettled = Vec::new();
        for saved in transfers {
            let id = saved.transfer.id();
            match saved.finished {
                Some(order) => {
                    state.finished.push_back(id);
                    state.next_finish = state.next_finish.max(order.saturating_add(1));
                }
                None if saved.transfer.is_terminal() => unsettled.push(id),
                None => {}
            }
            state.transfers.insert(
                id,
                Kept {
                    transfer: saved.transfer,
                    info: saved.info,
                },
            );
        }

        // A crash between saving a transfer's end and its finish order
        // leaves it ended but never evictable; it is finished now.
        for id in unsettled {
            state.settle(id);
        }
        Ok(state)
    }

    pub fn trust(&self) -> &TrustStore {
        &self.trust
    }

    /// The durable store, for the installation and the file ledger.
    pub fn store(&mut self) -> &mut Store {
        &mut self.store
    }

    pub fn transfer(&self, id: &TransferId) -> Option<&Transfer> {
        self.transfers.get(id).map(|k| &k.transfer)
    }

    pub fn info(&self, id: &TransferId) -> Option<&TransferInfo> {
        self.transfers.get(id).map(|k| &k.info)
    }

    pub fn transfers(&self) -> impl Iterator<Item = &Transfer> {
        self.transfers.values().map(|k| &k.transfer)
    }

    /// Whether a durable write failed since the service started.
    pub fn store_failed(&self) -> bool {
        self.store_failed
    }

    /// Refuses new work after a failed durable write.
    pub(crate) fn check_store(&self) -> Result<(), StateError> {
        if self.store_failed {
            return Err(StateError::StoreFailed);
        }
        Ok(())
    }

    /// The time every saved deadline is stamped with and checked against.
    /// Both sides use this one clock, so a wall clock set back can neither
    /// lengthen nor shorten a lifetime.
    pub fn now_ms(&mut self) -> u64 {
        self.lifetime_now(wall_now(), std::time::Instant::now())
    }

    /// [`State::now_ms`] at a given wall and monotonic time; see
    /// [`LifetimeClock`]. It is saved about once a minute, so a restart
    /// cannot set it back further.
    pub fn lifetime_now(&mut self, wall_ms: u64, mono: std::time::Instant) -> u64 {
        const SAVE_EVERY_MS: u64 = 60_000;
        let now = self.clock.now(wall_ms, mono);
        if now.saturating_sub(self.clock_saved_ms) >= SAVE_EVERY_MS {
            let saved = self.store.save_clock(self.clock.reading());
            if saved.is_ok() {
                self.clock_saved_ms = now;
            }
            self.note(saved);
        }
        now
    }

    /// Records a file Hyakume now owns. A failed write stops new work, as
    /// any failed durable write does.
    pub fn add_file(&mut self, file: &NewFile<'_>) -> Result<i64, StoreError> {
        let added = self.store.add_file(file);
        if let Err(e) = &added {
            tracing::error!(error = %e, "a durable write failed");
            self.store_failed = true;
        }
        added
    }

    /// Records a file Hyakume owns and could not delete, so cleanup
    /// retries it and its bytes stay charged until it is gone.
    pub fn add_file_to_delete(&mut self, file: &NewFile<'_>) -> Result<(), StoreError> {
        let now = self.now_ms();
        let added = self.store.add_file_to_delete(file, now);
        if let Err(e) = &added {
            tracing::error!(error = %e, "a durable write failed");
            self.store_failed = true;
        }
        added
    }

    /// Whether the time since the last run cannot be measured. Startup
    /// recovery then expires paused work, as it does for work whose
    /// clock reads earlier than its start.
    pub fn downtime_unknown(&self) -> bool {
        self.downtime_unknown
    }

    fn note(&mut self, result: Result<(), StoreError>) {
        if let Err(e) = result {
            tracing::error!(error = %e, "a durable write failed");
            self.store_failed = true;
        }
    }

    // ---- Trust ----

    /// Saves trust in a peer whose pairing held `reservation`. The record
    /// and the epoch counter are written before memory changes.
    pub(crate) fn trust_peer(
        &mut self,
        peer: VerifiedPeer,
        reservation: Reservation,
    ) -> Result<Authority, StateError> {
        let held = reservation.key();
        if let Err(e) = self.check_store() {
            self.trust.release_key(held);
            return Err(e);
        }

        let device = peer.device();
        let mut next = self.trust.clone();
        let saved = next
            .trust_reserved(peer, reservation)
            .map_err(StateError::from)
            .and_then(|a| {
                let record = next.record(&device).ok_or(TrustError::NotTrusted)?;
                self.store.save_trust(record, next.next_epoch())?;
                Ok(a)
            });
        match saved {
            Ok(authority) => {
                self.trust = next;
                Ok(authority)
            }
            Err(e) => {
                self.trust.release_key(held);
                Err(e)
            }
        }
    }

    /// Revokes a device: trust first, durably, then every transfer with it.
    pub fn revoke(&mut self, device: &DeviceId) -> Result<(), StateError> {
        let mut next = self.trust.clone();
        next.revoke(device)?;
        if let Some(record) = next.record(device) {
            self.store.save_trust(record, next.next_epoch())?;
        }

        self.trust = next;
        let ids: Vec<TransferId> = self
            .transfers
            .values()
            .filter(|k| k.transfer.peer() == *device)
            .map(|k| k.transfer.id())
            .collect();
        for id in ids {
            self.update_transfer(&id, Transfer::revoke);
        }
        Ok(())
    }

    // ---- Transfers ----

    pub(crate) fn insert_transfer(
        &mut self,
        transfer: Transfer,
        info: TransferInfo,
    ) -> Result<(), StateError> {
        self.check_store()?;
        if self.transfers.contains_key(&transfer.id()) {
            return Err(StateError::DuplicateTransfer);
        }
        while self.transfers.len() >= MAX_KEPT_TRANSFERS {
            let Some(&oldest) = self.finished.front() else {
                return Err(StateError::TooManyTransfers);
            };
            // It stays first in line until it is really gone.
            self.store.delete_transfer(oldest)?;
            self.finished.pop_front();
            self.transfers.remove(&oldest);
        }
        self.store.insert_transfer(&transfer, &info)?;
        self.transfers
            .insert(transfer.id(), Kept { transfer, info });
        Ok(())
    }

    /// Applies `f` to a transfer, saves what changed, and records the
    /// transfer as finished once it is. For changes that end or interrupt
    /// work; see the module notes.
    pub(crate) fn update_transfer<R>(
        &mut self,
        id: &TransferId,
        f: impl FnOnce(&mut Transfer) -> R,
    ) -> Option<R> {
        let now = self.now_ms();
        let kept = self.transfers.get_mut(id)?;
        let before = kept.transfer.clone();
        let result = f(&mut kept.transfer);
        let durable = |t: &Transfer| {
            (
                t.offer_state(),
                t.items().iter().map(|s| stored(*s)).collect::<Vec<_>>(),
            )
        };
        if durable(&kept.transfer) != durable(&before) {
            let saved = self.store.save_transfer(&kept.transfer, now);
            self.note(saved);
        }
        self.settle(*id);
        Some(result)
    }

    /// Records a transfer that reached a terminal state as finished, so it
    /// may be dropped later to make room, drops its offer's preview, and
    /// lets its paused slot go.
    fn settle(&mut self, id: TransferId) {
        let Some(kept) = self.transfers.get_mut(&id) else {
            return;
        };
        if !kept.transfer.is_terminal() || self.finished.contains(&id) {
            return;
        }
        // History keeps no content; see `Offer::forget_preview`.
        kept.info.offer.forget_preview();
        let order = self.next_finish;
        self.next_finish = order.saturating_add(1);
        let saved = self.store.finish_transfer(id, order, &kept.info.offer);
        self.cancels.remove(&id);
        self.paused_slots.remove(&id);
        self.note(saved);
        self.finished.push_back(id);
    }

    /// Accepts an offered transfer if trust in its peer is unchanged. The
    /// acceptance is saved before memory changes. On the receiving side,
    /// `into` names the destination and reserves each item's partial file.
    pub(crate) fn accept(
        &mut self,
        id: &TransferId,
        into: Option<(&Place, &[PartialFile])>,
    ) -> Result<(), PublishError> {
        let kept = self
            .transfers
            .get(id)
            .ok_or(TransitionError::NoSuchItem { item: 0 })?;
        let mut next = kept.transfer.clone();
        if let Err(e) = next.accept(&self.trust) {
            // A revoked peer's offer is rejected; save that like any end.
            self.update_transfer(id, |t| *t = next);
            return Err(e);
        }

        let now = self.now_ms();
        let saved = match into {
            Some((place, partials)) => self.store.accept_into(&next, place, partials, now),
            None => self.store.save_transfer(&next, now),
        };
        if let Err(e) = saved {
            tracing::error!(error = %e, "could not save an acceptance");
            self.store_failed = true;
            return Err(PublishError::Store);
        }

        if let Some(kept) = self.transfers.get_mut(id) {
            kept.transfer = next;
            if let Some((place, partials)) = into {
                kept.info.place = Some(place.clone());
                for p in partials {
                    if let Some(slot) = kept.info.outputs.get_mut(p.item) {
                        *slot = Some(p.output.clone());
                    }
                }
            }
        }
        Ok(())
    }

    /// Checks, at the commit point, that a verified item may be published:
    /// nobody asked to cancel the transfer and trust in the peer is
    /// unchanged. If not, the transfer is cancelled or revoked here, in the
    /// same order as everything else, and the item is never published.
    pub(crate) fn may_publish(&mut self, id: &TransferId, item: usize) -> Result<(), PublishError> {
        let kept = self
            .transfers
            .get(id)
            .ok_or(TransitionError::NoSuchItem { item })?;
        if self.cancels.contains(id) {
            return Err(PublishError::Cancelled);
        }
        if let Err(e) = self.trust.check(&kept.transfer.authority()) {
            self.update_transfer(id, Transfer::revoke);
            return Err(PublishError::Unauthorized(e));
        }
        let state = *kept
            .transfer
            .items()
            .get(item)
            .ok_or(TransitionError::NoSuchItem { item })?;
        if state != ItemState::Verifying {
            return Err(TransitionError::NotAllowed {
                from: state,
                event: crate::transfer::ItemEvent::BytesComplete,
            }
            .into());
        }
        Ok(())
    }

    /// Records that a verified item was committed under its final name.
    /// Call only after [`State::may_publish`] passed and the filesystem
    /// commit succeeded, inside the same lock. The file exists whatever
    /// happens here, so memory always shows it published; a failed write
    /// leaves the record for startup recovery and is reported.
    pub(crate) fn record_published(
        &mut self,
        id: &TransferId,
        item: usize,
    ) -> Result<(), StoreError> {
        let now = self.now_ms();
        let Some(kept) = self.transfers.get_mut(id) else {
            return Ok(());
        };
        if kept.transfer.publish(item, &self.trust).is_err() {
            // `may_publish` checked both under this lock; nothing changed.
            crate::tasks::fatal("an item checked for publication could not be published");
        }
        let saved = self.store.publish(&kept.transfer, item, now);
        if let Err(e) = &saved {
            tracing::error!(error = %e, "could not record a publication");
            self.store_failed = true;
        }
        self.settle(*id);
        saved
    }

    /// Recovery found `item` committed under its final name, with the
    /// identity its partial file had, and its folder synced; the record of
    /// that is saved now.
    pub(crate) fn recovered_published(&mut self, id: &TransferId, item: usize) {
        let now = self.now_ms();
        let Some(kept) = self.transfers.get_mut(id) else {
            return;
        };
        if kept
            .transfer
            .found_on_disk(item, ItemState::Published)
            .is_ok()
        {
            let saved = self.store.publish(&kept.transfer, item, now);
            self.note(saved);
            self.settle(*id);
        }
    }

    /// Unfinished transfers, for startup recovery and status.
    pub fn unfinished(&self) -> Vec<TransferId> {
        self.transfers
            .values()
            .filter(|k| !k.transfer.is_terminal())
            .map(|k| k.transfer.id())
            .collect()
    }

    /// Forgets the installation's trust and transfers, in the store and in
    /// memory. Files Hyakume owns stay charged until they are deleted.
    pub fn forget_all(&mut self) -> Result<(), StateError> {
        let now = self.now_ms();
        self.store.forget_installation(now)?;
        self.trust = TrustStore::restore(Vec::new(), self.trust.next_epoch())
            .map_err(|_| StateError::Restore)?;
        self.invitations.clear();
        self.transfers.clear();
        self.finished.clear();
        self.offered.clear();
        self.cancels.clear();
        self.paused_slots.clear();
        Ok(())
    }

    /// Records an offered transfer ID. Returns false when the ID is not new:
    /// it names a transfer still kept, or one of the last
    /// [`MAX_REMEMBERED_OFFERS`] offers. Older IDs are forgotten. Only the
    /// authenticated peer can send an old offer again, and it gets a new
    /// consent question, the same as any new offer would.
    pub(crate) fn first_offer(&mut self, id: TransferId) -> bool {
        if self.transfers.contains_key(&id) || self.offered.contains(&id) {
            return false;
        }
        if self.offered.len() == MAX_REMEMBERED_OFFERS {
            self.offered.pop_front();
        }
        self.offered.push_back(id);
        let saved = self.store.push_offered(id);
        self.note(saved);
        true
    }

    /// Ends a transfer whose connection broke. While the node stops, the
    /// work pauses instead, so it can resume later.
    pub(crate) fn interrupt(&mut self, id: &TransferId, cause: FailCause) {
        let pausing = self.pausing;
        self.update_transfer(id, |t| {
            if pausing {
                t.pause_all();
            } else {
                t.fail_rest(cause);
            }
        });
    }

    /// A reserved file now exists with this device and inode. Recovery
    /// needs this record to recognize the file, so a caller must not go on
    /// using the file unless this returns true. False means the reservation
    /// is gone, as after a cancel.
    pub(crate) fn file_created(
        &mut self,
        kind: crate::storage::FileKind,
        transfer: TransferId,
        item: usize,
        identity: (u64, u64),
    ) -> Result<bool, StoreError> {
        let saved = self.store.file_created(kind, transfer, item, identity);
        if let Err(e) = &saved {
            tracing::error!(error = %e, "a durable write failed");
            self.store_failed = true;
        }
        saved
    }

    /// Whether the node is stopping, so broken connections pause their
    /// work instead of failing it.
    pub fn pausing(&self) -> bool {
        self.pausing
    }

    pub(crate) fn set_pausing(&mut self, pausing: bool) {
        self.pausing = pausing;
    }

    /// Asks the connection carrying `id` to cancel it. Paused work has no
    /// connection, so it is cancelled here. An unconfirmed item keeps the
    /// cancel until the disk shows whether it was published.
    pub fn request_cancel(&mut self, id: TransferId) -> bool {
        let Some(kept) = self.transfers.get(&id) else {
            return false;
        };
        if kept.transfer.is_terminal() {
            return false;
        }
        if is_paused(&kept.transfer) {
            let cause = if kept.info.outgoing {
                CancelCause::Sender
            } else {
                CancelCause::Receiver
            };
            self.update_transfer(&id, |t| t.cancel(cause));
            return true;
        }
        self.cancels.insert(id)
    }

    pub(crate) fn cancel_requested(&self, id: &TransferId) -> bool {
        self.cancels.contains(id)
    }

    /// Takes a transfer slot from `budget` for each paused transfer, so
    /// paused work counts against the nonterminal limit of a new node.
    pub(crate) fn hold_paused(&mut self, budget: &Budget) {
        self.paused_slots.clear();
        let paused: Vec<TransferId> = self
            .transfers
            .values()
            .filter(|k| is_paused(&k.transfer))
            .map(|k| k.transfer.id())
            .collect();
        for id in paused {
            match budget.admit_transfer() {
                Ok(slot) => {
                    self.paused_slots.insert(id, slot);
                }
                Err(_) => break,
            }
        }
    }

    /// Keeps `slot` for `transfer` while it stays paused, as
    /// [`State::hold_paused`] does for work paused before a node started.
    pub(crate) fn hold_paused_slot(&mut self, transfer: TransferId, slot: TransferSlot) {
        if self
            .transfers
            .get(&transfer)
            .is_some_and(|k| is_paused(&k.transfer))
        {
            self.paused_slots.insert(transfer, slot);
        }
    }

    /// Expires paused work whose time to resume has passed. Returns the
    /// transfers that expired. An unconfirmed item does not expire; it
    /// waits for the disk to settle it.
    pub fn expire(&mut self, now_ms: u64) -> Vec<TransferId> {
        let due: Vec<TransferId> = self
            .transfers
            .values()
            .filter(|k| {
                is_paused(&k.transfer)
                    && k.transfer.items().contains(&ItemState::Paused)
                    && k.info.expired(now_ms)
            })
            .map(|k| k.transfer.id())
            .collect();
        for id in &due {
            self.update_transfer(id, Transfer::expire_all);
        }
        due
    }

    /// Adds an invitation, dropping closed ones first. A used invitation
    /// whose exchange still waits for the other device's answer stays, so
    /// the exchange can record how it ended.
    pub(crate) fn add_invitation(
        &mut self,
        now: Moment,
        pending: PendingInvitation,
    ) -> Result<(), StateError> {
        self.invitations
            .retain(|_, p| match p.invitation.state(now) {
                InvitationState::Open => true,
                InvitationState::Consumed => p.outcome.is_none(),
                _ => false,
            });
        if self.invitations.len() >= MAX_OPEN_INVITATIONS {
            return Err(StateError::TooManyInvitations);
        }
        self.invitations.insert(pending.invitation.id(), pending);
        Ok(())
    }
}

/// Unfinished work with no connection: every unfinished item is paused or
/// waits for its folder to sync.
pub fn is_paused(t: &Transfer) -> bool {
    !t.is_terminal()
        && t.items()
            .iter()
            .all(|s| s.is_terminal() || *s == ItemState::Paused || s.is_unconfirmed())
}

/// The state behind one lock, with a signal for everything that waits on
/// it. The service owns it; each node it starts shares it.
pub struct StateCell {
    state: Mutex<State>,
    changes: watch::Sender<u64>,
}

impl std::fmt::Debug for StateCell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StateCell")
    }
}

impl StateCell {
    pub fn new(state: State) -> Self {
        Self {
            state: Mutex::new(state),
            changes: watch::Sender::new(0),
        }
    }

    pub fn lock(&self) -> MutexGuard<'_, State> {
        // A panic while holding the lock stops the process (see tasks), so a
        // poisoned lock is never observed by surviving code.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Runs `f` on the state and wakes everything waiting for changes.
    pub fn change<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        let result = f(&mut self.lock());
        self.notify();
        result
    }

    /// Handles a wake-up. Others are notified only when the handler did
    /// something; otherwise a task woken by its own notification would wake
    /// itself again and spin.
    pub fn wake<T>(&self, f: impl FnOnce(&mut State) -> Vec<T>) -> Vec<T> {
        let outputs = f(&mut self.lock());
        if !outputs.is_empty() {
            self.notify();
        }
        outputs
    }

    pub fn notify(&self) {
        self.changes.send_modify(|v| *v = v.wrapping_add(1));
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    /// Resolves once `condition` holds for the state.
    pub async fn until(&self, mut condition: impl FnMut(&State) -> bool) {
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
