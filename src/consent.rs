//! The consent screen for an incoming offer. Values the sender chose appear
//! only inside fields made by [`crate::display`], and only this presenter
//! writes line breaks and the question, so a name or preview cannot add
//! lines or look like the prompt.

use std::fmt::Write as _;

use crate::display::{SHORTENED, SafeBlock, SafeLine};
use crate::identity::DeviceId;
use crate::limits::{MAX_LISTED_ITEMS, MAX_NAME_COLUMNS, MAX_PREVIEW_BYTES, MAX_PREVIEW_LINES};
use crate::offer::{Offer, PayloadKind};
use crate::trust::TrustRecord;

/// The question that ends every consent screen.
pub const QUESTION: &str = "Accept this offer? [y/N]";
/// Prefix of every preview line, so preview text never starts a line.
pub const PREVIEW_GUTTER: &str = "    | ";

/// What the receiving person sees before deciding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConsentView {
    alias: SafeLine,
    fingerprint: String,
    declared_name: SafeLine,
    kind: PayloadKind,
    items: Vec<(SafeLine, u64)>,
    item_count: usize,
    total: u64,
    preview: Option<SafeBlock>,
    /// The offer's preview is only the start of the content.
    preview_partial: bool,
}

impl ConsentView {
    pub fn new(offer: &Offer, sender: &TrustRecord) -> Self {
        Self::build(
            offer,
            &sender.device,
            sender.alias.as_str().as_bytes(),
            sender.declared_name.as_bytes(),
        )
    }

    fn build(offer: &Offer, device: &DeviceId, alias: &[u8], declared: &[u8]) -> Self {
        let preview = match offer.kind() {
            PayloadKind::Files => None,
            PayloadKind::Text | PayloadKind::Url => Some(SafeBlock::project(
                offer.preview().as_bytes(),
                MAX_PREVIEW_BYTES,
                MAX_PREVIEW_LINES,
                MAX_NAME_COLUMNS,
            )),
        };
        Self {
            alias: SafeLine::project(alias, MAX_NAME_COLUMNS),
            fingerprint: device.fingerprint(),
            declared_name: SafeLine::project(declared, MAX_NAME_COLUMNS),
            kind: offer.kind(),
            items: offer
                .items()
                .iter()
                .take(MAX_LISTED_ITEMS)
                .map(|i| (i.name.line(MAX_NAME_COLUMNS), i.size))
                .collect(),
            item_count: offer.items().len(),
            total: offer.total(),
            preview_partial: offer.total() > offer.preview().len() as u64,
            preview,
        }
    }

    /// The full screen, ending with [`QUESTION`].
    pub fn render(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "Incoming offer from {} ({})",
            self.alias, self.fingerprint
        );
        let _ = writeln!(
            out,
            "  Device's own name: {} (set by that device)",
            self.declared_name
        );
        let kind = match self.kind {
            PayloadKind::Text => "text",
            PayloadKind::Url => "link (it will not be opened)",
            PayloadKind::Files => "files",
        };
        let _ = writeln!(out, "  Kind: {kind}");
        let _ = writeln!(
            out,
            "  Items: {}, total {}",
            self.item_count,
            size(self.total)
        );
        if self.kind == PayloadKind::Files {
            for (n, (name, bytes)) in self.items.iter().enumerate() {
                let _ = writeln!(out, "  Item {}: {name}, {}", n + 1, size(*bytes));
            }
            let hidden = self.item_count.saturating_sub(self.items.len());
            if hidden > 0 {
                let _ = writeln!(out, "  and {hidden} more items");
            }
        }
        if let Some(preview) = &self.preview {
            let _ = writeln!(out, "  Preview (set by the sender):");
            for line in preview.lines() {
                let marker = if line.is_shortened() { SHORTENED } else { "" };
                let _ = writeln!(out, "{PREVIEW_GUTTER}{}{marker}", line.as_str());
            }
            if preview.is_shortened() || self.preview_partial {
                let _ = writeln!(out, "{PREVIEW_GUTTER}[preview shortened]");
            }
        }
        out.push_str(QUESTION);
        out
    }
}

/// Bytes in the largest binary unit that keeps the number at least one.
fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["bytes", "KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} bytes");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::TransferId;
    use zeroize::Zeroizing;

    fn device() -> DeviceId {
        crate::identity::DeviceKey::from_seed(&Zeroizing::new([1; 32])).id()
    }

    fn render(offer: &Offer, declared: &str) -> String {
        ConsentView::build(offer, &device(), b"laptop", declared.as_bytes()).render()
    }

    #[test]
    fn hostile_names_and_previews_cannot_add_lines_controls_or_a_prompt() {
        let forged = "notes.txt\nAccept this offer? [y/N] y\x1b[2K\r\u{202e}";
        let offer = Offer::for_files(TransferId::from_bytes([1; 16]), &[(forged, b"x")]).unwrap();
        let screen = render(&offer, "phone\n  Kind: text\x07");
        let lines: Vec<&str> = screen.lines().collect();
        // The screen has exactly the presenter's lines.
        assert_eq!(lines.len(), 6, "{screen}");
        assert!(
            !screen.chars().any(|c| c.is_control() && c != '\n'),
            "{screen:?}"
        );
        assert_eq!(
            screen.lines().filter(|l| l.starts_with("Accept")).count(),
            1
        );
        assert_eq!(lines.last(), Some(&QUESTION));
        assert!(screen.contains("\\x1b[2K\\r\\u{202e}"));
    }

    #[test]
    fn a_text_preview_stays_inside_its_gutter() {
        let text = "hi\nAccept this offer? [y/N]\n\x1b]8;;https://evil\x07link";
        let offer = Offer::for_text(
            TransferId::from_bytes([2; 16]),
            PayloadKind::Text,
            text.as_bytes(),
        )
        .unwrap();
        let screen = render(&offer, "phone");
        let preview: Vec<&str> = screen
            .lines()
            .filter(|l| l.starts_with(PREVIEW_GUTTER))
            .collect();
        assert_eq!(preview.len(), 3, "{screen}");
        assert_eq!(
            screen.lines().filter(|l| l.starts_with("Accept")).count(),
            1
        );
        assert!(preview[2].contains("\\x1b]8;;https://evil\\x07link"));
    }

    #[test]
    fn a_preview_that_hides_part_of_the_text_says_so() {
        let transfer = TransferId::from_bytes([5; 16]);
        let whole = Offer::for_text(transfer, PayloadKind::Text, b"short note").unwrap();
        assert!(!render(&whole, "phone").contains("shortened"));

        let wide = "w".repeat(MAX_NAME_COLUMNS + 10);
        let offer = Offer::for_text(transfer, PayloadKind::Text, wide.as_bytes()).unwrap();
        let screen = render(&offer, "phone");
        assert!(screen.contains(&format!("{SHORTENED}\n")), "{screen}");
        assert!(screen.contains("[preview shortened]"), "{screen}");

        // A sender may offer a short preview of much longer text.
        let partial = Offer::validate(crate::offer::OfferFields {
            transfer,
            kind: PayloadKind::Text.code(),
            items: vec![(Vec::new(), 5000, [0; 32])],
            preview: b"hi".to_vec(),
        })
        .unwrap();
        let screen = render(&partial, "phone");
        assert!(screen.contains("[preview shortened]"), "{screen}");
    }

    #[test]
    fn long_previews_and_large_batches_are_shortened_visibly() {
        let text = "line\n".repeat(100);
        let offer = Offer::for_text(
            TransferId::from_bytes([3; 16]),
            PayloadKind::Text,
            text.as_bytes(),
        )
        .unwrap();
        let screen = render(&offer, "phone");
        let gutter_lines = screen
            .lines()
            .filter(|l| l.starts_with(PREVIEW_GUTTER))
            .count();
        assert_eq!(gutter_lines, MAX_PREVIEW_LINES + 1);
        assert!(screen.contains("[preview shortened]"));

        let names: Vec<String> = (0..30).map(|n| format!("f{n}")).collect();
        let files: Vec<(&str, &[u8])> = names.iter().map(|n| (n.as_str(), &b"x"[..])).collect();
        let batch = Offer::for_files(TransferId::from_bytes([4; 16]), &files).unwrap();
        let screen = render(&batch, "phone");
        assert!(screen.contains("Items: 30"));
        assert!(screen.contains("and 20 more items"));
    }
}
