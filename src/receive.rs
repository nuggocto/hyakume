//! The receiving side of a session, as a state machine. It does no file or
//! network I/O; the runtime does what it returns, so every duplicate,
//! reordered, or oversized message has a direct test. Bytes stay in a
//! private partial file until length and digest match the offer, and the
//! rename happens under the lock that orders revocation.

use crate::consent::ConsentView;
use crate::files::destination::PublishFailure;
use crate::files::names::{OutputName, partial_name};
use crate::identity::DeviceId;
use crate::ids::TransferId;
use crate::limits::{
    ActiveSlot, Budget, MAX_PARTIAL_TOTAL, MIN_FREE_BYTES, SessionSlot, TransferSlot,
};
use crate::offer::{Offer, OfferError, PayloadKind};
use crate::protocol::{CancelReason, ErrorCode, ItemResult, Message, RejectReason};
use crate::state::{PartialFile, Place, State, TransferInfo};
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
    /// Write `data` at `offset` of the item's partial file, creating the
    /// file first if it does not exist.
    Write {
        item: usize,
        offset: u64,
        data: Vec<u8>,
    },
    /// Every byte of the item arrived. Sync its partial file (creating it
    /// if the item is empty), then call [`Receiver::on_verified`].
    Verify {
        item: usize,
    },
    /// Send an error code, then end the session.
    Abort(ErrorCode),
    /// End the session after the messages before this one.
    End,
}

/// Where accepted items go, and how much room is there now.
pub struct Target<'a> {
    pub place: &'a Place,
    pub free_bytes: u64,
}

/// What the runtime saw in one item's bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Content {
    pub digest: [u8; 32],
    /// The bytes are UTF-8 and start with the offer's preview. Checked
    /// only for text and links.
    pub text_ok: bool,
}

/// Follows an item's bytes as they are written, so the receiver can check
/// them without reading the file back.
pub struct ContentCheck {
    hasher: blake3::Hasher,
    text: bool,
    preview: Vec<u8>,
    matched: usize,
    prefix_ok: bool,
    /// An incomplete UTF-8 sequence at the end of the bytes so far.
    carry: Vec<u8>,
    utf8_ok: bool,
}

impl ContentCheck {
    pub fn new(offer: &Offer) -> Self {
        Self {
            hasher: blake3::Hasher::new(),
            text: offer.kind() != PayloadKind::Files,
            preview: offer.preview().as_bytes().to_vec(),
            matched: 0,
            prefix_ok: true,
            carry: Vec::new(),
            utf8_ok: true,
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.hasher.update(data);
        if !self.text {
            return;
        }
        if self.matched < self.preview.len() {
            let n = (self.preview.len() - self.matched).min(data.len());
            if data[..n] != self.preview[self.matched..self.matched + n] {
                self.prefix_ok = false;
            }
            self.matched += n;
        }
        if !self.utf8_ok {
            return;
        }
        if !self.carry.is_empty() {
            let width = utf8_width(self.carry[0]);
            let need = width.saturating_sub(self.carry.len()).min(data.len());
            self.carry.extend_from_slice(&data[..need]);
            data = &data[need..];
            if self.carry.len() < width {
                return;
            }
            if std::str::from_utf8(&self.carry).is_err() {
                self.utf8_ok = false;
                return;
            }
            self.carry.clear();
        }
        match std::str::from_utf8(data) {
            Ok(_) => {}
            Err(e) if e.error_len().is_none() => self.carry = data[e.valid_up_to()..].to_vec(),
            Err(_) => self.utf8_ok = false,
        }
    }

    pub fn finish(self) -> Content {
        Content {
            digest: *self.hasher.finalize().as_bytes(),
            text_ok: !self.text
                || (self.utf8_ok
                    && self.carry.is_empty()
                    && self.prefix_ok
                    && self.matched == self.preview.len()),
        }
    }
}

/// Bytes in the UTF-8 sequence that starts with `lead`; 1 for a byte that
/// cannot start one, which then fails validation.
fn utf8_width(lead: u8) -> usize {
    match lead {
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => 1,
    }
}

struct Pending {
    offer: Offer,
    outputs: Vec<OutputName>,
    _slot: TransferSlot,
}

struct Active {
    offer: Offer,
    outputs: Vec<OutputName>,
    slot: TransferSlot,
    _active: ActiveSlot,
    item: usize,
    /// Bytes of the current item received so far.
    received: u64,
    /// The current item is complete and waits for [`Receiver::on_verified`].
    verifying: bool,
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

    /// The offer being received, if any.
    pub fn receiving(&self) -> Option<&Offer> {
        match &self.step {
            Step::Receiving(a) => Some(&a.offer),
            _ => None,
        }
    }

    /// Handles one decoded message. `target` is where an offer would be
    /// received; the runtime supplies it for offers.
    pub fn on_message(
        &mut self,
        msg: Message,
        target: Option<&Target<'_>>,
        state: &mut State,
    ) -> Vec<Output> {
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
            (Step::Idle, Message::Offer(fields)) => self.on_offer(fields, target, state),
            (Step::Deciding(p), Message::Cancel { transfer, .. })
                if transfer == p.offer.transfer() =>
            {
                state.update_transfer(&transfer, |t| t.cancel(CancelCause::Sender));
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
            ) if transfer == a.offer.transfer() && !a.verifying => {
                self.on_chunk(a, item, offset, data, state)
            }
            (Step::Receiving(a), Message::ItemEnd { transfer, item })
                if transfer == a.offer.transfer() && !a.verifying =>
            {
                self.on_item_end(a, item, state)
            }
            (Step::Receiving(a), Message::Cancel { transfer, .. })
                if transfer == a.offer.transfer() =>
            {
                state.update_transfer(&transfer, |t| t.cancel(CancelCause::Sender));
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
        target: Option<&Target<'_>>,
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
            return self.decline(id, RejectCause::Receiver, RejectReason::Declined, state);
        }

        let Some(target) = target else {
            return self.decline(id, RejectCause::Busy, RejectReason::Busy, state);
        };
        if let Err((cause, reason)) = room_for(&pending.offer, target, state) {
            return self.decline(id, cause, reason, state);
        }
        let Ok(active) = self.budget.activate(&self.session, &pending._slot) else {
            return self.decline(id, RejectCause::Busy, RejectReason::Busy, state);
        };

        let partials: Vec<PartialFile> = pending
            .outputs
            .iter()
            .zip(pending.offer.items())
            .enumerate()
            .map(|(n, (output, item))| PartialFile {
                item: n,
                output: output.as_bytes().to_vec(),
                partial: partial_name(id, n).into_bytes(),
                bytes: item.size,
            })
            .collect();

        // Accepting checks current trust and saves the acceptance, with the
        // destination and reserved partial files, before the sender hears
        // of it.
        match state.accept(&id, Some((target.place, &partials))) {
            Ok(()) => {}
            Err(PublishError::Unauthorized(_)) => {
                return vec![
                    Output::Send(reject(id, RejectReason::NotTrusted)),
                    Output::End,
                ];
            }
            Err(_) => return self.decline(id, RejectCause::Busy, RejectReason::Busy, state),
        }

        let digest = pending.offer.digest();
        self.step = Step::Receiving(Active {
            offer: pending.offer,
            outputs: pending.outputs,
            slot: pending._slot,
            _active: active,
            item: 0,
            received: 0,
            verifying: false,
        });
        vec![Output::Send(Message::OfferAccept {
            transfer: id,
            offer_digest: digest,
        })]
    }

    /// Nobody can be asked about `transfer` now, such as when nobody is
    /// receiving or too many questions wait. The offer is refused as busy.
    pub fn on_unasked(&mut self, transfer: TransferId, state: &mut State) -> Vec<Output> {
        match std::mem::replace(&mut self.step, Step::Idle) {
            Step::Deciding(p) if p.offer.transfer() == transfer => {
                self.decline(transfer, RejectCause::Busy, RejectReason::Busy, state)
            }
            other => {
                self.step = other;
                Vec::new()
            }
        }
    }

    /// The runtime finished writing `item`: decide whether it is published.
    /// `publish` renames the partial file to the output name; it runs only
    /// after every check passed, inside the caller's state lock, so
    /// revocation cannot slip between the check and the rename.
    pub fn on_verified(
        &mut self,
        item: usize,
        content: Content,
        state: &mut State,
        publish: impl FnOnce(&str, &OutputName) -> Result<(), PublishFailure>,
    ) -> Vec<Output> {
        let mut a = match std::mem::replace(&mut self.step, Step::Idle) {
            Step::Receiving(a) if a.verifying && a.item == item => a,
            other => {
                self.step = other;
                return Vec::new();
            }
        };

        let id = a.offer.transfer();
        let declared = &a.offer.items()[item];
        let failure = if content.digest != declared.digest {
            Some((FailCause::Digest, ItemResult::DigestMismatch))
        } else if !content.text_ok {
            Some((FailCause::Content, ItemResult::ContentMismatch))
        } else {
            None
        };

        let result = match failure {
            Some((cause, result)) => {
                state.update_transfer(&id, |t| t.apply(item, ItemEvent::Fail(cause)));
                result
            }
            None => match state.may_publish(&id, item) {
                Err(PublishError::Cancelled) => {
                    self.step = Step::Receiving(a);
                    return self.cancel_locally(state);
                }
                Err(PublishError::Unauthorized(_)) => {
                    // Revocation won; the transfer is already cancelled.
                    self.step = Step::Closed;
                    return vec![
                        Output::Send(item_result(id, item, ItemResult::Unauthorized)),
                        Output::Send(Message::Cancel {
                            transfer: id,
                            reason: CancelReason::Revoked,
                        }),
                        Output::End,
                    ];
                }
                Err(_) => {
                    return self.fail_and_close(a, FailCause::Protocol, ErrorCode::Internal, state);
                }
                Ok(()) => match publish(&partial_name(id, item), &a.outputs[item]) {
                    Err(PublishFailure::Unsynced(_)) => {
                        self.step = Step::Closed;
                        return unconfirmed(a, state);
                    }
                    Ok(()) => {
                        if state.record_published(&id, item).is_err() {
                            // The file is published but its record is not
                            // saved. Stop without telling the peer; startup
                            // recovery finds the file and records it.
                            self.step = Step::Closed;
                            return vec![Output::Abort(ErrorCode::Internal)];
                        }
                        ItemResult::Published
                    }
                    Err(failure) => {
                        let (cause, result) = match failure {
                            PublishFailure::Exists => (FailCause::Exists, ItemResult::Exists),
                            PublishFailure::Io(_) | PublishFailure::Unsynced(_) => {
                                (FailCause::Storage, ItemResult::StorageFailed)
                            }
                        };
                        state.update_transfer(&id, |t| t.apply(item, ItemEvent::Fail(cause)));
                        result
                    }
                },
            },
        };

        a.item += 1;
        a.received = 0;
        a.verifying = false;
        if a.item < a.offer.items().len() {
            self.step = Step::Receiving(a);
        }
        vec![Output::Send(item_result(id, item, result))]
    }

    /// Writing the current item's partial file failed, or the disk is
    /// full. The item fails and the rest of the transfer is cancelled.
    /// A cancel or revoke that reached the state first wins instead: it
    /// may be what took the partial file's record away.
    pub fn on_storage_failure(&mut self, state: &mut State) -> Vec<Output> {
        if self.should_stop(state) {
            return self.on_wake(state);
        }

        let a = match std::mem::replace(&mut self.step, Step::Closed) {
            Step::Receiving(a) => a,
            other => {
                self.step = other;
                return Vec::new();
            }
        };

        let id = a.offer.transfer();
        let item = a.item;
        state.update_transfer(&id, |t| {
            if t.items()[item] == ItemState::Queued {
                let _ = t.apply(item, ItemEvent::Start);
            }
            let _ = t.apply(item, ItemEvent::Fail(FailCause::Storage));
            t.cancel(CancelCause::Receiver);
        });
        vec![
            Output::Send(Message::Cancel {
                transfer: id,
                reason: CancelReason::Receiver,
            }),
            Output::End,
        ]
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
        self.step = Step::Closed;
        state.update_transfer(&transfer, |t| t.cancel(CancelCause::Receiver));
        vec![
            Output::Send(Message::Cancel {
                transfer,
                reason: CancelReason::Receiver,
            }),
            Output::End,
        ]
    }

    /// The connection ended. Unfinished work fails, or pauses while the
    /// node stops; nothing is published.
    pub fn on_disconnect(&mut self, state: &mut State) {
        if let Some(id) = self.current() {
            state.interrupt(&id, FailCause::Connection);
        }
        self.step = Step::Closed;
    }

    fn on_offer(
        &mut self,
        fields: crate::offer::OfferFields,
        target: Option<&Target<'_>>,
        state: &mut State,
    ) -> Vec<Output> {
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
        if !state.first_offer(id) {
            return self.close(ErrorCode::UnexpectedMessage);
        }

        // Each item needs a file name this device can create, and no two
        // items of one offer may share one.
        let Some(outputs) = output_names(&offer) else {
            return vec![Output::Send(reject(id, RejectReason::Malformed))];
        };
        let transfer = match Transfer::offered(id, self.authority, offer.items().len()) {
            Ok(t) => t,
            Err(_) => return vec![Output::Send(reject(id, RejectReason::Malformed))],
        };

        // An offer that could not be accepted now is refused before the
        // person is asked.
        let Some(target) = target else {
            return vec![Output::Send(reject(id, RejectReason::Busy))];
        };
        if let Err((_, reason)) = room_for(&offer, target, state) {
            return vec![Output::Send(reject(id, reason))];
        }
        let slot = match self.budget.admit_transfer() {
            Ok(s) => s,
            Err(_) => return vec![Output::Send(reject(id, RejectReason::Busy))],
        };

        let info = TransferInfo::new(false, offer.clone(), state.now_ms());
        if state.insert_transfer(transfer, info).is_err() {
            return vec![Output::Send(reject(id, RejectReason::Busy))];
        }

        let Some(record) = state.trust().record(&self.peer) else {
            return self.revoked(state);
        };
        let view = ConsentView::new(&offer, record);
        self.step = Step::Deciding(Pending {
            offer,
            outputs,
            _slot: slot,
        });
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
        if usize::from(item) != a.item || offset != a.received {
            // A chunk for another item or offset is out of order.
            return self.fail_and_close(
                a,
                FailCause::Protocol,
                ErrorCode::UnexpectedMessage,
                state,
            );
        }
        let Some(end) = a
            .received
            .checked_add(data.len() as u64)
            .filter(|end| *end <= expected)
        else {
            return self.fail_item_and_close(
                a,
                FailCause::Length,
                ItemResult::LengthMismatch,
                state,
            );
        };

        if a.received == 0 {
            let index = a.item;
            let started = state.update_transfer(&id, |t| {
                t.items()[index] != ItemState::Queued || t.apply(index, ItemEvent::Start).is_ok()
            });
            if started != Some(true) {
                return self.fail_and_close(a, FailCause::Protocol, ErrorCode::Internal, state);
            }
        }

        a.received = end;
        let index = a.item;
        self.step = Step::Receiving(a);
        vec![Output::Write {
            item: index,
            offset,
            data,
        }]
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
        if a.received != a.offer.items()[index].size {
            return self.fail_item_and_close(
                a,
                FailCause::Length,
                ItemResult::LengthMismatch,
                state,
            );
        }
        let complete = state.update_transfer(&id, |t| {
            let started =
                t.items()[index] != ItemState::Queued || t.apply(index, ItemEvent::Start).is_ok();
            started && t.apply(index, ItemEvent::BytesComplete).is_ok()
        });
        if complete != Some(true) {
            return self.fail_and_close(a, FailCause::Protocol, ErrorCode::Internal, state);
        }
        a.verifying = true;
        self.step = Step::Receiving(a);
        vec![Output::Verify { item: index }]
    }

    fn revoked(&mut self, state: &mut State) -> Vec<Output> {
        let current = self.current();
        self.step = Step::Closed;
        let mut out = Vec::new();
        if let Some(id) = current {
            state.update_transfer(&id, Transfer::revoke);
            out.push(Output::Send(Message::Cancel {
                transfer: id,
                reason: CancelReason::Revoked,
            }));
        }
        out.push(Output::Abort(ErrorCode::NotTrusted));
        out
    }

    fn decline(
        &mut self,
        id: TransferId,
        cause: RejectCause,
        reason: RejectReason,
        state: &mut State,
    ) -> Vec<Output> {
        state.update_transfer(&id, |t| t.reject(cause));
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
        let item = a.item;
        state.update_transfer(&id, |t| {
            if t.items()[item] == ItemState::Queued {
                let _ = t.apply(item, ItemEvent::Start);
            }
            let _ = t.apply(item, ItemEvent::Fail(cause));
        });
        let mut out = vec![Output::Send(item_result(id, item, result))];
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
        let id = match step {
            Step::Deciding(p) => p.offer.transfer(),
            Step::Receiving(a) => a.offer.transfer(),
            Step::Idle | Step::Closed => return,
        };
        state.update_transfer(&id, |t| t.fail_rest(cause));
    }

    fn close(&mut self, code: ErrorCode) -> Vec<Output> {
        self.step = Step::Closed;
        vec![Output::Abort(code)]
    }
}

/// The file names an offer's items get here, or `None` if one cannot be
/// used or two would collide.
fn output_names(offer: &Offer) -> Option<Vec<OutputName>> {
    if offer.kind() != PayloadKind::Files {
        return Some(vec![OutputName::generated(offer.kind(), offer.transfer())]);
    }
    let mut names = Vec::with_capacity(offer.items().len());
    for item in offer.items() {
        let name = OutputName::new(item.name.as_bytes()).ok()?;
        if names.contains(&name) {
            return None;
        }
        names.push(name);
    }
    Some(names)
}

/// Whether the offer fits the partial-file quota and the free space at the
/// destination. Free space is advice; a write can still find the disk full.
fn room_for(
    offer: &Offer,
    target: &Target<'_>,
    state: &mut State,
) -> Result<(), (RejectCause, RejectReason)> {
    let (_, partials) = state
        .store()
        .charged()
        .map_err(|_| (RejectCause::Busy, RejectReason::Busy))?;
    let quota = partials
        .checked_add(offer.total())
        .is_some_and(|t| t <= MAX_PARTIAL_TOTAL);
    let space = offer
        .total()
        .checked_add(MIN_FREE_BYTES)
        .is_some_and(|t| t <= target.free_bytes);
    if quota && space {
        Ok(())
    } else {
        Err((RejectCause::TooLarge, RejectReason::TooLarge))
    }
}

fn reject(transfer: TransferId, reason: RejectReason) -> Message {
    Message::OfferReject { transfer, reason }
}

/// The rename happened but the folder could not be synced, so nobody can
/// say whether the file survives a crash. The item pauses with its ledger
/// row, which records the file's inode, and startup recovery decides it;
/// the rest of the batch is cancelled. The sender hears that storage
/// failed, never that the item was published.
fn unconfirmed(a: Active, state: &mut State) -> Vec<Output> {
    let (id, item) = (a.offer.transfer(), a.item);
    state.update_transfer(&id, |t| {
        let _ = t.apply(item, ItemEvent::Pause);
        for rest in item + 1..t.items().len() {
            let _ = t.apply(rest, ItemEvent::Cancel(CancelCause::Receiver));
        }
    });
    // The paused work outlives this session, so its place among the
    // unfinished transfers goes to the state until the work ends.
    state.hold_paused_slot(id, a.slot);
    vec![
        Output::Send(item_result(id, item, ItemResult::StorageFailed)),
        Output::Send(Message::Cancel {
            transfer: id,
            reason: CancelReason::Receiver,
        }),
        Output::End,
    ]
}

fn item_result(transfer: TransferId, item: usize, result: ItemResult) -> Message {
    Message::ItemResult {
        transfer,
        // Bounded by MAX_BATCH_ITEMS when the offer was validated.
        item: item as u16,
        result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::PeerText;
    use crate::identity::DeviceKey;
    use crate::limits::{Limits, Usage};
    use crate::storage::FileState;
    use crate::transfer::{BatchOutcome, OfferState};
    use crate::trust::{LocalAlias, VerifiedPeer};
    use zeroize::Zeroizing;

    const ROOMY: u64 = 1 << 40;

    struct Fixture {
        state: State,
        budget: Budget,
        rx: Receiver,
        peer: DeviceId,
        place: Place,
        free: u64,
        /// Names published by the commit step.
        published: Vec<String>,
    }

    fn fixture() -> Fixture {
        let peer = DeviceKey::from_seed(&Zeroizing::new([8; 32])).id();
        let mut state = State::in_memory().unwrap();
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
            place: Place {
                dir: b"/tmp/in".to_vec(),
                dev: 1,
                ino: 2,
            },
            free: ROOMY,
            published: Vec::new(),
        }
    }

    impl Fixture {
        fn msg(&mut self, msg: Message) -> Vec<Output> {
            let target = Target {
                place: &self.place,
                free_bytes: self.free,
            };
            self.rx.on_message(msg, Some(&target), &mut self.state)
        }

        fn answer(&mut self, transfer: TransferId, accept: bool) -> Vec<Output> {
            let target = Target {
                place: &self.place,
                free_bytes: self.free,
            };
            self.rx
                .on_answer(transfer, accept, Some(&target), &mut self.state)
        }

        /// Plays the runtime for one item: reports the bytes written, then
        /// publishes with `publish` if asked to.
        fn verify(
            &mut self,
            n: u8,
            item: usize,
            written: &[u8],
            publish: Result<(), PublishFailure>,
        ) -> Vec<Output> {
            let offer = self.state.info(&id(n)).unwrap().offer.clone();
            let mut check = ContentCheck::new(&offer);
            check.update(written);
            let published = &mut self.published;
            self.rx
                .on_verified(item, check.finish(), &mut self.state, |_, output| {
                    if !matches!(publish, Err(PublishFailure::Exists | PublishFailure::Io(_))) {
                        published.push(output.as_str().to_owned());
                    }
                    publish
                })
        }

        fn partial_bytes(&mut self) -> u64 {
            self.state.store().charged().unwrap().1
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

    fn result(n: u8, item: u16, result: ItemResult) -> Output {
        Output::Send(Message::ItemResult {
            transfer: id(n),
            item,
            result,
        })
    }

    /// Offers `content` as transfer `n` and accepts it.
    fn accepted(f: &mut Fixture, n: u8, content: &[u8]) {
        let out = f.msg(file_offer(n, content));
        assert!(matches!(out.as_slice(), [Output::Ask { .. }]), "{out:?}");
        let out = f.answer(id(n), true);
        assert!(
            matches!(out.as_slice(), [Output::Send(Message::OfferAccept { .. })]),
            "{out:?}"
        );
    }

    fn two_items(f: &mut Fixture, n: u8) {
        let fields = Offer::for_files(id(n), &[("a", b"one"), ("b", b"two")])
            .unwrap()
            .fields();
        f.msg(Message::Offer(fields));
        f.answer(id(n), true);
    }

    fn items(f: &Fixture, n: u8) -> Vec<ItemState> {
        f.state.transfer(&id(n)).unwrap().items().to_vec()
    }

    #[test]
    fn a_complete_item_is_published_only_after_its_length_and_digest_match() {
        let mut f = fixture();
        accepted(&mut f, 1, b"hello world");
        assert_eq!(f.partial_bytes(), 11);
        let out = f.msg(chunk(1, 0, 0, b"hello "));
        assert_eq!(
            out,
            [Output::Write {
                item: 0,
                offset: 0,
                data: b"hello ".to_vec()
            }]
        );
        f.msg(chunk(1, 0, 6, b"world"));
        assert_eq!(f.msg(end(1, 0)), [Output::Verify { item: 0 }]);
        assert!(f.published.is_empty());
        let out = f.verify(1, 0, b"hello world", Ok(()));
        assert_eq!(out, [result(1, 0, ItemResult::Published)]);
        assert_eq!(f.published, ["f"]);
        assert_eq!(
            f.state.transfer(&id(1)).unwrap().outcome(),
            BatchOutcome::Completed
        );
        // The partial became the published file, so nothing stays charged.
        assert_eq!(f.partial_bytes(), 0);
    }

    #[test]
    fn a_declined_offer_reserves_nothing() {
        let mut f = fixture();
        f.msg(file_offer(1, b"data"));
        let out = f.answer(id(1), false);
        assert_eq!(out, [Output::Send(reject(id(1), RejectReason::Declined))]);
        assert_eq!(items(&f, 1), [ItemState::Rejected]);
        assert!(f.state.store().files().unwrap().is_empty());
    }

    #[test]
    fn a_replayed_offer_identifier_ends_the_session() {
        let mut f = fixture();
        f.msg(file_offer(1, b"data"));
        f.answer(id(1), false);
        let out = f.msg(file_offer(1, b"data"));
        assert_eq!(out, [Output::Abort(ErrorCode::UnexpectedMessage)]);
        assert!(f.rx.is_closed());
    }

    #[test]
    fn a_replayed_offer_identifier_is_refused_after_its_transfer_leaves_the_history() {
        let mut f = fixture();
        let first = file_offer(0, b"data");
        f.msg(first.clone());
        f.answer(id(0), false);
        for n in 1..=crate::state::MAX_KEPT_TRANSFERS {
            let later = TransferId::from_bytes((n as u128).to_be_bytes());
            let offer = Offer::for_files(later, &[("f", b"x")]).unwrap().fields();
            f.msg(Message::Offer(offer));
            f.answer(later, false);
        }
        assert!(f.state.transfer(&id(0)).is_none(), "still in the history");
        let out = f.msg(first);
        assert_eq!(out, [Output::Abort(ErrorCode::UnexpectedMessage)]);
    }

    #[test]
    fn a_chunk_before_acceptance_ends_the_session_without_writing() {
        let mut f = fixture();
        f.msg(file_offer(1, b"data"));
        let out = f.msg(chunk(1, 0, 0, b"da"));
        assert_eq!(out, [Output::Abort(ErrorCode::UnexpectedMessage)]);
        assert!(f.state.transfer(&id(1)).unwrap().is_terminal());
    }

    #[test]
    fn an_out_of_order_chunk_fails_the_item_and_its_partial_is_marked_for_deletion() {
        let mut f = fixture();
        accepted(&mut f, 1, b"abcdef");
        f.msg(chunk(1, 0, 0, b"ab"));
        let out = f.msg(chunk(1, 0, 4, b"ef"));
        assert_eq!(out, [Output::Abort(ErrorCode::UnexpectedMessage)]);
        assert_eq!(items(&f, 1), [ItemState::Failed(FailCause::Protocol)]);
        // Its bytes stay charged until the file is gone.
        assert_eq!(f.state.store().files().unwrap()[0].state, FileState::Delete);
        assert_eq!(f.partial_bytes(), 6);
    }

    #[test]
    fn bytes_beyond_the_offered_length_fail_the_item_without_writing() {
        let mut f = fixture();
        accepted(&mut f, 1, b"abc");
        let out = f.msg(chunk(1, 0, 0, b"abcd"));
        assert_eq!(
            out,
            [
                result(1, 0, ItemResult::LengthMismatch),
                Output::Abort(ErrorCode::Malformed)
            ]
        );
        assert_eq!(items(&f, 1), [ItemState::Failed(FailCause::Length)]);
    }

    #[test]
    fn an_item_that_ends_short_fails_with_a_length_mismatch() {
        let mut f = fixture();
        accepted(&mut f, 1, b"abc");
        f.msg(chunk(1, 0, 0, b"ab"));
        let out = f.msg(end(1, 0));
        assert!(out.contains(&result(1, 0, ItemResult::LengthMismatch)));
        assert_eq!(items(&f, 1), [ItemState::Failed(FailCause::Length)]);
    }

    #[test]
    fn bytes_that_do_not_match_the_offered_digest_are_not_published() {
        let mut f = fixture();
        accepted(&mut f, 1, b"abc");
        f.msg(chunk(1, 0, 0, b"abX"));
        f.msg(end(1, 0));
        let out = f.verify(1, 0, b"abX", Ok(()));
        assert_eq!(out, [result(1, 0, ItemResult::DigestMismatch)]);
        assert_eq!(items(&f, 1), [ItemState::Failed(FailCause::Digest)]);
        assert!(f.published.is_empty());
    }

    #[test]
    fn text_whose_preview_differs_from_its_content_is_refused() {
        let mut f = fixture();
        let real = b"pay the invoice";
        let mut fields = Offer::for_text(id(1), PayloadKind::Text, b"say hi to mum!!")
            .unwrap()
            .fields();
        fields.items[0].2 = *blake3::hash(real).as_bytes();
        f.msg(Message::Offer(fields));
        f.answer(id(1), true);
        f.msg(chunk(1, 0, 0, real));
        f.msg(end(1, 0));
        let out = f.verify(1, 0, real, Ok(()));
        // The sender learns the real reason, not a digest mismatch.
        assert_eq!(out, [result(1, 0, ItemResult::ContentMismatch)]);
        assert_eq!(items(&f, 1), [ItemState::Failed(FailCause::Content)]);
        assert!(f.published.is_empty());
    }

    #[test]
    fn text_checks_follow_utf8_across_chunk_boundaries() {
        let text = "héllo €".as_bytes();
        let mut split =
            ContentCheck::new(&Offer::for_text(id(1), PayloadKind::Text, text).unwrap());
        for byte in text {
            split.update(std::slice::from_ref(byte));
        }
        assert!(split.finish().text_ok);
        let cut = b"ab\xe2\x82";
        let mut ends_inside =
            ContentCheck::new(&Offer::for_text(id(2), PayloadKind::Text, cut).unwrap());
        ends_inside.update(cut);
        assert!(!ends_inside.finish().text_ok);
        let bad = b"a\xffb";
        let mut invalid =
            ContentCheck::new(&Offer::for_text(id(3), PayloadKind::Text, bad).unwrap());
        invalid.update(bad);
        assert!(!invalid.finish().text_ok);
    }

    #[test]
    fn an_existing_file_fails_only_that_item_and_the_batch_continues() {
        let mut f = fixture();
        two_items(&mut f, 1);
        f.msg(chunk(1, 0, 0, b"one"));
        f.msg(end(1, 0));
        let out = f.verify(1, 0, b"one", Err(PublishFailure::Exists));
        assert_eq!(out, [result(1, 0, ItemResult::Exists)]);
        f.msg(chunk(1, 1, 0, b"two"));
        f.msg(end(1, 1));
        f.verify(1, 1, b"two", Ok(()));
        assert_eq!(
            items(&f, 1),
            [ItemState::Failed(FailCause::Exists), ItemState::Published]
        );
        assert!(matches!(
            f.state.transfer(&id(1)).unwrap().outcome(),
            BatchOutcome::PartiallyCompleted { published: 1, .. }
        ));
    }

    #[test]
    fn a_rename_whose_folder_cannot_be_synced_is_never_acknowledged() {
        let mut f = fixture();
        two_items(&mut f, 1);
        f.msg(chunk(1, 0, 0, b"one"));
        f.msg(end(1, 0));
        let unsynced = PublishFailure::Unsynced(std::io::Error::other("EIO"));
        let out = f.verify(1, 0, b"one", Err(unsynced));
        assert_eq!(
            out,
            [
                result(1, 0, ItemResult::StorageFailed),
                Output::Send(Message::Cancel {
                    transfer: id(1),
                    reason: CancelReason::Receiver,
                }),
                Output::End,
            ]
        );
        // The item waits for recovery, keeping the ledger row that names
        // its file; the item that never started is cancelled.
        assert_eq!(
            items(&f, 1),
            [
                ItemState::Paused,
                ItemState::Cancelled(CancelCause::Receiver)
            ]
        );
        let rows: Vec<FileState> = f
            .state
            .store()
            .files()
            .unwrap()
            .iter()
            .map(|r| r.state)
            .collect();
        assert_eq!(rows, [FileState::Reserved, FileState::Delete]);

        // The paused transfer still counts as unfinished after its session
        // ends, until it expires.
        drop(f.rx);
        assert_eq!(f.budget.usage().nonterminal_transfers, 1);
        let now = f.state.now_ms();
        let window = crate::limits::RESUME_WINDOW.as_millis() as u64;
        assert_eq!(f.state.expire(now + window), [id(1)]);
        assert_eq!(f.budget.usage().nonterminal_transfers, 0);
    }

    #[test]
    fn unusable_or_colliding_names_are_refused_before_consent() {
        let mut f = fixture();
        let bad = Offer::for_files(id(1), &[("../escape", b"x")]).unwrap();
        let out = f.msg(Message::Offer(bad.fields()));
        assert_eq!(out, [Output::Send(reject(id(1), RejectReason::Malformed))]);
        let twins = Offer::for_files(id(2), &[("same", b"x"), ("same", b"y")]).unwrap();
        let out = f.msg(Message::Offer(twins.fields()));
        assert_eq!(out, [Output::Send(reject(id(2), RejectReason::Malformed))]);
    }

    #[test]
    fn revocation_during_an_item_cancels_it_and_publishes_nothing() {
        let mut f = fixture();
        accepted(&mut f, 1, b"abcdef");
        f.msg(chunk(1, 0, 0, b"abc"));
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
    }

    #[test]
    fn a_completion_verified_after_revocation_is_not_published() {
        let mut f = fixture();
        accepted(&mut f, 1, b"abc");
        f.msg(chunk(1, 0, 0, b"abc"));
        f.msg(end(1, 0));
        // Revocation reaches the state while the runtime syncs the file.
        f.state.revoke(&f.peer).unwrap();
        let out = f.verify(1, 0, b"abc", Ok(()));
        assert!(
            out.contains(&result(1, 0, ItemResult::Unauthorized)),
            "{out:?}"
        );
        assert!(f.published.is_empty(), "the commit step never ran");
        assert_eq!(items(&f, 1), [ItemState::Cancelled(CancelCause::Revoked)]);
    }

    #[test]
    fn an_item_published_before_revocation_stays_published() {
        let mut f = fixture();
        two_items(&mut f, 1);
        f.msg(chunk(1, 0, 0, b"one"));
        f.msg(end(1, 0));
        f.verify(1, 0, b"one", Ok(()));
        f.state.revoke(&f.peer).unwrap();
        f.rx.on_wake(&mut f.state);
        assert_eq!(
            items(&f, 1),
            [
                ItemState::Published,
                ItemState::Cancelled(CancelCause::Revoked)
            ]
        );
        assert_eq!(f.published.len(), 1);
    }

    #[test]
    fn offers_beyond_free_space_or_capacity_are_refused_before_consent() {
        let mut f = fixture();
        f.free = 100 + MIN_FREE_BYTES - 1;
        let out = f.msg(file_offer(1, &[0; 100]));
        assert_eq!(out, [Output::Send(reject(id(1), RejectReason::TooLarge))]);

        let budget = Budget::new(Limits {
            nonterminal_transfers: 0,
            ..Limits::COMPILED
        });
        let session = budget.admit_session().unwrap();
        let authority = f.state.trust.authorize(&f.peer).unwrap();
        f.rx = Receiver::new(authority, budget, session);
        f.free = ROOMY;
        let out = f.msg(file_offer(2, b"x"));
        assert_eq!(out, [Output::Send(reject(id(2), RejectReason::Busy))]);
    }

    #[test]
    fn an_offer_with_nowhere_to_go_is_refused_as_busy() {
        let mut f = fixture();
        let out = f.rx.on_message(file_offer(1, b"x"), None, &mut f.state);
        assert_eq!(out, [Output::Send(reject(id(1), RejectReason::Busy))]);
    }

    #[test]
    fn a_dropped_connection_fails_unfinished_work_and_frees_its_capacity() {
        let mut f = fixture();
        accepted(&mut f, 1, b"abcdef");
        f.msg(chunk(1, 0, 0, b"abc"));
        f.rx.on_disconnect(&mut f.state);
        assert_eq!(items(&f, 1), [ItemState::Failed(FailCause::Connection)]);
        drop(f.rx);
        assert_eq!(f.budget.usage(), Usage::default());
    }

    #[test]
    fn work_interrupted_while_the_node_stops_pauses_and_keeps_its_partial_file() {
        let mut f = fixture();
        accepted(&mut f, 1, b"abcdef");
        f.msg(chunk(1, 0, 0, b"abc"));
        f.state.set_pausing(true);
        f.rx.on_disconnect(&mut f.state);
        assert_eq!(items(&f, 1), [ItemState::Paused]);
        assert_eq!(
            f.state.store().files().unwrap()[0].state,
            FileState::Reserved
        );
        assert_eq!(f.partial_bytes(), 6);
    }

    #[test]
    fn work_received_after_the_wall_clock_went_back_keeps_its_whole_window() {
        const DAY: u64 = 86_400_000;
        let mut f = fixture();
        // The lifetime clock already reached 8 days past what the wall
        // clock now says, as after the wall clock was set back.
        let wall = crate::clock::wall_now();
        f.state
            .lifetime_now(wall + 8 * DAY, std::time::Instant::now());
        accepted(&mut f, 1, b"abcdef");
        f.msg(chunk(1, 0, 0, b"abc"));
        f.state.set_pausing(true);
        f.rx.on_disconnect(&mut f.state);
        assert_eq!(items(&f, 1), [ItemState::Paused]);
        let now = f.state.now_ms();
        assert!(f.state.expire(now).is_empty());
        let window = crate::limits::RESUME_WINDOW.as_millis() as u64;
        assert_eq!(f.state.expire(now + window), [id(1)]);
    }

    #[test]
    fn a_cancel_requested_before_verification_wins_over_publication() {
        let mut f = fixture();
        accepted(&mut f, 1, b"abc");
        f.msg(chunk(1, 0, 0, b"abc"));
        f.msg(end(1, 0));
        // The person cancels while the runtime syncs the file.
        assert!(f.state.request_cancel(id(1)));
        let out = f.verify(1, 0, b"abc", Ok(()));
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
        assert!(f.published.is_empty());
        assert_eq!(items(&f, 1), [ItemState::Cancelled(CancelCause::Receiver)]);
    }

    #[test]
    fn a_storage_failure_fails_the_item_and_cancels_the_rest() {
        let mut f = fixture();
        two_items(&mut f, 1);
        f.msg(chunk(1, 0, 0, b"on"));
        let out = f.rx.on_storage_failure(&mut f.state);
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
        assert_eq!(
            items(&f, 1),
            [
                ItemState::Failed(FailCause::Storage),
                ItemState::Cancelled(CancelCause::Receiver)
            ]
        );
    }

    #[test]
    fn the_sender_can_withdraw_an_offer_before_the_answer() {
        let mut f = fixture();
        f.msg(file_offer(1, b"x"));
        let out = f.msg(Message::Cancel {
            transfer: id(1),
            reason: CancelReason::Sender,
        });
        assert_eq!(out, [Output::End]);
        assert_eq!(
            f.state.transfer(&id(1)).unwrap().offer_state(),
            OfferState::Rejected(RejectCause::Withdrawn)
        );
        // A late answer for the withdrawn offer changes nothing.
        assert!(f.answer(id(1), true).is_empty());
    }
}
