//! The sending side of an authenticated session, as a state machine.
//!
//! ```text
//! Offering ──OfferAccept (same offer digest)──▶ Sending item ──ItemEnd──▶ Awaiting result
//!     │                                             ▲                         │
//!     └─OfferReject──▶ Done                          └──────next item──────────┘
//!                                                                     last item ▼
//!                                                                             Done
//! ```
//!
//! Before every chunk the sender checks that trust in the receiver is
//! unchanged and that nobody asked to cancel. A message that does not fit
//! the current step, or names another transfer, item, or offer, ends the
//! session with an error and fails the unfinished items.

use crate::ids::TransferId;
use crate::limits::{ActiveSlot, Budget, MAX_CHUNK_BYTES, SessionSlot, TransferSlot};
use crate::offer::Offer;
use crate::protocol::{CancelReason, ErrorCode, ItemResult, Message, RejectReason};
use crate::state::{State, StateError};
use crate::transfer::{CancelCause, FailCause, ItemEvent, RejectCause, Transfer};
use crate::trust::Authority;

#[derive(Debug, PartialEq, Eq)]
pub enum Output {
    Send(Message),
    Abort(ErrorCode),
    End,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Offering,
    Sending { item: usize, offset: usize },
    Awaiting { item: usize },
    Done,
}

pub struct Sender {
    offer: Offer,
    payload: Vec<Vec<u8>>,
    chunk: usize,
    authority: Authority,
    budget: Budget,
    session: SessionSlot,
    _slot: TransferSlot,
    active: Option<ActiveSlot>,
    step: Step,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SendError {
    #[error("the payload does not match the offer")]
    PayloadMismatch,
    #[error(transparent)]
    State(#[from] StateError),
    #[error(transparent)]
    Budget(#[from] crate::limits::Refusal),
}

impl Sender {
    /// Prepares a transfer of `payload`, one entry per offered item, and
    /// records it in `state`. `chunk` is capped at the protocol maximum.
    pub fn new(
        offer: Offer,
        payload: Vec<Vec<u8>>,
        chunk: usize,
        authority: Authority,
        budget: Budget,
        session: SessionSlot,
        state: &mut State,
    ) -> Result<Self, SendError> {
        let sizes_match = payload.len() == offer.items().len()
            && payload
                .iter()
                .zip(offer.items())
                .all(|(p, i)| p.len() as u64 == i.size);
        if !sizes_match {
            return Err(SendError::PayloadMismatch);
        }
        let slot = budget.admit_transfer()?;
        let transfer = Transfer::offered(offer.transfer(), authority, offer.items().len())
            .map_err(|_| SendError::PayloadMismatch)?;
        state.insert_transfer(transfer)?;
        Ok(Self {
            offer,
            payload,
            chunk: chunk.clamp(1, MAX_CHUNK_BYTES),
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
            Step::Sending { item, offset } => self.payload[item]
                .len()
                .saturating_sub(offset)
                .min(self.chunk),
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

    /// The next frame to send while sending, after checking trust and
    /// cancellation.
    pub fn next_frame(&mut self, state: &mut State) -> Option<Output> {
        let Step::Sending { item, offset } = self.step else {
            return None;
        };
        if let Some(stop) = self.stop_if_needed(state) {
            return Some(stop);
        }
        let id = self.offer.transfer();
        let bytes = &self.payload[item];
        if offset == 0
            && let Some(t) = state.transfer_mut(&id)
            && t.apply(item, ItemEvent::Start).is_err()
        {
            return Some(self.abort(state, FailCause::Protocol, ErrorCode::Internal));
        }
        if offset < bytes.len() {
            let end = (offset + self.chunk).min(bytes.len());
            self.step = Step::Sending { item, offset: end };
            return Some(Output::Send(Message::Chunk {
                transfer: id,
                item: item as u16,
                offset: offset as u64,
                data: bytes[offset..end].to_vec(),
            }));
        }
        if let Some(t) = state.transfer_mut(&id) {
            let _ = t.apply(item, ItemEvent::BytesComplete);
        }
        self.step = Step::Awaiting { item };
        Some(Output::Send(Message::ItemEnd {
            transfer: id,
            item: item as u16,
        }))
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
                if state.accept(&id).is_err() {
                    return self.finish_cancelled(state, CancelReason::Revoked);
                }
                match self.budget.activate(&self.session, &self._slot) {
                    Ok(a) => self.active = Some(a),
                    Err(_) => return vec![self.abort(state, FailCause::Protocol, ErrorCode::Busy)],
                }
                self.step = Step::Sending { item: 0, offset: 0 };
                Vec::new()
            }
            (Step::Offering, Message::OfferReject { transfer, reason }) if transfer == id => {
                if let Some(t) = state.transfer_mut(&id) {
                    t.reject(match reason {
                        RejectReason::Declined => RejectCause::Receiver,
                        RejectReason::TooLarge | RejectReason::Malformed => RejectCause::TooLarge,
                        RejectReason::Busy => RejectCause::Busy,
                        RejectReason::NotTrusted => RejectCause::Revoked,
                    });
                }
                self.done(state);
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
                if let Some(t) = state.transfer_mut(&id) {
                    let _ = match result {
                        ItemResult::Published => t.confirm_published(item).map(|_| ()),
                        ItemResult::DigestMismatch => t
                            .apply(item, ItemEvent::Fail(FailCause::Digest))
                            .map(|_| ()),
                        ItemResult::LengthMismatch => t
                            .apply(item, ItemEvent::Fail(FailCause::Length))
                            .map(|_| ()),
                        ItemResult::Cancelled => t
                            .apply(item, ItemEvent::Cancel(CancelCause::Receiver))
                            .map(|_| ()),
                        ItemResult::Unauthorized => t
                            .apply(item, ItemEvent::Cancel(CancelCause::Revoked))
                            .map(|_| ()),
                    };
                }
                if item + 1 < self.payload.len() && result != ItemResult::Unauthorized {
                    self.step = Step::Sending {
                        item: item + 1,
                        offset: 0,
                    };
                    return Vec::new();
                }
                if result == ItemResult::Unauthorized
                    && let Some(t) = state.transfer_mut(&id)
                {
                    t.cancel(CancelCause::Revoked);
                }
                self.done(state);
                vec![Output::End]
            }
            (
                Step::Sending { .. } | Step::Awaiting { .. } | Step::Offering,
                Message::Cancel { transfer, reason },
            ) if transfer == id => {
                if let Some(t) = state.transfer_mut(&id) {
                    t.cancel(match reason {
                        CancelReason::Revoked => CancelCause::Revoked,
                        CancelReason::Shutdown => CancelCause::Shutdown,
                        CancelReason::Receiver | CancelReason::Sender => CancelCause::Receiver,
                    });
                }
                self.done(state);
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
            Step::Sending { .. } | Step::Done => Vec::new(),
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

    /// The connection ended. Unfinished items fail.
    pub fn on_disconnect(&mut self, state: &mut State) {
        if !self.is_done() {
            self.fail(state, FailCause::Connection);
        }
    }

    fn stop_if_needed(&mut self, state: &mut State) -> Option<Output> {
        let id = self.offer.transfer();
        if state.trust().check(&self.authority).is_err() {
            if let Some(t) = state.transfer_mut(&id) {
                t.revoke();
            }
            self.done(state);
            return Some(Output::Send(Message::Cancel {
                transfer: id,
                reason: CancelReason::Revoked,
            }));
        }
        if state.cancel_requested(&id) {
            if let Some(t) = state.transfer_mut(&id) {
                t.cancel(CancelCause::Sender);
            }
            self.done(state);
            return Some(Output::Send(Message::Cancel {
                transfer: id,
                reason: CancelReason::Sender,
            }));
        }
        None
    }

    fn finish_cancelled(&mut self, state: &mut State, reason: CancelReason) -> Vec<Output> {
        self.done(state);
        vec![
            Output::Send(Message::Cancel {
                transfer: self.offer.transfer(),
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
        if let Some(t) = state.transfer_mut(&self.offer.transfer()) {
            t.fail_rest(cause);
        }
        self.done(state);
    }

    fn done(&mut self, state: &mut State) {
        self.step = Step::Done;
        self.active = None;
        state.mark_finished(self.offer.transfer());
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
        let mut state = State::new();
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
        let payload = content.iter().map(|c| c.to_vec()).collect();
        let sender = Sender::new(
            offer,
            payload,
            2,
            authority,
            budget.clone(),
            session,
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

    fn drain(sender: &mut Sender, state: &mut State) -> Vec<Message> {
        let mut sent = Vec::new();
        while let Some(Output::Send(m)) = sender.next_frame(state) {
            let last = matches!(m, Message::ItemEnd { .. });
            sent.push(m);
            if last {
                break;
            }
        }
        sent
    }

    #[test]
    fn the_sender_streams_each_item_in_order_and_records_the_receivers_results() {
        let (mut state, budget, mut sender, _) = setup(&[b"abcde", b""]);
        accept(&mut sender, &mut state);
        let first = drain(&mut sender, &mut state);
        let offsets: Vec<u64> = first
            .iter()
            .filter_map(|m| match m {
                Message::Chunk { offset, .. } => Some(*offset),
                _ => None,
            })
            .collect();
        assert_eq!(offsets, [0, 2, 4]);
        sender.on_message(
            Message::ItemResult {
                transfer: ID,
                item: 0,
                result: ItemResult::Published,
            },
            &mut state,
        );
        // The empty item has no chunks, only its end.
        assert_eq!(
            drain(&mut sender, &mut state),
            [Message::ItemEnd {
                transfer: ID,
                item: 1
            }]
        );
        let out = sender.on_message(
            Message::ItemResult {
                transfer: ID,
                item: 1,
                result: ItemResult::Published,
            },
            &mut state,
        );
        assert_eq!(out, [Output::End]);
        assert_eq!(
            state.transfer(&ID).unwrap().outcome(),
            BatchOutcome::Completed
        );
        drop(sender);
        assert_eq!(budget.usage(), Usage::default());
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
        let early = sender.on_message(
            Message::ItemResult {
                transfer: ID,
                item: 0,
                result: ItemResult::Published,
            },
            &mut state,
        );
        assert_eq!(early, [Output::Abort(ErrorCode::UnexpectedMessage)]);
        assert_eq!(
            state.transfer(&ID).unwrap().items(),
            [ItemState::Rejected, ItemState::Rejected]
        );

        let (mut state, _, mut sender, _) = setup(&[b"abc", b"de"]);
        accept(&mut sender, &mut state);
        drain(&mut sender, &mut state);
        let wrong = sender.on_message(
            Message::ItemResult {
                transfer: ID,
                item: 1,
                result: ItemResult::Published,
            },
            &mut state,
        );
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
            Some(Output::Send(Message::Chunk { .. }))
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
        let (mut state, _, mut sender, _) = setup(&[b"abc"]);
        let out = sender.on_message(
            Message::OfferReject {
                transfer: ID,
                reason: RejectReason::Declined,
            },
            &mut state,
        );
        assert_eq!(out, [Output::End]);
        assert_eq!(
            state.transfer(&ID).unwrap().offer_state(),
            crate::transfer::OfferState::Rejected(RejectCause::Receiver)
        );
    }

    #[test]
    fn a_payload_that_differs_from_its_offer_is_refused() {
        let peer = DeviceKey::from_seed(&Zeroizing::new([9; 32])).id();
        let mut state = State::new();
        let authority = state
            .trust
            .trust(
                VerifiedPeer::new(peer, PeerText::default()),
                LocalAlias::new("rx").unwrap(),
            )
            .unwrap();
        let budget = Budget::new(Limits::COMPILED);
        let offer = Offer::for_files(ID, &[("f", b"abc")]).unwrap();
        let session = budget.admit_session().unwrap();
        let result = Sender::new(
            offer,
            vec![b"abcd".to_vec()],
            2,
            authority,
            budget,
            session,
            &mut state,
        );
        assert_eq!(result.err(), Some(SendError::PayloadMismatch));
    }
}
