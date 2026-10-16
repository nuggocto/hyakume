//! Startup recovery, expiry, and cleanup of the files Hyakume owns. Recovery
//! settles unfinished work from evidence on disk, and only the very file
//! Hyakume wrote, same device and inode, counts as published, so ambiguous
//! evidence never becomes a success. `docs/local-service.md` has the table
//! of outcomes.

use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use rustix::fs::AtFlags;
use rustix::io::Errno;

use crate::files::destination::{Destination, DestinationError};
use crate::files::names::{partial_name, spool_transfer};
use crate::files::source::reopen_spool;
use crate::files::{FileIdentity, owned_regular};
use crate::ids::TransferId;
use crate::limits::{CLEANUP_WINDOW, MAX_SPOOL_BYTES};
use crate::state::{Place, State, StateError};
use crate::storage::{FileKind, FileState, NewFile, OwnedFile};
use crate::transfer::{FailCause, ItemState, OfferState, Transfer};

/// What recovery found.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Recovered {
    pub published: usize,
    /// Items renamed under their final name whose folder still does not
    /// sync. They wait for a later check.
    pub unconfirmed: usize,
    /// Items whose folder or file could not be read. They become unconfirmed,
    /// since a crash after the rename can leave one saved as queued.
    pub unreadable: usize,
    pub paused: usize,
    pub lost: usize,
    pub interrupted: usize,
    pub revoked: usize,
    pub expired: usize,
    /// Spools no row recorded, now in the ledger for cleanup to delete.
    pub orphans: usize,
    /// Entries in the spool folder that are not spools Hyakume makes.
    /// They are left alone.
    pub strays: usize,
    /// The spool folder could not be read in full, so a spool may still
    /// be there unrecorded until a later start finds it.
    pub spool_unread: bool,
}

/// Settles unfinished work after a restart, from the evidence on disk.
pub fn recover(state: &mut State, spool_dir: &Path, now_ms: u64) -> Result<Recovered, StateError> {
    let mut found = Recovered::default();
    let files = state.store().files()?;
    for id in state.unfinished() {
        let (Some(transfer), Some(info)) = (state.transfer(&id).cloned(), state.info(&id).cloned())
        else {
            continue;
        };
        if transfer.offer_state() == OfferState::Offered {
            state.update_transfer(&id, Transfer::pause_all);
            found.interrupted += 1;
            continue;
        }

        for (item, item_state) in transfer.items().iter().enumerate() {
            if item_state.is_terminal() {
                continue;
            }
            let row = files.iter().find(|f| f.transfer == id && f.item == item);
            let evidence = if info.outgoing {
                spool_evidence(row)
            } else {
                partial_evidence(row, info.outputs.get(item).cloned().flatten(), id, item)
            };
            match evidence {
                Evidence::Published => found.published += 1,
                Evidence::Unconfirmed => found.unconfirmed += 1,
                Evidence::Unreadable => found.unreadable += 1,
                Evidence::Kept => {}
                Evidence::Lost => found.lost += 1,
            }
            record(state, &id, item, evidence);
        }

        let Some(transfer) = state.transfer(&id).cloned() else {
            continue;
        };
        if transfer.is_terminal() {
            continue;
        }

        // An unconfirmed item keeps a revocation or expiry for later, so
        // these run even when nothing else is left to stop.
        if state.trust().check(&transfer.authority()).is_err() {
            state.update_transfer(&id, Transfer::revoke);
            found.revoked += 1;
        } else if info.expired(now_ms) || state.downtime_unknown() {
            state.update_transfer(&id, Transfer::expire_all);
            found.expired += 1;
        } else if !transfer
            .items()
            .iter()
            .all(|s| s.is_terminal() || s.is_unconfirmed())
        {
            state.update_transfer(&id, Transfer::pause_all);
            found.paused += 1;
        }
    }

    // Files no unfinished item needs any more, such as a spool sealed just
    // before a crash that came before its offer, go.
    let unfinished = state.unfinished();
    for file in state.store().files()? {
        let needed = unfinished.contains(&file.transfer)
            && state
                .transfer(&file.transfer)
                .and_then(|t| t.items().get(file.item))
                .is_some_and(|s| !s.is_terminal());
        if matches!(file.state, FileState::Reserved | FileState::Live) && !needed {
            state.store().delete_file(file.id, now_ms)?;
        }
    }

    record_orphan_spools(state, spool_dir, now_ms, &mut found)?;
    state.store().retry_retained(now_ms)?;
    Ok(found)
}

/// What the disk shows for one unfinished item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Evidence {
    /// The file Hyakume wrote is under its final name and the folder
    /// synced.
    Published,
    /// The file Hyakume wrote is under its final name, but the folder does
    /// not sync.
    Unconfirmed,
    /// The data for the item is still there to resume from.
    Kept,
    /// The folder or a file in it could not be read, so nothing is known.
    Unreadable,
    Lost,
}

/// Records what the disk shows for one item. A partial file found again
/// after an unconfirmed rename means the rename did not survive, so the
/// item is paused work again.
fn record(state: &mut State, id: &TransferId, item: usize, evidence: Evidence) {
    let found = match evidence {
        Evidence::Published => return state.recovered_published(id, item),
        // Unknown is not gone, and the rename may have happened; a later
        // check decides.
        Evidence::Unconfirmed | Evidence::Unreadable => ItemState::Unconfirmed(None),
        Evidence::Kept => ItemState::Paused,
        Evidence::Lost => ItemState::Failed(FailCause::Lost),
    };
    state.update_transfer(id, |t| {
        // Startup pauses kept work later, together with its revocation and
        // expiry checks.
        if evidence != Evidence::Kept || t.items().get(item).is_some_and(|s| s.is_unconfirmed()) {
            let _ = t.found_on_disk(item, found);
        }
    });
}

/// An unconfirmed item and what checking it again needs, copied out under
/// the lock so the blocking check can run outside it.
#[derive(Clone, Debug)]
pub struct Unsettled {
    transfer: TransferId,
    item: usize,
    row: Option<OwnedFile>,
    output: Option<Vec<u8>>,
}

/// The unconfirmed items, for maintenance to check again while the service
/// runs, so one does not wait for a restart.
pub fn unsettled(state: &mut State) -> Result<Vec<Unsettled>, StateError> {
    let files = state.store().files()?;
    let mut out = Vec::new();
    for id in state.unfinished() {
        let (Some(transfer), Some(info)) = (state.transfer(&id), state.info(&id)) else {
            continue;
        };
        for (item, s) in transfer.items().iter().enumerate() {
            if s.is_unconfirmed() {
                out.push(Unsettled {
                    transfer: id,
                    item,
                    row: files
                        .iter()
                        .find(|f| f.transfer == id && f.item == item)
                        .cloned(),
                    output: info.outputs.get(item).cloned().flatten(),
                });
            }
        }
    }
    Ok(out)
}

/// Checks the disk again for an unconfirmed item, as startup recovery
/// would. Blocking.
pub fn recheck(u: &Unsettled) -> Evidence {
    partial_evidence(u.row.as_ref(), u.output.clone(), u.transfer, u.item)
}

/// Records what [`recheck`] found. The check ran without the lock, so the
/// item may have changed meanwhile; only a still unconfirmed one is touched.
pub fn settle(state: &mut State, u: &Unsettled, evidence: Evidence) {
    let waiting = state
        .transfer(&u.transfer)
        .and_then(|t| t.items().get(u.item))
        .is_some_and(|s| s.is_unconfirmed());
    if waiting {
        record(state, &u.transfer, u.item, evidence);
    }
}

fn partial_evidence(
    row: Option<&OwnedFile>,
    output: Option<Vec<u8>>,
    transfer: TransferId,
    item: usize,
) -> Evidence {
    let Some(row) = row.filter(|r| r.kind == FileKind::Partial) else {
        return Evidence::Lost;
    };
    let Some(key) = row.identity else {
        // The file was never recorded as created, so no byte of it counts.
        // The item can still resume from the start.
        return Evidence::Kept;
    };
    let dest = match Destination::reopen(&row.dir) {
        Ok(dest) => dest,
        // The path no longer leads to the recorded folder, so neither name
        // can hold the file there.
        Err(
            DestinationError::Missing
            | DestinationError::Replaced
            | DestinationError::NotDirectory
            | DestinationError::Symlink,
        ) => return Evidence::Lost,
        Err(_) => return Evidence::Unreadable,
    };
    // The file under its final name is published only once the folder
    // syncs: a rename whose sync failed is not known to survive a crash.
    let at_output = match output {
        Some(output) => dest.identity_of(&output),
        None => Ok(None),
    };
    match at_output {
        Ok(Some(found)) if found == key => {
            return match dest.sync() {
                Ok(()) => Evidence::Published,
                Err(_) => Evidence::Unconfirmed,
            };
        }
        Ok(_) => {}
        Err(_) => return Evidence::Unreadable,
    }
    match dest.identity_of(partial_name(transfer, item).as_bytes()) {
        Ok(Some(found)) if found == key => Evidence::Kept,
        Ok(_) => Evidence::Lost,
        Err(_) => Evidence::Unreadable,
    }
}

fn spool_evidence(row: Option<&OwnedFile>) -> Evidence {
    // An item read from the sender's own file has no spool; whether the
    // file is still the same is checked if the item ever resumes.
    let Some(row) = row else {
        return Evidence::Kept;
    };
    let Some(key) = row.identity else {
        return Evidence::Lost;
    };
    let dir = Path::new(std::ffi::OsStr::from_bytes(&row.dir.dir));
    match reopen_spool(dir, &row.name, key, row.bytes) {
        Ok(Some(_)) => Evidence::Kept,
        _ => Evidence::Lost,
    }
}

/// Records each spool no row records, left by input that never finished,
/// as a file to delete. Cleanup then deletes it like any other owned file:
/// its bytes stay charged until it is gone, and a deletion that fails is
/// retried. Recovery itself deletes nothing here, so a deletion that fails
/// cannot leave a file the ledger does not know.
fn record_orphan_spools(
    state: &mut State,
    spool_dir: &Path,
    now_ms: u64,
    found: &mut Recovered,
) -> Result<(), StateError> {
    let known: Vec<Vec<u8>> = state
        .store()
        .files()?
        .into_iter()
        .filter(|f| f.kind == FileKind::Spool)
        .map(|f| f.name)
        .collect();
    let opened = std::fs::File::open(spool_dir).and_then(|dir| {
        let st = rustix::fs::fstat(&dir)?;
        Ok((dir, st, std::fs::read_dir(spool_dir)?))
    });
    let (dir, st, entries) = match opened {
        Ok(opened) => opened,
        Err(e) => {
            tracing::warn!(error = %e, "cannot read the spool folder");
            found.spool_unread = true;
            return Ok(());
        }
    };
    let id = FileIdentity::of(&st);
    let place = Place {
        dir: spool_dir.as_os_str().as_bytes().to_vec(),
        dev: id.dev,
        ino: id.ino,
    };

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                tracing::warn!(error = %e, "cannot read the spool folder to its end");
                found.spool_unread = true;
                break;
            }
        };
        let name = entry.file_name();
        let name = name.as_bytes();
        if known.iter().any(|k| k == name) {
            continue;
        }
        let Some(transfer) = spool_transfer(name) else {
            found.strays += 1;
            continue;
        };
        let (identity, bytes) = match rustix::fs::statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) if owned_regular(&st) => {
                let id = FileIdentity::of(&st);
                (Some(id.key()), id.size)
            }
            Ok(_) => {
                found.strays += 1;
                continue;
            }
            Err(Errno::NOENT) => continue,
            // Its size cannot be read, so it is charged the most a spool
            // can hold.
            Err(_) => (None, MAX_SPOOL_BYTES),
        };
        state.store().add_file_to_delete(
            &NewFile {
                kind: FileKind::Spool,
                transfer,
                item: 0,
                dir: &place,
                name,
                identity,
                bytes,
            },
            now_ms,
        )?;
        found.orphans += 1;
    }
    if found.strays > 0 {
        tracing::warn!(
            strays = found.strays,
            "the spool folder holds entries Hyakume did not make; they stay"
        );
    }
    Ok(())
}

/// Deletes one file Hyakume owns, if it is still the file recorded. A
/// directory that no longer exists holds nothing to delete. Blocking.
pub fn delete_owned(file: &OwnedFile) -> io::Result<()> {
    match Destination::reopen(&file.dir) {
        Ok(dir) => dir.remove(&file.name, file.identity),
        Err(DestinationError::Missing) => Ok(()),
        Err(e) => Err(io::Error::other(e.to_string())),
    }
}

pub fn record_deletion(
    state: &mut State,
    file: &OwnedFile,
    result: io::Result<()>,
    now_ms: u64,
) -> Result<(), StateError> {
    match result {
        Ok(()) => state.store().file_deleted(file.id)?,
        Err(e) => {
            let next = next_attempt(file, now_ms);
            state
                .store()
                .deletion_failed(file.id, &e.to_string(), next)?;
        }
    }
    Ok(())
}

/// When to try again, or `None` once the retry window is over.
fn next_attempt(file: &OwnedFile, now_ms: u64) -> Option<u64> {
    const FIRST: u64 = 10_000;
    const LONGEST: u64 = 60 * 60 * 1000;
    let delay = FIRST
        .saturating_mul(4u64.saturating_pow(file.attempts))
        .min(LONGEST);
    let since = file.delete_since_ms.unwrap_or(now_ms);
    let window_end = since.saturating_add(CLEANUP_WINDOW.as_millis() as u64);
    let next = now_ms.saturating_add(delay);
    // A clock earlier than the window's start cannot tell how long the
    // window has run; it stops retrying rather than retrying forever.
    (next < window_end && now_ms >= since).then_some(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::PeerText;
    use crate::identity::{DeviceId, DeviceKey};
    use crate::limits::RESUME_WINDOW;
    use crate::offer::Offer;
    use crate::state::{Place, TransferInfo};
    use crate::transfer::{CancelCause, ItemState, RejectCause};
    use crate::trust::{Authority, LocalAlias, VerifiedPeer};
    use zeroize::Zeroizing;

    const START: u64 = 1_000_000;

    fn device() -> DeviceId {
        DeviceKey::from_seed(&Zeroizing::new([3; 32])).id()
    }

    /// A state trusting one peer, with one sent transfer of a file source,
    /// accepted unless `answered` is false.
    fn with_transfer(answered: bool) -> (State, TransferId) {
        transfer_in(State::in_memory().unwrap(), answered, START)
    }

    /// The same, in `state`, created at `created_ms`.
    fn transfer_in(mut state: State, answered: bool, created_ms: u64) -> (State, TransferId) {
        let reservation = state
            .trust
            .reserve(device(), LocalAlias::new("peer").unwrap())
            .unwrap();
        let authority: Authority = state
            .trust_peer(
                VerifiedPeer::new(device(), PeerText::default()),
                reservation,
            )
            .unwrap();
        let id = TransferId::from_bytes([1; 16]);
        let offer = Offer::for_files(id, &[("f", b"abc")]).unwrap();
        let t = Transfer::offered(id, authority, 1).unwrap();
        state
            .insert_transfer(t, TransferInfo::new(true, offer, created_ms))
            .unwrap();
        if answered {
            state.accept(&id, None).unwrap();
        }
        (state, id)
    }

    fn items(state: &State, id: &TransferId) -> Vec<ItemState> {
        state.transfer(id).unwrap().items().to_vec()
    }

    fn spool_dir() -> std::path::PathBuf {
        std::env::temp_dir().join("hyakume-recovery-no-spools")
    }

    #[test]
    fn unfinished_work_pauses_while_its_window_lasts() {
        let (mut state, id) = with_transfer(true);
        let found = recover(&mut state, &spool_dir(), START + 1).unwrap();
        assert_eq!(found.paused, 1);
        assert_eq!(items(&state, &id), [ItemState::Paused]);
    }

    #[test]
    fn an_unanswered_offer_ends_as_interrupted() {
        let (mut state, id) = with_transfer(false);
        recover(&mut state, &spool_dir(), START + 1).unwrap();
        assert_eq!(
            state.transfer(&id).unwrap().offer_state(),
            OfferState::Rejected(RejectCause::Interrupted)
        );
    }

    #[test]
    fn work_of_a_device_removed_before_the_restart_is_cancelled() {
        let (mut state, id) = with_transfer(true);
        // The trust record changed, but the process stopped before the
        // transfer was updated.
        state.trust.revoke(&device()).unwrap();
        let found = recover(&mut state, &spool_dir(), START + 1).unwrap();
        assert_eq!(found.revoked, 1);
        assert_eq!(
            items(&state, &id),
            [ItemState::Cancelled(CancelCause::Revoked)]
        );
    }

    #[test]
    fn work_past_its_window_or_under_a_clock_that_went_back_expires() {
        let window = RESUME_WINDOW.as_millis() as u64;
        for now in [START + window, START - 1] {
            let (mut state, id) = with_transfer(true);
            let found = recover(&mut state, &spool_dir(), now).unwrap();
            assert_eq!(found.expired, 1, "at {now}");
            assert_eq!(items(&state, &id), [ItemState::Expired]);
        }
        // Paused work expires the same way while the service runs.
        let (mut state, id) = with_transfer(true);
        recover(&mut state, &spool_dir(), START + 1).unwrap();
        assert!(state.expire(START + window - 1).is_empty());
        assert_eq!(state.expire(START + window), [id]);
    }

    #[test]
    fn a_restart_that_cannot_measure_its_downtime_expires_paused_work() {
        use rand_core::{OsRng, RngCore};
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "hyakume-recovery-{}-{:x}",
            std::process::id(),
            OsRng.next_u64()
        ));
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let db = dir.join("hyakume.db");
        let now = crate::clock::wall_now();
        let state = State::load(crate::storage::Store::open(&db).unwrap()).unwrap();
        let (mut state, id) = transfer_in(state, true, now);
        // The last run read the wall clock a day ahead of where it is now:
        // nobody can tell how long the service was stopped.
        state.lifetime_now(now + 86_400_000, std::time::Instant::now());
        drop(state);
        let mut state = State::load(crate::storage::Store::open(&db).unwrap()).unwrap();
        let at = state.lifetime_now(crate::clock::wall_now(), std::time::Instant::now());
        let found = recover(&mut state, &spool_dir(), at).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(found.expired, 1);
        assert_eq!(items(&state, &id), [ItemState::Expired]);
    }

    #[test]
    fn an_orphan_spool_that_cannot_be_deleted_stays_charged_until_a_retry_deletes_it() {
        use rand_core::{OsRng, RngCore};
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "hyakume-orphans-{}-{:x}",
            std::process::id(),
            OsRng.next_u64()
        ));
        std::fs::create_dir(&dir).unwrap();
        let orphan = dir.join(crate::files::names::spool_name(TransferId::from_bytes(
            [7; 16],
        )));
        std::fs::write(&orphan, [1; 300]).unwrap();
        std::fs::write(dir.join("notes.txt"), b"not a spool").unwrap();
        // Nothing can be removed from the folder.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();

        let mut state = State::in_memory().unwrap();
        let now = state.now_ms();
        let found = recover(&mut state, &dir, now).unwrap();
        let due = state.store().files_due(now).unwrap();
        let first = delete_owned(&due[0]);
        let failed = first.is_err();
        record_deletion(&mut state, &due[0], first, now).unwrap();
        let charged_after_failure = state.store().charged().unwrap();
        let retry_at = state.store().files().unwrap()[0].next_attempt_ms;

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let later = retry_at.unwrap();
        let due_later = state.store().files_due(later).unwrap();
        let second = delete_owned(&due_later[0]);
        record_deletion(&mut state, &due_later[0], second, later).unwrap();
        let orphan_left = orphan.exists();
        let stray_left = dir.join("notes.txt").exists();
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!((found.orphans, found.strays), (1, 1));
        assert_eq!(due.len(), 1);
        assert!(failed);
        assert_eq!(charged_after_failure, (300, 0));
        assert!(retry_at.is_some_and(|t| t > now));
        assert!(!orphan_left);
        assert!(stray_left);
        assert_eq!(state.store().charged().unwrap(), (0, 0));
        assert!(state.store().files().unwrap().is_empty());
    }

    /// A fresh folder in the temporary directory, mode 0700 because
    /// `Destination::qualify` refuses a folder others can write to.
    fn private_dir(tag: &str) -> std::path::PathBuf {
        use rand_core::{OsRng, RngCore};
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "hyakume-{tag}-{}-{:x}",
            std::process::id(),
            OsRng.next_u64()
        ));
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    const RECEIVED: &[u8] = b"every byte verified";

    /// Receives one file into `dir/inbox`, with the state saved in
    /// `dir/hyakume.db`. The rename really happens; the folder sync after
    /// it is what fails.
    fn unsynced_rename(dir: &Path) -> (State, TransferId) {
        use crate::files::destination::PublishFailure;
        use crate::limits::{Budget, Limits};
        use crate::protocol::Message;
        use crate::receive::{Content, Receiver, Target};
        use std::io::Write;

        let inbox = dir.join("inbox");
        std::fs::create_dir(&inbox).unwrap();
        let dest = Destination::qualify(&inbox).unwrap();
        let mut state =
            State::load(crate::storage::Store::open(&dir.join("hyakume.db")).unwrap()).unwrap();
        let reservation = state
            .trust
            .reserve(device(), LocalAlias::new("peer").unwrap())
            .unwrap();
        let authority = state
            .trust_peer(
                VerifiedPeer::new(device(), PeerText::default()),
                reservation,
            )
            .unwrap();
        let budget = Budget::new(Limits::COMPILED);
        let session = budget.admit_session().unwrap();
        let mut rx = Receiver::new(authority, budget, session);

        let id = TransferId::from_bytes([8; 16]);
        let offer = Offer::for_files(id, &[("final.txt", RECEIVED)]).unwrap();
        let target = Target {
            place: dest.place(),
            free_bytes: u64::MAX,
        };
        rx.on_message(Message::Offer(offer.fields()), Some(&target), &mut state);
        rx.on_answer(id, true, Some(&target), &mut state);
        let (mut file, identity) = dest.create_partial(&partial_name(id, 0)).unwrap();
        assert!(
            state
                .file_created(FileKind::Partial, id, 0, identity.key())
                .unwrap()
        );
        let chunk = Message::Chunk {
            transfer: id,
            item: 0,
            offset: 0,
            data: RECEIVED.to_vec(),
        };
        rx.on_message(chunk, None, &mut state);
        file.write_all(RECEIVED).unwrap();
        file.sync_all().unwrap();
        rx.on_message(
            Message::ItemEnd {
                transfer: id,
                item: 0,
            },
            None,
            &mut state,
        );
        let content = Content {
            digest: offer.items()[0].digest,
            text_ok: true,
        };
        rx.on_verified(0, content, &mut state, |partial, output| {
            dest.publish(partial, output.as_str()).unwrap();
            Err(PublishFailure::Unsynced(io::Error::other("EIO")))
        });
        assert_eq!(items(&state, &id), [ItemState::Unconfirmed(None)]);
        (state, id)
    }

    /// Deletes every file that is due, as maintenance does.
    fn clean(state: &mut State) {
        for file in state.store().files_due(u64::MAX).unwrap() {
            let result = delete_owned(&file);
            record_deletion(state, &file, result, 0).unwrap();
        }
    }

    #[test]
    fn an_unconfirmed_rename_survives_cancel_revocation_expiry_and_cleanup_until_recovery_publishes_it()
     {
        for action in ["nothing", "cancel", "revoke", "expire"] {
            let dir = private_dir("unconfirmed");
            let (mut state, id) = unsynced_rename(&dir);
            match action {
                "cancel" => {
                    state.request_cancel(id);
                }
                "revoke" => state.revoke(&device()).unwrap(),
                "expire" => {
                    let at = state.info(&id).unwrap().expires_ms;
                    state.expire(at);
                }
                _ => {}
            }
            clean(&mut state);
            drop(state);

            let store = crate::storage::Store::open(&dir.join("hyakume.db")).unwrap();
            let mut state = State::load(store).unwrap();
            let now = state.now_ms();
            let found = recover(&mut state, &spool_dir(), now).unwrap();
            clean(&mut state);
            let received = std::fs::read(dir.join("inbox").join("final.txt")).unwrap();
            let charged = state.store().charged().unwrap();
            std::fs::remove_dir_all(&dir).unwrap();

            assert_eq!(found.published, 1, "after {action}");
            assert_eq!(items(&state, &id), [ItemState::Published], "after {action}");
            assert_eq!(received, RECEIVED, "after {action}");
            assert_eq!(charged, (0, 0), "after {action}");
        }
    }

    #[test]
    fn maintenance_settles_an_unconfirmed_rename_from_the_disk() {
        let cases = [
            (false, ItemState::Published),
            // The person removed the file before its folder synced.
            (true, ItemState::Failed(FailCause::Lost)),
        ];
        for (removed, expected) in cases {
            let dir = private_dir("unconfirmed");
            let (mut state, id) = unsynced_rename(&dir);
            if removed {
                std::fs::remove_file(dir.join("inbox").join("final.txt")).unwrap();
            }
            let waiting = unsettled(&mut state).unwrap();
            for item in &waiting {
                let evidence = recheck(item);
                settle(&mut state, item, evidence);
            }
            clean(&mut state);
            let charged = state.store().charged().unwrap();
            std::fs::remove_dir_all(&dir).unwrap();

            assert_eq!(waiting.len(), 1);
            assert_eq!(items(&state, &id), [expected], "removed: {removed}");
            assert_eq!(charged, (0, 0), "removed: {removed}");
            assert!(unsettled(&mut state).unwrap().is_empty());
        }
    }

    /// Runs one maintenance check of every unconfirmed item.
    fn maintain(state: &mut State) {
        for item in unsettled(state).unwrap() {
            let evidence = recheck(&item);
            settle(state, &item, evidence);
        }
    }

    #[test]
    fn an_unreadable_folder_leaves_an_unconfirmed_rename_for_a_later_check() {
        use std::os::unix::fs::PermissionsExt;
        let dir = private_dir("unreadable");
        let inbox = dir.join("inbox");
        let (mut state, id) = unsynced_rename(&dir);
        let mode = |m| std::fs::set_permissions(&inbox, std::fs::Permissions::from_mode(m));

        // Neither maintenance nor a restart may take an access error for a
        // missing file, or cleanup would drop the row that names it.
        mode(0o000).unwrap();
        maintain(&mut state);
        let while_running = items(&state, &id);
        clean(&mut state);
        drop(state);
        let store = crate::storage::Store::open(&dir.join("hyakume.db")).unwrap();
        let mut state = State::load(store).unwrap();
        let now = state.now_ms();
        let found = recover(&mut state, &spool_dir(), now).unwrap();
        let after_restart = items(&state, &id);
        clean(&mut state);

        mode(0o700).unwrap();
        maintain(&mut state);
        clean(&mut state);
        let charged = state.store().charged().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(while_running, [ItemState::Unconfirmed(None)]);
        assert_eq!((found.unreadable, found.lost), (1, 0));
        assert_eq!(after_restart, [ItemState::Unconfirmed(None)]);
        assert_eq!(items(&state, &id), [ItemState::Published]);
        assert_eq!(charged, (0, 0));
    }

    /// A stop that reaches an unconfirmed item.
    type Stopping = fn(State, &Path, TransferId) -> State;
    /// How the disk check then happens.
    type Checking = fn(State, &Path) -> State;

    fn cancel(mut state: State, _: &Path, id: TransferId) -> State {
        state.request_cancel(id);
        state
    }

    fn revoke(mut state: State, _: &Path, _: TransferId) -> State {
        state.revoke(&device()).unwrap();
        state
    }

    fn reopen(dir: &Path) -> State {
        State::load(crate::storage::Store::open(&dir.join("hyakume.db")).unwrap()).unwrap()
    }

    /// A restart with the folder unreadable, while the wall clock reads a
    /// day behind its last saved reading, so the downtime is unknown.
    fn restart_with_unknown_downtime(mut state: State, dir: &Path, _: TransferId) -> State {
        use std::os::unix::fs::PermissionsExt;
        let now = crate::clock::wall_now();
        state.lifetime_now(now + 86_400_000, std::time::Instant::now());
        drop(state);
        let inbox = dir.join("inbox");
        let mode = |m| std::fs::set_permissions(&inbox, std::fs::Permissions::from_mode(m));
        mode(0o000).unwrap();
        let mut state = reopen(dir);
        let at = state.lifetime_now(crate::clock::wall_now(), std::time::Instant::now());
        let found = recover(&mut state, &spool_dir(), at).unwrap();
        mode(0o700).unwrap();
        assert!(state.downtime_unknown());
        assert_eq!((found.unreadable, found.expired), (1, 1));
        state
    }

    fn by_maintenance(mut state: State, _: &Path) -> State {
        maintain(&mut state);
        state
    }

    fn by_restart(state: State, dir: &Path) -> State {
        drop(state);
        let mut state = reopen(dir);
        let now = state.now_ms();
        recover(&mut state, &spool_dir(), now).unwrap();
        state
    }

    #[test]
    fn a_kept_stop_ends_the_item_once_the_disk_shows_no_rename() {
        let stops: [(&str, Stopping, ItemState); 3] = [
            (
                "cancel",
                cancel,
                ItemState::Cancelled(CancelCause::Receiver),
            ),
            ("revoke", revoke, ItemState::Cancelled(CancelCause::Revoked)),
            (
                "unknown downtime",
                restart_with_unknown_downtime,
                ItemState::Expired,
            ),
        ];
        let checks: [(&str, Checking); 2] =
            [("maintenance", by_maintenance), ("restart", by_restart)];
        for (stop, stopping, expected) in stops {
            for (check, checking) in checks {
                let dir = private_dir("undone");
                let inbox = dir.join("inbox");
                let (state, id) = unsynced_rename(&dir);
                // The rename did not survive: the file is back under its
                // partial name, as after a crash.
                let partial = inbox.join(partial_name(id, 0));
                std::fs::rename(inbox.join("final.txt"), &partial).unwrap();
                let state = stopping(state, &dir, id);
                let before_check = items(&state, &id);
                let mut state = checking(state, &dir);
                clean(&mut state);
                let charged = state.store().charged().unwrap();
                let partial_left = partial.exists();
                std::fs::remove_dir_all(&dir).unwrap();

                let case = format!("{stop}, then {check}");
                assert!(before_check[0].is_unconfirmed(), "{case}: {before_check:?}");
                assert_eq!(items(&state, &id), [expected], "{case}");
                assert_eq!(charged, (0, 0), "{case}");
                assert!(!partial_left, "{case}");
            }
        }
    }

    fn file(attempts: u32, since: u64) -> OwnedFile {
        OwnedFile {
            id: 1,
            kind: FileKind::Partial,
            transfer: TransferId::from_bytes([1; 16]),
            item: 0,
            dir: Place {
                dir: b"/nowhere".to_vec(),
                dev: 0,
                ino: 0,
            },
            name: b"x".to_vec(),
            identity: None,
            bytes: 10,
            state: FileState::Delete,
            attempts,
            delete_since_ms: Some(since),
            next_attempt_ms: None,
            last_error: None,
        }
    }

    #[test]
    fn retries_grow_and_stop_at_the_end_of_the_window() {
        let window = CLEANUP_WINDOW.as_millis() as u64;
        assert_eq!(next_attempt(&file(0, 0), 0), Some(10_000));
        assert_eq!(next_attempt(&file(1, 0), 0), Some(40_000));
        assert_eq!(next_attempt(&file(9, 0), 0), Some(3_600_000));
        assert_eq!(next_attempt(&file(9, 0), window - 1000), None);
        // A clock moved behind the window's start stops retrying.
        assert_eq!(next_attempt(&file(0, 5_000), 1_000), None);
    }
}
