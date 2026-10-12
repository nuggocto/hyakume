//! Property tests over random operation sequences.
//!
//! Each property states an invariant from the plan and checks it after every
//! step of a bounded random sequence. Case counts, sequence lengths, and
//! input sizes are all capped.

use proptest::prelude::*;
use unicode_width::UnicodeWidthStr;
use zeroize::Zeroizing;

use crate::clock::Moment;
use crate::display::{PeerText, SafeLine};
use crate::identity::DeviceKey;
use crate::ids::{ClaimId, InvitationId, TransferId};
use crate::invitation::{
    ClaimState, Invitation, InvitationKind, InvitationSecret, InvitationState,
};
use crate::limits::{Budget, Limits, MIN_BUFFER_BYTES, Usage};
use crate::pairing::Verifier;
use crate::protocol::{self, Message};
use crate::transfer::{BatchOutcome, CancelCause, FailCause, ItemEvent, ItemState, Transfer};
use crate::trust::{LocalAlias, TrustStore, VerifiedPeer};

fn config() -> ProptestConfig {
    ProptestConfig {
        cases: 256,
        max_shrink_iters: 2048,
        // A failure prints its shrunk input; it becomes a named unit test
        // instead of a seed file in the tree.
        failure_persistence: None,
        ..ProptestConfig::default()
    }
}

#[derive(Clone, Debug)]
enum TransferOp {
    Accept,
    Event(usize, ItemEvent),
    Publish(usize),
    Resume(usize),
    Revoke,
}

fn item_event() -> impl Strategy<Value = ItemEvent> {
    prop_oneof![
        Just(ItemEvent::Start),
        Just(ItemEvent::BytesComplete),
        Just(ItemEvent::Pause),
        Just(ItemEvent::Resumed),
        Just(ItemEvent::Expire),
        Just(ItemEvent::Cancel(CancelCause::Sender)),
        Just(ItemEvent::Fail(FailCause::Digest)),
    ]
}

fn transfer_op() -> impl Strategy<Value = TransferOp> {
    prop_oneof![
        1 => Just(TransferOp::Accept),
        8 => (0..4usize, item_event()).prop_map(|(i, e)| TransferOp::Event(i, e)),
        3 => (0..4usize).prop_map(TransferOp::Publish),
        1 => (0..4usize).prop_map(TransferOp::Resume),
        1 => Just(TransferOp::Revoke),
    ]
}

proptest! {
    #![proptest_config(config())]

    /// No sequence produces a false `Completed`, a published item never
    /// changes, nothing is published after revocation, and at most one item
    /// is active.
    #[test]
    fn transfer_outcomes_never_disagree_with_item_states(
        items in 1..4usize,
        ops in prop::collection::vec(transfer_op(), 0..64),
    ) {
        let device = DeviceKey::from_seed(&Zeroizing::new([1; 32])).id();
        let mut trust = TrustStore::new();
        let authority = trust
            .trust(VerifiedPeer::new(device, PeerText::default()), LocalAlias::new("p").unwrap())
            .unwrap();
        let mut t = Transfer::offered(TransferId::from_bytes([1; 16]), authority, items).unwrap();
        let mut published = vec![false; items];
        let mut revoked = false;
        for op in ops {
            let before: Vec<ItemState> = t.items().to_vec();
            match op {
                TransferOp::Accept => { let _ = t.accept(&trust); }
                TransferOp::Event(i, e) => { let _ = t.apply(i % items, e); }
                TransferOp::Publish(i) => {
                    let i = i % items;
                    let ok = t.publish(i, &trust).is_ok();
                    prop_assert!(!ok || (!revoked && before[i] == ItemState::Verifying));
                }
                TransferOp::Resume(i) => { let _ = t.resume(i % items, &trust); }
                TransferOp::Revoke => {
                    trust.revoke(&device).unwrap();
                    revoked = true;
                    t.revoke();
                }
            }
            for (i, state) in t.items().iter().enumerate() {
                if published[i] {
                    prop_assert_eq!(*state, ItemState::Published);
                }
                if *state == ItemState::Published && !published[i] {
                    prop_assert!(!revoked, "published after revocation");
                    published[i] = true;
                }
            }
            let active = t
                .items()
                .iter()
                .filter(|s| matches!(s, ItemState::Transferring | ItemState::Verifying | ItemState::Resuming))
                .count();
            prop_assert!(active <= 1);
            let all_published = t.items().iter().all(|s| *s == ItemState::Published);
            prop_assert_eq!(t.outcome() == BatchOutcome::Completed, all_published);
            if let BatchOutcome::PartiallyCompleted { published: n, unsuccessful: u } = t.outcome() {
                prop_assert!(n > 0 && n < items);
                prop_assert_eq!(n + u.rejected + u.cancelled + u.failed + u.expired, items);
            }
        }
    }
}

#[derive(Clone, Debug)]
enum BudgetOp {
    Session,
    Transfer,
    Activate,
    Lease(u64),
    Drop(usize),
}

fn budget_op() -> impl Strategy<Value = BudgetOp> {
    prop_oneof![
        Just(BudgetOp::Session),
        Just(BudgetOp::Transfer),
        Just(BudgetOp::Activate),
        (0..MIN_BUFFER_BYTES * 4 / 5).prop_map(BudgetOp::Lease),
        (0..16usize).prop_map(BudgetOp::Drop),
    ]
}

/// Capacity a property holds; dropping an entry releases it.
enum Held {
    Session(crate::limits::SessionSlot),
    Transfer(crate::limits::TransferSlot),
    Active(#[allow(dead_code)] crate::limits::ActiveSlot),
    Lease(crate::limits::BufferLease),
}

proptest! {
    #![proptest_config(config())]

    /// Usage never passes a limit, equals what is actually held, and returns
    /// to zero when everything is released, whatever the order.
    #[test]
    fn budget_usage_matches_what_is_held(ops in prop::collection::vec(budget_op(), 0..80)) {
        let limits = Limits {
            sessions: 3,
            active_transfers: 2,
            active_transfers_per_session: 1,
            nonterminal_transfers: 4,
            buffer_bytes: MIN_BUFFER_BYTES,
            ..Limits::COMPILED
        };
        let budget = Budget::new(limits);
        let mut held: Vec<Held> = Vec::new();
        for op in ops {
            match op {
                BudgetOp::Session => if let Ok(s) = budget.admit_session() { held.push(Held::Session(s)) },
                BudgetOp::Transfer => if let Ok(t) = budget.admit_transfer() { held.push(Held::Transfer(t)) },
                BudgetOp::Activate => {
                    let session = held.iter().find_map(|h| match h { Held::Session(s) => Some(s), _ => None });
                    let transfer = held.iter().find_map(|h| match h { Held::Transfer(t) => Some(t), _ => None });
                    if let (Some(s), Some(t)) = (session, transfer)
                        && let Ok(a) = budget.activate(s, t)
                    {
                        held.push(Held::Active(a));
                    }
                }
                BudgetOp::Lease(n) => if let Ok(l) = budget.lease(n) { held.push(Held::Lease(l)) },
                BudgetOp::Drop(i) => if !held.is_empty() { let i = i % held.len(); held.remove(i); },
            }
            let expected = Usage {
                connections: 0,
                sessions: held.iter().filter(|h| matches!(h, Held::Session(_))).count(),
                active_transfers: held.iter().filter(|h| matches!(h, Held::Active(_))).count(),
                nonterminal_transfers: held.iter().filter(|h| matches!(h, Held::Transfer(_))).count(),
                buffer_bytes: held.iter().map(|h| match h { Held::Lease(l) => l.bytes(), _ => 0 }).sum(),
            };
            prop_assert_eq!(budget.usage(), expected);
            prop_assert!(expected.sessions <= limits.sessions);
            prop_assert!(expected.active_transfers <= limits.active_transfers);
            prop_assert!(expected.nonterminal_transfers <= limits.nonterminal_transfers);
            prop_assert!(expected.buffer_bytes <= limits.buffer_bytes);
        }
        held.clear();
        prop_assert_eq!(budget.usage(), Usage::default());
    }
}

#[derive(Clone, Debug)]
enum InviteOp {
    Open(u8),
    Verified(u8),
    Unverified(u8),
    Approve(u8),
    Decline(u8),
    Cancel,
    Wait(u64),
}

fn invite_op() -> impl Strategy<Value = InviteOp> {
    prop_oneof![
        4 => (0..10u8).prop_map(InviteOp::Open),
        4 => (0..10u8).prop_map(InviteOp::Verified),
        2 => (0..10u8).prop_map(InviteOp::Unverified),
        3 => (0..10u8).prop_map(InviteOp::Approve),
        1 => (0..10u8).prop_map(InviteOp::Decline),
        1 => Just(InviteOp::Cancel),
        2 => (0..30_000u64).prop_map(InviteOp::Wait),
    ]
}

proptest! {
    #![proptest_config(config())]

    /// An invitation is used at most once, by exactly one claim, and never
    /// reopens once it has closed.
    #[test]
    fn an_invitation_is_used_at_most_once(
        kind in prop_oneof![Just(InvitationKind::Permanent), Just(InvitationKind::Temporary)],
        ops in prop::collection::vec(invite_op(), 0..64),
    ) {
        let mut inv = Invitation::new(
            InvitationId::from_bytes([1; 16]),
            kind,
            InvitationSecret::from_bytes([2; 32]),
            Moment::from_millis(0),
            std::time::Duration::from_secs(60),
        );
        let mut now = 0u64;
        let mut uses = 0;
        let mut closed = false;
        for op in ops {
            let at = Moment::from_millis(now);
            match op {
                InviteOp::Open(n) => { let _ = inv.open_claim(at, ClaimId::from_bytes([n; 16])); }
                InviteOp::Verified(n) => {
                    let _ = inv.claim_verified(at, ClaimId::from_bytes([n; 16]), Verifier::from_bytes([n; 4]));
                }
                InviteOp::Unverified(n) => { let _ = inv.claim_unverified(at, ClaimId::from_bytes([n; 16])); }
                InviteOp::Approve(n) => {
                    let claim = ClaimId::from_bytes([n; 16]);
                    let used = match kind {
                        InvitationKind::Permanent => inv.confirm_permanent(at, claim).is_ok(),
                        InvitationKind::Temporary => inv.approve(at, claim).is_ok(),
                    };
                    if used { uses += 1; }
                }
                InviteOp::Decline(n) => { let _ = inv.decline(at, ClaimId::from_bytes([n; 16])); }
                InviteOp::Cancel => inv.cancel(at),
                InviteOp::Wait(ms) => now += ms,
            }
            prop_assert!(uses <= 1);
            let state = inv.state(Moment::from_millis(now));
            if closed {
                prop_assert_ne!(state, InvitationState::Open);
            }
            closed |= state != InvitationState::Open;
            let used_claims = (0..10u8)
                .filter(|n| inv.claim_state(ClaimId::from_bytes([*n; 16])) == Some(ClaimState::Used))
                .count();
            prop_assert_eq!(used_claims, uses);
            prop_assert_eq!(state == InvitationState::Consumed, uses == 1);
        }
    }

    /// Decoding arbitrary bytes never panics, and anything accepted encodes
    /// back to the same bytes.
    #[test]
    fn decoding_arbitrary_payloads_is_total_and_canonical(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
        if let Ok(msg) = protocol::decode_payload(&bytes) {
            prop_assert_eq!(protocol::encode_payload(&msg), bytes);
        }
    }

    /// Any frame built from a valid message decodes to the same message.
    #[test]
    fn chunk_and_offer_frames_survive_encoding(
        transfer in any::<[u8; 16]>(),
        item in any::<u16>(),
        offset in any::<u64>(),
        data in prop::collection::vec(any::<u8>(), 0..256),
        names in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..32), 1..5),
    ) {
        let chunk = Message::Chunk { transfer: TransferId::from_bytes(transfer), item, offset, data };
        let frame = protocol::encode_frame(&chunk).unwrap();
        prop_assert_eq!(protocol::decode_frame(&frame).unwrap().map(|(m, _)| m), Some(chunk));
        let offer = Message::Offer(crate::offer::OfferFields {
            transfer: TransferId::from_bytes(transfer),
            kind: 3,
            items: names.into_iter().map(|n| (n, offset, [7; 32])).collect(),
            preview: Vec::new(),
        });
        let frame = protocol::encode_frame(&offer).unwrap();
        prop_assert_eq!(protocol::decode_frame(&frame).unwrap().map(|(m, _)| m), Some(offer));
    }

    /// A projection never contains a control or hidden character and never
    /// exceeds its column limit, whatever the input.
    #[test]
    fn projections_are_bounded_and_inert(
        bytes in prop::collection::vec(any::<u8>(), 0..2048),
        columns in 1..200usize,
    ) {
        let line = SafeLine::project(&bytes, columns);
        prop_assert!(line.as_str().width() <= columns);
        prop_assert!(!line.as_str().chars().any(|c| c.is_control()));
        let printable = line.as_str().chars().all(|c| c == ' ' || !c.is_whitespace());
        prop_assert!(printable);
    }
}
