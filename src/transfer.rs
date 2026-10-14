//! Offer and item states, and the batch outcome derived from them.
//! [`Transfer::outcome`] is computed from the items each time, so there is no
//! completion flag to drift out of step. `Published` comes only through
//! [`Transfer::publish`], which checks the peer's authority first.

use crate::identity::DeviceId;
use crate::ids::TransferId;
use crate::limits::MAX_BATCH_ITEMS;
use crate::trust::{Authority, TrustError, TrustStore};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemState {
    Queued,
    Transferring,
    Verifying,
    Paused,
    Resuming,
    Published,
    Rejected,
    Cancelled(CancelCause),
    Failed(FailCause),
    Expired,
}

impl ItemState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Published | Self::Rejected | Self::Cancelled(_) | Self::Failed(_) | Self::Expired
        )
    }

    /// Whether the item is moving or verifying bytes right now.
    fn is_active(self) -> bool {
        matches!(self, Self::Transferring | Self::Verifying | Self::Resuming)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelCause {
    Sender,
    Receiver,
    Revoked,
    Shutdown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailCause {
    /// The bytes did not match the digest in the offer.
    Digest,
    /// More or fewer bytes arrived than the offer declared.
    Length,
    /// The connection ended before the item finished.
    Connection,
    /// The peer broke the protocol.
    Protocol,
    /// The content was not what its kind requires, such as text that is not
    /// UTF-8 or a preview that differs from the start of the text.
    Content,
    /// The destination already had an item with that name.
    Exists,
    /// This device had no buffer room left to continue.
    Busy,
    /// Reading or writing local storage failed, such as a full disk.
    Storage,
    /// The sender's file changed after it was prepared, so its bytes no
    /// longer match the offer.
    SourceChanged,
    /// After a restart, the data kept for this item was missing or had
    /// changed, so what happened to it cannot be established.
    Lost,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemEvent {
    Start,
    BytesComplete,
    Pause,
    Resume,
    Resumed,
    Cancel(CancelCause),
    Fail(FailCause),
    Expire,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RejectCause {
    /// The receiving person declined.
    Receiver,
    /// The offer was over a limit.
    TooLarge,
    /// The receiver had no capacity for another transfer.
    Busy,
    /// The peer was revoked before the offer was accepted.
    Revoked,
    /// The sender withdrew the offer before the receiver answered.
    Withdrawn,
    /// The offer ended before it was answered: the connection failed or a
    /// device stopped.
    Interrupted,
    /// The offer broke a rule of this device, such as an item name that
    /// cannot be used as a file name here.
    Invalid,
}

/// Stable names for states and causes, used in storage and in output.
macro_rules! names {
    ($ty:ident { $($variant:ident => $name:literal),* $(,)? }) => {
        impl $ty {
            pub fn name(self) -> &'static str {
                match self { $(Self::$variant => $name),* }
            }

            pub fn from_name(name: &str) -> Option<Self> {
                match name { $($name => Some(Self::$variant),)* _ => None }
            }
        }
    };
}

names!(CancelCause {
    Sender => "sender",
    Receiver => "receiver",
    Revoked => "revoked",
    Shutdown => "shutdown",
});

names!(FailCause {
    Digest => "digest",
    Length => "length",
    Connection => "connection",
    Protocol => "protocol",
    Content => "content",
    Exists => "exists",
    Busy => "busy",
    Storage => "storage",
    SourceChanged => "source-changed",
    Lost => "lost",
});

names!(RejectCause {
    Receiver => "declined",
    TooLarge => "too-large",
    Busy => "busy",
    Revoked => "revoked",
    Withdrawn => "withdrawn",
    Interrupted => "interrupted",
    Invalid => "invalid",
});

impl ItemState {
    /// A stable name such as `published` or `failed:digest`.
    pub fn name(self) -> String {
        match self {
            Self::Queued => "queued".into(),
            Self::Transferring => "transferring".into(),
            Self::Verifying => "verifying".into(),
            Self::Paused => "paused".into(),
            Self::Resuming => "resuming".into(),
            Self::Published => "published".into(),
            Self::Rejected => "rejected".into(),
            Self::Cancelled(c) => format!("cancelled:{}", c.name()),
            Self::Failed(c) => format!("failed:{}", c.name()),
            Self::Expired => "expired".into(),
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name.split_once(':') {
            Some(("cancelled", c)) => Self::Cancelled(CancelCause::from_name(c)?),
            Some(("failed", c)) => Self::Failed(FailCause::from_name(c)?),
            Some(_) => return None,
            None => match name {
                "queued" => Self::Queued,
                "transferring" => Self::Transferring,
                "verifying" => Self::Verifying,
                "paused" => Self::Paused,
                "resuming" => Self::Resuming,
                "published" => Self::Published,
                "rejected" => Self::Rejected,
                "expired" => Self::Expired,
                _ => return None,
            },
        })
    }
}

impl OfferState {
    pub fn name(self) -> String {
        match self {
            Self::Offered => "offered".into(),
            Self::Accepted => "accepted".into(),
            Self::Rejected(c) => format!("rejected:{}", c.name()),
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name.split_once(':') {
            Some(("rejected", c)) => RejectCause::from_name(c).map(Self::Rejected),
            Some(_) => None,
            None => match name {
                "offered" => Some(Self::Offered),
                "accepted" => Some(Self::Accepted),
                _ => None,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OfferState {
    Offered,
    Accepted,
    Rejected(RejectCause),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TransitionError {
    #[error("item {item} does not exist")]
    NoSuchItem { item: usize },
    #[error("{event:?} is not allowed while the item is {from:?}")]
    NotAllowed { from: ItemState, event: ItemEvent },
    #[error("the offer is {0:?}, so it cannot change that way")]
    WrongOfferState(OfferState),
    #[error("another item of this transfer is already active")]
    AnotherItemActive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TransferError {
    #[error("an offer must contain at least one item")]
    Empty,
    #[error("an offer may contain at most {MAX_BATCH_ITEMS} items")]
    TooManyItems,
    #[error("the item states do not fit the offer state")]
    Inconsistent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PublishError {
    #[error(transparent)]
    Transition(#[from] TransitionError),
    /// Trust changed first. The item is now cancelled and was not published.
    #[error("not published: {0}")]
    Unauthorized(TrustError),
    /// Someone asked to cancel the transfer first.
    #[error("not published: the transfer is being cancelled")]
    Cancelled,
    /// The decision could not be saved, so it was not made.
    #[error("not done: the state database failed")]
    Store,
}

/// How many items ended each unsuccessful way.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Unsuccessful {
    pub rejected: usize,
    pub cancelled: usize,
    pub failed: usize,
    pub expired: usize,
}

/// The batch result, derived from the item states.
///
/// When items ended in different unsuccessful ways, the counts keep every
/// cause; no precedence rule hides one behind another.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatchOutcome {
    /// Waiting for the receiver's decision.
    Offered,
    /// At least one item is queued, active, or paused.
    InProgress,
    /// Every item was published.
    Completed,
    /// Some items were published and the rest ended unsuccessfully.
    PartiallyCompleted {
        published: usize,
        unsuccessful: Unsuccessful,
    },
    /// No item was published.
    Unsuccessful(Unsuccessful),
}

/// One offer and its items, as seen by one side.
#[derive(Clone, Debug)]
pub struct Transfer {
    id: TransferId,
    authority: Authority,
    offer_state: OfferState,
    items: Vec<ItemState>,
}

impl Transfer {
    /// A new offer of `items` items, authorized under `authority`.
    pub fn offered(
        id: TransferId,
        authority: Authority,
        items: usize,
    ) -> Result<Self, TransferError> {
        if items == 0 {
            return Err(TransferError::Empty);
        }
        if items > MAX_BATCH_ITEMS {
            return Err(TransferError::TooManyItems);
        }
        Ok(Self {
            id,
            authority,
            offer_state: OfferState::Offered,
            items: vec![ItemState::Queued; items],
        })
    }

    /// Rebuilds a transfer saved earlier. The same limits apply as to a
    /// new one, and an offer that was never accepted has no item progress.
    pub fn restore(
        id: TransferId,
        authority: Authority,
        offer_state: OfferState,
        items: Vec<ItemState>,
    ) -> Result<Self, TransferError> {
        let mut transfer = Self::offered(id, authority, items.len())?;
        let consistent = match offer_state {
            OfferState::Offered => items.iter().all(|s| *s == ItemState::Queued),
            OfferState::Rejected(_) => items.iter().all(|s| *s == ItemState::Rejected),
            OfferState::Accepted => !items.contains(&ItemState::Rejected),
        };
        if !consistent {
            return Err(TransferError::Inconsistent);
        }
        transfer.offer_state = offer_state;
        transfer.items = items;
        Ok(transfer)
    }

    pub fn id(&self) -> TransferId {
        self.id
    }

    pub fn peer(&self) -> DeviceId {
        self.authority.device()
    }

    pub fn authority(&self) -> Authority {
        self.authority
    }

    pub fn offer_state(&self) -> OfferState {
        self.offer_state
    }

    pub fn items(&self) -> &[ItemState] {
        &self.items
    }

    pub fn accept(&mut self, trust: &TrustStore) -> Result<(), PublishError> {
        if self.offer_state != OfferState::Offered {
            return Err(TransitionError::WrongOfferState(self.offer_state).into());
        }
        if let Err(e) = trust.check(&self.authority) {
            self.reject(RejectCause::Revoked);
            return Err(PublishError::Unauthorized(e));
        }
        self.offer_state = OfferState::Accepted;
        Ok(())
    }

    /// Declines an offer that was not accepted. Does nothing afterwards.
    pub fn reject(&mut self, cause: RejectCause) {
        if self.offer_state == OfferState::Offered {
            self.offer_state = OfferState::Rejected(cause);
            self.items.fill(ItemState::Rejected);
        }
    }

    /// Applies one event to one item of an accepted offer.
    pub fn apply(&mut self, item: usize, event: ItemEvent) -> Result<ItemState, TransitionError> {
        if self.offer_state != OfferState::Accepted {
            return Err(TransitionError::WrongOfferState(self.offer_state));
        }
        let from = *self
            .items
            .get(item)
            .ok_or(TransitionError::NoSuchItem { item })?;
        let to = next(from, event).ok_or(TransitionError::NotAllowed { from, event })?;
        let becomes_active = to.is_active() && !from.is_active();
        if becomes_active && self.items.iter().any(|s| s.is_active()) {
            return Err(TransitionError::AnotherItemActive);
        }
        self.items[item] = to;
        Ok(to)
    }

    /// Resumes a paused item if trust in the peer is unchanged.
    pub fn resume(&mut self, item: usize, trust: &TrustStore) -> Result<ItemState, PublishError> {
        if let Err(e) = trust.check(&self.authority) {
            self.revoke();
            return Err(PublishError::Unauthorized(e));
        }
        Ok(self.apply(item, ItemEvent::Resume)?)
    }

    /// Publishes a verified item if trust in the peer is unchanged.
    ///
    /// This is the commit point that shares one order with revocation. If
    /// the trust store has already revoked the peer, the item is cancelled
    /// instead and stays unpublished.
    pub fn publish(&mut self, item: usize, trust: &TrustStore) -> Result<(), PublishError> {
        if self.offer_state != OfferState::Accepted {
            return Err(TransitionError::WrongOfferState(self.offer_state).into());
        }
        let from = *self
            .items
            .get(item)
            .ok_or(TransitionError::NoSuchItem { item })?;
        if from != ItemState::Verifying {
            return Err(TransitionError::NotAllowed {
                from,
                event: ItemEvent::BytesComplete,
            }
            .into());
        }
        if let Err(e) = trust.check(&self.authority) {
            self.revoke();
            return Err(PublishError::Unauthorized(e));
        }
        self.items[item] = ItemState::Published;
        Ok(())
    }

    /// Records the receiver's report that an outgoing item was published.
    /// Only the receiver decides publication, so the sender does not consult
    /// its own trust store here.
    pub fn confirm_published(&mut self, item: usize) -> Result<(), TransitionError> {
        if self.offer_state != OfferState::Accepted {
            return Err(TransitionError::WrongOfferState(self.offer_state));
        }
        let from = *self
            .items
            .get(item)
            .ok_or(TransitionError::NoSuchItem { item })?;
        if from != ItemState::Verifying {
            return Err(TransitionError::NotAllowed {
                from,
                event: ItemEvent::BytesComplete,
            });
        }
        self.items[item] = ItemState::Published;
        Ok(())
    }

    /// Fails every unfinished item, for when the connection or the peer
    /// breaks the exchange.
    pub fn fail_rest(&mut self, cause: FailCause) {
        match self.offer_state {
            OfferState::Offered => self.reject(RejectCause::Interrupted),
            OfferState::Accepted => {
                for state in &mut self.items {
                    if !state.is_terminal() {
                        *state = ItemState::Failed(cause);
                    }
                }
            }
            OfferState::Rejected(_) => {}
        }
    }

    /// Applies revocation: an unanswered offer is rejected, and every
    /// unfinished item is cancelled. Published items stay published.
    pub fn revoke(&mut self) {
        match self.offer_state {
            OfferState::Offered => self.reject(RejectCause::Revoked),
            OfferState::Accepted => {
                for state in &mut self.items {
                    if !state.is_terminal() {
                        *state = ItemState::Cancelled(CancelCause::Revoked);
                    }
                }
            }
            OfferState::Rejected(_) => {}
        }
    }

    /// Records an item found committed under its final name after a
    /// restart, when the record of its publication was not saved. Only
    /// startup recovery calls this, with the file's identity as evidence.
    pub fn recovered_published(&mut self, item: usize) -> Result<(), TransitionError> {
        if self.offer_state != OfferState::Accepted {
            return Err(TransitionError::WrongOfferState(self.offer_state));
        }
        let from = *self
            .items
            .get(item)
            .ok_or(TransitionError::NoSuchItem { item })?;
        if from.is_terminal() {
            return Err(TransitionError::NotAllowed {
                from,
                event: ItemEvent::BytesComplete,
            });
        }
        self.items[item] = ItemState::Published;
        Ok(())
    }

    /// Pauses every unfinished item of an accepted offer, for when this
    /// device stops while work is under way. An unanswered offer ends as
    /// interrupted, since the question cannot be answered later.
    pub fn pause_all(&mut self) {
        match self.offer_state {
            OfferState::Offered => self.reject(RejectCause::Interrupted),
            OfferState::Accepted => {
                for state in &mut self.items {
                    if !state.is_terminal() {
                        *state = ItemState::Paused;
                    }
                }
            }
            OfferState::Rejected(_) => {}
        }
    }

    /// Expires every unfinished item: the time to resume has passed.
    pub fn expire_all(&mut self) {
        match self.offer_state {
            OfferState::Offered => self.reject(RejectCause::Interrupted),
            OfferState::Accepted => {
                for state in &mut self.items {
                    if !state.is_terminal() {
                        *state = ItemState::Expired;
                    }
                }
            }
            OfferState::Rejected(_) => {}
        }
    }

    /// Cancels every unfinished item for `cause`.
    pub fn cancel(&mut self, cause: CancelCause) {
        match self.offer_state {
            OfferState::Offered => self.reject(match cause {
                CancelCause::Revoked => RejectCause::Revoked,
                CancelCause::Receiver => RejectCause::Receiver,
                CancelCause::Sender | CancelCause::Shutdown => RejectCause::Withdrawn,
            }),
            OfferState::Accepted => {
                for state in &mut self.items {
                    if !state.is_terminal() {
                        *state = ItemState::Cancelled(cause);
                    }
                }
            }
            OfferState::Rejected(_) => {}
        }
    }

    pub fn is_terminal(&self) -> bool {
        match self.offer_state {
            OfferState::Offered => false,
            OfferState::Rejected(_) => true,
            OfferState::Accepted => self.items.iter().all(|s| s.is_terminal()),
        }
    }

    pub fn outcome(&self) -> BatchOutcome {
        match self.offer_state {
            OfferState::Offered => return BatchOutcome::Offered,
            OfferState::Rejected(_) => {
                return BatchOutcome::Unsuccessful(Unsuccessful {
                    rejected: self.items.len(),
                    ..Unsuccessful::default()
                });
            }
            OfferState::Accepted => {}
        }
        if !self.items.iter().all(|s| s.is_terminal()) {
            return BatchOutcome::InProgress;
        }
        let mut published = 0;
        let mut unsuccessful = Unsuccessful::default();
        for state in &self.items {
            match state {
                ItemState::Published => published += 1,
                ItemState::Rejected => unsuccessful.rejected += 1,
                ItemState::Cancelled(_) => unsuccessful.cancelled += 1,
                ItemState::Failed(_) => unsuccessful.failed += 1,
                ItemState::Expired => unsuccessful.expired += 1,
                ItemState::Queued
                | ItemState::Transferring
                | ItemState::Verifying
                | ItemState::Paused
                | ItemState::Resuming => {}
            }
        }
        if published == self.items.len() {
            BatchOutcome::Completed
        } else if published > 0 {
            BatchOutcome::PartiallyCompleted {
                published,
                unsuccessful,
            }
        } else {
            BatchOutcome::Unsuccessful(unsuccessful)
        }
    }
}

/// The item transition table. `None` means the event is not allowed.
fn next(from: ItemState, event: ItemEvent) -> Option<ItemState> {
    use ItemEvent as E;
    use ItemState as S;
    if from.is_terminal() {
        return None;
    }
    Some(match (from, event) {
        (S::Queued, E::Start) => S::Transferring,
        (S::Transferring, E::BytesComplete) => S::Verifying,
        // A verified item pauses when its publication cannot be confirmed.
        (S::Transferring | S::Resuming | S::Verifying, E::Pause) => S::Paused,
        (S::Paused, E::Resume) => S::Resuming,
        (S::Resuming, E::Resumed) => S::Transferring,
        (S::Queued | S::Paused, E::Expire) => S::Expired,
        (_, E::Cancel(cause)) => S::Cancelled(cause),
        (_, E::Fail(cause)) => S::Failed(cause),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::PeerText;
    use crate::identity::DeviceKey;
    use crate::trust::{LocalAlias, VerifiedPeer};
    use zeroize::Zeroizing;

    fn setup(items: usize) -> (TrustStore, Transfer, DeviceId) {
        let device = DeviceKey::from_seed(&Zeroizing::new([3; 32])).id();
        let mut trust = TrustStore::new();
        let authority = trust
            .trust(
                VerifiedPeer::new(device, PeerText::default()),
                LocalAlias::new("peer").unwrap(),
            )
            .unwrap();
        let transfer =
            Transfer::offered(TransferId::from_bytes([1; 16]), authority, items).unwrap();
        (trust, transfer, device)
    }

    fn deliver(t: &mut Transfer, item: usize, trust: &TrustStore) {
        t.apply(item, ItemEvent::Start).unwrap();
        t.apply(item, ItemEvent::BytesComplete).unwrap();
        t.publish(item, trust).unwrap();
    }

    #[test]
    fn transition_table_allows_only_the_declared_moves() {
        use ItemEvent as E;
        use ItemState as S;
        let allowed = [
            (S::Queued, E::Start, Some(S::Transferring)),
            (S::Queued, E::BytesComplete, None),
            (S::Queued, E::Pause, None),
            (S::Transferring, E::BytesComplete, Some(S::Verifying)),
            (S::Transferring, E::Resume, None),
            (S::Verifying, E::Pause, Some(S::Paused)),
            (
                S::Verifying,
                E::Fail(FailCause::Digest),
                Some(S::Failed(FailCause::Digest)),
            ),
            (S::Paused, E::Resume, Some(S::Resuming)),
            (S::Paused, E::Start, None),
            (S::Paused, E::Expire, Some(S::Expired)),
            (S::Resuming, E::Resumed, Some(S::Transferring)),
            (S::Transferring, E::Expire, None),
            (S::Published, E::Cancel(CancelCause::Sender), None),
            (S::Cancelled(CancelCause::Sender), E::Start, None),
            (S::Failed(FailCause::Length), E::Resume, None),
        ];
        for (from, event, to) in allowed {
            assert_eq!(next(from, event), to, "{from:?} + {event:?}");
        }
    }

    #[test]
    fn empty_and_oversized_manifests_are_refused() {
        let (_, t, _) = setup(1);
        let authority = t.authority();
        let id = TransferId::from_bytes([2; 16]);
        assert_eq!(
            Transfer::offered(id, authority, 0).err(),
            Some(TransferError::Empty)
        );
        assert_eq!(
            Transfer::offered(id, authority, MAX_BATCH_ITEMS + 1).err(),
            Some(TransferError::TooManyItems)
        );
        assert!(Transfer::offered(id, authority, MAX_BATCH_ITEMS).is_ok());
    }

    #[test]
    fn a_single_published_item_completes_the_transfer() {
        let (trust, mut t, _) = setup(1);
        t.accept(&trust).unwrap();
        assert_eq!(t.outcome(), BatchOutcome::InProgress);
        deliver(&mut t, 0, &trust);
        assert_eq!(t.outcome(), BatchOutcome::Completed);
    }

    #[test]
    fn mixed_published_and_failed_items_are_partially_completed() {
        let (trust, mut t, _) = setup(3);
        t.accept(&trust).unwrap();
        deliver(&mut t, 0, &trust);
        t.apply(1, ItemEvent::Start).unwrap();
        t.apply(1, ItemEvent::BytesComplete).unwrap();
        t.apply(1, ItemEvent::Fail(FailCause::Digest)).unwrap();
        t.apply(2, ItemEvent::Cancel(CancelCause::Receiver))
            .unwrap();
        assert_eq!(
            t.outcome(),
            BatchOutcome::PartiallyCompleted {
                published: 1,
                unsuccessful: Unsuccessful {
                    failed: 1,
                    cancelled: 1,
                    ..Unsuccessful::default()
                }
            }
        );
    }

    #[test]
    fn a_paused_item_keeps_the_batch_in_progress() {
        let (trust, mut t, _) = setup(2);
        t.accept(&trust).unwrap();
        deliver(&mut t, 0, &trust);
        t.apply(1, ItemEvent::Start).unwrap();
        t.apply(1, ItemEvent::Pause).unwrap();
        assert_eq!(t.outcome(), BatchOutcome::InProgress);
        assert!(!t.is_terminal());
    }

    #[test]
    fn only_one_item_of_a_transfer_is_active_at_a_time() {
        let (trust, mut t, _) = setup(2);
        t.accept(&trust).unwrap();
        t.apply(0, ItemEvent::Start).unwrap();
        assert_eq!(
            t.apply(1, ItemEvent::Start),
            Err(TransitionError::AnotherItemActive)
        );
    }

    #[test]
    fn duplicate_and_out_of_order_requests_change_nothing() {
        let (trust, mut t, _) = setup(1);
        assert_eq!(
            t.apply(0, ItemEvent::Start),
            Err(TransitionError::WrongOfferState(OfferState::Offered))
        );
        t.accept(&trust).unwrap();
        assert!(t.accept(&trust).is_err());
        deliver(&mut t, 0, &trust);
        assert!(t.apply(0, ItemEvent::Cancel(CancelCause::Sender)).is_err());
        assert!(t.publish(0, &trust).is_err());
        assert_eq!(
            t.apply(5, ItemEvent::Start),
            Err(TransitionError::NoSuchItem { item: 5 })
        );
        assert_eq!(t.outcome(), BatchOutcome::Completed);
    }

    #[test]
    fn revocation_before_the_commit_wins_and_nothing_is_published() {
        let (mut trust, mut t, device) = setup(1);
        t.accept(&trust).unwrap();
        t.apply(0, ItemEvent::Start).unwrap();
        t.apply(0, ItemEvent::BytesComplete).unwrap();
        trust.revoke(&device).unwrap();
        // The verification result arrives after revocation.
        assert_eq!(
            t.publish(0, &trust),
            Err(PublishError::Unauthorized(TrustError::Revoked))
        );
        assert_eq!(t.items(), [ItemState::Cancelled(CancelCause::Revoked)]);
        assert_eq!(
            t.outcome(),
            BatchOutcome::Unsuccessful(Unsuccessful {
                cancelled: 1,
                ..Unsuccessful::default()
            })
        );
    }

    #[test]
    fn a_commit_before_revocation_stays_published_and_the_rest_is_cancelled() {
        let (mut trust, mut t, device) = setup(2);
        t.accept(&trust).unwrap();
        deliver(&mut t, 0, &trust);
        t.apply(1, ItemEvent::Start).unwrap();
        trust.revoke(&device).unwrap();
        t.revoke();
        assert_eq!(
            t.items(),
            [
                ItemState::Published,
                ItemState::Cancelled(CancelCause::Revoked)
            ]
        );
    }

    #[test]
    fn revocation_rejects_an_unanswered_offer_and_blocks_resume() {
        let (mut trust, mut offered, device) = setup(1);
        let (_, mut paused, _) = setup(1);
        paused.accept(&trust).unwrap();
        paused.apply(0, ItemEvent::Start).unwrap();
        paused.apply(0, ItemEvent::Pause).unwrap();
        trust.revoke(&device).unwrap();

        assert!(matches!(
            offered.accept(&trust),
            Err(PublishError::Unauthorized(_))
        ));
        assert_eq!(
            offered.offer_state(),
            OfferState::Rejected(RejectCause::Revoked)
        );
        assert!(matches!(
            paused.resume(0, &trust),
            Err(PublishError::Unauthorized(_))
        ));
        assert_eq!(paused.items(), [ItemState::Cancelled(CancelCause::Revoked)]);
    }

    #[test]
    fn fresh_pairing_does_not_revive_a_cancelled_transfer() {
        let (mut trust, mut t, device) = setup(1);
        t.accept(&trust).unwrap();
        t.apply(0, ItemEvent::Start).unwrap();
        t.apply(0, ItemEvent::Pause).unwrap();
        trust.revoke(&device).unwrap();
        trust
            .trust(
                VerifiedPeer::new(device, PeerText::default()),
                LocalAlias::new("peer").unwrap(),
            )
            .unwrap();
        // Trust is back, but this transfer's authority predates it.
        assert!(matches!(
            t.resume(0, &trust),
            Err(PublishError::Unauthorized(TrustError::Stale))
        ));
        assert!(t.is_terminal());
    }
}
