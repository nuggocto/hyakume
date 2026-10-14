//! Long-term device identity: an Ed25519 key. [`DeviceKey`] cannot be
//! cloned, printed, or serialized, and is wiped on drop. It is separate from
//! the per-run transport key, so it shows only inside authenticated,
//! encrypted streams, never in a handshake.

use rand_core::{CryptoRng, RngCore};
use zeroize::Zeroizing;

/// The private device key. Use [`DeviceKey::sign`]; the key bytes never
/// leave this type except through [`DeviceKey::seed`] for the secret store.
pub struct DeviceKey(iroh::SecretKey);

impl DeviceKey {
    pub fn generate(rng: &mut (impl RngCore + CryptoRng)) -> Self {
        let mut seed = Zeroizing::new([0u8; 32]);
        rng.fill_bytes(&mut *seed);
        Self::from_seed(&seed)
    }

    /// Rebuilds a key from the 32-byte seed kept in the secret store.
    pub fn from_seed(seed: &Zeroizing<[u8; 32]>) -> Self {
        Self(iroh::SecretKey::from_bytes(seed))
    }

    /// The seed, for writing to the secret store.
    pub fn seed(&self) -> Zeroizing<[u8; 32]> {
        Zeroizing::new(self.0.to_bytes())
    }

    pub fn id(&self) -> DeviceId {
        DeviceId(self.0.public())
    }

    /// Signs `message` under `domain`. Each protocol step uses its own
    /// domain, so a signature made for one step is never valid for another.
    pub fn sign(&self, domain: &[u8], message: &[u8]) -> [u8; 64] {
        self.0.sign(&domain_separated(domain, message)).to_bytes()
    }
}

impl std::fmt::Debug for DeviceKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DeviceKey([redacted])")
    }
}

/// A validated Ed25519 public key that identifies one device.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeviceId(iroh::PublicKey);

impl DeviceId {
    /// Accepts only bytes that decode to a valid curve point.
    pub fn from_bytes(bytes: &[u8; 32]) -> Option<Self> {
        iroh::PublicKey::from_bytes(bytes).ok().map(Self)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        self.0.as_bytes()
    }

    /// Checks a signature made by [`DeviceKey::sign`] with the same domain.
    /// Verification is strict, so malleable or small-order forms fail.
    pub fn verify(&self, domain: &[u8], message: &[u8], signature: &[u8; 64]) -> bool {
        let signature = iroh::Signature::from_bytes(signature);
        self.0
            .verify(&domain_separated(domain, message), &signature)
            .is_ok()
    }

    /// The short form people compare: four groups of four base32 letters.
    pub fn fingerprint(&self) -> String {
        let text = data_encoding::BASE32_NOPAD.encode(self.as_bytes());
        text.as_bytes()
            .chunks(4)
            .take(4)
            .map(|c| String::from_utf8_lossy(c).into_owned())
            .collect::<Vec<_>>()
            .join("-")
    }
}

impl std::fmt::Debug for DeviceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DeviceId({})", self.fingerprint())
    }
}

/// Prefixes the message with its length-delimited domain so that no two
/// (domain, message) pairs produce the same signed bytes.
fn domain_separated(domain: &[u8], message: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + domain.len() + message.len());
    out.extend_from_slice(&(domain.len() as u64).to_be_bytes());
    out.extend_from_slice(domain);
    out.extend_from_slice(message);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(seed: u8) -> DeviceKey {
        DeviceKey::from_seed(&Zeroizing::new([seed; 32]))
    }

    #[test]
    fn a_signature_is_valid_only_for_its_key_domain_and_message() {
        let (alice, bob) = (key(1), key(2));
        let sig = alice.sign(b"step one", b"message");
        assert!(alice.id().verify(b"step one", b"message", &sig));
        assert!(!alice.id().verify(b"step two", b"message", &sig));
        assert!(!alice.id().verify(b"step one", b"other", &sig));
        assert!(!bob.id().verify(b"step one", b"message", &sig));
    }

    #[test]
    fn formatting_a_device_key_reveals_no_key_material() {
        let k = key(7);
        let printed = format!("{k:?}");
        let seed = k.seed();
        assert_eq!(printed, "DeviceKey([redacted])");
        let hex: String = seed.iter().map(|b| format!("{b:02x}")).collect();
        assert!(!printed.contains(&hex));
    }

    #[test]
    fn invalid_public_key_bytes_are_refused() {
        // No point on the curve has y = 2.
        let mut bytes = [0u8; 32];
        bytes[0] = 2;
        assert!(DeviceId::from_bytes(&bytes).is_none());
    }
}
