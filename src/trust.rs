//! Trust records and the per-peer authorization order.
//!
//! Trust comes from exactly one transition: [`TrustStore::trust`] with a
//! [`VerifiedPeer`], which only the pairing exchange can create, and only
//! after both people confirmed the same verifier. Names and aliases never
//! authorize anything.
//!
//! Each grant of trust gets a fresh [`Epoch`] from one store-wide counter.
//! Work authorized under a peer carries an [`Authority`] naming that epoch.
//! Revoking the peer, or pairing it again, makes every older authority stale,
//! so a session, offer, resume request, or publication checks
//! [`TrustStore::check`] instead of trusting a saved flag. The store is the
//! single owner of this order: whichever of revocation and publication
//! reaches it first decides the outcome.

use std::collections::BTreeMap;

use crate::display::PeerText;
use crate::identity::DeviceId;
use crate::limits::{MAX_TRUST_RECORDS, MAX_TRUSTED_DEVICES};

/// Longest local alias in bytes.
pub const MAX_ALIAS_BYTES: usize = 64;

/// A private name the local user gives a trusted device.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct LocalAlias(String);

impl LocalAlias {
    /// Accepts 1 to 64 bytes of text without control or formatting
    /// characters and without surrounding spaces.
    pub fn new(alias: &str) -> Result<Self, TrustError> {
        let valid = !alias.is_empty()
            && alias.len() <= MAX_ALIAS_BYTES
            && alias.trim() == alias
            && crate::display::SafeLine::project(alias.as_bytes(), MAX_ALIAS_BYTES).as_str()
                == alias;
        if !valid {
            return Err(TrustError::InvalidAlias);
        }
        Ok(Self(alias.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Which grant of trust an authority belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Epoch(u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrustState {
    Trusted { epoch: Epoch },
    Revoked,
}

#[derive(Clone, Debug)]
pub struct TrustRecord {
    pub device: DeviceId,
    pub alias: LocalAlias,
    /// The name the device declared during pairing. Presentation only.
    pub declared_name: PeerText,
    pub state: TrustState,
}

/// Proof that work was authorized under one grant of trust in a peer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Authority {
    device: DeviceId,
    epoch: Epoch,
}

impl Authority {
    pub fn device(&self) -> DeviceId {
        self.device
    }
}

/// A peer whose identity was proven and whose verifier both people
/// confirmed. Only the pairing exchange creates one.
pub struct VerifiedPeer {
    device: DeviceId,
    declared_name: PeerText,
}

impl VerifiedPeer {
    pub(crate) fn new(device: DeviceId, declared_name: PeerText) -> Self {
        Self {
            device,
            declared_name,
        }
    }

    pub fn device(&self) -> DeviceId {
        self.device
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TrustError {
    #[error("that device is not trusted")]
    NotTrusted,
    #[error("that device was removed")]
    Revoked,
    #[error("trust in that device changed since this work was authorized")]
    Stale,
    #[error("another trusted device already uses that name")]
    AliasInUse,
    #[error("a name must be 1 to 64 characters without control characters")]
    InvalidAlias,
    #[error("the trusted device limit is reached")]
    Full,
}

#[derive(Default)]
pub struct TrustStore {
    records: BTreeMap<DeviceId, TrustRecord>,
    next_epoch: u64,
}

impl TrustStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Checks that `alias` is free for `device` before an exchange starts,
    /// so a conflict cannot appear after both people have confirmed.
    pub fn check_alias(
        &self,
        alias: &LocalAlias,
        device: Option<DeviceId>,
    ) -> Result<(), TrustError> {
        let taken = self.records.values().any(|r| {
            matches!(r.state, TrustState::Trusted { .. })
                && r.alias == *alias
                && Some(r.device) != device
        });
        if taken {
            return Err(TrustError::AliasInUse);
        }
        Ok(())
    }

    /// Checks, without changing anything, that [`TrustStore::trust`] would
    /// succeed for `device` under `alias`. Pairing asks this before a side
    /// answers yes, so it never agrees to trust it cannot save.
    pub fn can_trust(&self, device: DeviceId, alias: &LocalAlias) -> Result<(), TrustError> {
        self.check_alias(alias, Some(device))?;
        let known = self.records.contains_key(&device);
        if !known && self.records.len() >= MAX_TRUST_RECORDS {
            return Err(TrustError::Full);
        }
        let already_trusted = matches!(
            self.records.get(&device).map(|r| r.state),
            Some(TrustState::Trusted { .. })
        );
        if !already_trusted && self.trusted().count() >= MAX_TRUSTED_DEVICES {
            return Err(TrustError::Full);
        }
        Ok(())
    }

    /// Saves trust in a verified peer. A revoked device becomes trusted again
    /// under a new epoch; nothing authorized before carries over.
    pub fn trust(
        &mut self,
        peer: VerifiedPeer,
        alias: LocalAlias,
    ) -> Result<Authority, TrustError> {
        self.can_trust(peer.device, &alias)?;
        let epoch = Epoch(self.next_epoch);
        // 2^64 grants of trust cannot happen; treat the impossible as full.
        self.next_epoch = self.next_epoch.checked_add(1).ok_or(TrustError::Full)?;
        self.records.insert(
            peer.device,
            TrustRecord {
                device: peer.device,
                alias,
                declared_name: peer.declared_name,
                state: TrustState::Trusted { epoch },
            },
        );
        Ok(Authority {
            device: peer.device,
            epoch,
        })
    }

    /// Authorizes new work with a currently trusted device.
    pub fn authorize(&self, device: &DeviceId) -> Result<Authority, TrustError> {
        match self.records.get(device).map(|r| r.state) {
            Some(TrustState::Trusted { epoch }) => Ok(Authority {
                device: *device,
                epoch,
            }),
            Some(TrustState::Revoked) => Err(TrustError::Revoked),
            None => Err(TrustError::NotTrusted),
        }
    }

    /// Confirms that `authority` is still the current grant of trust.
    pub fn check(&self, authority: &Authority) -> Result<(), TrustError> {
        match self.records.get(&authority.device).map(|r| r.state) {
            Some(TrustState::Trusted { epoch }) if epoch == authority.epoch => Ok(()),
            Some(TrustState::Trusted { .. }) => Err(TrustError::Stale),
            Some(TrustState::Revoked) => Err(TrustError::Revoked),
            None => Err(TrustError::NotTrusted),
        }
    }

    /// Removes trust. Every authority issued for the device becomes stale.
    pub fn revoke(&mut self, device: &DeviceId) -> Result<(), TrustError> {
        let record = self.records.get_mut(device).ok_or(TrustError::NotTrusted)?;
        record.state = TrustState::Revoked;
        Ok(())
    }

    /// Drops the record of a revoked device. Epochs never repeat, so this
    /// cannot revive anything authorized before.
    pub fn forget(&mut self, device: &DeviceId) -> Result<(), TrustError> {
        match self.records.get(device).map(|r| r.state) {
            Some(TrustState::Revoked) => {
                self.records.remove(device);
                Ok(())
            }
            Some(TrustState::Trusted { .. }) => Err(TrustError::NotTrusted),
            None => Ok(()),
        }
    }

    pub fn record(&self, device: &DeviceId) -> Option<&TrustRecord> {
        self.records.get(device)
    }

    /// The trusted device with this local alias, if any.
    pub fn by_alias(&self, alias: &str) -> Option<DeviceId> {
        self.trusted()
            .find(|r| r.alias.as_str() == alias)
            .map(|r| r.device)
    }

    pub fn trusted(&self) -> impl Iterator<Item = &TrustRecord> {
        self.records
            .values()
            .filter(|r| matches!(r.state, TrustState::Trusted { .. }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::DeviceKey;
    use zeroize::Zeroizing;

    fn device(seed: u8) -> DeviceId {
        DeviceKey::from_seed(&Zeroizing::new([seed; 32])).id()
    }

    fn verified(seed: u8, name: &str) -> VerifiedPeer {
        VerifiedPeer::new(device(seed), PeerText::new(name.into(), 64).unwrap())
    }

    fn alias(a: &str) -> LocalAlias {
        LocalAlias::new(a).unwrap()
    }

    #[test]
    fn a_name_alone_authorizes_nothing() {
        let mut store = TrustStore::new();
        store.trust(verified(1, "laptop"), alias("laptop")).unwrap();
        // A second device declaring the same name is still unknown.
        assert_eq!(store.authorize(&device(2)), Err(TrustError::NotTrusted));
        assert_eq!(store.by_alias("laptop"), Some(device(1)));
    }

    #[test]
    fn revocation_makes_existing_authority_stale_and_refuses_new_work() {
        let mut store = TrustStore::new();
        let auth = store.trust(verified(1, "a"), alias("a")).unwrap();
        assert_eq!(store.check(&auth), Ok(()));
        store.revoke(&device(1)).unwrap();
        assert_eq!(store.check(&auth), Err(TrustError::Revoked));
        assert_eq!(store.authorize(&device(1)), Err(TrustError::Revoked));
    }

    #[test]
    fn fresh_pairing_does_not_revive_authority_from_before_revocation() {
        let mut store = TrustStore::new();
        let old = store.trust(verified(1, "a"), alias("a")).unwrap();
        store.revoke(&device(1)).unwrap();
        let new = store.trust(verified(1, "a"), alias("a")).unwrap();
        assert_eq!(store.check(&new), Ok(()));
        assert_eq!(store.check(&old), Err(TrustError::Stale));
    }

    #[test]
    fn forgetting_a_revoked_device_and_pairing_again_keeps_old_authority_stale() {
        let mut store = TrustStore::new();
        let old = store.trust(verified(1, "a"), alias("a")).unwrap();
        store.revoke(&device(1)).unwrap();
        store.forget(&device(1)).unwrap();
        store.trust(verified(1, "a"), alias("a")).unwrap();
        assert_eq!(store.check(&old), Err(TrustError::Stale));
    }

    #[test]
    fn a_revoked_devices_alias_is_free_for_another_device() {
        let mut store = TrustStore::new();
        store.trust(verified(1, "a"), alias("laptop")).unwrap();
        assert_eq!(
            store.check_alias(&alias("laptop"), Some(device(2))),
            Err(TrustError::AliasInUse)
        );
        store.revoke(&device(1)).unwrap();
        assert!(store.trust(verified(2, "b"), alias("laptop")).is_ok());
        assert_eq!(store.by_alias("laptop"), Some(device(2)));
    }

    #[test]
    fn aliases_with_controls_or_padding_are_refused() {
        for bad in ["", " lap", "lap\n", "a\u{202e}b", &"x".repeat(65)] {
            assert_eq!(
                LocalAlias::new(bad),
                Err(TrustError::InvalidAlias),
                "{bad:?}"
            );
        }
        assert!(LocalAlias::new("Ma machine 1").is_ok());
    }
}
