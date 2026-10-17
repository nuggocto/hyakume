//! The receiving side of a session: consent, partial files, and
//! publication. A verified item is renamed into place inside the state
//! lock, so a revocation or cancel either comes first or finds it published.

use std::collections::VecDeque;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::Arc;

use iroh::Endpoint;
use iroh::endpoint::Connection;
use tokio::sync::oneshot;

use super::connections::exporter;
use super::wire::Wire;
use super::{NodeError, Prompt, Shared};
use crate::files::destination::{Destination, PublishFailure};
use crate::ids::TransferId;
use crate::limits::{BufferLease, MIN_FREE_BYTES};
use crate::protocol::{self, Message};
use crate::receive::{self, ContentCheck, Receiver, Target};
use crate::session::{self, SessionBinding, Side};
use crate::storage::FileKind;
use crate::tasks;

/// Owns a session's receiver and ends its unfinished work however the
/// session task stops: an error, a lost connection, or abort.
pub(super) struct ReceiverGuard {
    shared: Arc<Shared>,
    receiver: Receiver,
}

impl Drop for ReceiverGuard {
    fn drop(&mut self) {
        let receiver = &mut self.receiver;
        self.shared.change(|s| receiver.on_disconnect(s));
    }
}

/// The partial file of the item being received, and what its bytes showed
/// so far.
pub(super) struct Partial {
    transfer: TransferId,
    item: usize,
    file: Arc<File>,
    check: Option<ContentCheck>,
}

/// Where an offer would go now: the destination and its free space.
pub(super) fn target_now(dest: &Option<Arc<Destination>>) -> Option<(Arc<Destination>, u64)> {
    let dest = dest.clone()?;
    let free = dest.free_bytes().ok()?;
    Some((dest, free))
}

pub(super) async fn receive_session(
    shared: &Arc<Shared>,
    endpoint: &Endpoint,
    conn: &Connection,
    wire: &mut Wire,
    negotiation: [u8; 32],
) -> Result<(), NodeError> {
    let deadline = shared.limits().handshake_deadline;
    let proof = match wire.message(deadline).await? {
        Message::SessionProof(p) => p,
        _ => return Err(NodeError::Unexpected),
    };
    let binding = SessionBinding {
        exporter: exporter(conn, session::EXPORTER_LABEL)?,
        dialer_endpoint: *conn.remote_id().as_bytes(),
        listener_endpoint: *endpoint.id().as_bytes(),
        negotiation,
    };

    // Check the dialer before revealing this device's identity.
    let device = session::verify(&proof, &binding, Side::Dialer)?;
    let authority = shared.lock().trust.authorize(&device)?;
    let slot = shared.budget.admit_session()?;
    wire.slot.attach()?;
    wire.write(&Message::SessionProof(session::prove(
        &shared.key,
        &binding,
        Side::Listener,
    )))
    .await?;

    wire.end_handshake();
    let mut guard = ReceiverGuard {
        shared: shared.clone(),
        receiver: Receiver::new(authority, shared.budget.clone(), slot),
    };
    let receiver = &mut guard.receiver;

    let mut changes = shared.cell.subscribe();
    let mut answer: Option<(TransferId, oneshot::Receiver<bool>)> = None;
    // The destination of the transfer being received, fixed when it was
    // accepted, and the current item's partial file.
    let mut accepted_into: Option<Arc<Destination>> = None;
    let mut partial: Option<Partial> = None;
    let idle = shared.limits().idle_timeout;

    loop {
        // A chunk's buffer lease lives until its bytes are written.
        let mut held: Option<BufferLease> = None;
        let outputs = tokio::select! {
            frame = wire.next_by(wire.idle_until(idle)) => {
                let decoded = frame.and_then(|f| {
                    let msg = protocol::decode_payload(&f.payload).map_err(NodeError::from)?;
                    held = Some(f._lease);
                    Ok(msg)
                });
                match decoded {
                    Ok(msg) => {
                        let target = match msg {
                            Message::Offer(_) => target_now(&shared.destination()),
                            _ => None,
                        };
                        shared.change(|s| {
                            let t = target.as_ref().map(|(d, free)| Target { place: d.place(), free_bytes: *free });
                            receiver.on_message(msg, t.as_ref(), s)
                        })
                    }
                    // The guard ends unfinished work on every way out.
                    Err(NodeError::Ended) if receiver.awaiting_answer().is_none() => return Ok(()),
                    Err(e) => return Err(e),
                }
            },
            reply = async { match answer.as_mut() { Some((_, rx)) => rx.await, None => std::future::pending().await } } => {
                let Some((transfer, _)) = answer.take() else {
                    continue;
                };
                match reply {
                    // Dropped without an answer, because nobody could take
                    // the question: refused as busy. Only a person declines.
                    Err(_) => shared.change(|s| receiver.on_unasked(transfer, s)),
                    Ok(accept) => {
                        let target = target_now(&shared.destination());
                        let outputs = shared.change(|s| {
                            let t = target.as_ref().map(|(d, free)| Target { place: d.place(), free_bytes: *free });
                            receiver.on_answer(transfer, accept, t.as_ref(), s)
                        });
                        if receiver.receiving().is_some() {
                            accepted_into = target.map(|(d, _)| d);
                        }
                        outputs
                    }
                }
            }
            _ = changes.changed() => shared.wake(|s| receiver.on_wake(s)),
        };

        let mut queue: VecDeque<receive::Output> = outputs.into();
        while let Some(out) = queue.pop_front() {
            match out {
                receive::Output::Send(msg) => {
                    let stop = shared.until(|s| receiver.should_stop(s));
                    match wire.write_or_stop(&msg, stop).await {
                        Ok(()) => {}
                        Err(NodeError::Stopped) => {
                            // As on the sending side: the reset stream tells
                            // the peer.
                            shared.change(|s| receiver.on_wake(s));
                            return Ok(());
                        }
                        Err(e) => return Err(e),
                    }
                }
                receive::Output::Ask { transfer, view } => {
                    let from = device;
                    let digest = receiver_offer_digest(shared, transfer);
                    let asked = shared.ask(|answer| Prompt::Offer {
                        transfer,
                        from,
                        view,
                        digest,
                        answer,
                    });
                    match asked {
                        Some(rx) => answer = Some((transfer, rx)),
                        None => queue.extend(shared.change(|s| receiver.on_unasked(transfer, s))),
                    }
                }
                receive::Output::Write { item, offset, data } => {
                    let lease = held.take();
                    let written = write_partial(
                        shared,
                        receiver,
                        &accepted_into,
                        &mut partial,
                        item,
                        offset,
                        data,
                        lease,
                    )
                    .await?;
                    if !written {
                        partial = None;
                        queue.extend(shared.change(|s| receiver.on_storage_failure(s)));
                    }
                }
                receive::Output::Verify { item } => {
                    let finished =
                        finish_partial(shared, receiver, &accepted_into, &mut partial, item)
                            .await?;
                    let Some((dest, content)) = finished else {
                        partial = None;
                        queue.extend(shared.change(|s| receiver.on_storage_failure(s)));
                        continue;
                    };
                    partial = None;
                    queue.extend(shared.change(|s| {
                        receiver.on_verified(item, content, s, |name, output| {
                            tasks::crash_point("before-rename");
                            let published = dest.publish(name, output.as_str());
                            tasks::crash_point("published-unrecorded");
                            if let Err(e @ PublishFailure::Unsynced(_)) = &published {
                                tracing::warn!(error = %e, "a received file is in place but not confirmed; it stays paused until recovery");
                            }
                            published
                        })
                    }));
                }
                receive::Output::Abort(code) => {
                    wire.write(&Message::Error(code)).await.ok();
                    return Ok(());
                }
                receive::Output::End => return Ok(()),
            }
        }
    }
}

pub(super) fn receiver_offer_digest(shared: &Shared, transfer: TransferId) -> [u8; 32] {
    shared
        .lock()
        .info(&transfer)
        .map_or([0; 32], |i| i.offer.digest())
}

/// Opens the item's partial file if it is not open yet, recording its
/// identity in the file ledger. Returns false if it cannot be created.
pub(super) async fn open_partial(
    shared: &Arc<Shared>,
    receiver: &Receiver,
    dest: &Arc<Destination>,
    partial: &mut Option<Partial>,
    item: usize,
) -> Result<bool, NodeError> {
    let Some(offer) = receiver.receiving() else {
        return Ok(false);
    };
    let transfer = offer.transfer();
    if partial
        .as_ref()
        .is_some_and(|p| p.transfer == transfer && p.item == item)
    {
        return Ok(true);
    }

    let check = ContentCheck::new(offer);
    let name = crate::files::names::partial_name(transfer, item);
    let job_dest = dest.clone();
    let created = shared
        .blocking(move || job_dest.create_partial(&name))
        .await?;
    match created {
        Ok((file, identity)) => {
            let key = identity.key();
            let recorded =
                shared.change(|s| s.file_created(FileKind::Partial, transfer, item, key));
            if !matches!(recorded, Ok(true)) {
                // Without its inode on record, recovery could never tell this
                // file from another under the output name, and cleanup would
                // never find it, so nothing of it may be used. It goes now,
                // before any byte is in it.
                let (dest, name) = (
                    dest.clone(),
                    crate::files::names::partial_name(transfer, item),
                );
                let removed = shared
                    .blocking(move || dest.remove(name.as_bytes(), Some(key)))
                    .await?;
                if let Err(e) = removed {
                    tracing::warn!(error = %e, "could not remove an unrecorded partial file");
                }
                return Ok(false);
            }
            *partial = Some(Partial {
                transfer,
                item,
                file: Arc::new(file),
                check: Some(check),
            });
            Ok(true)
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not create a partial file");
            Ok(false)
        }
    }
}

/// Writes received bytes to the partial file, refusing to fill the disk.
/// Returns false on a storage failure.
#[allow(clippy::too_many_arguments)]
pub(super) async fn write_partial(
    shared: &Arc<Shared>,
    receiver: &Receiver,
    dest: &Option<Arc<Destination>>,
    partial: &mut Option<Partial>,
    item: usize,
    offset: u64,
    data: Vec<u8>,
    lease: Option<BufferLease>,
) -> Result<bool, NodeError> {
    let Some(dest) = dest else {
        return Ok(false);
    };
    if !open_partial(shared, receiver, dest, partial, item).await? {
        return Ok(false);
    }
    let Some(p) = partial.as_mut() else {
        return Ok(false);
    };

    let (file, mut check, dest) = (p.file.clone(), p.check.take(), dest.clone());
    let (written, check) = shared
        .blocking(move || {
            let free = dest.free_bytes();
            let result = match free {
                Ok(free) if free >= (data.len() as u64).saturating_add(MIN_FREE_BYTES) => {
                    file.write_all_at(&data, offset)
                }
                Ok(_) => Err(std::io::Error::other("not enough free disk space")),
                Err(e) => Err(e),
            };
            if result.is_ok()
                && let Some(c) = check.as_mut()
            {
                c.update(&data);
            }
            drop(lease);
            (result, check)
        })
        .await?;
    p.check = check;
    if let Err(e) = written {
        tracing::warn!(error = %e, "writing a partial file failed");
        return Ok(false);
    }
    Ok(true)
}

/// Syncs the item's partial file, creating it first for an empty item, and
/// reports what its bytes showed. `None` on a storage failure.
pub(super) async fn finish_partial(
    shared: &Arc<Shared>,
    receiver: &Receiver,
    dest: &Option<Arc<Destination>>,
    partial: &mut Option<Partial>,
    item: usize,
) -> Result<Option<(Arc<Destination>, receive::Content)>, NodeError> {
    let Some(dest) = dest else {
        return Ok(None);
    };
    if !open_partial(shared, receiver, dest, partial, item).await? {
        return Ok(None);
    }
    let Some(p) = partial.take() else {
        return Ok(None);
    };
    let Some(check) = p.check else {
        return Ok(None);
    };

    let file = p.file;
    let synced = shared
        .blocking(move || file.sync_all().map(|()| check.finish()))
        .await?;
    match synced {
        Ok(content) => Ok(Some((dest.clone(), content))),
        Err(e) => {
            tracing::warn!(error = %e, "syncing a partial file failed");
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::PeerText;
    use crate::identity::DeviceKey;
    use crate::node::test_shared;
    use crate::offer::Offer;
    use crate::state::State;
    use crate::trust::{LocalAlias, VerifiedPeer};
    use rand_core::{OsRng, RngCore};
    use zeroize::Zeroizing;

    /// Opens the partial file of a one-item receive after `lose_record`
    /// has made its record impossible, and checks that the file is neither
    /// used nor left behind.
    async fn open_partial_without_its_record(lose_record: impl FnOnce(&mut State, TransferId)) {
        let shared = Arc::new(test_shared());
        let dir = std::env::temp_dir().join(format!(
            "hyakume-node-unrecorded-{}-{:x}",
            std::process::id(),
            OsRng.next_u64()
        ));
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .unwrap();
        let dest = Arc::new(Destination::qualify(&dir).unwrap());

        // A receiver that accepted a one-item offer.
        let peer = DeviceKey::from_seed(&Zeroizing::new([2; 32])).id();
        let authority = shared
            .change(|s| {
                let verified = VerifiedPeer::new(peer, PeerText::default());
                s.trust.trust(verified, LocalAlias::new("peer").unwrap())
            })
            .unwrap();
        let session = shared.budget.admit_session().unwrap();
        let mut receiver = Receiver::new(authority, shared.budget.clone(), session);
        let transfer = TransferId::from_bytes([7; 16]);
        let offer = Offer::for_files(transfer, &[("f", b"abc")]).unwrap();
        let target = Target {
            place: dest.place(),
            free_bytes: u64::MAX,
        };
        shared.change(|s| receiver.on_message(Message::Offer(offer.fields()), Some(&target), s));
        shared.change(|s| receiver.on_answer(transfer, true, Some(&target), s));
        assert!(receiver.receiving().is_some());

        shared.change(|s| lose_record(s, transfer));
        let mut partial = None;
        let opened = open_partial(&shared, &receiver, &dest, &mut partial, 0)
            .await
            .unwrap();
        assert!(!opened);
        assert!(partial.is_none());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_partial_file_whose_record_cannot_be_saved_is_never_used() {
        // The disk under the state database starts failing.
        open_partial_without_its_record(|s, _| s.store().refuse_writes()).await;
    }

    #[tokio::test]
    async fn a_partial_file_whose_reservation_is_gone_is_never_used() {
        // A cancel and cleanup removed the reservation before the file was
        // created.
        open_partial_without_its_record(|s, transfer| {
            s.store().delete_files_of(transfer, 0).unwrap();
            for file in s.store().files_due(u64::MAX).unwrap() {
                s.store().file_deleted(file.id).unwrap();
            }
        })
        .await;
    }
}
