//! The pairing exchange: SPAKE2 bound to the QUIC connection, the
//! invitation, the claim, and both identities, ending in a short verifier
//! both people compare. It does no I/O, so every step has a unit test. The
//! joiner commits before it sees the inviter's share, so neither side can
//! steer the verifier; `docs/pairing-identity-session.md` has the design.

use curve25519_dalek::edwards::CompressedEdwardsY;
use rand_core::{CryptoRng, RngCore};
use spake2::{Ed25519Group, Identity, Password, Spake2};
use zeroize::Zeroizing;

use crate::display::PeerText;
use crate::identity::{DeviceId, DeviceKey};
use crate::ids::{ClaimId, InvitationId};
use crate::invitation::{InvitationKind, InvitationSecret};
use crate::trust::VerifiedPeer;

/// Bytes in the verifier; each byte is one word.
pub const VERIFIER_BYTES: usize = 4;
/// Bytes in a SPAKE2 share for the Ed25519 group: side byte and point.
pub const SHARE_BYTES: usize = 33;

/// Label for the TLS exporter value bound into pairing.
pub const EXPORTER_LABEL: &[u8] = b"hyakume v1 pairing exporter";

const SPAKE_JOINER: &[u8] = b"hyakume v1 pairing joiner";
const SPAKE_INVITER: &[u8] = b"hyakume v1 pairing inviter";
const CTX_COMMIT: &str = "hyakume v1 pairing commitment";
const CTX_ID_COMMIT: &str = "hyakume v1 pairing identity commitment";
const CTX_TRANSCRIPT: &str = "hyakume v1 pairing transcript";
const CTX_CONFIRM_INVITER: &str = "hyakume v1 pairing confirm inviter";
const CTX_CONFIRM_JOINER: &str = "hyakume v1 pairing confirm joiner";
const CTX_DECIDE_INVITER: &str = "hyakume v1 pairing decision inviter";
const CTX_DECIDE_JOINER: &str = "hyakume v1 pairing decision joiner";
const CTX_VERIFIER: &str = "hyakume v1 pairing verifier";
const SIG_INVITER: &[u8] = b"hyakume v1 pairing identity inviter";
const SIG_JOINER: &[u8] = b"hyakume v1 pairing identity joiner";

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PairingError {
    #[error("the other device sent an invalid pairing share")]
    BadShare,
    #[error("the other device changed values it had committed to")]
    CommitMismatch,
    #[error("the secrets differ or the connection was intercepted")]
    ConfirmationFailed,
    #[error("the other device sent an invalid identity key")]
    BadIdentity,
    #[error("the other device could not prove its identity")]
    BadSignature,
    #[error("the other device's decision could not be authenticated")]
    BadDecision,
    #[error("pairing was declined")]
    Declined,
    #[error("this step does not apply to this kind of invitation")]
    WrongKind,
}

/// The four bytes both people compare, shown as words.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Verifier([u8; VERIFIER_BYTES]);

impl Verifier {
    pub const fn from_bytes(bytes: [u8; VERIFIER_BYTES]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; VERIFIER_BYTES] {
        &self.0
    }

    pub fn words(&self) -> String {
        crate::words::render(&self.0)
    }
}

/// Values both sides learn from the authenticated connection and the
/// version negotiation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelBinding {
    /// TLS exporter output for [`EXPORTER_LABEL`].
    pub exporter: [u8; 32],
    pub inviter_endpoint: [u8; 32],
    pub joiner_endpoint: [u8; 32],
    /// Hash of both version messages exactly as sent.
    pub negotiation: [u8; 32],
}

/// What this exchange is for. Both sides must agree on every field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExchangeContext {
    pub binding: ChannelBinding,
    pub kind: InvitationKind,
    pub invitation: InvitationId,
    pub claim: ClaimId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit(pub [u8; 32]);

/// One side's public contribution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Contribution {
    pub share: [u8; SHARE_BYTES],
    pub id_commit: [u8; 32],
    pub nonce: [u8; 32],
    /// The device name this side declares. Presentation only.
    pub name: PeerText,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Confirm {
    pub mac: [u8; 32],
}

/// A device key, the salt that opens its commitment, and a signature over
/// the transcript. Sent only after both confirmation MACs checked out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdentityProof {
    pub id: [u8; 32],
    pub salt: [u8; 32],
    pub sig: [u8; 64],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decision {
    pub accept: bool,
    pub mac: [u8; 32],
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Role {
    Inviter,
    Joiner,
}

impl Role {
    fn other(self) -> Self {
        match self {
            Self::Inviter => Self::Joiner,
            Self::Joiner => Self::Inviter,
        }
    }
}

fn field(h: &mut blake3::Hasher, bytes: &[u8]) {
    h.update(&(bytes.len() as u64).to_be_bytes());
    h.update(bytes);
}

impl Contribution {
    fn hash_into(&self, h: &mut blake3::Hasher) {
        field(h, &self.share);
        field(h, &self.id_commit);
        field(h, &self.nonce);
        field(h, self.name.as_bytes());
    }

    fn commitment(&self) -> blake3::Hash {
        let mut h = blake3::Hasher::new_derive_key(CTX_COMMIT);
        self.hash_into(&mut h);
        h.finalize()
    }
}

fn identity_commitment(id: &[u8; 32], salt: &[u8; 32]) -> blake3::Hash {
    let mut h = blake3::Hasher::new_derive_key(CTX_ID_COMMIT);
    h.update(id);
    h.update(salt);
    h.finalize()
}

fn spake_password(secret: &InvitationSecret) -> Password {
    Password::new(secret.expose())
}

fn spake_ids() -> (Identity, Identity) {
    (Identity::new(SPAKE_JOINER), Identity::new(SPAKE_INVITER))
}

/// The opening of this side's identity commitment, held until it may be
/// revealed.
struct Opening {
    id: [u8; 32],
    salt: [u8; 32],
}

fn new_contribution(
    share: Vec<u8>,
    me: &DeviceKey,
    name: &PeerText,
    rng: &mut (impl RngCore + CryptoRng),
) -> Result<(Contribution, Opening), PairingError> {
    let share: [u8; SHARE_BYTES] = share.try_into().map_err(|_| PairingError::BadShare)?;
    let mut nonce = [0u8; 32];
    rng.fill_bytes(&mut nonce);
    let mut salt = [0u8; 32];
    rng.fill_bytes(&mut salt);
    let id = *me.id().as_bytes();
    Ok((
        Contribution {
            share,
            id_commit: *identity_commitment(&id, &salt).as_bytes(),
            nonce,
            name: name.clone(),
        },
        Opening { id, salt },
    ))
}

/// Keys and outputs both sides derive after the PAKE.
struct Schedule {
    transcript: [u8; 32],
    confirm_inviter: Zeroizing<[u8; 32]>,
    confirm_joiner: Zeroizing<[u8; 32]>,
    decide_inviter: Zeroizing<[u8; 32]>,
    decide_joiner: Zeroizing<[u8; 32]>,
    verifier: Verifier,
}

impl Schedule {
    fn new(
        key: &[u8],
        cx: &ExchangeContext,
        commit: &[u8; 32],
        inviter: &Contribution,
        joiner: &Contribution,
    ) -> Self {
        let mut h = blake3::Hasher::new_derive_key(CTX_TRANSCRIPT);
        field(&mut h, &cx.binding.exporter);
        field(&mut h, &cx.binding.inviter_endpoint);
        field(&mut h, &cx.binding.joiner_endpoint);
        field(&mut h, &cx.binding.negotiation);
        field(
            &mut h,
            match cx.kind {
                InvitationKind::Permanent => b"permanent",
                InvitationKind::Temporary => b"temporary",
            },
        );
        field(&mut h, cx.invitation.as_bytes());
        field(&mut h, cx.claim.as_bytes());
        field(&mut h, commit);
        inviter.hash_into(&mut h);
        joiner.hash_into(&mut h);
        let transcript = *h.finalize().as_bytes();
        let derive = |ctx: &str| {
            let mut h = blake3::Hasher::new_derive_key(ctx);
            h.update(key);
            h.update(&transcript);
            Zeroizing::new(*h.finalize().as_bytes())
        };
        let verifier_bytes = derive(CTX_VERIFIER);
        let mut verifier = [0u8; VERIFIER_BYTES];
        verifier.copy_from_slice(&verifier_bytes[..VERIFIER_BYTES]);
        Self {
            transcript,
            confirm_inviter: derive(CTX_CONFIRM_INVITER),
            confirm_joiner: derive(CTX_CONFIRM_JOINER),
            decide_inviter: derive(CTX_DECIDE_INVITER),
            decide_joiner: derive(CTX_DECIDE_JOINER),
            verifier: Verifier(verifier),
        }
    }

    fn confirm_key(&self, role: Role) -> &[u8; 32] {
        match role {
            Role::Inviter => &self.confirm_inviter,
            Role::Joiner => &self.confirm_joiner,
        }
    }

    fn confirm(&self, role: Role) -> Confirm {
        Confirm {
            mac: *blake3::keyed_hash(self.confirm_key(role), &self.transcript).as_bytes(),
        }
    }

    fn check_confirm(&self, msg: &Confirm, from: Role) -> Result<(), PairingError> {
        // blake3::Hash equality runs in constant time.
        if blake3::keyed_hash(self.confirm_key(from), &self.transcript)
            != blake3::Hash::from(msg.mac)
        {
            return Err(PairingError::ConfirmationFailed);
        }
        Ok(())
    }

    fn signature_domain(role: Role) -> &'static [u8] {
        match role {
            Role::Inviter => SIG_INVITER,
            Role::Joiner => SIG_JOINER,
        }
    }

    fn prove(&self, me: &DeviceKey, opening: &Opening, role: Role) -> IdentityProof {
        IdentityProof {
            id: opening.id,
            salt: opening.salt,
            sig: me.sign(Self::signature_domain(role), &self.transcript),
        }
    }

    /// Checks that the proof opens the committed identity and signs this
    /// transcript, and returns the proven device.
    fn check_identity(
        &self,
        proof: &IdentityProof,
        committed: &[u8; 32],
        from: Role,
    ) -> Result<DeviceId, PairingError> {
        if identity_commitment(&proof.id, &proof.salt) != blake3::Hash::from(*committed) {
            return Err(PairingError::CommitMismatch);
        }
        let peer = DeviceId::from_bytes(&proof.id).ok_or(PairingError::BadIdentity)?;
        if !peer.verify(Self::signature_domain(from), &self.transcript, &proof.sig) {
            return Err(PairingError::BadSignature);
        }
        Ok(peer)
    }

    fn decision_mac(&self, role: Role, accept: bool) -> blake3::Hash {
        let key = match role {
            Role::Inviter => &self.decide_inviter,
            Role::Joiner => &self.decide_joiner,
        };
        let mut h = blake3::Hasher::new_keyed(key);
        h.update(&self.transcript);
        h.update(&[u8::from(accept)]);
        h.finalize()
    }
}

fn finish_pake(
    state: Spake2<Ed25519Group>,
    peer_share: &[u8],
) -> Result<Zeroizing<Vec<u8>>, PairingError> {
    // spake2 0.4.0 takes any curve point. A point with a small-order part
    // could leak a few bits of this side's secret scalar to the sender, so
    // only points in the prime-order group are taken.
    let in_group = peer_share
        .get(1..)
        .and_then(|point| <[u8; 32]>::try_from(point).ok())
        .and_then(|point| CompressedEdwardsY(point).decompress())
        .is_some_and(|point| point.is_torsion_free());
    if !in_group {
        return Err(PairingError::BadShare);
    }
    state
        .finish(peer_share)
        .map(Zeroizing::new)
        .map_err(|_| PairingError::BadShare)
}

/// State after the PAKE and before identities are exchanged.
struct Confirmed {
    schedule: Schedule,
    kind: InvitationKind,
    peer_commit: [u8; 32],
    peer_name: PeerText,
    opening: Opening,
}

/// The joiner waits for the inviter's share.
pub struct JoinerAwaitShare {
    spake: Spake2<Ed25519Group>,
    mine: Contribution,
    opening: Opening,
    commit: [u8; 32],
    cx: ExchangeContext,
}

/// Starts the joiner's side and returns the commitment to send.
pub fn joiner_start(
    secret: &InvitationSecret,
    me: &DeviceKey,
    name: &PeerText,
    cx: ExchangeContext,
    rng: &mut (impl RngCore + CryptoRng),
) -> Result<(JoinerAwaitShare, Commit), PairingError> {
    let (a, b) = spake_ids();
    let (spake, share) =
        Spake2::<Ed25519Group>::start_a_with_rng(&spake_password(secret), &a, &b, &mut *rng);
    let (mine, opening) = new_contribution(share, me, name, rng)?;
    let commit = *mine.commitment().as_bytes();
    Ok((
        JoinerAwaitShare {
            spake,
            mine,
            opening,
            commit,
            cx,
        },
        Commit(commit),
    ))
}

pub struct JoinerAwaitConfirm(Confirmed);
pub struct JoinerAwaitIdentity(Confirmed);

impl JoinerAwaitShare {
    /// Takes the inviter's share and returns the reveal to send.
    pub fn on_share(
        self,
        share: Contribution,
    ) -> Result<(JoinerAwaitConfirm, Contribution), PairingError> {
        let key = finish_pake(self.spake, &share.share)?;
        let schedule = Schedule::new(&key, &self.cx, &self.commit, &share, &self.mine);
        Ok((
            JoinerAwaitConfirm(Confirmed {
                schedule,
                kind: self.cx.kind,
                peer_commit: share.id_commit,
                peer_name: share.name,
                opening: self.opening,
            }),
            self.mine,
        ))
    }
}

impl JoinerAwaitConfirm {
    /// Checks the inviter's MAC; only then does the joiner confirm in turn.
    pub fn on_confirm(self, msg: &Confirm) -> Result<(JoinerAwaitIdentity, Confirm), PairingError> {
        self.0.schedule.check_confirm(msg, Role::Inviter)?;
        let reply = self.0.schedule.confirm(Role::Joiner);
        Ok((JoinerAwaitIdentity(self.0), reply))
    }
}

impl JoinerAwaitIdentity {
    pub fn on_identity(
        self,
        proof: &IdentityProof,
        me: &DeviceKey,
    ) -> Result<(Verified, IdentityProof), PairingError> {
        let c = self.0;
        let peer = c
            .schedule
            .check_identity(proof, &c.peer_commit, Role::Inviter)?;
        let mine = c.schedule.prove(me, &c.opening, Role::Joiner);
        Ok((
            Verified {
                schedule: c.schedule,
                role: Role::Joiner,
                kind: c.kind,
                peer,
                peer_name: c.peer_name,
            },
            mine,
        ))
    }
}

/// The inviter waits for the joiner's reveal.
pub struct InviterAwaitReveal {
    spake: Spake2<Ed25519Group>,
    mine: Contribution,
    opening: Opening,
    commit: [u8; 32],
    cx: ExchangeContext,
}

/// Starts the inviter's side on the joiner's commitment and returns the
/// share to send.
pub fn inviter_on_commit(
    secret: &InvitationSecret,
    me: &DeviceKey,
    name: &PeerText,
    cx: ExchangeContext,
    commit: Commit,
    rng: &mut (impl RngCore + CryptoRng),
) -> Result<(InviterAwaitReveal, Contribution), PairingError> {
    let (a, b) = spake_ids();
    let (spake, share) =
        Spake2::<Ed25519Group>::start_b_with_rng(&spake_password(secret), &a, &b, &mut *rng);
    let (mine, opening) = new_contribution(share, me, name, rng)?;
    let reply = mine.clone();
    Ok((
        InviterAwaitReveal {
            spake,
            mine,
            opening,
            commit: commit.0,
            cx,
        },
        reply,
    ))
}

pub struct InviterAwaitConfirm(Confirmed);
pub struct InviterAwaitIdentity(Confirmed);

impl InviterAwaitReveal {
    pub fn on_reveal(
        self,
        reveal: Contribution,
    ) -> Result<(InviterAwaitConfirm, Confirm), PairingError> {
        if reveal.commitment() != blake3::Hash::from(self.commit) {
            return Err(PairingError::CommitMismatch);
        }
        let key = finish_pake(self.spake, &reveal.share)?;
        let schedule = Schedule::new(&key, &self.cx, &self.commit, &self.mine, &reveal);
        let confirm = schedule.confirm(Role::Inviter);
        Ok((
            InviterAwaitConfirm(Confirmed {
                schedule,
                kind: self.cx.kind,
                peer_commit: reveal.id_commit,
                peer_name: reveal.name,
                opening: self.opening,
            }),
            confirm,
        ))
    }
}

impl InviterAwaitConfirm {
    /// Checks the joiner's MAC. Only a joiner that proved knowledge of the
    /// secret receives the inviter's identity.
    pub fn on_confirm(
        self,
        msg: &Confirm,
        me: &DeviceKey,
    ) -> Result<(InviterAwaitIdentity, IdentityProof), PairingError> {
        self.0.schedule.check_confirm(msg, Role::Joiner)?;
        let proof = self.0.schedule.prove(me, &self.0.opening, Role::Inviter);
        Ok((InviterAwaitIdentity(self.0), proof))
    }
}

impl InviterAwaitIdentity {
    pub fn on_identity(self, proof: &IdentityProof) -> Result<Verified, PairingError> {
        let c = self.0;
        let peer = c
            .schedule
            .check_identity(proof, &c.peer_commit, Role::Joiner)?;
        Ok(Verified {
            schedule: c.schedule,
            role: Role::Inviter,
            kind: c.kind,
            peer,
            peer_name: c.peer_name,
        })
    }
}

/// The exchange succeeded cryptographically. Trust still needs both people
/// to confirm the verifier and both authenticated decisions to be yes.
pub struct Verified {
    schedule: Schedule,
    role: Role,
    kind: InvitationKind,
    peer: DeviceId,
    peer_name: PeerText,
}

/// A temporary recipient or sender proven by one claim. It has no path
/// into the trust store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemporaryPeer {
    pub device: DeviceId,
    pub declared_name: PeerText,
}

impl Verified {
    pub fn peer(&self) -> DeviceId {
        self.peer
    }

    /// The other device's declared name. Presentation only.
    pub fn peer_name(&self) -> &PeerText {
        &self.peer_name
    }

    pub fn verifier(&self) -> Verifier {
        self.schedule.verifier
    }

    pub fn kind(&self) -> InvitationKind {
        self.kind
    }

    /// This side's authenticated answer.
    pub fn decide(&self, accept: bool) -> Decision {
        Decision {
            accept,
            mac: *self.schedule.decision_mac(self.role, accept).as_bytes(),
        }
    }

    /// Checks the other side's answer and returns it.
    pub fn peer_decision(&self, msg: &Decision) -> Result<bool, PairingError> {
        if self.schedule.decision_mac(self.role.other(), msg.accept) != blake3::Hash::from(msg.mac)
        {
            return Err(PairingError::BadDecision);
        }
        Ok(msg.accept)
    }

    /// Ends a permanent exchange. Trust may be saved only from the returned
    /// value, and only when both answers were yes.
    pub fn conclude_permanent(
        self,
        mine: bool,
        theirs: &Decision,
    ) -> Result<VerifiedPeer, PairingError> {
        if self.kind != InvitationKind::Permanent {
            return Err(PairingError::WrongKind);
        }
        let theirs = self.peer_decision(theirs)?;
        if !(mine && theirs) {
            return Err(PairingError::Declined);
        }
        Ok(VerifiedPeer::new(self.peer, self.peer_name))
    }

    /// Ends a temporary exchange when both answers were yes.
    pub fn conclude_temporary(
        self,
        mine: bool,
        theirs: &Decision,
    ) -> Result<TemporaryPeer, PairingError> {
        if self.kind != InvitationKind::Temporary {
            return Err(PairingError::WrongKind);
        }
        let theirs = self.peer_decision(theirs)?;
        if !(mine && theirs) {
            return Err(PairingError::Declined);
        }
        Ok(TemporaryPeer {
            device: self.peer,
            declared_name: self.peer_name,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand_chacha::ChaCha20Rng;
    use rand_core::SeedableRng;

    fn rng(seed: u64) -> ChaCha20Rng {
        ChaCha20Rng::seed_from_u64(seed)
    }

    fn key(seed: u8) -> DeviceKey {
        DeviceKey::from_seed(&Zeroizing::new([seed; 32]))
    }

    fn secret(seed: u8) -> InvitationSecret {
        InvitationSecret::from_bytes([seed; 32])
    }

    fn name(n: &str) -> PeerText {
        PeerText::new(n.into(), 64).unwrap()
    }

    fn cx(exporter: u8, kind: InvitationKind, claim: u8) -> ExchangeContext {
        ExchangeContext {
            binding: ChannelBinding {
                exporter: [exporter; 32],
                inviter_endpoint: [1; 32],
                joiner_endpoint: [2; 32],
                negotiation: [3; 32],
            },
            kind,
            invitation: InvitationId::from_bytes([4; 16]),
            claim: ClaimId::from_bytes([claim; 16]),
        }
    }

    fn permanent(exporter: u8) -> ExchangeContext {
        cx(exporter, InvitationKind::Permanent, 5)
    }

    struct Outcome {
        inviter: Result<Verified, PairingError>,
        joiner: Result<Verified, PairingError>,
        inviter_identity_sent: bool,
    }

    /// Runs one exchange and stops at the first failure.
    fn run(
        inviter: (&InvitationSecret, &DeviceKey, ExchangeContext),
        joiner: (&InvitationSecret, &DeviceKey, ExchangeContext),
        seed: u64,
    ) -> Outcome {
        let fail = |e: PairingError, sent| Outcome {
            inviter: Err(e),
            joiner: Err(e),
            inviter_identity_sent: sent,
        };
        let mut r = rng(seed);
        let (j1, commit) =
            joiner_start(joiner.0, joiner.1, &name("joiner"), joiner.2, &mut r).unwrap();
        let (i1, share) = inviter_on_commit(
            inviter.0,
            inviter.1,
            &name("inviter"),
            inviter.2,
            commit,
            &mut r,
        )
        .unwrap();
        let (j2, reveal) = match j1.on_share(share) {
            Ok(v) => v,
            Err(e) => return fail(e, false),
        };
        let (i2, confirm_i) = match i1.on_reveal(reveal) {
            Ok(v) => v,
            Err(e) => return fail(e, false),
        };
        let (j3, confirm_j) = match j2.on_confirm(&confirm_i) {
            Ok(v) => v,
            Err(e) => return fail(e, false),
        };
        let (i3, proof_i) = match i2.on_confirm(&confirm_j, inviter.1) {
            Ok(v) => v,
            Err(e) => return fail(e, false),
        };
        let (joined, proof_j) = match j3.on_identity(&proof_i, joiner.1) {
            Ok(v) => v,
            Err(e) => return fail(e, true),
        };
        Outcome {
            inviter: i3.on_identity(&proof_j),
            joiner: Ok(joined),
            inviter_identity_sent: true,
        }
    }

    #[test]
    fn a_share_with_a_small_order_part_is_refused() {
        let secret = InvitationSecret::from_bytes([5; 32]);
        let (joiner, inviter) = spake_ids();
        let start = |seed| {
            Spake2::<Ed25519Group>::start_a_with_rng(
                &spake_password(&secret),
                &joiner,
                &inviter,
                &mut rng(seed),
            )
        };
        let (_, honest) = Spake2::<Ed25519Group>::start_b_with_rng(
            &spake_password(&secret),
            &joiner,
            &inviter,
            &mut rng(2),
        );
        let point = CompressedEdwardsY(honest[1..].try_into().unwrap())
            .decompress()
            .unwrap();
        let tainted = point + curve25519_dalek::constants::EIGHT_TORSION[1];
        let mut bad = honest.clone();
        bad[1..].copy_from_slice(tainted.compress().as_bytes());

        assert!(finish_pake(start(1).0, &honest).is_ok());
        assert!(matches!(
            finish_pake(start(1).0, &bad),
            Err(PairingError::BadShare)
        ));
    }

    #[test]
    fn the_same_secret_and_connection_give_both_sides_the_same_verifier() {
        let (inv, joi, s) = (key(10), key(20), secret(1));
        let out = run((&s, &inv, permanent(7)), (&s, &joi, permanent(7)), 2);
        let (i, j) = (out.inviter.unwrap(), out.joiner.unwrap());
        assert_eq!(i.verifier(), j.verifier());
        assert_eq!(i.peer(), joi.id());
        assert_eq!(j.peer(), inv.id());
        assert_eq!(i.peer_name().as_bytes(), b"joiner");
    }

    #[test]
    fn a_wrong_secret_fails_before_any_identity_is_revealed() {
        let (inv, joi) = (key(10), key(20));
        let out = run(
            (&secret(1), &inv, permanent(7)),
            (&secret(2), &joi, permanent(7)),
            2,
        );
        assert_eq!(out.joiner.err(), Some(PairingError::ConfirmationFailed));
        assert!(!out.inviter_identity_sent);
    }

    #[test]
    fn identities_are_absent_from_every_message_before_both_confirmations() {
        use crate::protocol::{Message, encode_payload};
        let (inv, joi, s) = (key(10), key(20), secret(1));
        let mut r = rng(3);
        let (j1, commit) = joiner_start(&s, &joi, &name("j"), permanent(7), &mut r).unwrap();
        let (i1, share) =
            inviter_on_commit(&s, &inv, &name("i"), permanent(7), commit.clone(), &mut r).unwrap();
        let (j2, reveal) = j1.on_share(share.clone()).unwrap();
        let (_, confirm) = i1.on_reveal(reveal.clone()).unwrap();
        let (_, answer) = j2.on_confirm(&confirm).unwrap();
        // The messages as they go on the wire, so a field added to any of
        // them is checked too. Declared names travel with the shares by
        // design; device keys must not.
        let claim = Message::Claim {
            invitation: InvitationId::from_bytes([4; 16]),
            claim: ClaimId::from_bytes([5; 16]),
            kind: InvitationKind::Permanent,
        };
        let sent = [
            claim,
            Message::PairCommit(commit),
            Message::PairShare(share),
            Message::PairReveal(reveal),
            Message::PairConfirm(confirm),
            Message::PairConfirm(answer),
        ]
        .map(|m| encode_payload(&m));
        for bytes in &sent {
            for id in [inv.id(), joi.id()] {
                assert!(!bytes.windows(32).any(|w| w == id.as_bytes()));
            }
        }
    }

    #[test]
    fn different_connections_fail_even_with_the_right_secret() {
        let s = secret(1);
        let out = run(
            (&s, &key(10), permanent(7)),
            (&s, &key(20), permanent(8)),
            2,
        );
        assert_eq!(out.joiner.err(), Some(PairingError::ConfirmationFailed));
    }

    #[test]
    fn disagreement_on_invitation_kind_or_claim_fails_the_exchange() {
        let s = secret(1);
        let temp = cx(7, InvitationKind::Temporary, 5);
        let out = run((&s, &key(10), permanent(7)), (&s, &key(20), temp), 2);
        assert_eq!(out.joiner.err(), Some(PairingError::ConfirmationFailed));
        let other_claim = cx(7, InvitationKind::Permanent, 6);
        let out = run((&s, &key(10), permanent(7)), (&s, &key(20), other_claim), 2);
        assert_eq!(out.joiner.err(), Some(PairingError::ConfirmationFailed));
    }

    #[test]
    fn tampered_version_negotiation_fails_the_exchange() {
        let s = secret(1);
        let mut downgraded = permanent(7);
        downgraded.binding.negotiation = [0xee; 32];
        let out = run((&s, &key(10), permanent(7)), (&s, &key(20), downgraded), 2);
        assert_eq!(out.joiner.err(), Some(PairingError::ConfirmationFailed));
    }

    #[test]
    fn an_interceptor_with_the_secret_produces_different_verifiers_on_each_side() {
        let s = secret(1);
        let (alice, bob, mallory) = (key(10), key(20), key(66));
        let to_alice = run((&s, &alice, permanent(1)), (&s, &mallory, permanent(1)), 3);
        let to_bob = run((&s, &mallory, permanent(2)), (&s, &bob, permanent(2)), 4);
        let alice_view = to_alice.inviter.unwrap();
        let bob_view = to_bob.joiner.unwrap();
        assert_ne!(alice_view.verifier(), bob_view.verifier());
        assert_eq!(alice_view.peer(), mallory.id());
        assert_eq!(bob_view.peer(), mallory.id());
    }

    #[test]
    fn substituting_the_committed_identity_is_detected() {
        let (inv, joi, mallory, s) = (key(10), key(20), key(66), secret(1));
        let mut r = rng(5);
        let (j1, commit) = joiner_start(&s, &joi, &name("j"), permanent(7), &mut r).unwrap();
        let (i1, share) =
            inviter_on_commit(&s, &inv, &name("i"), permanent(7), commit, &mut r).unwrap();
        let (j2, reveal) = j1.on_share(share).unwrap();
        let (i2, confirm_i) = i1.on_reveal(reveal).unwrap();
        let (j3, confirm_j) = j2.on_confirm(&confirm_i).unwrap();
        let (i3, proof_i) = i2.on_confirm(&confirm_j, &inv).unwrap();
        let (_, proof_j) = j3.on_identity(&proof_i, &joi).unwrap();
        let forged = IdentityProof {
            id: *mallory.id().as_bytes(),
            salt: proof_j.salt,
            sig: mallory.sign(SIG_JOINER, &[0u8; 32]),
        };
        assert_eq!(
            i3.on_identity(&forged).err(),
            Some(PairingError::CommitMismatch)
        );
    }

    #[test]
    fn an_identity_signature_is_bound_to_its_role() {
        let (inv, joi, s) = (key(10), key(20), secret(1));
        let mut r = rng(6);
        let (j1, commit) = joiner_start(&s, &joi, &name("j"), permanent(7), &mut r).unwrap();
        let (i1, share) =
            inviter_on_commit(&s, &inv, &name("i"), permanent(7), commit, &mut r).unwrap();
        let (j2, reveal) = j1.on_share(share).unwrap();
        let (i2, confirm_i) = i1.on_reveal(reveal).unwrap();
        let (j3, confirm_j) = j2.on_confirm(&confirm_i).unwrap();
        let (_, proof_i) = i2.on_confirm(&confirm_j, &inv).unwrap();
        // The inviter's own key over this exchange's transcript, signed as
        // the joiner would sign. Only the role is wrong.
        let mut wrong_role = proof_i.clone();
        wrong_role.sig = inv.sign(SIG_JOINER, &j3.0.schedule.transcript);
        assert!(matches!(
            j3.on_identity(&wrong_role, &joi),
            Err(PairingError::BadSignature)
        ));
    }

    #[test]
    fn the_joiner_cannot_change_its_contribution_after_seeing_the_inviters() {
        let (inv, joi, s) = (key(10), key(20), secret(1));
        let mut r = rng(8);
        let (j1, commit) = joiner_start(&s, &joi, &name("j"), permanent(7), &mut r).unwrap();
        let (i1, share) =
            inviter_on_commit(&s, &inv, &name("i"), permanent(7), commit, &mut r).unwrap();
        let (_, mut reveal) = j1.on_share(share).unwrap();
        reveal.nonce[0] ^= 1;
        assert_eq!(
            i1.on_reveal(reveal).err(),
            Some(PairingError::CommitMismatch)
        );
    }

    #[test]
    fn each_temporary_claim_has_its_own_verifier() {
        let (inv, s) = (key(10), secret(1));
        let first = run(
            (&s, &inv, cx(1, InvitationKind::Temporary, 1)),
            (&s, &key(20), cx(1, InvitationKind::Temporary, 1)),
            6,
        );
        let second = run(
            (&s, &inv, cx(2, InvitationKind::Temporary, 2)),
            (&s, &key(21), cx(2, InvitationKind::Temporary, 2)),
            7,
        );
        assert_ne!(
            first.inviter.unwrap().verifier(),
            second.inviter.unwrap().verifier()
        );
    }

    #[test]
    fn a_decision_cannot_be_forged_flipped_or_reflected() {
        let s = secret(1);
        let out = run(
            (&s, &key(10), permanent(7)),
            (&s, &key(20), permanent(7)),
            2,
        );
        let (i, j) = (out.inviter.unwrap(), out.joiner.unwrap());
        assert_eq!(i.peer_decision(&j.decide(false)), Ok(false));
        assert_eq!(i.peer_decision(&j.decide(true)), Ok(true));
        let mut flipped = j.decide(false);
        flipped.accept = true;
        assert_eq!(i.peer_decision(&flipped), Err(PairingError::BadDecision));
        assert_eq!(
            i.peer_decision(&i.decide(true)),
            Err(PairingError::BadDecision)
        );
    }

    #[test]
    fn trust_needs_both_answers_to_be_yes() {
        let s = secret(1);
        let out = run(
            (&s, &key(10), permanent(7)),
            (&s, &key(20), permanent(7)),
            2,
        );
        let (i, j) = (out.inviter.unwrap(), out.joiner.unwrap());
        let no = j.decide(false);
        assert_eq!(
            i.conclude_permanent(true, &no).err(),
            Some(PairingError::Declined)
        );
        let out = run(
            (&s, &key(10), permanent(7)),
            (&s, &key(20), permanent(7)),
            2,
        );
        let (i, j) = (out.inviter.unwrap(), out.joiner.unwrap());
        assert_eq!(
            i.conclude_permanent(false, &j.decide(true)).err(),
            Some(PairingError::Declined)
        );
    }

    #[test]
    fn a_temporary_claim_cannot_conclude_as_permanent_trust() {
        let s = secret(1);
        let temp = cx(7, InvitationKind::Temporary, 5);
        let out = run((&s, &key(10), temp.clone()), (&s, &key(20), temp), 2);
        let (i, j) = (out.inviter.unwrap(), out.joiner.unwrap());
        assert_eq!(
            i.conclude_permanent(true, &j.decide(true)).err(),
            Some(PairingError::WrongKind)
        );
    }
}
