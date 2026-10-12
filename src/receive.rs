//! The receiving side of an authenticated session, as a state machine.
//!
//! The runtime decodes frames and feeds them in; the machine changes
//! [`State`] and returns what to do next. It performs no I/O, so every
//! duplicate, reordered, oversized, or hostile message can be tested
//! directly.
//!
//! ```text
//! Idle ──Offer (valid, room, capacity)──▶ Deciding ──yes──▶ Receiving ──last item──▶ Idle
//!   │                                       │  no                │
//!   └─Offer (invalid) → OfferReject          └──▶ OfferReject     ├─ Chunk / ItemEnd per item
//!                                                                 ├─ Cancel from sender
//!   any unexpected message ─▶ Error + close                       └─ revocation, local cancel
//! ```
//!
//! Every chunk and every publication checks the sender's authority first.
//! Received bytes stay in receiver-owned staging until the item's exact
//! length and digest match the offer, and only then go to the model
//! publisher through [`crate::transfer::Transfer::publish`].

use crate::consent::ConsentView;
use crate::identity::DeviceId;
use crate::ids::TransferId;
use crate::limits::{ActiveSlot, Budget, BufferLease, SessionSlot, TransferSlot};
use crate::offer::{Offer, OfferError, PayloadKind};
use crate::protocol::{CancelReason, ErrorCode, ItemResult, Message, RejectReason};
use crate::state::{Received, Room, State};
use crate::transfer::{
    CancelCause, FailCause, ItemEvent, ItemState, PublishError, RejectCause, Transfer,
};
use crate::trust::Authority;

/// What the runtime should do next.
#[derive(Debug, PartialEq, Eq)]
pub enum Output {
    Send(Message),
    /// Show the consent screen and call [`Receiver::on_answer`].
    Ask {
        transfer: TransferId,
        view: ConsentView,
    },
    /// Send an error code, then end the session.
    Abort(ErrorCode),
    /// End the session after the messages before this one.
    End,
}

struct Pending {
    offer: Offer,
    _slot: TransferSlot,
}

struct Active {
    offer: Offer,
    _slot: TransferSlot,
    _active: ActiveSlot,
    /// Buffer budget for the staged bytes, held until the transfer ends.
    _staging: BufferLease,
    item: usize,
    buf: Vec<u8>,
    /// Room still reserved for this transfer's unfinished items.
    reserved: Room,
}

enum Step {
    Idle,
    Deciding(Pending),
    Receiving(Active),
    Closed,
}

pub struct Receiver {
    peer: DeviceId,
    authority: Authority,
    budget: Budget,
    session: SessionSlot,
    step: Step,
}

impl Receiver {
    pub fn new(authority: Authority, budget: Budget, session: SessionSlot) -> Self {
        Self {
            peer: authority.device(),
            authority,
            budget,
            session,
            step: Step::Idle,
        }
    }

    pub fn is_closed(&self) -> bool {
        matches!(self.step, Step::Closed)
    }

    /// The transfer waiting for the receiving person's answer, if any.
    pub fn awaiting_answer(&self) -> Option<TransferId> {
        match &self.step {
            Step::Deciding(p) => Some(p.offer.transfer()),
            _ => None,
        }
    }

    pub fn on_message(&mut self, msg: Message, state: &mut State) -> Vec<Output> {
        if self.is_closed() {
            return Vec::new();
        }
        if state.trust().check(&self.authority).is_err() {
            return self.revoked(state);
        }
        // A cancel request that reached the state first wins over whatever
        // this message would have done, publication included.
        if self.cancel_requested(state) {
            return self.cancel_locally(state);
        }
        match (std::mem::replace(&mut self.step, Step::Idle), msg) {
            (Step::Idle, Message::Offer(fields)) => self.on_offer(fields, state),
            (Step::Deciding(p), Message::Cancel { transfer, .. })
                if transfer == p.offer.transfer() =>
            {
                if let Some(t) = state.transfer_mut(&transfer) {
                    t.cancel(CancelCause::Sender);
                }
                state.mark_finished(transfer);
                vec![Output::End]
            }
            (
                Step::Receiving(a),
                Message::Chunk {
                    transfer,
                    item,
                    offset,
                    data,
                },
            ) if transfer == a.offer.transfer() => self.on_chunk(a, item, offset, data, state),
            (Step::Receiving(a), Message::ItemEnd { transfer, item })
                if transfer == a.offer.transfer() =>
            {
                self.on_item_end(a, item, state)
            }
            (Step::Receiving(a), Message::Cancel { transfer, .. })
                if transfer == a.offer.transfer() =>
            {
                if let Some(t) = state.transfer_mut(&transfer) {
                    t.cancel(CancelCause::Sender);
                }
                state.release(a.reserved);
                state.mark_finished(transfer);
                vec![Output::End]
            }
            (step, _) => {
                self.abort_transfer(step, FailCause::Protocol, state);
                self.close(ErrorCode::UnexpectedMessage)
            }
        }
    }

    /// The person answered the consent screen for `transfer`.
    pub fn on_answer(
        &mut self,
        transfer: TransferId,
        accept: bool,
        state: &mut State,
    ) -> Vec<Output> {
        let pending = match std::mem::replace(&mut self.step, Step::Idle) {
            Step::Deciding(p) if p.offer.transfer() == transfer => p,
            other => {
                self.step = other;
                return Vec::new();
            }
        };
        let id = pending.offer.transfer();
        if state.cancel_requested(&id) {
            self.step = Step::Deciding(pending);
            return self.cancel_locally(state);
        }
        if !accept {
            if let Some(t) = state.transfer_mut(&id) {
                t.reject(RejectCause::Receiver);
            }
            state.mark_finished(id);
            return vec![Output::Send(reject(id, RejectReason::Declined))];
        }
        let total = pending.offer.total();
        let room = Room::new(total, pending.offer.items().len());
        if state.reserve(room).is_err() {
            return self.decline_after_check(
                id,
                RejectCause::TooLarge,
                RejectReason::TooLarge,
                state,
            );
        }
        // Staged bytes are payload buffers, so they come from the same
        // budget as frames, but always leave room to read the next frame.
        let staging = self.budget.lease_held(total);
        let active = self.budget.activate(&self.session, &pending._slot);
        let (active, staging) = match (active, staging) {
            (Ok(a), Ok(s)) => (a, s),
            _ => {
                state.release(room);
                return self.decline_after_check(id, RejectCause::Busy, RejectReason::Busy, state);
            }
        };
        // Accepting checks current trust; a revoked peer's offer is rejected.
        if state.accept(&id).is_err() {
            state.release(room);
            state.mark_finished(id);
            return vec![
                Output::Send(reject(id, RejectReason::NotTrusted)),
                Output::End,
            ];
        }
        let digest = pending.offer.digest();
        self.step = Step::Receiving(Active {
            offer: pending.offer,
            _slot: pending._slot,
            _active: active,
            _staging: staging,
            item: 0,
            buf: Vec::new(),
            reserved: room,
        });
        vec![Output::Send(Message::OfferAccept {
            transfer: id,
            offer_digest: digest,
        })]
    }

    /// Nobody can be asked about `transfer` now, such as when too many
    /// questions already wait for an answer. The offer is refused as busy.
    pub fn on_unasked(&mut self, transfer: TransferId, state: &mut State) -> Vec<Output> {
        match std::mem::replace(&mut self.step, Step::Idle) {
            Step::Deciding(p) if p.offer.transfer() == transfer => {
                self.decline_after_check(transfer, RejectCause::Busy, RejectReason::Busy, state)
            }
            other => {
                self.step = other;
                Vec::new()
            }
        }
    }

    /// Something changed locally: trust, or a cancel request.
    pub fn on_wake(&mut self, state: &mut State) -> Vec<Output> {
        if self.is_closed() {
            return Vec::new();
        }
        if state.trust().check(&self.authority).is_err() {
            return self.revoked(state);
        }
        if self.cancel_requested(state) {
            return self.cancel_locally(state);
        }
        Vec::new()
    }

    /// Whether [`Receiver::on_wake`] would stop the session now. The runtime
    /// asks this to interrupt a write that the peer is not reading.
    pub fn should_stop(&self, state: &State) -> bool {
        !self.is_closed()
            && (state.trust().check(&self.authority).is_err() || self.cancel_requested(state))
    }

    fn current(&self) -> Option<TransferId> {
        match &self.step {
            Step::Deciding(p) => Some(p.offer.transfer()),
            Step::Receiving(a) => Some(a.offer.transfer()),
            Step::Idle | Step::Closed => None,
        }
    }

    fn cancel_requested(&self, state: &State) -> bool {
        self.current().is_some_and(|t| state.cancel_requested(&t))
    }

    /// The receiving person cancelled the current transfer.
    fn cancel_locally(&mut self, state: &mut State) -> Vec<Output> {
        let Some(transfer) = self.current() else {
            return Vec::new();
        };
        if let Step::Receiving(a) = std::mem::replace(&mut self.step, Step::Closed) {
            state.release(a.reserved);
        }
        if let Some(t) = state.transfer_mut(&transfer) {
            t.cancel(CancelCause::Receiver);
        }
        state.mark_finished(transfer);
        vec![
            Output::Send(Message::Cancel {
                transfer,
                reason: CancelReason::Receiver,
            }),
            Output::End,
        ]
    }

    /// The connection ended. Unfinished work fails; nothing is published.
    pub fn on_disconnect(&mut self, state: &mut State) {
        let step = std::mem::replace(&mut self.step, Step::Closed);
        self.abort_transfer(step, FailCause::Connection, state);
    }

    fn on_offer(&mut self, fields: crate::offer::OfferFields, state: &mut State) -> Vec<Output> {
        let id = fields.transfer;
        let offer = match Offer::validate(fields) {
            Ok(o) => o,
            Err(e) => {
                let reason = match e {
                    OfferError::ItemTooLarge
                    | OfferError::TotalTooLarge
                    | OfferError::TooManyItems => RejectReason::TooLarge,
                    _ => RejectReason::Malformed,
                };
                return vec![Output::Send(reject(id, reason))];
            }
        };
        // A transfer ID seen before is a replayed or duplicated offer.
        if state.transfer(&id).is_some() {
            return self.close(ErrorCode::UnexpectedMessage);
        }
        let transfer = match Transfer::offered(id, self.authority, offer.items().len()) {
            Ok(t) => t,
            Err(_) => return vec![Output::Send(reject(id, RejectReason::Malformed))],
        };
        // An offer that could never be staged next to a frame is refused
        // before the person is asked.
        if !state.has_room(Room::new(offer.total(), offer.items().len()))
            || offer.total() > self.budget.held_capacity()
        {
            return vec![Output::Send(reject(id, RejectReason::TooLarge))];
        }
        let slot = match self.budget.admit_transfer() {
            Ok(s) => s,
            Err(_) => return vec![Output::Send(reject(id, RejectReason::Busy))],
        };
        if state.insert_transfer(transfer).is_err() {
            return vec![Output::Send(reject(id, RejectReason::Busy))];
        }
        let Some(record) = state.trust().record(&self.peer) else {
            return self.revoked(state);
        };
        let view = ConsentView::new(&offer, record);
        self.step = Step::Deciding(Pending { offer, _slot: slot });
        vec![Output::Ask { transfer: id, view }]
    }

    fn on_chunk(
        &mut self,
        mut a: Active,
        item: u16,
        offset: u64,
        data: Vec<u8>,
        state: &mut State,
    ) -> Vec<Output> {
        let id = a.offer.transfer();
        let expected = a.offer.items()[a.item].size;
        let within = usize::from(item) == a.item
            && offset == a.buf.len() as u64
            && (a.buf.len() as u64)
                .checked_add(data.len() as u64)
                .is_some_and(|end| end <= expected);
        if !within {
            let result = if usize::from(item) == a.item && offset == a.buf.len() as u64 {
                ItemResult::LengthMismatch
            } else {
                // A chunk for another item or offset is out of order.
                return self.fail_and_close(
                    a,
                    FailCause::Protocol,
                    ErrorCode::UnexpectedMessage,
                    state,
                );
            };
            return self.fail_item_and_close(a, FailCause::Length, result, state);
        }
        if let Some(t) = state.transfer_mut(&id)
            && t.items()[a.item] == ItemState::Queued
            && t.apply(a.item, ItemEvent::Start).is_err()
        {
            return self.fail_and_close(a, FailCause::Protocol, ErrorCode::Internal, state);
        }
        if a.buf.capacity() == 0 {
            // Allocate exactly the leased size once, instead of growing by
            // doubling past it. The size is at most the staging limit.
            a.buf.reserve_exact(usize::try_from(expected).unwrap_or(0));
        }
        a.buf.extend_from_slice(&data);
        self.step = Step::Receiving(a);
        Vec::new()
    }

    fn on_item_end(&mut self, mut a: Active, item: u16, state: &mut State) -> Vec<Output> {
        let id = a.offer.transfer();
        if usize::from(item) != a.item {
            return self.fail_and_close(
                a,
                FailCause::Protocol,
                ErrorCode::UnexpectedMessage,
                state,
            );
        }
        let index = a.item;
        let declared = a.offer.items()[index].clone();
        if a.buf.len() as u64 != declared.size {
            return self.fail_item_and_close(
                a,
                FailCause::Length,
                ItemResult::LengthMismatch,
                state,
            );
        }
        let started = match state.transfer_mut(&id) {
            Some(t) => {
                let start_ok = t.items()[index] != ItemState::Queued
                    || t.apply(index, ItemEvent::Start).is_ok();
                start_ok && t.apply(index, ItemEvent::BytesComplete).is_ok()
            }
            None => false,
        };
        if !started {
            return self.fail_and_close(a, FailCause::Protocol, ErrorCode::Internal, state);
        }
        let bytes = std::mem::take(&mut a.buf);
        let failure = if *blake3::hash(&bytes).as_bytes() != declared.digest {
            Some(FailCause::Digest)
        } else if !content_matches_kind(&a.offer, &bytes) {
            Some(FailCause::Content)
        } else {
            None
        };
        let result = match failure {
            Some(cause) => {
                if let Some(t) = state.transfer_mut(&id) {
                    let _ = t.apply(index, ItemEvent::Fail(cause));
                }
                if cause == FailCause::Content {
                    ItemResult::ContentMismatch
                } else {
                    ItemResult::DigestMismatch
                }
            }
            None => match state.publish(Received {
                transfer: id,
                item: index,
                from: self.peer,
                kind: a.offer.kind(),
                name: declared.name,
                bytes,
            }) {
                Ok(()) => ItemResult::Published,
                Err(PublishError::Unauthorized(_)) => ItemResult::Unauthorized,
                Err(PublishError::Cancelled | PublishError::Transition(_)) => ItemResult::Cancelled,
            },
        };
        // A published item keeps its room until it is taken.
        let room = Room::item(declared.size);
        a.reserved = a.reserved.saturating_sub(room);
        if result != ItemResult::Published {
            state.release(room);
        }
        let mut out = vec![Output::Send(Message::ItemResult {
            transfer: id,
            item,
            result,
        })];
        if result == ItemResult::Unauthorized {
            // Revocation won; publish cancelled every unfinished item.
            state.release(a.reserved);
            state.mark_finished(id);
            out.push(Output::Send(Message::Cancel {
                transfer: id,
                reason: CancelReason::Revoked,
            }));
            out.push(Output::End);
            self.step = Step::Closed;
            return out;
        }
        a.item += 1;
        if a.item == a.offer.items().len() {
            state.release(a.reserved);
            state.mark_finished(id);
            self.step = Step::Idle;
        } else {
            self.step = Step::Receiving(a);
        }
        out
    }

    fn revoked(&mut self, state: &mut State) -> Vec<Output> {
        let step = std::mem::replace(&mut self.step, Step::Closed);
        let mut out = Vec::new();
        let transfer = match step {
            Step::Deciding(p) => Some((p.offer.transfer(), Room::default())),
            Step::Receiving(a) => Some((a.offer.transfer(), a.reserved)),
            Step::Idle | Step::Closed => None,
        };
        if let Some((id, reserved)) = transfer {
            state.release(reserved);
            if let Some(t) = state.transfer_mut(&id) {
                t.revoke();
            }
            state.mark_finished(id);
            out.push(Output::Send(Message::Cancel {
                transfer: id,
                reason: CancelReason::Revoked,
            }));
        }
        out.push(Output::Abort(ErrorCode::NotTrusted));
        out
    }

    fn decline_after_check(
        &mut self,
        id: TransferId,
        cause: RejectCause,
        reason: RejectReason,
        state: &mut State,
    ) -> Vec<Output> {
        if let Some(t) = state.transfer_mut(&id) {
            t.reject(cause);
        }
        state.mark_finished(id);
        vec![Output::Send(reject(id, reason))]
    }

    fn fail_item_and_close(
        &mut self,
        a: Active,
        cause: FailCause,
        result: ItemResult,
        state: &mut State,
    ) -> Vec<Output> {
        let id = a.offer.transfer();
        let item = a.item as u16;
        if let Some(t) = state.transfer_mut(&id) {
            if t.items()[a.item] == ItemState::Queued {
                let _ = t.apply(a.item, ItemEvent::Start);
            }
            let _ = t.apply(a.item, ItemEvent::Fail(cause));
        }
        let mut out = vec![Output::Send(Message::ItemResult {
            transfer: id,
            item,
            result,
        })];
        out.extend(self.fail_and_close(a, FailCause::Protocol, ErrorCode::Malformed, state));
        out
    }

    fn fail_and_close(
        &mut self,
        a: Active,
        cause: FailCause,
        code: ErrorCode,
        state: &mut State,
    ) -> Vec<Output> {
        self.abort_transfer(Step::Receiving(a), cause, state);
        self.close(code)
    }

    fn abort_transfer(&mut self, step: Step, cause: FailCause, state: &mut State) {
        let (id, reserved) = match step {
            Step::Deciding(p) => (p.offer.transfer(), Room::default()),
            Step::Receiving(a) => (a.offer.transfer(), a.reserved),
            Step::Idle | Step::Closed => return,
        };
        state.release(reserved);
        if let Some(t) = state.transfer_mut(&id) {
            t.fail_rest(cause);
        }
        state.mark_finished(id);
    }

    fn close(&mut self, code: ErrorCode) -> Vec<Output> {
        self.step = Step::Closed;
        vec![Output::Abort(code)]
    }
}

fn reject(transfer: TransferId, reason: RejectReason) -> Message {
    Message::OfferReject { transfer, reason }
}

/// Text must be UTF-8, and its preview must be the start of the text.
fn content_matches_kind(offer: &Offer, bytes: &[u8]) -> bool {
    match offer.kind() {
        PayloadKind::Files => true,
        PayloadKind::Text | PayloadKind::Url => {
            std::str::from_utf8(bytes).is_ok() && bytes.starts_with(offer.preview().as_bytes())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::PeerText;
    use crate::identity::DeviceKey;
    use crate::limits::{Limits, MIN_BUFFER_BYTES, Usage};
    use crate::transfer::BatchOutcome;
    use crate::trust::{LocalAlias, VerifiedPeer};
    use zeroize::Zeroizing;

    struct Fixture {
        state: State,
        budget: Budget,
        rx: Receiver,
        peer: DeviceId,
    }

    fn fixture() -> Fixture {
        let peer = DeviceKey::from_seed(&Zeroizing::new([8; 32])).id();
        let mut state = State::new();
        let authority = state
            .trust
            .trust(
                VerifiedPeer::new(peer, PeerText::default()),
                LocalAlias::new("sender").unwrap(),
            )
            .unwrap();
        let budget = Budget::new(Limits::COMPILED);
        let session = budget.admit_session().unwrap();
        let rx = Receiver::new(authority, budget.clone(), session);
        Fixture {
            state,
            budget,
            rx,
            peer,
        }
    }

    fn id(n: u8) -> TransferId {
        TransferId::from_bytes([n; 16])
    }

    fn file_offer(n: u8, content: &[u8]) -> Message {
        Message::Offer(Offer::for_files(id(n), &[("f", content)]).unwrap().fields())
    }

    fn chunk(n: u8, item: u16, offset: u64, data: &[u8]) -> Message {
        Message::Chunk {
            transfer: id(n),
            item,
            offset,
            data: data.to_vec(),
        }
    }

    fn end(n: u8, item: u16) -> Message {
        Message::ItemEnd {
            transfer: id(n),
            item,
        }
    }

    /// Offers `content` as transfer `n` and accepts it.
    fn accepted(f: &mut Fixture, n: u8, content: &[u8]) {
        let out = f.rx.on_message(file_offer(n, content), &mut f.state);
        assert!(matches!(out.as_slice(), [Output::Ask { .. }]), "{out:?}");
        let out = f.rx.on_answer(id(n), true, &mut f.state);
        assert!(
            matches!(out.as_slice(), [Output::Send(Message::OfferAccept { .. })]),
            "{out:?}"
        );
    }

    fn items(f: &Fixture, n: u8) -> Vec<ItemState> {
        f.state.transfer(&id(n)).unwrap().items().to_vec()
    }

    #[test]
    fn a_complete_item_is_published_only_after_its_length_and_digest_match() {
        let mut f = fixture();
        accepted(&mut f, 1, b"hello world");
        assert!(
            f.rx.on_message(chunk(1, 0, 0, b"hello "), &mut f.state)
                .is_empty()
        );
        assert!(f.state.received().is_empty());
        assert!(
            f.rx.on_message(chunk(1, 0, 6, b"world"), &mut f.state)
                .is_empty()
        );
        let out = f.rx.on_message(end(1, 0), &mut f.state);
        assert_eq!(
            out,
            [Output::Send(Message::ItemResult {
                transfer: id(1),
                item: 0,
                result: ItemResult::Published
            })]
        );
        assert_eq!(f.state.received()[0].bytes, b"hello world");
        assert_eq!(f.state.received()[0].from, f.peer);
        assert_eq!(
            f.state.transfer(&id(1)).unwrap().outcome(),
            BatchOutcome::Completed
        );
    }

    #[test]
    fn a_declined_offer_writes_nothing_and_reserves_nothing() {
        let mut f = fixture();
        f.rx.on_message(file_offer(1, b"data"), &mut f.state);
        let out = f.rx.on_answer(id(1), false, &mut f.state);
        assert_eq!(out, [Output::Send(reject(id(1), RejectReason::Declined))]);
        assert_eq!(items(&f, 1), [ItemState::Rejected]);
        assert_eq!(f.state.held_bytes(), 0);
        assert!(f.state.received().is_empty());
    }

    #[test]
    fn a_replayed_offer_identifier_ends_the_session() {
        let mut f = fixture();
        f.rx.on_message(file_offer(1, b"data"), &mut f.state);
        f.rx.on_answer(id(1), false, &mut f.state);
        let out = f.rx.on_message(file_offer(1, b"data"), &mut f.state);
        assert_eq!(out, [Output::Abort(ErrorCode::UnexpectedMessage)]);
        assert!(f.rx.is_closed());
    }

    #[test]
    fn a_chunk_before_acceptance_ends_the_session_without_writing() {
        let mut f = fixture();
        f.rx.on_message(file_offer(1, b"data"), &mut f.state);
        let out = f.rx.on_message(chunk(1, 0, 0, b"da"), &mut f.state);
        assert_eq!(out, [Output::Abort(ErrorCode::UnexpectedMessage)]);
        assert!(f.state.received().is_empty());
        assert!(f.state.transfer(&id(1)).unwrap().is_terminal());
    }

    #[test]
    fn an_out_of_order_chunk_fails_the_item() {
        let mut f = fixture();
        accepted(&mut f, 1, b"abcdef");
        f.rx.on_message(chunk(1, 0, 0, b"ab"), &mut f.state);
        let out = f.rx.on_message(chunk(1, 0, 4, b"ef"), &mut f.state);
        assert_eq!(out, [Output::Abort(ErrorCode::UnexpectedMessage)]);
        assert_eq!(items(&f, 1), [ItemState::Failed(FailCause::Protocol)]);
        assert_eq!(f.state.held_bytes(), 0);
    }

    #[test]
    fn bytes_beyond_the_offered_length_fail_the_item() {
        let mut f = fixture();
        accepted(&mut f, 1, b"abc");
        let out = f.rx.on_message(chunk(1, 0, 0, b"abcd"), &mut f.state);
        assert_eq!(
            out,
            [
                Output::Send(Message::ItemResult {
                    transfer: id(1),
                    item: 0,
                    result: ItemResult::LengthMismatch
                }),
                Output::Abort(ErrorCode::Malformed)
            ]
        );
        assert_eq!(items(&f, 1), [ItemState::Failed(FailCause::Length)]);
        assert!(f.state.received().is_empty());
    }

    #[test]
    fn an_item_that_ends_short_fails_with_a_length_mismatch() {
        let mut f = fixture();
        accepted(&mut f, 1, b"abc");
        f.rx.on_message(chunk(1, 0, 0, b"ab"), &mut f.state);
        let out = f.rx.on_message(end(1, 0), &mut f.state);
        assert!(out.contains(&Output::Send(Message::ItemResult {
            transfer: id(1),
            item: 0,
            result: ItemResult::LengthMismatch
        })));
        assert_eq!(items(&f, 1), [ItemState::Failed(FailCause::Length)]);
    }

    #[test]
    fn bytes_that_do_not_match_the_offered_digest_are_not_published() {
        let mut f = fixture();
        accepted(&mut f, 1, b"abc");
        f.rx.on_message(chunk(1, 0, 0, b"abX"), &mut f.state);
        let out = f.rx.on_message(end(1, 0), &mut f.state);
        assert_eq!(
            out,
            [Output::Send(Message::ItemResult {
                transfer: id(1),
                item: 0,
                result: ItemResult::DigestMismatch
            })]
        );
        assert_eq!(items(&f, 1), [ItemState::Failed(FailCause::Digest)]);
        assert!(f.state.received().is_empty());
        assert_eq!(f.state.held_bytes(), 0);
    }

    #[test]
    fn text_whose_preview_differs_from_its_content_is_refused() {
        let mut f = fixture();
        let real = b"pay the invoice";
        let mut fields = Offer::for_text(id(1), PayloadKind::Text, b"say hi to mum!!")
            .unwrap()
            .fields();
        fields.items[0].2 = *blake3::hash(real).as_bytes();
        f.rx.on_message(Message::Offer(fields), &mut f.state);
        f.rx.on_answer(id(1), true, &mut f.state);
        f.rx.on_message(chunk(1, 0, 0, real), &mut f.state);
        let out = f.rx.on_message(end(1, 0), &mut f.state);
        // The sender learns the real reason, not a digest mismatch.
        assert_eq!(
            out,
            [Output::Send(Message::ItemResult {
                transfer: id(1),
                item: 0,
                result: ItemResult::ContentMismatch
            })]
        );
        assert_eq!(items(&f, 1), [ItemState::Failed(FailCause::Content)]);
        assert!(f.state.received().is_empty());
    }

    #[test]
    fn revocation_during_an_item_cancels_it_and_publishes_nothing() {
        let mut f = fixture();
        accepted(&mut f, 1, b"abcdef");
        f.rx.on_message(chunk(1, 0, 0, b"abc"), &mut f.state);
        assert!(!f.rx.should_stop(&f.state));
        f.state.revoke(&f.peer).unwrap();
        // A write already under way is interrupted on this answer.
        assert!(f.rx.should_stop(&f.state));
        let out = f.rx.on_wake(&mut f.state);
        assert_eq!(
            out,
            [
                Output::Send(Message::Cancel {
                    transfer: id(1),
                    reason: CancelReason::Revoked
                }),
                Output::Abort(ErrorCode::NotTrusted)
            ]
        );
        assert_eq!(items(&f, 1), [ItemState::Cancelled(CancelCause::Revoked)]);
        assert_eq!(f.state.held_bytes(), 0);
    }

    #[test]
    fn a_completion_arriving_after_revocation_is_not_published() {
        let mut f = fixture();
        accepted(&mut f, 1, b"abc");
        f.rx.on_message(chunk(1, 0, 0, b"abc"), &mut f.state);
        // Revocation reaches the state before the item's last message.
        f.state.revoke(&f.peer).unwrap();
        let out = f.rx.on_message(end(1, 0), &mut f.state);
        assert!(
            out.contains(&Output::Abort(ErrorCode::NotTrusted)),
            "{out:?}"
        );
        assert!(f.state.received().is_empty());
        assert_eq!(items(&f, 1), [ItemState::Cancelled(CancelCause::Revoked)]);
    }

    #[test]
    fn an_item_published_before_revocation_stays_published() {
        let mut f = fixture();
        let fields = Offer::for_files(id(1), &[("a", b"one"), ("b", b"two")])
            .unwrap()
            .fields();
        f.rx.on_message(Message::Offer(fields), &mut f.state);
        f.rx.on_answer(id(1), true, &mut f.state);
        f.rx.on_message(chunk(1, 0, 0, b"one"), &mut f.state);
        f.rx.on_message(end(1, 0), &mut f.state);
        f.state.revoke(&f.peer).unwrap();
        f.rx.on_wake(&mut f.state);
        assert_eq!(
            items(&f, 1),
            [
                ItemState::Published,
                ItemState::Cancelled(CancelCause::Revoked)
            ]
        );
        assert_eq!(f.state.received().len(), 1);
    }

    #[test]
    fn offers_beyond_staging_room_or_capacity_are_refused_before_consent() {
        let mut f = fixture();
        let huge = Offer::validate(crate::offer::OfferFields {
            transfer: id(1),
            kind: PayloadKind::Files.code(),
            items: vec![(b"f".to_vec(), crate::limits::MAX_STAGED_BYTES + 1, [0; 32])],
            preview: Vec::new(),
        })
        .unwrap();
        let out = f.rx.on_message(Message::Offer(huge.fields()), &mut f.state);
        assert_eq!(out, [Output::Send(reject(id(1), RejectReason::TooLarge))]);

        let budget = Budget::new(Limits {
            nonterminal_transfers: 0,
            ..Limits::COMPILED
        });
        let session = budget.admit_session().unwrap();
        let authority = f.state.trust.authorize(&f.peer).unwrap();
        let mut busy = Receiver::new(authority, budget, session);
        let out = busy.on_message(file_offer(2, b"x"), &mut f.state);
        assert_eq!(out, [Output::Send(reject(id(2), RejectReason::Busy))]);
    }

    #[test]
    fn staged_bytes_count_against_the_buffer_budget_until_the_transfer_ends() {
        let mut f = fixture();
        accepted(&mut f, 1, b"abcdef");
        assert_eq!(f.budget.usage().buffer_bytes, 6);
        f.rx.on_message(chunk(1, 0, 0, b"abcdef"), &mut f.state);
        f.rx.on_message(end(1, 0), &mut f.state);
        assert_eq!(f.budget.usage().buffer_bytes, 0);
    }

    #[test]
    fn an_offer_that_leaves_no_room_to_send_and_read_is_refused_before_consent() {
        let mut f = fixture();
        let budget = Budget::new(Limits {
            buffer_bytes: MIN_BUFFER_BYTES + 60 * 1024 - 1,
            ..Limits::COMPILED
        });
        let session = budget.admit_session().unwrap();
        let authority = f.state.trust.authorize(&f.peer).unwrap();
        let mut small = Receiver::new(authority, budget, session);
        let out = small.on_message(file_offer(1, &[0; 60 * 1024]), &mut f.state);
        assert_eq!(out, [Output::Send(reject(id(1), RejectReason::TooLarge))]);
        assert_eq!(f.state.held(), Room::default());
    }

    #[test]
    fn an_offer_larger_than_the_free_buffer_budget_is_refused_as_busy() {
        let mut f = fixture();
        let _hog = f.budget.lease(f.budget.limits().buffer_bytes - 2).unwrap();
        f.rx.on_message(file_offer(1, b"abc"), &mut f.state);
        let out = f.rx.on_answer(id(1), true, &mut f.state);
        assert_eq!(out, [Output::Send(reject(id(1), RejectReason::Busy))]);
        assert_eq!(f.state.held_bytes(), 0);
    }

    #[test]
    fn a_dropped_connection_fails_unfinished_work_and_frees_everything() {
        let mut f = fixture();
        accepted(&mut f, 1, b"abcdef");
        f.rx.on_message(chunk(1, 0, 0, b"abc"), &mut f.state);
        f.rx.on_disconnect(&mut f.state);
        assert_eq!(items(&f, 1), [ItemState::Failed(FailCause::Connection)]);
        assert_eq!(f.state.held_bytes(), 0);
        drop(f.rx);
        assert_eq!(f.budget.usage(), Usage::default());
    }

    #[test]
    fn a_cancel_requested_before_the_last_message_wins_over_publication() {
        let mut f = fixture();
        accepted(&mut f, 1, b"abc");
        f.rx.on_message(chunk(1, 0, 0, b"abc"), &mut f.state);
        // The person cancels; the item's last message arrives before the
        // session notices the request.
        assert!(f.state.request_cancel(id(1)));
        assert!(f.rx.should_stop(&f.state));
        let out = f.rx.on_message(end(1, 0), &mut f.state);
        assert_eq!(
            out,
            [
                Output::Send(Message::Cancel {
                    transfer: id(1),
                    reason: CancelReason::Receiver
                }),
                Output::End
            ]
        );
        assert!(f.state.received().is_empty());
        assert_eq!(items(&f, 1), [ItemState::Cancelled(CancelCause::Receiver)]);
        assert_eq!(f.state.held(), Room::default());
    }

    #[test]
    fn empty_items_count_against_the_held_item_limit_until_taken() {
        let mut f = fixture();
        let names: Vec<String> = (0..crate::limits::MAX_BATCH_ITEMS)
            .map(|n| format!("e{n}"))
            .collect();
        let empties: Vec<(&str, &[u8])> = names.iter().map(|n| (n.as_str(), &b""[..])).collect();
        let offer = |n: u8| Message::Offer(Offer::for_files(id(n), &empties).unwrap().fields());
        let batches = crate::limits::MAX_HELD_ITEMS / crate::limits::MAX_BATCH_ITEMS;
        for n in 1..=batches as u8 {
            f.rx.on_message(offer(n), &mut f.state);
            f.rx.on_answer(id(n), true, &mut f.state);
            for item in 0..empties.len() as u16 {
                f.rx.on_message(end(n, item), &mut f.state);
            }
        }
        assert_eq!(f.state.received().len(), crate::limits::MAX_HELD_ITEMS);
        let next = batches as u8 + 1;
        let out = f.rx.on_message(offer(next), &mut f.state);
        assert_eq!(
            out,
            [Output::Send(reject(id(next), RejectReason::TooLarge))]
        );

        f.state.take_received();
        let out = f.rx.on_message(offer(next + 1), &mut f.state);
        assert!(matches!(out.as_slice(), [Output::Ask { .. }]), "{out:?}");
    }

    #[test]
    fn the_sender_can_withdraw_an_offer_before_the_answer() {
        let mut f = fixture();
        f.rx.on_message(file_offer(1, b"x"), &mut f.state);
        let out = f.rx.on_message(
            Message::Cancel {
                transfer: id(1),
                reason: CancelReason::Sender,
            },
            &mut f.state,
        );
        assert_eq!(out, [Output::End]);
        assert_eq!(
            f.state.transfer(&id(1)).unwrap().offer_state(),
            crate::transfer::OfferState::Rejected(RejectCause::Withdrawn)
        );
        // A late answer for the withdrawn offer changes nothing.
        assert!(f.rx.on_answer(id(1), true, &mut f.state).is_empty());
    }
}
