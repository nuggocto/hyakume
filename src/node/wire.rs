//! Frames over one QUIC stream. Every read and write has a deadline, so a
//! slow or silent peer cannot hold a connection.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use iroh::endpoint::{Connection, ReadExactError, RecvStream, SendStream, VarInt};
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout_at};

use super::{CLOSE_DONE, CLOSE_STOPPED, FRAME_QUEUE, NodeError, Shared};
use crate::limits::{Budget, BufferLease, ConnectionSlot};
use crate::protocol::{self, ErrorCode, Message, ProtocolError};
use crate::tasks;

pub(super) struct Frame {
    pub(super) payload: Vec<u8>,
    pub(super) _lease: BufferLease,
}

/// One bidirectional stream. A reader task owns the receive half and hands
/// over whole frames, so waiting for a frame can be cancelled without
/// splitting one.
pub(super) struct Wire {
    conn: Connection,
    /// Counts the connection against the node's limit while it is open.
    pub(super) slot: ConnectionSlot,
    send: SendStream,
    frames: mpsc::Receiver<Result<Frame, NodeError>>,
    reader: JoinHandle<()>,
    /// How long writing one frame may take.
    write_deadline: Duration,
    /// Until the peer has proved who it is, no read, write, or close on
    /// this connection waits past this instant.
    handshake: Option<Instant>,
    /// When this session last read or wrote a frame. Other sessions' work
    /// does not count, so it cannot keep this one from going idle.
    active: Instant,
    /// A write stopped partway, so nothing more can follow on this stream.
    broken: bool,
}

impl Wire {
    pub(super) fn new(
        conn: &Connection,
        send: SendStream,
        recv: RecvStream,
        shared: &Shared,
        slot: ConnectionSlot,
        handshake: Instant,
    ) -> Self {
        let (tx, frames) = mpsc::channel(FRAME_QUEUE);
        let reader = tokio::spawn(read_frames(
            recv,
            shared.budget.clone(),
            shared.read_gate.clone(),
            tx,
        ));
        Self {
            conn: conn.clone(),
            slot,
            send,
            frames,
            reader,
            write_deadline: shared.limits().idle_timeout,
            handshake: Some(handshake),
            active: Instant::now(),
            broken: false,
        }
    }

    /// The peer has proved who it is; waits are no longer capped by the
    /// handshake deadline.
    pub(super) fn end_handshake(&mut self) {
        self.handshake = None;
    }

    /// `until`, or the handshake deadline if that comes first.
    pub(super) fn cap(&self, until: Instant) -> Instant {
        self.handshake.map_or(until, |h| h.min(until))
    }

    /// When the session counts as idle: `idle` after its last frame in or
    /// out.
    pub(super) fn idle_until(&self, idle: Duration) -> Instant {
        self.active + idle
    }

    pub(super) async fn write(&mut self, msg: &Message) -> Result<(), NodeError> {
        self.write_or_stop(msg, std::future::pending()).await
    }

    /// Writes `msg`, giving up as soon as `stop` resolves.
    pub(super) async fn write_or_stop(
        &mut self,
        msg: &Message,
        stop: impl Future<Output = ()>,
    ) -> Result<(), NodeError> {
        let frame = protocol::encode_frame(msg)?;
        self.write_parts(&[&frame], stop).await
    }

    pub(super) async fn write_payload(&mut self, payload: &[u8]) -> Result<(), NodeError> {
        let len = u32::try_from(payload.len()).map_err(|_| ProtocolError::TooLarge)?;
        self.write_parts(&[&len.to_be_bytes(), payload], std::future::pending())
            .await
    }

    /// Writes one frame. A peer that stops reading cannot hold the
    /// connection: the write gives up after the write deadline, or when
    /// `stop` resolves. A frame cut partway cannot be continued, so the
    /// stream is then reset.
    pub(super) async fn write_parts(
        &mut self,
        parts: &[&[u8]],
        stop: impl Future<Output = ()>,
    ) -> Result<(), NodeError> {
        if self.broken {
            return Err(NodeError::ConnectionLost);
        }
        let until = self.cap(Instant::now() + self.write_deadline);
        let send = &mut self.send;
        let written = async {
            for part in parts {
                send.write_all(part).await.map_err(|_| ())?;
            }
            Ok::<(), ()>(())
        };
        let error = tokio::select! {
            biased;
            written = timeout_at(until, written) => match written {
                Ok(Ok(())) => {
                    self.active = Instant::now();
                    return Ok(());
                }
                Ok(Err(())) => NodeError::ConnectionLost,
                Err(_) => NodeError::Timeout,
            },
            () = stop => NodeError::Stopped,
        };
        self.broken = true;
        let _ = self.send.reset(VarInt::from_u32(CLOSE_STOPPED));
        Err(error)
    }

    /// The next frame, if it comes within `deadline`.
    pub(super) async fn next(&mut self, deadline: Duration) -> Result<Frame, NodeError> {
        self.next_by(Instant::now() + deadline).await
    }

    /// The next frame, if it comes by `until`. Queued frames come first;
    /// after them, a closed connection ends the wait even if the reader is
    /// still waiting for buffer room.
    pub(super) async fn next_by(&mut self, until: Instant) -> Result<Frame, NodeError> {
        let until = self.cap(until);
        let next = tokio::select! {
            biased;
            frame = timeout_at(until, self.frames.recv()) => frame,
            _ = self.conn.closed() => Ok(self.frames.try_recv().ok()),
        };
        match next {
            Ok(Some(frame)) => {
                self.active = Instant::now();
                frame
            }
            Ok(None) => Err(NodeError::Ended),
            Err(_) => Err(NodeError::Timeout),
        }
    }

    pub(super) fn try_next(&mut self) -> Option<Result<Frame, NodeError>> {
        self.frames.try_recv().ok()
    }

    pub(super) async fn message(&mut self, deadline: Duration) -> Result<Message, NodeError> {
        let frame = self.next(deadline).await?;
        match protocol::decode_payload(&frame.payload)? {
            Message::Error(code) => Err(NodeError::Peer(code)),
            msg => Ok(msg),
        }
    }

    /// Tells the peer why the exchange stopped, when it has a code.
    pub(super) async fn send_error(&mut self, error: &NodeError) {
        let code = match error {
            NodeError::Protocol(e) => e.code(),
            NodeError::Unexpected => ErrorCode::UnexpectedMessage,
            NodeError::Pairing(_) => ErrorCode::PairingFailed,
            NodeError::Session(_) | NodeError::Trust(_) => ErrorCode::NotTrusted,
            NodeError::Refused(_) => ErrorCode::Busy,
            NodeError::Invitation => ErrorCode::InvitationUnavailable,
            NodeError::Timeout => ErrorCode::Timeout,
            _ => return,
        };
        let _ = self.write(&Message::Error(code)).await;
    }

    /// The listener's orderly end: finish sending, wait for the dialer to
    /// finish, and let the dialer close the connection. After a broken
    /// write there is nothing orderly left, so it closes at once.
    pub(super) async fn finish_listener(mut self, conn: &Connection, deadline: Duration) {
        if self.broken {
            conn.close(VarInt::from_u32(CLOSE_STOPPED), b"stopped");
        } else {
            let _ = self.send.finish();
            // Keep taking frames so the dialer can finish writing, until it
            // closes. A reader stuck waiting for buffer room does not delay
            // that.
            let until = self.cap(Instant::now() + deadline);
            let frames = &mut self.frames;
            let _ = timeout_at(until, async {
                tokio::select! {
                    () = async { while frames.recv().await.is_some() {} std::future::pending().await } => {}
                    _ = conn.closed() => {}
                }
            })
            .await;
        }
        self.reader.abort();
        tasks::observe((&mut self.reader).await);
    }

    /// The dialer's orderly end: finish sending, read until the listener
    /// finishes or closes, then close.
    pub(super) async fn finish_dialer(&mut self, conn: &Connection, deadline: Duration) {
        if self.broken {
            conn.close(VarInt::from_u32(CLOSE_STOPPED), b"stopped");
        } else {
            let _ = self.send.finish();
            // As in finish_listener, a reader stuck waiting for buffer room
            // does not delay a close the listener already made.
            let until = self.cap(Instant::now() + deadline);
            let frames = &mut self.frames;
            let _ = timeout_at(until, async {
                tokio::select! {
                    () = async { while frames.recv().await.is_some() {} } => {}
                    _ = conn.closed() => {}
                }
            })
            .await;
            conn.close(VarInt::from_u32(CLOSE_DONE), b"done");
        }
        self.reader.abort();
        tasks::observe((&mut self.reader).await);
    }
}

impl Drop for Wire {
    /// The reader task belongs to this stream; it never outlives it.
    fn drop(&mut self) {
        self.reader.abort();
    }
}

pub(super) async fn read_frames(
    mut recv: RecvStream,
    budget: Budget,
    gate: Option<Arc<Semaphore>>,
    tx: mpsc::Sender<Result<Frame, NodeError>>,
) {
    loop {
        if let Some(gate) = &gate
            && let Ok(permit) = gate.acquire().await
        {
            permit.forget();
        }
        let frame = read_frame(&mut recv, &budget).await;
        let stop = frame.is_err();
        if let Err(NodeError::Ended) = frame {
            return;
        }
        if tx.send(frame).await.is_err() || stop {
            return;
        }
    }
}

pub(super) async fn read_frame(recv: &mut RecvStream, budget: &Budget) -> Result<Frame, NodeError> {
    let mut header = [0u8; protocol::HEADER_BYTES];
    match recv.read_exact(&mut header).await {
        Ok(()) => {}
        Err(ReadExactError::FinishedEarly(0)) => return Err(NodeError::Ended),
        Err(ReadExactError::FinishedEarly(_)) => return Err(ProtocolError::Truncated.into()),
        Err(ReadExactError::ReadError(_)) => return Err(NodeError::ConnectionLost),
    }
    let len = protocol::payload_length(header)?;
    // The decoded message copies the payload, so reserve room for both.
    // Waiting here leaves the rest of the frame in QUIC's flow-control
    // window, which pushes back on the peer.
    let lease = budget.lease_wait((len as u64).saturating_mul(2)).await?;
    let mut payload = vec![0u8; len];
    match recv.read_exact(&mut payload).await {
        Ok(()) => Ok(Frame {
            payload,
            _lease: lease,
        }),
        Err(ReadExactError::FinishedEarly(_)) => Err(ProtocolError::Truncated.into()),
        Err(ReadExactError::ReadError(_)) => Err(NodeError::ConnectionLost),
    }
}
