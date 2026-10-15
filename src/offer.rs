//! Offers: what a sender proposes before any byte moves. [`Offer::validate`]
//! accepts one only when every limit holds, and sizes are always exact, so
//! the receiver can decide and reserve space up front. Each item carries the
//! digest checked before publication, and acceptance names the digest of
//! the whole offer seen. An offer that validates also fits the wire's
//! metadata limit, so the sender never records an offer it cannot send or
//! load again.

use crate::display::PeerText;
use crate::ids::TransferId;
use crate::limits::{
    MAX_BATCH_BYTES, MAX_BATCH_ITEMS, MAX_FILE_BYTES, MAX_OFFER_METADATA_BYTES,
    MAX_PEER_NAME_BYTES, MAX_PREVIEW_BYTES, MAX_TEXT_BYTES, MAX_URL_BYTES,
};
use crate::protocol;

const CTX_OFFER_DIGEST: &str = "hyakume v1 offer digest";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PayloadKind {
    /// UTF-8 text.
    Text,
    /// A URL, treated as text and never opened.
    Url,
    /// One file or a batch of files.
    Files,
}

impl PayloadKind {
    pub fn code(self) -> u8 {
        match self {
            Self::Text => 1,
            Self::Url => 2,
            Self::Files => 3,
        }
    }

    pub fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Text),
            2 => Some(Self::Url),
            3 => Some(Self::Files),
            _ => None,
        }
    }
}

/// One item as it appears in the offer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    /// The sender's name for the item. Presentation only; it is never used
    /// as a path.
    pub name: PeerText,
    pub size: u64,
    pub digest: [u8; 32],
}

/// Offer fields as decoded from the wire, before validation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OfferFields {
    pub transfer: TransferId,
    pub kind: u8,
    pub items: Vec<(Vec<u8>, u64, [u8; 32])>,
    pub preview: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum OfferError {
    #[error("unknown payload kind")]
    UnknownKind,
    #[error("an offer must contain at least one item")]
    Empty,
    #[error("the offer has too many items")]
    TooManyItems,
    #[error("text and URL offers carry exactly one item")]
    NotSingle,
    #[error("an item is larger than allowed for its kind")]
    ItemTooLarge,
    #[error("the offer's total size is larger than allowed")]
    TotalTooLarge,
    #[error("an item name is too long")]
    NameTooLong,
    #[error("a file needs a name")]
    MissingName,
    #[error("the preview is too long")]
    PreviewTooLong,
    #[error("the item names together are too long for one offer; send fewer files at once")]
    MetadataTooLarge,
}

/// A validated offer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offer {
    transfer: TransferId,
    kind: PayloadKind,
    items: Vec<Item>,
    total: u64,
    /// For text and URLs, the first bytes of the content shown before
    /// consent. Empty for files.
    preview: PeerText,
}

impl Offer {
    pub fn validate(fields: OfferFields) -> Result<Self, OfferError> {
        let kind = PayloadKind::from_code(fields.kind).ok_or(OfferError::UnknownKind)?;
        if fields.items.is_empty() {
            return Err(OfferError::Empty);
        }
        if fields.items.len() > MAX_BATCH_ITEMS {
            return Err(OfferError::TooManyItems);
        }
        let item_limit = match kind {
            PayloadKind::Text => MAX_TEXT_BYTES,
            PayloadKind::Url => MAX_URL_BYTES,
            PayloadKind::Files => MAX_FILE_BYTES,
        };
        if kind != PayloadKind::Files && fields.items.len() != 1 {
            return Err(OfferError::NotSingle);
        }
        // Checked last, so a single bad item still gets its own error.
        let encoded = protocol::offer_payload_len(&fields);
        let mut total = 0u64;
        let mut items = Vec::with_capacity(fields.items.len());
        for (name, size, digest) in fields.items {
            if size > item_limit {
                return Err(OfferError::ItemTooLarge);
            }
            total = total.checked_add(size).ok_or(OfferError::TotalTooLarge)?;
            if total > MAX_BATCH_BYTES {
                return Err(OfferError::TotalTooLarge);
            }
            if kind == PayloadKind::Files && name.is_empty() {
                return Err(OfferError::MissingName);
            }
            let name =
                PeerText::new(name, MAX_PEER_NAME_BYTES).map_err(|_| OfferError::NameTooLong)?;
            items.push(Item { name, size, digest });
        }
        let preview_limit = match kind {
            PayloadKind::Files => 0,
            PayloadKind::Text | PayloadKind::Url => usize::try_from(total)
                .unwrap_or(usize::MAX)
                .min(MAX_PREVIEW_BYTES),
        };
        if fields.preview.len() > preview_limit {
            return Err(OfferError::PreviewTooLong);
        }
        let preview =
            PeerText::new(fields.preview, preview_limit).map_err(|_| OfferError::PreviewTooLong)?;
        if encoded > MAX_OFFER_METADATA_BYTES {
            return Err(OfferError::MetadataTooLarge);
        }
        Ok(Self {
            transfer: fields.transfer,
            kind,
            items,
            total,
            preview,
        })
    }

    /// Builds an outgoing offer for text or a URL, with its preview.
    pub fn for_text(
        transfer: TransferId,
        kind: PayloadKind,
        text: &[u8],
    ) -> Result<Self, OfferError> {
        let preview = text[..text.len().min(MAX_PREVIEW_BYTES)].to_vec();
        Self::validate(OfferFields {
            transfer,
            kind: kind.code(),
            items: vec![(
                Vec::new(),
                text.len() as u64,
                *blake3::hash(text).as_bytes(),
            )],
            preview,
        })
    }

    /// Builds an outgoing offer for named files.
    pub fn for_files(transfer: TransferId, files: &[(&str, &[u8])]) -> Result<Self, OfferError> {
        Self::validate(OfferFields {
            transfer,
            kind: PayloadKind::Files.code(),
            items: files
                .iter()
                .map(|(name, bytes)| {
                    (
                        name.as_bytes().to_vec(),
                        bytes.len() as u64,
                        *blake3::hash(bytes).as_bytes(),
                    )
                })
                .collect(),
            preview: Vec::new(),
        })
    }

    pub fn transfer(&self) -> TransferId {
        self.transfer
    }

    pub fn kind(&self) -> PayloadKind {
        self.kind
    }

    pub fn items(&self) -> &[Item] {
        &self.items
    }

    pub fn total(&self) -> u64 {
        self.total
    }

    pub fn preview(&self) -> &PeerText {
        &self.preview
    }

    /// The fields to encode on the wire.
    pub fn fields(&self) -> OfferFields {
        OfferFields {
            transfer: self.transfer,
            kind: self.kind.code(),
            items: self
                .items
                .iter()
                .map(|i| (i.name.as_bytes().to_vec(), i.size, i.digest))
                .collect(),
            preview: self.preview.as_bytes().to_vec(),
        }
    }

    /// A digest of every field of the offer. The receiver's acceptance
    /// names it, so the sender knows exactly which offer was accepted.
    pub fn digest(&self) -> [u8; 32] {
        let mut h = blake3::Hasher::new_derive_key(CTX_OFFER_DIGEST);
        h.update(self.transfer.as_bytes());
        h.update(&[self.kind.code()]);
        h.update(&(self.items.len() as u64).to_be_bytes());
        for item in &self.items {
            h.update(&(item.name.len() as u64).to_be_bytes());
            h.update(item.name.as_bytes());
            h.update(&item.size.to_be_bytes());
            h.update(&item.digest);
        }
        h.update(&(self.preview.len() as u64).to_be_bytes());
        h.update(self.preview.as_bytes());
        *h.finalize().as_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(kind: PayloadKind, items: Vec<(Vec<u8>, u64)>) -> OfferFields {
        OfferFields {
            transfer: TransferId::from_bytes([1; 16]),
            kind: kind.code(),
            items: items.into_iter().map(|(n, s)| (n, s, [0; 32])).collect(),
            preview: Vec::new(),
        }
    }

    #[test]
    fn sizes_are_refused_one_byte_past_each_limit() {
        let file = |size| fields(PayloadKind::Files, vec![(b"f".to_vec(), size)]);
        assert!(Offer::validate(file(MAX_FILE_BYTES)).is_ok());
        assert_eq!(
            Offer::validate(file(MAX_FILE_BYTES + 1)),
            Err(OfferError::ItemTooLarge)
        );
        let text = |size| fields(PayloadKind::Text, vec![(Vec::new(), size)]);
        assert!(Offer::validate(text(MAX_TEXT_BYTES)).is_ok());
        assert_eq!(
            Offer::validate(text(MAX_TEXT_BYTES + 1)),
            Err(OfferError::ItemTooLarge)
        );
        let url = |size| fields(PayloadKind::Url, vec![(Vec::new(), size)]);
        assert_eq!(
            Offer::validate(url(MAX_URL_BYTES + 1)),
            Err(OfferError::ItemTooLarge)
        );
    }

    #[test]
    fn a_batch_total_cannot_overflow_or_pass_the_limit() {
        let half = MAX_BATCH_BYTES / 2;
        let ok = fields(
            PayloadKind::Files,
            vec![(b"a".to_vec(), half), (b"b".to_vec(), half)],
        );
        assert!(Offer::validate(ok).is_ok());
        let over = fields(
            PayloadKind::Files,
            vec![
                (b"a".to_vec(), half),
                (b"b".to_vec(), half),
                (b"c".to_vec(), 1),
            ],
        );
        assert_eq!(Offer::validate(over), Err(OfferError::TotalTooLarge));
    }

    #[test]
    fn every_offer_that_validates_can_be_sent() {
        // Grow a batch of longest names until validation refuses it. The
        // last batch accepted must fit in a frame, and the refusal must come
        // from the metadata limit before the item limit is reached.
        let name = vec![b'n'; MAX_PEER_NAME_BYTES];
        let mut items = Vec::new();
        let refused = loop {
            items.push((name.clone(), 1));
            if let Err(e) = Offer::validate(fields(PayloadKind::Files, items.clone())) {
                break e;
            }
        };
        assert_eq!(refused, OfferError::MetadataTooLarge);
        items.pop();
        let offer = Offer::validate(fields(PayloadKind::Files, items)).unwrap();
        assert!(protocol::encode_frame(&protocol::Message::Offer(offer.fields())).is_ok());
    }

    #[test]
    fn manifests_must_have_one_to_the_maximum_items() {
        assert_eq!(
            Offer::validate(fields(PayloadKind::Files, vec![])),
            Err(OfferError::Empty)
        );
        let many = vec![(b"f".to_vec(), 0); MAX_BATCH_ITEMS + 1];
        assert_eq!(
            Offer::validate(fields(PayloadKind::Files, many)),
            Err(OfferError::TooManyItems)
        );
        let two_texts = vec![(Vec::new(), 1), (Vec::new(), 1)];
        assert_eq!(
            Offer::validate(fields(PayloadKind::Text, two_texts)),
            Err(OfferError::NotSingle)
        );
    }

    #[test]
    fn unknown_kinds_long_names_and_missing_names_are_refused() {
        let mut unknown = fields(PayloadKind::Files, vec![(b"f".to_vec(), 1)]);
        unknown.kind = 9;
        assert_eq!(Offer::validate(unknown), Err(OfferError::UnknownKind));
        let long = fields(
            PayloadKind::Files,
            vec![(vec![b'n'; MAX_PEER_NAME_BYTES + 1], 1)],
        );
        assert_eq!(Offer::validate(long), Err(OfferError::NameTooLong));
        let unnamed = fields(PayloadKind::Files, vec![(Vec::new(), 1)]);
        assert_eq!(Offer::validate(unnamed), Err(OfferError::MissingName));
    }

    #[test]
    fn a_preview_cannot_be_longer_than_the_content_or_the_limit() {
        let mut text = fields(PayloadKind::Text, vec![(Vec::new(), 3)]);
        text.preview = b"four".to_vec();
        assert_eq!(Offer::validate(text), Err(OfferError::PreviewTooLong));
        let mut file = fields(PayloadKind::Files, vec![(b"f".to_vec(), 10)]);
        file.preview = b"x".to_vec();
        assert_eq!(Offer::validate(file), Err(OfferError::PreviewTooLong));
    }

    #[test]
    fn the_offer_digest_changes_when_any_field_changes() {
        let base = Offer::for_files(TransferId::from_bytes([1; 16]), &[("a", b"hello")]).unwrap();
        let renamed =
            Offer::for_files(TransferId::from_bytes([1; 16]), &[("b", b"hello")]).unwrap();
        let changed =
            Offer::for_files(TransferId::from_bytes([1; 16]), &[("a", b"hellO")]).unwrap();
        let other_id =
            Offer::for_files(TransferId::from_bytes([2; 16]), &[("a", b"hello")]).unwrap();
        assert_ne!(base.digest(), renamed.digest());
        assert_ne!(base.digest(), changed.digest());
        assert_ne!(base.digest(), other_id.digest());
        assert_eq!(
            base.digest(),
            Offer::validate(base.fields()).unwrap().digest()
        );
    }
}
