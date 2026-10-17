//! Projections of state for status, devices, and transfers. Names from
//! peers and paths from the disk pass through a safe projection first.

use std::collections::HashMap;

use super::{Phase, Service, hex, lock, shown, state_failure};
use crate::display::SafeLine;
use crate::identity::DeviceId;
use crate::ids::TransferId;
use crate::ipc::{
    Body, DeviceView, ErrorCode, Failure, ItemView, StatusView, TransferSummary, TransferView,
};
use crate::limits::MAX_NAME_COLUMNS;
use crate::offer::PayloadKind;
use crate::state::{State, TransferInfo, is_paused};
use crate::storage::FileState;
use crate::transfer::{BatchOutcome, Transfer};
use crate::trust::{TrustError, TrustState};

/// What a transfer's view and its line in the list both show.
pub(super) struct Heading {
    /// The alias, or the fingerprint once no record is left.
    peer: String,
    kind: &'static str,
    outcome: &'static str,
}

impl Heading {
    pub(super) fn of(s: &State, t: &Transfer, info: &TransferInfo) -> Self {
        let peer = match s.trust().record(&t.peer()) {
            Some(r) if matches!(r.state, TrustState::Trusted { .. }) => r.alias.as_str().to_owned(),
            Some(r) => format!("{} (removed)", r.alias.as_str()),
            None => t.peer().fingerprint(),
        };
        let outcome = match t.outcome() {
            BatchOutcome::Offered => "offered",
            BatchOutcome::InProgress if t.items().iter().any(|s| s.is_unconfirmed()) => {
                "unconfirmed"
            }
            BatchOutcome::InProgress if is_paused(t) => "paused",
            BatchOutcome::InProgress => "in-progress",
            BatchOutcome::Completed => "completed",
            BatchOutcome::PartiallyCompleted { .. } => "partially-completed",
            BatchOutcome::Unsuccessful(_) => "unsuccessful",
        };
        let kind = match info.offer.kind() {
            PayloadKind::Files => "files",
            PayloadKind::Text => "text",
            PayloadKind::Url => "link",
        };
        Self {
            peer,
            kind,
            outcome,
        }
    }
}

impl Service {
    pub(super) fn status(&self) -> StatusView {
        let (state, installation, route) = match &*lock(&self.phase) {
            Phase::Uninitialized => ("uninitialized", None, None),
            Phase::Locked(i) => ("locked", Some(i.clone()), None),
            Phase::Unlocked {
                installation, node, ..
            } => (
                "unlocked",
                Some(installation.clone()),
                node.route().to_text().ok(),
            ),
        };

        let receiving = lock(&self.routes)
            .receiver
            .as_ref()
            .map(|(_, _, d)| shown(&d.place().dir));

        let mut s = self.cell.lock();
        let trusted = s.trust().trusted().count();
        let unfinished = s.unfinished();
        let paused = unfinished
            .iter()
            .filter(|t| s.transfer(t).is_some_and(is_paused))
            .count();
        let store_failed = s.store_failed();
        let files = s.store().files().unwrap_or_default();
        let (spool_bytes, partial_bytes) = s.store().charged().unwrap_or_default();
        let retained: Vec<_> = files
            .iter()
            .filter(|f| f.state == FileState::Retained)
            .collect();

        StatusView {
            state: state.into(),
            store: installation.as_ref().map(|i| i.store),
            fingerprint: installation.as_ref().map(|i| i.device.fingerprint()),
            // Loading already refused a name with controls; escaping
            // again leaves a valid name as it is.
            name: installation.as_ref().map(|i| {
                SafeLine::project(i.name.as_bytes(), crate::trust::MAX_ALIAS_BYTES)
                    .as_str()
                    .to_owned()
            }),
            route,
            trusted,
            unfinished: unfinished.len(),
            paused,
            receiving,
            retained_files: retained.len(),
            retained_bytes: retained.iter().map(|f| f.bytes).sum(),
            spool_bytes,
            partial_bytes,
            store_failed,
        }
    }

    pub(super) fn devices(&self) -> Body {
        let s = self.cell.lock();
        let devices = s
            .trust()
            .records()
            .map(|r| DeviceView {
                alias: r.alias.as_str().to_owned(),
                fingerprint: r.device.fingerprint(),
                name: r.declared_name.line(MAX_NAME_COLUMNS).to_string(),
                trusted: matches!(r.state, TrustState::Trusted { .. }),
            })
            .collect();
        Body::Devices { devices }
    }

    pub(super) fn find_device(&self, name: &str) -> Result<DeviceId, Failure> {
        self.cell.lock().trust().find(name).map_err(|e| match e {
            TrustError::Ambiguous => Failure::new(ErrorCode::Invalid, e.to_string()),
            _ => Failure::new(ErrorCode::NotFound, "no trusted device has that name"),
        })
    }

    pub(super) fn remove(&self, name: &str) -> Result<Body, Failure> {
        let device = self.find_device(name)?;
        self.cell
            .change(|s| s.revoke(&device))
            .map_err(state_failure)?;
        let s = self.cell.lock();
        let record = s
            .trust()
            .record(&device)
            .ok_or_else(|| Failure::new(ErrorCode::NotFound, "no such device"))?;
        Ok(Body::Device(DeviceView {
            alias: record.alias.as_str().to_owned(),
            fingerprint: record.device.fingerprint(),
            name: record.declared_name.line(MAX_NAME_COLUMNS).to_string(),
            trusted: false,
        }))
    }

    /// Finds a transfer by its ID or a unique prefix of at least 8 hex
    /// digits.
    pub(super) fn find_transfer(&self, text: &str) -> Result<TransferId, Failure> {
        let text = text.to_ascii_lowercase();
        if text.len() < 8 || text.len() > 32 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Failure::new(
                ErrorCode::Invalid,
                "a transfer is named by at least 8 hex digits of its ID",
            ));
        }
        let s = self.cell.lock();
        let mut found = s
            .transfers()
            .map(|t| t.id())
            .filter(|id| hex(id.as_bytes()).starts_with(&text));
        match (found.next(), found.next()) {
            (Some(id), None) => Ok(id),
            (None, _) => Err(Failure::new(ErrorCode::NotFound, "no transfer has that ID")),
            (Some(_), Some(_)) => Err(Failure::new(
                ErrorCode::Invalid,
                "more than one transfer starts with that; give more digits",
            )),
        }
    }

    pub(super) fn transfer_view(&self, id: TransferId) -> Result<TransferView, Failure> {
        let mut s = self.cell.lock();
        let retained_bytes = s
            .store()
            .files()
            .unwrap_or_default()
            .iter()
            .filter(|f| f.transfer == id)
            .map(|f| f.bytes)
            .sum();

        let (Some(t), Some(info)) = (s.transfer(&id), s.info(&id)) else {
            return Err(Failure::new(ErrorCode::NotFound, "no transfer has that ID"));
        };
        let heading = Heading::of(&s, t, info);
        let items = info
            .offer
            .items()
            .iter()
            .zip(t.items())
            .enumerate()
            .map(|(n, (item, state))| ItemView {
                name: match info.offer.kind() {
                    PayloadKind::Files => item.name.line(MAX_NAME_COLUMNS).to_string(),
                    PayloadKind::Text => "(text)".into(),
                    PayloadKind::Url => "(link)".into(),
                },
                size: item.size,
                state: state.name(),
                output: info.outputs.get(n).cloned().flatten().map(|o| shown(&o)),
            })
            .collect();

        Ok(TransferView {
            transfer: hex(id.as_bytes()),
            outgoing: info.outgoing,
            peer: heading.peer,
            kind: heading.kind.into(),
            offer: t.offer_state().name(),
            outcome: heading.outcome.into(),
            items,
            place: info.place.as_ref().map(|p| shown(&p.dir)),
            retained_bytes,
            expires_ms: info.expires_ms,
        })
    }

    /// Every kept transfer, newest first, read under one lock with one
    /// pass over the file ledger.
    pub(super) fn transfers(&self) -> Body {
        let mut s = self.cell.lock();
        let mut retained: HashMap<TransferId, u64> = HashMap::new();
        for f in s.store().files().unwrap_or_default() {
            *retained.entry(f.transfer).or_default() += f.bytes;
        }
        let mut listed: Vec<(u64, TransferSummary)> = s
            .transfers()
            .filter_map(|t| {
                let info = s.info(&t.id())?;
                let heading = Heading::of(&s, t, info);
                let summary = TransferSummary {
                    transfer: hex(t.id().as_bytes()),
                    outgoing: info.outgoing,
                    peer: heading.peer,
                    kind: heading.kind.into(),
                    outcome: heading.outcome.into(),
                    items: t.items().len(),
                    retained_bytes: retained.get(&t.id()).copied().unwrap_or(0),
                    expires_ms: info.expires_ms,
                };
                Some((info.created_ms, summary))
            })
            .collect();
        listed.sort_by_key(|(created, _)| std::cmp::Reverse(*created));
        Body::Transfers {
            transfers: listed.into_iter().map(|(_, t)| t).collect(),
        }
    }
}
