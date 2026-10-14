//! Trust records and the per-peer authorization order. Trust comes only from
//! a [`VerifiedPeer`], made after both people confirmed the same words. Each
//! grant gets a fresh [`Epoch`], so revoking or pairing again makes older
//! [`Authority`] values stale. A [`Reservation`] holds the alias until the
//! other device answers, so saving trust cannot hit a late conflict.

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

impl Epoch {
    pub fn value(self) -> u64 {
        self.0
    }

    /// An epoch read back from storage. It authorizes nothing by itself:
    /// [`TrustStore::check`] still compares it with the current grant.
    pub(crate) fn restore(value: u64) -> Self {
        Self(value)
    }
}

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

    pub fn epoch(&self) -> Epoch {
        self.epoch
    }

    /// An authority read back from storage, as stale or current as the
    /// trust store says when it is checked.
    pub(crate) fn restore(device: DeviceId, epoch: Epoch) -> Self {
        Self { device, epoch }
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

/// An alias and one trusted place held for a device while its pairing
/// waits for the other side's answer. Only [`TrustStore::reserve`] makes
/// one; [`TrustStore::trust_reserved`] or [`TrustStore::release`] ends it.
#[derive(Debug, PartialEq, Eq)]
pub struct Reservation {
    id: u64,
    alias: LocalAlias,
}

impl Reservation {
    /// Names this reservation in [`TrustStore::release_key`].
    pub(crate) fn key(&self) -> u64 {
        self.id
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
    #[error("that name is one device's alias and another's fingerprint")]
    Ambiguous,
}

/// Saved trust records that cannot be loaded as they are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("saved trust records are inconsistent: {0}")]
pub struct RestoreError(&'static str);

#[derive(Clone, Default)]
pub struct TrustStore {
    records: BTreeMap<DeviceId, TrustRecord>,
    next_epoch: u64,
    /// Held aliases and places, by reservation.
    reserved: BTreeMap<u64, (DeviceId, LocalAlias)>,
    next_reservation: u64,
}

impl TrustStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuilds the store from saved records and the saved epoch counter.
    /// The counter must be past every saved epoch, so no epoch is ever
    /// handed out twice, and the saved records must respect the limits.
    pub fn restore(records: Vec<TrustRecord>, next_epoch: u64) -> Result<Self, RestoreError> {
        if records.len() > MAX_TRUST_RECORDS {
            return Err(RestoreError("too many records"));
        }
        let mut store = Self {
            next_epoch,
            ..Self::default()
        };
        for record in records {
            if let TrustState::Trusted { epoch } = record.state {
                if epoch.0 >= next_epoch {
                    return Err(RestoreError("an epoch is not below the counter"));
                }
                if store.check_alias(&record.alias, None).is_err() {
                    return Err(RestoreError("two trusted devices share an alias"));
                }
            }
            if store.records.insert(record.device, record).is_some() {
                return Err(RestoreError("a device appears twice"));
            }
        }
        if store.trusted().count() > MAX_TRUSTED_DEVICES {
            return Err(RestoreError("too many trusted devices"));
        }
        Ok(store)
    }

    /// The epoch the next grant of trust will get.
    pub fn next_epoch(&self) -> u64 {
        self.next_epoch
    }

    /// Checks that `alias` is free for `device`: no other trusted device has
    /// it and no pairing holds it. Checking before an exchange starts saves
    /// people from comparing words for a pairing that cannot be saved.
    pub fn check_alias(
        &self,
        alias: &LocalAlias,
        device: Option<DeviceId>,
    ) -> Result<(), TrustError> {
        let trusted = self.records.values().any(|r| {
            matches!(r.state, TrustState::Trusted { .. })
                && r.alias == *alias
                && Some(r.device) != device
        });
        let held = self.reserved.values().any(|(_, a)| a == alias);
        if trusted || held {
            return Err(TrustError::AliasInUse);
        }
        Ok(())
    }

    /// Checks, without changing anything, that `device` could be trusted
    /// under `alias`, counting what other pairings hold.
    pub fn can_trust(&self, device: DeviceId, alias: &LocalAlias) -> Result<(), TrustError> {
        self.check_alias(alias, Some(device))?;
        let is_trusted = |d: &DeviceId| {
            matches!(
                self.records.get(d).map(|r| r.state),
                Some(TrustState::Trusted { .. })
            )
        };
        let held = self.reserved.values().map(|(d, _)| d);
        let held_new = held.clone().filter(|d| !self.records.contains_key(d));
        if !self.records.contains_key(&device)
            && self.records.len() + held_new.count() >= MAX_TRUST_RECORDS
        {
            return Err(TrustError::Full);
        }
        let held_untrusted = held.filter(|d| !is_trusted(d));
        if !is_trusted(&device)
            && self.trusted().count() + held_untrusted.count() >= MAX_TRUSTED_DEVICES
        {
            return Err(TrustError::Full);
        }
        Ok(())
    }

    /// Holds `alias` and one trusted place for `device`. Pairing calls this
    /// when its person answers yes, so it never agrees to trust it could not
    /// save once the other side agrees too.
    pub fn reserve(
        &mut self,
        device: DeviceId,
        alias: LocalAlias,
    ) -> Result<Reservation, TrustError> {
        self.can_trust(device, &alias)?;
        let id = self.next_reservation;
        // Reservations end, so 2^64 of them cannot be held; treat the
        // impossible as full.
        self.next_reservation = id.checked_add(1).ok_or(TrustError::Full)?;
        self.reserved.insert(id, (device, alias.clone()));
        Ok(Reservation { id, alias })
    }

    /// Gives back what a pairing held when it ends without trust.
    pub fn release(&mut self, reservation: Reservation) {
        self.reserved.remove(&reservation.id);
    }

    /// Gives back a reservation by its key, for when the reservation itself
    /// was used in an attempt that failed.
    pub(crate) fn release_key(&mut self, key: u64) {
        self.reserved.remove(&key);
    }

    /// Saves trust in a verified peer without holding anything first.
    pub fn trust(
        &mut self,
        peer: VerifiedPeer,
        alias: LocalAlias,
    ) -> Result<Authority, TrustError> {
        let reservation = self.reserve(peer.device, alias)?;
        self.trust_reserved(peer, reservation)
    }

    /// Saves trust in a verified peer under the alias it reserved. A revoked
    /// device becomes trusted again under a new epoch; nothing authorized
    /// before carries over.
    pub fn trust_reserved(
        &mut self,
        peer: VerifiedPeer,
        reservation: Reservation,
    ) -> Result<Authority, TrustError> {
        self.reserved.remove(&reservation.id);
        let alias = reservation.alias;
        // Cannot fail for the device the reservation was made for.
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

    /// The trusted device a person names by alias or fingerprint. Revoked
    /// records never match, because a newer device may reuse their alias.
    pub fn find(&self, name: &str) -> Result<DeviceId, TrustError> {
        let wanted = name.replace('-', "").to_ascii_uppercase();
        let by_fingerprint = self
            .trusted()
            .find(|r| r.device.fingerprint().replace('-', "") == wanted)
            .map(|r| r.device);
        match (self.by_alias(name), by_fingerprint) {
            (Some(a), Some(f)) if a != f => Err(TrustError::Ambiguous),
            (Some(device), _) | (None, Some(device)) => Ok(device),
            (None, None) => Err(TrustError::NotTrusted),
        }
    }

    pub fn records(&self) -> impl Iterator<Item = &TrustRecord> {
        self.records.values()
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
    fn a_name_finds_the_trusted_device_not_a_revoked_one_with_the_same_alias() {
        // The revoked record must sort first to be found first by a scan.
        let (old, new) = if device(1) < device(2) {
            (1, 2)
        } else {
            (2, 1)
        };
        let mut store = TrustStore::new();
        store.trust(verified(old, "a"), alias("laptop")).unwrap();
        store.revoke(&device(old)).unwrap();
        store.trust(verified(new, "b"), alias("laptop")).unwrap();
        assert_eq!(store.find("laptop"), Ok(device(new)));
        let fingerprint = device(new).fingerprint().to_ascii_lowercase();
        assert_eq!(store.find(&fingerprint), Ok(device(new)));
        assert_eq!(
            store.find(&device(old).fingerprint()),
            Err(TrustError::NotTrusted)
        );
    }

    #[test]
    fn a_name_that_is_one_alias_and_another_fingerprint_is_refused() {
        let mut store = TrustStore::new();
        let fingerprint = device(1).fingerprint();
        store.trust(verified(1, "a"), alias("one")).unwrap();
        store.trust(verified(2, "b"), alias(&fingerprint)).unwrap();
        assert_eq!(store.find(&fingerprint), Err(TrustError::Ambiguous));
    }

    #[test]
    fn a_reserved_alias_and_place_cannot_be_taken_until_released() {
        let mut store = TrustStore::new();
        let held = store.reserve(device(1), alias("phone")).unwrap();
        assert_eq!(
            store.trust(verified(2, "b"), alias("phone")),
            Err(TrustError::AliasInUse)
        );
        store.release(held);
        assert!(store.trust(verified(2, "b"), alias("phone")).is_ok());

        let held = store.reserve(device(3), alias("tablet")).unwrap();
        assert!(store.trust(verified(4, "d"), alias("other")).is_ok());
        let authority = store.trust_reserved(verified(3, "c"), held).unwrap();
        assert_eq!(store.check(&authority), Ok(()));
        assert_eq!(store.by_alias("tablet"), Some(device(3)));
    }

    #[test]
    fn reservations_count_against_the_trusted_device_limit() {
        let mut store = TrustStore::new();
        for n in 0..MAX_TRUSTED_DEVICES - 1 {
            let peer = VerifiedPeer::new(device_n(n), PeerText::default());
            store.trust(peer, alias(&format!("d{n}"))).unwrap();
        }
        let last = store.reserve(device(1), alias("last")).unwrap();
        assert_eq!(
            store.reserve(device(2), alias("over")),
            Err(TrustError::Full)
        );
        assert!(store.trust_reserved(verified(1, "a"), last).is_ok());
    }

    fn device_n(n: usize) -> DeviceId {
        let mut seed = [0u8; 32];
        seed[..8].copy_from_slice(&(n as u64 + 1000).to_be_bytes());
        DeviceKey::from_seed(&Zeroizing::new(seed)).id()
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
