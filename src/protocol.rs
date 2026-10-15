//! Versioned control messages and their framing. Decoding is strict: every
//! length is checked before anything is allocated, and leftover bytes or
//! unknown tags are refused, so the reader never has to guess. It checks
//! only shape; domain types validate meaning, and the state machines decide
//! which message may come next.

use crate::ids::{ClaimId, InvitationId, TransferId};
use crate::invitation::InvitationKind;
use crate::limits::{
    MAX_BATCH_ITEMS, MAX_CHUNK_BYTES, MAX_FRAME_BYTES, MAX_OFFER_METADATA_BYTES,
    MAX_PEER_NAME_BYTES, MAX_PREVIEW_BYTES,
};
use crate::offer::OfferFields;
use crate::pairing::{Commit, Confirm, Contribution, Decision, IdentityProof, SHARE_BYTES};
use crate::session::SessionProof;

/// The protocol version this build speaks.
pub const VERSION: u16 = 1;
/// Versions this build accepts.
pub const SUPPORTED: VersionRange = VersionRange { min: 1, max: 1 };
/// Bytes in the frame length prefix.
pub const HEADER_BYTES: usize = 4;

const CTX_NEGOTIATION: &str = "hyakume v1 version negotiation";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VersionRange {
    pub min: u16,
    pub max: u16,
}

/// What a connection is for. A version message for one purpose cannot be
/// used to start the other.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Purpose {
    Pair,
    Session,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hello {
    pub purpose: Purpose,
    pub versions: VersionRange,
    /// Optional features. None are defined in version 1; unknown bits are
    /// ignored and the result is the intersection.
    pub capabilities: u32,
}

impl Hello {
    pub fn new(purpose: Purpose) -> Self {
        Self {
            purpose,
            versions: SUPPORTED,
            capabilities: 0,
        }
    }
}

/// Chooses the highest version both sides support.
pub fn negotiate(local: VersionRange, peer: VersionRange) -> Result<u16, ProtocolError> {
    if peer.min > peer.max {
        return Err(ProtocolError::Malformed);
    }
    let high = local.max.min(peer.max);
    let low = local.min.max(peer.min);
    if high < low {
        return Err(ProtocolError::UnsupportedVersion);
    }
    Ok(high)
}

/// Hashes both version messages exactly as sent, so the negotiation can be
/// bound into the authenticated transcripts.
pub fn negotiation_digest(dialer_hello: &[u8], listener_hello: &[u8]) -> [u8; 32] {
    let mut h = blake3::Hasher::new_derive_key(CTX_NEGOTIATION);
    h.update(&(dialer_hello.len() as u64).to_be_bytes());
    h.update(dialer_hello);
    h.update(&(listener_hello.len() as u64).to_be_bytes());
    h.update(listener_hello);
    *h.finalize().as_bytes()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RejectReason {
    Declined,
    TooLarge,
    Busy,
    NotTrusted,
    Malformed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemResult {
    Published,
    DigestMismatch,
    LengthMismatch,
    Cancelled,
    Unauthorized,
    /// The bytes match the digest but not the offered kind: text that is
    /// not UTF-8, or that differs from its preview.
    ContentMismatch,
    /// The receiver already has a file with that name; nothing was
    /// overwritten.
    Exists,
    /// The receiver could not write the item to its storage.
    StorageFailed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelReason {
    Sender,
    Receiver,
    Revoked,
    Shutdown,
}

/// Stable error codes sent before closing a connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorCode {
    UnsupportedVersion,
    UnexpectedMessage,
    Malformed,
    TooLarge,
    NotTrusted,
    Busy,
    InvitationUnavailable,
    PairingFailed,
    Timeout,
    Internal,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    Hello(Hello),
    Claim {
        invitation: InvitationId,
        claim: ClaimId,
        kind: InvitationKind,
    },
    PairCommit(Commit),
    PairShare(Contribution),
    PairReveal(Contribution),
    PairConfirm(Confirm),
    PairIdentity(IdentityProof),
    PairDecision(Decision),
    SessionProof(SessionProof),
    Offer(OfferFields),
    OfferAccept {
        transfer: TransferId,
        offer_digest: [u8; 32],
    },
    OfferReject {
        transfer: TransferId,
        reason: RejectReason,
    },
    Chunk {
        transfer: TransferId,
        item: u16,
        offset: u64,
        data: Vec<u8>,
    },
    ItemEnd {
        transfer: TransferId,
        item: u16,
    },
    ItemResult {
        transfer: TransferId,
        item: u16,
        result: ItemResult,
    },
    Cancel {
        transfer: TransferId,
        reason: CancelReason,
    },
    Error(ErrorCode),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ProtocolError {
    #[error("the message ended early")]
    Truncated,
    #[error("the message has unexpected trailing bytes")]
    Trailing,
    #[error("the message is larger than allowed")]
    TooLarge,
    #[error("unknown message type {0}")]
    UnknownMessage(u8),
    #[error("the message has an invalid field")]
    Malformed,
    #[error("no protocol version in common")]
    UnsupportedVersion,
}

impl ProtocolError {
    /// The code to send to the peer before closing.
    pub fn code(self) -> ErrorCode {
        match self {
            Self::TooLarge => ErrorCode::TooLarge,
            Self::UnsupportedVersion => ErrorCode::UnsupportedVersion,
            Self::Truncated | Self::Trailing | Self::UnknownMessage(_) | Self::Malformed => {
                ErrorCode::Malformed
            }
        }
    }
}

mod tag {
    pub const HELLO: u8 = 1;
    pub const CLAIM: u8 = 2;
    pub const PAIR_COMMIT: u8 = 3;
    pub const PAIR_SHARE: u8 = 4;
    pub const PAIR_REVEAL: u8 = 5;
    pub const PAIR_CONFIRM: u8 = 6;
    pub const PAIR_IDENTITY: u8 = 7;
    pub const PAIR_DECISION: u8 = 8;
    pub const SESSION_PROOF: u8 = 9;
    pub const OFFER: u8 = 10;
    pub const OFFER_ACCEPT: u8 = 11;
    pub const OFFER_REJECT: u8 = 12;
    pub const CHUNK: u8 = 13;
    pub const ITEM_END: u8 = 14;
    pub const ITEM_RESULT: u8 = 15;
    pub const CANCEL: u8 = 16;
    pub const ERROR: u8 = 17;
}

/// Reads the payload length from a frame header and checks it against the
/// frame limit, before the payload is read or allocated.
pub fn payload_length(header: [u8; HEADER_BYTES]) -> Result<usize, ProtocolError> {
    let len = u32::from_be_bytes(header);
    let len = usize::try_from(len).map_err(|_| ProtocolError::TooLarge)?;
    if len > MAX_FRAME_BYTES - HEADER_BYTES {
        return Err(ProtocolError::TooLarge);
    }
    Ok(len)
}

/// Decodes one frame from the start of `buf`. Returns `None` when more bytes
/// are needed, or the message and the bytes it used.
pub fn decode_frame(buf: &[u8]) -> Result<Option<(Message, usize)>, ProtocolError> {
    let Some((header, rest)) = buf.split_first_chunk::<HEADER_BYTES>() else {
        return Ok(None);
    };
    let len = payload_length(*header)?;
    let Some(payload) = rest.get(..len) else {
        return Ok(None);
    };
    Ok(Some((decode_payload(payload)?, HEADER_BYTES + len)))
}

/// Encodes a message as a complete frame.
pub fn encode_frame(msg: &Message) -> Result<Vec<u8>, ProtocolError> {
    let payload = encode_payload(msg);
    if payload.len() > MAX_FRAME_BYTES - HEADER_BYTES {
        return Err(ProtocolError::TooLarge);
    }
    if matches!(msg, Message::Offer(_)) && payload.len() > MAX_OFFER_METADATA_BYTES {
        return Err(ProtocolError::TooLarge);
    }
    let mut frame = Vec::with_capacity(HEADER_BYTES + payload.len());
    // The length fits: it is at most MAX_FRAME_BYTES.
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

/// The length of `offer` encoded as a payload, which
/// [`MAX_OFFER_METADATA_BYTES`] bounds. Offer validation checks it, so an
/// offer that can be recorded can also be sent and loaded again.
pub fn offer_payload_len(offer: &OfferFields) -> usize {
    let mut w = Writer::default();
    write_offer(&mut w, offer);
    w.0.len()
}

fn write_offer(w: &mut Writer, o: &OfferFields) {
    w.u8(tag::OFFER);
    w.bytes(o.transfer.as_bytes());
    w.u8(o.kind);
    // Callers build offers from validated values, so the count fits.
    w.u16(o.items.len() as u16);
    for (name, size, digest) in &o.items {
        w.var(name);
        w.u64(*size);
        w.bytes(digest);
    }
    w.var(&o.preview);
}

pub fn encode_payload(msg: &Message) -> Vec<u8> {
    let mut w = Writer::default();
    match msg {
        Message::Hello(h) => {
            w.u8(tag::HELLO);
            w.u8(match h.purpose {
                Purpose::Pair => 1,
                Purpose::Session => 2,
            });
            w.u16(h.versions.min);
            w.u16(h.versions.max);
            w.u32(h.capabilities);
        }
        Message::Claim {
            invitation,
            claim,
            kind,
        } => {
            w.u8(tag::CLAIM);
            w.bytes(invitation.as_bytes());
            w.bytes(claim.as_bytes());
            w.u8(match kind {
                InvitationKind::Permanent => 1,
                InvitationKind::Temporary => 2,
            });
        }
        Message::PairCommit(c) => {
            w.u8(tag::PAIR_COMMIT);
            w.bytes(&c.0);
        }
        Message::PairShare(c) | Message::PairReveal(c) => {
            w.u8(if matches!(msg, Message::PairShare(_)) {
                tag::PAIR_SHARE
            } else {
                tag::PAIR_REVEAL
            });
            w.bytes(&c.share);
            w.bytes(&c.id_commit);
            w.bytes(&c.nonce);
            w.var(c.name.as_bytes());
        }
        Message::PairConfirm(c) => {
            w.u8(tag::PAIR_CONFIRM);
            w.bytes(&c.mac);
        }
        Message::PairIdentity(p) => {
            w.u8(tag::PAIR_IDENTITY);
            w.bytes(&p.id);
            w.bytes(&p.salt);
            w.bytes(&p.sig);
        }
        Message::PairDecision(d) => {
            w.u8(tag::PAIR_DECISION);
            w.u8(u8::from(d.accept));
            w.bytes(&d.mac);
        }
        Message::SessionProof(p) => {
            w.u8(tag::SESSION_PROOF);
            w.bytes(&p.device);
            w.bytes(&p.sig);
        }
        Message::Offer(o) => write_offer(&mut w, o),
        Message::OfferAccept {
            transfer,
            offer_digest,
        } => {
            w.u8(tag::OFFER_ACCEPT);
            w.bytes(transfer.as_bytes());
            w.bytes(offer_digest);
        }
        Message::OfferReject { transfer, reason } => {
            w.u8(tag::OFFER_REJECT);
            w.bytes(transfer.as_bytes());
            w.u8(match reason {
                RejectReason::Declined => 1,
                RejectReason::TooLarge => 2,
                RejectReason::Busy => 3,
                RejectReason::NotTrusted => 4,
                RejectReason::Malformed => 5,
            });
        }
        Message::Chunk {
            transfer,
            item,
            offset,
            data,
        } => {
            w.u8(tag::CHUNK);
            w.bytes(transfer.as_bytes());
            w.u16(*item);
            w.u64(*offset);
            w.var(data);
        }
        Message::ItemEnd { transfer, item } => {
            w.u8(tag::ITEM_END);
            w.bytes(transfer.as_bytes());
            w.u16(*item);
        }
        Message::ItemResult {
            transfer,
            item,
            result,
        } => {
            w.u8(tag::ITEM_RESULT);
            w.bytes(transfer.as_bytes());
            w.u16(*item);
            w.u8(match result {
                ItemResult::Published => 1,
                ItemResult::DigestMismatch => 2,
                ItemResult::LengthMismatch => 3,
                ItemResult::Cancelled => 4,
                ItemResult::Unauthorized => 5,
                ItemResult::ContentMismatch => 6,
                ItemResult::Exists => 7,
                ItemResult::StorageFailed => 8,
            });
        }
        Message::Cancel { transfer, reason } => {
            w.u8(tag::CANCEL);
            w.bytes(transfer.as_bytes());
            w.u8(match reason {
                CancelReason::Sender => 1,
                CancelReason::Receiver => 2,
                CancelReason::Revoked => 3,
                CancelReason::Shutdown => 4,
            });
        }
        Message::Error(code) => {
            w.u8(tag::ERROR);
            w.u16(match code {
                ErrorCode::UnsupportedVersion => 1,
                ErrorCode::UnexpectedMessage => 2,
                ErrorCode::Malformed => 3,
                ErrorCode::TooLarge => 4,
                ErrorCode::NotTrusted => 5,
                ErrorCode::Busy => 6,
                ErrorCode::InvitationUnavailable => 7,
                ErrorCode::PairingFailed => 8,
                ErrorCode::Timeout => 9,
                ErrorCode::Internal => 10,
            });
        }
    }
    w.0
}

pub fn decode_payload(payload: &[u8]) -> Result<Message, ProtocolError> {
    let mut r = Reader(payload);
    let msg = match r.u8()? {
        tag::HELLO => {
            let purpose = match r.u8()? {
                1 => Purpose::Pair,
                2 => Purpose::Session,
                _ => return Err(ProtocolError::Malformed),
            };
            let min = r.u16()?;
            let max = r.u16()?;
            Message::Hello(Hello {
                purpose,
                versions: VersionRange { min, max },
                capabilities: r.u32()?,
            })
        }
        tag::CLAIM => Message::Claim {
            invitation: InvitationId::from_bytes(r.array()?),
            claim: ClaimId::from_bytes(r.array()?),
            kind: match r.u8()? {
                1 => InvitationKind::Permanent,
                2 => InvitationKind::Temporary,
                _ => return Err(ProtocolError::Malformed),
            },
        },
        tag::PAIR_COMMIT => Message::PairCommit(Commit(r.array()?)),
        t @ (tag::PAIR_SHARE | tag::PAIR_REVEAL) => {
            let c = Contribution {
                share: r.array::<SHARE_BYTES>()?,
                id_commit: r.array()?,
                nonce: r.array()?,
                name: crate::display::PeerText::new(
                    r.var(MAX_PEER_NAME_BYTES)?,
                    MAX_PEER_NAME_BYTES,
                )
                .map_err(|_| ProtocolError::TooLarge)?,
            };
            if t == tag::PAIR_SHARE {
                Message::PairShare(c)
            } else {
                Message::PairReveal(c)
            }
        }
        tag::PAIR_CONFIRM => Message::PairConfirm(Confirm { mac: r.array()? }),
        tag::PAIR_IDENTITY => Message::PairIdentity(IdentityProof {
            id: r.array()?,
            salt: r.array()?,
            sig: r.array()?,
        }),
        tag::PAIR_DECISION => Message::PairDecision(Decision {
            accept: match r.u8()? {
                0 => false,
                1 => true,
                _ => return Err(ProtocolError::Malformed),
            },
            mac: r.array()?,
        }),
        tag::SESSION_PROOF => Message::SessionProof(SessionProof {
            device: r.array()?,
            sig: r.array()?,
        }),
        tag::OFFER => {
            if payload.len() > MAX_OFFER_METADATA_BYTES {
                return Err(ProtocolError::TooLarge);
            }
            let transfer = TransferId::from_bytes(r.array()?);
            let kind = r.u8()?;
            let count = usize::from(r.u16()?);
            if count > MAX_BATCH_ITEMS {
                return Err(ProtocolError::TooLarge);
            }
            let mut items = Vec::with_capacity(count);
            for _ in 0..count {
                let name = r.var(MAX_PEER_NAME_BYTES)?;
                let size = r.u64()?;
                let digest = r.array()?;
                items.push((name, size, digest));
            }
            let preview = r.var(MAX_PREVIEW_BYTES)?;
            Message::Offer(OfferFields {
                transfer,
                kind,
                items,
                preview,
            })
        }
        tag::OFFER_ACCEPT => Message::OfferAccept {
            transfer: TransferId::from_bytes(r.array()?),
            offer_digest: r.array()?,
        },
        tag::OFFER_REJECT => Message::OfferReject {
            transfer: TransferId::from_bytes(r.array()?),
            reason: match r.u8()? {
                1 => RejectReason::Declined,
                2 => RejectReason::TooLarge,
                3 => RejectReason::Busy,
                4 => RejectReason::NotTrusted,
                5 => RejectReason::Malformed,
                _ => return Err(ProtocolError::Malformed),
            },
        },
        tag::CHUNK => Message::Chunk {
            transfer: TransferId::from_bytes(r.array()?),
            item: r.u16()?,
            offset: r.u64()?,
            data: r.var(MAX_CHUNK_BYTES)?,
        },
        tag::ITEM_END => Message::ItemEnd {
            transfer: TransferId::from_bytes(r.array()?),
            item: r.u16()?,
        },
        tag::ITEM_RESULT => Message::ItemResult {
            transfer: TransferId::from_bytes(r.array()?),
            item: r.u16()?,
            result: match r.u8()? {
                1 => ItemResult::Published,
                2 => ItemResult::DigestMismatch,
                3 => ItemResult::LengthMismatch,
                4 => ItemResult::Cancelled,
                5 => ItemResult::Unauthorized,
                6 => ItemResult::ContentMismatch,
                7 => ItemResult::Exists,
                8 => ItemResult::StorageFailed,
                _ => return Err(ProtocolError::Malformed),
            },
        },
        tag::CANCEL => Message::Cancel {
            transfer: TransferId::from_bytes(r.array()?),
            reason: match r.u8()? {
                1 => CancelReason::Sender,
                2 => CancelReason::Receiver,
                3 => CancelReason::Revoked,
                4 => CancelReason::Shutdown,
                _ => return Err(ProtocolError::Malformed),
            },
        },
        tag::ERROR => Message::Error(match r.u16()? {
            1 => ErrorCode::UnsupportedVersion,
            2 => ErrorCode::UnexpectedMessage,
            3 => ErrorCode::Malformed,
            4 => ErrorCode::TooLarge,
            5 => ErrorCode::NotTrusted,
            6 => ErrorCode::Busy,
            7 => ErrorCode::InvitationUnavailable,
            8 => ErrorCode::PairingFailed,
            9 => ErrorCode::Timeout,
            10 => ErrorCode::Internal,
            _ => return Err(ProtocolError::Malformed),
        }),
        other => return Err(ProtocolError::UnknownMessage(other)),
    };
    if !r.0.is_empty() {
        return Err(ProtocolError::Trailing);
    }
    Ok(msg)
}

#[derive(Default)]
struct Writer(Vec<u8>);

impl Writer {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn bytes(&mut self, v: &[u8]) {
        self.0.extend_from_slice(v);
    }
    fn var(&mut self, v: &[u8]) {
        // Every variable field is bounded far below 4 GiB by its limit.
        self.u32(v.len() as u32);
        self.bytes(v);
    }
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn array<const N: usize>(&mut self) -> Result<[u8; N], ProtocolError> {
        let (head, rest) = self
            .0
            .split_first_chunk::<N>()
            .ok_or(ProtocolError::Truncated)?;
        self.0 = rest;
        Ok(*head)
    }
    fn u8(&mut self) -> Result<u8, ProtocolError> {
        Ok(self.array::<1>()?[0])
    }
    fn u16(&mut self) -> Result<u16, ProtocolError> {
        Ok(u16::from_be_bytes(self.array()?))
    }
    fn u32(&mut self) -> Result<u32, ProtocolError> {
        Ok(u32::from_be_bytes(self.array()?))
    }
    fn u64(&mut self) -> Result<u64, ProtocolError> {
        Ok(u64::from_be_bytes(self.array()?))
    }
    /// Reads a length-prefixed field, checking the length against `limit`
    /// and the remaining input before copying anything.
    fn var(&mut self, limit: usize) -> Result<Vec<u8>, ProtocolError> {
        let len = usize::try_from(self.u32()?).map_err(|_| ProtocolError::TooLarge)?;
        if len > limit {
            return Err(ProtocolError::TooLarge);
        }
        if len > self.0.len() {
            return Err(ProtocolError::Truncated);
        }
        let (head, rest) = self.0.split_at(len);
        self.0 = rest;
        Ok(head.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(msg: Message) {
        let frame = encode_frame(&msg).unwrap();
        let (decoded, used) = decode_frame(&frame).unwrap().unwrap();
        assert_eq!(decoded, msg);
        assert_eq!(used, frame.len());
    }

    #[test]
    fn negotiation_picks_the_highest_common_version_or_refuses() {
        let r = |min, max| VersionRange { min, max };
        assert_eq!(negotiate(r(1, 3), r(2, 5)), Ok(3));
        assert_eq!(negotiate(r(1, 1), r(1, 1)), Ok(1));
        assert_eq!(
            negotiate(r(1, 1), r(2, 4)),
            Err(ProtocolError::UnsupportedVersion)
        );
        assert_eq!(negotiate(r(1, 1), r(3, 2)), Err(ProtocolError::Malformed));
    }

    #[test]
    fn messages_survive_encoding() {
        roundtrip(Message::Hello(Hello::new(Purpose::Session)));
        roundtrip(Message::Chunk {
            transfer: TransferId::from_bytes([1; 16]),
            item: 3,
            offset: 1 << 40,
            data: vec![7; MAX_CHUNK_BYTES],
        });
        roundtrip(Message::Error(ErrorCode::Busy));
    }

    #[test]
    fn a_frame_length_over_the_limit_is_refused_before_reading_the_payload() {
        let header = ((MAX_FRAME_BYTES - HEADER_BYTES + 1) as u32).to_be_bytes();
        assert_eq!(decode_frame(&header), Err(ProtocolError::TooLarge));
        assert_eq!(
            payload_length(u32::MAX.to_be_bytes()),
            Err(ProtocolError::TooLarge)
        );
    }

    #[test]
    fn a_field_length_past_its_limit_or_the_input_is_refused() {
        let mut chunk = vec![tag::CHUNK];
        chunk.extend_from_slice(&[0; 16 + 2 + 8]);
        chunk.extend_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(decode_payload(&chunk), Err(ProtocolError::TooLarge));
        let mut short = vec![tag::CHUNK];
        short.extend_from_slice(&[0; 16 + 2 + 8]);
        short.extend_from_slice(&10u32.to_be_bytes());
        short.extend_from_slice(&[1; 9]);
        assert_eq!(decode_payload(&short), Err(ProtocolError::Truncated));
    }

    #[test]
    fn truncated_trailing_and_unknown_messages_are_refused() {
        let frame = encode_frame(&Message::Error(ErrorCode::Busy)).unwrap();
        assert_eq!(decode_frame(&frame[..frame.len() - 1]), Ok(None));
        assert_eq!(
            decode_payload(&frame[4..frame.len() - 1]),
            Err(ProtocolError::Truncated)
        );
        let mut trailing = frame[4..].to_vec();
        trailing.push(0);
        assert_eq!(decode_payload(&trailing), Err(ProtocolError::Trailing));
        // A fixed-size field followed by one byte too many.
        let mut commit = vec![tag::PAIR_COMMIT];
        commit.extend([0; 33]);
        assert_eq!(decode_payload(&commit), Err(ProtocolError::Trailing));
        assert_eq!(
            decode_payload(&[200]),
            Err(ProtocolError::UnknownMessage(200))
        );
        assert_eq!(decode_payload(&[]), Err(ProtocolError::Truncated));
        assert_eq!(
            decode_payload(&[tag::ERROR, 0, 99]),
            Err(ProtocolError::Malformed)
        );
    }

    #[test]
    fn an_offer_with_too_many_items_is_refused_before_reading_them() {
        let mut offer = vec![tag::OFFER];
        offer.extend_from_slice(&[0; 16]);
        offer.push(3);
        offer.extend_from_slice(&((MAX_BATCH_ITEMS + 1) as u16).to_be_bytes());
        assert_eq!(decode_payload(&offer), Err(ProtocolError::TooLarge));
    }

    #[test]
    fn an_offer_over_the_metadata_limit_is_refused_both_ways() {
        let name = vec![b'n'; MAX_PEER_NAME_BYTES];
        let fields = OfferFields {
            transfer: TransferId::from_bytes([1; 16]),
            kind: 3,
            items: vec![(name, 1, [0; 32]); 70],
            preview: Vec::new(),
        };
        let msg = Message::Offer(fields);
        assert_eq!(encode_frame(&msg), Err(ProtocolError::TooLarge));
        assert_eq!(
            decode_payload(&encode_payload(&msg)),
            Err(ProtocolError::TooLarge)
        );
    }

    #[test]
    fn the_negotiation_digest_depends_on_both_messages_and_their_order() {
        let a = encode_payload(&Message::Hello(Hello::new(Purpose::Pair)));
        let b = encode_payload(&Message::Hello(Hello::new(Purpose::Session)));
        assert_ne!(negotiation_digest(&a, &b), negotiation_digest(&b, &a));
        assert_ne!(negotiation_digest(&a, &a), negotiation_digest(&a, &b));
    }
}
