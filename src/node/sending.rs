//! The sending side of a session: the recorded offer, then the chunks the
//! [`Sender`] asks for, hashed as they go so a changed source fails.

use std::collections::VecDeque;
use std::sync::Arc;

use iroh::endpoint::Connection;
use iroh::{Endpoint, EndpointAddr};
use tokio::time::timeout_at;

use super::connections::{dial, exporter};
use super::wire::{Frame, Wire};
use super::{NodeError, Prepared, SendOptions, Shared};
use crate::files::source::Source;
use crate::identity::DeviceId;
use crate::ids::TransferId;
use crate::limits::{Refusal, chunk_cost};
use crate::protocol::{self, Message, Purpose};
use crate::send::{self, Sender};
use crate::session::{self, SessionBinding, Side};
use crate::state::TransferInfo;
use crate::tasks;
use crate::transfer::FailCause;

/// Everything a send needs once its transfer is recorded.
pub(super) struct Outgoing {
    pub(super) transfer: TransferId,
    to: DeviceId,
    at: EndpointAddr,
    sender: Sender,
    sources: Vec<Source>,
    options: SendOptions,
    /// Ends the transfer if this send is dropped unfinished, even before
    /// its task first runs, such as when shutdown finds it still queued.
    unfinished: SendGuard,
}

/// Checks trust and admission and records the offered transfer. Nothing is
/// recorded when this fails.
pub(super) fn begin_send(
    shared: &Arc<Shared>,
    to: DeviceId,
    at: EndpointAddr,
    prepared: Prepared,
    options: SendOptions,
) -> Result<Outgoing, NodeError> {
    let authority = shared.lock().trust.authorize(&to)?;
    let offer = prepared.offer()?;
    let session = shared.budget.admit_session()?;

    let sender = shared.change(|s| {
        let mut info = TransferInfo::new(true, offer.clone(), s.now_ms());
        info.sources = prepared.paths;
        Sender::new(
            offer,
            options.chunk_bytes,
            authority,
            shared.budget.clone(),
            session,
            info,
            s,
        )
    })?;

    Ok(Outgoing {
        transfer: prepared.transfer,
        to,
        at,
        sender,
        sources: prepared.items.into_iter().map(|i| i.source).collect(),
        options,
        unfinished: SendGuard {
            shared: shared.clone(),
            transfer: prepared.transfer,
        },
    })
}

/// Runs a recorded send to its end. Returns an error only when it stopped
/// before the offer reached the other device.
pub(super) async fn run_send(
    shared: Arc<Shared>,
    endpoint: Endpoint,
    out: Outgoing,
) -> Result<(), NodeError> {
    let Outgoing {
        transfer,
        to,
        at,
        mut sender,
        sources,
        options,
        unfinished: _unfinished,
    } = out;

    let (conn, mut wire, negotiation) = match dial(&shared, &endpoint, at, Purpose::Session).await {
        Ok(dialed) => dialed,
        Err(e) => {
            shared.change(|s| sender.on_disconnect(s));
            return Err(e);
        }
    };

    let mut offered = false;
    let result = send_session(
        &shared,
        &conn,
        &mut wire,
        *endpoint.id().as_bytes(),
        negotiation,
        to,
        &mut sender,
        &sources,
        options,
        &mut offered,
    )
    .await;

    if let Err(e) = &result {
        wire.send_error(e).await;
        let cause = match e {
            NodeError::Refused(_) => FailCause::Busy,
            _ => FailCause::Connection,
        };
        shared.change(|s| s.interrupt(&transfer, cause));
    }

    wire.finish_dialer(&conn, shared.limits().idle_timeout)
        .await;
    match result {
        Err(e) if !offered => Err(e),
        _ => Ok(()),
    }
}

/// Ends a sent transfer that is still unfinished when its task ends or is
/// dropped: the caller gave it up, so it is cancelled, or paused while the
/// node stops.
pub(super) struct SendGuard {
    shared: Arc<Shared>,
    transfer: TransferId,
}

impl Drop for SendGuard {
    fn drop(&mut self) {
        let transfer = self.transfer;
        self.shared.change(|s| {
            let pausing = s.pausing();
            s.update_transfer(&transfer, |t| {
                if pausing {
                    t.pause_all();
                } else {
                    t.cancel(crate::transfer::CancelCause::Sender);
                }
            });
        });
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn send_session(
    shared: &Arc<Shared>,
    conn: &Connection,
    wire: &mut Wire,
    me: [u8; 32],
    negotiation: [u8; 32],
    to: DeviceId,
    sender: &mut Sender,
    sources: &[Source],
    options: SendOptions,
    offered: &mut bool,
) -> Result<(), NodeError> {
    let deadline = shared.limits().handshake_deadline;
    let binding = SessionBinding {
        exporter: exporter(conn, session::EXPORTER_LABEL)?,
        dialer_endpoint: me,
        listener_endpoint: *conn.remote_id().as_bytes(),
        negotiation,
    };

    wire.write(&Message::SessionProof(session::prove(
        &shared.key,
        &binding,
        Side::Dialer,
    )))
    .await?;

    let proof = match wire.message(deadline).await? {
        Message::SessionProof(p) => p,
        _ => return Err(NodeError::Unexpected),
    };
    if session::verify(&proof, &binding, Side::Listener)? != to {
        return Err(NodeError::WrongPeer);
    }
    wire.end_handshake();

    // Trust may have changed while connecting.
    let stop = shared.change(|s| sender.check_stop(s));
    if !stop.is_empty() {
        return Ok(());
    }

    wire.write(&sender.offer_message()).await?;
    *offered = true;

    let mut changes = shared.cell.subscribe();
    let idle = shared.limits().idle_timeout;
    // Hashed as sent, to notice a file that changed underneath.
    let mut sent: Option<blake3::Hasher> = None;

    while !sender.is_done() {
        let mut lease = None;
        let outputs = if sender.is_sending() {
            let cost = chunk_cost(sender.next_chunk_len());
            let until = wire.idle_until(idle);
            // Waiting for buffer room still notices a stop, a reply, or a
            // closed connection. None of them needs a lease.
            let waited = tokio::select! {
                biased;
                lease = timeout_at(until, shared.budget.lease_send_wait(cost)) => Ok(lease),
                () = shared.until(|s| sender.should_stop(s)) => {
                    Err(shared.change(|s| sender.check_stop(s)))
                }
                frame = wire.next_by(until) => Err(incoming(shared, sender, frame)),
            };
            match waited {
                Err(outputs) => outputs,
                Ok(granted) => {
                    lease = Some(granted.map_err(|_| Refusal::Buffer)??);
                    // Wait for permission to send (tests only), while still
                    // noticing replies and local changes.
                    if let Some(gate) = &options.gate {
                        tokio::select! {
                            permit = gate.acquire() => {
                                if let Ok(p) = permit { p.forget(); }
                                shared.change(|s| sender.next_frame(s)).into_iter().collect()
                            }
                            frame = wire.next_by(wire.idle_until(idle)) => incoming(shared, sender, frame),
                            _ = changes.changed() => shared.wake(|s| sender.check_stop(s)),
                        }
                    } else {
                        match wire.try_next() {
                            Some(frame) => incoming(shared, sender, frame),
                            None => shared
                                .change(|s| sender.next_frame(s))
                                .into_iter()
                                .collect(),
                        }
                    }
                }
            }
        } else {
            tokio::select! {
                frame = wire.next_by(wire.idle_until(idle)) => incoming(shared, sender, frame),
                _ = changes.changed() => shared.wake(|s| sender.on_wake(s)),
            }
        };

        let mut queue: VecDeque<send::Output> = outputs.into();
        while let Some(out) = queue.pop_front() {
            match out {
                send::Output::Send(msg) => {
                    if let Some(end) = send_or_stop(shared, wire, sender, &msg).await? {
                        return Ok(end);
                    }
                }
                send::Output::Read { item, offset, len } => {
                    let source = sources[item].clone();
                    let read = shared.blocking(move || source.read_at(offset, len)).await?;
                    let data = match read {
                        Ok(data) => data,
                        Err(e) => {
                            tracing::warn!(error = %e, "reading a source failed");
                            let cause = if e.kind() == std::io::ErrorKind::UnexpectedEof {
                                FailCause::SourceChanged
                            } else {
                                FailCause::Storage
                            };
                            queue.extend(shared.change(|s| sender.on_read_failure(cause, s)));
                            continue;
                        }
                    };

                    if offset == 0 {
                        sent = Some(blake3::Hasher::new());
                        tasks::crash_point("sending");
                    }
                    if let Some(hasher) = sent.as_mut() {
                        hasher.update(&data);
                    }

                    let msg = sender.chunk(item, offset, data);
                    if let Some(end) = send_or_stop(shared, wire, sender, &msg).await? {
                        return Ok(end);
                    }
                    // The chunk is written; its buffer can go.
                    lease = None;
                }
                send::Output::Check { item } => {
                    let digest = sent.take().unwrap_or_default().finalize();
                    let source = sources[item].clone();
                    let unchanged = shared.blocking(move || source.unchanged()).await?;
                    let expected = sender.item_digest(item);
                    let failure = match unchanged {
                        Ok(true) if Some(*digest.as_bytes()) == expected => None,
                        Ok(_) => Some(FailCause::SourceChanged),
                        Err(_) => Some(FailCause::Storage),
                    };
                    queue.extend(shared.change(|s| sender.end_item(failure, s)));
                }
                send::Output::Abort(code) => {
                    wire.write(&Message::Error(code)).await.ok();
                    return Ok(());
                }
                send::Output::End => return Ok(()),
            }
        }
        drop(lease);
    }
    Ok(())
}

/// Writes one message for the sender, giving up when the transfer must
/// stop. Returns `Some(())` when the session is over.
pub(super) async fn send_or_stop(
    shared: &Arc<Shared>,
    wire: &mut Wire,
    sender: &mut Sender,
    msg: &Message,
) -> Result<Option<()>, NodeError> {
    let stop = shared.until(|s| sender.should_stop(s));
    match wire.write_or_stop(msg, stop).await {
        Ok(()) => Ok(None),
        Err(NodeError::Stopped) => {
            // The stream is reset, so the peer learns of the stop from the
            // closed connection.
            shared.change(|s| sender.check_stop(s));
            Ok(Some(()))
        }
        Err(NodeError::ConnectionLost) => {
            // The receiver stopped reading; its reason may be queued.
            let frame = wire.next(shared.limits().handshake_deadline).await;
            incoming(shared, sender, frame);
            if !sender.is_done() {
                shared.change(|s| sender.on_disconnect(s));
            }
            Ok(Some(()))
        }
        Err(e) => Err(e),
    }
}

pub(super) fn incoming(
    shared: &Shared,
    sender: &mut Sender,
    frame: Result<Frame, NodeError>,
) -> Vec<send::Output> {
    match frame.and_then(|f| protocol::decode_payload(&f.payload).map_err(NodeError::from)) {
        Ok(msg) => shared.change(|s| sender.on_message(msg, s)),
        Err(NodeError::Protocol(e)) => {
            shared.change(|s| sender.on_disconnect(s));
            vec![send::Output::Abort(e.code())]
        }
        Err(_) => {
            shared.change(|s| sender.on_disconnect(s));
            vec![send::Output::End]
        }
    }
}
