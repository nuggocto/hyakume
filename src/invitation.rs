//! Invitations, claims, and the invitation token.
//!
//! An invitation lets other devices start a pairing exchange. Each attempt
//! is a claim. Every claim ends in exactly one way, and an invitation is
//! used at most once:
//!
//! ```text
//! Open ──permanent claim confirmed by both people──▶ Consumed
//!   │ ──temporary claim approved by the sender───────▶ Consumed (other claims refused)
//!   │ ──verifier declined or mismatched──────────────▶ Declined
//!   │ ──too many claims that did not verify──────────▶ Exhausted
//!   │ ──lifetime over────────────────────────────────▶ Expired
//!   └ ──inviter cancels──────────────────────────────▶ Cancelled
//! ```
//!
//! Expiry counts only forward movement of the clock. A clock that moves
//! backward adds no time, so it cannot extend an invitation's life.
//!
//! A temporary claim never becomes trust. [`Invitation::approve`] returns a
//! [`TemporaryGrant`], which has no path into the trust store.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use rand_core::{CryptoRng, RngCore};
use zeroize::Zeroizing;

use crate::clock::Moment;
use crate::ids::{ClaimId, InvitationId};
use crate::limits::{MAX_ATTEMPTS, MAX_CLAIMANTS};
use crate::pairing::Verifier;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvitationKind {
    /// Pairs two devices permanently.
    Permanent,
    /// Admits one approved recipient for one transfer.
    Temporary,
}

/// The invitation's long random secret, used as the PAKE password.
pub struct InvitationSecret(Zeroizing<[u8; 32]>);

impl InvitationSecret {
    pub fn random(rng: &mut (impl RngCore + CryptoRng)) -> Self {
        let mut bytes = Zeroizing::new([0u8; 32]);
        rng.fill_bytes(&mut *bytes);
        Self(bytes)
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    pub(crate) fn expose(&self) -> &[u8; 32] {
        &self.0
    }
}

impl std::fmt::Debug for InvitationSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("InvitationSecret([redacted])")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvitationState {
    Open,
    Consumed,
    Declined,
    Exhausted,
    Expired,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaimState {
    /// The pairing exchange is running.
    Exchanging,
    /// The exchange proved the secret; the verifier is waiting for people.
    Verified(Verifier),
    /// This claim used the invitation.
    Used,
    Refused(ClaimRefusal),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaimRefusal {
    /// The exchange ended without proving the secret.
    NotVerified,
    /// Someone declined, or the verifiers did not match.
    Declined,
    /// Another claim used the invitation first.
    OtherClaimUsed,
    /// The invitation closed before this claim finished.
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InvitationError {
    #[error("the invitation has expired")]
    Expired,
    #[error("the invitation was already used")]
    Consumed,
    #[error("the invitation was declined and cannot be used again")]
    Declined,
    #[error("the invitation was closed after too many failed attempts")]
    Exhausted,
    #[error("the invitation was cancelled")]
    Cancelled,
    #[error("the invitation has too many claims")]
    TooManyClaims,
    #[error("that claim was already made")]
    DuplicateClaim,
    #[error("that claim does not exist")]
    UnknownClaim,
    #[error("that claim is not waiting for this step")]
    WrongClaimState,
    #[error("that step does not apply to this kind of invitation")]
    WrongKind,
}

impl InvitationError {
    fn closed(state: InvitationState) -> Self {
        match state {
            InvitationState::Consumed => Self::Consumed,
            InvitationState::Declined => Self::Declined,
            InvitationState::Exhausted => Self::Exhausted,
            InvitationState::Expired => Self::Expired,
            InvitationState::Cancelled | InvitationState::Open => Self::Cancelled,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Claim {
    id: ClaimId,
    state: ClaimState,
}

/// Proof that a permanent invitation was used by one confirmed claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PermanentUse {
    pub invitation: InvitationId,
    pub claim: ClaimId,
}

/// Admission of one temporary recipient. It authorizes one transfer and
/// can never become trust.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TemporaryGrant {
    pub invitation: InvitationId,
    pub claim: ClaimId,
}

pub struct Invitation {
    id: InvitationId,
    kind: InvitationKind,
    secret: InvitationSecret,
    lifetime_ms: u64,
    /// Forward clock movement observed since creation.
    elapsed_ms: u64,
    last_seen: Moment,
    attempts_left: u8,
    state: InvitationState,
    claims: Vec<Claim>,
}

impl Invitation {
    pub fn new(
        id: InvitationId,
        kind: InvitationKind,
        secret: InvitationSecret,
        now: Moment,
        lifetime: Duration,
    ) -> Self {
        Self {
            id,
            kind,
            secret,
            lifetime_ms: u64::try_from(lifetime.as_millis()).unwrap_or(u64::MAX),
            elapsed_ms: 0,
            last_seen: now,
            attempts_left: MAX_ATTEMPTS,
            state: InvitationState::Open,
            claims: Vec::new(),
        }
    }

    pub fn id(&self) -> InvitationId {
        self.id
    }

    pub fn kind(&self) -> InvitationKind {
        self.kind
    }

    pub fn secret(&self) -> &InvitationSecret {
        &self.secret
    }

    /// The state after applying the passage of time up to `now`.
    pub fn state(&mut self, now: Moment) -> InvitationState {
        self.observe(now);
        self.state
    }

    pub fn claim_state(&self, claim: ClaimId) -> Option<ClaimState> {
        self.claims.iter().find(|c| c.id == claim).map(|c| c.state)
    }

    fn observe(&mut self, now: Moment) {
        let forward = now.as_millis().saturating_sub(self.last_seen.as_millis());
        self.elapsed_ms = self.elapsed_ms.saturating_add(forward);
        self.last_seen = now;
        if self.state == InvitationState::Open && self.elapsed_ms >= self.lifetime_ms {
            self.close(InvitationState::Expired);
        }
    }

    fn close(&mut self, state: InvitationState) {
        self.state = state;
        for c in &mut self.claims {
            if matches!(c.state, ClaimState::Exchanging | ClaimState::Verified(_)) {
                c.state = ClaimState::Refused(ClaimRefusal::Closed);
            }
        }
    }

    fn require_open(&mut self, now: Moment) -> Result<(), InvitationError> {
        self.observe(now);
        match self.state {
            InvitationState::Open => Ok(()),
            other => Err(InvitationError::closed(other)),
        }
    }

    fn claim_mut(&mut self, claim: ClaimId) -> Result<&mut Claim, InvitationError> {
        self.claims
            .iter_mut()
            .find(|c| c.id == claim)
            .ok_or(InvitationError::UnknownClaim)
    }

    /// Starts a claim when an exchange begins.
    pub fn open_claim(&mut self, now: Moment, claim: ClaimId) -> Result<(), InvitationError> {
        self.require_open(now)?;
        if self.claims.iter().any(|c| c.id == claim) {
            return Err(InvitationError::DuplicateClaim);
        }
        if self.claims.len() >= MAX_CLAIMANTS {
            return Err(InvitationError::TooManyClaims);
        }
        self.claims.push(Claim {
            id: claim,
            state: ClaimState::Exchanging,
        });
        Ok(())
    }

    /// Records that a claim's exchange proved the secret.
    pub fn claim_verified(
        &mut self,
        now: Moment,
        claim: ClaimId,
        verifier: Verifier,
    ) -> Result<(), InvitationError> {
        self.require_open(now)?;
        let c = self.claim_mut(claim)?;
        if c.state != ClaimState::Exchanging {
            return Err(InvitationError::WrongClaimState);
        }
        c.state = ClaimState::Verified(verifier);
        Ok(())
    }

    /// Ends a claim whose exchange did not prove the secret: a wrong secret,
    /// a broken exchange, a timeout, or a dropped connection. Each one uses
    /// an attempt, because each could be one online guess.
    pub fn claim_unverified(&mut self, now: Moment, claim: ClaimId) -> Result<(), InvitationError> {
        self.observe(now);
        let c = self.claim_mut(claim)?;
        if c.state != ClaimState::Exchanging {
            return Err(InvitationError::WrongClaimState);
        }
        c.state = ClaimState::Refused(ClaimRefusal::NotVerified);
        if self.state == InvitationState::Open {
            self.attempts_left = self.attempts_left.saturating_sub(1);
            if self.attempts_left == 0 {
                self.close(InvitationState::Exhausted);
            }
        }
        Ok(())
    }

    /// Someone declined, or the verifiers did not match. The invitation
    /// cannot be used again, whatever its kind.
    pub fn decline(&mut self, now: Moment, claim: ClaimId) -> Result<(), InvitationError> {
        self.observe(now);
        let c = self.claim_mut(claim)?;
        if !matches!(c.state, ClaimState::Exchanging | ClaimState::Verified(_)) {
            return Err(InvitationError::WrongClaimState);
        }
        c.state = ClaimState::Refused(ClaimRefusal::Declined);
        if self.state == InvitationState::Open {
            self.close(InvitationState::Declined);
        }
        Ok(())
    }

    fn use_claim(&mut self, now: Moment, claim: ClaimId) -> Result<(), InvitationError> {
        self.require_open(now)?;
        let c = self.claim_mut(claim)?;
        if !matches!(c.state, ClaimState::Verified(_)) {
            return Err(InvitationError::WrongClaimState);
        }
        c.state = ClaimState::Used;
        self.state = InvitationState::Consumed;
        for other in &mut self.claims {
            if other.id != claim
                && matches!(
                    other.state,
                    ClaimState::Exchanging | ClaimState::Verified(_)
                )
            {
                other.state = ClaimState::Refused(ClaimRefusal::OtherClaimUsed);
            }
        }
        Ok(())
    }

    /// Uses a permanent invitation once both people confirmed the verifier.
    pub fn confirm_permanent(
        &mut self,
        now: Moment,
        claim: ClaimId,
    ) -> Result<PermanentUse, InvitationError> {
        if self.kind != InvitationKind::Permanent {
            return Err(InvitationError::WrongKind);
        }
        self.use_claim(now, claim)?;
        Ok(PermanentUse {
            invitation: self.id,
            claim,
        })
    }

    /// The sender approves one temporary claimant. Every other claim is
    /// refused at the same moment.
    pub fn approve(
        &mut self,
        now: Moment,
        claim: ClaimId,
    ) -> Result<TemporaryGrant, InvitationError> {
        if self.kind != InvitationKind::Temporary {
            return Err(InvitationError::WrongKind);
        }
        self.use_claim(now, claim)?;
        Ok(TemporaryGrant {
            invitation: self.id,
            claim,
        })
    }

    pub fn cancel(&mut self, now: Moment) {
        self.observe(now);
        if self.state == InvitationState::Open {
            self.close(InvitationState::Cancelled);
        }
    }
}

/// Text that starts every native invitation.
pub const TOKEN_PREFIX: &str = "hyakume://invite#";
/// Version of the token encoding.
pub const TOKEN_VERSION: u8 = 1;
/// Addresses one token may carry.
pub const MAX_ROUTE_ADDRESSES: usize = 4;
const MAX_TOKEN_BYTES: usize = 1 + 1 + 16 + 32 + 32 + 1 + MAX_ROUTE_ADDRESSES * (1 + 16 + 2);
/// Longest token text, checked before decoding.
pub const MAX_TOKEN_CHARS: usize = TOKEN_PREFIX.len() + MAX_TOKEN_BYTES.div_ceil(5) * 8;

/// Where the inviting endpoint can be reached.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    /// The inviter's current transport endpoint ID. It changes every run.
    pub endpoint: [u8; 32],
    pub addresses: Vec<SocketAddr>,
}

/// Everything a joining device needs: which invitation, its secret, and
/// where to connect. Shown only through the explicit pairing action.
#[derive(Debug)]
pub struct InvitationToken {
    pub kind: InvitationKind,
    pub id: InvitationId,
    pub secret: InvitationSecret,
    pub route: Route,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TokenError {
    #[error("not a Hyakume invitation")]
    Prefix,
    #[error("the invitation is too long")]
    TooLong,
    #[error("the invitation is not valid base32")]
    Encoding,
    #[error("unsupported invitation version {0}")]
    Version(u8),
    #[error("unknown invitation kind")]
    Kind,
    #[error("the invitation is truncated")]
    Truncated,
    #[error("the invitation has unexpected trailing data")]
    Trailing,
    #[error("the invitation names an invalid endpoint")]
    Endpoint,
    #[error("the invitation has no address or too many")]
    Addresses,
    #[error("the invitation has an unknown address family")]
    AddressFamily,
}

impl InvitationToken {
    /// The binary form inside the text token.
    pub fn to_bytes(&self) -> Result<Zeroizing<Vec<u8>>, TokenError> {
        if self.route.addresses.is_empty() || self.route.addresses.len() > MAX_ROUTE_ADDRESSES {
            return Err(TokenError::Addresses);
        }
        let mut out = Zeroizing::new(Vec::with_capacity(MAX_TOKEN_BYTES));
        out.push(TOKEN_VERSION);
        out.push(match self.kind {
            InvitationKind::Permanent => 1,
            InvitationKind::Temporary => 2,
        });
        out.extend_from_slice(self.id.as_bytes());
        out.extend_from_slice(self.secret.expose());
        out.extend_from_slice(&self.route.endpoint);
        // Bounded by MAX_ROUTE_ADDRESSES above.
        out.push(self.route.addresses.len() as u8);
        for addr in &self.route.addresses {
            match addr.ip() {
                IpAddr::V4(ip) => {
                    out.push(4);
                    out.extend_from_slice(&ip.octets());
                }
                IpAddr::V6(ip) => {
                    out.push(6);
                    out.extend_from_slice(&ip.octets());
                }
            }
            out.extend_from_slice(&addr.port().to_be_bytes());
        }
        Ok(out)
    }

    /// The text form, `hyakume://invite#` followed by base32.
    pub fn to_text(&self) -> Result<Zeroizing<String>, TokenError> {
        let bytes = self.to_bytes()?;
        let mut text = Zeroizing::new(String::with_capacity(MAX_TOKEN_CHARS));
        text.push_str(TOKEN_PREFIX);
        text.push_str(
            &data_encoding::BASE32_NOPAD
                .encode(&bytes)
                .to_ascii_lowercase(),
        );
        Ok(text)
    }

    /// Parses the text form. The length is checked before decoding.
    pub fn parse(text: &str) -> Result<Self, TokenError> {
        if text.len() > MAX_TOKEN_CHARS {
            return Err(TokenError::TooLong);
        }
        let body = text.strip_prefix(TOKEN_PREFIX).ok_or(TokenError::Prefix)?;
        let bytes = Zeroizing::new(
            data_encoding::BASE32_NOPAD_NOCASE
                .decode(body.as_bytes())
                .map_err(|_| TokenError::Encoding)?,
        );
        Self::from_bytes(&bytes)
    }

    /// Parses the binary form strictly: every byte must be accounted for.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, TokenError> {
        let mut r = bytes;
        let version = take::<1>(&mut r)?[0];
        if version != TOKEN_VERSION {
            return Err(TokenError::Version(version));
        }
        let kind = match take::<1>(&mut r)?[0] {
            1 => InvitationKind::Permanent,
            2 => InvitationKind::Temporary,
            _ => return Err(TokenError::Kind),
        };
        let id = InvitationId::from_bytes(take::<16>(&mut r)?);
        let secret = InvitationSecret::from_bytes(take::<32>(&mut r)?);
        let endpoint = take::<32>(&mut r)?;
        if iroh::PublicKey::from_bytes(&endpoint).is_err() {
            return Err(TokenError::Endpoint);
        }
        let count = usize::from(take::<1>(&mut r)?[0]);
        if count == 0 || count > MAX_ROUTE_ADDRESSES {
            return Err(TokenError::Addresses);
        }
        let mut addresses = Vec::with_capacity(count);
        for _ in 0..count {
            let ip = match take::<1>(&mut r)?[0] {
                4 => IpAddr::V4(Ipv4Addr::from(take::<4>(&mut r)?)),
                6 => IpAddr::V6(Ipv6Addr::from(take::<16>(&mut r)?)),
                _ => return Err(TokenError::AddressFamily),
            };
            let port = u16::from_be_bytes(take::<2>(&mut r)?);
            addresses.push(SocketAddr::new(ip, port));
        }
        if !r.is_empty() {
            return Err(TokenError::Trailing);
        }
        Ok(Self {
            kind,
            id,
            secret,
            route: Route {
                endpoint,
                addresses,
            },
        })
    }
}

fn take<const N: usize>(r: &mut &[u8]) -> Result<[u8; N], TokenError> {
    let (head, rest) = r.split_first_chunk::<N>().ok_or(TokenError::Truncated)?;
    *r = rest;
    Ok(*head)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIFE: Duration = Duration::from_secs(60);

    fn at(ms: u64) -> Moment {
        Moment::from_millis(ms)
    }

    fn claim(n: u8) -> ClaimId {
        ClaimId::from_bytes([n; 16])
    }

    fn invitation(kind: InvitationKind) -> Invitation {
        Invitation::new(
            InvitationId::from_bytes([9; 16]),
            kind,
            InvitationSecret::from_bytes([7; 32]),
            at(0),
            LIFE,
        )
    }

    fn verified(inv: &mut Invitation, n: u8) {
        inv.open_claim(at(1), claim(n)).unwrap();
        inv.claim_verified(at(1), claim(n), Verifier::from_bytes([n; 4]))
            .unwrap();
    }

    #[test]
    fn expiry_takes_effect_exactly_at_the_end_of_the_lifetime() {
        let mut inv = invitation(InvitationKind::Permanent);
        assert!(inv.open_claim(at(59_999), claim(1)).is_ok());
        assert_eq!(
            inv.open_claim(at(60_000), claim(2)),
            Err(InvitationError::Expired)
        );
        assert_eq!(
            inv.claim_state(claim(1)),
            Some(ClaimState::Refused(ClaimRefusal::Closed))
        );
    }

    #[test]
    fn a_clock_moving_backward_cannot_extend_validity() {
        let mut inv = invitation(InvitationKind::Permanent);
        assert_eq!(inv.state(at(60_000)), InvitationState::Expired);
        assert_eq!(
            inv.open_claim(at(10), claim(1)),
            Err(InvitationError::Expired)
        );

        let mut later = invitation(InvitationKind::Permanent);
        later.state(at(59_000));
        // The clock jumps back by 50 s. Only one second of life remains.
        assert!(later.open_claim(at(9_000), claim(1)).is_ok());
        assert_eq!(
            later.open_claim(at(10_000), claim(2)),
            Err(InvitationError::Expired)
        );
    }

    #[test]
    fn a_permanent_invitation_is_used_once_and_refuses_other_claims() {
        let mut inv = invitation(InvitationKind::Permanent);
        verified(&mut inv, 1);
        verified(&mut inv, 2);
        assert!(inv.confirm_permanent(at(2), claim(1)).is_ok());
        assert_eq!(
            inv.claim_state(claim(2)),
            Some(ClaimState::Refused(ClaimRefusal::OtherClaimUsed))
        );
        assert_eq!(
            inv.confirm_permanent(at(2), claim(2)),
            Err(InvitationError::Consumed)
        );
        assert_eq!(
            inv.open_claim(at(3), claim(3)),
            Err(InvitationError::Consumed)
        );
    }

    #[test]
    fn approving_a_temporary_claimant_refuses_every_other_claim() {
        let mut inv = invitation(InvitationKind::Temporary);
        verified(&mut inv, 1);
        verified(&mut inv, 2);
        inv.open_claim(at(1), claim(3)).unwrap();
        let grant = inv.approve(at(2), claim(2)).unwrap();
        assert_eq!(grant.claim, claim(2));
        assert_eq!(
            inv.claim_state(claim(1)),
            Some(ClaimState::Refused(ClaimRefusal::OtherClaimUsed))
        );
        assert_eq!(
            inv.claim_state(claim(3)),
            Some(ClaimState::Refused(ClaimRefusal::OtherClaimUsed))
        );
        // The race is decided: the losing claimant cannot be approved later.
        assert_eq!(inv.approve(at(2), claim(1)), Err(InvitationError::Consumed));
    }

    #[test]
    fn a_claim_must_verify_before_it_can_use_the_invitation() {
        let mut inv = invitation(InvitationKind::Temporary);
        inv.open_claim(at(1), claim(1)).unwrap();
        assert_eq!(
            inv.approve(at(1), claim(1)),
            Err(InvitationError::WrongClaimState)
        );
        assert_eq!(
            inv.approve(at(1), claim(9)),
            Err(InvitationError::UnknownClaim)
        );
    }

    #[test]
    fn steps_for_the_other_kind_of_invitation_are_refused() {
        let mut temp = invitation(InvitationKind::Temporary);
        verified(&mut temp, 1);
        assert_eq!(
            temp.confirm_permanent(at(2), claim(1)),
            Err(InvitationError::WrongKind)
        );
        let mut perm = invitation(InvitationKind::Permanent);
        verified(&mut perm, 1);
        assert_eq!(
            perm.approve(at(2), claim(1)),
            Err(InvitationError::WrongKind)
        );
    }

    #[test]
    fn replayed_claims_are_refused() {
        let mut inv = invitation(InvitationKind::Permanent);
        inv.open_claim(at(1), claim(1)).unwrap();
        assert_eq!(
            inv.open_claim(at(1), claim(1)),
            Err(InvitationError::DuplicateClaim)
        );
        inv.claim_unverified(at(1), claim(1)).unwrap();
        assert_eq!(
            inv.open_claim(at(1), claim(1)),
            Err(InvitationError::DuplicateClaim)
        );
    }

    #[test]
    fn failed_attempts_close_the_invitation_at_the_limit() {
        let mut inv = invitation(InvitationKind::Permanent);
        for n in 0..MAX_ATTEMPTS - 1 {
            inv.open_claim(at(1), claim(n)).unwrap();
            inv.claim_unverified(at(1), claim(n)).unwrap();
        }
        verified(&mut inv, 100);
        inv.open_claim(at(1), claim(200)).unwrap();
        inv.claim_unverified(at(1), claim(200)).unwrap();
        assert_eq!(inv.state(at(1)), InvitationState::Exhausted);
        assert_eq!(
            inv.claim_state(claim(100)),
            Some(ClaimState::Refused(ClaimRefusal::Closed))
        );
        assert_eq!(
            inv.confirm_permanent(at(1), claim(100)),
            Err(InvitationError::Exhausted)
        );
    }

    #[test]
    fn a_declined_verifier_closes_the_invitation_for_good() {
        let mut inv = invitation(InvitationKind::Permanent);
        verified(&mut inv, 1);
        inv.decline(at(2), claim(1)).unwrap();
        assert_eq!(inv.state(at(2)), InvitationState::Declined);
        assert_eq!(
            inv.open_claim(at(2), claim(2)),
            Err(InvitationError::Declined)
        );
    }

    #[test]
    fn cancelling_refuses_pending_claims() {
        let mut inv = invitation(InvitationKind::Temporary);
        verified(&mut inv, 1);
        inv.cancel(at(2));
        assert_eq!(
            inv.claim_state(claim(1)),
            Some(ClaimState::Refused(ClaimRefusal::Closed))
        );
        assert_eq!(
            inv.approve(at(2), claim(1)),
            Err(InvitationError::Cancelled)
        );
    }

    #[test]
    fn claims_past_the_claimant_limit_are_refused() {
        let mut inv = invitation(InvitationKind::Temporary);
        for n in 0..MAX_CLAIMANTS as u8 {
            inv.open_claim(at(1), claim(n)).unwrap();
        }
        assert_eq!(
            inv.open_claim(at(1), claim(200)),
            Err(InvitationError::TooManyClaims)
        );
    }

    fn token() -> InvitationToken {
        let endpoint = *crate::identity::DeviceKey::from_seed(&Zeroizing::new([5; 32]))
            .id()
            .as_bytes();
        InvitationToken {
            kind: InvitationKind::Temporary,
            id: InvitationId::from_bytes([3; 16]),
            secret: InvitationSecret::from_bytes([4; 32]),
            route: Route {
                endpoint,
                addresses: vec![
                    "127.0.0.1:4433".parse().unwrap(),
                    "[::1]:9".parse().unwrap(),
                ],
            },
        }
    }

    #[test]
    fn a_token_survives_its_text_form_in_any_letter_case() {
        let original = token();
        let text = original.to_text().unwrap();
        assert!(text.starts_with(TOKEN_PREFIX));
        assert!(text.len() <= MAX_TOKEN_CHARS);
        let upper = format!(
            "{TOKEN_PREFIX}{}",
            text[TOKEN_PREFIX.len()..].to_ascii_uppercase()
        );
        for form in [text.as_str(), upper.as_str()] {
            let parsed = InvitationToken::parse(form).unwrap();
            assert_eq!(parsed.kind, original.kind);
            assert_eq!(parsed.id, original.id);
            assert_eq!(parsed.secret.expose(), original.secret.expose());
            assert_eq!(parsed.route, original.route);
        }
    }

    #[test]
    fn malformed_tokens_are_refused_with_the_reason() {
        let good = token().to_bytes().unwrap();
        let mut bad_version = good.to_vec();
        bad_version[0] = 2;
        let mut bad_kind = good.to_vec();
        bad_kind[1] = 0;
        let mut trailing = good.to_vec();
        trailing.push(0);
        let mut no_addresses = good.to_vec();
        no_addresses.truncate(1 + 1 + 16 + 32 + 32);
        no_addresses.push(0);
        let cases: [(&[u8], TokenError); 6] = [
            (&[], TokenError::Truncated),
            (&good[..good.len() - 1], TokenError::Truncated),
            (&bad_version, TokenError::Version(2)),
            (&bad_kind, TokenError::Kind),
            (&trailing, TokenError::Trailing),
            (&no_addresses, TokenError::Addresses),
        ];
        for (bytes, expected) in cases {
            assert_eq!(InvitationToken::from_bytes(bytes).err(), Some(expected));
        }
        assert_eq!(
            InvitationToken::parse("https://hyakume.org/x").err(),
            Some(TokenError::Prefix)
        );
        assert_eq!(
            InvitationToken::parse(&format!("{TOKEN_PREFIX}{}", "a".repeat(MAX_TOKEN_CHARS))).err(),
            Some(TokenError::TooLong)
        );
        assert_eq!(
            InvitationToken::parse(&format!("{TOKEN_PREFIX}!!")).err(),
            Some(TokenError::Encoding)
        );
    }

    #[test]
    fn formatting_a_token_hides_its_secret() {
        let printed = format!("{:?}", token());
        assert!(printed.contains("InvitationSecret([redacted])"));
        assert!(!printed.contains("[4, 4, 4"));
    }
}
