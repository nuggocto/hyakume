//! Random 128-bit identifiers for invitations, claims, and transfers.
//!
//! Identifiers are not secrets. They come from the caller's random source so
//! tests can make them reproducible.

use rand_core::{CryptoRng, RngCore};

macro_rules! id_type {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name([u8; 16]);

        impl $name {
            pub fn random(rng: &mut (impl RngCore + CryptoRng)) -> Self {
                let mut bytes = [0u8; 16];
                rng.fill_bytes(&mut bytes);
                Self(bytes)
            }

            pub const fn from_bytes(bytes: [u8; 16]) -> Self {
                Self(bytes)
            }

            pub const fn as_bytes(&self) -> &[u8; 16] {
                &self.0
            }
        }

        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}(", stringify!($name))?;
                for byte in &self.0[..4] {
                    write!(f, "{byte:02x}")?;
                }
                f.write_str("..)")
            }
        }
    };
}

id_type!(
    /// Names one invitation on the inviting device.
    InvitationId
);
id_type!(
    /// Names one attempt to use an invitation.
    ClaimId
);
id_type!(
    /// Names one offer and the transfer that follows it, on both devices.
    TransferId
);
