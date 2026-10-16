//! The local protocol between the command line and the service: a length,
//! a kind byte, then the body, with the length checked before allocating.
//! Credentials travel only in secret frames, never inside JSON, so no message
//! can carry one into a log. Only the connection that received a prompt can
//! answer it, by repeating its context.

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use zeroize::Zeroizing;

use crate::secrets::MAX_CREDENTIAL_BYTES;
use crate::storage::StoreKind;

/// The protocol version this build speaks.
pub const VERSION: u16 = 1;
/// Largest frame body.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;
/// Clients connected at once.
pub const MAX_CLIENTS: usize = 8;
/// Events queued for one client. A client that lets it fill is
/// disconnected and must reconnect for a fresh view.
pub const EVENT_QUEUE: usize = 64;
/// Bytes in one path.
pub const MAX_PATH_BYTES: usize = 4096;

const KIND_MESSAGE: u8 = 1;
const KIND_DATA: u8 = 2;
const KIND_SECRET: u8 = 3;

#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("the connection closed")]
    Closed,
    #[error("a frame is larger than allowed")]
    TooLarge,
    #[error("a frame is malformed")]
    Malformed,
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

/// One frame as read.
pub enum Frame {
    Message(Vec<u8>),
    Data(Vec<u8>),
    Secret(Zeroizing<Vec<u8>>),
}

/// Reads frames, checking each length before reading its body. A frame
/// read in part survives cancellation: every await is one `read`, which
/// takes no bytes when it is cancelled, and the bytes read so far stay
/// here. Callers can race [`FrameReader::next`] against other work.
pub struct FrameReader<R> {
    r: R,
    header: [u8; 5],
    /// Header bytes read so far.
    have: usize,
    /// The body being read: its kind, buffer, and bytes read so far.
    body: Option<(u8, Zeroizing<Vec<u8>>, usize)>,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    pub fn new(r: R) -> Self {
        Self {
            r,
            header: [0; 5],
            have: 0,
            body: None,
        }
    }

    pub async fn next(&mut self) -> Result<Frame, IpcError> {
        loop {
            if let Some((_, body, filled)) = self.body.as_mut()
                && *filled < body.len()
            {
                let n = self.r.read(&mut body[*filled..]).await?;
                if n == 0 {
                    return Err(IpcError::Closed);
                }
                *filled += n;
                continue;
            }
            if let Some((kind, mut body, _)) = self.body.take() {
                return Ok(match kind {
                    KIND_SECRET => Frame::Secret(body),
                    KIND_DATA => Frame::Data(std::mem::take(&mut *body)),
                    _ => Frame::Message(std::mem::take(&mut *body)),
                });
            }
            if self.have < self.header.len() {
                let n = self.r.read(&mut self.header[self.have..]).await?;
                if n == 0 {
                    return Err(IpcError::Closed);
                }
                self.have += n;
                continue;
            }
            self.have = 0;
            let h = self.header;
            let len = u32::from_be_bytes([h[0], h[1], h[2], h[3]]) as usize;
            let limit = match h[4] {
                KIND_MESSAGE | KIND_DATA => MAX_FRAME_BYTES,
                KIND_SECRET => MAX_CREDENTIAL_BYTES,
                _ => return Err(IpcError::Malformed),
            };
            if len > limit {
                return Err(IpcError::TooLarge);
            }
            self.body = Some((h[4], Zeroizing::new(vec![0u8; len]), 0));
        }
    }
}

async fn write_raw(
    w: &mut (impl AsyncWriteExt + Unpin),
    kind: u8,
    body: &[u8],
) -> Result<(), IpcError> {
    if body.len() > MAX_FRAME_BYTES {
        return Err(IpcError::TooLarge);
    }
    let mut header = [0u8; 5];
    header[..4].copy_from_slice(&(body.len() as u32).to_be_bytes());
    header[4] = kind;
    w.write_all(&header).await?;
    w.write_all(body).await?;
    w.flush().await?;
    Ok(())
}

pub async fn write_message<T: Serialize>(
    w: &mut (impl AsyncWriteExt + Unpin),
    msg: &T,
) -> Result<(), IpcError> {
    let body = serde_json::to_vec(msg).map_err(|_| IpcError::Malformed)?;
    write_raw(w, KIND_MESSAGE, &body).await
}

pub async fn write_data(w: &mut (impl AsyncWriteExt + Unpin), data: &[u8]) -> Result<(), IpcError> {
    write_raw(w, KIND_DATA, data).await
}

pub async fn write_secret(
    w: &mut (impl AsyncWriteExt + Unpin),
    secret: &[u8],
) -> Result<(), IpcError> {
    if secret.len() > MAX_CREDENTIAL_BYTES {
        return Err(IpcError::TooLarge);
    }
    write_raw(w, KIND_SECRET, secret).await
}

pub fn decode<T: for<'de> Deserialize<'de>>(body: &[u8]) -> Result<T, IpcError> {
    serde_json::from_slice(body).map_err(|_| IpcError::Malformed)
}

// ---- Messages ----

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ClientMessage {
    Hello {
        version: u16,
    },
    Request {
        id: u64,
        request: Request,
    },
    /// The person's answer to a prompt this connection received.
    Answer {
        prompt: u64,
        context: String,
        accept: bool,
    },
    /// The data frames of the running stream request are complete.
    StreamEnd,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Request {
    Status {},
    /// Which stores `init` can use now.
    Stores {},
    /// Followed by a secret frame with the passphrase for a key file.
    Init {
        store: StoreKind,
        name: String,
    },
    /// Followed by a secret frame with the passphrase for a key file.
    Unlock {},
    Lock {},
    Invite {
        alias: String,
    },
    /// Followed by a secret frame with the invitation.
    Join {
        alias: String,
    },
    Devices {},
    Remove {
        device: String,
    },
    /// Receive into `dir`, or into Hyakume's own folder when `None`.
    Receive {
        dir: Option<Vec<u8>>,
    },
    SendFiles {
        device: String,
        route: String,
        paths: Vec<Vec<u8>>,
    },
    /// Followed by data frames and a stream end.
    SendStream {
        device: String,
        route: String,
        kind: StreamKind,
        name: Option<String>,
    },
    Cancel {
        transfer: String,
    },
    Inspect {
        transfer: String,
    },
    Transfers {},
    /// Retries deleting files kept after their retry window.
    Cleanup {},
    /// Forgets this device's identity, trust, and transfers.
    Reset {},
    Stop {},
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StreamKind {
    Text,
    Url,
    Stdin,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ServerMessage {
    Hello {
        version: u16,
    },
    Event {
        id: u64,
        event: Event,
    },
    Reply {
        id: u64,
        reply: Reply,
    },
    /// An answer named no prompt this connection holds, or the wrong
    /// context. Nothing was approved.
    AnswerRefused {
        prompt: u64,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "kebab-case")]
pub enum Reply {
    Ok { body: Body },
    Failed { failure: Failure },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Failure {
    pub code: ErrorCode,
    pub message: String,
}

impl Failure {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// Stable error codes for scripts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ErrorCode {
    Unsupported,
    Malformed,
    TooLarge,
    Busy,
    NotInitialized,
    AlreadyInitialized,
    Locked,
    StoreUnavailable,
    WrongPassphrase,
    WeakPassphrase,
    NotFound,
    NotTrusted,
    Invalid,
    Destination,
    Refused,
    Storage,
    Failed,
    Stopping,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "body", rename_all = "kebab-case")]
pub enum Body {
    Done,
    Status(StatusView),
    Stores(StoresView),
    Devices { devices: Vec<DeviceView> },
    Device(DeviceView),
    Transfer(TransferView),
    Transfers { transfers: Vec<TransferSummary> },
    Cleanup { retried: usize },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum Event {
    /// The invitation to give to the other device. Shown once.
    Invitation { token: String },
    /// Compare these words with the other device's screen.
    Verify {
        prompt: u64,
        context: String,
        fingerprint: String,
        name: String,
        words: String,
    },
    /// The consent screen for an incoming offer.
    Offer {
        prompt: u64,
        context: String,
        transfer: String,
        screen: String,
    },
    /// Receiving into this folder.
    Receiving { dir: String },
    /// The send is recorded under this transfer ID.
    Started { transfer: String },
    /// An incoming transfer ended.
    Received(TransferView),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StatusView {
    /// `uninitialized`, `locked`, or `unlocked`.
    pub state: String,
    pub store: Option<StoreKind>,
    pub fingerprint: Option<String>,
    pub name: Option<String>,
    /// Where other devices can reach this one while unlocked.
    pub route: Option<String>,
    pub trusted: usize,
    pub unfinished: usize,
    pub paused: usize,
    pub receiving: Option<String>,
    /// Files Hyakume could not delete and still keeps.
    pub retained_files: usize,
    pub retained_bytes: u64,
    /// Bytes charged to spools and partial files, deletions pending
    /// included.
    pub spool_bytes: u64,
    pub partial_bytes: u64,
    /// A write to the state database failed; restart the service.
    pub store_failed: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StoresView {
    pub secret_service: String,
    pub key_file: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DeviceView {
    pub alias: String,
    pub fingerprint: String,
    /// The name the device declared, as a safe projection.
    pub name: String,
    pub trusted: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TransferView {
    pub transfer: String,
    pub outgoing: bool,
    /// The other device's alias, or its fingerprint if it is no longer
    /// trusted.
    pub peer: String,
    pub kind: String,
    pub offer: String,
    /// `offered`, `in-progress`, `paused`, `unconfirmed` (a file was
    /// renamed into place but its folder did not sync yet), `completed`,
    /// `partially-completed`, or `unsuccessful`.
    pub outcome: String,
    pub items: Vec<ItemView>,
    /// Receiving: the folder items go to.
    pub place: Option<String>,
    /// Bytes still kept on disk for this transfer: partial files or a spool.
    pub retained_bytes: u64,
    pub expires_ms: u64,
}

/// One line of the transfer list. Item details come from `inspect`, so a
/// list of every kept transfer always fits in one frame.
#[derive(Debug, Serialize, Deserialize)]
pub struct TransferSummary {
    pub transfer: String,
    pub outgoing: bool,
    pub peer: String,
    pub kind: String,
    pub outcome: String,
    pub items: usize,
    pub retained_bytes: u64,
    pub expires_ms: u64,
}

impl From<&TransferView> for TransferSummary {
    fn from(t: &TransferView) -> Self {
        Self {
            transfer: t.transfer.clone(),
            outgoing: t.outgoing,
            peer: t.peer.clone(),
            kind: t.kind.clone(),
            outcome: t.outcome.clone(),
            items: t.items.len(),
            retained_bytes: t.retained_bytes,
            expires_ms: t.expires_ms,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ItemView {
    /// The name, as a safe projection.
    pub name: String,
    pub size: u64,
    pub state: String,
    /// Receiving: the file name the item gets in the folder.
    pub output: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_frame_longer_than_its_kind_allows_is_refused_before_reading_it() {
        let mut big = Vec::new();
        big.extend_from_slice(&((MAX_FRAME_BYTES + 1) as u32).to_be_bytes());
        big.push(KIND_MESSAGE);
        assert!(matches!(
            FrameReader::new(big.as_slice()).next().await,
            Err(IpcError::TooLarge)
        ));
        let mut secret = Vec::new();
        secret.extend_from_slice(&((MAX_CREDENTIAL_BYTES + 1) as u32).to_be_bytes());
        secret.push(KIND_SECRET);
        assert!(matches!(
            FrameReader::new(secret.as_slice()).next().await,
            Err(IpcError::TooLarge)
        ));
        let unknown = [0, 0, 0, 0, 9];
        assert!(matches!(
            FrameReader::new(unknown.as_slice()).next().await,
            Err(IpcError::Malformed)
        ));
    }

    #[tokio::test]
    async fn a_frame_read_in_part_survives_a_cancelled_read() {
        let (mut client, server) = tokio::io::duplex(64);
        let mut frames = FrameReader::new(server);
        let mut frame = 5u32.to_be_bytes().to_vec();
        frame.push(KIND_DATA);
        frame.extend_from_slice(b"hello");
        // Part of the header, then part of the body. A zero timeout polls
        // the read once, which takes the bytes, then cancels it.
        for part in [&frame[..3], &frame[3..7]] {
            client.write_all(part).await.unwrap();
            let waited = tokio::time::timeout(std::time::Duration::ZERO, frames.next()).await;
            assert!(waited.is_err(), "a partial frame came back whole");
        }
        client.write_all(&frame[7..]).await.unwrap();
        let rest = tokio::time::timeout(std::time::Duration::from_secs(5), frames.next());
        match rest.await {
            Ok(Ok(Frame::Data(data))) => assert_eq!(data, b"hello"),
            _ => panic!("the frame was lost"),
        }
    }

    #[test]
    fn every_reply_and_event_reads_back_as_it_was_written() {
        let view = || TransferView {
            transfer: "00".into(),
            outgoing: true,
            peer: "phone".into(),
            kind: "files".into(),
            offer: "accepted".into(),
            outcome: "completed".into(),
            items: vec![ItemView {
                name: "\"a\"".into(),
                size: 1,
                state: "published".into(),
                output: None,
            }],
            place: None,
            retained_bytes: 0,
            expires_ms: 0,
        };
        let messages = [
            ServerMessage::Reply {
                id: 1,
                reply: Reply::Ok {
                    body: Body::Transfer(view()),
                },
            },
            ServerMessage::Event {
                id: 1,
                event: Event::Received(view()),
            },
            ServerMessage::Reply {
                id: 2,
                reply: Reply::Ok {
                    body: Body::Transfers {
                        transfers: vec![TransferSummary::from(&view())],
                    },
                },
            },
        ];
        for message in messages {
            let bytes = serde_json::to_vec(&message).unwrap();
            let decoded = decode::<ServerMessage>(&bytes).unwrap();
            assert_eq!(
                serde_json::to_value(&decoded).unwrap(),
                serde_json::to_value(&message).unwrap()
            );
        }
    }

    #[test]
    fn a_list_of_every_kept_transfer_fits_in_one_frame() {
        // The longest alias, made of characters JSON escapes.
        let summary = || TransferSummary {
            transfer: "f".repeat(32),
            outgoing: true,
            peer: format!("{} (removed)", "\"".repeat(64)),
            kind: "files".into(),
            outcome: "partially-completed".into(),
            items: 256,
            retained_bytes: u64::MAX,
            expires_ms: u64::MAX,
        };
        let reply = ServerMessage::Reply {
            id: u64::MAX,
            reply: Reply::Ok {
                body: Body::Transfers {
                    transfers: (0..crate::state::MAX_KEPT_TRANSFERS)
                        .map(|_| summary())
                        .collect(),
                },
            },
        };
        assert!(serde_json::to_vec(&reply).unwrap().len() <= MAX_FRAME_BYTES);
    }

    #[test]
    fn unknown_fields_and_operations_are_refused() {
        assert!(decode::<ClientMessage>(br#"{"type":"hello","version":1}"#).is_ok());
        assert!(decode::<ClientMessage>(br#"{"type":"hello","version":1,"extra":0}"#).is_err());
        assert!(
            decode::<ClientMessage>(br#"{"type":"request","id":1,"request":{"op":"format-disk"}}"#)
                .is_err()
        );
        // A passphrase has no place in any message.
        assert!(
            decode::<ClientMessage>(
                br#"{"type":"request","id":1,"request":{"op":"unlock","passphrase":"x"}}"#
            )
            .is_err()
        );
    }
}
