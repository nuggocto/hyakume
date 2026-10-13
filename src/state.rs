//! One endpoint's domain state, owned by a single lock in the runtime.
//!
//! Everything that must share one order lives here: trust, invitations,
//! transfers, and the model publisher's received items. Revocation and
//! publication both happen through `&mut State`, so whichever reaches the
//! lock first decides the outcome.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::clock::Moment;
use crate::display::PeerText;
use crate::identity::DeviceId;
use crate::ids::{InvitationId, TransferId};
use crate::invitation::{Invitation, InvitationState};
use crate::limits::{MAX_HELD_ITEMS, MAX_OPEN_INVITATIONS, MAX_STAGED_BYTES};
use crate::offer::PayloadKind;
use crate::transfer::{PublishError, Transfer, TransitionError};
use crate::trust::{LocalAlias, TrustError, TrustStore};

/// Finished transfers kept for status before the oldest are dropped.
pub const MAX_KEPT_TRANSFERS: usize = 256;
/// Offered transfer IDs a receiver remembers to refuse a repeat, whatever
/// the status history has dropped.
pub const MAX_REMEMBERED_OFFERS: usize = 4096;

/// An item the model publisher committed. The qualified file pipeline
/// replaces this with a file under its final name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Received {
    pub transfer: TransferId,
    pub item: usize,
    pub from: DeviceId,
    pub kind: PayloadKind,
    pub name: PeerText,
    pub bytes: Vec<u8>,
}

/// Room for accepted or received items: their bytes and their number.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Room {
    pub bytes: u64,
    pub items: usize,
}

impl Room {
    pub fn new(bytes: u64, items: usize) -> Self {
        Self { bytes, items }
    }

    /// Room for one item of `bytes`.
    pub fn item(bytes: u64) -> Self {
        Self::new(bytes, 1)
    }

    pub fn saturating_sub(self, other: Self) -> Self {
        Self::new(
            self.bytes.saturating_sub(other.bytes),
            self.items.saturating_sub(other.items),
        )
    }

    fn checked_add(self, other: Self) -> Option<Self> {
        Some(Self::new(
            self.bytes.checked_add(other.bytes)?,
            self.items.checked_add(other.items)?,
        ))
    }

    /// Whether this much fits under the staging limits.
    pub fn fits(self) -> bool {
        self.bytes <= MAX_STAGED_BYTES && self.items <= MAX_HELD_ITEMS
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum StateError {
    #[error("a transfer with that identifier already exists")]
    DuplicateTransfer,
    #[error("too many transfers are kept")]
    TooManyTransfers,
    #[error("too many invitations are open")]
    TooManyInvitations,
    #[error("not enough room to receive that much")]
    NoRoom,
}

/// An open invitation and the alias its device will get.
pub struct PendingInvitation {
    pub invitation: Invitation,
    pub alias: LocalAlias,
}

#[derive(Default)]
pub struct State {
    pub(crate) trust: TrustStore,
    pub(crate) invitations: BTreeMap<InvitationId, PendingInvitation>,
    transfers: BTreeMap<TransferId, Transfer>,
    /// Finished transfers in the order they finished.
    finished: Vec<TransferId>,
    received: Vec<Received>,
    /// The last offered transfer IDs, oldest first.
    offered: VecDeque<TransferId>,
    /// Room reserved for accepted items plus room held by received items.
    held: Room,
    cancels: BTreeSet<TransferId>,
}

impl State {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn trust(&self) -> &TrustStore {
        &self.trust
    }

    pub fn transfer(&self, id: &TransferId) -> Option<&Transfer> {
        self.transfers.get(id)
    }

    pub fn transfers(&self) -> impl Iterator<Item = &Transfer> {
        self.transfers.values()
    }

    pub(crate) fn transfer_mut(&mut self, id: &TransferId) -> Option<&mut Transfer> {
        self.transfers.get_mut(id)
    }

    pub fn received(&self) -> &[Received] {
        &self.received
    }

    /// Room reserved for accepted items or held by received items.
    pub fn held(&self) -> Room {
        self.held
    }

    /// Bytes reserved for accepted items or held by received items.
    pub fn held_bytes(&self) -> u64 {
        self.held.bytes
    }

    /// Removes and returns the received items, releasing their room.
    pub fn take_received(&mut self) -> Vec<Received> {
        let taken = std::mem::take(&mut self.received);
        let bytes: u64 = taken.iter().map(|r| r.bytes.len() as u64).sum();
        self.held = self.held.saturating_sub(Room::new(bytes, taken.len()));
        taken
    }

    pub(crate) fn insert_transfer(&mut self, transfer: Transfer) -> Result<(), StateError> {
        if self.transfers.contains_key(&transfer.id()) {
            return Err(StateError::DuplicateTransfer);
        }
        while self.transfers.len() >= MAX_KEPT_TRANSFERS && !self.finished.is_empty() {
            let oldest = self.finished.remove(0);
            self.transfers.remove(&oldest);
        }
        if self.transfers.len() >= MAX_KEPT_TRANSFERS {
            return Err(StateError::TooManyTransfers);
        }
        self.transfers.insert(transfer.id(), transfer);
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
        true
    }

    /// Records that a transfer reached a terminal state, so it may be
    /// dropped later to make room.
    pub(crate) fn mark_finished(&mut self, id: TransferId) {
        if !self.finished.contains(&id) {
            self.finished.push(id);
        }
        self.cancels.remove(&id);
    }

    /// Whether `room` more would still fit, without reserving it.
    pub(crate) fn has_room(&self, room: Room) -> bool {
        self.held.checked_add(room).is_some_and(Room::fits)
    }

    /// Reserves room for accepted content.
    pub(crate) fn reserve(&mut self, room: Room) -> Result<(), StateError> {
        let total = self
            .held
            .checked_add(room)
            .filter(|t| t.fits())
            .ok_or(StateError::NoRoom)?;
        self.held = total;
        Ok(())
    }

    /// Returns reserved room that will not be used.
    pub(crate) fn release(&mut self, room: Room) {
        self.held = self.held.saturating_sub(room);
    }

    /// Accepts an offered transfer if trust in its peer is unchanged.
    pub(crate) fn accept(&mut self, id: &TransferId) -> Result<(), PublishError> {
        let t = self.transfers.get_mut(id).ok_or(PublishError::Transition(
            TransitionError::NoSuchItem { item: 0 },
        ))?;
        t.accept(&self.trust)
    }

    /// The commit point for one verified item: it becomes `Published` only
    /// if trust in the peer is unchanged and nobody asked to cancel the
    /// transfer. The item then goes to the model publisher, and its
    /// reservation becomes the room the received item holds.
    pub(crate) fn publish(&mut self, received: Received) -> Result<(), PublishError> {
        if self.cancels.contains(&received.transfer) {
            return Err(PublishError::Cancelled);
        }
        let t = self
            .transfers
            .get_mut(&received.transfer)
            .ok_or(PublishError::Transition(TransitionError::NoSuchItem {
                item: received.item,
            }))?;
        t.publish(received.item, &self.trust)?;
        self.received.push(received);
        Ok(())
    }

    /// Asks the connection carrying `id` to cancel it.
    pub fn request_cancel(&mut self, id: TransferId) -> bool {
        match self.transfers.get(&id) {
            Some(t) if !t.is_terminal() => self.cancels.insert(id),
            _ => false,
        }
    }

    pub(crate) fn cancel_requested(&self, id: &TransferId) -> bool {
        self.cancels.contains(id)
    }

    /// Revokes a device: trust first, then every transfer with it. Each
    /// connection notices on its next wake and stops.
    pub fn revoke(&mut self, device: &DeviceId) -> Result<(), TrustError> {
        self.trust.revoke(device)?;
        for transfer in self.transfers.values_mut() {
            if transfer.peer() == *device {
                transfer.revoke();
            }
        }
        Ok(())
    }

    /// Adds an invitation, dropping closed ones first.
    pub(crate) fn add_invitation(
        &mut self,
        now: Moment,
        pending: PendingInvitation,
    ) -> Result<(), StateError> {
        self.invitations
            .retain(|_, p| p.invitation.state(now) == InvitationState::Open);
        if self.invitations.len() >= MAX_OPEN_INVITATIONS {
            return Err(StateError::TooManyInvitations);
        }
        self.invitations.insert(pending.invitation.id(), pending);
        Ok(())
    }
}
