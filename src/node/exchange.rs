//! Both sides of a pairing over one connection. [`PairingOwner`] settles
//! the claim and the trust reservation however the exchange ends.

use std::sync::Arc;

use iroh::Endpoint;
use iroh::endpoint::Connection;
use rand_core::OsRng;
use tokio::time::timeout;

use super::connections::{dial, exporter};
use super::wire::Wire;
use super::{NodeError, Prompt, Shared, route_addr};
use crate::clock::Moment;
use crate::identity::DeviceId;
use crate::ids::{ClaimId, InvitationId};
use crate::invitation::{ClaimState, InvitationKind, InvitationSecret, InvitationToken};
use crate::pairing::{self, ChannelBinding, ExchangeContext};
use crate::protocol::{Message, Purpose};
use crate::state::State;
use crate::trust::{LocalAlias, Reservation, VerifiedPeer};

/// The inviting side. Every claim that does not verify uses an attempt.
pub(super) async fn invite_exchange(
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

    let mut owner = PairingOwner {
        shared,
        claim: Some((invitation, claim)),
        reservation: None,
        paired: None,
    };
    let cx = ExchangeContext {
        binding: ChannelBinding {
            exporter: exporter(conn, pairing::EXPORTER_LABEL)?,
            inviter_endpoint: *endpoint.id().as_bytes(),
            joiner_endpoint: *conn.remote_id().as_bytes(),
            negotiation,
        },
        kind,
        invitation,
        claim,
    };

    let verified = inviter_steps(shared, wire, &secret, cx).await?;

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

    let person = ask_verifier(shared, conn, invitation, &verified).await;
    let answer =
        person && shared.change(|s| owner.accept(s, shared.clock.now(), verified.peer(), alias));
    wire.write(&Message::PairDecision(verified.decide(answer)))
        .await?;
    let theirs = match wire.message(shared.limits().idle_timeout).await? {
        Message::PairDecision(d) => d,
        _ => return Err(NodeError::Unexpected),
    };

    let peer = verified.conclude_permanent(answer, &theirs)?;
    shared.change(|s| owner.trust(s, peer))
}

/// Settles a pairing's claim and reservation when dropped, however the
/// exchange ends. A verified claim never reopens the invitation: each one
/// allows a single comparison of words.
pub(super) struct PairingOwner<'a> {
    shared: &'a Shared,
    /// The inviting side's claim. The joining side has none.
    claim: Option<(InvitationId, ClaimId)>,
    reservation: Option<Reservation>,
    /// The device trusted once both people said yes.
    paired: Option<DeviceId>,
}

impl PairingOwner<'_> {
    /// This side's person said yes. Holds the alias and a trusted place for
    /// `peer`, and on the inviting side uses the invitation. Returns false,
    /// changing nothing, when either is no longer possible; this side then
    /// answers no.
    fn accept(&mut self, s: &mut State, now: Moment, peer: DeviceId, alias: LocalAlias) -> bool {
        let Ok(reservation) = s.trust.reserve(peer, alias) else {
            return false;
        };
        if let Some((invitation, claim)) = self.claim {
            let used = s
                .invitations
                .get_mut(&invitation)
                .is_some_and(|p| p.invitation.confirm_permanent(now, claim).is_ok());
            if !used {
                s.trust.release(reservation);
                return false;
            }
        }
        self.reservation = Some(reservation);
        true
    }

    /// Both people said yes: saves the trust this side reserved.
    fn trust(&mut self, s: &mut State, peer: VerifiedPeer) -> Result<(), NodeError> {
        // A peer only concludes as trusted after this side said yes, and
        // saying yes took a reservation.
        let reservation = self.reservation.take().ok_or(NodeError::Invitation)?;
        let device = peer.device();
        s.trust_peer(peer, reservation)?;
        self.paired = Some(device);
        Ok(())
    }
}

impl Drop for PairingOwner<'_> {
    fn drop(&mut self) {
        let now = self.shared.clock.now();
        let (claim, reservation, paired) = (self.claim, self.reservation.take(), self.paired);
        self.shared.change(|s| {
            if let Some(reservation) = reservation {
                s.trust.release(reservation);
            }
            let Some((invitation, claim)) = claim else {
                return;
            };
            if let Some(p) = s.invitations.get_mut(&invitation) {
                let compared = matches!(
                    p.invitation.claim_state(claim),
                    Some(ClaimState::Verified(_) | ClaimState::Used)
                );
                let _ = match p.invitation.claim_state(claim) {
                    Some(ClaimState::Exchanging) => p.invitation.claim_unverified(now, claim),
                    Some(ClaimState::Verified(_)) => p.invitation.decline(now, claim),
                    Some(ClaimState::Used | ClaimState::Refused(_)) | None => Ok(()),
                };
                if compared && p.outcome.is_none() {
                    p.outcome = Some(paired.ok_or_else(|| {
                        "the pairing ended without both devices saying yes".to_owned()
                    }));
                }
            }
        });
    }
}

pub(super) async fn inviter_steps(
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

/// Asks the person to compare the verifier. No answer, a full prompt
/// queue, or a closed connection count as no.
pub(super) async fn ask_verifier(
    shared: &Shared,
    conn: &Connection,
    invitation: InvitationId,
    verified: &pairing::Verified,
) -> bool {
    let Some(reply) = shared.ask(|answer| Prompt::Verify {
        invitation,
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
pub(super) async fn join_exchange(
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

    let mut owner = PairingOwner {
        shared,
        claim: None,
        reservation: None,
        paired: None,
    };

    let person = ask_verifier(shared, conn, token.id, &verified).await;
    let answer =
        person && shared.change(|s| owner.accept(s, shared.clock.now(), verified.peer(), alias));
    wire.write(&Message::PairDecision(verified.decide(answer)))
        .await?;
    let theirs = match wire.message(shared.limits().idle_timeout).await? {
        Message::PairDecision(d) => d,
        _ => return Err(NodeError::Unexpected),
    };

    let peer = verified.conclude_permanent(answer, &theirs)?;
    let device = peer.device();
    shared.change(|s| owner.trust(s, peer))?;
    Ok(device)
}

pub(super) fn check_join(
    shared: &Shared,
    token: &str,
    alias: &str,
) -> Result<(InvitationToken, LocalAlias), NodeError> {
    let token = InvitationToken::parse(token)?;
    if token.kind != InvitationKind::Permanent {
        return Err(NodeError::Invitation);
    }
    let alias = LocalAlias::new(alias)?;
    shared.change(|s| {
        s.check_store()?;
        s.trust.check_alias(&alias, None).map_err(NodeError::from)
    })?;
    Ok((token, alias))
}

pub(super) async fn run_join(
    shared: Arc<Shared>,
    endpoint: Endpoint,
    token: InvitationToken,
    alias: LocalAlias,
) -> Result<DeviceId, NodeError> {
    let addr = route_addr(&token.route)?;
    let (conn, mut wire, negotiation) = dial(&shared, &endpoint, addr, Purpose::Pair).await?;
    let me = *endpoint.id().as_bytes();
    let result = join_exchange(&shared, &conn, &mut wire, &token, me, negotiation, alias).await;
    if let Err(e) = &result {
        wire.send_error(e).await;
    }
    wire.finish_dialer(&conn, shared.limits().handshake_deadline)
        .await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::DeviceKey;
    use crate::invitation::ClaimRefusal;
    use crate::invitation::{Invitation, InvitationState};
    use crate::limits::INVITATION_LIFETIME;
    use crate::node::test_shared;
    use crate::pairing::Verifier;
    use crate::state::PendingInvitation;
    use crate::trust::TrustError;
    use zeroize::Zeroizing;

    /// Adds an invitation for `alias` with one claim in progress.
    fn claimed(shared: &Shared, alias: &str, n: u8) -> (InvitationId, ClaimId) {
        let now = shared.clock.now();
        let (invitation, claim) = (
            InvitationId::from_bytes([n; 16]),
            ClaimId::from_bytes([n; 16]),
        );
        let mut open = Invitation::new(
            invitation,
            InvitationKind::Permanent,
            InvitationSecret::from_bytes([n; 32]),
            now,
            INVITATION_LIFETIME,
        );
        open.open_claim(now, claim).unwrap();
        let alias = LocalAlias::new(alias).unwrap();
        shared
            .change(|s| {
                s.add_invitation(
                    now,
                    PendingInvitation {
                        invitation: open,
                        alias,
                        outcome: None,
                    },
                )
            })
            .unwrap();
        (invitation, claim)
    }

    fn verify(shared: &Shared, (invitation, claim): (InvitationId, ClaimId)) {
        let now = shared.clock.now();
        let mut s = shared.lock();
        let p = s.invitations.get_mut(&invitation).unwrap();
        p.invitation
            .claim_verified(now, claim, Verifier::from_bytes([4; 4]))
            .unwrap();
    }

    fn owner(shared: &Shared, claim: (InvitationId, ClaimId)) -> PairingOwner<'_> {
        PairingOwner {
            shared,
            claim: Some(claim),
            reservation: None,
            paired: None,
        }
    }

    fn invitation_state(shared: &Shared, invitation: InvitationId) -> InvitationState {
        let now = shared.clock.now();
        let mut s = shared.lock();
        s.invitations
            .get_mut(&invitation)
            .unwrap()
            .invitation
            .state(now)
    }

    #[test]
    fn an_exchange_that_ends_unverified_uses_an_attempt() {
        let shared = test_shared();
        let (invitation, claim) = claimed(&shared, "phone", 1);
        drop(owner(&shared, (invitation, claim)));
        let mut s = shared.lock();
        let p = s.invitations.get_mut(&invitation).unwrap();
        assert_eq!(
            p.invitation.claim_state(claim),
            Some(ClaimState::Refused(ClaimRefusal::NotVerified))
        );
    }

    #[test]
    fn an_exchange_that_ends_after_verification_never_leaves_the_invitation_open() {
        let shared = test_shared();
        let device = |seed| DeviceKey::from_seed(&Zeroizing::new([seed; 32])).id();

        // Nobody answered.
        let unanswered = claimed(&shared, "phone", 1);
        verify(&shared, unanswered);
        drop(owner(&shared, unanswered));
        assert_eq!(
            invitation_state(&shared, unanswered.0),
            InvitationState::Declined
        );

        // This side said yes; the other side never answered.
        let accepted = claimed(&shared, "tablet", 2);
        verify(&shared, accepted);
        let mut owner = owner(&shared, accepted);
        let now = shared.clock.now();
        let alias = LocalAlias::new("tablet").unwrap();
        assert!(shared.change(|s| owner.accept(s, now, device(6), alias.clone())));
        // While it waits, no other pairing can take the alias.
        assert_eq!(
            shared.lock().trust.can_trust(device(7), &alias),
            Err(TrustError::AliasInUse)
        );
        drop(owner);
        assert_eq!(
            invitation_state(&shared, accepted.0),
            InvitationState::Consumed
        );
        assert_eq!(shared.lock().trust.can_trust(device(7), &alias), Ok(()));
        assert!(shared.lock().trust.authorize(&device(6)).is_err());
    }
}
