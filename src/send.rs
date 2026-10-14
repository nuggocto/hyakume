//! The sending side of a session, as a state machine. It does no I/O; the
//! runtime reads and hashes the bytes it names, so a source that changed
//! fails its item instead of sending different bytes. Before every chunk it
//! checks trust and cancellation, and a message that does not fit the step
//! ends the session.

use crate::ids::TransferId;
use crate::limits::{ActiveSlot, Budget, MAX_CHUNK_BYTES, SessionSlot, TransferSlot};
use crate::offer::Offer;
use crate::protocol::{CancelReason, ErrorCode, ItemResult, Message, RejectReason};
use crate::state::{State, StateError, TransferInfo};
use crate::transfer::{CancelCause, FailCause, ItemEvent, PublishError, RejectCause, Transfer};
use crate::trust::Authority;

#[derive(Debug, PartialEq, Eq)]
pub enum Output {
    Send(Message),
    /// Read `len` bytes at `offset` of `item`'s source and send them in
    /// [`Sender::chunk`].
    Read {
        item: usize,
        offset: u64,
        len: usize,
    },
    /// Every byte of `item` was read. Check that its source still matches
    /// the offer, then call [`Sender::end_item`].
    Check {
        item: usize,
    },
    Abort(ErrorCode),
    End,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Offering,
    Sending { item: usize, offset: u64 },
    Checking { item: usize },
    Awaiting { item: usize },
    Done,
}

pub struct Sender {
    offer: Offer,
    chunk: u64,
    authority: Authority,
    budget: Budget,
    session: SessionSlot,
    _slot: TransferSlot,
    active: Option<ActiveSlot>,
    step: Step,
}

#[derive(Debug, thiserror::Error)]
pub enum SendError {
    #[error(transparent)]
    State(#[from] StateError),
    #[error(transparent)]
    Budget(#[from] crate::limits::Refusal),
    #[error("the offer could not be recorded")]
    Offer,
}

impl Sender {
    /// Prepares a transfer of `offer` and records it in `state`. `chunk` is
    /// capped at the protocol maximum.
    pub fn new(
        offer: Offer,
        chunk: usize,
        authority: Authority,
        budget: Budget,
        session: SessionSlot,
        info: TransferInfo,
        state: &mut State,
    ) -> Result<Self, SendError> {
        let slot = budget.admit_transfer()?;
        let transfer = Transfer::offered(offer.transfer(), authority, offer.items().len())
            .map_err(|_| SendError::Offer)?;
        state.insert_transfer(transfer, info)?;
        Ok(Self {
            offer,
            chunk: chunk.clamp(1, MAX_CHUNK_BYTES) as u64,
            authority,
            budget,
            session,
            _slot: slot,
            active: None,
            step: Step::Offering,
        })
    }

    pub fn transfer(&self) -> TransferId {
        self.offer.transfer()
    }

    /// The first message: the offer.
    pub fn offer_message(&self) -> Message {
        Message::Offer(self.offer.fields())
    }

    /// The digest the offer declares for `item`.
    pub fn item_digest(&self, item: usize) -> Option<[u8; 32]> {
        self.offer.items().get(item).map(|i| i.digest)
    }

    pub fn is_done(&self) -> bool {
        self.step == Step::Done
    }

    /// Whether the sender has bytes to send rather than a reply to wait for.
    pub fn is_sending(&self) -> bool {
        matches!(self.step, Step::Sending { .. })
    }

    /// Payload bytes in the next frame: a chunk's length, or zero.
    pub fn next_chunk_len(&self) -> usize {
        match self.step {
            Step::Sending { item, offset } => {
                let left = self.offer.items()[item].size.saturating_sub(offset);
                // At most `chunk`, which fits in usize.
                left.min(self.chunk) as usize
            }
            _ => 0,
        }
    }

    /// Whether [`Sender::check_stop`] would stop the transfer now. The
    /// runtime asks this to interrupt a write that the peer is not reading.
    pub fn should_stop(&self, state: &State) -> bool {
        !self.is_done()
            && (state.trust().check(&self.authority).is_err()
                || state.cancel_requested(&self.offer.transfer()))
    }

    /// What to do next while sending, after checking trust and
    /// cancellation: read a chunk, or check the finished item.
    pub fn next_frame(&mut self, state: &mut State) -> Option<Output> {
        let Step::Sending { item, offset } = self.step else {
            return None;
        };
        if let Some(stop) = self.stop_if_needed(state) {
            return Some(stop);
        }
        let id = self.offer.transfer();
        let size = self.offer.items()[item].size;
        if offset == 0 {
            let started = state.update_transfer(&id, |t| t.apply(item, ItemEvent::Start).is_ok());
            if started != Some(true) {
                return Some(self.abort(state, FailCause::Protocol, ErrorCode::Internal));
            }
        }
        if offset < size {
            let len = self.next_chunk_len();
            self.step = Step::Sending {
                item,
                offset: offset + len as u64,
            };
            return Some(Output::Read { item, offset, len });
        }
        state.update_transfer(&id, |t| t.apply(item, ItemEvent::BytesComplete));
        self.step = Step::Checking { item };
        Some(Output::Check { item })
    }

    /// The chunk message for bytes read at `offset` of `item`.
    pub fn chunk(&self, item: usize, offset: u64, data: Vec<u8>) -> Message {
        Message::Chunk {
            transfer: self.offer.transfer(),
            // Bounded by MAX_BATCH_ITEMS when the offer was validated.
            item: item as u16,
            offset,
            data,
        }
    }

    /// The runtime checked the item's source. If it still matches the
    /// offer, the item ends; otherwise it fails with `failure` and the rest
    /// of the transfer is cancelled, so different bytes never complete
    /// under this transfer.
    pub fn end_item(&mut self, failure: Option<FailCause>, state: &mut State) -> Vec<Output> {
        let Step::Checking { item } = self.step else {
            return Vec::new();
        };
        match failure {
            None => {
                self.step = Step::Awaiting { item };
                vec![Output::Send(Message::ItemEnd {
                    transfer: self.offer.transfer(),
                    item: item as u16,
                })]
            }
            Some(cause) => self.source_failed(item, cause, state),
        }
    }

    /// Reading the current item's source failed.
    pub fn on_read_failure(&mut self, cause: FailCause, state: &mut State) -> Vec<Output> {
        match self.step {
            Step::Sending { item, .. } | Step::Checking { item } => {
                self.source_failed(item, cause, state)
            }
            _ => Vec::new(),
        }
    }

    fn source_failed(&mut self, item: usize, cause: FailCause, state: &mut State) -> Vec<Output> {
        let id = self.offer.transfer();
        state.update_transfer(&id, |t| {
            let _ = t.apply(item, ItemEvent::Fail(cause));
            t.cancel(CancelCause::Sender);
        });
        self.done();
        vec![
            Output::Send(Message::Cancel {
                transfer: id,
                reason: CancelReason::Sender,
            }),
            Output::End,
        ]
    }

    pub fn on_message(&mut self, msg: Message, state: &mut State) -> Vec<Output> {
        let id = self.offer.transfer();
        match (self.step, msg) {
            (Step::Done, _) => Vec::new(),
            (
                Step::Offering,
                Message::OfferAccept {
                    transfer,
                    offer_digest,
                },
            ) if transfer == id => {
                if offer_digest != self.offer.digest() {
                    return vec![self.abort(state, FailCause::Protocol, ErrorCode::Malformed)];
                }
                match state.accept(&id, None) {
                    Ok(()) => {}
                    Err(PublishError::Unauthorized(_)) => {
                        return self.finish_cancelled(state, CancelReason::Revoked);
                    }
                    Err(_) => {
                        return vec![self.abort(state, FailCause::Storage, ErrorCode::Internal)];
                    }
                }
                match self.budget.activate(&self.session, &self._slot) {
                    Ok(a) => self.active = Some(a),
                    Err(_) => return vec![self.abort(state, FailCause::Protocol, ErrorCode::Busy)],
                }
                self.step = Step::Sending { item: 0, offset: 0 };
                Vec::new()
            }
            (Step::Offering, Message::OfferReject { transfer, reason }) if transfer == id => {
                state.update_transfer(&id, |t| {
                    t.reject(match reason {
                        RejectReason::Declined => RejectCause::Receiver,
                        RejectReason::TooLarge => RejectCause::TooLarge,
                        RejectReason::Malformed => RejectCause::Invalid,
                        RejectReason::Busy => RejectCause::Busy,
                        RejectReason::NotTrusted => RejectCause::Revoked,
                    });
                });
                self.done();
                vec![Output::End]
            }
            (
                Step::Awaiting { item },
                Message::ItemResult {
                    transfer,
                    item: got,
                    result,
                },
            ) if transfer == id && usize::from(got) == item => {
                state.update_transfer(&id, |t| {
                    let _ = match result {
                        ItemResult::Published => t.confirm_published(item).map(|_| ()),
                        other => t.apply(item, result_event(other)).map(|_| ()),
                    };
                    if result == ItemResult::Unauthorized {
                        t.cancel(CancelCause::Revoked);
                    }
                });
                if item + 1 < self.offer.items().len() && result != ItemResult::Unauthorized {
                    self.step = Step::Sending {
                        item: item + 1,
                        offset: 0,
                    };
                    return Vec::new();
                }
                self.done();
                vec![Output::End]
            }
            (
                Step::Sending { .. } | Step::Awaiting { .. } | Step::Offering,
                Message::Cancel { transfer, reason },
            ) if transfer == id => {
                state.update_transfer(&id, |t| {
                    t.cancel(match reason {
                        CancelReason::Revoked => CancelCause::Revoked,
                        CancelReason::Shutdown => CancelCause::Shutdown,
                        CancelReason::Receiver | CancelReason::Sender => CancelCause::Receiver,
                    });
                });
                self.done();
                vec![Output::End]
            }
            (_, Message::Error(_)) => {
                self.fail(state, FailCause::Protocol);
                vec![Output::End]
            }
            _ => vec![self.abort(state, FailCause::Protocol, ErrorCode::UnexpectedMessage)],
        }
    }

    /// Something changed locally while waiting for the receiver.
    pub fn on_wake(&mut self, state: &mut State) -> Vec<Output> {
        match self.step {
            Step::Offering | Step::Awaiting { .. } => {
                self.stop_if_needed(state).into_iter().collect()
            }
            Step::Sending { .. } | Step::Checking { .. } | Step::Done => Vec::new(),
        }
    }

    /// Checks trust and cancellation at any step, such as while a send is
    /// paused waiting for permission.
    pub fn check_stop(&mut self, state: &mut State) -> Vec<Output> {
        if self.is_done() {
            return Vec::new();
        }
        self.stop_if_needed(state).into_iter().collect()
    }

    /// The connection ended. Unfinished items fail, or pause while the node
    /// stops.
    pub fn on_disconnect(&mut self, state: &mut State) {
        if !self.is_done() {
            state.interrupt(&self.offer.transfer(), FailCause::Connection);
            self.done();
        }
    }

    fn stop_if_needed(&mut self, state: &mut State) -> Option<Output> {
        let id = self.offer.transfer();
        if state.trust().check(&self.authority).is_err() {
            state.update_transfer(&id, Transfer::revoke);
            self.done();
            return Some(Output::Send(Message::Cancel {
                transfer: id,
                reason: CancelReason::Revoked,
            }));
        }
        if state.cancel_requested(&id) {
            state.update_transfer(&id, |t| t.cancel(CancelCause::Sender));
            self.done();
            return Some(Output::Send(Message::Cancel {
                transfer: id,
                reason: CancelReason::Sender,
            }));
        }
        None
    }

    fn finish_cancelled(&mut self, state: &mut State, reason: CancelReason) -> Vec<Output> {
        let id = self.offer.transfer();
        state.update_transfer(&id, |t| t.cancel(CancelCause::Revoked));
        self.done();
        vec![
            Output::Send(Message::Cancel {
                transfer: id,
                reason,
            }),
            Output::End,
        ]
    }

    fn abort(&mut self, state: &mut State, cause: FailCause, code: ErrorCode) -> Output {
        self.fail(state, cause);
        Output::Abort(code)
    }

    fn fail(&mut self, state: &mut State, cause: FailCause) {
        state.update_transfer(&self.offer.transfer(), |t| t.fail_rest(cause));
        self.done();
    }

    fn done(&mut self) {
        self.step = Step::Done;
        self.active = None;
    }
}

/// The item event a receiver's unsuccessful result stands for.
fn result_event(result: ItemResult) -> ItemEvent {
    match result {
        ItemResult::DigestMismatch => ItemEvent::Fail(FailCause::Digest),
        ItemResult::LengthMismatch => ItemEvent::Fail(FailCause::Length),
        ItemResult::ContentMismatch => ItemEvent::Fail(FailCause::Content),
        ItemResult::Exists => ItemEvent::Fail(FailCause::Exists),
        ItemResult::StorageFailed => ItemEvent::Fail(FailCause::Storage),
        ItemResult::Cancelled => ItemEvent::Cancel(CancelCause::Receiver),
        ItemResult::Unauthorized | ItemResult::Published => ItemEvent::Cancel(CancelCause::Revoked),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::PeerText;
    use crate::identity::{DeviceId, DeviceKey};
    use crate::limits::{Limits, Usage};
    use crate::transfer::{BatchOutcome, ItemState};
    use crate::trust::{LocalAlias, VerifiedPeer};
    use zeroize::Zeroizing;

    const ID: TransferId = TransferId::from_bytes([1; 16]);

    fn setup(content: &[&[u8]]) -> (State, Budget, Sender, DeviceId) {
        let peer = DeviceKey::from_seed(&Zeroizing::new([9; 32])).id();
        let mut state = State::in_memory().unwrap();
        let authority = state
            .trust
            .trust(
                VerifiedPeer::new(peer, PeerText::default()),
                LocalAlias::new("rx").unwrap(),
            )
            .unwrap();
        let budget = Budget::new(Limits::COMPILED);
        let names: Vec<String> = (0..content.len()).map(|n| format!("f{n}")).collect();
        let files: Vec<(&str, &[u8])> = names
            .iter()
            .map(|n| n.as_str())
            .zip(content.iter().copied())
            .collect();
        let offer = Offer::for_files(ID, &files).unwrap();
        let session = budget.admit_session().unwrap();
        let info = TransferInfo::new(true, offer.clone(), 0);
        let sender = Sender::new(
            offer,
            2,
            authority,
            budget.clone(),
            session,
            info,
            &mut state,
        )
        .unwrap();
        (state, budget, sender, peer)
    }

    fn accept(sender: &mut Sender, state: &mut State) {
        let digest = match sender.offer_message() {
            Message::Offer(fields) => Offer::validate(fields).unwrap().digest(),
            _ => unreachable!(),
        };
        let out = sender.on_message(
            Message::OfferAccept {
                transfer: ID,
                offer_digest: digest,
            },
            state,
        );
        assert!(out.is_empty(), "{out:?}");
    }

    /// Plays the runtime through one item with an unchanged source, and
    /// returns the reads it was asked for.
    fn drain(sender: &mut Sender, state: &mut State) -> Vec<(u64, usize)> {
        let mut reads = Vec::new();
        loop {
            match sender.next_frame(state) {
                Some(Output::Read { offset, len, .. }) => reads.push((offset, len)),
                Some(Output::Check { .. }) => {
                    let out = sender.end_item(None, state);
                    assert!(matches!(
                        out.as_slice(),
                        [Output::Send(Message::ItemEnd { .. })]
                    ));
                    return reads;
                }
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    fn item_result(item: u16, result: ItemResult) -> Message {
        Message::ItemResult {
            transfer: ID,
            item,
            result,
        }
    }

    #[test]
    fn the_sender_reads_each_item_in_order_and_records_the_receivers_results() {
        let (mut state, budget, mut sender, _) = setup(&[b"abcde", b""]);
        accept(&mut sender, &mut state);
        assert_eq!(drain(&mut sender, &mut state), [(0, 2), (2, 2), (4, 1)]);
        sender.on_message(item_result(0, ItemResult::Published), &mut state);
        // The empty item has no chunks, only its end.
        assert!(drain(&mut sender, &mut state).is_empty());
        let out = sender.on_message(item_result(1, ItemResult::Published), &mut state);
        assert_eq!(out, [Output::End]);
        assert_eq!(
            state.transfer(&ID).unwrap().outcome(),
            BatchOutcome::Completed
        );
        drop(sender);
        assert_eq!(budget.usage(), Usage::default());
    }

    #[test]
    fn a_source_that_changed_fails_its_item_and_cancels_the_rest() {
        let (mut state, _, mut sender, _) = setup(&[b"abc", b"de"]);
        accept(&mut sender, &mut state);
        assert!(matches!(
            sender.next_frame(&mut state),
            Some(Output::Read { .. })
        ));
        assert_eq!(
            sender.next_frame(&mut state),
            Some(Output::Read {
                item: 0,
                offset: 2,
                len: 1
            })
        );
        assert_eq!(
            sender.next_frame(&mut state),
            Some(Output::Check { item: 0 })
        );
        let out = sender.end_item(Some(FailCause::SourceChanged), &mut state);
        assert_eq!(
            out,
            [
                Output::Send(Message::Cancel {
                    transfer: ID,
                    reason: CancelReason::Sender
                }),
                Output::End
            ]
        );
        assert_eq!(
            state.transfer(&ID).unwrap().items(),
            [
                ItemState::Failed(FailCause::SourceChanged),
                ItemState::Cancelled(CancelCause::Sender)
            ]
        );
    }

    #[test]
    fn the_receivers_reason_for_a_failed_item_is_kept() {
        for (result, cause) in [
            (ItemResult::DigestMismatch, FailCause::Digest),
            (ItemResult::LengthMismatch, FailCause::Length),
            (ItemResult::ContentMismatch, FailCause::Content),
            (ItemResult::Exists, FailCause::Exists),
            (ItemResult::StorageFailed, FailCause::Storage),
        ] {
            let (mut state, _, mut sender, _) = setup(&[b"abc"]);
            accept(&mut sender, &mut state);
            drain(&mut sender, &mut state);
            sender.on_message(item_result(0, result), &mut state);
            assert_eq!(
                state.transfer(&ID).unwrap().items(),
                [ItemState::Failed(cause)]
            );
        }
    }

    #[test]
    fn an_acceptance_naming_another_offer_is_a_protocol_error() {
        let (mut state, _, mut sender, _) = setup(&[b"abc"]);
        let out = sender.on_message(
            Message::OfferAccept {
                transfer: ID,
                offer_digest: [0; 32],
            },
            &mut state,
        );
        assert_eq!(out, [Output::Abort(ErrorCode::Malformed)]);
        assert!(sender.is_done());
        assert!(sender.next_frame(&mut state).is_none());
    }

    #[test]
    fn a_result_for_the_wrong_item_or_out_of_turn_is_refused() {
        let (mut state, _, mut sender, _) = setup(&[b"abc", b"de"]);
        let early = sender.on_message(item_result(0, ItemResult::Published), &mut state);
        assert_eq!(early, [Output::Abort(ErrorCode::UnexpectedMessage)]);
        assert_eq!(
            state.transfer(&ID).unwrap().items(),
            [ItemState::Rejected, ItemState::Rejected]
        );

        let (mut state, _, mut sender, _) = setup(&[b"abc", b"de"]);
        accept(&mut sender, &mut state);
        drain(&mut sender, &mut state);
        let wrong = sender.on_message(item_result(1, ItemResult::Published), &mut state);
        assert_eq!(wrong, [Output::Abort(ErrorCode::UnexpectedMessage)]);
        assert_eq!(
            state.transfer(&ID).unwrap().items(),
            [
                ItemState::Failed(FailCause::Protocol),
                ItemState::Failed(FailCause::Protocol)
            ]
        );
    }

    #[test]
    fn revoking_the_receiver_stops_the_next_chunk() {
        let (mut state, _, mut sender, peer) = setup(&[b"abcdef"]);
        accept(&mut sender, &mut state);
        assert!(matches!(
            sender.next_frame(&mut state),
            Some(Output::Read { .. })
        ));
        assert!(!sender.should_stop(&state));
        state.revoke(&peer).unwrap();
        // A write already under way is interrupted on this answer.
        assert!(sender.should_stop(&state));
        assert_eq!(
            sender.next_frame(&mut state),
            Some(Output::Send(Message::Cancel {
                transfer: ID,
                reason: CancelReason::Revoked
            }))
        );
        assert_eq!(
            state.transfer(&ID).unwrap().items(),
            [ItemState::Cancelled(CancelCause::Revoked)]
        );
        assert!(sender.next_frame(&mut state).is_none());
    }

    #[test]
    fn a_rejection_ends_the_transfer_with_the_receivers_reason() {
        for (reason, cause) in [
            (RejectReason::Declined, RejectCause::Receiver),
            (RejectReason::Malformed, RejectCause::Invalid),
        ] {
            let (mut state, _, mut sender, _) = setup(&[b"abc"]);
            let out = sender.on_message(
                Message::OfferReject {
                    transfer: ID,
                    reason,
                },
                &mut state,
            );
            assert_eq!(out, [Output::End]);
            assert_eq!(
                state.transfer(&ID).unwrap().offer_state(),
                crate::transfer::OfferState::Rejected(cause)
            );
        }
    }
}
