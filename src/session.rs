//! Authentication of a connection between trusted devices.
//!
//! After the QUIC handshake both sides read the TLS exporter value. Each
//! signs a transcript of that value, both transport endpoint IDs, and the
//! version negotiation with its long-term device key, under a domain for its
//! role. The dialer proves first. The listener checks that the dialer is
//! trusted before it reveals its own identity.
//!
//! A proof is bound to one connection by the exporter value and to one role
//! by its domain, so it cannot be replayed on another connection or
//! reflected back to its sender. QUIC with TLS 1.3 then protects everything
//! on the connection; there is no second layer of content encryption.

use crate::identity::{DeviceId, DeviceKey};

/// Label for the TLS exporter value bound into session authentication.
pub const EXPORTER_LABEL: &[u8] = b"hyakume v1 session exporter";

const CTX_TRANSCRIPT: &str = "hyakume v1 session transcript";
const SIG_DIALER: &[u8] = b"hyakume v1 session dialer";
const SIG_LISTENER: &[u8] = b"hyakume v1 session listener";

/// Values both sides learn from the connection and the version negotiation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionBinding {
    pub exporter: [u8; 32],
    pub dialer_endpoint: [u8; 32],
    pub listener_endpoint: [u8; 32],
    /// Hash of both version messages exactly as sent.
    pub negotiation: [u8; 32],
}

impl SessionBinding {
    fn transcript(&self) -> [u8; 32] {
        let mut h = blake3::Hasher::new_derive_key(CTX_TRANSCRIPT);
        h.update(&self.exporter);
        h.update(&self.dialer_endpoint);
        h.update(&self.listener_endpoint);
        h.update(&self.negotiation);
        *h.finalize().as_bytes()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Dialer,
    Listener,
}

impl Side {
    fn domain(self) -> &'static [u8] {
        match self {
            Self::Dialer => SIG_DIALER,
            Self::Listener => SIG_LISTENER,
        }
    }
}

/// A device's claim to be the peer on this connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionProof {
    pub device: [u8; 32],
    pub sig: [u8; 64],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SessionError {
    #[error("the other device sent an invalid identity key")]
    BadIdentity,
    #[error("the other device could not prove its identity on this connection")]
    BadSignature,
}

/// Proves that `key` is on this connection, in role `side`.
pub fn prove(key: &DeviceKey, binding: &SessionBinding, side: Side) -> SessionProof {
    SessionProof {
        device: *key.id().as_bytes(),
        sig: key.sign(side.domain(), &binding.transcript()),
    }
}

/// Checks a proof from the peer in role `side` and returns its device.
pub fn verify(
    proof: &SessionProof,
    binding: &SessionBinding,
    side: Side,
) -> Result<DeviceId, SessionError> {
    let device = DeviceId::from_bytes(&proof.device).ok_or(SessionError::BadIdentity)?;
    if !device.verify(side.domain(), &binding.transcript(), &proof.sig) {
        return Err(SessionError::BadSignature);
    }
    Ok(device)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroize::Zeroizing;

    fn binding(exporter: u8) -> SessionBinding {
        SessionBinding {
            exporter: [exporter; 32],
            dialer_endpoint: [1; 32],
            listener_endpoint: [2; 32],
            negotiation: [3; 32],
        }
    }

    #[test]
    fn a_proof_is_bound_to_its_connection_and_role() {
        let key = DeviceKey::from_seed(&Zeroizing::new([4; 32]));
        let proof = prove(&key, &binding(1), Side::Dialer);
        assert_eq!(verify(&proof, &binding(1), Side::Dialer), Ok(key.id()));
        // Replayed on another connection.
        assert_eq!(
            verify(&proof, &binding(2), Side::Dialer),
            Err(SessionError::BadSignature)
        );
        // Reflected back as the listener's proof.
        assert_eq!(
            verify(&proof, &binding(1), Side::Listener),
            Err(SessionError::BadSignature)
        );
        // A tampered negotiation.
        let mut downgraded = binding(1);
        downgraded.negotiation = [9; 32];
        assert_eq!(
            verify(&proof, &downgraded, Side::Dialer),
            Err(SessionError::BadSignature)
        );
    }

    #[test]
    fn a_proof_cannot_claim_another_devices_identity() {
        let (key, other) = (
            DeviceKey::from_seed(&Zeroizing::new([4; 32])),
            DeviceKey::from_seed(&Zeroizing::new([5; 32])),
        );
        let mut proof = prove(&key, &binding(1), Side::Dialer);
        proof.device = *other.id().as_bytes();
        assert_eq!(
            verify(&proof, &binding(1), Side::Dialer),
            Err(SessionError::BadSignature)
        );
    }
}
