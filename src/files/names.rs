//! The file names this device creates for received items. A name that
//! cannot be used here is refused, never rewritten, so the file that
//! appears is the one the person accepted.

use crate::display::is_hidden_format;
use crate::ids::TransferId;
use crate::offer::PayloadKind;

/// Longest output name in bytes, the usual Linux limit.
pub const MAX_NAME_BYTES: usize = 255;
/// Names Hyakume keeps for its own files inside a destination.
pub const RESERVED_PREFIX: &str = ".hyakume-";

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum NameError {
    #[error("the name is empty")]
    Empty,
    #[error("the name is . or ..")]
    Dots,
    #[error("the name contains a path separator")]
    Separator,
    #[error("the name contains a NUL byte")]
    Nul,
    #[error("the name is longer than {MAX_NAME_BYTES} bytes")]
    TooLong,
    #[error("the name is not UTF-8")]
    Encoding,
    #[error("the name contains control or invisible formatting characters")]
    Control,
    #[error("the name starts with {RESERVED_PREFIX}, which Hyakume keeps for its own files")]
    Reserved,
}

/// A validated single file name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputName(String);

impl OutputName {
    /// Accepts one UTF-8 file name component of at most
    /// [`MAX_NAME_BYTES`] bytes, without separators, NUL, controls, or
    /// invisible formatting characters, and outside Hyakume's own prefix.
    pub fn new(raw: &[u8]) -> Result<Self, NameError> {
        if raw.is_empty() {
            return Err(NameError::Empty);
        }
        if raw.len() > MAX_NAME_BYTES {
            return Err(NameError::TooLong);
        }
        if raw.contains(&0) {
            return Err(NameError::Nul);
        }
        if raw.contains(&b'/') {
            return Err(NameError::Separator);
        }
        let name = std::str::from_utf8(raw).map_err(|_| NameError::Encoding)?;
        if name == "." || name == ".." {
            return Err(NameError::Dots);
        }
        if name.chars().any(|c| c.is_control() || is_hidden_format(c)) {
            return Err(NameError::Control);
        }
        if name.starts_with(RESERVED_PREFIX) {
            return Err(NameError::Reserved);
        }
        Ok(Self(name.to_owned()))
    }

    /// The name this device gives received text or a received link, which
    /// arrive without a file name of their own.
    pub fn generated(kind: PayloadKind, transfer: TransferId) -> Self {
        let id = &super::hex(transfer.as_bytes())[..8];
        let stem = match kind {
            PayloadKind::Url => "link",
            PayloadKind::Text | PayloadKind::Files => "text",
        };
        Self(format!("{stem}-{id}.txt"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

/// The private partial file of one item.
pub fn partial_name(transfer: TransferId, item: usize) -> String {
    format!(
        "{RESERVED_PREFIX}{}-{item}.part",
        super::hex(transfer.as_bytes())
    )
}

/// The sealed spool of a transfer's input, in the spool folder.
pub fn spool_name(transfer: TransferId) -> String {
    format!("{}.spool", super::hex(transfer.as_bytes()))
}

/// The transfer whose spool has exactly this name, if any.
pub fn spool_transfer(name: &[u8]) -> Option<TransferId> {
    let hex = name.strip_suffix(b".spool")?;
    let bytes = data_encoding::HEXLOWER.decode(hex).ok()?;
    let transfer = TransferId::from_bytes(bytes.try_into().ok()?);
    (spool_name(transfer).as_bytes() == name).then_some(transfer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_that_could_leave_the_destination_or_hide_themselves_are_refused() {
        let cases: [(&[u8], NameError); 10] = [
            (b"", NameError::Empty),
            (b".", NameError::Dots),
            (b"..", NameError::Dots),
            (b"../etc/passwd", NameError::Separator),
            (b"a/b", NameError::Separator),
            (b"a\0b", NameError::Nul),
            (b"\xff.txt", NameError::Encoding),
            ("photo\u{202e}gpj.exe".as_bytes(), NameError::Control),
            (b"line\nbreak", NameError::Control),
            (b".hyakume-0-0.part", NameError::Reserved),
        ];
        for (raw, error) in cases {
            assert_eq!(OutputName::new(raw), Err(error), "{raw:?}");
        }
        assert_eq!(
            OutputName::new(&[b'a'; MAX_NAME_BYTES + 1]),
            Err(NameError::TooLong)
        );
    }

    #[test]
    fn ordinary_names_are_kept_exactly() {
        for name in [
            "notes.txt",
            ".bashrc",
            "Résumé 2026.pdf",
            "日本語.md",
            "a..b",
        ] {
            assert_eq!(OutputName::new(name.as_bytes()).unwrap().as_str(), name);
        }
        assert!(OutputName::new(&[b'a'; MAX_NAME_BYTES]).is_ok());
    }

    #[test]
    fn generated_and_partial_names_are_valid_and_distinct() {
        let id = TransferId::from_bytes([0xab; 16]);
        let text = OutputName::generated(PayloadKind::Text, id);
        assert_eq!(text.as_str(), "text-abababab.txt");
        assert!(OutputName::new(text.as_bytes()).is_ok());
        // A peer cannot offer a name that collides with a partial file.
        assert_eq!(
            OutputName::new(partial_name(id, 3).as_bytes()),
            Err(NameError::Reserved)
        );
    }
}
